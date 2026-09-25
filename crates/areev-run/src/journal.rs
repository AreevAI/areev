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

/// Where a CONTENT-BEARING harness record goes (#301).
///
/// Run evidence splits in two. Ids and counters — the manifest link, the
/// cancel Facts, the run-outcome census — stay in the memory-wide
/// `agent:harness` so `run list`, cancel and the lease paths are unchanged.
/// Records that carry CONTENT — the model's own summary of a transcript, an
/// outbound call's URL and request headers, a blob read — may instead go to
/// `agent:harness.<run_ns>` when the host asked for it.
///
/// The dotted CHILD of the harness namespace, deliberately, not the run's own
/// namespace: keeping them out of the agent's recall scope is why they are
/// not written there in the first place. `agent:harness.*` still reads
/// everything, so an operator surface loses nothing; what changes is that one
/// namespace's run evidence becomes separately grantable, retainable and
/// erasable, instead of `read ON agent:harness` disclosing every namespace's.
pub fn harness_ns_for(run_ns: Option<&str>) -> String {
    match run_ns {
        Some(ns) if !ns.is_empty() => format!("{HARNESS_NS}.{ns}"),
        _ => HARNESS_NS.to_string(),
    }
}
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
const P_RUN_INPUT: &str = "mg:run_input";
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
        NodeExecutor::Client { tool_hash, tool_name, .. } => {
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
        // A declared memory read journals under `mg:<op>` (`mg:entity_at`,
        // `mg:related`, `mg:recall`); what it read rides the result's `read`
        // field.
        NodeExecutor::MemoryRead { op, .. } => {
            (String::new(), format!("mg:{op}"), ExecutorKind::Host)
        }
        // A decision NODE (C3) journals under its own Definition's name and
        // hash — an ordinary Tool execution grain, so `run-trace`,
        // `step-actions` and the loop's `run_outcome` see it like any tool.
        // The scheduler's own asks (C1/C2) carry no hash and the reserved
        // `mg:decide` name.
        NodeExecutor::Decide { tool_hash, tool_name } => {
            (tool_hash.clone(), tool_name.clone(), ExecutorKind::Host)
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
    let mut t =
        result_tool(ns, run_id, plan_hash, key, executor, outcome, superstep, clock_ms, principal);
    m.supersede(intent, &mut t)
}

/// [`write_result`] plus, for a declared memory read (#255), what was read —
/// namespace, operands, axis and instant as RESOLVED, and the grain hash —
/// under `read`. The result content alone is the answer; the record is what
/// makes a determination made against it reproducible after the file has
/// moved on. `None` writes exactly what [`write_result`] does.
#[allow(clippy::too_many_arguments)]
pub fn write_result_with_read(
    m: &mut Areev,
    ns: &str,
    run_id: &str,
    plan_hash: &Hash,
    intent: &Hash,
    key: &JournalKey,
    executor: &NodeExecutor,
    outcome: &EffectOutcome,
    record: Option<&Value>,
    superstep: u64,
    clock_ms: u64,
    principal: &str,
) -> Result<Hash> {
    let mut t =
        result_tool(ns, run_id, plan_hash, key, executor, outcome, superstep, clock_ms, principal);
    if let Some(record) = record {
        t.common
            .extra_fields
            .insert(crate::memread::READ_RECORD_FIELD.into(), record.clone());
    }
    m.supersede(intent, &mut t)
}

#[allow(clippy::too_many_arguments)]
fn result_tool(
    ns: &str,
    run_id: &str,
    plan_hash: &Hash,
    key: &JournalKey,
    executor: &NodeExecutor,
    outcome: &EffectOutcome,
    superstep: u64,
    clock_ms: u64,
    principal: &str,
) -> Tool {
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
                FailCause::ContextOverflow => FailureCause::ContextOverflow,
            });
            t.failure_detail = Some(detail.clone());
        }
    }
    t
}

/// Record a transcript fold's SUMMARY as a Tier-2 Observation in the harness
/// namespace.
///
/// The summary already exists as the fold effect's result grain, but a journal
/// Tool grain is not reachable by recall: its payload lives in `tool_content`,
/// which the store's text projection does not index, and it carries no
/// subject/relation/object. So what an agent worked out over fifty turns is
/// stored and findable only if you already know the run.
///
/// An Observation fixes that without inventing a second copy of the truth.
/// It is typed, it is indexed, and it is EVIDENCE about the run rather than a
/// memory of the agent's — which is why it goes to `agent:harness` and never
/// to the agent's own namespace. Nothing here becomes a durable lesson on its
/// own: the loop reads these, an LLM may propose a lesson citing one, and the
/// four gates decide. Recording the summary verbatim as agent memory would
/// pollute it — a summary is working state ("round 4 outstanding"), not a
/// thing that is true tomorrow.
#[allow(clippy::too_many_arguments)]
pub fn write_fold_summary(
    m: &mut Areev,
    ns: &str,
    run_id: &str,
    key: &JournalKey,
    plan_hash: &Hash,
    summary: &str,
    clock_ms: u64,
    principal: &str,
    // The namespace this record's CONTENT belongs to (#301);
    // `None` keeps the memory-wide `agent:harness`.
    harness_ns: Option<&str>,
) -> Result<Hash> {
    let mut obs = Observation::new(principal, "agent")
        .subject(&format!("run:{run_id}"))
        .object(summary)
        .namespace(&harness_ns_for(harness_ns))
        .created_at(clock_ms as i64);
    let ex = &mut obs.common.extra_fields;
    ex.insert("run_id".into(), json!(run_id));
    ex.insert("observation_kind".into(), json!("fold_summary"));
    ex.insert("plan_hash".into(), json!(plan_hash.to_hex()));
    ex.insert("node".into(), json!(key.node));
    ex.insert("attempt".into(), json!(key.attempt));
    // Where the folded turns themselves are. The summary stands in for a
    // range of the transcript; this is how a reader gets back to the range.
    ex.insert("effect_seq".into(), json!(key.effect_seq));
    // The run's OWN namespace, so a reader can find the journal it came from
    // without guessing — the observation lives in the harness namespace, the
    // run does not.
    ex.insert("run_ns".into(), json!(ns));
    m.add(&obs)
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
    // The namespace this record's CONTENT belongs to (#301);
    // `None` keeps the memory-wide `agent:harness`.
    harness_ns: Option<&str>,
) -> Result<Hash> {
    let caller = if refusal.caller.is_empty() { "connector" } else { &refusal.caller };
    let mut obs = Observation::new(principal, "system")
        .subject(&format!("run:{run_id}"))
        .object(&refusal.destination)
        .namespace(&harness_ns_for(harness_ns))
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
    // The namespace this record's CONTENT belongs to (#301);
    // `None` keeps the memory-wide `agent:harness`.
    harness_ns: Option<&str>,
) -> Result<Hash> {
    let caller = if call.caller.is_empty() { "connector" } else { &call.caller };
    let mut obs = Observation::new(principal, "system")
        .subject(&format!("run:{run_id}"))
        .object(&call.url)
        .namespace(&harness_ns_for(harness_ns))
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
    if let Some(mime) = &call.response_mime {
        ex.insert("response_mime".into(), json!(mime));
    }
    if let Some(uri) = &call.response_ref {
        ex.insert("response_ref".into(), json!(uri));
        obs.common.content_refs.push(areev_core::types::ContentRef {
            uri: uri.clone(), modality: None, mime_type: call.response_mime.clone(),
            size_bytes: Some(call.response_bytes as u64),
            checksum: Some(call.response_digest.clone()), metadata: None,
        });
    }
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

/// Record one CAS blob a capability tool READ, as a Tier-2 Observation (#106).
///
/// The other half of the mediated-I/O trail. `write_egress_call` records what
/// a module sent; this records what it opened — and for the tool this
/// capability exists for, parsing an attachment that arrived from outside,
/// what it opened is the more interesting question. "Which bytes did the thing
/// that processes untrusted input actually process" is the first thing an
/// incident asks.
///
/// The `cas://` address IS the content hash, so naming it names exactly the
/// bytes without putting a mailbox attachment into an immutable, replicating
/// grain — the same reason bodies are recorded as digests.
///
/// Deliberately NOT a journal entry, exactly like its two siblings: replay
/// never sees it, so `verify` stays byte-identical whether or not a broker was
/// configured. Evidence about the run, not a step of it.
pub fn write_blob_read(
    m: &mut Areev,
    run_id: &str,
    read: &crate::broker::BlobRead,
    clock_ms: u64,
    principal: &str,
    // The namespace this record's CONTENT belongs to (#301);
    // `None` keeps the memory-wide `agent:harness`.
    harness_ns: Option<&str>,
) -> Result<Hash> {
    let caller = if read.caller.is_empty() { "connector" } else { &read.caller };
    let mut obs = Observation::new(principal, "system")
        .subject(&format!("run:{run_id}"))
        .object(&read.uri)
        .namespace(&harness_ns_for(harness_ns))
        .created_at(clock_ms as i64);
    let ex = &mut obs.common.extra_fields;
    ex.insert("run_id".into(), json!(run_id));
    ex.insert("observation_kind".into(), json!("blob_read"));
    ex.insert("caller".into(), json!(caller));
    ex.insert("blob".into(), json!(read.uri));
    ex.insert("bytes".into(), json!(read.bytes));
    m.add(&obs)
}

/// Write one steering message: a Fact on the run, in the run's own
/// namespace so it rides the run index the journal reads.
pub fn write_input(
    m: &mut Areev,
    ns: &str,
    run_id: &str,
    message: &str,
    clock_ms: u64,
    principal: &str,
) -> Result<Hash> {
    let mut f = areev_core::types::Fact::new(&format!("run:{run_id}"), P_RUN_INPUT, message)
        .namespace(ns)
        .created_at(clock_ms as i64);
    f.common.author_did = Some(principal.to_string());
    f.common.extra_fields.insert("run_id".into(), json!(run_id));
    m.add(&f)
}

/// Steering messages written since `cursor`, with the cursor advanced —
/// what a live driver polls at each wave boundary.
pub fn poll_inputs(
    m: &mut Areev,
    ns: &str,
    run_id: &str,
    cursor: i64,
) -> Result<(Vec<Value>, i64)> {
    poll_run(m, ns, run_id, cursor).map(|(inputs, _, at)| (inputs, at))
}

/// Steering messages AND pause records written since `cursor`, with the
/// cursor advanced — one forward read of the run index per wave boundary,
/// for both of the run's in-band control channels.
pub fn poll_run(
    m: &mut Areev,
    ns: &str,
    run_id: &str,
    cursor: i64,
) -> Result<(Vec<Value>, Vec<PauseEvent>, i64)> {
    // The cursor advances only on success: a page read that fails midway
    // would otherwise leave it past messages this call is dropping, and the
    // next poll would never see them again.
    let mut out = Vec::new();
    let mut pauses = Vec::new();
    let mut at = cursor;
    loop {
        let page = m.run_grains(ns, run_id, at, 512)?;
        let exhausted = page.len() < 512;
        for (seq, g) in page {
            at = seq;
            if g.get_str("relation") == Some(P_RUN_INPUT) {
                if let Some(msg) = g.fields.get("object") {
                    out.push(msg.clone());
                }
            } else if let Some(ev) = pause_event(&g) {
                pauses.push(ev);
            }
        }
        if exhausted {
            return Ok((out, pauses, at));
        }
    }
}

// ---- host pause (#344) -----------------------------------------------------
//
// Three Facts in the RUN's namespace, indexed by `run_id` exactly like a
// steering message, so every reader that already walks the run index —
// `load` (resume, inspect, verify) and the live driver's per-wave poll — sees
// them in op-log order at no extra read. Order is the point: a cancel Fact is
// read with `latest`, which ranks by `created_at` and then hash, and two
// pause cycles inside one clock millisecond would rank arbitrarily.
//
// - `mg:run_pause`    the REQUEST: object = the reason, author = who asked.
// - `mg:run_paused`   the driver HONOURED it: object = the request's hash,
//                     `superstep` = the boundary it parked after.
// - `mg:run_unpause`  a resume CONSUMED it: object = the request's hash,
//                     author = who resumed. A consumed request never
//                     re-pauses the run, however stale.
//
// None of them is a journal entry or touches scheduler state, so `verify`
// is byte-identical with or without them.
pub const P_RUN_PAUSE: &str = "mg:run_pause";
pub const P_RUN_PAUSED: &str = "mg:run_paused";
pub const P_RUN_UNPAUSE: &str = "mg:run_unpause";

/// A standing pause request.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct PauseRequest {
    /// The request Fact's hash — what `mg:run_paused` / `mg:run_unpause` name.
    pub request: String,
    pub paused_by: String,
    pub because: String,
    pub requested_at: i64,
}

/// One pause record, as read off the run index.
#[derive(Debug, Clone, PartialEq)]
pub enum PauseEvent {
    Requested(PauseRequest),
    Applied { request: String, superstep: u64, at: i64 },
    Released { request: String, by: String, at: i64 },
}

/// The run's pause state, folded from its pause records in op-log order.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PauseLog {
    /// Requests ever written — the next one's ordinal, which keeps two
    /// otherwise-identical requests from collapsing into one address.
    pub requests: u64,
    /// The request no resume has consumed yet.
    pub active: Option<PauseRequest>,
    /// Set once a driver honoured `active`: `(superstep, at)`.
    pub applied: Option<(u64, i64)>,
}

impl PauseLog {
    pub fn apply(&mut self, ev: &PauseEvent) {
        match ev {
            PauseEvent::Requested(r) => {
                self.requests += 1;
                self.active = Some(r.clone());
                self.applied = None;
            }
            PauseEvent::Applied { request, superstep, at } => {
                if self.active.as_ref().is_some_and(|a| &a.request == request) {
                    self.applied = Some((*superstep, *at));
                }
            }
            PauseEvent::Released { request, .. } => {
                if self.active.as_ref().is_some_and(|a| &a.request == request) {
                    self.active = None;
                    self.applied = None;
                }
            }
        }
    }

    /// Parked on the standing request (honoured, not yet consumed).
    pub fn is_paused(&self) -> bool {
        self.active.is_some() && self.applied.is_some()
    }
}

fn pause_event(g: &areev_core::format::deserialize::DeserializedGrain) -> Option<PauseEvent> {
    let at = g.get_i64("created_at").unwrap_or(0);
    match g.get_str("relation")? {
        P_RUN_PAUSE => Some(PauseEvent::Requested(PauseRequest {
            request: g.hash.to_hex(),
            paused_by: g.get_str("author_did").unwrap_or("unknown").to_string(),
            because: g.get_str("object").unwrap_or("").to_string(),
            requested_at: at,
        })),
        P_RUN_PAUSED => Some(PauseEvent::Applied {
            request: g.get_str("object")?.to_string(),
            superstep: g.get_u64(F_SUPERSTEP).unwrap_or(0),
            at,
        }),
        P_RUN_UNPAUSE => Some(PauseEvent::Released {
            request: g.get_str("object")?.to_string(),
            by: g.get_str("author_did").unwrap_or("unknown").to_string(),
            at,
        }),
        _ => None,
    }
}

fn pause_fact(
    ns: &str,
    run_id: &str,
    relation: &str,
    object: &str,
    clock_ms: u64,
    principal: &str,
) -> areev_core::types::Fact {
    let mut f = areev_core::types::Fact::new(&format!("run:{run_id}"), relation, object)
        .namespace(ns)
        .created_at(clock_ms as i64);
    f.common.author_did = Some(principal.to_string());
    f.common.extra_fields.insert("run_id".into(), json!(run_id));
    f
}

/// Write a pause REQUEST. `ordinal` is the request's position in the run's
/// pause history, so a second request with the same reason, principal and
/// millisecond is still a second grain.
pub fn write_pause_request(
    m: &mut Areev,
    ns: &str,
    run_id: &str,
    because: &str,
    ordinal: u64,
    clock_ms: u64,
    principal: &str,
) -> Result<Hash> {
    let mut f = pause_fact(ns, run_id, P_RUN_PAUSE, because, clock_ms, principal);
    f.common.extra_fields.insert("pause_ordinal".into(), json!(ordinal));
    m.add(&f)
}

/// Record that the driver honoured `request`, parking after `superstep`.
pub fn write_pause_applied(
    m: &mut Areev,
    ns: &str,
    run_id: &str,
    request: &str,
    superstep: u64,
    clock_ms: u64,
    principal: &str,
) -> Result<Hash> {
    let mut f = pause_fact(ns, run_id, P_RUN_PAUSED, request, clock_ms, principal);
    f.common.extra_fields.insert(F_SUPERSTEP.into(), json!(superstep));
    m.add(&f)
}

/// Record that a resume consumed `request`.
pub fn write_pause_released(
    m: &mut Areev,
    ns: &str,
    run_id: &str,
    request: &str,
    clock_ms: u64,
    principal: &str,
) -> Result<Hash> {
    m.add(&pause_fact(ns, run_id, P_RUN_UNPAUSE, request, clock_ms, principal))
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
    /// Steering messages in journal order — the sequence `inputs_seen`
    /// counts against.
    pub inputs: Vec<Value>,
    /// The run-index cursor this view was read to, so a live driver can
    /// poll forward for new steering messages without reloading.
    pub cursor: i64,
    /// The host-pause state (#344), folded from the run's pause records.
    pub pause: PauseLog,
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
    view.cursor = cursor;
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
    if g.get_str("relation") == Some(P_RUN_INPUT) {
        if let Some(msg) = g.fields.get("object") {
            view.inputs.push(msg.clone());
        }
        return Ok(());
    }
    if let Some(ev) = pause_event(g) {
        view.pause.apply(&ev);
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
        // Every cause round-trips through the grain's own vocabulary now, which
        // is what makes REPLAY see what the live run saw. When `context_overflow`
        // was flattened to `executor_error` on the way in, a resumed or verified
        // run read back a plain executor error, retried the identical prompt
        // instead of folding, and diverged.
        let cause = match g.get_str("failure_cause") {
            Some("timeout") => FailCause::Timeout,
            Some("executor_error") => FailCause::ExecutorError,
            Some("schema_validation_failed") => FailCause::SchemaValidationFailed,
            Some("user_aborted") => FailCause::UserAborted,
            Some("context_overflow") => FailCause::ContextOverflow,
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
