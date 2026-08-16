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
}

impl FailCause {
    /// §6.3: Timeout/ExecutorError retryable; SchemaValidationFailed only on
    /// the LLM re-prompt path (Wave 2); UserAborted/Unknown never.
    pub fn retryable(&self, kind: EffectKind) -> bool {
        match self {
            Self::Timeout | Self::ExecutorError => true,
            Self::SchemaValidationFailed => kind == EffectKind::Llm,
            Self::UserAborted | Self::Unknown => false,
        }
    }
}

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
}

/// One tool offered to an abstract node's model, pinned by the manifest.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct OfferedTool {
    pub tool_name: String,
    pub tool_hash: String,
}

/// What executes a node (resolved by the driver's V7 freeze).
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum NodeExecutor {
    /// Host-executed tool; the driver resolved the Definition hash.
    Host { tool_hash: String, tool_name: String },
    /// Client-executed: park and emit a `requires_action` ask.
    Client { tool_hash: String, tool_name: String },
    /// An abstract node (§6.2): the node label is an instruction an LLM
    /// interprets, choosing among the workflow's pinned Host tools. The
    /// node runs as an LLM LOOP — turn, tool calls, results, next turn —
    /// each effect journaled under its own `effect_seq`.
    Abstract { tools: Vec<OfferedTool> },
    /// A subgraph node (Wave 2): the binding resolves to another Workflow
    /// grain; the child runs as its own run (own run_id, own journal),
    /// linked by `parent_task_id`, and its final context becomes this
    /// node's result.
    Subgraph { workflow_hash: String },
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
