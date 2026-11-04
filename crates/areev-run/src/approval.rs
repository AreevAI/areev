//! The run-approval trail (#420) — one hash-chained record per `respond`
//! decision.
//!
//! #292 journaled every REJECTED response as an Observation in
//! `agent:harness`, and the accepted ones lived only as the superseding result
//! grain of each run's journal. Neither was chained, so `areev audit export`
//! could not carry them and a host anchoring one hash root of its evidence had
//! to walk every journal and re-implement the chaining the destruction trail
//! already does. Here every decision — accepted, declined (`--is-error`),
//! refused (missing grant, separation of duties, a repeated vote, an expired
//! or settled ask, a distinct-approver violation, a closed veto window), a
//! quorum vote (#424) and a veto (#416) — is ONE Observation in
//! `agent:harness`, appended to the memory's `approval` audit chain
//! (`Areev::append_audit_on`), carrying the run id so `run_grains` joins it to
//! its run.
//!
//! The records are also LOAD-BEARING, not only evidence: a quorum vote has no
//! other durable form until the ask settles, so the quorum count and the
//! distinct-approver check read them back ([`load_for_run`]).
//!
//! A refusal keeps the object text #292 wrote (`rejected response to ask …`),
//! so anything already matching on it keeps matching; no other outcome uses
//! that phrase.

use areev_core::authz::HARNESS_NS;
use areev_core::error::{Hash, Result};
use areev_core::format::deserialize::DeserializedGrain;
use areev_core::types::{Grain, Observation};
use areev_store::Areev;
use serde_json::{json, Value};

/// `context.audit` on every approval record — what `audit export` selects on.
pub const APPROVAL_AUDIT: &str = "approval";

/// The response was accepted and settled the ask.
pub const OUTCOME_ACCEPTED: &str = "accepted";
/// The response was accepted as a refusal (`--is-error`): the ask settled,
/// and the node fails.
pub const OUTCOME_DECLINED: &str = "declined";
/// `respond` itself refused the response; `refusal` says why.
pub const OUTCOME_REFUSED: &str = "refused";
/// A counted quorum approval that did not yet settle its ask (#424).
pub const OUTCOME_VOTE: &str = "vote";
/// A veto inside a wait's window (#416): the node fails as user-aborted.
pub const OUTCOME_VETO: &str = "veto";

/// One decision about one response.
pub struct ApprovalRecord<'a> {
    pub run_id: &'a str,
    pub ask: &'a str,
    pub node: Option<&'a str>,
    pub responder: &'a str,
    /// The run's principal and initiator — who the responder was judged
    /// against.
    pub principal: Option<&'a str>,
    pub initiator: Option<&'a str>,
    pub outcome: &'a str,
    /// Set exactly when `outcome` is [`OUTCOME_REFUSED`]: `missing_grant`,
    /// `unknown_ask`, `already_settled`, `expired`, `separation_of_duties`,
    /// `distinct_approver`, `duplicate_vote` or `window_closed`.
    pub refusal: Option<&'a str>,
    pub reason: &'a str,
    /// Outcome-specific detail (quorum progress, a wait's `due_at_ms`).
    pub extra: Option<Value>,
    pub at_ms: u64,
}

/// Append one record to the memory's approval chain.
pub fn append(m: &mut Areev, rec: &ApprovalRecord<'_>) -> Result<Hash> {
    let object = match rec.outcome {
        // The #292 phrasing, kept verbatim for refusals only.
        OUTCOME_REFUSED => format!("rejected response to ask {}: {}", rec.ask, rec.reason),
        other => format!("{other} response to ask {}", rec.ask),
    };
    let mut obs = Observation::new(rec.responder, "human")
        .subject(&format!("run:{}", rec.run_id))
        .object(&object)
        .namespace(HARNESS_NS)
        .created_at(rec.at_ms as i64);
    obs.common.extra_fields.insert("run_id".into(), json!(rec.run_id));
    let mut ctx = json!({
        "audit": APPROVAL_AUDIT,
        "outcome": rec.outcome,
        "ask": rec.ask,
        "responder": rec.responder,
        "reason": rec.reason,
    });
    let o = ctx.as_object_mut().expect("json! built an object");
    if let Some(r) = rec.refusal {
        o.insert("refusal".into(), json!(r));
    }
    if let Some(n) = rec.node {
        o.insert("node".into(), json!(n));
    }
    if let Some(p) = rec.principal {
        o.insert("principal".into(), json!(p));
    }
    if let Some(i) = rec.initiator {
        o.insert("initiator".into(), json!(i));
    }
    if let Some(Value::Object(extra)) = &rec.extra {
        for (k, v) in extra {
            o.entry(k.clone()).or_insert_with(|| v.clone());
        }
    }
    obs.common.context = Some(ctx);
    m.append_audit_on(Areev::AUDIT_CHAIN_APPROVAL, &mut obs)
}

/// The record's `context`, when `g` is an approval record.
pub fn context_of(g: &DeserializedGrain) -> Option<&Value> {
    let ctx = g.fields.get("context")?;
    (ctx.get("audit").and_then(|a| a.as_str()) == Some(APPROVAL_AUDIT)).then_some(ctx)
}

/// One record as a report row: the decision fields plus its address and
/// time. What `run inspect`, `run oversight-report` and `audit export` print.
pub fn row(g: &DeserializedGrain) -> Option<Value> {
    let ctx = context_of(g)?;
    let mut row = json!({
        "hash": g.hash.to_hex(),
        "at_ms": g.get_i64("created_at").unwrap_or(0),
    });
    let o = row.as_object_mut().expect("json! built an object");
    // The run id rides the grain itself — top-level, which is what indexes the
    // record to its run — not the context (`run_id` is a compact-field name).
    if let Some(r) = g.get_str("run_id") {
        o.insert("run_id".into(), json!(r));
    }
    for k in [
        "outcome", "refusal", "ask", "node", "responder", "principal", "initiator", "reason",
        "quorum", "due_at_ms",
    ] {
        if let Some(v) = ctx.get(k) {
            o.insert(k.into(), v.clone());
        }
    }
    Some(row)
}

/// Every approval record of one run, oldest first (an indexed read —
/// `run_grains` on `agent:harness` — paged to exhaustion).
pub fn load_for_run(m: &mut Areev, run_id: &str) -> Result<Vec<Value>> {
    const PAGE: usize = 512;
    let mut out = Vec::new();
    let mut after = 0i64;
    loop {
        let page = m.run_grains(HARNESS_NS, run_id, after, PAGE)?;
        let exhausted = page.len() < PAGE;
        for (seq, g) in page {
            after = seq;
            if let Some(r) = row(&g) {
                out.push(r);
            }
        }
        if exhausted {
            break;
        }
    }
    Ok(out)
}

/// The distinct principals whose quorum votes on `ask` are recorded, in vote
/// order.
pub fn voters(records: &[Value], ask: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for r in records {
        if r.get("ask").and_then(|a| a.as_str()) != Some(ask) {
            continue;
        }
        if r.get("outcome").and_then(|o| o.as_str()) != Some(OUTCOME_VOTE) {
            continue;
        }
        if let Some(who) = r.get("responder").and_then(|w| w.as_str()) {
            if !out.iter().any(|o| o == who) {
                out.push(who.to_string());
            }
        }
    }
    out
}
