//! `SchedulerState` — everything a checkpoint carries (§5.4). Rebuilt-from
//! on resume with nothing inferred from wall-clock or environment; every
//! collection is a `Vec` or `BTreeMap` so iteration order is content order,
//! never hash order (§4's determinism rule).

use crate::types::{Ask, DecisionRecord, JournalKey, RunOutcome};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

/// Per-node lifecycle at the node's current generation (§6.2's machine,
/// operationally realized with per-node re-entry generations).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum NodeState {
    /// In-edges unresolved at the current generation.
    Waiting,
    /// Dispatchable: no in-edge Pending, ≥1 Fired.
    Ready,
    /// An effect is outstanding (Host dispatch or retry in flight).
    Dispatched,
    /// Parked on a Client ask (§6.6).
    AwaitingClient,
    DoneOk,
    DoneFailed,
    /// All in-edges Dead at the current generation.
    Dead,
}

/// Resolution of one in-edge at the destination's current generation.
/// Absent from the map = Pending.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum EdgeRes {
    Fired,
    Dead,
}

/// Budget accumulators — pure functions of the journal, carried here as the
/// running total and re-derived + asserted on replay (§6.7). USD in integer
/// micro-dollars: budgets never do float arithmetic.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub struct Spent {
    pub supersteps: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub usd_micros: u64,
    /// Active wall only — Σ(superstep close − open). Parked/crashed gaps
    /// accumulate in `elapsed_ms`, reported but never charged (§6.7).
    pub wall_ms: u64,
    pub storage_bytes: u64,
    pub journal_grains: u64,
}

/// The run's phase within the BSP cycle.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum Phase {
    /// Between supersteps.
    Idle,
    /// A superstep is open: effects outstanding, results buffering, the
    /// decision record accumulating.
    Open {
        /// Keys still awaiting resolution (Host effects and Client asks).
        outstanding: BTreeSet<JournalKey>,
        /// Buffered results by node index — merged in canonical order at
        /// close, never in arrival order (§5.2).
        results: BTreeMap<usize, Value>,
        /// Buffered Send-task results by `task_path` (zero-padded ordinals,
        /// so lexicographic order IS spawn order) — merged after the static
        /// results, in path order.
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        send_results: BTreeMap<String, Value>,
        record: DecisionRecord,
    },
    /// Terminal.
    Finished(RunOutcome),
}

/// One Send-spawned task (§5.1: `task_path = parent_path/spawn_ordinal`).
/// Task-scoped input; attempts continue across re-spawns of the same path
/// (a later generation re-activating a path keeps the journal keys unique).
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct SendTask {
    pub node_idx: usize,
    pub input: Value,
    pub attempt: u32,
    pub state: SendTaskState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum SendTaskState {
    Queued,
    Running,
    Done,
}

/// One parked Client ask.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct PendingAsk {
    pub key: JournalKey,
    pub node_idx: usize,
    pub ask: Ask,
}

/// A tool call the model made, awaiting its result inside an abstract
/// node's LLM loop.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct PendingToolCall {
    /// The MODEL's call id (what the transcript's tool_result addresses).
    pub model_call_id: String,
    pub tool_name: String,
    pub arguments: Value,
}

/// What an abstract flow needs next — set by effect resolution, consumed by
/// the progress loop (resolution has no command sink; emission is the
/// loop's job, in canonical order).
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum FlowNeed {
    /// Dispatch the next LLM turn over the current transcript.
    NextTurn,
    /// Dispatch these model-issued tool calls (already appended to the
    /// transcript as the assistant entry).
    Tools(Vec<PendingToolCall>),
}

/// In-flight LLM-loop state for one abstract node at its current attempt
/// (§6.2's "a workflow with abstract nodes is an agent — same runtime").
/// Every entry is built from journaled outcomes, so the transcript is
/// replay-stable by construction.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct AbstractFlow {
    /// Journal-shaped messages:
    /// `{"role":"user","content":…}` ·
    /// `{"role":"assistant","text":…,"tool_calls":[{id,name,arguments}]}` ·
    /// `{"role":"tool","tool_call_id":…,"content":…,"is_error":…}`.
    pub messages: Vec<Value>,
    /// The next effect ordinal within this attempt (LLM turns and tool
    /// calls share the sequence — §5.1).
    pub next_effect_seq: u32,
    /// The current tool round: effect_seq → the model call it serves.
    pub pending_tools: BTreeMap<u32, PendingToolCall>,
    /// Settled results of the current round, by effect_seq (canonical
    /// transcript order regardless of completion order).
    pub round_results: BTreeMap<u32, Value>,
    /// What the loop should do next (None while waiting on effects).
    pub need: Option<FlowNeed>,
    /// Unknown-tool corrections issued (§6.11: ONE re-prompt, then the
    /// node fails `ExecutorError`).
    pub unknown_strikes: u32,
}

/// The complete scheduler state. Serialized (as JSON) into each checkpoint's
/// `context_data`; byte-stability across resumes is what the
/// replay-equivalence gate compares.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct SchedulerState {
    pub run_id: String,
    pub superstep: u64,
    pub phase: Phase,
    /// Per node: re-entry generation (0 = first activation).
    pub node_gen: Vec<u32>,
    pub node_state: Vec<NodeState>,
    /// Per node: in-edge resolutions AT ITS CURRENT GENERATION, keyed by
    /// edge index. Absent = Pending.
    pub in_res: Vec<BTreeMap<usize, EdgeRes>>,
    /// Per edge: total Fired count across the run (`max_cycles` counts
    /// these — §6.3: "fires in at most n generations per run").
    pub edge_fired: Vec<u32>,
    /// Per node: attempt counter — MONOTONIC across re-entry generations
    /// (never reset), because the journal key is
    /// (run, task_path, node, attempt, effect_seq, kind) and a reset would
    /// mint generation 1 the same keys as generation 0 — the driver would
    /// then answer iteration 2 from iteration 1's journaled results and the
    /// loop body would execute once, ever. The per-generation retry budget
    /// measures from `attempt_base` instead.
    pub attempt: Vec<u32>,
    /// Per node: the attempt count when the CURRENT generation began —
    /// `attempt - attempt_base` is this generation's attempt number.
    pub attempt_base: Vec<u32>,
    /// The accumulated run state the reducers fold into.
    pub context: Value,
    pub spent: Spent,
    /// Un-charged calendar time (parks, crashes) — reported, never billed.
    pub elapsed_ms: u64,
    /// The latest journaled clock reading seen.
    pub clock_ms: u64,
    /// The active wall segment's start (None while parked/idle). Wall
    /// charges only active segments — open→park and resume→close — so a
    /// three-day approval pause costs nothing (§6.7).
    pub wall_open: Option<u64>,
    /// When the current park began (drives `elapsed_ms` on resume).
    pub paused_at: Option<u64>,
    /// Parked Client asks by `tool_call_id` (§6.6 — addressed by id, never
    /// by index).
    pub pending_asks: BTreeMap<String, PendingAsk>,
    /// In-flight abstract-node LLM loops, by node index.
    pub abstract_flows: BTreeMap<usize, AbstractFlow>,
    /// Send-spawned tasks by `task_path`. A target node shows `Dispatched`
    /// while its batch is unresolved and completes when the batch drains.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub send_tasks: BTreeMap<String, SendTask>,
    /// Next spawn ordinal per parent path — monotonic for the whole run, so
    /// a re-entered spawner mints fresh task paths and journal keys never
    /// collide across generations.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub spawn_counter: BTreeMap<String, u32>,
    /// Ask ids already announced in an envelope — a parked run polled again
    /// must not re-emit its envelope every step call.
    pub announced_asks: BTreeSet<String>,
    /// A cancel marker observed (drain in progress).
    pub cancel: Option<(String, String)>,
    /// A budget axis exhausted mid-superstep (per-dispatch reservation,
    /// §6.7): no new work is emitted; the open superstep's effects drain,
    /// then the run finishes `BudgetExhausted` — checkpointed, resumable
    /// after a manifest supersession raises the ceiling.
    pub exhausted: Option<crate::error::BudgetAxis>,
    /// Set when fail-fast drain began: the lowest-index permanently-failed
    /// node and its detail (deterministic under permutation — chosen at
    /// superstep close, not at arrival).
    pub failed: Option<(usize, String)>,
}

impl SchedulerState {
    /// Fresh state for a validated plan.
    pub fn new(run_id: &str, plan: &crate::plan::PlanGraph) -> Self {
        let n = plan.nodes.len();
        SchedulerState {
            run_id: run_id.to_string(),
            superstep: 0,
            phase: Phase::Idle,
            node_gen: vec![0; n],
            node_state: vec![NodeState::Waiting; n],
            in_res: vec![BTreeMap::new(); n],
            edge_fired: vec![0; plan.edges.len()],
            attempt: vec![0; n],
            attempt_base: vec![0; n],
            context: Value::Object(serde_json::Map::new()),
            spent: Spent::default(),
            elapsed_ms: 0,
            clock_ms: 0,
            wall_open: None,
            paused_at: None,
            pending_asks: BTreeMap::new(),
            abstract_flows: BTreeMap::new(),
            send_tasks: BTreeMap::new(),
            spawn_counter: BTreeMap::new(),
            announced_asks: BTreeSet::new(),
            cancel: None,
            exhausted: None,
            failed: None,
        }
    }

    pub fn is_terminal(&self) -> bool {
        matches!(self.phase, Phase::Finished(_))
    }

    pub fn outcome(&self) -> Option<&RunOutcome> {
        match &self.phase {
            Phase::Finished(o) => Some(o),
            _ => None,
        }
    }
}
