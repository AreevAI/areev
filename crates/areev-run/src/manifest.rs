//! The run manifest (§5.3/§6.1 V7): everything a resume must know, written
//! ONCE at run start — the plan hash, the frozen tool resolutions, the
//! budgets, the principal, the redaction posture. Persisted through the
//! facade's `record_run_manifest` shape (a value-level State at epoch 0 +
//! a `run:<id> mg:harness <hash>` Fact), so the manifest replicates with
//! the file and `runs_touching` can join runs to their configuration.
//!
//! **Resolution freeze**: Named/Abstract resolution happens here, once; the
//! outcome is pinned. Resume reads the manifest and NEVER re-resolves — a
//! newer tool definition appearing between pause and resume must not
//! silently change the run.
//!
//! v1 ownership note: the F7 owner-nonce check (resume-of-a-copy detection
//! via the op-log head) needs an op-cursor read API the store does not yet
//! expose; v1 ships taint detection (forked journal tips → `RUN-E016`) and
//! the explicit `--fork` path, and records this as the documented gap.

use crate::err_run;
use areev_core::error::{Hash, Result};
use areev_core::types::Grain;
use areev_run_core::{Budgets, NodeExecutor, PlanGraph, RunError};
use areev_store::Areev;
use serde_json::json;
use std::collections::BTreeMap;

/// §5.4: what a fork descends from. Journal grains of the inherited prefix
/// carry the PARENT's run id; the fork's verifiability is this recorded
/// base-checkpoint hash (the seed checkpoint `derived_from`s it).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ForkBase {
    pub base_run: String,
    pub base_checkpoint: String,
    pub base_superstep: u64,
}

/// One frozen resolution.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PinnedTool {
    pub node: String,
    pub tool_hash: String,
    pub tool_name: String,
    /// "host" | "client" (the definition's `executor_kind`, default host).
    pub executor: String,
    /// The definition's `executor_uri`, when it names a `cas://sha256:` code
    /// blob. Pinned here so a run executes the address it resolved at start
    /// even if the Definition is superseded mid-run — the same resolution
    /// freeze the bindings get. Absent for every ordinary tool, so existing
    /// manifests serialize byte-identically.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub executor_uri: Option<String>,
    /// The Definition's declared runtime ("wasm32-areev"), pinned with the
    /// address so a mid-run supersession cannot re-route a blob from the
    /// sandbox to native exec. Absent = native, and existing manifests
    /// serialize byte-identically.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime: Option<String>,
    /// The declared sandbox limits (`{"fuel", "max_pages"}`), pinned with
    /// the runtime.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime_limits: Option<serde_json::Value>,
    /// The Definition's declared `capabilities` (#101), frozen at start
    /// beside the pinned runtime so the effective set — `declared ∩
    /// host-granted` — replays identically and a mid-run supersession cannot
    /// widen what a module may reach. Absent for every non-capability tool,
    /// so existing manifests serialize byte-identically.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capabilities: Option<serde_json::Value>,
    /// A declared memory read (#255): the plan's normalized `reads` entry for
    /// this node, frozen at start so a superseded plan cannot change what a
    /// running run reads. Present exactly when `executor` is `memory`; absent
    /// for every other pin, so existing manifests serialize byte-identically.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub read: Option<serde_json::Value>,
    /// The Definition's `ask_kind` (#294): `"approval"` (absent = this) or
    /// `"confirmation"`. Valid only on a `client` pin.
    ///
    /// Frozen here beside `executor_uri` and `runtime`, for the same reason:
    /// a mid-run supersession must not be able to downgrade an approval a
    /// person is already parked on. Absent for every ordinary pin, so
    /// existing manifests serialize byte-identically.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ask_kind: Option<String>,
    /// A decision node (C3, `executor_uri: "areev://decide"`): the
    /// Definition's `decide` declaration, normalized and frozen at start —
    /// `{"into": "<state key>", "questions": {<wire questions>}?}`. Present
    /// exactly when `executor` is [`DECIDE_EXECUTOR`]; absent for every other
    /// pin, so existing manifests serialize byte-identically.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decide: Option<serde_json::Value>,
}

/// The `executor` a decision node (C3) pins as.
pub const DECIDE_EXECUTOR: &str = "decide";

/// The decision backend a run STARTED under (C1–C3), frozen like the model.
///
/// What the scheduler may ASK is decided from this, never from the host at
/// hand: `calibrated` gates the fold and the narrowing (proposal rule 2), and
/// a resume or `verify` under a different host must make the same choices
/// the run made. Whoever ANSWERS a later ask is recorded per decision, in the
/// journaled result's own provenance.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct DeciderPin {
    /// `DecisionBackend::describe()`, e.g. `"typesafe:jev-latest+anon"`.
    pub describe: String,
    pub calibrated: bool,
}

/// The manifest, as serialized into the run-config State grain.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct RunManifest {
    pub run_id: String,
    pub plan_hash: String,
    pub principal: String,
    pub pinned: Vec<PinnedTool>,
    pub budgets: BudgetsSpec,
    pub ask_ttl_sec: Option<i64>,
    /// D9: the redaction posture, explicit in every manifest — "none" or a
    /// redactor id — so every audit sees what the journal did or did not
    /// scrub.
    pub redaction: String,
    /// The run's input — §5's function is `(plan, manifest, input, journal)`
    /// and the manifest is where the input is journaled; verify replays
    /// seed from here, never from a checkpoint's post-reducer context.
    pub input: serde_json::Value,
    /// Per-LLM-call `max_tokens` — both the request ceiling handed to the
    /// model and the §6.7 per-dispatch reservation the scheduler checks
    /// before emitting an LLM effect. This is an OUTPUT ceiling; nothing here
    /// bounds how large a transcript may grow.
    #[serde(default)]
    pub llm_max_tokens: Option<u32>,
    /// Effects one node attempt may spend (an abstract node's turns, tool
    /// calls and re-prompts share the counter). Frozen here so a resume
    /// bounds the loop exactly as the start did, and absent on every manifest
    /// written before the knob existed — which is why it is `Option` with
    /// `skip_serializing_if`: those manifests must serialize byte-identically.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_effects_per_attempt: Option<u32>,
    /// Bound on ONE tool result's size in an abstract node's TRANSCRIPT, in
    /// characters. The journal keeps every result in full regardless — this
    /// bounds only what the model is shown. `None` = unbounded, the behaviour
    /// of every run before the knob existed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub llm_tool_result_chars: Option<usize>,
    /// Ceiling on an abstract node's WHOLE transcript, in the provider's own
    /// reported prompt tokens. Reaching it emits one journaled summarizer turn
    /// and splices its result over the folded range. `None` = no ceiling, the
    /// behaviour of every run before the fold existed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub llm_context_tokens: Option<u64>,
    /// Typed reducers (§6.5): state_key → builtin reducer name, read off
    /// the Workflow grain's `reducers` field and FROZEN here — a resume
    /// must merge exactly as the original run did. Undeclared keys are LWW.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub reducers: BTreeMap<String, String>,
    /// Present exactly when this run is a §5.4 fork.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fork_of: Option<ForkBase>,
    /// Who or what this run was started ON BEHALF OF (#293).
    ///
    /// Event, poll and schedule runs execute under an agent's SERVICE
    /// principal, so `principal` alone cannot name the person behind the
    /// work — and the approval check, which compares a responder against
    /// `principal`, could not refuse them. Free-form attribution: a value
    /// that names no principal (a trigger occurrence id, say) simply never
    /// matches a responder.
    ///
    /// Frozen here so a park-and-resume days later judges against the same
    /// answer. Absent on every manifest written before this existed, and
    /// `skip_serializing_if` keeps those byte-identical.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub initiator: Option<String>,
    /// The model configuration this run STARTED under (#287).
    ///
    /// Everything else that shapes a run is frozen — tool resolutions,
    /// runtime, capabilities, reads, reducers, every LLM ceiling — but the
    /// model was not, so a run parked on a human approval could finish days
    /// later on a different model, provider or region with nothing in the
    /// journal saying so. `resume` compares this against the transport it is
    /// offered and refuses a mismatch; `fork` is the sanctioned way to move a
    /// run to a new model.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub llm: Option<LlmPin>,
    /// The engine that WROTE this run (#288).
    ///
    /// Investment-adviser records are kept five years and more, and a
    /// scheduler change can make an older journal replay differently — 1.8.3
    /// (#251) is the recorded case. A verifier holding such a run sees a
    /// divergence and cannot tell tampering from "written by 1.8.2". This
    /// makes the run say so itself.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub engine: Option<EnginePin>,
    /// Where this run's INPUT lives (#301).
    ///
    /// `None` (the default) means by value in [`input`](Self::input), which
    /// puts it in the memory-wide `agent:harness` namespace — outside the
    /// grants, retention and erasure of the namespace the run belongs to.
    /// `Some(hash)` means the input is its own grain in the run's own
    /// namespace and this is its address.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_ref: Option<String>,
    /// The namespace this run's CONTENT-BEARING harness records go to
    /// (#301): fold summaries, egress call records, blob reads.
    ///
    /// `None` keeps them in `agent:harness` as before. `Some(ns)` — set by
    /// the host at start and frozen here — writes them to
    /// `agent:harness.<run_ns>`, which keeps them out of the agent's own
    /// recall scope (the reason they are not simply written to the run
    /// namespace) while making one namespace's run evidence separately
    /// grantable, retainable and erasable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub harness_ns: Option<String>,
    /// Whether this run may settle a `confirmation` ask from its own
    /// initiator (#294). Host opt-in, frozen so a mid-run change cannot
    /// weaken a parked approval.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub allow_confirmation_asks: bool,
    /// The decision backend this run started under (C1–C3), or `None` — which
    /// is every run before decisions existed, and every run on a host without
    /// one: the scheduler then asks nothing, byte-for-byte as before.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decider: Option<DeciderPin>,
}

/// The model configuration a run is pinned to (#287).
///
/// `tag` is an opaque host string — a configuration hash, typically — so a
/// host wrapping its own transport can have Areev enforce the host's own
/// notion of "the same configuration", without Areev having to model it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct LlmPin {
    pub provider: String,
    pub model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub region: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tag: Option<String>,
    /// Content address of the request profile (#285) — what was actually
    /// sent is part of what a run ran under.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
}

impl LlmPin {
    /// Whether `other` is the same configuration this run was pinned to.
    pub fn matches(&self, other: &LlmPin) -> bool {
        self == other
    }

    /// One-line rendering for an error message.
    pub fn describe(&self) -> String {
        let mut s = format!("{}:{}", self.provider, self.model);
        if let Some(r) = &self.region {
            s.push_str(&format!(" region={r}"));
        }
        if let Some(t) = &self.tag {
            s.push_str(&format!(" tag={t}"));
        }
        if let Some(p) = &self.profile {
            s.push_str(&format!(" profile={}", &p[..p.len().min(12)]));
        }
        s
    }
}

/// The engine that wrote a run (#288).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct EnginePin {
    pub version: String,
    pub scheduler_epoch: u32,
}

impl EnginePin {
    /// This build's pin.
    pub fn current() -> Self {
        EnginePin {
            version: env!("CARGO_PKG_VERSION").to_string(),
            scheduler_epoch: areev_run_core::SCHEDULER_EPOCH,
        }
    }
}

/// Serializable twin of the core's `Budgets` (kept separate so the manifest
/// wire shape is owned here, not by the pure crate).
#[derive(Debug, Clone, Copy, Default, serde::Serialize, serde::Deserialize)]
pub struct BudgetsSpec {
    pub max_supersteps: Option<u64>,
    pub max_tokens: Option<u64>,
    pub max_usd_micros: Option<u64>,
    pub max_wall_ms: Option<u64>,
    pub max_storage_bytes: Option<u64>,
    /// Settled effects across the whole run (#295). `skip_serializing_if`
    /// because `BudgetsSpec` serializes nulls today and the manifest golden
    /// pins that shape — a new always-present key would change every
    /// manifest's bytes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_effects: Option<u64>,
    /// Host tool calls across the whole run (#295).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tool_calls: Option<u64>,
}

impl BudgetsSpec {
    pub fn to_budgets(self) -> Budgets {
        Budgets {
            max_supersteps: self.max_supersteps.unwrap_or(Budgets::default().max_supersteps),
            max_tokens: self.max_tokens,
            max_usd_micros: self.max_usd_micros,
            max_wall_ms: self.max_wall_ms,
            max_storage_bytes: self.max_storage_bytes,
            max_effects: self.max_effects,
            max_tool_calls: self.max_tool_calls,
        }
    }
}

/// A single tool result may occupy at most this fraction of the context
/// ceiling. A quarter is a heuristic and named as one: large enough that an
/// ordinary file read arrives whole, small enough that one dump cannot carry
/// the next request past the window on its own.
const RESULT_SHARE_OF_CEILING: u64 = 4;

/// The runtime's characters-per-token conversion, used ONLY to turn a token
/// ceiling into a character bound — never to measure a transcript, which is
/// always the provider's own reported count. Same ratio the store's estimator
/// uses, and the same honesty applies: it converts a limit, it does not
/// pretend to count tokens.
const CHARS_PER_TOKEN: u64 = 4;

impl RunManifest {
    /// Freeze resolutions for every node (V3 + V7):
    /// - **Read** (a `reads` declaration, #255): a `memory` executor the
    ///   driver answers from the store; the node must bind nothing.
    /// - **Bound** (a `bindings` hash): must resolve to a Tool Definition.
    /// - **Named** (no binding): a Definition head whose `tool_name` equals
    ///   the node id, found via the definition catalogue.
    /// - **Abstract** (neither): Wave 2 (`RUN-E006` until then).
    // Mirrored to the CLI/MCP surfaces like `record_tool_call`: grouped
    // params would make the scalar-in mirrors diverge.
    #[allow(clippy::too_many_arguments)]
    pub fn resolve(
        m: &mut Areev,
        ns: &str,
        run_id: &str,
        plan_hash: &Hash,
        plan: &PlanGraph,
        principal: &str,
        budgets: BudgetsSpec,
        ask_ttl_sec: Option<i64>,
        input: serde_json::Value,
        llm_available: bool,
    ) -> std::result::Result<RunManifest, RunError> {
        let fields: Option<serde_json::Map<String, serde_json::Value>> =
            m.get(plan_hash).ok().map(|g| g.fields.into_iter().collect());
        Self::resolve_with_fields(
            m,
            ns,
            run_id,
            plan_hash,
            fields.as_ref(),
            plan,
            principal,
            budgets,
            ask_ttl_sec,
            input,
            llm_available,
        )
    }

    /// [`resolve`](Self::resolve) against plan fields the caller already
    /// holds — a DRAFT body that is not in the store (`shadow --plan-file`, a
    /// loop-drafted `plan_revision`). The graph alone does not say how a run
    /// executes: `reducers` decide merges and `reads` decide which nodes the
    /// runtime answers, so a rehearsal that resolved the graph without them
    /// would rehearse a different plan. `None` = no plan fields at all.
    #[allow(clippy::too_many_arguments)]
    pub fn resolve_with_fields(
        m: &mut Areev,
        ns: &str,
        run_id: &str,
        plan_hash: &Hash,
        fields: Option<&serde_json::Map<String, serde_json::Value>>,
        plan: &PlanGraph,
        principal: &str,
        budgets: BudgetsSpec,
        ask_ttl_sec: Option<i64>,
        input: serde_json::Value,
        llm_available: bool,
    ) -> std::result::Result<RunManifest, RunError> {
        // Where the plan itself lives. Used only when a node fails to
        // resolve: a plan run in a namespace that is not its own is the
        // misroute #230 reported, and the plan grain is the one thing that
        // knows where its Definitions were authored.
        let plan_ns = fields
            .and_then(|f| f.get("namespace"))
            .and_then(|n| n.as_str())
            .map(str::to_string);
        // Declared memory reads (#255), validated in full before any node
        // resolves: a read answered by the runtime binds nothing, and a
        // malformed or out-of-scope declaration refuses the run here.
        let mut reads = crate::memread::parse_reads(
            fields.and_then(|f| f.get(crate::memread::READS_FIELD)),
            plan,
            ns,
        )?;
        let mut pinned = Vec::with_capacity(plan.nodes.len());
        for (i, node) in plan.nodes.iter().enumerate() {
            if let Some(spec) = reads.remove(node) {
                let op = spec.get("op").and_then(|o| o.as_str()).unwrap_or_default();
                pinned.push(PinnedTool {
                    node: node.clone(),
                    tool_hash: String::new(),
                    tool_name: format!("mg:{op}"),
                    executor: crate::memread::MEMORY_EXECUTOR.into(),
                    executor_uri: None,
                    runtime: None,
                    runtime_limits: None,
                    capabilities: None,
                    read: Some(spec),
                    ask_kind: None,
                    decide: None,
                });
                continue;
            }
            let resolved = match &plan.bindings[i] {
                Some(hash_str) => {
                    let h = Hash::from_hex(hash_str).map_err(|_| RunError::UnresolvedRef {
                        what: format!("binding for node '{node}' is not a content address"),
                    })?;
                    let g = m.get_stored(&h).map_err(|e| RunError::UnresolvedRef {
                        what: format!("binding for node '{node}': {e}"),
                    })?;
                    if g.grain_type == areev_core::types::GrainType::Workflow {
                        // A binding to another plan = a subgraph node.
                        PinnedTool {
                            node: node.clone(),
                            tool_hash: h.to_hex(),
                            tool_name: node.clone(),
                            executor: "subgraph".into(),
                            executor_uri: None,
                            runtime: None,
                            runtime_limits: None,
                            capabilities: None,
                            read: None,
                            ask_kind: None,
                            decide: None,
                        }
                    } else {
                        pin_from_definition(node, &h, &g)?
                    }
                }
                None => {
                    // Named: the newest Definition with tool_name == node;
                    // otherwise ABSTRACT (§6.2) — the node label is an
                    // instruction the model interprets — which needs the
                    // tool-calling seam configured.
                    match find_definition_by_name(m, ns, node) {
                        Some((h, g)) => pin_from_definition(node, &h, &g)?,
                        // The misroute FIRST, before either fallback (#230).
                        // A node whose name matches a real Definition sitting
                        // in the plan's own namespace is a run started in the
                        // wrong place, not an instruction for a model: its
                        // `executor_uri`, runtime and capabilities would all
                        // be silently dropped, and with an LLM configured the
                        // node would quietly become an abstract one and the
                        // run would report Completed having called nothing.
                        // Costs one extra catalogue scan, and only on a miss.
                        None if misrouted(m, ns, plan_ns.as_deref(), node) => {
                            let pns = plan_ns.as_deref().unwrap_or_default();
                            return Err(RunError::UnresolvedRef {
                                what: format!(
                                    "node '{node}' has no binding, and namespace '{ns}' — where \
                                     this run reads and journals — holds no Tool Definition \
                                     named '{node}'. The plan grain itself lives in namespace \
                                     '{pns}', which does. Start the run there (`--ns {pns}`), \
                                     or bind the node to a Definition by hash so it resolves \
                                     from any namespace"
                                ),
                            });
                        }
                        None if llm_available => PinnedTool {
                            node: node.clone(),
                            tool_hash: String::new(),
                            tool_name: node.clone(),
                            executor: "abstract".into(),
                            executor_uri: None,
                            runtime: None,
                            runtime_limits: None,
                            capabilities: None,
                            read: None,
                            ask_kind: None,
                            decide: None,
                        },
                        None => return Err(RunError::NoToolLlm { node: node.clone() }),
                    }
                }
            };
            pinned.push(resolved);
        }
        // Reducer table (§6.5): declared on the Workflow grain, validated
        // here — an unknown name fails at run start, not at first merge.
        let mut reducers: BTreeMap<String, String> = BTreeMap::new();
        if let Some(fields) = fields {
            if let Some(table) = fields.get("reducers").and_then(|v| v.as_object()) {
                for (key, name) in table {
                    let Some(name) = name.as_str() else {
                        return Err(RunError::InvalidPlan {
                            why: format!("reducer for state key '{key}' is not a string"),
                        });
                    };
                    if !crate::reducers::is_builtin(name) {
                        return Err(RunError::InvalidPlan {
                            why: format!(
                                "unknown reducer '{name}' for state key '{key}' \
                                 (builtins: {:?})",
                                crate::reducers::BUILTIN_REDUCERS
                            ),
                        });
                    }
                    reducers.insert(key.clone(), name.to_string());
                }
            }
        }
        Ok(RunManifest {
            run_id: run_id.to_string(),
            plan_hash: plan_hash.to_hex(),
            principal: principal.to_string(),
            pinned,
            budgets,
            ask_ttl_sec,
            redaction: "none".into(),
            input,
            llm_max_tokens: None,
            max_effects_per_attempt: None,
            llm_tool_result_chars: None,
            llm_context_tokens: None,
            reducers,
            fork_of: None,
            initiator: None,
            llm: None,
            engine: None,
            input_ref: None,
            harness_ns: None,
            allow_confirmation_asks: false,
            decider: None,
        })
    }

    /// Freeze the model configuration this run starts under (#287).
    ///
    /// A builder beside [`with_limits`](Self::with_limits) rather than a new
    /// `resolve` argument, for the reason that method already gives: none of
    /// this affects resolution, and an eleventh positional argument is how
    /// two call sites come to disagree about which knobs they passed.
    pub fn with_llm_pin(mut self, pin: Option<LlmPin>) -> Self {
        self.llm = pin;
        self
    }

    /// Freeze the host's decision backend (C1–C3) — and refuse, naming the
    /// node, a plan that binds a decision node when there is none
    /// (`RUN-E030`).
    ///
    /// A builder beside [`with_llm_pin`](Self::with_llm_pin), for the reason
    /// that method gives, but fallible: this is the V7 half of resolution for
    /// decision nodes. Called right after `resolve` on every path that makes
    /// a run (start, a migrating fork), so the refusal lands before the run
    /// exists — never as a node failing mid-run.
    pub fn with_decider_pin(mut self, pin: Option<DeciderPin>) -> std::result::Result<Self, RunError> {
        if pin.is_none() {
            if let Some(p) = self.pinned.iter().find(|p| p.executor == DECIDE_EXECUTOR) {
                return Err(RunError::NoDecider { node: p.node.clone() });
            }
        }
        self.decider = pin;
        Ok(self)
    }

    /// Stamp the engine that is writing this run (#288).
    pub fn with_engine_pin(mut self) -> Self {
        self.engine = Some(EnginePin::current());
        self
    }

    /// Freeze the initiator (#293), the input placement and the harness
    /// namespace (#301), and the confirmation opt-in (#294).
    pub fn with_attribution(mut self, opts: &crate::RunOptions) -> Self {
        self.initiator = opts.initiator.clone();
        self.harness_ns = opts.harness_ns.clone();
        self.allow_confirmation_asks = opts.allow_confirmation_asks;
        self
    }

    /// Record that the input lives in its own grain (#301).
    pub fn with_input_ref(mut self, hash: String) -> Self {
        self.input_ref = Some(hash);
        // The manifest no longer carries the content itself. `input` keeps a
        // `#[serde(default)]` so a manifest with neither still loads.
        self.input = serde_json::Value::Null;
        self
    }

    /// Freeze the caller's run-level LLM limits into the manifest.
    ///
    /// Deliberately NOT arguments to [`RunManifest::resolve`]: not one of them
    /// affects resolution — they are ceilings the scheduler reads back on
    /// every resume and verify — and threading each through an
    /// already-ten-argument signature would make the next one an eleventh,
    /// with two call sites free to disagree about which knobs they passed.
    /// One place to add a limit, one place that can forget it.
    pub fn with_limits(mut self, opts: &crate::RunOptions, model_window: Option<u64>) -> Self {
        self.llm_max_tokens = opts.llm_max_tokens;
        self.max_effects_per_attempt = opts.max_effects_per_attempt;
        // The ceiling the operator set — or, failing that, one derived from
        // the model's own window. This is what makes folding automatic: a long
        // agent should not need a flag to survive, and a host that knows the
        // window can answer the question the operator would have had to.
        //
        // Reserved output comes off the top, because a prompt that fits and a
        // reply that does not is the same rejection. A backend that reports no
        // window leaves this None and relies on the reactive path instead.
        self.llm_context_tokens = opts.llm_context_tokens.or_else(|| {
            model_window.map(|w| w.saturating_sub(u64::from(self.llm_max_tokens.unwrap_or(
                crate::runner::DEFAULT_LLM_MAX_TOKENS,
            ))))
        });
        // And the per-result bound follows the ceiling, because a ceiling on
        // its own is not safe: the fold measures the PREVIOUS turn, so one
        // oversized result appended since can carry the next request past the
        // window before anything checks. Deriving it means an operator who
        // sets one bound gets the other, rather than discovering the gap.
        self.llm_tool_result_chars = opts.llm_tool_result_chars.or_else(|| {
            self.llm_context_tokens
                .map(|c| ((c / RESULT_SHARE_OF_CEILING) * CHARS_PER_TOKEN) as usize)
        });
        self
    }

    /// The per-dispatch token reserve (§6.7) — the scheduler refuses to
    /// emit an LLM effect that could not fit under the token ceiling.
    pub fn llm_reserve_tokens(&self) -> u64 {
        u64::from(self.llm_max_tokens.unwrap_or(crate::runner::DEFAULT_LLM_MAX_TOKENS))
    }

    /// Effects one node attempt may spend, as the scheduler's `StepEnv` reads
    /// it. Every `StepEnv` the driver builds — drive, verify — takes it from
    /// here, so a resumed or replayed run is bounded exactly as the start was.
    pub fn max_effects_per_attempt(&self) -> u32 {
        self.max_effects_per_attempt
            .unwrap_or(areev_run_core::DEFAULT_MAX_EFFECTS_PER_ATTEMPT)
    }

    /// The executor vector the scheduler env consumes, in node order. An
    /// abstract node's offered tools are the manifest's HOST-pinned tools —
    /// derived, not stored, so the manifest stays minimal and the offer is
    /// deterministic from it.
    pub fn executors(&self) -> Vec<NodeExecutor> {
        // ONE OFFER PER DEFINITION, however many nodes bind it. `pinned` holds one
        // entry per plan node, so a Definition bound to two nodes — the ordinary
        // shape for a terminal step reached from two branches — appeared twice in
        // the offer, and every provider refuses a tools list with a repeated name
        // ("tools: Tool names must be unique", HTTP 400 on the whole request; #270).
        // First occurrence wins, so the offer's order stays that of the plan.
        let mut seen = std::collections::HashSet::new();
        let offered: Vec<areev_run_core::OfferedTool> = self
            .pinned
            .iter()
            .filter(|p| p.executor == "host")
            .filter(|p| seen.insert(p.tool_name.clone()))
            .map(|p| areev_run_core::OfferedTool {
                tool_name: p.tool_name.clone(),
                tool_hash: p.tool_hash.clone(),
            })
            .collect();
        self.pinned
            .iter()
            .map(|p| match p.executor.as_str() {
                "client" => NodeExecutor::Client {
                    tool_hash: p.tool_hash.clone(),
                    tool_name: p.tool_name.clone(),
                    // Absent means approval — the stricter reading (#294).
                    approval: p.ask_kind.as_deref() != Some(ASK_KIND_CONFIRMATION),
                },
                "subgraph" => NodeExecutor::Subgraph { workflow_hash: p.tool_hash.clone() },
                // Never Host, even for a pin whose declaration went missing:
                // a read that fell through to `--tool-cmd` would let a tool
                // answer it (#255). A malformed spec fails at execution.
                crate::memread::MEMORY_EXECUTOR => {
                    let spec = p.read.clone().unwrap_or(serde_json::Value::Null);
                    NodeExecutor::MemoryRead {
                        op: spec.get("op").and_then(|o| o.as_str()).unwrap_or_default().to_string(),
                        spec,
                    }
                }
                "abstract" => NodeExecutor::Abstract { tools: offered.clone() },
                // Never Host, for the memory-read reason: a decision that
                // fell through to `--tool-cmd` would let a tool answer it.
                DECIDE_EXECUTOR => NodeExecutor::Decide {
                    tool_hash: p.tool_hash.clone(),
                    tool_name: p.tool_name.clone(),
                },
                _ => NodeExecutor::Host {
                    tool_hash: p.tool_hash.clone(),
                    tool_name: p.tool_name.clone(),
                },
            })
            .collect()
    }

    /// Persist via the established run-manifest shape (a value-level State
    /// at epoch 0 + the `run:<id> mg:harness <hash>` link Fact in
    /// `agent:harness`). Store-direct like every other runtime control
    /// write: the driver's `run.execute` check is THE authorization decision
    /// for starting a run — requiring executor principals to additionally
    /// hold `write ON agent:harness` would make every governed deployment
    /// hand out grants on a governance namespace. Attribution still lands on
    /// the grains (`author_did`). The link also carries the run's session
    /// namespace (`run_ns`), which is where its journal lives — the run index
    /// is what `areev run list` and the console page through, and without
    /// the namespace on it a per-tenant listing could only be filtered
    /// after the fact, over an already-truncated page (#165). Returns
    /// (config hash, link hash).
    pub fn persist_in_namespace(&self, m: &mut Areev, ns: &str) -> Result<(Hash, Hash)> {
        self.persist_with_input(m, ns, None)
    }

    /// Persist the manifest, optionally storing the run's INPUT as its own
    /// grain in the run's namespace first (#301).
    ///
    /// The manifest is written to the memory-wide `agent:harness`, and it
    /// carries the input BY VALUE — so `read ON agent:harness`, which
    /// anything that lists or inspects runs needs, disclosed the inputs of
    /// every namespace in the memory, retention and erasure of a namespace
    /// left them behind, and no grant could say "the run records of this
    /// namespace only".
    ///
    /// Under `input_ns`, the input is a State grain in that namespace and the
    /// manifest keeps only its address. `input` then serializes as `null`,
    /// which is why it carries `#[serde(default)]`: a manifest with an
    /// `input_ref` and no value must still load.
    pub fn persist_with_input(
        &self,
        m: &mut Areev,
        ns: &str,
        input_ns: Option<&str>,
    ) -> Result<(Hash, Hash)> {
        let mut manifest = self.clone();
        if let Some(input_ns) = input_ns {
            let mut grain = areev_core::types::State::new(self.input.clone())
                .namespace(input_ns)
                .created_at(0);
            grain.common.author_did = Some(self.principal.clone());
            grain
                .common
                .extra_fields
                .insert("run_id".into(), serde_json::json!(self.run_id));
            let h = m.add(&grain)?;
            manifest = manifest.with_input_ref(h.to_hex());
        }
        let this = &manifest;
        let config = serde_json::to_value(json!({ "areev_run": this }))
            .expect("manifest serializes");
        let mut state = areev_core::types::State::new(config)
            .namespace(areev_core::authz::HARNESS_NS)
            .created_at(0);
        state.common.author_did = Some(self.principal.clone());
        let config_hash = m.add(&state)?;
        let mut link = areev_core::types::Fact::new(
            &format!("run:{}", self.run_id),
            "mg:harness",
            &config_hash.to_hex(),
        )
        .namespace(areev_core::authz::HARNESS_NS);
        link.common.author_did = Some(self.principal.clone());
        link.common
            .extra_fields
            .insert("run_id".into(), serde_json::json!(self.run_id));
        if !ns.is_empty() {
            link.common.extra_fields.insert("run_ns".into(), serde_json::json!(ns));
        }
        let link_hash = m.add(&link)?;
        Ok((config_hash, link_hash))
    }

    /// The pre-#165 signature, kept so a host that persists manifests itself
    /// keeps compiling. It stamps no session namespace, so the run lists as
    /// unattributed — visible unscoped, excluded and counted under any
    /// scope. Prefer [`persist_in_namespace`](Self::persist_in_namespace).
    #[deprecated(
        note = "use persist_in_namespace(m, ns) so the run index can be scoped by namespace"
    )]
    pub fn persist(&self, m: &mut Areev) -> Result<(Hash, Hash)> {
        self.persist_in_namespace(m, "")
    }

    /// Load a run's manifest back from the file (resume/verify path).
    pub fn load(m: &mut Areev, run_id: &str) -> std::result::Result<RunManifest, RunError> {
        let link = m
            .latest(areev_core::authz::HARNESS_NS, &format!("run:{run_id}"), "mg:harness")
            .map_err(err_run)?
            .ok_or_else(|| RunError::ManifestMismatch {
                why: format!("no manifest link for run '{run_id}'"),
            })?;
        let config_hash = link
            .get_str("object")
            .and_then(|h| Hash::from_hex(h).ok())
            .ok_or_else(|| RunError::ManifestMismatch {
                why: "manifest link carries no config hash".into(),
            })?;
        let config = m.get_stored(&config_hash).map_err(|e| RunError::ManifestMismatch {
            why: format!("manifest config unreadable: {e}"),
        })?;
        let manifest = config
            .fields
            .get("context")
            .or_else(|| config.fields.get("context_data"))
            .and_then(|c| c.get("areev_run"))
            .cloned()
            .ok_or_else(|| RunError::ManifestMismatch {
                why: "config State carries no areev_run manifest".into(),
            })?;
        let mut manifest: RunManifest =
            serde_json::from_value(manifest).map_err(|e| RunError::ManifestMismatch {
                why: format!("manifest does not parse: {e}"),
            })?;
        manifest.resolve_input(m)?;
        Ok(manifest)
    }

    /// Hydrate an input stored by reference (#301).
    ///
    /// A missing input grain is a REFUSAL, not a `null` input: replaying a
    /// run against `null` because its input was erased would produce a
    /// different run wearing the same id, and `verify` would call it a
    /// journal integrity failure rather than a missing premise.
    fn resolve_input(&mut self, m: &mut Areev) -> std::result::Result<(), RunError> {
        let Some(hex) = self.input_ref.clone() else {
            return Ok(());
        };
        let h = Hash::from_hex(&hex).map_err(|_| RunError::ManifestMismatch {
            why: format!("input_ref {hex:?} is not a content address"),
        })?;
        let grain = m.get_stored(&h).map_err(|_| RunError::UnresolvedRef {
            what: format!(
                "run '{}' stores its input as grain {hex}, which is no longer readable \
                 — the input a run replays from cannot be reconstructed, so verify and \
                 resume refuse rather than replay against a different run",
                self.run_id
            ),
        })?;
        self.input = grain
            .fields
            .get("context")
            .or_else(|| grain.fields.get("context_data"))
            .cloned()
            .unwrap_or(serde_json::Value::Null);
        Ok(())
    }
}

/// Read a Tool Definition into the pinned shape a run (or a trigger's
/// connector, #185) executes under: name, executor kind, `executor_uri`,
/// runtime, limits and capability declaration, each either dispatchable or
/// refused by name.
///
/// `label` is what a refusal calls the caller — a node name on the run path,
/// the connector name on the trigger path. Public because the trigger
/// evaluator resolves a connector Definition the same way and must reach the
/// same verdict: two readers of one declaration would drift the first time
/// either moved, and the one that drifts quietly is the heartbeat nobody
/// watches.
pub fn pin_from_definition(
    label: &str,
    h: &Hash,
    g: &areev_core::format::deserialize::DeserializedGrain,
) -> std::result::Result<PinnedTool, RunError> {
    let node = label;
    if g.get_str("kind") != Some("definition") {
        return Err(RunError::UnresolvedRef {
            what: format!(
                "node '{node}' binds {} which is not a Tool Definition \
                 (kind = {:?})",
                h.to_hex(),
                g.get_str("kind").unwrap_or("execution")
            ),
        });
    }
    let tool_name = g
        .get_str("tool_name")
        .unwrap_or(node)
        .to_string();
    // Refuse a malformed tool name at resolve time, before the run starts.
    // This name reaches a host tool as `$AREEV_TOOL_NAME`, and it arrives from
    // a plan grain — which may have been imported from a bundle whose author we
    // do not vouch for (import verifies content integrity, not authorship). It
    // is not shell-interpolated, so this is not injection; the point is that an
    // attacker-influenced string with newlines or shell metacharacters should
    // never reach a child's environment, because a perfectly ordinary tool
    // script that does something like `eval "$AREEV_TOOL_NAME"` turns it into
    // one. Failing here also fails loudly and early rather than mid-superstep.
    if !areev_core::types::json_schema_subset::is_valid_tool_name(&tool_name) {
        return Err(RunError::UnresolvedRef {
            what: format!(
                "node '{node}' binds {} whose tool_name {tool_name:?} is not a valid \
                 tool name (1-64 chars of [A-Za-z0-9_.-])",
                h.to_hex()
            ),
        });
    }
    let executor = match g.get_str("executor_kind") {
        Some("client") => "client",
        _ => "host",
    };
    // A decision node (C3): the reserved URI names the HOST's decision
    // backend, not code. A client tool naming it is refused below with every
    // other client `executor_uri`.
    let decide_node =
        executor == "host" && g.get_str("executor_uri") == Some(areev_run_core::DECIDE_URI);
    // `executor_uri` used to be written, parsed, CAL-buildable — and read by
    // nothing, so a Definition naming `executor://crm.lookup@v3` executed
    // whatever `--tool-cmd` happened to be. That is the failure with no
    // symptom this runtime exists to refuse, so every value now either
    // dispatches or is refused by name.
    let executor_uri = match g.get_str("executor_uri") {
        None => None,
        Some(uri) if executor == "client" => {
            return Err(RunError::CodeExecRefused {
                condition: format!(
                    "node '{node}' binds a client tool carrying executor_uri {uri:?} — a \
                     client tool is answered by a person through `run respond`, so it has \
                     no executor to name"
                ),
            })
        }
        Some(_) if decide_node => None,
        Some(uri) => {
            let hex = crate::executor::strip_cas(uri);
            if hex.len() != 64 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
                return Err(RunError::CodeExecRefused {
                    condition: format!(
                        "node '{node}' names executor_uri {uri:?}, which this build cannot \
                         dispatch — the only executable form is a content address, \
                         cas://sha256:<64 hex>"
                    ),
                });
            }
            Some(format!("cas://sha256:{}", hex.to_ascii_lowercase()))
        }
    };
    // The declared runtime (#86). Fail closed on both axes: a runtime on a
    // tool with no code blob describes nothing, and an unknown runtime —
    // possible on a grain that arrived by sync from another implementation,
    // since our own write path refuses it — must not silently fall back to
    // native exec, which would run wasm bytes as a program.
    let runtime = match g.get_str("runtime") {
        None | Some("native") => None,
        Some(rt) if decide_node => {
            return Err(RunError::CodeExecRefused {
                condition: format!(
                    "node '{node}' is a decision node ({}) but declares runtime {rt:?} — \
                     the host's decision backend answers it, and there is no code to run",
                    areev_run_core::DECIDE_URI
                ),
            })
        }
        Some(rt) if executor_uri.is_none() => {
            return Err(RunError::CodeExecRefused {
                condition: format!(
                    "node '{node}' declares runtime {rt:?} but names no executor_uri —                      a runtime routes a code blob, and there is none"
                ),
            })
        }
        Some(rt) if crate::executor::is_sandbox_runtime(rt) => Some(rt.to_string()),
        Some(rt) => {
            return Err(RunError::CodeExecRefused {
                condition: format!(
                    "node '{node}' declares runtime {rt:?}, which this build cannot                      dispatch — accepted: native, wasm32-areev, wasm32-areev-io"
                ),
            })
        }
    };
    let runtime_limits = if runtime.is_some() { g.fields.get("runtime_limits").cloned() } else { None };
    // #339: the brokered-transfer ceilings, refused at start — before any
    // upstream I/O — when malformed, zero or above the hard maximum. A grain
    // can arrive by sync or in a pack, so the write path is not the only way in.
    areev_core::types::capability::validate_transfer_limits(runtime_limits.as_ref())
        .map_err(|detail| RunError::TransferLimitInvalid { node: node.to_string(), detail })?;
    // The declared capability set (#101). Fail closed on both axes, the same
    // way the runtime does: a declaration on a tool whose runtime cannot honour
    // it describes nothing, and a malformed one must be refused at start rather
    // than at the first call — a grain can arrive by sync from another
    // implementation, and our own write path is not the only way in.
    let capabilities = match g.fields.get("capabilities") {
        None => None,
        Some(v) if v.is_null() => None,
        Some(v) => {
            if !crate::executor::runtime_allows_capabilities(runtime.as_deref()) {
                return Err(RunError::CodeExecRefused {
                    condition: format!(
                        "node '{node}' declares capabilities but its runtime is {:?} — \
                         capabilities require runtime \"wasm32-areev-io\"",
                        runtime.as_deref().unwrap_or("native")
                    ),
                });
            }
            areev_core::types::capability::Declaration::parse(v).map_err(|e| RunError::CodeExecRefused {
                condition: format!("node '{node}' declares malformed capabilities: {e}"),
            })?;
            Some(v.clone())
        }
    };
    // The inverse: a capability RUNTIME with nothing declared can reach
    // nothing, and saying so at start beats a module that instantiates and
    // then has every call refused.
    if crate::executor::runtime_allows_capabilities(runtime.as_deref()) && capabilities.is_none() {
        return Err(RunError::CodeExecRefused {
            condition: format!(
                "node '{node}' declares runtime \"wasm32-areev-io\" but no capabilities — \
                 a capability runtime with an empty declaration can reach nothing; \
                 use \"wasm32-areev\" for a pure module"
            ),
        });
    }
    // `ask_kind` (#294). Valid only on a client tool — an approval boundary
    // is a PERSON answering, and a host tool has no person — and refused by
    // name for any other value, the way an unknown runtime is: a grain can
    // arrive by sync or in a pack from an implementation we do not vouch
    // for, and a value this build does not understand must not silently
    // fall through to the weaker reading.
    let ask_kind = match g.get_str("ask_kind") {
        None | Some(ASK_KIND_APPROVAL) => None,
        Some(_) if executor != "client" => {
            return Err(RunError::InvalidPlan {
                why: format!(
                    "node '{node}' declares ask_kind on a {executor} tool — only a \
                     client tool is answered by a person, so only a client tool has \
                     an ask kind"
                ),
            })
        }
        Some(ASK_KIND_CONFIRMATION) => Some(ASK_KIND_CONFIRMATION.to_string()),
        Some(other) => {
            return Err(RunError::InvalidPlan {
                why: format!(
                    "node '{node}' declares ask_kind {other:?}, which this build does \
                     not understand — accepted: {ASK_KIND_APPROVAL}, \
                     {ASK_KIND_CONFIRMATION}"
                ),
            })
        }
    };
    let decide = if decide_node { Some(parse_decide(node, g.fields.get("decide"))?) } else { None };
    Ok(PinnedTool {
        node: node.to_string(),
        tool_hash: h.to_hex(),
        tool_name,
        executor: if decide_node { DECIDE_EXECUTOR.into() } else { executor.into() },
        executor_uri,
        runtime,
        runtime_limits,
        capabilities,
        read: None,
        ask_kind,
        decide,
    })
}

/// Validate and normalize a decision node's `decide` declaration (C3):
/// `{"questions": {<wire questions>}, "into": "<state key>"}`, both optional.
///
/// - `questions` — the typed questions, in the wire shape. When present they
///   are FROZEN here and win over any `questions` key in the node's input, so
///   a payload a trigger handed the run cannot rewrite what the plan asks.
///   When absent, the node's input must carry them at dispatch.
/// - `into` — the state key the decision lands under; default the node's own
///   id. Edges then branch on it in the frozen grammar, e.g.
///   `triage.answers.route.choice == "escalate"`.
///
/// Refused at start, never at dispatch: an unaskable question (`DEC-E006`) or
/// a reserved key is a plan defect.
fn parse_decide(
    node: &str,
    decl: Option<&serde_json::Value>,
) -> std::result::Result<serde_json::Value, RunError> {
    let bad = |why: String| RunError::InvalidPlan { why: format!("decision node '{node}': {why}") };
    let decl = match decl {
        None | Some(serde_json::Value::Null) => serde_json::Map::new(),
        Some(serde_json::Value::Object(o)) => o.clone(),
        Some(_) => return Err(bad("`decide` must be an object".into())),
    };
    if let Some(k) = decl.keys().find(|k| !matches!(k.as_str(), "questions" | "into")) {
        return Err(bad(format!("unknown `decide` key {k:?} (accepted: questions, into)")));
    }
    let into = match decl.get("into") {
        None => node.to_string(),
        Some(serde_json::Value::String(s)) if !s.is_empty() && !s.starts_with('$') => s.clone(),
        Some(other) => {
            return Err(bad(format!(
                "`into` must be a non-empty state key not starting with '$', got {other}"
            )))
        }
    };
    let mut out = json!({ "into": into });
    if let Some(q) = decl.get("questions") {
        let parsed = areev_core::decide::questions_from_wire(q).map_err(|e| bad(e.to_string()))?;
        if parsed.is_empty() {
            return Err(bad("`questions` is empty".into()));
        }
        out["questions"] = areev_core::decide::questions_to_wire(&parsed);
    }
    Ok(out)
}

/// A Client ask a SECOND person must answer — the default, and what every
/// Client ask meant before #294.
pub const ASK_KIND_APPROVAL: &str = "approval";
/// A Client ask the run's own initiator may answer, when the host has opted
/// in. For a REVERSIBLE write a firm may decide the person who requested it
/// can confirm it; the alternative today is a prepare run, a separate commit
/// run under the confirmer, and a product ledger joining the two.
pub const ASK_KIND_CONFIRMATION: &str = "confirmation";

/// Newest Definition with this tool_name, or None. Definitions are few; a
/// bounded scan of the namespace's Tool grains is the v1 catalogue (a
/// dedicated definition index is a later optimization, never a semantics
/// change). Deterministic tiebreak: newest `created_at`, then highest hash
/// — the provisional-head rule's shape, so every host resolves identically.
fn find_definition_by_name(
    m: &mut Areev,
    ns: &str,
    name: &str,
) -> Option<(Hash, areev_core::format::deserialize::DeserializedGrain)> {
    // Journal intents/results are Tool grains IN THE SAME NAMESPACE (two
    // per effect), so a small window would let an active file crowd its own
    // definitions out of the catalogue — and with an LLM configured the
    // miss silently becomes an abstract node. Scan wide until a dedicated
    // definition index exists.
    const CATALOG_SCAN: usize = 200_000;
    let hits = m
        .recent(ns, Some(areev_core::types::GrainType::Tool), CATALOG_SCAN)
        .ok()?;
    let mut best: Option<(i64, String)> = None;
    let mut candidates: BTreeMap<String, areev_core::format::deserialize::DeserializedGrain> =
        BTreeMap::new();
    for g in hits {
        if g.get_str("kind") == Some("definition") && g.get_str("tool_name") == Some(name) {
            let at = g.get_i64("created_at").unwrap_or(0);
            let key = (at, g.hash.to_hex());
            if best.as_ref().is_none_or(|b| key > *b) {
                best = Some(key);
            }
            candidates.insert(g.hash.to_hex(), g);
        }
    }
    let (_, hex) = best?;
    let g = candidates.remove(&hex)?;
    let h = Hash::from_hex(&hex).ok()?;
    Some((h, g))
}

/// Is this unresolved node a MISROUTED run rather than an abstract one
/// (#230)? True when the plan grain lives in another namespace and a Tool
/// Definition named `node` is there — the run was started somewhere the
/// plan's own tools cannot be seen.
///
/// Deliberately narrow: it asks the plan's namespace and no other. A sweep
/// over every namespace in the memory would be both unbounded (journal Tool
/// grains crowd the catalogue) and less certain — the plan grain is the one
/// place that records where its nodes were authored to resolve. A run whose
/// namespace IS the plan's, or whose plan names a namespace with no such
/// Definition, falls through to the existing verdicts unchanged: abstract
/// with a model configured, `RUN-E006` without.
fn misrouted(m: &mut Areev, ns: &str, plan_ns: Option<&str>, node: &str) -> bool {
    match plan_ns {
        Some(p) if p != ns => find_definition_by_name(m, p, node).is_some(),
        _ => false,
    }
}

/// The nodes of `plan_hash` that would resolve as **abstract** — no binding
/// and no Definition head whose `tool_name` matches — plus whether the plan
/// itself is present at all.
///
/// A pre-flight read: it takes no lease, writes nothing, and is exactly the
/// half of [`RunManifest::resolve`] that decides `RUN-E006`. It exists so a
/// declaration that names a plan can say at AUTHORING time what the plan will
/// need at fire time (#73): `areev trigger add --workflow <WF>` used to accept
/// a plan whose nodes did not resolve, report `waiting`, and fail at the first
/// firing — on the operator's mailbox rather than at their keyboard.
///
/// Abstract is not an error here, which is why this returns node ids rather
/// than a `Result`: an abstract node is legitimate with a tool-calling model
/// configured. The caller decides whether that is a warning or nothing.
pub fn abstract_nodes(
    m: &mut Areev,
    ns: &str,
    plan_hash: &Hash,
) -> std::result::Result<Vec<String>, String> {
    let grain = m.get(plan_hash).map_err(|e| format!("{e}"))?;
    let wf = grain.to_workflow().map_err(|e| format!("{e}"))?;
    let plan = PlanGraph::build(&wf).map_err(|e| e.to_string())?;
    // A declared memory read (#255) is answered by the runtime, never a model.
    let reads = grain.fields.get(crate::memread::READS_FIELD).and_then(|r| r.as_object());
    let mut out = Vec::new();
    for (i, node) in plan.nodes.iter().enumerate() {
        if reads.is_some_and(|r| r.contains_key(node)) {
            continue;
        }
        if plan.bindings[i].is_none() && find_definition_by_name(m, ns, node).is_none() {
            out.push(node.clone());
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A manifest with nothing pinned and no limits set — the shape every
    /// pre-1.9 run persisted.
    fn bare() -> RunManifest {
        RunManifest {
            run_id: "r1".into(),
            plan_hash: "deadbeef".into(),
            principal: "user:t".into(),
            pinned: vec![],
            budgets: BudgetsSpec::default(),
            ask_ttl_sec: None,
            redaction: "none".into(),
            input: json!({}),
            llm_max_tokens: None,
            max_effects_per_attempt: None,
            llm_tool_result_chars: None,
            llm_context_tokens: None,
            reducers: BTreeMap::new(),
            fork_of: None,
            initiator: None,
            llm: None,
            engine: None,
            input_ref: None,
            harness_ns: None,
            allow_confirmation_asks: false,
            decider: None,
        }
    }

    fn host_pin(node: &str, tool: &str) -> PinnedTool {
        PinnedTool {
            node: node.into(),
            tool_hash: format!("hash-{tool}"),
            tool_name: tool.into(),
            executor: "host".into(),
            executor_uri: None,
            runtime: None,
            runtime_limits: None,
            capabilities: None,
            read: None,
            ask_kind: None,
            decide: None,
        }
    }

    /// One Definition bound to two nodes is offered to an abstract node ONCE (#270):
    /// a repeated name in a tools list is a 400 on the whole model request at every
    /// provider. Each bound node still executes its own binding.
    #[test]
    fn a_definition_bound_to_two_nodes_is_offered_once() {
        let mut m = bare();
        m.pinned = vec![
            host_pin("parse", "parse_attachments"),
            PinnedTool {
                node: "extract".into(),
                tool_hash: String::new(),
                tool_name: "extract".into(),
                executor: "abstract".into(),
                executor_uri: None,
                runtime: None,
                runtime_limits: None,
                capabilities: None,
                read: None,
                ask_kind: None,
                decide: None,
            },
            host_pin("reply_done", "reply_email"),
            host_pin("reply_rejected", "reply_email"),
        ];
        let executors = m.executors();
        assert_eq!(executors.len(), 4);
        match &executors[1] {
            NodeExecutor::Abstract { tools } => {
                let names: Vec<&str> = tools.iter().map(|t| t.tool_name.as_str()).collect();
                assert_eq!(names, vec!["parse_attachments", "reply_email"]);
                assert_eq!(tools[1].tool_hash, "hash-reply_email");
            }
            other => panic!("expected the abstract node's offer, got {other:?}"),
        }
        for (i, node) in [(2usize, "reply_done"), (3, "reply_rejected")] {
            match &executors[i] {
                NodeExecutor::Host { tool_name, tool_hash } => {
                    assert_eq!(tool_name, "reply_email", "{node} runs its own binding");
                    assert_eq!(tool_hash, "hash-reply_email");
                }
                other => panic!("{node}: expected a host executor, got {other:?}"),
            }
        }
    }

    /// A declared memory read (#255) is the runtime's: never in an abstract
    /// node's offer, and never a Host executor — not even for a pin whose
    /// declaration went missing, which would otherwise route the read to
    /// `--tool-cmd` and let a tool answer it.
    #[test]
    fn a_memory_read_is_never_offered_and_never_host() {
        let spec = json!({"op": "entity_at", "ns": "ops", "into": "cover", "subject": "s",
                          "relation": "p", "at": 1, "axis": "world"});
        let read_pin = |read: Option<serde_json::Value>| PinnedTool {
            node: "cover".into(),
            tool_hash: String::new(),
            tool_name: "mg:entity_at".into(),
            executor: "memory".into(),
            executor_uri: None,
            runtime: None,
            runtime_limits: None,
            capabilities: None,
            read,
            ask_kind: None,
            decide: None,
        };
        let mut m = bare();
        m.pinned = vec![
            host_pin("parse", "parse_attachments"),
            read_pin(Some(spec.clone())),
            PinnedTool { executor: "abstract".into(), ..host_pin("decide", "decide") },
        ];
        let executors = m.executors();
        assert_eq!(executors[1], NodeExecutor::MemoryRead { op: "entity_at".into(), spec });
        let NodeExecutor::Abstract { tools } = &executors[2] else { panic!("{executors:?}") };
        let offered: Vec<&str> = tools.iter().map(|t| t.tool_name.as_str()).collect();
        assert_eq!(offered, vec!["parse_attachments"], "a read is never offered to a model");

        m.pinned = vec![read_pin(None)];
        assert!(
            matches!(m.executors()[0], NodeExecutor::MemoryRead { .. }),
            "a memory pin without its declaration still never becomes Host"
        );
        let json = serde_json::to_string(&m).unwrap();
        assert!(!json.contains("\"read\""), "an absent read does not reach the wire: {json}");
    }

    /// The whole reason `max_effects_per_attempt` is an `Option` carrying
    /// `skip_serializing_if`: a manifest written before the knob existed must
    /// serialize to the SAME bytes it did then. The manifest is a stored
    /// grain, so a widened wire shape would re-address every one of them.
    #[test]
    fn an_unset_effect_cap_serializes_as_it_did_before_the_field_existed() {
        let json = serde_json::to_string(&bare()).unwrap();
        assert_eq!(
            json,
            r#"{"run_id":"r1","plan_hash":"deadbeef","principal":"user:t","pinned":[],"budgets":{"max_supersteps":null,"max_tokens":null,"max_usd_micros":null,"max_wall_ms":null,"max_storage_bytes":null},"ask_ttl_sec":null,"redaction":"none","input":{},"llm_max_tokens":null}"#
        );
    }

    /// The other half: a manifest persisted before the field existed still
    /// loads, and bounds its loops at the documented default.
    #[test]
    fn a_pre_existing_manifest_reads_back_at_the_default_cap() {
        let stored = r#"{"run_id":"r1","plan_hash":"deadbeef","principal":"user:t",
            "pinned":[],"budgets":{"max_supersteps":null,"max_tokens":null,
            "max_usd_micros":null,"max_wall_ms":null,"max_storage_bytes":null},
            "ask_ttl_sec":null,"redaction":"none","input":{},"llm_max_tokens":null}"#;
        let m: RunManifest = serde_json::from_str(stored).unwrap();
        assert_eq!(m.max_effects_per_attempt, None);
        assert_eq!(m.max_effects_per_attempt(), 16);
        assert_eq!(
            m.llm_tool_result_chars, None,
            "unbounded tool results: what every pre-1.9 run did"
        );
        // Named here as well as in run-core, because changing it changes the
        // behaviour of every existing plan that relies on the bound.
        assert_eq!(areev_run_core::DEFAULT_MAX_EFFECTS_PER_ATTEMPT, 16);
    }

    #[test]
    fn a_pinned_cap_wins_over_the_default() {
        let m = RunManifest { max_effects_per_attempt: Some(40), ..bare() };
        assert_eq!(m.max_effects_per_attempt(), 40);
        let json = serde_json::to_string(&m).unwrap();
        assert!(json.contains(r#""max_effects_per_attempt":40"#), "{json}");
    }
}
