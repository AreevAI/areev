//! The sans-IO step function (governed-agents §4): apply the driver's
//! events, progress the BSP machine as far as pure reasoning allows, return
//! the commands the driver must perform. Same plan + same env + same events
//! in the same order ⇒ byte-identical state and commands — that is the
//! property the DST harness and the three CI gates exercise.
//!
//! Operational semantics (v1, refining §6.2's generation model into a
//! per-node re-entry form with the same observable contract):
//!
//! - **Superstep**: open (record the journaled clock; check every budget
//!   axis) → dispatch every Ready node in canonical order → collect ALL
//!   results (retries included) → merge results through the reducers in
//!   canonical order → evaluate completed nodes' out-edges in canonical
//!   order, journaling every outcome → deliver firings → checkpoint → close
//!   (record the clock again; wall charges close − open, so a three-day
//!   HITL park costs nothing).
//! - **Re-entry**: an edge firing into a node already resolved at its
//!   current generation increments the node's generation and resets it to
//!   Waiting: the firing edge is Fired at the new generation; every other
//!   in-edge is Pending if its source is still unresolved (it may yet fire)
//!   or Dead otherwise. This is how a bounded cycle iterates — the Rev-2
//!   edge machine, whose states were all terminal, could not.
//! - **Death**: a Waiting node whose in-edges all resolved with none Fired
//!   is Dead; its out-edges propagate Dead in canonical order — a join
//!   below an untaken branch (or a died-out cycle) resolves instead of
//!   pending forever.
//! - **Fail-fast** (v1's only failure policy): a node that exhausts its
//!   retries marks the run draining — no retries elsewhere are suppressed
//!   (each node's retry decision is independent, or determinism under
//!   permutation breaks), but no edges evaluate at close and no new
//!   superstep opens; the run finishes `Failed` naming the LOWEST-index
//!   failed node, chosen at close, not at arrival.
//! - **Cancel**: drain; retries stop; parked Client asks are abandoned (the
//!   journal keeps their grains); finish `Canceled`.
//! - **Budgets** (§6.7): every axis is checked at superstep open, before
//!   any dispatch. Per-dispatch reservation refines this in Wave 2, where
//!   LLM effects carry a mandatory `max_tokens` reserve to check — a v1
//!   Host tool has no reserve quantity, so open-time is the honest v1
//!   enforcement point, with overshoot bounded by one superstep's
//!   dispatches (stated, not hidden).

use crate::error::{BudgetAxis, RunError};
use crate::plan::PlanGraph;
use crate::state::{EdgeRes, FoldInFlight, NodeState, PendingAsk, Phase, SchedulerState};
use crate::types::{
    Ask, Budgets, Command, DecisionRecord, EdgeOutcome, EffectKind, EffectOutcome, EventIn,
    FailCause, JournalKey, NodeExecutor, RunOutcome,
};
use serde_json::Value;
use std::collections::BTreeMap;

/// Effects one node attempt may spend when the host pins no other number.
/// Named because three places must agree — both of the driver's `StepEnv`
/// sites and the manifest accessor that feeds them — and because changing it
/// changes the behaviour of every existing plan that relies on the bound.
pub const DEFAULT_MAX_EFFECTS_PER_ATTEMPT: u32 = 16;

/// Transcript entries a fold keeps at the END, beyond `messages[0]`.
///
/// Four is two complete rounds at one tool call per turn, which is the shortest
/// tail that still shows the model what it just did and what came back. It is
/// expressed in entries rather than "rounds" because a round has no fixed
/// length — a turn may issue three tool calls or none — and a count of entries
/// is something `fold_range` can check without parsing the transcript's shape.
/// The cut then moves forward off any `tool` entry, which is what actually
/// keeps rounds intact.
pub const KEEP_TAIL: usize = 4;

/// The instruction a summarizer turn carries.
///
/// This text RIDES THE JOURNAL: it is part of the fold intent's `input`, so a
/// stored run records the exact prompt its summary was produced from. Changing
/// it therefore changes what a journal contains — bump [`FOLD_PROMPT_V`] and
/// say so in the changelog rather than editing it quietly.
pub const FOLD_PROMPT: &str = "Summarize the working state of this task so far \
for someone who will continue it: what has been established, which tools were \
called and what they returned in substance, what remains to be done, and any \
errors or dead ends. Be concrete; keep identifiers, paths and values verbatim. \
Reply with the summary only.";

/// The version of [`FOLD_PROMPT`], journaled beside it so a reader of an old
/// run knows which wording produced its summaries.
pub const FOLD_PROMPT_V: u32 = 1;

/// The injected pure behavior. Everything here is REQUIRED to be pure —
/// enforced by the journaled decision record + replay assertion, not trust.
pub struct StepEnv<'a> {
    pub plan: &'a PlanGraph,
    /// Per node: the resolved executor (the driver's V7 freeze).
    pub executors: &'a [NodeExecutor],
    pub budgets: Budgets,
    /// Merge one result key into the accumulated state:
    /// `(key, prev, new) -> merged`. The driver's registry with LWW
    /// fallback; must be pure, deterministic, batching-invariant (§6.5).
    pub reduce: &'a dyn Fn(&str, Option<&Value>, &Value) -> Value,
    /// Evaluate an edge's condition against the state — the built-in
    /// (`cond::eval` over the parsed form) or a host evaluator over the raw
    /// string. Must be pure; only consulted for edges that carry a cond.
    pub eval_cond: &'a dyn Fn(&crate::plan::PlanEdge, &Value) -> bool,
    /// TTL for Client asks (seconds); None = asks never expire.
    pub ask_ttl_sec: Option<i64>,
    /// Validate a model-issued tool call's arguments against the pinned
    /// Definition's `input_schema` (§6.11 — the driver wires
    /// `validate_instance` over strict definitions; Ok(()) for non-strict).
    /// Must be pure: it runs identically on replay.
    pub validate_args: &'a dyn Fn(&str, &Value) -> std::result::Result<(), String>,
    /// The per-call token reservation for LLM dispatches (§6.7's
    /// per-dispatch enforcement: `spent + reserve` must fit before an LLM
    /// effect is emitted). The driver sets this to the request's mandatory
    /// `max_tokens`.
    pub llm_reserve_tokens: u64,
    /// Hard bound on effects per node attempt — an abstract node's LLM loop
    /// (turns + tool calls + re-prompts) that exceeds it fails the node
    /// with `ExecutorError` rather than looping forever. Frozen in the run
    /// manifest, so a resume bounds the loop exactly as the start did;
    /// [`DEFAULT_MAX_EFFECTS_PER_ATTEMPT`] when the manifest pins none.
    pub max_effects_per_attempt: u32,
    /// Bound on ONE tool result's size in the TRANSCRIPT (characters — there
    /// is no tokenizer here, and calling it tokens would be a lie). `None` =
    /// unbounded, which is what every run did before the knob existed. The
    /// journal is never bounded: see [`bound_tool_content`].
    pub llm_tool_result_chars: Option<usize>,
    /// Ceiling on the WHOLE transcript, in the provider's own prompt tokens
    /// (`AbstractFlow.last_prompt_tokens`). `None` = no ceiling, which is what
    /// every run did before the fold existed. Reaching it emits one journaled
    /// summarizer turn and splices its result over the folded range; failing
    /// to find a foldable range is `RUN-E024`.
    pub llm_context_tokens: Option<u64>,
}

/// One step's output.
pub struct StepOutcome {
    pub commands: Vec<Command>,
    pub state: SchedulerState,
}

/// Apply events, progress, emit commands. Driver contract: any call that
/// may open or close a superstep includes a fresh `ClockReading`.
pub fn step(env: &StepEnv<'_>, mut st: SchedulerState, events: &[EventIn]) -> StepOutcome {
    let mut out: Vec<Command> = Vec::new();
    for ev in events {
        apply_event(env, &mut st, ev, &mut out);
    }
    progress(env, &mut st, &mut out);
    StepOutcome { commands: out, state: st }
}

fn apply_event(
    env: &StepEnv<'_>,
    st: &mut SchedulerState,
    ev: &EventIn,
    out: &mut Vec<Command>,
) {
    match ev {
        EventIn::ClockReading { unix_ms } => {
            st.clock_ms = (*unix_ms).max(st.clock_ms);
        }
        EventIn::Start { input } => {
            if st.superstep == 0 && matches!(st.phase, Phase::Idle) {
                // The accumulated state is an object by construction; a
                // non-object input is carried under "input" rather than
                // refused (the driver validates and warns at its surface).
                st.context = match input {
                    Value::Object(_) => input.clone(),
                    other => serde_json::json!({ "input": other }),
                };
                // Bootstrap exception: the entry node (OMS §8.4: nodes[0])
                // activates unconditionally at generation 0, even if a
                // back-edge targets it.
                if st.node_state[0] == NodeState::Waiting {
                    st.node_state[0] = NodeState::Ready;
                }
            }
        }
        EventIn::CancelSeen { principal, reason } => {
            if st.cancel.is_none() && !st.is_terminal() {
                st.cancel = Some((principal.clone(), reason.clone()));
            }
        }
        EventIn::EffectResolved { key, outcome } => {
            resolve_effect(env, st, key, outcome);
        }
        EventIn::InputSeen { message } => {
            st.inputs_seen += 1;
            st.inbox.push(message.clone());
        }
        EventIn::AskForwarded { tool_call_id } => {
            let Some(pending) = st.pending_asks.get(tool_call_id).cloned() else {
                return;
            };
            drop_bubble(st, &pending.key);
            st.node_state[pending.node_idx] = NodeState::Dispatched;
            let executor = env.executors[pending.node_idx].clone();
            let input = st.context.clone();
            let (superstep, clock_ms) = (st.superstep, st.clock_ms);
            out.push(Command::WriteIntent {
                key: pending.key.clone(),
                executor: executor.clone(),
                input: input.clone(),
                superstep,
                clock_ms,
            });
            out.push(Command::Dispatch { key: pending.key, executor, input });
        }
        EventIn::ResponseSettled { tool_call_id, outcome } => {
            let Some(pending) = st.pending_asks.remove(tool_call_id) else {
                // Unknown/settled asks are validated, journaled, and
                // refused by the driver (§6.6) before ever reaching here.
                return;
            };
            // Deliberately NO wall/elapsed bookkeeping here: the reading in
            // effect at response-apply is the driver's resume time, which is
            // NOT journaled — accruing from it would make checkpoint state a
            // function of when the operator typed `resume`, and verify could
            // never reproduce it. Both accruals happen at superstep close,
            // from the journaled close reading (see `close_superstep`); the
            // park→close tail is never charged as wall (it is µs of
            // bookkeeping), and elapsed = park→close, both ends journaled.
            st.announced_asks.remove(tool_call_id);
            st.node_state[pending.node_idx] = NodeState::Dispatched;
            resolve_effect(env, st, &pending.key, outcome);
        }
    }
}

/// Book a resolved effect: buffer the result, or retry / mark failure per
/// the §6.3 table. Retries are decided PER NODE, independent of other
/// nodes' failures — otherwise whether a retry happens would depend on
/// which failure arrived first (permutation-variant).
fn resolve_effect(
    env: &StepEnv<'_>,
    st: &mut SchedulerState,
    key: &JournalKey,
    outcome: &EffectOutcome,
) {
    let Some(node_idx) = env.plan.nodes.iter().position(|n| *n == key.node) else {
        return;
    };
    {
        let Phase::Open { outstanding, .. } = &mut st.phase else {
            return; // Late/duplicate resolution of a closed superstep.
        };
        if !outstanding.remove(key) {
            return; // Duplicate delivery for this key: first-commit won (§6.6).
        }
    }
    // Accounting is per-effect, whatever the effect serves (§6.7: every
    // figure is a pure function of the journal).
    match outcome {
        EffectOutcome::Completed {
            journal_bytes,
            input_tokens,
            output_tokens,
            usd_micros,
            ..
        } => {
            st.spent.journal_grains += 2; // intent + result supersession
            st.spent.storage_bytes += journal_bytes;
            st.spent.input_tokens += input_tokens;
            st.spent.output_tokens += output_tokens;
            st.spent.usd_micros += usd_micros;
        }
        EffectOutcome::Failed { journal_bytes, .. } => {
            st.spent.journal_grains += 2;
            st.spent.storage_bytes += journal_bytes;
        }
    }

    // Abstract-flow effects continue the LOOP rather than resolving what
    // owns it — a node, or one Send task against an abstract target.
    let flow = flow_key(node_idx, &key.task_path);
    if st.abstract_flows.contains_key(&flow) {
        match key.kind {
            EffectKind::Llm => {
                handle_llm_outcome(env, st, node_idx, &key.task_path, outcome);
                return;
            }
            EffectKind::Tool
                if st.abstract_flows[&flow].pending_tools.contains_key(&key.effect_seq) =>
            {
                handle_flow_tool_outcome(
                    env,
                    st,
                    node_idx,
                    &key.task_path,
                    key.effect_seq,
                    outcome,
                );
                return;
            }
            _ => {}
        }
    }

    // Send-task effects resolve the TASK, never the target node directly —
    // the node completes when its batch drains.
    if !key.task_path.is_empty() {
        handle_send_task_outcome(env, st, key, outcome);
        return;
    }
    // A resolved node's straggler (a flow-tool result landing after
    // `fail_abstract` tore its flow down mid-round): accounted above, but it
    // must not flip a DoneFailed node back to DoneOk with a raw tool result.
    if matches!(
        st.node_state[node_idx],
        NodeState::DoneOk | NodeState::DoneFailed | NodeState::Dead
    ) {
        return;
    }

    if let EffectOutcome::Completed { result, .. } = outcome {
        if matches!(env.executors[node_idx], NodeExecutor::Subgraph { .. }) {
            if let Some(asks) = result
                .get(crate::types::PARKED_ASKS)
                .and_then(|v| serde_json::from_value::<Vec<Ask>>(v.clone()).ok())
                .filter(|a| !a.is_empty())
            {
                park_bubbled(st, node_idx, key, asks);
                return;
            }
        }
    }

    let attempts_used = st.attempt[node_idx] - st.attempt_base[node_idx];
    let retry_budget = env.plan.retries[node_idx];
    let canceling = st.cancel.is_some();
    match outcome {
        EffectOutcome::Completed { result, .. } => {
            st.node_state[node_idx] = NodeState::DoneOk;
            if let Phase::Open { results, .. } = &mut st.phase {
                results.insert(node_idx, result.clone());
            }
        }
        EffectOutcome::Failed { cause, detail, .. } => {
            let can_retry =
                cause.retryable(key.kind) && attempts_used <= retry_budget && !canceling;
            if can_retry {
                // Flip to Ready inside the open superstep; `progress`
                // dispatches the next attempt.
                st.node_state[node_idx] = NodeState::Ready;
            } else {
                st.node_state[node_idx] = NodeState::DoneFailed;
                let named = st.failed.as_ref().map(|(i, _)| *i).unwrap_or(usize::MAX);
                if node_idx < named {
                    st.failed = Some((node_idx, format!("{cause:?}: {detail}")));
                }
            }
        }
    }
}

/// The `abstract_flows` key. A node's own loop keys on its index alone, so
/// the serialized shape is exactly what it was before Send could target an
/// abstract node; a task's loop qualifies that with its path.
pub fn flow_key(node_idx: usize, task_path: &str) -> String {
    if task_path.is_empty() {
        node_idx.to_string()
    } else {
        format!("{node_idx}@{task_path}")
    }
}

fn flow_attempt(st: &SchedulerState, i: usize, path: &str) -> u32 {
    match st.send_tasks.get(path) {
        Some(task) => task.attempt,
        None => st.attempt[i],
    }
}

/// Open a fresh LLM loop for a node or one Send task against an abstract
/// node, and emit its first turn.
fn start_flow(env: &StepEnv<'_>, st: &mut SchedulerState, i: usize, path: &str, input: Value, out: &mut Vec<Command>) {
    st.abstract_flows.insert(
        flow_key(i, path),
        crate::state::AbstractFlow {
            messages: vec![serde_json::json!({
                "role": "user",
                "content": {
                    "instruction": env.plan.nodes[i],
                    "state": input,
                }
            })],
            next_effect_seq: 0,
            pending_tools: BTreeMap::new(),
            round_results: BTreeMap::new(),
            need: None,
            unknown_strikes: 0,
            last_prompt_tokens: 0,
            folding: None,
            folds: 0,
        },
    );
    dispatch_llm_turn(env, st, i, path, out);
}

fn flow_owners(st: &SchedulerState) -> Vec<(usize, String)> {
    let mut owners: Vec<(usize, String)> = st
        .abstract_flows
        .iter()
        .filter(|(_, f)| f.need.is_some())
        .filter_map(|(k, _)| match k.split_once('@') {
            Some((i, path)) => Some((i.parse().ok()?, path.to_string())),
            None => Some((k.parse().ok()?, String::new())),
        })
        .collect();
    owners.sort();
    owners
}

fn apply_inbox(st: &mut SchedulerState) {
    let Value::Object(o) = &mut st.context else {
        return;
    };
    if st.inbox.is_empty() {
        o.remove(crate::types::INBOX);
    } else {
        o.insert(crate::types::INBOX.into(), Value::Array(std::mem::take(&mut st.inbox)));
    }
}

fn drop_bubble(st: &mut SchedulerState, key: &JournalKey) {
    let ids: Vec<String> = st
        .pending_asks
        .iter()
        .filter(|(_, p)| p.key == *key)
        .map(|(id, _)| id.clone())
        .collect();
    for id in ids {
        st.pending_asks.remove(&id);
        st.announced_asks.remove(&id);
    }
}

fn park_bubbled(st: &mut SchedulerState, i: usize, resolved: &JournalKey, asks: Vec<Ask>) {
    if !matches!(st.phase, Phase::Open { .. }) {
        return;
    }
    let stale: Vec<JournalKey> = st
        .pending_asks
        .values()
        .filter(|p| p.node_idx == i)
        .map(|p| p.key.clone())
        .collect();
    for key in stale {
        drop_bubble(st, &key);
    }
    // The next ROUND of the same attempt, never the next attempt: the child
    // run id is derived from the attempt, so bumping it here would forward
    // the answered ask into a brand-new child. Holding `attempt` still also
    // means a park costs no retry budget without touching `attempt_base`.
    let key = JournalKey { effect_seq: resolved.effect_seq + 1, ..resolved.clone() };
    if let Phase::Open { outstanding, .. } = &mut st.phase {
        outstanding.insert(key.clone());
    }
    st.node_state[i] = NodeState::AwaitingClient;
    for ask in asks {
        st.pending_asks.insert(
            ask.tool_call_id.clone(),
            PendingAsk { key: key.clone(), node_idx: i, ask },
        );
    }
}

/// A Send task's effect resolved: buffer the result (or retry / fail per
/// the same §6.3 table nodes use), and complete the target node when its
/// batch has drained — the join below a fan-out.
fn handle_send_task_outcome(
    env: &StepEnv<'_>,
    st: &mut SchedulerState,
    key: &JournalKey,
    outcome: &EffectOutcome,
) {
    let Some(task) = st.send_tasks.get_mut(&key.task_path) else { return };
    // A settled task's straggler — an abstract target's flow tool landing
    // after `fail_abstract` tore the loop down. Accounted above, but it must
    // not overwrite the task's contribution with a raw tool result the model
    // never saw. The node-level guard below does the same job for nodes.
    if task.state == crate::state::SendTaskState::Done {
        return;
    }
    let target = task.node_idx;
    match outcome {
        EffectOutcome::Completed { result, .. } => {
            settle_send_task(st, &key.task_path, result.clone());
        }
        EffectOutcome::Failed { cause, detail, .. } => {
            let can_retry = cause.retryable(key.kind)
                && task.attempt <= env.plan.retries[target]
                && st.cancel.is_none();
            if can_retry {
                task.state = crate::state::SendTaskState::Queued;
                return;
            }
            task.state = crate::state::SendTaskState::Done;
            st.node_state[target] = NodeState::DoneFailed;
            let named = st.failed.as_ref().map(|(i, _)| *i).unwrap_or(usize::MAX);
            if target < named {
                st.failed = Some((
                    target,
                    format!("{cause:?}: {detail} (task {})", key.task_path),
                ));
            }
        }
    }
}

/// One task settled with a result: buffer it, then complete the target node
/// if its whole batch has drained — the join below a fan-out.
fn settle_send_task(st: &mut SchedulerState, path: &str, result: Value) {
    let Some(task) = st.send_tasks.get_mut(path) else { return };
    let target = task.node_idx;
    task.state = crate::state::SendTaskState::Done;
    if let Phase::Open { send_results, .. } = &mut st.phase {
        send_results.insert(path.to_string(), result);
    }
    let all_done = st
        .send_tasks
        .values()
        .filter(|t| t.node_idx == target)
        .all(|t| t.state == crate::state::SendTaskState::Done);
    if all_done && st.node_state[target] == NodeState::Dispatched {
        st.node_state[target] = NodeState::DoneOk;
        if let Phase::Open { results, .. } = &mut st.phase {
            results.entry(target).or_insert(Value::Null);
        }
    }
}

/// A model turn resolved: end the loop, request tools, or re-prompt.
fn handle_llm_outcome(
    env: &StepEnv<'_>,
    st: &mut SchedulerState,
    i: usize,
    path: &str,
    outcome: &EffectOutcome,
) {
    let NodeExecutor::Abstract { tools } = &env.executors[i] else { return };
    let flow_id = flow_key(i, path);
    // A summarizer turn resolves differently from a model turn: its result is
    // not the node's answer, and any tool calls in it are ignored (it was
    // offered none). Taken here so a retryable failure also clears the marker
    // and `dispatch_llm_turn` re-derives the decision from the same transcript.
    let folding = st.abstract_flows.get_mut(&flow_id).and_then(|f| f.folding.take());
    if let Some(f) = folding {
        resolve_fold(env, st, i, path, f, outcome);
        return;
    }
    let offered: Vec<&str> = tools.iter().map(|t| t.tool_name.as_str()).collect();
    match outcome {
        EffectOutcome::Failed { cause, detail, .. } => match cause {
            // Transient model failures retry the TURN (same transcript,
            // next effect_seq) — idempotent and journal-clean; schema
            // failures re-prompt with the error appended (§6.11). Both are
            // bounded by max_effects_per_attempt.
            FailCause::Timeout | FailCause::ExecutorError => {
                if let Some(flow) = st.abstract_flows.get_mut(&flow_id) {
                    flow.need = Some(crate::state::FlowNeed::NextTurn);
                }
            }
            FailCause::SchemaValidationFailed => {
                if let Some(flow) = st.abstract_flows.get_mut(&flow_id) {
                    flow.messages.push(serde_json::json!({
                        "role": "user",
                        "content": format!(
                            "Your previous output failed validation: {detail}. \
                             Correct it and answer again."
                        ),
                    }));
                    flow.need = Some(crate::state::FlowNeed::NextTurn);
                }
            }
            FailCause::UserAborted | FailCause::Unknown => {
                fail_abstract(env, st, i, path, detail);
            }
        },
        EffectOutcome::Completed { result, input_tokens, .. } => {
            // The transcript's size at the MODEL's own tokenizer, for the
            // context ceiling to measure on the next dispatch. The provider
            // reported it for the transcript exactly as sent, which is why the
            // runtime never estimates.
            if let Some(flow) = st.abstract_flows.get_mut(&flow_id) {
                flow.last_prompt_tokens = *input_tokens;
            }
            let text = result.get("text").and_then(|t| t.as_str()).map(str::to_string);
            let calls: Vec<crate::state::PendingToolCall> = result
                .get("tool_calls")
                .and_then(|c| c.as_array())
                .map(|arr| {
                    arr.iter()
                        .map(|c| crate::state::PendingToolCall {
                            model_call_id: c
                                .get("id")
                                .and_then(|v| v.as_str())
                                .unwrap_or_default()
                                .to_string(),
                            tool_name: c
                                .get("name")
                                .and_then(|v| v.as_str())
                                .unwrap_or_default()
                                .to_string(),
                            arguments: c.get("arguments").cloned().unwrap_or(Value::Null),
                        })
                        .collect()
                })
                .unwrap_or_default();

            // Unknown tool names: ONE corrective re-prompt, then the node
            // fails `ExecutorError` (§6.11) — the model must choose among
            // what was offered, and a model that cannot is not looped on.
            let unknown: Vec<String> = calls
                .iter()
                .map(|c| c.tool_name.clone())
                .filter(|n| !offered.contains(&n.as_str()))
                .collect();
            if !unknown.is_empty() {
                let strikes = st
                    .abstract_flows
                    .get(&flow_id)
                    .map(|f| f.unknown_strikes)
                    .unwrap_or(0);
                if strikes >= 1 {
                    fail_abstract(
                        env,
                        st,
                        i,
                        path,
                        &format!("model called unknown tool(s) {unknown:?} after a corrective re-prompt"),
                    );
                    return;
                }
                let Some(flow) = st.abstract_flows.get_mut(&flow_id) else { return };
                flow.unknown_strikes += 1;
                flow.messages.push(serde_json::json!({
                    "role": "user",
                    "content": format!(
                        "Unknown tool(s) {:?}; choose among: {:?}",
                        unknown, offered
                    ),
                }));
                flow.need = Some(crate::state::FlowNeed::NextTurn);
                return;
            }

            // Argument validation (§6.11): unparseable argument payloads and
            // strict-schema violations re-prompt with the error appended —
            // consuming `effect_seq`, bounded by `max_effects_per_attempt`.
            let invalid: Option<(String, String)> = calls.iter().find_map(|c| {
                if !c.arguments.is_object() {
                    return Some((
                        c.tool_name.clone(),
                        "arguments are not a JSON object".to_string(),
                    ));
                }
                (env.validate_args)(&c.tool_name, &c.arguments)
                    .err()
                    .map(|e| (c.tool_name.clone(), e))
            });
            let Some(flow) = st.abstract_flows.get_mut(&flow_id) else { return };
            if let Some((tool, why)) = invalid {
                flow.messages.push(serde_json::json!({
                    "role": "user",
                    "content": format!(
                        "Arguments for tool '{tool}' failed validation ({why}). \
                         Correct the call and try again."
                    ),
                }));
                flow.need = Some(crate::state::FlowNeed::NextTurn);
                return;
            }
            if calls.is_empty() {
                // The loop ends: the final text is the node's result — a
                // JSON object merges as one; anything else lands under the
                // node's own key.
                let final_text = text.unwrap_or_default();
                let value = serde_json::from_str::<Value>(&final_text)
                    .ok()
                    .filter(|v| v.is_object())
                    .unwrap_or_else(|| {
                        serde_json::json!({ env.plan.nodes[i].clone(): final_text })
                    });
                st.abstract_flows.remove(&flow_id);
                if path.is_empty() {
                    st.node_state[i] = NodeState::DoneOk;
                    if let Phase::Open { results, .. } = &mut st.phase {
                        results.insert(i, value);
                    }
                } else {
                    settle_send_task(st, path, value);
                }
                return;
            }
            // Tool round: the assistant entry is journal-derived, so the
            // transcript is replay-stable.
            flow.messages.push(serde_json::json!({
                "role": "assistant",
                "text": text,
                "tool_calls": calls.iter().map(|c| serde_json::json!({
                    "id": c.model_call_id,
                    "name": c.tool_name,
                    "arguments": c.arguments,
                })).collect::<Vec<_>>(),
            }));
            flow.round_results.clear();
            flow.need = Some(crate::state::FlowNeed::Tools(calls));
        }
    }
}

/// Bound ONE tool result for the TRANSCRIPT. The journal is untouched: the
/// driver wrote the full result grain before this event ever reached `step`,
/// so what is dropped here is dropped from scheduler state only and stays
/// addressable at `(run, task_path, node, attempt, effect_seq)` forever.
///
/// `cap` is in **characters**, and is named that because that is what it is.
/// This crate holds no tokenizer, and a "token" bound implemented as
/// `chars / 4` would be a guess wearing a precise name. It is also why this is
/// a separate bound from the context ceiling, which uses the provider's own
/// reported prompt tokens: one oversized entry is a different problem from a
/// transcript that grew, and no fold can shrink a single entry.
///
/// Under the cap the value passes through **byte-identically**, so a run with
/// no cap configured builds exactly the transcript it always did. Over it, the
/// model sees the head, the tail, the true length, and where the whole thing
/// lives. Pure and total: a function of the journaled outcome and the manifest,
/// so replay rebuilds the same entry.
pub fn bound_tool_content(
    raw: &Value,
    cap: Option<usize>,
    attempt: u32,
    effect_seq: u32,
) -> Value {
    let Some(cap) = cap else { return raw.clone() };
    // A string result is measured as its text; anything else as the JSON the
    // model would have been shown.
    let text = match raw {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    };
    let total = text.chars().count();
    if total <= cap {
        return raw.clone();
    }
    let half = cap / 2;
    serde_json::json!({
        "truncated": true,
        "chars": total,
        "head": text.chars().take(half).collect::<String>(),
        "tail": text.chars().skip(total - half).collect::<String>(),
        "journal": { "attempt": attempt, "effect_seq": effect_seq },
    })
}

/// A summarizer turn resolved: splice its result over the range it stood in
/// for, and hand the loop back to an ordinary next turn.
///
/// The splice is an edit to **scheduler state only**. Every entry it removes
/// remains a journaled intent + result grain — nothing is deleted, nothing is
/// rewritten, and the fold message says where the record is. That is the root
/// `CLAUDE.md`'s immutability invariant applied to the runtime.
fn resolve_fold(
    env: &StepEnv<'_>,
    st: &mut SchedulerState,
    i: usize,
    path: &str,
    f: FoldInFlight,
    outcome: &EffectOutcome,
) {
    let flow_id = flow_key(i, path);
    match outcome {
        EffectOutcome::Completed { result, .. } => {
            let summary =
                result.get("text").and_then(Value::as_str).unwrap_or_default().to_string();
            let attempt = flow_attempt(st, i, path);
            let Some(flow) = st.abstract_flows.get_mut(&flow_id) else { return };
            // The range was computed against this very transcript and nothing
            // has touched it since — but a scheduler must not panic on a range
            // it cannot apply. Dropping the fold and taking an ordinary turn
            // re-derives the decision on the next dispatch.
            if f.from >= f.to || f.to > flow.messages.len() {
                flow.need = Some(crate::state::FlowNeed::NextTurn);
                return;
            }
            let n = flow.folds + 1;
            let fold_msg = serde_json::json!({
                "role": "user",
                "content": format!(
                    "[Areev fold {n}: transcript entries {}..{} of attempt {attempt} were \
                     replaced by this summary, journaled at effect_seq {}. The full record \
                     — every turn and every tool result — is in this run's journal.]\n{summary}",
                    f.from, f.to, f.effect_seq,
                ),
            });
            flow.messages.splice(f.from..f.to, [fold_msg]);
            flow.folds = n;
            // The spliced transcript has not been measured yet. Zero means "ask
            // the provider again", not "it is small" — the next real turn's
            // reported prompt tokens decide whether another fold is due.
            flow.last_prompt_tokens = 0;
            flow.need = Some(crate::state::FlowNeed::NextTurn);
        }
        EffectOutcome::Failed { cause, detail, .. } => match cause {
            // Transient: retry the TURN, which re-derives the fold decision
            // from the unchanged transcript. Bounded by the effect cap like
            // every other retry in the loop, so it cannot spin.
            FailCause::Timeout | FailCause::ExecutorError => {
                if let Some(flow) = st.abstract_flows.get_mut(&flow_id) {
                    flow.need = Some(crate::state::FlowNeed::NextTurn);
                }
            }
            // Terminal, including a schema failure: there is no schema on a
            // summarizer turn, so a corrective re-prompt would be theatre.
            FailCause::Unknown
            | FailCause::UserAborted
            | FailCause::SchemaValidationFailed => {
                fail_abstract(env, st, i, path, detail);
            }
        },
    }
}

/// A model-issued tool call resolved. Tool failures inside an abstract loop
/// are MODEL-VISIBLE error results, never scheduler retries — the model
/// decides what to do about its own tool's failure.
fn handle_flow_tool_outcome(
    env: &StepEnv<'_>,
    st: &mut SchedulerState,
    i: usize,
    path: &str,
    effect_seq: u32,
    outcome: &EffectOutcome,
) {
    // Read before the flow is borrowed mutably — the attempt is what makes the
    // truncation's journal pointer resolvable.
    let attempt = flow_attempt(st, i, path);
    let cap = env.llm_tool_result_chars;
    let Some(flow) = st.abstract_flows.get_mut(&flow_key(i, path)) else { return };
    let Some(call) = flow.pending_tools.remove(&effect_seq) else { return };
    let entry = match outcome {
        EffectOutcome::Completed { result, .. } => serde_json::json!({
            "role": "tool",
            "tool_call_id": call.model_call_id,
            "content": bound_tool_content(result, cap, attempt, effect_seq),
            "is_error": false,
        }),
        // Failure details are short by construction, but they carry a tool's
        // stderr — so the same rule applies rather than one honest path and one
        // hopeful one.
        EffectOutcome::Failed { cause, detail, .. } => serde_json::json!({
            "role": "tool",
            "tool_call_id": call.model_call_id,
            "content": bound_tool_content(
                &Value::String(format!("{cause:?}: {detail}")),
                cap,
                attempt,
                effect_seq,
            ),
            "is_error": true,
        }),
    };
    flow.round_results.insert(effect_seq, entry);
    if flow.pending_tools.is_empty() {
        // The round is settled: append results in effect_seq order —
        // canonical transcript order regardless of completion order.
        let entries: Vec<Value> = flow.round_results.values().cloned().collect();
        flow.messages.extend(entries);
        flow.round_results.clear();
        flow.need = Some(crate::state::FlowNeed::NextTurn);
    }
}

fn progress(env: &StepEnv<'_>, st: &mut SchedulerState, out: &mut Vec<Command>) {
    loop {
        if st.is_terminal() {
            return;
        }
        if matches!(st.phase, Phase::Open { .. }) {
            if progress_open(env, st, out) {
                continue;
            }
            return;
        }
        if !progress_idle(env, st, out) {
            return;
        }
    }
}

/// One pass over an open superstep. Returns true when the caller should
/// loop (state changed), false to yield for external events.
fn progress_open(env: &StepEnv<'_>, st: &mut SchedulerState, out: &mut Vec<Command>) -> bool {
    let draining = st.cancel.is_some() || st.exhausted.is_some();

    // Abstract flows with settled needs: emit their next effects in
    // canonical node order (never emission-time order).
    if !draining {
        let needy = flow_owners(st);
        if !needy.is_empty() {
            for (i, path) in needy {
                let need =
                    st.abstract_flows.get_mut(&flow_key(i, &path)).and_then(|f| f.need.take());
                match need {
                    Some(crate::state::FlowNeed::NextTurn) => {
                        dispatch_llm_turn(env, st, i, &path, out);
                    }
                    Some(crate::state::FlowNeed::Tools(calls)) => {
                        for call in calls {
                            dispatch_flow_tool(env, st, i, &path, call, out);
                        }
                    }
                    None => {}
                }
            }
            return true;
        }
    }

    // Retries (nodes flipped back to Ready inside the open step).
    let retry_nodes: Vec<usize> = (0..env.plan.nodes.len())
        .filter(|&i| st.node_state[i] == NodeState::Ready)
        .collect();
    if !retry_nodes.is_empty() && !draining {
        for i in retry_nodes {
            dispatch_node(env, st, i, out);
        }
        return true;
    }

    // Send-task retries (tasks flipped back to Queued inside the open step).
    let retry_tasks: Vec<String> = st
        .send_tasks
        .iter()
        .filter(|(_, t)| t.state == crate::state::SendTaskState::Queued)
        .map(|(p, _)| p.clone())
        .collect();
    if !retry_tasks.is_empty() && !draining {
        for p in retry_tasks {
            dispatch_send_task(env, st, &p, out);
        }
        return true;
    }

    let (outstanding_empty, only_asks_left) = match &st.phase {
        Phase::Open { outstanding, .. } => {
            let only_asks = !outstanding.is_empty()
                && outstanding
                    .iter()
                    .all(|k| st.pending_asks.values().any(|p| p.key == *k));
            (outstanding.is_empty(), only_asks)
        }
        _ => unreachable!("progress_open requires Phase::Open"),
    };

    if outstanding_empty || (st.cancel.is_some() && only_asks_left) {
        if st.cancel.is_some() && only_asks_left {
            // A canceled run does not wait for humans: abandon the asks.
            if let Phase::Open { outstanding, .. } = &mut st.phase {
                outstanding.clear();
            }
            st.pending_asks.clear();
            st.announced_asks.clear();
        }
        close_superstep(env, st, out);
        return true;
    }
    if only_asks_left {
        // Pause the wall segment: parked time is elapsed, never wall.
        if let Some(open) = st.wall_open.take() {
            st.spent.wall_ms += st.clock_ms.saturating_sub(open);
            st.paused_at = Some(st.clock_ms);
        }
        park(st, out);
    }
    false
}

/// One pass over the idle phase. Returns true to keep looping.
fn progress_idle(env: &StepEnv<'_>, st: &mut SchedulerState, out: &mut Vec<Command>) -> bool {
    if let Some((by, reason)) = st.cancel.clone() {
        finish(st, out, RunOutcome::Canceled { by, reason });
        return false;
    }
    if let Some(axis) = st.exhausted {
        finish(st, out, RunOutcome::BudgetExhausted { axis });
        return false;
    }
    if let Some((idx, detail)) = st.failed.clone() {
        finish(
            st,
            out,
            RunOutcome::Failed { node: env.plan.nodes[idx].clone(), detail },
        );
        return false;
    }
    let ready: Vec<usize> = (0..env.plan.nodes.len())
        .filter(|&i| st.node_state[i] == NodeState::Ready)
        .collect();
    let queued_tasks: Vec<String> = st
        .send_tasks
        .iter()
        .filter(|(_, t)| t.state == crate::state::SendTaskState::Queued)
        .map(|(p, _)| p.clone())
        .collect();
    // Abstract flows whose next turn survived a reservation refusal: a
    // fork with raised budgets resumes exactly here (the pinned promise).
    let needy_flows = flow_owners(st);
    if ready.is_empty() && queued_tasks.is_empty() && needy_flows.is_empty() {
        // A step call before any Start is a driver protocol slip, not a
        // stall — yield; terminalizing would brick the run id.
        if st.superstep == 0 && st.node_state[0] == NodeState::Waiting {
            return false;
        }
        // Defensive: an in-flight node at Idle means a driver protocol bug,
        // not a stall — yield rather than misreport.
        if st
            .node_state
            .iter()
            .any(|s| matches!(s, NodeState::Dispatched | NodeState::AwaitingClient))
        {
            return false;
        }
        let completed = env
            .plan
            .terminals()
            .iter()
            .any(|&t| st.node_state[t] == NodeState::DoneOk);
        if completed {
            finish(st, out, RunOutcome::Completed);
        } else {
            let node = stalled_culprit(env, st);
            finish(st, out, RunOutcome::Stalled { node });
        }
        return false;
    }

    // Budget pre-flight, every axis, before any dispatch (§6.7).
    let axes = [
        (BudgetAxis::Supersteps, st.spent.supersteps, Some(env.budgets.max_supersteps)),
        (BudgetAxis::WallMs, st.spent.wall_ms, env.budgets.max_wall_ms),
        (
            BudgetAxis::Tokens,
            st.spent.input_tokens + st.spent.output_tokens,
            env.budgets.max_tokens,
        ),
        (BudgetAxis::Usd, st.spent.usd_micros, env.budgets.max_usd_micros),
        (BudgetAxis::Storage, st.spent.storage_bytes, env.budgets.max_storage_bytes),
    ];
    for (axis, spent, max) in axes {
        if let Some(max) = max {
            if spent >= max {
                finish(st, out, RunOutcome::BudgetExhausted { axis });
                return false;
            }
        }
    }

    // Open the superstep.
    st.superstep += 1;
    st.spent.supersteps += 1;
    st.wall_open = Some(st.clock_ms);
    st.phase = Phase::Open {
        outstanding: Default::default(),
        results: BTreeMap::new(),
        send_results: BTreeMap::new(),
        record: DecisionRecord {
            superstep: st.superstep,
            clock_open_ms: st.clock_ms,
            ..DecisionRecord::default()
        },
    };
    apply_inbox(st);
    for i in ready {
        dispatch_node(env, st, i, out);
    }
    for p in queued_tasks {
        dispatch_send_task(env, st, &p, out);
    }
    for (i, path) in needy_flows {
        let need = st.abstract_flows.get_mut(&flow_key(i, &path)).and_then(|fl| fl.need.take());
        match need {
            Some(crate::state::FlowNeed::NextTurn) => dispatch_llm_turn(env, st, i, &path, out),
            Some(crate::state::FlowNeed::Tools(calls)) => {
                for call in calls {
                    dispatch_flow_tool(env, st, i, &path, call, out);
                }
            }
            None => {}
        }
    }
    true
}

/// Dispatch one queued Send task: its own attempt counter, its own
/// task-scoped input, the target node's Host executor.
fn dispatch_send_task(
    env: &StepEnv<'_>,
    st: &mut SchedulerState,
    path: &str,
    out: &mut Vec<Command>,
) {
    let Some(task) = st.send_tasks.get_mut(path) else { return };
    task.attempt += 1;
    task.state = crate::state::SendTaskState::Running;
    let (target, attempt, input) = (task.node_idx, task.attempt, task.input.clone());
    let executor = env.executors[target].clone();
    if let NodeExecutor::Abstract { .. } = &executor {
        start_flow(env, st, target, path, input, out);
        return;
    }
    let key = JournalKey {
        run_id: st.run_id.clone(),
        task_path: path.to_string(),
        node: env.plan.nodes[target].clone(),
        attempt,
        effect_seq: 0,
        kind: EffectKind::Tool,
    };
    let clock = st.clock_ms;
    let superstep = st.superstep;
    if let Phase::Open { outstanding, record, .. } = &mut st.phase {
        outstanding.insert(key.clone());
        record.task_dispatched.push((path.to_string(), attempt));
    }
    out.push(Command::WriteIntent {
        key: key.clone(),
        executor: executor.clone(),
        input: input.clone(),
        superstep,
        clock_ms: clock,
    });
    out.push(Command::Dispatch { key, executor, input });
}

/// Allocate the next attempt for a Ready node and emit its first effect —
/// a Host/Subgraph dispatch, a Client ask, or an abstract node's opening
/// LLM turn.
fn dispatch_node(env: &StepEnv<'_>, st: &mut SchedulerState, i: usize, out: &mut Vec<Command>) {
    st.attempt[i] += 1;
    let executor = env.executors[i].clone();

    // Abstract nodes run as an LLM loop: fresh flow, then the first turn.
    if let NodeExecutor::Abstract { .. } = &executor {
        st.node_state[i] = NodeState::Dispatched;
        let input = st.context.clone();
        start_flow(env, st, i, "", input, out);
        return;
    }

    let key = JournalKey {
        run_id: st.run_id.clone(),
        task_path: String::new(),
        node: env.plan.nodes[i].clone(),
        attempt: st.attempt[i],
        effect_seq: 0,
        kind: EffectKind::Tool,
    };
    let input = st.context.clone();
    let clock = st.clock_ms;
    let superstep = st.superstep;

    {
        let Phase::Open { outstanding, record, .. } = &mut st.phase else {
            return;
        };
        outstanding.insert(key.clone());
        record.dispatched.push((i, st.attempt[i]));
    }

    out.push(Command::WriteIntent {
        key: key.clone(),
        executor: executor.clone(),
        input: input.clone(),
        superstep,
        clock_ms: clock,
    });
    match &executor {
        NodeExecutor::Host { .. } | NodeExecutor::Subgraph { .. } => {
            st.node_state[i] = NodeState::Dispatched;
            out.push(Command::Dispatch { key, executor, input });
        }
        NodeExecutor::Client { tool_name, .. } => {
            st.node_state[i] = NodeState::AwaitingClient;
            let ask = Ask {
                tool_call_id: key.tool_call_id(),
                node: env.plan.nodes[i].clone(),
                tool_name: tool_name.clone(),
                input,
                expires_at_sec: env
                    .ask_ttl_sec
                    .map(|ttl| (st.clock_ms / 1000) as i64 + ttl),
                // v1: every Client ask is an approval boundary (§6.6);
                // responder ≠ triggering principal is enforced by the
                // driver on respond.
                approval: true,
            };
            st.pending_asks
                .insert(ask.tool_call_id.clone(), PendingAsk { key, node_idx: i, ask });
        }
        NodeExecutor::Abstract { .. } => unreachable!("handled above"),
    }
}

/// Which transcript entries a fold may replace: everything between the node's
/// input and the last [`KEEP_TAIL`] entries. `None` when that middle is empty —
/// there is nothing a summary could stand in for.
///
/// Two rules make the result safe to send:
///
/// - **`messages[0]` is never folded.** It is the node's instruction and its
///   input state — the only entry that says what the node is for. A summary of
///   the task cannot replace the task.
/// - **The cut never separates a `tool` result from the `assistant` entry that
///   issued it.** Providers reject a tool result with no preceding tool call,
///   so the cut moves FORWARD past any leading `tool` entries, shrinking the
///   kept tail rather than splitting a round. Moving it backward would instead
///   fold the assistant entry and orphan the results.
///
/// Pure, and only ever called at a `NextTurn` boundary (`dispatch_llm_turn`
/// runs with `pending_tools` empty by construction), so a fold cannot land
/// mid-round.
fn fold_range(messages: &[Value]) -> Option<(usize, usize)> {
    let from = 1;
    let mut to = messages.len().saturating_sub(KEEP_TAIL);
    if to <= from {
        return None;
    }
    // Keep the round intact: a `tool` entry at the cut belongs to an assistant
    // entry inside the folded range, so advance until the boundary is not one.
    while to < messages.len() && messages[to].get("role").and_then(Value::as_str) == Some("tool") {
        to += 1;
    }
    if to <= from || to > messages.len() {
        return None;
    }
    Some((from, to))
}

/// Emit the summarizer turn for `from..to` of node `i`'s transcript.
///
/// An ordinary [`EffectKind::Llm`] effect in every respect that matters: it
/// consumes an `effect_seq`, spends the per-call reservation, and is journaled
/// as intent + result. That is the whole design — a fold is one more journaled
/// turn, so `verify` answers it from the journal like any other and never calls
/// the model. The `fold` key in its input is what tells the driver to offer the
/// summarizer NO tools, and what makes the decision visible in `run inspect`.
fn emit_fold_turn(
    env: &StepEnv<'_>,
    st: &mut SchedulerState,
    i: usize,
    path: &str,
    from: usize,
    to: usize,
    out: &mut Vec<Command>,
) {
    let flow_id = flow_key(i, path);
    let Some(flow) = st.abstract_flows.get_mut(&flow_id) else { return };
    if flow.next_effect_seq >= env.max_effects_per_attempt {
        fail_abstract(env, st, i, path, "llm loop exceeded max_effects_per_attempt");
        return;
    }
    let effect_seq = flow.next_effect_seq;
    flow.next_effect_seq += 1;
    flow.need = None;
    let mut messages: Vec<Value> = flow.messages[from..to].to_vec();
    messages.push(serde_json::json!({ "role": "user", "content": FOLD_PROMPT }));
    flow.folding = Some(crate::state::FoldInFlight { from, to, effect_seq });
    let key = JournalKey {
        run_id: st.run_id.clone(),
        task_path: path.to_string(),
        node: env.plan.nodes[i].clone(),
        attempt: flow_attempt(st, i, path),
        effect_seq,
        kind: EffectKind::Llm,
    };
    let input = serde_json::json!({
        "messages": messages,
        "fold": { "from": from, "to": to, "seq": effect_seq, "prompt_v": FOLD_PROMPT_V },
    });
    emit_effect(env, st, i, path, key, input, out);
}

/// Emit the next LLM turn of node `i`'s abstract flow, with the §6.7
/// per-dispatch token reservation: `spent + reserve` must fit BEFORE the
/// effect is emitted — the refinement pre-flight-only checking could not
/// give, now that LLM effects carry a reservable `max_tokens`.
fn dispatch_llm_turn(
    env: &StepEnv<'_>,
    st: &mut SchedulerState,
    i: usize,
    path: &str,
    out: &mut Vec<Command>,
) {
    let flow_id = flow_key(i, path);
    if let Some(max) = env.budgets.max_tokens {
        let spent = st.spent.input_tokens + st.spent.output_tokens;
        if spent + env.llm_reserve_tokens > max {
            st.exhausted = Some(crate::error::BudgetAxis::Tokens);
            // The un-dispatched turn survives as a need, so a resume with a
            // raised ceiling picks the loop up exactly here.
            if let Some(flow) = st.abstract_flows.get_mut(&flow_id) {
                flow.need = Some(crate::state::FlowNeed::NextTurn);
            }
            return;
        }
    }
    // The context ceiling, checked AFTER the budget reservation and BEFORE the
    // effect cap: a fold spends an effect like any turn, so it must not be the
    // thing that pushes the node over its count without the count having been
    // checked. Measured on the PROVIDER's reported prompt tokens for the last
    // completed turn — never on an estimate — plus the output we are about to
    // reserve, because a prompt that fits and a reply that does not is the
    // same rejection.
    if let Some(ceiling) = env.llm_context_tokens {
        let projected = st
            .abstract_flows
            .get(&flow_id)
            .map(|f| f.last_prompt_tokens)
            .unwrap_or(0)
            .saturating_add(env.llm_reserve_tokens);
        let idle = st.abstract_flows.get(&flow_id).is_some_and(|f| f.folding.is_none());
        if idle && projected > ceiling {
            match st.abstract_flows.get(&flow_id).and_then(|f| fold_range(&f.messages)) {
                Some((from, to)) => {
                    emit_fold_turn(env, st, i, path, from, to, out);
                    return;
                }
                // Nothing foldable: the node's input plus the kept tail alone
                // exceed the ceiling. Another fold cannot help, and looping
                // would spend the effect budget summarizing summaries.
                None => {
                    let tokens = st
                        .abstract_flows
                        .get(&flow_id)
                        .map(|f| f.last_prompt_tokens)
                        .unwrap_or(0);
                    let detail = RunError::ContextExceeded {
                        node: env.plan.nodes[i].clone(),
                        tokens,
                        ceiling,
                    }
                    .to_string();
                    fail_abstract_coded(env, st, i, path, &detail);
                    return;
                }
            }
        }
    }
    let Some(flow) = st.abstract_flows.get_mut(&flow_id) else { return };
    if flow.next_effect_seq >= env.max_effects_per_attempt {
        fail_abstract(env, st, i, path, "llm loop exceeded max_effects_per_attempt");
        return;
    }
    let effect_seq = flow.next_effect_seq;
    flow.next_effect_seq += 1;
    flow.need = None;
    let messages = flow.messages.clone();
    let key = JournalKey {
        run_id: st.run_id.clone(),
        task_path: path.to_string(),
        node: env.plan.nodes[i].clone(),
        attempt: flow_attempt(st, i, path),
        effect_seq,
        kind: EffectKind::Llm,
    };
    let input = serde_json::json!({ "messages": messages });
    emit_effect(env, st, i, path, key, input, out);
}

/// Emit one model-issued tool call from node `i`'s flow.
fn dispatch_flow_tool(
    env: &StepEnv<'_>,
    st: &mut SchedulerState,
    i: usize,
    path: &str,
    call: crate::state::PendingToolCall,
    out: &mut Vec<Command>,
) {
    let flow_id = flow_key(i, path);
    // Draining (exhausted mid-pass): preserve the call as a need instead of
    // emitting new work — state.rs pins "no new work is emitted".
    if st.exhausted.is_some() {
        if let Some(flow) = st.abstract_flows.get_mut(&flow_id) {
            match &mut flow.need {
                Some(crate::state::FlowNeed::Tools(calls)) => calls.push(call),
                other => *other = Some(crate::state::FlowNeed::Tools(vec![call])),
            }
        }
        return;
    }
    let Some(flow) = st.abstract_flows.get_mut(&flow_id) else { return };
    if flow.next_effect_seq >= env.max_effects_per_attempt {
        fail_abstract(env, st, i, path, "llm loop exceeded max_effects_per_attempt");
        return;
    }
    // Resolve the offered tool BEFORE booking anything: a resolution set
    // that changed across a fork must fail the node loudly, not panic —
    // and never leave a booked pending tool with no outstanding key.
    let NodeExecutor::Abstract { tools } = &env.executors[i] else { return };
    let Some(tool) = tools.iter().find(|t| t.tool_name == call.tool_name).cloned() else {
        fail_abstract(
            env,
            st,
            i,
            path,
            &format!("offered tool '{}' missing from the manifest's executors", call.tool_name),
        );
        return;
    };
    let Some(flow) = st.abstract_flows.get_mut(&flow_id) else { return };
    let effect_seq = flow.next_effect_seq;
    flow.next_effect_seq += 1;
    let arguments = call.arguments.clone();
    flow.pending_tools.insert(effect_seq, call.clone());
    let key = JournalKey {
        run_id: st.run_id.clone(),
        task_path: path.to_string(),
        node: env.plan.nodes[i].clone(),
        attempt: flow_attempt(st, i, path),
        effect_seq,
        kind: EffectKind::Tool,
    };
    let host = NodeExecutor::Host {
        tool_hash: tool.tool_hash.clone(),
        tool_name: tool.tool_name.clone(),
    };
    let clock = st.clock_ms;
    let superstep = st.superstep;
    if let Phase::Open { outstanding, .. } = &mut st.phase {
        outstanding.insert(key.clone());
    }
    out.push(Command::WriteIntent {
        key: key.clone(),
        executor: host.clone(),
        input: arguments.clone(),
        superstep,
        clock_ms: clock,
    });
    out.push(Command::Dispatch { key, executor: host, input: arguments });
}

/// Shared effect emission for abstract turns.
fn emit_effect(
    env: &StepEnv<'_>,
    st: &mut SchedulerState,
    i: usize,
    path: &str,
    key: JournalKey,
    input: Value,
    out: &mut Vec<Command>,
) {
    let executor = env.executors[i].clone();
    let clock = st.clock_ms;
    let superstep = st.superstep;
    if let Phase::Open { outstanding, record, .. } = &mut st.phase {
        outstanding.insert(key.clone());
        if key.effect_seq == 0 {
            if path.is_empty() {
                record.dispatched.push((i, key.attempt));
            } else {
                record.task_dispatched.push((path.to_string(), key.attempt));
            }
        }
    }
    out.push(Command::WriteIntent {
        key: key.clone(),
        executor: executor.clone(),
        input: input.clone(),
        superstep,
        clock_ms: clock,
    });
    out.push(Command::Dispatch { key, executor, input });
}

/// Fail an abstract node (loop bound, terminal model failure): fail-fast
/// semantics, the same path a Host node's retry exhaustion takes. The detail is
/// classified `ExecutorError:` — the loop's own failures are the executor's as
/// far as a reader of the outcome is concerned.
fn fail_abstract(
    env: &StepEnv<'_>,
    st: &mut SchedulerState,
    i: usize,
    path: &str,
    detail: &str,
) {
    fail_abstract_coded(env, st, i, path, &format!("ExecutorError: {detail}"));
}

/// Fail an abstract node with a detail that already leads with its own
/// `RUN-Ennn`. Separate from [`fail_abstract`] because the workspace rule is
/// that the code is the LEADING token of what a user reads, and an
/// `ExecutorError:` prefix in front of one buries it — besides being wrong:
/// a transcript the scheduler refused to send is not an executor's failure.
fn fail_abstract_coded(
    env: &StepEnv<'_>,
    st: &mut SchedulerState,
    i: usize,
    path: &str,
    detail: &str,
) {
    st.abstract_flows.remove(&flow_key(i, path));
    if let Some(task) = st.send_tasks.get_mut(path) {
        task.state = crate::state::SendTaskState::Done;
    }
    st.node_state[i] = NodeState::DoneFailed;
    let named = st.failed.as_ref().map(|(n, _)| *n).unwrap_or(usize::MAX);
    if i < named {
        let at = if path.is_empty() { String::new() } else { format!(" (task {path})") };
        st.failed = Some((i, format!("{detail}{at}")));
    }
    let _ = env;
}

/// Park: checkpoint + envelope, once per ask (a parked run polled again
/// must not re-announce).
fn park(st: &mut SchedulerState, out: &mut Vec<Command>) {
    let fresh: Vec<Ask> = st
        .pending_asks
        .values()
        .filter(|p| !st.announced_asks.contains(&p.ask.tool_call_id))
        .map(|p| p.ask.clone())
        .collect();
    if fresh.is_empty() {
        return;
    }
    for a in &fresh {
        st.announced_asks.insert(a.tool_call_id.clone());
    }
    if let Phase::Open { record, .. } = &st.phase {
        let mut record = record.clone();
        // The park reading MUST be journaled: the wall segment accrued at
        // park uses st.clock_ms, and verify can only reproduce it from the
        // checkpoint's own record (feeding an unjournaled reading would
        // false-diverge any superstep that resolved an effect before
        // parking).
        record.clock_close_ms = st.clock_ms;
        let state_json = serde_json::to_value(&*st).unwrap_or(Value::Null);
        out.push(Command::WriteCheckpoint {
            superstep: st.superstep,
            decision_record: record,
            state_json,
        });
    }
    out.push(Command::EmitEnvelope { asks: fresh });
}

/// Close the open superstep: extract Send spawns, reduce in canonical
/// order, evaluate edges in canonical order (unless draining), checkpoint.
fn close_superstep(env: &StepEnv<'_>, st: &mut SchedulerState, out: &mut Vec<Command>) {
    let Phase::Open { mut results, mut send_results, mut record, .. } =
        std::mem::replace(&mut st.phase, Phase::Idle)
    else {
        return;
    };

    // Send extraction (§5.1): a result's reserved `$send` key is a spawn
    // decision, made HERE at close in canonical order — static spawners by
    // node index, then task spawners by path — journaled in the decision
    // record, and stripped before reduction. A malformed decision is
    // fail-fast (the spawner is named; no tasks spawn; the run drains).
    if st.failed.is_none() && st.cancel.is_none() {
        let mut decisions: Vec<(String, usize, Vec<Value>)> = Vec::new();
        for (i, r) in results.iter_mut() {
            if let Some(list) = take_send(r) {
                decisions.push((String::new(), *i, list));
            }
        }
        let task_nodes: BTreeMap<String, usize> = st
            .send_tasks
            .iter()
            .map(|(p, t)| (p.clone(), t.node_idx))
            .collect();
        for (path, r) in send_results.iter_mut() {
            if let Some(list) = take_send(r) {
                decisions.push((path.clone(), task_nodes[path], list));
            }
        }
        apply_spawns(env, st, &mut record, decisions);
    }

    // Reducers, canonical order (§5.2: state-merge order is canonical —
    // node index, never completion order; Send results after the static
    // set, in spawn order — task paths zero-pad their ordinals so
    // lexicographic IS numeric). An object result merges per top-level
    // key; anything else merges under the node's own id.
    let send_merges: Vec<(usize, Value)> = send_results
        .iter()
        .map(|(p, v)| (st.send_tasks[p].node_idx, v.clone()))
        .collect();
    for (node_idx, result) in results
        .iter()
        .map(|(i, v)| (*i, v.clone()))
        .chain(send_merges)
    {
        match result {
            Value::Object(map) => {
                for (k, v) in map {
                    let prev = st.context.get(&k).cloned();
                    let merged = (env.reduce)(&k, prev.as_ref(), &v);
                    st.context[k.as_str()] = merged;
                }
            }
            Value::Null => {}
            other => {
                let key = env.plan.nodes[node_idx].clone();
                let prev = st.context.get(&key).cloned();
                let merged = (env.reduce)(&key, prev.as_ref(), &other);
                st.context[key.as_str()] = merged;
            }
        }
    }

    // Edge evaluation — skipped when the run is dying (fail-fast /
    // cancel). An EXHAUSTED run still evaluates: it is designed to resume
    // under raised budgets, so its last superstep's firings must land
    // (dispatch stays drained either way — readiness without dispatch).
    if st.failed.is_none() && st.cancel.is_none() {
        for i in results.keys().copied().collect::<Vec<_>>() {
            for ei in env.plan.out_edges[i].clone() {
                let edge = &env.plan.edges[ei];
                let outcome = if edge.max_cycles.is_some_and(|m| st.edge_fired[ei] >= m) {
                    EdgeOutcome::CycleExhausted
                } else {
                    // Unconditional edges always fire; the evaluator is only
                    // consulted for edges that carry a condition.
                    let fires = (edge.cond.is_none() && edge.cond_raw.is_none())
                        || (env.eval_cond)(edge, &st.context);
                    if fires { EdgeOutcome::Fired } else { EdgeOutcome::CondFalse }
                };
                if outcome == EdgeOutcome::Fired {
                    st.edge_fired[ei] += 1;
                }
                let gen = deliver(env, st, ei, outcome == EdgeOutcome::Fired);
                record.edges.push((ei, gen, outcome));
            }
        }
        propagate_death(env, st, &mut record);
    }

    record.clock_close_ms = st.clock_ms;
    // Close the active wall segment (open→park segments were already
    // charged at park time). A superstep that parked charges NOTHING after
    // the park; the park→close span accrues as `elapsed` instead — both
    // endpoints are journaled readings, so replay reproduces both figures
    // byte-exactly (§6.7: a three-day approval pause is reported, never
    // billed — and never a replay divergence either).
    if let Some(open) = st.wall_open.take() {
        st.spent.wall_ms += st.clock_ms.saturating_sub(open);
    }
    if let Some(paused) = st.paused_at.take() {
        st.elapsed_ms += st.clock_ms.saturating_sub(paused);
    }
    let state_json = serde_json::to_value(&*st).unwrap_or(Value::Null);
    out.push(Command::WriteCheckpoint {
        superstep: st.superstep,
        decision_record: record,
        state_json,
    });
}

/// Pull a result's `$send` list out (stripping the key so reducers never
/// see it). Returns None when the result carries no spawn decision.
fn take_send(result: &mut Value) -> Option<Vec<Value>> {
    let obj = result.as_object_mut()?;
    let taken = obj.remove("$send")?;
    match taken {
        Value::Array(list) => Some(list),
        other => Some(vec![other]), // validated (and refused) in apply_spawns
    }
}

/// Per-decision spawn cap (also the per-parent lifetime cap via the
/// 4-digit ordinal space). Far above any sane fan-out; a backstop, not a
/// tuning knob.
const MAX_SPAWNS: usize = 4096;

/// Validate and apply the superstep's spawn decisions: fresh task paths
/// (`parent/NNNN`, monotonic per parent), targets pinned Host-executed,
/// the target node preempted into `Dispatched`. Violations are fail-fast
/// naming the spawner.
fn apply_spawns(
    env: &StepEnv<'_>,
    st: &mut SchedulerState,
    record: &mut DecisionRecord,
    decisions: Vec<(String, usize, Vec<Value>)>,
) {
    let fail = |st: &mut SchedulerState, spawner: usize, why: String| {
        st.node_state[spawner] = NodeState::DoneFailed;
        let named = st.failed.as_ref().map(|(i, _)| *i).unwrap_or(usize::MAX);
        if spawner < named {
            st.failed = Some((spawner, format!("ExecutorError: {why}")));
        }
    };
    // Validate everything before spawning anything: a half-applied spawn
    // decision would leave tasks the journaled record never mentions.
    // (parent path, spawner idx, [(target idx, task input)]).
    type ValidatedSpawn = (String, usize, Vec<(usize, Value)>);
    let mut validated: Vec<ValidatedSpawn> = Vec::new();
    for (parent, spawner, list) in decisions {
        if list.len() > MAX_SPAWNS {
            fail(st, spawner, format!("$send spawns {} tasks (cap {MAX_SPAWNS})", list.len()));
            return;
        }
        let mut entries = Vec::with_capacity(list.len());
        for e in &list {
            let Some(target_name) = e.get("node").and_then(|n| n.as_str()) else {
                fail(
                    st,
                    spawner,
                    "$send entries must be {\"node\": \"<name>\", \"input\": …}".into(),
                );
                return;
            };
            let Some(target) = env.plan.nodes.iter().position(|n| n == target_name) else {
                fail(st, spawner, format!("$send targets unknown node '{target_name}'"));
                return;
            };
            if !matches!(
                env.executors[target],
                NodeExecutor::Host { .. } | NodeExecutor::Abstract { .. }
            ) {
                fail(
                    st,
                    spawner,
                    format!("$send target '{target_name}' is not a Host tool or abstract node"),
                );
                return;
            }
            if target == spawner {
                fail(st, spawner, format!("$send target '{target_name}' is the spawner itself"));
                return;
            }
            let input = e.get("input").cloned().unwrap_or_else(|| serde_json::json!({}));
            entries.push((target, input));
        }
        validated.push((parent, spawner, entries));
    }
    for (parent, spawner, entries) in validated {
        for (target, input) in entries {
            let counter = st.spawn_counter.entry(parent.clone()).or_insert(0);
            let ordinal = *counter;
            if ordinal > 9999 {
                // The 4-digit pad is what makes lexicographic order spawn
                // order; past it the merge-order invariant breaks — fail
                // the spawner loudly instead.
                fail(st, spawner, format!(
                    "spawn ordinal space exhausted under '{parent}' (9999 max per parent)"
                ));
                return;
            }
            *counter += 1;
            let path = format!("{parent}/{ordinal:04}");
            st.send_tasks.insert(
                path.clone(),
                crate::state::SendTask {
                    node_idx: target,
                    input,
                    attempt: 0,
                    state: crate::state::SendTaskState::Queued,
                },
            );
            st.node_state[target] = NodeState::Dispatched;
            record.spawns.push((path, target));
        }
    }
}

/// Deliver an edge resolution to its destination, with re-entry. Returns
/// the destination generation the delivery landed in.
fn deliver(env: &StepEnv<'_>, st: &mut SchedulerState, ei: usize, fired: bool) -> u32 {
    let dst = env.plan.edges[ei].dst;
    let resolved = matches!(
        st.node_state[dst],
        NodeState::DoneOk | NodeState::DoneFailed | NodeState::Dead
    );
    if resolved {
        if !fired {
            return st.node_gen[dst]; // a dead edge never re-enters
        }
        st.node_gen[dst] += 1;
        st.node_state[dst] = NodeState::Waiting;
        // Attempt stays MONOTONIC (journal keys must be generation-unique);
        // the new generation's retry budget measures from here.
        st.attempt_base[dst] = st.attempt[dst];
        st.in_res[dst].clear();
        for &other in &env.plan.in_edges[dst] {
            if other == ei {
                continue;
            }
            let src = env.plan.edges[other].src;
            let src_unresolved = matches!(
                st.node_state[src],
                NodeState::Waiting
                    | NodeState::Ready
                    | NodeState::Dispatched
                    | NodeState::AwaitingClient
            );
            if !src_unresolved {
                st.in_res[dst].insert(other, EdgeRes::Dead);
            }
        }
        st.in_res[dst].insert(ei, EdgeRes::Fired);
    } else {
        st.in_res[dst]
            .insert(ei, if fired { EdgeRes::Fired } else { EdgeRes::Dead });
    }
    refresh_readiness(env, st, dst);
    st.node_gen[dst]
}

/// Waiting → Ready when no in-edge is Pending and ≥1 Fired; → Dead when all
/// resolved and none Fired (§6.2: edges must RESOLVE, not fire).
///
/// A node's FIRST generation (`node_gen[i] == 0`) only gates on in-edges
/// that could possibly have resolved by then: a back-edge that closes a
/// cycle through this very node (`env.plan.cycle_edge`) cannot fire before
/// the node's own first run, so requiring it here would deadlock any cycle
/// whose re-entry point isn't the plan's entry node (RUN issue #33) — every
/// OTHER in-edge into a reachable non-root node is guaranteed non-back (the
/// DFS tree edge that first discovered it, at minimum), so this never
/// starves a node of every gating edge. The entry node sidesteps this rule
/// entirely via an unconditional bootstrap (`apply_event`'s `Start`
/// handler), since it may have zero in-edges at all. Once a node is
/// re-entered (generation ≥ 1), `deliver`'s re-entry branch already
/// resolves every other in-edge from concrete current node state, so the
/// full AND-join applies again from generation 1 onward.
fn refresh_readiness(env: &StepEnv<'_>, st: &mut SchedulerState, i: usize) {
    if st.node_state[i] != NodeState::Waiting {
        return;
    }
    let ins = &env.plan.in_edges[i];
    let gen0 = st.node_gen[i] == 0;
    let all_gated_resolved = ins
        .iter()
        .all(|&e| (gen0 && env.plan.cycle_edge[e]) || st.in_res[i].contains_key(&e));
    if !all_gated_resolved {
        return;
    }
    let any_fired = ins.iter().any(|e| st.in_res[i].get(e) == Some(&EdgeRes::Fired));
    st.node_state[i] = if any_fired { NodeState::Ready } else { NodeState::Dead };
}

/// Dead-path propagation to fixpoint, canonical order.
fn propagate_death(env: &StepEnv<'_>, st: &mut SchedulerState, record: &mut DecisionRecord) {
    loop {
        let mut changed = false;
        for i in 0..env.plan.nodes.len() {
            if st.node_state[i] != NodeState::Dead {
                continue;
            }
            for &ei in &env.plan.out_edges[i] {
                let dst = env.plan.edges[ei].dst;
                if st.node_state[dst] == NodeState::Waiting
                    && !st.in_res[dst].contains_key(&ei)
                {
                    st.in_res[dst].insert(ei, EdgeRes::Dead);
                    record.edges.push((ei, st.node_gen[dst], EdgeOutcome::DeadPath));
                    refresh_readiness(env, st, dst);
                    changed = true;
                }
            }
        }
        if !changed {
            return;
        }
    }
}

/// The `Stalled` diagnosis: the lowest-index completed node whose out-edges
/// all resolved without ever firing.
fn stalled_culprit(env: &StepEnv<'_>, st: &SchedulerState) -> String {
    for i in 0..env.plan.nodes.len() {
        if st.node_state[i] == NodeState::DoneOk
            && !env.plan.out_edges[i].is_empty()
            && env.plan.out_edges[i].iter().all(|&ei| st.edge_fired[ei] == 0)
        {
            return env.plan.nodes[i].clone();
        }
    }
    // No completed node's own out-edges explain it (every completed node
    // fired something downstream) — name the lowest-index node that never
    // got a chance to run at all, the actual culprit, rather than blaming
    // the entry unconditionally when its own edge fired correctly.
    for i in 0..env.plan.nodes.len() {
        if st.node_state[i] == NodeState::Waiting {
            return env.plan.nodes[i].clone();
        }
    }
    env.plan.nodes[0].clone()
}

fn finish(st: &mut SchedulerState, out: &mut Vec<Command>, outcome: RunOutcome) {
    st.phase = Phase::Finished(outcome.clone());
    out.push(Command::Finish { outcome });
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// No cap = the transcript entry every run before this knob built. Not
    /// "close enough": byte-identical, so an unbounded run's checkpoints are
    /// unchanged and its `verify` still passes.
    #[test]
    fn no_cap_passes_the_result_through_verbatim() {
        let big = json!({"rows": vec!["x"; 5_000]});
        assert_eq!(bound_tool_content(&big, None, 3, 7), big);
    }

    /// The boundary, from both sides. At exactly the cap nothing happens; one
    /// character more and the entry becomes the bounded shape.
    #[test]
    fn the_cap_is_inclusive() {
        let at = Value::String("a".repeat(64));
        assert_eq!(bound_tool_content(&at, Some(64), 0, 0), at);

        let over = Value::String("a".repeat(65));
        let bounded = bound_tool_content(&over, Some(64), 3, 7);
        assert_eq!(bounded["truncated"], true);
        assert_eq!(bounded["chars"], 65, "the TRUE length, not the kept length");
        assert_eq!(bounded["head"].as_str().unwrap().chars().count(), 32);
        assert_eq!(bounded["tail"].as_str().unwrap().chars().count(), 32);
        // Where the whole thing still is. A model told "truncated" with no
        // pointer has learned nothing it can act on, and neither has a human
        // reading the transcript back.
        assert_eq!(bounded["journal"], json!({"attempt": 3, "effect_seq": 7}));
    }

    /// The cap counts CHARACTERS, which is what it is called. Slicing by bytes
    /// here would both mis-measure a multibyte result and be able to cut a
    /// UTF-8 sequence in half.
    #[test]
    fn a_multibyte_result_is_cut_on_character_boundaries() {
        // 40 characters, 120 bytes: a byte cap of 20 would land mid-sequence.
        let text: String = "日本語です".repeat(8);
        assert_eq!(text.chars().count(), 40);
        assert!(text.len() > 40, "the point of the case");

        let bounded = bound_tool_content(&Value::String(text.clone()), Some(20), 1, 2);
        assert_eq!(bounded["chars"], 40);
        let head = bounded["head"].as_str().unwrap();
        let tail = bounded["tail"].as_str().unwrap();
        assert_eq!(head.chars().count(), 10);
        assert_eq!(tail.chars().count(), 10);
        assert!(text.starts_with(head), "the head is a real prefix");
        assert!(text.ends_with(tail), "the tail is a real suffix");
    }

    /// A structured result is measured and shown as the JSON the model would
    /// have been handed, not as Rust's `Debug` of it.
    #[test]
    fn a_json_result_is_bounded_as_its_serialized_form() {
        let big = json!({"k": "v".repeat(200)});
        let bounded = bound_tool_content(&big, Some(40), 0, 1);
        assert_eq!(bounded["chars"], big.to_string().chars().count());
        assert!(bounded["head"].as_str().unwrap().starts_with(r#"{"k":"#), "{bounded}");
        assert!(bounded["tail"].as_str().unwrap().ends_with(r#""}"#), "{bounded}");
    }

    /// A degenerate cap must not panic or produce nonsense — it keeps nothing
    /// and still says how much there was and where it is.
    #[test]
    fn a_cap_of_zero_keeps_nothing_and_still_points_at_the_journal() {
        let bounded = bound_tool_content(&Value::String("abc".into()), Some(0), 9, 4);
        assert_eq!(bounded["chars"], 3);
        assert_eq!(bounded["head"], "");
        assert_eq!(bounded["tail"], "");
        assert_eq!(bounded["journal"]["effect_seq"], 4);
    }

    // ---- the fold's range decision -------------------------------------

    fn msg(role: &str) -> Value {
        json!({"role": role, "content": "x"})
    }

    /// A short transcript has no middle: `messages[0]` is the node's input and
    /// the rest is the kept tail, so there is nothing a summary could replace.
    /// Returning `None` here is what produces `RUN-E024` rather than a fold
    /// that would delete the task description.
    #[test]
    fn nothing_to_fold_below_the_kept_tail() {
        for n in 0..=KEEP_TAIL + 1 {
            let messages: Vec<Value> = (0..n).map(|_| msg("user")).collect();
            assert_eq!(fold_range(&messages), None, "len {n} must not be foldable");
        }
    }

    /// The first entry is the node's instruction and input state. A summary of
    /// the task cannot stand in for the task, so the range always starts at 1.
    #[test]
    fn the_nodes_input_is_never_folded() {
        let messages: Vec<Value> = (0..12).map(|_| msg("user")).collect();
        let (from, to) = fold_range(&messages).expect("a middle exists");
        assert_eq!(from, 1, "messages[0] is the node's own input");
        assert_eq!(to, 12 - KEEP_TAIL);
    }

    /// Providers reject a `tool` result whose `assistant` tool call is not in
    /// the request. So the cut moves FORWARD off a tool entry — shrinking the
    /// kept tail — rather than backward, which would fold the assistant entry
    /// and orphan the results that follow it.
    #[test]
    fn the_cut_never_separates_a_tool_result_from_its_assistant_turn() {
        // Tail of 4 would start at index 8, which is a `tool`: the cut must
        // advance to 10, the next non-tool entry.
        let messages = vec![
            msg("user"),      // 0 node input
            msg("assistant"), // 1
            msg("tool"),      // 2
            msg("user"),      // 3
            msg("assistant"), // 4
            msg("tool"),      // 5
            msg("user"),      // 6
            msg("assistant"), // 7
            msg("tool"),      // 8  <- naive cut lands here
            msg("tool"),      // 9
            msg("user"),      // 10
            msg("assistant"), // 11
        ];
        let (from, to) = fold_range(&messages).expect("a middle exists");
        assert_eq!(from, 1);
        assert_eq!(to, 10, "advanced past both tool entries");
        assert_ne!(
            messages[to].get("role").and_then(Value::as_str),
            Some("tool"),
            "the entry AFTER the fold must never be an orphaned tool result"
        );
    }

    /// A round wide enough to fill the whole tail — one assistant turn issuing
    /// eight tool calls — folds ENTIRELY, keeping no tail at all. That is the
    /// right answer, not a missing guard: every candidate tail entry is a `tool`
    /// result whose `assistant` entry sits inside the folded range, so keeping
    /// any of them would orphan it. Fold all of it and make progress, rather
    /// than refuse and kill the node with `RUN-E024`.
    #[test]
    fn a_round_that_fills_the_tail_folds_entirely_rather_than_orphaning_it() {
        let mut messages = vec![msg("user"), msg("assistant")];
        messages.extend((0..8).map(|_| msg("tool")));
        assert_eq!(fold_range(&messages), Some((1, 10)));
    }
}
