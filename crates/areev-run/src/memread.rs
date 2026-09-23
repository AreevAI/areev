//! Declared reads of the run's own memory (#255).
//!
//! A tool inside a run must never open the memory its run is holding: on the
//! embedded tier the file lock refuses it, and on every tier a tool that holds
//! a handle is a tool that can read anything. Until this existed, an as-of read
//! (`entity_at`) or a graph walk (`related`) could only happen in the DRIVER,
//! before `run start`, pinned into the input — which a host that only calls
//! `run start`/`resume` (a queue worker) has no place to do.
//!
//! So the PLAN declares the read, and the runtime answers it:
//!
//! ```json
//! "reads": {
//!   "cover_at_loss": {"op": "entity_at", "ns": "org.uw.policies",
//!                     "subject_from": "/policy_id", "relation": "mg:coverage_limit",
//!                     "at_from": "/date_of_loss", "axis": "world"}
//! }
//! ```
//!
//! The node `cover_at_loss` resolves to a `memory` executor instead of a tool.
//! At dispatch the driver resolves the `*_from` JSON pointers against the
//! node's input, performs the read on the store it already holds, and merges
//! `{into: <result>}` into state — the result byte-identical to what
//! `db.entity_at(...)` / `db.related(...)` return on the bindings. It is an
//! ordinary journaled effect: an intent before, a result after carrying the
//! resolved parameters and the grain hash under `read`, so `verify` and
//! `shadow` answer it from the journal and never re-read a file that has moved
//! on since.
//!
//! A third op, `recall` (#342), answers "what do I hold about this subject"
//! — the bindings' `db.recall(subject, relation, k, ns)` — with `k` capped at
//! [`MAX_K`] on the plan and enforced on the result, and an optional instant
//! that makes it an as-of recall (`Areev::recall_at`, one `entity_at` answer
//! per relation). Naming a saved query was considered and declined: see
//! ARCHITECTURE.md §10, "In-run recall is a third typed read; saved queries
//! are not".
//!
//! What it deliberately is not:
//! - **not a tool's capability.** Nothing a `--tool-cmd`, a native blob or a
//!   `wasm32-areev-io` module can call reaches it; the pool refuses a memory
//!   read outright rather than hand it to a host executor, and an abstract
//!   node is never offered one.
//! - **not a way out of the run's namespace.** The target is the run's own
//!   namespace or a dotted descendant of it, fixed on the plan, and the
//!   session must hold `read` there.
//! - **not a query language.** Three typed operations whose every operand is
//!   on the plan; `relation`, `axis`, `ns`, `k` and the walk's shape are
//!   literals a reviewer can read, and only the subject, the start and the
//!   instant may come from state. No free text, no predicate.

use areev_cal::AreevFacade;
use areev_run_core::{EffectOutcome, FailCause, PlanGraph, RunError};
use serde_json::{json, Map, Value};
use std::collections::BTreeMap;

/// The Workflow grain's extra field carrying the declarations.
pub const READS_FIELD: &str = "reads";

/// The manifest's executor name for a declared read.
pub const MEMORY_EXECUTOR: &str = "memory";

/// The extra field on a read's RESULT grain naming what was read.
pub const READ_RECORD_FIELD: &str = "read";

const OPS: [&str; 3] = ["entity_at", "related", "recall"];
const COMMON_KEYS: [&str; 3] = ["op", "ns", "into"];
const ENTITY_AT_KEYS: [&str; 6] = [
    "subject",
    "subject_from",
    "relation",
    "at",
    "at_from",
    "axis",
];
const RELATED_KEYS: [&str; 6] = [
    "start",
    "start_from",
    "relations",
    "direction",
    "depth",
    "limit",
];
const RECALL_KEYS: [&str; 7] = [
    "subject",
    "subject_from",
    "relation",
    "k",
    "at",
    "at_from",
    "axis",
];

/// `related`'s bounds, refused rather than clamped: the store clamps silently,
/// and a declaration that asks for depth 9 should learn it gets 4.
const MAX_DEPTH: u64 = 4;
const MAX_LIMIT: u64 = 512;
const DEFAULT_DEPTH: u64 = 2;
const DEFAULT_LIMIT: u64 = 64;

/// `recall`'s ceiling (#342): a plan may not ask for more, and the executed
/// read never returns more than the plan's `k`. The default is the MCP
/// `areev_recall` default.
pub const MAX_K: u64 = 64;
const DEFAULT_K: u64 = 16;

/// Is `ns` the run's own namespace or a dotted descendant of it?
pub fn in_run_scope(run_ns: &str, ns: &str) -> bool {
    ns == run_ns || ns.strip_prefix(run_ns).is_some_and(|r| r.starts_with('.'))
}

/// Parse and normalize a Workflow grain's `reads` field against its plan.
///
/// Returns node id → normalized spec. Every refusal happens here, at run
/// start, and names the node: a malformed declaration is `RUN-E019`, a target
/// outside the run's namespace `RUN-E012`. Unknown keys are refused too — an
/// `axsi: "knowledge"` that silently read the world axis would be the worst
/// failure this feature could have, a wrong answer with no error.
pub fn parse_reads(
    reads: Option<&Value>,
    plan: &PlanGraph,
    run_ns: &str,
) -> Result<BTreeMap<String, Value>, RunError> {
    let invalid = |why: String| RunError::InvalidPlan { why };
    let table = match reads {
        None | Some(Value::Null) => return Ok(BTreeMap::new()),
        Some(Value::Object(t)) => t,
        Some(_) => {
            return Err(invalid(format!(
                "`{READS_FIELD}` must be an object of node → read"
            )))
        }
    };
    let mut out = BTreeMap::new();
    for (node, raw) in table {
        let Some(i) = plan.nodes.iter().position(|n| n == node) else {
            return Err(invalid(format!(
                "`{READS_FIELD}` names unknown node '{node}'"
            )));
        };
        if plan.bindings[i].is_some() {
            return Err(invalid(format!(
                "node '{node}' both binds {} and declares a read — a read is answered by \
                 the runtime, so its node binds nothing",
                plan.bindings[i].as_deref().unwrap_or_default()
            )));
        }
        out.insert(node.clone(), normalize(node, raw, run_ns)?);
    }
    Ok(out)
}

fn normalize(node: &str, raw: &Value, run_ns: &str) -> Result<Value, RunError> {
    let invalid = |why: String| RunError::InvalidPlan {
        why: format!("read '{node}': {why}"),
    };
    let Some(obj) = raw.as_object() else {
        return Err(invalid("must be an object".into()));
    };
    let op = match obj.get("op").and_then(Value::as_str) {
        Some(op) if OPS.contains(&op) => op,
        Some(op) => {
            return Err(invalid(format!(
                "unknown op {op:?} (accepted: entity_at, related, recall)"
            )))
        }
        None => {
            return Err(invalid(
                "names no `op` (entity_at | related | recall)".into(),
            ))
        }
    };
    let op_keys: &[&str] = match op {
        "entity_at" => &ENTITY_AT_KEYS,
        "related" => &RELATED_KEYS,
        _ => &RECALL_KEYS,
    };
    if let Some(k) = obj
        .keys()
        .find(|k| !COMMON_KEYS.contains(&k.as_str()) && !op_keys.contains(&k.as_str()))
    {
        return Err(invalid(format!("unknown key `{k}` for op {op}")));
    }

    let mut spec = Map::new();
    spec.insert("op".into(), json!(op));
    let ns = match obj.get("ns") {
        None => run_ns.to_string(),
        Some(Value::String(s)) if !s.is_empty() => s.clone(),
        Some(_) => return Err(invalid("`ns` must be a non-empty string".into())),
    };
    // A literal namespace, never an `"org.*"` scope: a read names exactly the
    // namespace a reviewer approves and whose grant is checked (`recall` is
    // the one op whose store call would otherwise accept a pattern).
    if areev_core::ns::NsScope::is_pattern(&ns) {
        return Err(invalid(format!(
            "`ns` must be an exact namespace, not a pattern ({ns:?})"
        )));
    }
    if !in_run_scope(run_ns, &ns) {
        return Err(RunError::Unauthorized {
            what: format!(
                "read '{node}' targets namespace '{ns}', outside this run's namespace \
                 '{run_ns}' — a declared read sees the run's own namespace and its dotted \
                 descendants only"
            ),
        });
    }
    spec.insert("ns".into(), json!(ns));
    let into = match obj.get("into") {
        None => node.to_string(),
        Some(Value::String(s)) if !s.is_empty() && !s.starts_with('$') => s.clone(),
        Some(_) => {
            return Err(invalid(
                "`into` must be a non-empty state key not starting with `$` (reserved)".into(),
            ))
        }
    };
    spec.insert("into".into(), json!(into));

    if op == "entity_at" {
        operand(obj, "subject", &mut spec).map_err(invalid)?;
        spec.insert(
            "relation".into(),
            json!(required_str(obj, "relation").map_err(invalid)?),
        );
        if !instant(obj, &mut spec).map_err(invalid)? {
            return Err(invalid("needs exactly one of `at` / `at_from`".into()));
        }
    } else if op == "recall" {
        operand(obj, "subject", &mut spec).map_err(invalid)?;
        if obj.contains_key("relation") {
            spec.insert(
                "relation".into(),
                json!(required_str(obj, "relation").map_err(invalid)?),
            );
        }
        // Defaulted like `related`'s `limit`, and refused past the ceiling
        // rather than clamped: a plan asking for 65 learns it gets 64.
        spec.insert(
            "k".into(),
            json!(bounded(obj, "k", DEFAULT_K, MAX_K).map_err(invalid)?),
        );
        // The instant is optional — without one this is a current recall —
        // but an `axis` with no instant would be a knob that does nothing.
        if !instant(obj, &mut spec).map_err(invalid)? && obj.contains_key("axis") {
            return Err(invalid(
                "`axis` needs an instant (`at` / `at_from`); a recall without one reads \
                 what is current"
                    .into(),
            ));
        }
    } else {
        operand(obj, "start", &mut spec).map_err(invalid)?;
        let relations: Vec<String> = match obj.get("relations") {
            Some(Value::String(csv)) => areev_store::parse_relations(csv),
            Some(Value::Array(a)) => {
                let mut v = Vec::with_capacity(a.len());
                for r in a {
                    match r.as_str().map(str::trim) {
                        Some(s) if !s.is_empty() => v.push(s.to_string()),
                        _ => {
                            return Err(invalid(
                                "`relations` entries must be non-empty strings".into(),
                            ))
                        }
                    }
                }
                v
            }
            _ => Vec::new(),
        };
        if relations.is_empty() {
            return Err(invalid(
                "`relations` must name at least one relation".into(),
            ));
        }
        spec.insert("relations".into(), json!(relations));
        let direction = match obj.get("direction") {
            None => "out",
            Some(Value::String(d)) if ["out", "in", "both"].contains(&d.as_str()) => d.as_str(),
            Some(other) => {
                return Err(invalid(format!(
                    "`direction` must be out, in or both, not {other}"
                )))
            }
        };
        spec.insert("direction".into(), json!(direction));
        spec.insert(
            "depth".into(),
            json!(bounded(obj, "depth", DEFAULT_DEPTH, MAX_DEPTH).map_err(invalid)?),
        );
        spec.insert(
            "limit".into(),
            json!(bounded(obj, "limit", DEFAULT_LIMIT, MAX_LIMIT).map_err(invalid)?),
        );
    }
    Ok(Value::Object(spec))
}

/// The instant and its axis, as `entity_at` reads them: at most one of `at`
/// (normalized to epoch ms) / `at_from` (a pointer), and `axis` defaulting to
/// world. Returns whether an instant was given; an absent one inserts nothing,
/// and the caller decides whether that is allowed.
fn instant(obj: &Map<String, Value>, spec: &mut Map<String, Value>) -> Result<bool, String> {
    match (obj.get("at"), obj.get("at_from")) {
        (None, None) => return Ok(false),
        (Some(at), None) => {
            let ms = at_ms(at).map_err(|why| format!("`at`: {why}"))?;
            spec.insert("at".into(), json!(ms));
        }
        (None, Some(_)) => {
            spec.insert("at_from".into(), json!(pointer(obj, "at_from")?));
        }
        _ => return Err("needs exactly one of `at` / `at_from`".into()),
    }
    let axis = match obj.get("axis") {
        None => "world",
        Some(Value::String(a)) if a == "world" || a == "knowledge" => a.as_str(),
        Some(other) => {
            return Err(format!(
                "`axis` must be \"world\" or \"knowledge\", not {other}"
            ))
        }
    };
    spec.insert("axis".into(), json!(axis));
    Ok(true)
}

/// `name` as a literal string, or `name_from` as a JSON pointer — exactly one.
fn operand(
    obj: &Map<String, Value>,
    name: &str,
    spec: &mut Map<String, Value>,
) -> Result<(), String> {
    let from = format!("{name}_from");
    match (obj.contains_key(name), obj.contains_key(&from)) {
        (true, false) => {
            spec.insert(name.into(), json!(required_str(obj, name)?));
        }
        (false, true) => {
            spec.insert(from.clone(), json!(pointer(obj, &from)?));
        }
        _ => return Err(format!("needs exactly one of `{name}` / `{from}`")),
    }
    Ok(())
}

fn required_str(obj: &Map<String, Value>, key: &str) -> Result<String, String> {
    match obj.get(key) {
        Some(Value::String(s)) if !s.is_empty() => Ok(s.clone()),
        _ => Err(format!("`{key}` must be a non-empty string")),
    }
}

fn pointer(obj: &Map<String, Value>, key: &str) -> Result<String, String> {
    match obj.get(key) {
        Some(Value::String(p)) if p.starts_with('/') => Ok(p.clone()),
        _ => Err(format!("`{key}` must be a JSON pointer starting with '/'")),
    }
}

fn bounded(obj: &Map<String, Value>, key: &str, default: u64, max: u64) -> Result<u64, String> {
    match obj.get(key) {
        None => Ok(default),
        Some(v) => match v.as_u64() {
            Some(n) if (1..=max).contains(&n) => Ok(n),
            _ => Err(format!("`{key}` must be an integer from 1 to {max}")),
        },
    }
}

/// An instant on either clock: epoch milliseconds, or an ISO-8601 string
/// (`2026-03-18`, `2026-03-18T09:00:00Z`) read as UTC.
fn at_ms(v: &Value) -> Result<i64, String> {
    match v {
        Value::Number(n) => n
            .as_i64()
            .ok_or_else(|| format!("{n} is not an integer of epoch milliseconds")),
        Value::String(s) => areev_core::time::iso8601_to_ms(s)
            .ok_or_else(|| format!("{s:?} is not an ISO-8601 date or timestamp")),
        other => Err(format!(
            "must be epoch milliseconds or an ISO-8601 string, found {other}"
        )),
    }
}

/// A literal operand, or the value its `*_from` pointer lands on in `input`.
fn resolve<'a>(spec: &'a Value, input: &'a Value, name: &str) -> Result<&'a Value, String> {
    if let Some(v) = spec.get(name) {
        return Ok(v);
    }
    let from = format!("{name}_from");
    let ptr = spec
        .get(&from)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("the declaration names neither `{name}` nor `{from}`"))?;
    input
        .pointer(ptr)
        .ok_or_else(|| format!("`{from}` {ptr} does not resolve against the node's input"))
}

fn resolve_str(spec: &Value, input: &Value, name: &str) -> Result<String, String> {
    match resolve(spec, input, name)? {
        Value::String(s) if !s.is_empty() => Ok(s.clone()),
        other => Err(format!(
            "`{name}` must resolve to a non-empty string, found {other}"
        )),
    }
}

/// What executing one declared read produced: the effect outcome the
/// scheduler merges, and the record journaled beside it on the result grain.
pub struct ReadResult {
    pub outcome: EffectOutcome,
    pub record: Option<Value>,
}

fn failed(cause: FailCause, detail: String) -> ReadResult {
    ReadResult {
        outcome: EffectOutcome::Failed {
            journal_bytes: detail.len() as u64,
            cause,
            detail,
        },
        record: None,
    }
}

/// Perform one declared read against the store the driver holds.
///
/// Refusals are FAILED EFFECTS, never panics and never a silent empty answer:
/// a pointer that does not resolve fails the node (`schema_validation_failed`,
/// not retried — the same input would fail the same way), a denied grant fails
/// it (`unknown`, not retried), and a store error is `executor_error`, which
/// the node's `retries` may retry. An honest miss — no grain on that axis at
/// that instant — is a COMPLETED read of `{"found": false}`.
pub fn execute(facade: &AreevFacade, run_ns: &str, spec: &Value, input: &Value) -> ReadResult {
    let op = spec.get("op").and_then(Value::as_str).unwrap_or_default();
    let (Some(ns), Some(into)) = (
        spec.get("ns").and_then(Value::as_str),
        spec.get("into").and_then(Value::as_str),
    ) else {
        return failed(
            FailCause::Unknown,
            "memory read: the pinned declaration is malformed".into(),
        );
    };
    // Re-checked here, not only at start: the manifest is replicated data, and
    // the run may be resumed by a different session than the one that began it.
    if !in_run_scope(run_ns, ns) || areev_core::ns::NsScope::is_pattern(ns) {
        return failed(
            FailCause::Unknown,
            format!("memory read of '{ns}' refused: outside this run's namespace '{run_ns}'"),
        );
    }
    if let Err(e) = facade.authz().check(areev_core::authz::Verb::Read, ns) {
        return failed(
            FailCause::Unknown,
            format!("memory read of '{ns}' refused: {e}"),
        );
    }
    let schema = |why: String| {
        failed(
            FailCause::SchemaValidationFailed,
            format!("memory read: {why}"),
        )
    };
    let (payload, record) = match op {
        "entity_at" => {
            let subject = match resolve_str(spec, input, "subject") {
                Ok(s) => s,
                Err(why) => return schema(why),
            };
            let at = match resolve(spec, input, "at").and_then(at_ms) {
                Ok(at) => at,
                Err(why) => return schema(format!("`at`: {why}")),
            };
            let relation = spec
                .get("relation")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let axis_name = spec.get("axis").and_then(Value::as_str).unwrap_or("world");
            let Some(axis) = areev_store::Axis::parse(axis_name) else {
                return schema(format!("unknown axis {axis_name:?}"));
            };
            let found = match facade.with_store(|m| m.entity_at(ns, &subject, relation, at, axis)) {
                Ok(found) => found,
                Err(e) => return failed(FailCause::ExecutorError, format!("memory read: {e}")),
            };
            let grain = found.as_ref().map(|g| g.hash.to_hex());
            // The bindings' exact shape (`db.entity_at`), so a plan node reads
            // what a driver would have pinned.
            let payload = match found {
                Some(g) => json!({"found": true, "grain": g}),
                None => json!({"found": false}),
            };
            let record = json!({
                "op": op, "ns": ns, "subject": subject, "relation": relation,
                "at": at, "axis": axis_name, "grain": grain,
            });
            (payload, record)
        }
        "related" => {
            let start = match resolve_str(spec, input, "start") {
                Ok(s) => s,
                Err(why) => return schema(why),
            };
            let relations: Vec<String> = spec
                .get("relations")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default();
            let direction_name = spec
                .get("direction")
                .and_then(Value::as_str)
                .unwrap_or("out");
            let Some(direction) = areev_store::Direction::parse(direction_name) else {
                return schema(format!("unknown direction {direction_name:?}"));
            };
            let depth = spec
                .get("depth")
                .and_then(Value::as_u64)
                .unwrap_or(DEFAULT_DEPTH) as usize;
            let limit = spec
                .get("limit")
                .and_then(Value::as_u64)
                .unwrap_or(DEFAULT_LIMIT) as usize;
            let refs: Vec<&str> = relations.iter().map(String::as_str).collect();
            let reached = match facade
                .with_store(|m| m.related(ns, &start, &refs, direction, depth, limit))
            {
                Ok(r) => r,
                Err(e) => return failed(FailCause::ExecutorError, format!("memory read: {e}")),
            };
            let record = json!({
                "op": op, "ns": ns, "start": start, "relations": relations,
                "direction": direction_name, "depth": depth, "limit": limit,
                "reached": reached.len(),
            });
            // `db.related`'s exact shape.
            (json!({"start": start, "reached": reached}), record)
        }
        "recall" => {
            let subject = match resolve_str(spec, input, "subject") {
                Ok(s) => s,
                Err(why) => return schema(why),
            };
            let relation = spec.get("relation").and_then(Value::as_str);
            // The ceiling is the runtime's, not the declaration's word for it:
            // a pinned `k` past MAX_K (a hand-edited manifest) is refused, and
            // the answer is truncated to `k` whatever the store returned.
            let k = match spec.get("k").and_then(Value::as_u64) {
                Some(k) if (1..=MAX_K).contains(&k) => k as usize,
                other => {
                    return failed(
                        FailCause::Unknown,
                        format!("memory read: pinned `k` {other:?} is outside 1..={MAX_K}"),
                    )
                }
            };
            let as_of = if spec.get("at").is_some() || spec.get("at_from").is_some() {
                let at = match resolve(spec, input, "at").and_then(at_ms) {
                    Ok(at) => at,
                    Err(why) => return schema(format!("`at`: {why}")),
                };
                let axis_name = spec.get("axis").and_then(Value::as_str).unwrap_or("world");
                let Some(axis) = areev_store::Axis::parse(axis_name) else {
                    return schema(format!("unknown axis {axis_name:?}"));
                };
                Some((at, axis_name, axis))
            } else {
                None
            };
            let grains = match facade.with_store(|m| match as_of {
                Some((at, _, axis)) => m.recall_at(ns, &subject, relation, k, at, axis),
                None => m.recall(ns, &subject, relation, k),
            }) {
                Ok(mut g) => {
                    g.truncate(k);
                    g
                }
                Err(e) => return failed(FailCause::ExecutorError, format!("memory read: {e}")),
            };
            let hashes: Vec<String> = grains.iter().map(|g| g.hash.to_hex()).collect();
            let mut record = json!({
                "op": op, "ns": ns, "subject": subject, "relation": relation,
                "k": k, "grains": hashes,
            });
            if let Some((at, axis_name, _)) = as_of {
                record["at"] = json!(at);
                record["axis"] = json!(axis_name);
            }
            // `db.recall`'s exact shape, built as both bindings build it.
            let payload: Vec<Value> = grains
                .iter()
                .map(|g| {
                    json!({
                        "hash": g.hash.to_hex(),
                        "type": format!("{:?}", g.grain_type).to_lowercase(),
                        "fields": g.fields,
                    })
                })
                .collect();
            (Value::Array(payload), record)
        }
        other => {
            return failed(
                FailCause::Unknown,
                format!("memory read: unknown op {other:?}"),
            )
        }
    };
    let result = json!({ into: payload });
    ReadResult {
        outcome: EffectOutcome::Completed {
            journal_bytes: result.to_string().len() as u64,
            result,
            input_tokens: 0,
            output_tokens: 0,
            usd_micros: 0,
        },
        record: Some(record),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plan(nodes: &[&str]) -> PlanGraph {
        let mut wf =
            areev_core::types::Workflow::new(nodes.iter().map(|s| s.to_string()).collect());
        for w in nodes.windows(2) {
            wf = wf.edge(w[0], w[1]);
        }
        PlanGraph::build(&wf).unwrap()
    }

    #[test]
    fn scope_is_the_run_namespace_and_its_dotted_descendants() {
        assert!(in_run_scope("org.uw", "org.uw"));
        assert!(in_run_scope("org.uw", "org.uw.policies"));
        assert!(
            !in_run_scope("org.uw", "org.uwx"),
            "a shared prefix is not a descendant"
        );
        assert!(!in_run_scope("org.uw", "org"));
        assert!(!in_run_scope("org.uw", "agent:authz"));
    }

    #[test]
    fn a_declaration_normalizes_with_every_default_filled() {
        let p = plan(&["extract", "cover"]);
        let reads = json!({"cover": {"op": "entity_at", "subject_from": "/policy_id",
            "relation": "mg:coverage_limit", "at": "2026-03-18"}});
        let got = parse_reads(Some(&reads), &p, "org.uw").unwrap();
        assert_eq!(
            got["cover"],
            json!({"op": "entity_at", "ns": "org.uw", "into": "cover",
                   "subject_from": "/policy_id", "relation": "mg:coverage_limit",
                   "at": 1_773_792_000_000i64, "axis": "world"})
        );
        let walk = json!({"cover": {"op": "related", "start": "POL-1",
            "relations": "mg:owned_by, part_of", "ns": "org.uw.policies"}});
        let got = parse_reads(Some(&walk), &p, "org.uw").unwrap();
        assert_eq!(
            got["cover"],
            json!({"op": "related", "ns": "org.uw.policies", "into": "cover", "start": "POL-1",
                   "relations": ["mg:owned_by", "part_of"], "direction": "out",
                   "depth": 2, "limit": 64})
        );
    }

    /// `recall` (#342): `k` defaults to 16, the instant and its axis only
    /// appear when an instant is declared, and `relation` stays absent when
    /// the plan names none.
    #[test]
    fn a_recall_declaration_normalizes() {
        let p = plan(&["a", "r"]);
        let current = json!({"r": {"op": "recall", "subject_from": "/account"}});
        assert_eq!(
            parse_reads(Some(&current), &p, "org.uw").unwrap()["r"],
            json!({"op": "recall", "ns": "org.uw", "into": "r",
                   "subject_from": "/account", "k": 16})
        );
        let as_of = json!({"r": {"op": "recall", "ns": "org.uw.ledger", "subject": "ACC-1",
            "relation": "mg:statement", "k": 5, "at": "2026-03-18", "into": "stmts"}});
        assert_eq!(
            parse_reads(Some(&as_of), &p, "org.uw").unwrap()["r"],
            json!({"op": "recall", "ns": "org.uw.ledger", "into": "stmts", "subject": "ACC-1",
                   "relation": "mg:statement", "k": 5, "at": 1_773_792_000_000i64,
                   "axis": "world"})
        );
        let knowledge = json!({"r": {"op": "recall", "subject": "ACC-1", "k": 64,
            "at_from": "/asked_on", "axis": "knowledge"}});
        let got = parse_reads(Some(&knowledge), &p, "org.uw").unwrap();
        assert_eq!(got["r"]["k"], 64, "the ceiling itself is admitted");
        assert_eq!(got["r"]["axis"], "knowledge");
    }

    /// The count ceiling is refused at start, never clamped, and `recall`
    /// admits no free text and no predicate — an unknown key is a refusal.
    #[test]
    fn a_recall_past_its_bounds_is_refused_at_start() {
        let p = plan(&["a", "r"]);
        let cases: Vec<(Value, &str)> = vec![
            (json!({"op": "recall", "subject": "s", "k": 0}), "from 1 to 64"),
            (json!({"op": "recall", "subject": "s", "k": 65}), "from 1 to 64"),
            (json!({"op": "recall", "subject": "s", "k": -1}), "from 1 to 64"),
            (json!({"op": "recall", "subject": "s", "k": 2.5}), "from 1 to 64"),
            (json!({"op": "recall", "subject": "s", "k": "8"}), "from 1 to 64"),
            (json!({"op": "recall", "subject": "s", "query": "unpaid"}), "unknown key `query`"),
            (json!({"op": "recall", "subject": "s", "where": "x"}), "unknown key `where`"),
            (json!({"op": "recall", "subject": "s", "limit": 5}), "unknown key `limit`"),
            (json!({"op": "recall", "k": 3}), "exactly one of `subject` / `subject_from`"),
            (json!({"op": "recall", "subject": "s", "relation": ""}), "`relation` must be"),
            (json!({"op": "recall", "subject": "s", "axis": "world"}), "`axis` needs an instant"),
            (json!({"op": "recall", "subject": "s", "at": 1, "at_from": "/t"}), "exactly one of `at`"),
            (json!({"op": "recall", "subject": "s", "at": 1, "axis": "both"}), "`axis` must be"),
            (json!({"op": "recall", "subject": "s", "at_from": "t"}), "JSON pointer"),
            (json!({"op": "recall", "subject": "s", "ns": "org.uw.*"}), "not a pattern"),
        ];
        for (decl, want) in cases {
            let reads = json!({ "r": decl });
            let err = parse_reads(Some(&reads), &p, "org.uw").unwrap_err();
            assert!(matches!(err, RunError::InvalidPlan { .. }), "{reads}: {err}");
            assert!(err.to_string().contains(want), "{reads}: expected {want:?} in {err}");
            assert!(err.to_string().contains("read 'r'"), "names the node: {err}");
        }
        let outside = json!({"r": {"op": "recall", "ns": "org.uwx", "subject": "s"}});
        let err = parse_reads(Some(&outside), &p, "org.uw").unwrap_err();
        assert!(matches!(err, RunError::Unauthorized { .. }), "{err}");
    }

    /// Every refusal names the node, and a typo is a refusal — never a read
    /// on a default the author did not ask for.
    #[test]
    fn malformed_declarations_are_refused_at_start() {
        let p = plan(&["a", "r"]);
        let base = json!({"op": "entity_at", "subject": "s", "relation": "p", "at": 1});
        let cases: Vec<(Value, &str)> = vec![
            (json!({"zz": base}), "unknown node 'zz'"),
            // Declined in #342: a saved query is not a read op.
            (json!({"r": {"op": "saved_query", "name": "q"}}), "unknown op"),
            (json!({"r": {"subject": "s"}}), "names no `op`"),
            (
                json!({"r": {"op": "entity_at", "subject": "s", "relation": "p", "at": 1, "axsi": "knowledge"}}),
                "unknown key `axsi`",
            ),
            (
                json!({"r": {"op": "entity_at", "relation": "p", "at": 1}}),
                "exactly one of `subject` / `subject_from`",
            ),
            (
                json!({"r": {"op": "entity_at", "subject": "s", "subject_from": "/s", "relation": "p", "at": 1}}),
                "exactly one of `subject`",
            ),
            (
                json!({"r": {"op": "entity_at", "subject_from": "s", "relation": "p", "at": 1}}),
                "JSON pointer",
            ),
            (
                json!({"r": {"op": "entity_at", "subject": "s", "at": 1}}),
                "`relation` must be",
            ),
            (
                json!({"r": {"op": "entity_at", "subject": "s", "relation": "p"}}),
                "exactly one of `at` / `at_from`",
            ),
            (
                json!({"r": {"op": "entity_at", "subject": "s", "relation": "p", "at": "last week"}}),
                "not an ISO-8601",
            ),
            (
                json!({"r": {"op": "entity_at", "subject": "s", "relation": "p", "at": 1, "axis": "both"}}),
                "`axis` must be",
            ),
            (
                json!({"r": {"op": "entity_at", "subject": "s", "relation": "p", "at": 1, "into": "$send"}}),
                "reserved",
            ),
            (
                json!({"r": {"op": "related", "start": "s"}}),
                "at least one relation",
            ),
            (
                json!({"r": {"op": "related", "start": "s", "relations": ["p"], "depth": 9}}),
                "from 1 to 4",
            ),
            (
                json!({"r": {"op": "related", "start": "s", "relations": ["p"], "direction": "up"}}),
                "`direction` must be",
            ),
            (json!("nope"), "must be an object"),
        ];
        for (reads, want) in cases {
            let err = parse_reads(Some(&reads), &p, "org.uw").unwrap_err();
            assert!(
                matches!(err, RunError::InvalidPlan { .. }),
                "{reads}: {err}"
            );
            assert!(
                err.to_string().contains(want),
                "{reads}: expected {want:?} in {err}"
            );
        }
        let pattern = json!({"r": {"op": "related", "ns": "org.uw.*", "start": "s",
                                   "relations": ["p"]}});
        let err = parse_reads(Some(&pattern), &p, "org.uw").unwrap_err();
        assert!(err.to_string().contains("not a pattern"), "{err}");
        let outside = json!({"r": {"op": "entity_at", "ns": "org.other", "subject": "s",
                                   "relation": "p", "at": 1}});
        let err = parse_reads(Some(&outside), &p, "org.uw").unwrap_err();
        assert!(matches!(err, RunError::Unauthorized { .. }), "{err}");
        assert!(
            err.to_string().contains("outside this run's namespace"),
            "{err}"
        );
    }
}
