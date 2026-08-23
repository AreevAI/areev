//! The journal writer/reader (governed-agents §5.1) — the driver half of the
//! replay contract. Uses ONLY existing vocabulary: an intent is a Tool grain
//! stamped `Pending` before dispatch; its result **supersedes** it,
//! re-stating `run_id`, the `mg:step_action` link, and every key field —
//! supersession flips the intent's link rows off the current set, so a
//! result that forgot to re-state would erase the node's execution record
//! (the store-level trap pinned by
//! `superseding_an_execution_record_needs_the_link_restated`).
//!
//! Checkpoints are State grains: `context_data` carries the serialized
//! scheduler state + the superstep's decision record (inner State keys are
//! never compacted, so they round-trip verbatim), chained by `derived_from`.
//!
//! Reads are cursor-paginated (`run_grains`) and keyed by
//! `(task_path, node, attempt, effect_seq, kind)` — never by op-log order.

use areev_core::error::{AreevError, Hash, Result};
use areev_core::authz::HARNESS_NS;
use areev_core::types::{
    ExecutionStatus, ExecutorKind, FailureCause, Grain, Observation, State, Tool,
};
use areev_run_core::{DecisionRecord, EffectKind, EffectOutcome, FailCause, JournalKey, NodeExecutor};
use areev_store::Areev;
use serde_json::{json, Value};
use std::collections::BTreeMap;

/// Journal grains carry these extra fields so the key is reconstructible
/// from content alone (§5.1 mandatory content fields).
const F_TASK_PATH: &str = "task_path";
const F_NODE: &str = "node";
const F_ATTEMPT: &str = "attempt";
const F_EFFECT_SEQ: &str = "effect_seq";
const F_SUPERSTEP: &str = "superstep";
const F_EFFECT_KIND: &str = "effect_kind";
const F_USAGE_IN: &str = "usage_input_tokens";
const F_USAGE_OUT: &str = "usage_output_tokens";
const F_USD_MICROS: &str = "usage_usd_micros";
const F_JOURNAL_BYTES: &str = "usage_journal_bytes";

fn key_extras(t: &mut Tool, key: &JournalKey, superstep: u64, run_id: &str) {
    let ex = &mut t.common.extra_fields;
    ex.insert("run_id".into(), json!(run_id));
    ex.insert(F_TASK_PATH.into(), json!(key.task_path));
    ex.insert(F_NODE.into(), json!(key.node));
    ex.insert(F_ATTEMPT.into(), json!(key.attempt));
    ex.insert(F_EFFECT_SEQ.into(), json!(key.effect_seq));
    ex.insert(F_SUPERSTEP.into(), json!(superstep));
    ex.insert(F_EFFECT_KIND.into(), json!(key.kind.as_str()));
}

fn base_tool(
    key: &JournalKey,
    executor: &NodeExecutor,
    ns: &str,
    plan_hash: &Hash,
    clock_ms: u64,
    principal: &str,
) -> Tool {
    let (tool_hash, tool_name, ekind) = match executor {
        NodeExecutor::Host { tool_hash, tool_name } => {
            (tool_hash.clone(), tool_name.clone(), ExecutorKind::Host)
        }
        NodeExecutor::Client { tool_hash, tool_name } => {
            (tool_hash.clone(), tool_name.clone(), ExecutorKind::Client)
        }
        // An abstract node's model turns journal under the reserved
        // `mg:llm` name (the effect kind rides in `effect_kind`); a
        // subgraph effect journals under `mg:subgraph` with the child plan
        // as its spec.
        NodeExecutor::Abstract { .. } => {
            (String::new(), "mg:llm".to_string(), ExecutorKind::Host)
        }
        NodeExecutor::Subgraph { workflow_hash } => {
            (workflow_hash.clone(), "mg:subgraph".to_string(), ExecutorKind::Host)
        }
    };
    let mut t = Tool::new(&tool_name)
        .tool_call_id(&key.tool_call_id())
        .executor_kind(ekind)
        .created_at(clock_ms as i64)
        .namespace(ns)
        .step_action(&plan_hash.to_hex(), &key.node);
    if !tool_hash.is_empty() {
        t = t.spec_hash(&tool_hash);
    }
    t.common.author_did = Some(principal.to_string());
    t
}

/// Write the intent grain (Pending) — BEFORE dispatch, always.
#[allow(clippy::too_many_arguments)]
pub fn write_intent(
    m: &mut Areev,
    ns: &str,
    run_id: &str,
    plan_hash: &Hash,
    key: &JournalKey,
    executor: &NodeExecutor,
    input: &Value,
    superstep: u64,
    clock_ms: u64,
    principal: &str,
) -> Result<Hash> {
    let mut t = base_tool(key, executor, ns, plan_hash, clock_ms, principal);
    t.status = Some(ExecutionStatus::Pending);
    t.input = Some(input.clone());
    key_extras(&mut t, key, superstep, run_id);
    m.add(&t)
}

/// Write the result as a SUPERSESSION of the intent, re-stating identity,
/// link and run correlation (the §5.1 blocker-fix rule).
#[allow(clippy::too_many_arguments)]
pub fn write_result(
    m: &mut Areev,
    ns: &str,
    run_id: &str,
    plan_hash: &Hash,
    intent: &Hash,
    key: &JournalKey,
    executor: &NodeExecutor,
    outcome: &EffectOutcome,
    superstep: u64,
    clock_ms: u64,
    principal: &str,
) -> Result<Hash> {
    let mut t = base_tool(key, executor, ns, plan_hash, clock_ms, principal);
    key_extras(&mut t, key, superstep, run_id);
    match outcome {
        EffectOutcome::Completed {
            result,
            input_tokens,
            output_tokens,
            usd_micros,
            journal_bytes,
        } => {
            t.status = Some(ExecutionStatus::Completed);
            t.content = Some(result.to_string());
            let ex = &mut t.common.extra_fields;
            ex.insert(F_USAGE_IN.into(), json!(input_tokens));
            ex.insert(F_USAGE_OUT.into(), json!(output_tokens));
            ex.insert(F_USD_MICROS.into(), json!(usd_micros));
            // Storage accounting rides the grain so replayed spend equals
            // live spend — §6.7's "budgets are pure functions of the
            // journal" is only true if the accounting inputs ARE journaled.
            ex.insert(F_JOURNAL_BYTES.into(), json!(journal_bytes));
        }
        EffectOutcome::Failed { cause, detail, .. } => {
            t.status = Some(ExecutionStatus::Failed);
            t.is_error = Some(true);
            t.common
                .extra_fields
                .insert(F_JOURNAL_BYTES.into(), json!(outcome_journal_bytes(outcome)));
            t.failure_cause = Some(match cause {
                FailCause::Timeout => FailureCause::Timeout,
                FailCause::ExecutorError => FailureCause::ExecutorError,
                FailCause::SchemaValidationFailed => FailureCause::SchemaValidationFailed,
                FailCause::UserAborted => FailureCause::UserAborted,
                FailCause::Unknown => FailureCause::Unknown,
            });
            t.failure_detail = Some(detail.clone());
        }
    }
    m.supersede(intent, &mut t)
}

/// Record one refused outbound call as a Tier-2 Observation.
///
/// Deliberately NOT a journal entry: like the run-outcome record, replay never
/// sees it, so `verify` stays byte-identical whether or not a broker was
/// configured. It is evidence about the run, not a step of it.
pub fn write_egress_refusal(
    m: &mut Areev,
    run_id: &str,
    refusal: &crate::broker::EgressRefusal,
    clock_ms: u64,
    principal: &str,
) -> Result<Hash> {
    let caller = if refusal.caller.is_empty() { "connector" } else { &refusal.caller };
    let mut obs = Observation::new(principal, "system")
        .subject(&format!("run:{run_id}"))
        .object(&refusal.destination)
        .namespace(HARNESS_NS)
        .created_at(clock_ms as i64);
    let ex = &mut obs.common.extra_fields;
    ex.insert("run_id".into(), json!(run_id));
    ex.insert("observation_kind".into(), json!("egress_refusal"));
    ex.insert("caller".into(), json!(caller));
    ex.insert("destination".into(), json!(refusal.destination));
    ex.insert("reason".into(), json!(refusal.reason));
    m.add(&obs)
}

/// Record one brokered call that WENT OUT, as a Tier-2 Observation (#101).
///
/// The audit half of capability tools: a `wasm32-areev-io` module has no
/// socket of its own, so every byte it sends leaves through the broker and
/// every one of them lands here. Without this the memory would say a tool was
/// *allowed* to reach Gmail and never what it actually did.
///
/// Bodies are recorded as **digests**, never contents. A grain is immutable
/// and replicates, so an inbox body written into one cannot be taken back —
/// and the digest is what a reviewer needs anyway: it pins *which* request,
/// and it is the structure P2's verify-by-re-execution will match against.
/// The credential appears by NAME only, which is all the broker ever received
/// (`EgressRequest.credential` is a label; the value is attached internally),
/// so the audit record is safe by construction rather than by scrubbing.
///
/// Deliberately NOT a journal entry, exactly like [`write_egress_refusal`]:
/// replay never sees it, so `verify` stays byte-identical whether or not a
/// broker was configured. It is evidence about the run, not a step of it.
pub fn write_egress_call(
    m: &mut Areev,
    run_id: &str,
    call: &crate::broker::EgressCall,
    clock_ms: u64,
    principal: &str,
) -> Result<Hash> {
    let caller = if call.caller.is_empty() { "connector" } else { &call.caller };
    let mut obs = Observation::new(principal, "system")
        .subject(&format!("run:{run_id}"))
        .object(&call.url)
        .namespace(HARNESS_NS)
        .created_at(clock_ms as i64);
    let ex = &mut obs.common.extra_fields;
    ex.insert("run_id".into(), json!(run_id));
    ex.insert("observation_kind".into(), json!("egress_call"));
    ex.insert("caller".into(), json!(caller));
    ex.insert("method".into(), json!(call.method));
    ex.insert("destination".into(), json!(call.url));
    ex.insert("status".into(), json!(call.status));
    if call.redirects > 0 {
        ex.insert("redirects".into(), json!(call.redirects));
    }
    if let Some(d) = &call.request_digest {
        ex.insert("request_digest".into(), json!(d));
    }
    ex.insert("response_digest".into(), json!(call.response_digest));
    ex.insert("response_bytes".into(), json!(call.response_bytes));
    if let Some(c) = &call.credential {
        ex.insert("credential".into(), json!(c));
    }
    // Values and all, unlike the credential (#105). The caller supplied these,
    // so recording them discloses nothing it did not already hold, and it
    // turns "it was allowed to reach Google" into "it sent these four requests
    // billing this quota project". Omitted when empty, per the omit-default
    // rule these extras are canonically serialized under.
    if !call.headers.is_empty() {
        ex.insert("headers".into(), json!(call.headers));
    }
    m.add(&obs)
}

/// Write a checkpoint State grain, chained by `derived_from`.
#[allow(clippy::too_many_arguments)]
pub fn write_checkpoint(
    m: &mut Areev,
    ns: &str,
    run_id: &str,
    scheduler_state_json: &Value,
    record: &DecisionRecord,
    clock_ms: u64,
    prev: Option<&Hash>,
    principal: &str,
) -> Result<Hash> {
    let mut st = State::new(json!({
        "scheduler": scheduler_state_json,
        "decisions": record,
    }))
    .created_at(clock_ms as i64)
    .namespace(ns);
    st.common.author_did = Some(principal.to_string());
    st.common.extra_fields.insert("run_id".into(), json!(run_id));
    st.common
        .extra_fields
        .insert(F_SUPERSTEP.into(), json!(record.superstep));
    st.common
        .extra_fields
        .insert("checkpoint".into(), json!(true));
    if let Some(p) = prev {
        st.common.derived_from = Some(p.to_hex());
    }
    m.add(&st)
}

/// One journal entry: the intent and, when settled, its result.
#[derive(Debug, Clone)]
pub struct JournalEntry {
    pub key: JournalKey,
    pub intent: Hash,
    pub result: Option<(Hash, EffectOutcome)>,
    /// The result grain's journaled clock (`created_at`) — what a verify
    /// replay feeds as the reading before `ResponseSettled`/late arrivals.
    pub result_clock_ms: u64,
    pub superstep: u64,
}

/// One loaded checkpoint.
#[derive(Debug, Clone)]
pub struct CheckpointRow {
    pub superstep: u64,
    pub hash: Hash,
    pub scheduler: Value,
    pub decisions: DecisionRecord,
}

/// The loaded journal: entries keyed §5.1-style, checkpoints ascending.
#[derive(Debug, Default)]
pub struct JournalView {
    pub entries: BTreeMap<JournalKey, JournalEntry>,
    pub checkpoints: Vec<CheckpointRow>,
}

impl JournalView {
    /// Every dangling intent at or after `from_superstep` — Pending with no
    /// result, Host-side only (§5.3: Client asks are never re-dispatched).
    pub fn dangling(&self, from_superstep: u64) -> Vec<&JournalEntry> {
        self.entries
            .values()
            .filter(|e| e.result.is_none() && e.superstep >= from_superstep)
            .collect()
    }
}

/// Load the complete journal for a run — cursor-paginated, never the capped
/// `run_trace` (a resume reading a truncated journal re-executes effects it
/// can no longer see).
pub fn load(m: &mut Areev, ns: &str, run_id: &str) -> Result<JournalView> {
    let mut view = JournalView::default();
    let mut cursor = 0i64;
    loop {
        let page = m.run_grains(ns, run_id, cursor, 512)?;
        let exhausted = page.len() < 512;
        for (seq, g) in page {
            cursor = seq;
            ingest(&mut view, run_id, &g)?;
        }
        if exhausted {
            break;
        }
    }
    view.checkpoints.sort_by_key(|c| c.superstep);
    Ok(view)
}

fn ingest(
    view: &mut JournalView,
    run_id: &str,
    g: &areev_core::format::deserialize::DeserializedGrain,
) -> Result<()> {
    // Checkpoints: State grains stamped `checkpoint: true`.
    if g.get_bool("checkpoint") == Some(true) {
        let ss = g.get_u64(F_SUPERSTEP).unwrap_or(0);
        let ctx = g
            .fields
            .get("context")
            .or_else(|| g.fields.get("context_data"))
            .ok_or_else(|| {
                AreevError::Validation(format!(
                    "checkpoint {} carries no context",
                    g.hash.to_hex()
                ))
            })?;
        let sched = ctx.get("scheduler").cloned().ok_or_else(|| {
            AreevError::Validation(format!(
                "checkpoint {} carries no scheduler state",
                g.hash.to_hex()
            ))
        })?;
        let decisions: DecisionRecord = ctx
            .get("decisions")
            .cloned()
            .and_then(|d| serde_json::from_value(d).ok())
            .unwrap_or_default();
        view.checkpoints.push(CheckpointRow {
            superstep: ss,
            hash: g.hash,
            scheduler: sched,
            decisions,
        });
        return Ok(());
    }
    // Journal entries: Tool grains carrying the key fields.
    let (Some(node), Some(task_path)) = (g.get_str(F_NODE), g.get_str(F_TASK_PATH)) else {
        return Ok(()); // A run grain that is not a journal record (manifest link etc.)
    };
    let kind = match g.get_str(F_EFFECT_KIND) {
        Some("llm") => EffectKind::Llm,
        _ => EffectKind::Tool,
    };
    let key = JournalKey {
        run_id: run_id.to_string(),
        task_path: task_path.to_string(),
        node: node.to_string(),
        attempt: g.get_u64(F_ATTEMPT).unwrap_or(0) as u32,
        effect_seq: g.get_u64(F_EFFECT_SEQ).unwrap_or(0) as u32,
        kind,
    };
    let superstep = g.get_u64(F_SUPERSTEP).unwrap_or(0);
    let status = g.get_str("status").unwrap_or("completed");
    let entry = view.entries.entry(key.clone()).or_insert(JournalEntry {
        key,
        intent: g.hash,
        result: None,
        result_clock_ms: 0,
        superstep,
    });
    match status {
        "pending" => {
            entry.intent = g.hash;
            entry.superstep = superstep;
        }
        _ => {
            entry.result = Some((g.hash, outcome_from_grain(g, status)));
            entry.result_clock_ms = g.get_i64("created_at").unwrap_or(0).max(0) as u64;
        }
    }
    Ok(())
}

fn outcome_from_grain(
    g: &areev_core::format::deserialize::DeserializedGrain,
    status: &str,
) -> EffectOutcome {
    if status == "failed" {
        let cause = match g.get_str("failure_cause") {
            Some("timeout") => FailCause::Timeout,
            Some("executor_error") => FailCause::ExecutorError,
            Some("schema_validation_failed") => FailCause::SchemaValidationFailed,
            Some("user_aborted") => FailCause::UserAborted,
            _ => FailCause::Unknown,
        };
        EffectOutcome::Failed {
            cause,
            detail: g
                .get_str("failure_detail")
                .or_else(|| g.get_str("error"))
                .unwrap_or("")
                .to_string(),
            journal_bytes: g.get_u64(F_JOURNAL_BYTES).unwrap_or(0),
        }
    } else {
        // The Tool grain's `content` field holds the JSON-serialized result
        // value — and deserializes as `tool_content` (compact key `cnt`
        // expands to that, deliberately distinct from Event's uncompacted
        // `content`). Reading the wrong name here made every replayed
        // result Null, which the RUN-E009 field diff caught as "context:
        // stored has the merge, replayed doesn't".
        let result = g
            .get_str("tool_content")
            .or_else(|| g.get_str("content"))
            .and_then(|c| serde_json::from_str(c).ok())
            .unwrap_or(Value::Null);
        EffectOutcome::Completed {
            result,
            journal_bytes: g.get_u64(F_JOURNAL_BYTES).unwrap_or(0),
            input_tokens: g.get_u64(F_USAGE_IN).unwrap_or(0),
            output_tokens: g.get_u64(F_USAGE_OUT).unwrap_or(0),
            usd_micros: g.get_u64(F_USD_MICROS).unwrap_or(0),
        }
    }
}

/// The deterministic storage-accounting figure for an outcome: the byte
/// length of the payload the journal keeps (computed by the driver, stored
/// on the grain, read back identically on replay).
pub fn outcome_journal_bytes(outcome: &EffectOutcome) -> u64 {
    match outcome {
        EffectOutcome::Completed { result, .. } => result.to_string().len() as u64,
        EffectOutcome::Failed { detail, .. } => detail.len() as u64,
    }
}
