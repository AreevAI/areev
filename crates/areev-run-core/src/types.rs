//! The step-function protocol: journal keys, events in, commands out
//! (governed-agents §5.1/§6). The driver owns every effect; this vocabulary
//! is how the pure core describes what it wants done and learns what
//! happened.

use crate::error::BudgetAxis;
use serde_json::Value;
use sha2::{Digest, Sha256};

/// What kind of effect a journal entry records (§5.1's `kind`).
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
pub enum EffectKind {
    /// A tool execution (Host) or a Client ask.
    Tool,
    /// An LLM turn of an abstract node (Wave 2).
    Llm,
}

impl EffectKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Tool => "tool",
            Self::Llm => "llm",
        }
    }
}

/// The deterministic journal key (§5.1):
/// `(run_id, task_path, node, attempt, effect_seq, kind)`.
///
/// `task_path` is `""` for the static graph; Send-spawned tasks get
/// `parent_path/spawn_ordinal` in Wave 2. `effect_seq` is the 0-based
/// ordinal of the effect within a node attempt (an abstract node's LLM loop
/// consumes several). Replay addresses the journal by this key — never by
/// op-log sequence.
#[derive(
    Debug, Clone, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
pub struct JournalKey {
    pub run_id: String,
    pub task_path: String,
    pub node: String,
    pub attempt: u32,
    pub effect_seq: u32,
    pub kind: EffectKind,
}

impl JournalKey {
    /// `tool_call_id` = the digest of the journal key — unique per
    /// occurrence by construction AND reproducible under scheduler
    /// permutation (a random id would make the permutation gate impossible;
    /// a sequential one would be schedule-dependent). 0x1f separators keep
    /// `("a","b/c")` and `("a/b","c")` distinct.
    pub fn tool_call_id(&self) -> String {
        let mut h = Sha256::new();
        h.update(b"areev-run:v1");
        for part in [
            self.run_id.as_str(),
            self.task_path.as_str(),
            self.node.as_str(),
        ] {
            h.update([0x1f]);
            h.update(part.as_bytes());
        }
        h.update([0x1f]);
        h.update(self.attempt.to_be_bytes());
        h.update([0x1f]);
        h.update(self.effect_seq.to_be_bytes());
        h.update([0x1f]);
        h.update(self.kind.as_str().as_bytes());
        hex(&h.finalize())
    }

    /// The idempotency-key material prefix (§5.1); the driver appends
    /// `canonical(input)` and hashes.
    pub fn idempotency_prefix(&self) -> String {
        format!(
            "{}\u{1f}{}\u{1f}{}\u{1f}{}\u{1f}{}",
            self.run_id, self.task_path, self.node, self.attempt, self.effect_seq
        )
    }
}

fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        use std::fmt::Write;
        let _ = write!(s, "{b:02x}");
    }
    s
}

/// How an effect ended, as the driver reports it (mirrors the Tool grain's
/// lifecycle vocabulary — the driver journals the grain; this is the pure
/// echo the scheduler consumes).
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum EffectOutcome {
    Completed {
        /// The result value the node contributes to state (merged under the
        /// node's id by the reducers at superstep close).
        result: Value,
        /// Journal accounting: bytes the result grain + blobs cost (storage
        /// axis) and tokens for LLM effects.
        journal_bytes: u64,
        input_tokens: u64,
        output_tokens: u64,
        /// Micro-USD (integer — budgets never do float arithmetic).
        usd_micros: u64,
    },
    Failed {
        cause: FailCause,
        detail: String,
        journal_bytes: u64,
    },
}

/// The §6.3 retryability table's input — mirrors
/// `areev_core::types::FailureCause` without depending on how the driver
/// spells it on the grain.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum FailCause {
    Timeout,
    ExecutorError,
    SchemaValidationFailed,
    UserAborted,
    Unknown,
    /// The PROVIDER refused the prompt as too long, said so structurally, and
    /// the transcript is what has to change — not the number of attempts.
    ///
    /// Neither retryable nor terminal, which is why it is its own cause: an
    /// identical retry gets an identical refusal, and failing the node throws
    /// away work the journal already holds. The scheduler folds and re-sends
    /// the same turn on something smaller.
    ContextOverflow,
}

impl FailCause {
    /// §6.3: Timeout/ExecutorError retryable; SchemaValidationFailed only on
    /// the LLM re-prompt path (Wave 2); UserAborted/Unknown never.
    pub fn retryable(&self, kind: EffectKind) -> bool {
        match self {
            Self::Timeout | Self::ExecutorError => true,
            Self::SchemaValidationFailed => kind == EffectKind::Llm,
            // NOT retryable: the same prompt earns the same refusal. The
            // abstract-node loop handles it by folding first, which is a
            // different move from spending an attempt.
            Self::UserAborted | Self::Unknown | Self::ContextOverflow => false,
        }
    }
}

pub const PARKED_ASKS: &str = "$parked_asks";
pub const INBOX: &str = "$inbox";

/// Events the driver feeds into `step`. Every invocation that may open or
/// close a superstep must include a fresh `ClockReading` — the ONLY time the
/// scheduler ever sees, journaled in the decision record.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub enum EventIn {
    /// Bootstrap: the run's input becomes the initial accumulated state.
    Start { input: Value },
    /// The driver's journaled clock reading (unix ms).
    ClockReading { unix_ms: u64 },
    /// A dispatched effect resolved — from a live executor OR from a journal
    /// hit on replay; the scheduler cannot tell, which is the point.
    EffectResolved { key: JournalKey, outcome: EffectOutcome },
    /// A Client ask was answered (the driver already validated + journaled
    /// the superseding result).
    ResponseSettled { tool_call_id: String, outcome: EffectOutcome },
    /// A cancel marker was seen.
    CancelSeen { principal: String, reason: String },
    AskForwarded { tool_call_id: String },
    /// A steering message was queued against the run.
    InputSeen { message: Value },
    /// The driver picked this run back up from a checkpoint after it stopped
    /// running — a crash, a kill, a process restart.
    ///
    /// Fed ONLY when the loaded checkpoint is `Idle` (between supersteps).
    /// A park-resume closes a superstep that is still open and already
    /// accounts its own gap from the park reading, so feeding this there
    /// would charge the same span twice.
    ///
    /// It exists because the gap between the last close and the next open is
    /// invisible to the scheduler otherwise: `step()` normally opens the next
    /// superstep in the very call that closed the previous one, at the same
    /// reading. A driver that died in between hands the next open a LATER
    /// reading, and without this marker replay cannot tell that apart from
    /// active work — which is the whole `RUN-E009`-on-every-crash-recovered
    /// -run defect this closes.
    Resumed,
    /// A host asked the run to PAUSE (#344): hold at the next superstep
    /// boundary instead of opening the next superstep.
    ///
    /// TRANSIENT by design — it affects only the `step` call it rides and is
    /// never stored in `SchedulerState`, so the checkpoint a pause leaves
    /// behind is byte-identical to the one an uninterrupted run writes at the
    /// same close. The driver keeps feeding it on every call while the pause
    /// request stands; the first call that ends between supersteps (`Idle`,
    /// not terminal) is where the run parks. Terminal outcomes still win: a
    /// run with nothing left to do completes, cancels or exhausts rather than
    /// pausing. Continuing is an ordinary [`EventIn::Resumed`] from that
    /// `Idle` checkpoint, so `verify` replays a paused-then-resumed run
    /// through the resume-boundary rule it already has, and the paused span
    /// accrues as elapsed, never as wall.
    PauseRequested,
}

/// One tool offered to an abstract node's model, pinned by the manifest.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct OfferedTool {
    pub tool_name: String,
    pub tool_hash: String,
    /// Keys merged OVER the model's arguments at dispatch, the lock winning
    /// (#393): the Definition's `locked_params`, and for a dynamic tool its
    /// entry's `dispatch.locked` beneath them. What the intent journals and
    /// the executor receives is the merged object; the transcript keeps what
    /// the model said. `None` for every tool that locks nothing, so an offer
    /// without locks serializes as it always did.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub locked: Option<Value>,
    /// Present exactly when this is a tool declared in the run input (#392,
    /// `tools_from`): the model sees `tool_name` and `dynamic.input_schema`,
    /// and a call dispatches the pinned Definition `dynamic.dispatch` names —
    /// whose address `tool_hash` is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dynamic: Option<Box<DynamicTool>>,
}

impl OfferedTool {
    /// An offered pinned Definition with nothing locked — the pre-1.13 shape.
    pub fn new(tool_name: impl Into<String>, tool_hash: impl Into<String>) -> Self {
        OfferedTool {
            tool_name: tool_name.into(),
            tool_hash: tool_hash.into(),
            locked: None,
            dynamic: None,
        }
    }

    /// The Definition a call to this tool actually dispatches: its own name,
    /// or — for a dynamic tool — the pinned Definition behind it. This is the
    /// name the strict-argument table is keyed by.
    pub fn dispatch_name(&self) -> &str {
        match &self.dynamic {
            Some(d) => &d.dispatch.definition,
            None => &self.tool_name,
        }
    }

    /// The input a model-issued call dispatches with (#392, #393): the
    /// model's arguments — under `dispatch.args_into` for a dynamic tool
    /// that names one — with every locked key merged over them. Shallow, top
    /// level, the lock always winning. Pure and total, so replay rebuilds the
    /// same object; a non-object argument payload (refused before dispatch
    /// anyway) passes through unlocked.
    pub fn dispatch_input(&self, arguments: &Value) -> Value {
        let mut input = match self.dynamic.as_ref().and_then(|d| d.dispatch.args_into.as_deref()) {
            Some(key) => {
                let mut o = serde_json::Map::new();
                o.insert(key.to_string(), arguments.clone());
                Value::Object(o)
            }
            None => arguments.clone(),
        };
        if let (Some(Value::Object(lock)), Value::Object(obj)) = (&self.locked, &mut input) {
            for (k, v) in lock {
                obj.insert(k.clone(), v.clone());
            }
        }
        input
    }
}

/// A tool declared in the RUN INPUT and pinned at start (#392, `tools_from`).
///
/// The host builds the list per run — the `tools/list` of an MCP server the
/// tenant connected, say — and the manifest freezes it, so a resume and
/// `verify` offer exactly the set the run started with.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct DynamicTool {
    /// What the model is offered and calls (normalized for providers like a
    /// Definition's name: dots become underscores on the wire).
    pub name: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
    /// The arguments the model may fill; a call is validated against it,
    /// always — it is the contract the host declared.
    pub input_schema: Value,
    pub dispatch: DynamicDispatch,
}

/// Which pinned Definition a [`DynamicTool`] call reaches, and how.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct DynamicDispatch {
    /// The pinned host Definition's `tool_name` (canonical).
    pub definition: String,
    /// Its content address, frozen at start.
    pub hash: String,
    /// Keys merged over the model's arguments, the lock winning — the server
    /// URL, the MCP tool name, the credential. The Definition's own
    /// `locked_params` are merged over these in turn.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub locked: Option<Value>,
    /// When set, the model's arguments are placed under this key before the
    /// lock is merged — `"arguments"` for `mcp.call`, whose input carries the
    /// MCP tool's arguments one level down. Absent = merged at the top level.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub args_into: Option<String>,
    /// The dispatch Definition's own `input_schema`, frozen at start. The
    /// MERGED input — locks plus the wrapped arguments — is validated against
    /// it whether or not the Definition is `strict`: the host promised the
    /// model's arguments fit inside it, and a call that does not is a
    /// correction for the model, not a request the Definition refuses.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub definition_schema: Option<Value>,
}

/// A plan's per-node LLM configuration (#391, #392) — the `llm` block for one
/// abstract node, validated and normalized at start and FROZEN in the run
/// manifest, so a resume and `verify` run the block the run started with.
///
/// Every field is optional and absent means "what an abstract node did before
/// the block existed": the built-in system prompt, the node name as the
/// instruction, the whole state, every pinned host tool, no structured output,
/// temperature 0, the run's `llm_max_tokens`.
#[derive(Debug, Clone, PartialEq, Default, serde::Serialize, serde::Deserialize)]
pub struct LlmNodeConfig {
    /// The Definitions this node's dynamic tools may dispatch through that no
    /// node binds (#392), by canonical name — pinned at start into the
    /// manifest's `dispatch_pins`, never offered to the model themselves.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub dispatch: Vec<String>,
    /// Replaces the built-in system prompt for this node.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system: Option<String>,
    /// The literal instruction placed in `messages[0]`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instruction: Option<String>,
    /// A JSON pointer into the node's input whose STRING is the instruction.
    /// Resolving to nothing, or to a non-string, fails the node
    /// `SchemaValidationFailed` (not retried — the same state fails the same
    /// way), as a `reads` pointer does.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instruction_from: Option<String>,
    /// Which merged-state keys `messages[0]` carries: `"all"` (the default),
    /// `"none"`, or a list of keys. A steering `$inbox` is carried whatever
    /// the selection says: an operator's message must reach the model.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<Value>,
    /// The subset of pinned host Definitions offered (by `tool_name`), in
    /// manifest order. `None` = all of them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<String>>,
    /// The JSON pointer the dynamic tools were read from, kept for the record.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools_from: Option<String>,
    /// The tools `tools_from` resolved to at start, in input order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub dynamic_tools: Vec<DynamicTool>,
    /// `{"json_schema": {…}}` or `{"json": true}`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response: Option<Value>,
    /// A number, or `"omit"` to send none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<Value>,
    /// This node's output ceiling per call, in place of the run's
    /// `llm_max_tokens` — and its per-dispatch reservation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parallel_tool_calls: Option<bool>,
    /// The node's result gains this key: every tool call of the attempt, from
    /// the journaled outcomes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_results_into: Option<String>,
    /// The tool-offer narrowing for this node (C1, #396), normalized:
    /// `false` keeps the whole offer, `{"top_k": n}` narrows to the top `n`.
    /// `None` (also what `narrow: true` normalizes to) is the run-wide rule.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub narrow: Option<Value>,
}

impl LlmNodeConfig {
    /// `Some(k)` = narrow to the top `k` when more than `k` tools are offered;
    /// `None` = never narrow this node.
    pub fn narrow_top_k(&self, default: usize) -> Option<usize> {
        match &self.narrow {
            Some(Value::Bool(false)) => None,
            Some(v) => Some(
                v.get("top_k")
                    .and_then(Value::as_u64)
                    .map(|k| k as usize)
                    .filter(|k| *k >= 1)
                    .unwrap_or(default),
            ),
            None => Some(default),
        }
    }
}

/// What executes a node (resolved by the driver's V7 freeze).
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum NodeExecutor {
    /// Host-executed tool; the driver resolved the Definition hash.
    Host { tool_hash: String, tool_name: String },
    /// Client-executed: park and emit a `requires_action` ask.
    Client {
        tool_hash: String,
        tool_name: String,
        /// Whether this ask is an APPROVAL (a second person must answer) or
        /// a CONFIRMATION the run's own initiator may answer (#294).
        ///
        /// Frozen from the Definition at run start, so a mid-run supersession
        /// cannot downgrade an approval a person is already parked on.
        /// `#[serde(default)]` is `true`: every Client ask was an approval
        /// boundary before this existed, and a manifest that does not say
        /// must mean the stricter thing.
        #[serde(default = "crate::types::default_true")]
        approval: bool,
    },
    /// An abstract node (§6.2): the node label is an instruction an LLM
    /// interprets, choosing among the workflow's pinned Host tools. The
    /// node runs as an LLM LOOP — turn, tool calls, results, next turn —
    /// each effect journaled under its own `effect_seq`.
    Abstract {
        tools: Vec<OfferedTool>,
        /// The plan's frozen `llm` block for this node (#391), or `None` —
        /// every abstract node before the block existed, byte for byte.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        llm: Option<Box<LlmNodeConfig>>,
    },
    /// A subgraph node (Wave 2): the binding resolves to another Workflow
    /// grain; the child runs as its own run (own run_id, own journal),
    /// linked by `parent_task_id`, and its final context becomes this
    /// node's result.
    Subgraph { workflow_hash: String },
    /// A declared read of the run's own memory (#255): the plan's `reads`
    /// entry for this node, frozen by the manifest. The DRIVER answers it
    /// from the store it already holds — never a tool, never the pool — so
    /// the scheduler only has to treat it as an effect with a result.
    /// `op` is `entity_at` | `related` | `recall`; `spec` is the normalized
    /// declaration.
    MemoryRead { op: String, spec: Value },
    /// A decision effect (`docs/decision-model-proposal.md` C1–C3): answered
    /// by the HOST's decision backend, never by a tool and never by the pool.
    ///
    /// Two shapes share it. A decision NODE (C3) binds a Tool Definition whose
    /// `executor_uri` is the reserved [`DECIDE_URI`]; the manifest freezes it
    /// to this variant with that Definition's hash and name, so it journals as
    /// an ordinary Tool execution grain. The scheduler's OWN asks inside an
    /// abstract node — the decision-guided fold (C2) and the tool-offer
    /// narrowing (C1) — carry an empty `tool_hash` and the reserved
    /// [`DECIDE_TOOL`] name; their request rides the effect's input under
    /// `decide`.
    ///
    /// Its own variant rather than a Host tool with a reserved name, for the
    /// memory-read reason (#255): a missed interception must not fall through
    /// to `--tool-cmd`, where a tool could forge the answer.
    Decide { tool_hash: String, tool_name: String },
}

/// The reserved `executor_uri` a Tool Definition names to become a decision
/// node (C3). Not a code address: the host's decision backend answers it.
pub const DECIDE_URI: &str = "areev://decide";

/// The journal name of the scheduler's own decision effects (C1/C2).
pub const DECIDE_TOOL: &str = "mg:decide";

/// Serde default for a field whose absence must mean the STRICTER reading.
pub(crate) fn default_true() -> bool {
    true
}

/// Commands the driver must perform, in order.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub enum Command {
    /// Write the intent grain (Pending) BEFORE dispatching (§5.1). The
    /// driver stamps `created_at` from the journaled clock and re-states
    /// every key field as grain content.
    WriteIntent {
        key: JournalKey,
        executor: NodeExecutor,
        input: Value,
        superstep: u64,
        clock_ms: u64,
    },
    /// Execute the effect (Host only) with the deterministic idempotency
    /// key; report back as `EventIn::EffectResolved`.
    Dispatch {
        key: JournalKey,
        executor: NodeExecutor,
        input: Value,
    },
    /// Park: emit the `requires_action` envelope for these asks (§6.6).
    EmitEnvelope { asks: Vec<Ask> },
    /// Persist a checkpoint: the scheduler state AS OF this close (several
    /// supersteps can close inside one `step` call, so the snapshot rides
    /// the command — the driver must not serialize the final state for an
    /// intermediate close) plus the decision record for the superstep just
    /// closed (or the park point).
    WriteCheckpoint {
        superstep: u64,
        decision_record: DecisionRecord,
        state_json: Value,
    },
    /// The run reached a terminal status; the driver checkpoints and
    /// finalizes.
    Finish { outcome: RunOutcome },
}

/// One HITL ask (§6.6). Addressed by `tool_call_id`, never by index.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Ask {
    pub tool_call_id: String,
    pub node: String,
    pub tool_name: String,
    pub input: Value,
    pub expires_at_sec: Option<i64>,
    /// Approval asks structurally require responder ≠ triggering principal.
    pub approval: bool,
}

/// The journaled per-superstep decision record (§5.1) — what replay
/// re-derives and asserts against.
#[derive(Debug, Clone, PartialEq, Default, serde::Serialize, serde::Deserialize)]
pub struct DecisionRecord {
    pub superstep: u64,
    /// (edge index, generation of the destination delivery, outcome).
    pub edges: Vec<(usize, u32, EdgeOutcome)>,
    /// Clock readings: superstep open and close (§6.7's two readings).
    pub clock_open_ms: u64,
    pub clock_close_ms: u64,
    /// Nodes dispatched this superstep, canonical order: (node idx, attempt).
    pub dispatched: Vec<(usize, u32)>,
    /// Send spawn decisions journaled at this close: (task_path, target
    /// node idx). Empty on non-spawning supersteps — and skipped when empty
    /// so pre-Send checkpoints stay byte-identical.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub spawns: Vec<(String, usize)>,
    /// Send tasks dispatched this superstep: (task_path, attempt).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub task_dispatched: Vec<(String, u32)>,
    /// Set when this superstep opened after a RESUME rather than straight
    /// out of the previous close: the reading the driver took when it picked
    /// the run back up. The span from the previous close to here accrued as
    /// `elapsed_ms` — reported, never billed as wall.
    ///
    /// Journaled because `verify` cannot otherwise know a resume happened:
    /// the driver's fresh reading is not derivable from the journal, so a
    /// replay would open the superstep at the previous close and charge the
    /// crash gap as active wall. Absent on every superstep that did not
    /// resume — and on every checkpoint written before this existed, which
    /// is why it is `Option` with `skip_serializing_if`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resumed_at: Option<u64>,
    /// Decision-guided folds (C2) resolved this superstep — applied or not,
    /// each with the decision's provenance. Empty on every run with no
    /// decision backend pinned, and skipped when empty so those checkpoints
    /// stay byte-identical.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub folds: Vec<FoldRecord>,
}

/// One decision-guided fold (C2), journaled in the superstep's decision
/// record — the `kind: "decide"` variant of a fold. (A summarizer fold's
/// record is its own effect's `input.fold`.)
///
/// Indices are TRANSCRIPT positions as they stood when the decision was
/// asked. `kept` / `truncated` name tool-result entries; `dropped` names
/// every entry removed from the PROMPT — a tool result and, when its whole
/// round went, the assistant entry that issued it. Nothing named here left
/// the journal: every one is still an intent + result grain.
#[derive(Debug, Clone, PartialEq, Default, serde::Serialize, serde::Deserialize)]
pub struct FoldRecord {
    /// Always `"decide"`.
    pub kind: String,
    /// [`crate::step::FOLD_DECIDE_V`] — the questions' wording version.
    pub v: u32,
    pub node: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub task_path: String,
    pub attempt: u32,
    /// The decision effect's own `effect_seq` — where its request and
    /// answer are journaled.
    pub seq: u32,
    /// The foldable window asked about (`from..to`).
    pub from: usize,
    pub to: usize,
    pub kept: Vec<usize>,
    pub truncated: Vec<usize>,
    pub dropped: Vec<usize>,
    /// Whether the transcript was edited. `false` = the fail-open path: the
    /// summarizer fold runs next, exactly as it would have with no backend.
    pub applied: bool,
    /// Why it was not applied (uncalibrated, failed, malformed, nothing to
    /// prune).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// `{provider, model, calibrated, latency_ms}` of the answering backend;
    /// absent when the call failed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provenance: Option<Value>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum EdgeOutcome {
    Fired,
    /// Condition evaluated false.
    CondFalse,
    /// The edge's `max_cycles` is spent — an outcome, not an error.
    CycleExhausted,
    /// Dead-path propagation killed it.
    DeadPath,
}

/// Terminal run outcomes.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum RunOutcome {
    Completed,
    /// No terminal node succeeded; names the all-conditions-false node.
    Stalled { node: String },
    /// A node exhausted its retries (fail-fast v1).
    Failed { node: String, detail: String },
    Canceled { by: String, reason: String },
    BudgetExhausted { axis: BudgetAxis },
}

/// The manifest's budget ceilings (§6.7). Env, not state: raising a budget
/// is a journaled manifest supersession, and resume re-reads it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Budgets {
    pub max_supersteps: u64,
    pub max_tokens: Option<u64>,
    pub max_usd_micros: Option<u64>,
    pub max_wall_ms: Option<u64>,
    pub max_storage_bytes: Option<u64>,
    /// Settled effects across the whole run (#295).
    pub max_effects: Option<u64>,
    /// Host tool calls across the whole run (#295).
    pub max_tool_calls: Option<u64>,
}

impl Default for Budgets {
    fn default() -> Self {
        Budgets {
            // The global backstop (§6.1 V5's companion): even a validly
            // bounded plan cannot run away.
            max_supersteps: 1024,
            max_tokens: None,
            max_usd_micros: None,
            max_wall_ms: None,
            max_storage_bytes: None,
            max_effects: None,
            max_tool_calls: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_call_ids_are_deterministic_and_collision_free() {
        let base = JournalKey {
            run_id: "r".into(),
            task_path: "".into(),
            node: "n".into(),
            attempt: 1,
            effect_seq: 0,
            kind: EffectKind::Tool,
        };
        assert_eq!(base.tool_call_id(), base.tool_call_id(), "reproducible");

        // Every key component separates the id — including the separator
        // ambiguity case ("a","b/c") vs ("a/b","c").
        let mut variants = vec![base.clone()];
        variants.push(JournalKey { attempt: 2, ..base.clone() });
        variants.push(JournalKey { effect_seq: 1, ..base.clone() });
        variants.push(JournalKey { node: "m".into(), ..base.clone() });
        variants.push(JournalKey { kind: EffectKind::Llm, ..base.clone() });
        variants.push(JournalKey { task_path: "0".into(), ..base.clone() });
        variants.push(JournalKey {
            task_path: "a".into(),
            node: "b/c".into(),
            ..base.clone()
        });
        variants.push(JournalKey {
            task_path: "a/b".into(),
            node: "c".into(),
            ..base.clone()
        });
        let ids: std::collections::BTreeSet<String> =
            variants.iter().map(|k| k.tool_call_id()).collect();
        assert_eq!(ids.len(), variants.len(), "all keys yield distinct ids");
        for id in &ids {
            assert_eq!(id.len(), 64, "full sha256 hex");
        }
    }

    #[test]
    fn retryability_follows_the_pinned_table() {
        use EffectKind::*;
        assert!(FailCause::Timeout.retryable(Tool));
        assert!(FailCause::ExecutorError.retryable(Tool));
        assert!(!FailCause::SchemaValidationFailed.retryable(Tool));
        assert!(FailCause::SchemaValidationFailed.retryable(Llm), "the re-prompt path");
        assert!(!FailCause::UserAborted.retryable(Tool));
        assert!(!FailCause::Unknown.retryable(Llm));
    }
}
