use std::collections::BTreeMap;

use rmpv::Value;
use unicode_normalization::UnicodeNormalization;

use crate::error::{AreevError, Hash, Result};
use crate::format::field_map::{
    compact_content_ref_field, compact_embedding_ref_field, compact_field,
    compact_related_to_field, compact_workflow_edge_field,
};
use crate::format::header::{content_address, MgHeader};
#[allow(clippy::wildcard_imports)]
use crate::types::*;

/// Encode an rmpv::Value to bytes.
fn encode_value_to_vec(value: &Value) -> Result<Vec<u8>> {
    let mut buf = Vec::new();
    rmpv::encode::write_value(&mut buf, value)
        .map_err(|e| AreevError::Serialization(format!("msgpack encode error: {}", e)))?;
    Ok(buf)
}

/// Reject non-finite floats (NaN / ±Inf) anywhere in the payload before it is
/// written. The deserializer refuses them (`serde_json::Number::from_f64`
/// returns None), so persisting one yields a grain that serializes but can
/// never be read back — and grains are immutable, so the store would be stuck
/// with it forever. Enforcing it on the write side keeps the serialize ⇒
/// deserialize symmetry invariant (a grain that can be written can be read).
fn reject_non_finite(value: &Value) -> Result<()> {
    match value {
        Value::F32(f) if !f.is_finite() => Err(AreevError::Format(
            "non-finite float (NaN/Inf) cannot be serialized".into(),
        )),
        Value::F64(f) if !f.is_finite() => Err(AreevError::Format(
            "non-finite float (NaN/Inf) cannot be serialized".into(),
        )),
        Value::Array(arr) => arr.iter().try_for_each(reject_non_finite),
        Value::Map(pairs) => pairs.iter().try_for_each(|(_, v)| reject_non_finite(v)),
        _ => Ok(()),
    }
}

/// Serialize a grain to .mg blob bytes and compute its content address.
/// Returns (blob_bytes, content_address_hash).
pub fn serialize_grain<G: Grain + 'static>(grain: &G) -> Result<(Vec<u8>, Hash)> {
    let common = grain.common();
    let created_at = common.created_at.unwrap_or_else(|| {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0)
    });

    let mut map = BTreeMap::<String, Value>::new();

    // Type field
    map.insert(
        compact_field("type").to_string(),
        Value::String(grain.grain_type().as_str().into()),
    );

    // Add grain-type-specific fields
    add_type_specific_fields(grain, &mut map);

    // Add common fields
    add_common_fields(common, created_at, &mut map);

    // Encode to msgpack
    let msgpack_value = btree_to_msgpack_map(map);
    reject_non_finite(&msgpack_value)?;
    let payload = encode_value_to_vec(&msgpack_value)?;

    // Build header
    let mut header = MgHeader::new(grain.grain_type(), common.namespace.as_deref(), created_at);
    if !common.content_refs.is_empty() {
        header.set_has_content_refs(true);
    }
    if !common.embedding_refs.is_empty() {
        header.set_has_embedding_refs(true);
    }
    if let Some(ref st) = common.source_type {
        match st.as_str() {
            "llm_generated" | "consolidated" | "inferred" | "a2a_recalled" => {
                header.set_ai_generated(true);
            }
            _ => {}
        }
    }
    header.set_sensitivity(detect_sensitivity(&common.tags));

    // Assemble blob: header + payload
    let header_bytes = header.to_bytes();
    let mut blob = Vec::with_capacity(9 + payload.len());
    blob.extend_from_slice(&header_bytes);
    blob.extend_from_slice(&payload);

    // Symmetry guard: never persist a grain we could not read back. Enforce the
    // same size + framing limits the deserializer applies to untrusted blobs, so
    // a successful write always round-trips (see deserialize::guard_msgpack_shape).
    if blob.len() > crate::format::deserialize::MAX_GRAIN_BYTES {
        return Err(crate::error::AreevError::Format(format!(
            "grain too large ({} bytes, maximum {})",
            blob.len(),
            crate::format::deserialize::MAX_GRAIN_BYTES
        )));
    }
    crate::format::deserialize::guard_msgpack_shape(&payload)?;

    let hash = content_address(&blob);
    Ok((blob, hash))
}

/// Serialize a grain, set the is_signed flag, and wrap in a COSE Sign1 envelope.
///
/// Returns `(cose_bytes, content_hash, inner_blob)` where:
/// - `cose_bytes` is the COSE Sign1 envelope to store/transmit.
/// - `content_hash` is SHA-256(inner_blob) — computed over the inner blob with is_signed=1.
/// - `inner_blob` is the raw .mg blob with is_signed flag set.
///
/// `org_context` is bound into the COSE AAD to prevent cross-org replay attacks — pass
/// `org_id.as_bytes()` whenever the org context is available. An empty slice is accepted
/// but reduces the cross-context binding guarantee.
#[cfg(feature = "signing")]
pub fn serialize_grain_signed<G: crate::Grain + 'static>(
    grain: &G,
    signing_key: &crate::crypto::signing::GrainSigningKey,
    org_context: &[u8],
) -> crate::error::Result<(Vec<u8>, crate::error::Hash, Vec<u8>)> {
    // Step 1: Normal serialization (is_signed flag is 0 at this point)
    let (mut blob, _) = serialize_grain(grain)?;
    // Step 2: Set is_signed flag (bit 0 of byte 1)
    blob[1] |= 0x01;
    // Step 3: Recompute content address over blob with is_signed=1
    let hash = crate::format::header::content_address(&blob);
    // Step 4: COSE Sign1 wrap with org-scoped AAD
    let signed = crate::crypto::signing::sign_grain(&blob, signing_key, org_context)?;
    Ok((signed.cose_bytes, hash, blob))
}

/// Detect sensitivity level from tags.
fn detect_sensitivity(tags: &[String]) -> u8 {
    for tag in tags {
        if tag.starts_with("phi:") {
            return 0x03; // PHI
        }
    }
    for tag in tags {
        if tag.starts_with("pii:") || tag.starts_with("sec:") || tag.starts_with("legal:") {
            return 0x02; // PII
        }
    }
    for tag in tags {
        if tag.starts_with("reg:") {
            return 0x01; // Regulated
        }
    }
    0x00 // Public
}

/// Create an NFC-normalized msgpack string value.
fn nfc_string(s: &str) -> Value {
    Value::String(s.nfc().collect::<String>().into())
}

/// Convert BTreeMap to a msgpack Map value (keys already sorted by BTreeMap).
fn btree_to_msgpack_map(map: BTreeMap<String, Value>) -> Value {
    let pairs: Vec<(Value, Value)> = map
        .into_iter()
        .map(|(k, v)| (Value::String(k.into()), v))
        .collect();
    Value::Map(pairs)
}

/// Convert serde_json::Value to rmpv::Value.
fn json_to_msgpack(json: &serde_json::Value) -> Value {
    match json {
        serde_json::Value::Null => Value::Nil,
        serde_json::Value::Bool(b) => Value::Boolean(*b),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Value::Integer(i.into())
            } else if let Some(u) = n.as_u64() {
                // u64 above i64::MAX: keep it an integer (the deserializer reads
                // u64) rather than lossily coercing it to f64.
                Value::Integer(u.into())
            } else if let Some(f) = n.as_f64() {
                Value::F64(f)
            } else {
                Value::Nil
            }
        }
        serde_json::Value::String(s) => nfc_string(s),
        serde_json::Value::Array(arr) => Value::Array(arr.iter().map(json_to_msgpack).collect()),
        serde_json::Value::Object(obj) => {
            let mut sorted: BTreeMap<String, Value> = BTreeMap::new();
            for (k, v) in obj {
                // NFC-normalize keys, not only values — otherwise two Unicode
                // composition variants of one key hash to different content
                // addresses, defeating the dedup NFC exists to provide.
                sorted.insert(k.nfc().collect::<String>(), json_to_msgpack(v));
            }
            btree_to_msgpack_map(sorted)
        }
    }
}

/// Convert serde_json::Value to rmpv::Value, compacting top-level map keys via FIELD_MAP.
/// Used for the `context` map where keys like "int:base_url" should be compacted to "ib".
/// Nested map values use standard json_to_msgpack (no key compaction) since they contain
/// user-defined schemas, not OMS field names.
fn json_to_msgpack_with_key_compaction(json: &serde_json::Value) -> Value {
    match json {
        serde_json::Value::Object(obj) => {
            let mut sorted: BTreeMap<String, Value> = BTreeMap::new();
            for (k, v) in obj {
                // NFC-normalize before compacting so composition variants of a
                // context key collapse (int:* keys are ASCII, so unaffected).
                let nfc_k = k.nfc().collect::<String>();
                let compacted_key = compact_field(&nfc_k).to_string();
                sorted.insert(compacted_key, json_to_msgpack(v));
            }
            btree_to_msgpack_map(sorted)
        }
        // Non-object context values pass through as-is
        other => json_to_msgpack(other),
    }
}

/// Add grain-type-specific fields to the map using downcasting.
fn add_type_specific_fields<G: Grain + 'static>(grain: &G, map: &mut BTreeMap<String, Value>) {
    use std::any::Any;
    let any = grain as &dyn Any;

    if let Some(fact) = any.downcast_ref::<Fact>() {
        map.insert(
            compact_field("subject").to_string(),
            nfc_string(&fact.subject),
        );
        map.insert(
            compact_field("relation").to_string(),
            nfc_string(&fact.relation),
        );
        // object can be string or map in OMS 1.2 — serialize as string for now
        map.insert(
            compact_field("object").to_string(),
            nfc_string(&fact.object),
        );
    } else if let Some(ev) = any.downcast_ref::<Event>() {
        map.insert("content".to_string(), nfc_string(&ev.content));
        if let Some(ref s) = ev.subject {
            map.insert(compact_field("subject").to_string(), nfc_string(s));
        }
        if let Some(ref o) = ev.object {
            map.insert(compact_field("object").to_string(), nfc_string(o));
        }
        if let Some(r) = ev.role {
            map.insert(compact_field("role").to_string(), nfc_string(r.as_str()));
        }
        if let Some(ref sid) = ev.session_id {
            map.insert(compact_field("session_id").to_string(), nfc_string(sid));
        }
        if let Some(ref pmid) = ev.parent_message_id {
            map.insert(
                compact_field("parent_message_id").to_string(),
                nfc_string(pmid),
            );
        }
        if let Some(ref blocks) = ev.content_blocks {
            // Serialize via serde_json → rmpv. No key compaction — `type`, `id`,
            // `name`, `input`, `tool_use_id`, `is_error` are wire-format fixed
            // strings, not OMS fields.
            if let Ok(json) = serde_json::to_value(blocks) {
                map.insert(
                    compact_field("content_blocks").to_string(),
                    json_to_msgpack(&json),
                );
            }
        }
        if let Some(ref m) = ev.model_id {
            map.insert(compact_field("model_id").to_string(), nfc_string(m));
        }
        if let Some(ref sr) = ev.stop_reason {
            map.insert(compact_field("stop_reason").to_string(), nfc_string(sr));
        }
        if let Some(ref tu) = ev.token_usage {
            if let Ok(json) = serde_json::to_value(tu) {
                map.insert(
                    compact_field("token_usage").to_string(),
                    json_to_msgpack(&json),
                );
            }
        }
        if let Some(ref rid) = ev.run_id {
            map.insert(compact_field("run_id").to_string(), nfc_string(rid));
        }
    } else if let Some(st) = any.downcast_ref::<State>() {
        // OMS §8.3: `context` is State's required field and shares the `ctx` wire
        // key with the common context (§6.1). Written here, so add_common_fields
        // must not clobber it. Inner keys stay verbatim — no key compaction.
        map.insert(
            compact_field("context").to_string(),
            json_to_msgpack(&st.context_data),
        );
        // `plan` and `history` stay uncompacted per OMS §6.3.
        if let Some(ref plan) = st.plan {
            if !plan.is_empty() {
                let arr: Vec<Value> = plan.iter().map(|s| nfc_string(s)).collect();
                map.insert("plan".to_string(), Value::Array(arr));
            }
        }
        if let Some(ref history) = st.history {
            if !history.is_empty() {
                let arr: Vec<Value> = history.iter().map(json_to_msgpack).collect();
                map.insert("history".to_string(), Value::Array(arr));
            }
        }
    } else if let Some(wf) = any.downcast_ref::<Workflow>() {
        // nodes: Vec<String>
        let nodes: Vec<Value> = wf.nodes.iter().map(|s| nfc_string(s)).collect();
        map.insert("nodes".to_string(), Value::Array(nodes));
        // edges: Vec<WorkflowEdge> — serialized as array of maps
        if !wf.edges.is_empty() {
            let edges: Vec<Value> = wf
                .edges
                .iter()
                .map(|e| {
                    let mut m = BTreeMap::new();
                    m.insert("src".to_string(), nfc_string(&e.src));
                    m.insert("dst".to_string(), nfc_string(&e.dst));
                    if let Some(ref c) = e.cond {
                        m.insert("cond".to_string(), nfc_string(c));
                    }
                    if let Some(mc) = e.max_cycles {
                        m.insert(
                            compact_workflow_edge_field("max_cycles").to_string(),
                            Value::Integer(mc.into()),
                        );
                    }
                    btree_to_msgpack_map(m)
                })
                .collect();
            map.insert("edges".to_string(), Value::Array(edges));
        }
        // bindings: HashMap<String, String>
        if !wf.bindings.is_empty() {
            let mut bind_map = BTreeMap::new();
            for (k, v) in &wf.bindings {
                bind_map.insert(k.clone(), nfc_string(v));
            }
            map.insert(
                compact_field("bindings").to_string(),
                btree_to_msgpack_map(bind_map),
            );
        }
        // retries: HashMap<String, u32>
        if !wf.retries.is_empty() {
            let mut retry_map = BTreeMap::new();
            for (k, v) in &wf.retries {
                retry_map.insert(k.clone(), Value::Integer((*v).into()));
            }
            map.insert("retries".to_string(), btree_to_msgpack_map(retry_map));
        }
    } else if let Some(t) = any.downcast_ref::<Trigger>() {
        // `kind` is not omit-default here (unlike Tool's): a Trigger has no
        // sensible "absent means interval" reading — the kind decides which
        // other fields are even meaningful, so it is always stated.
        map.insert(compact_field("kind").to_string(), nfc_string(t.kind.as_str()));
        map.insert(compact_field("workflow").to_string(), nfc_string(&t.workflow));
        if let Some(ref c) = t.connector {
            map.insert(compact_field("connector").to_string(), nfc_string(c));
        }
        if let Some(ref sc) = t.scope {
            map.insert(compact_field("scope").to_string(), nfc_string(sc));
        }
        // ALWAYS emitted, unlike most bools here. Omit-when-default exists to
        // keep *legacy* blobs byte-identical across a rollout, and this type is
        // new so there are none — while omitting it would break the most
        // natural query anyone writes: `RECALL triggers WHERE enabled = true`
        // matches nothing if the field is absent on every live trigger. The
        // whole reason a trigger is its own grain type rather than an
        // Observation carrying `int:` keys is that its fields are queryable, so
        // saving one byte at the cost of that would be the wrong trade.
        map.insert(compact_field("enabled").to_string(), Value::Boolean(t.enabled));
        if !t.dedup_key.is_empty() {
            let keys: Vec<Value> = t.dedup_key.iter().map(|k| nfc_string(k)).collect();
            map.insert(compact_field("dedup_key").to_string(), Value::Array(keys));
        }
        if let Some(i) = t.interval_secs {
            map.insert(compact_field("interval_secs").to_string(), Value::Integer(i.into()));
        }
        if let Some(ref c) = t.cron {
            map.insert(compact_field("cron").to_string(), nfc_string(c));
        }
        if let Some(a) = t.at_ms {
            map.insert(compact_field("at_ms").to_string(), Value::Integer(a.into()));
        }
        if let Some(ref p) = t.predicate {
            // A Condition tree: nested keys are the AST's own, never OMS field
            // names, so they must NOT be compacted (see `KeyMode::Verbatim`).
            map.insert(
                compact_field("predicate").to_string(),
                json_to_msgpack(p),
            );
        }
        if !t.members.is_empty() {
            let mut members = BTreeMap::new();
            for (alias, hash) in &t.members {
                members.insert(alias.clone(), nfc_string(hash));
            }
            map.insert(compact_field("members").to_string(), btree_to_msgpack_map(members));
        }
        if let Some(ref c) = t.correlate {
            map.insert(compact_field("correlate").to_string(), nfc_string(c));
        }
        if let Some(ref q) = t.context_query {
            // Omit-default (unlike `enabled`): pre-1.5 triggers carry no
            // context query, and absent-means-none keeps their addresses.
            map.insert(compact_field("context_query").to_string(), nfc_string(q));
        }
        if let Some(w) = t.window_ms {
            map.insert(compact_field("window_ms").to_string(), Value::Integer(w.into()));
        }
        if t.concurrency != crate::types::Concurrency::default() {
            map.insert(
                compact_field("concurrency").to_string(),
                nfc_string(t.concurrency.as_str()),
            );
        }
        if t.catchup != crate::types::Catchup::default() {
            map.insert(compact_field("catchup").to_string(), nfc_string(t.catchup.as_str()));
        }
        if let Some(ref c) = t.config {
            // int:* keys ARE compacted (ContextTop mode on read); connector-
            // specific keys pass through verbatim.
            map.insert(
                compact_field("config").to_string(),
                json_to_msgpack_with_key_compaction(c),
            );
        }
    } else if let Some(action) = any.downcast_ref::<Tool>() {
        map.insert(
            compact_field("tool_name").to_string(),
            nfc_string(&action.tool_name),
        );
        // Phase 1 (2026-04-19): kind discriminator. Default Execution is
        // omitted to keep legacy execution-record blobs byte-identical.
        if action.kind != crate::types::ToolKind::Execution {
            map.insert(
                compact_field("kind").to_string(),
                nfc_string(action.kind.as_str()),
            );
        }
        if let Some(ref inp) = action.input {
            map.insert(compact_field("input").to_string(), json_to_msgpack(inp));
        }
        if let Some(ref cnt) = action.content {
            // OMS 1.2: "content" for Tool compacts to "cnt" to avoid collision with Event's uncompacted "content"
            map.insert(compact_field("tool_content").to_string(), nfc_string(cnt));
        }
        if let Some(is_err) = action.is_error {
            map.insert(
                compact_field("is_error").to_string(),
                Value::Boolean(is_err),
            );
        }
        if let Some(ref err) = action.error {
            map.insert(compact_field("error").to_string(), nfc_string(err));
        }
        if let Some(dur) = action.duration_ms {
            map.insert(
                compact_field("duration_ms").to_string(),
                Value::Integer(dur.into()),
            );
        }
        if let Some(ref ptid) = action.parent_task_id {
            map.insert(
                compact_field("parent_task_id").to_string(),
                nfc_string(ptid),
            );
        }
        if let Some(ref tcid) = action.tool_call_id {
            map.insert(compact_field("tool_call_id").to_string(), nfc_string(tcid));
        }
        if let Some(ref cbid) = action.call_batch_id {
            map.insert(compact_field("call_batch_id").to_string(), nfc_string(cbid));
        }
        // Phase 1 — definition fields promoted from extra_fields.
        if let Some(ref tdesc) = action.tool_description {
            map.insert(
                compact_field("tool_description").to_string(),
                nfc_string(tdesc),
            );
        }
        if let Some(ref ischema) = action.input_schema {
            map.insert(
                compact_field("input_schema").to_string(),
                json_to_msgpack(ischema),
            );
        }
        // OMS 1.3: output_schema — JSON Schema for the action's return value
        if let Some(ref osch) = action.output_schema {
            map.insert(
                compact_field("output_schema").to_string(),
                json_to_msgpack(osch),
            );
        }
        if let Some(strict) = action.strict {
            map.insert(compact_field("strict").to_string(), Value::Boolean(strict));
        }
        if let Some(am) = action.async_mode {
            map.insert(compact_field("async_mode").to_string(), Value::Boolean(am));
        }
        if let Some(ref axu) = action.executor_uri {
            map.insert(compact_field("executor_uri").to_string(), nfc_string(axu));
        }
        if let Some(ref rt) = action.runtime {
            map.insert(compact_field("runtime").to_string(), nfc_string(rt));
        }
        if let Some(ref rl) = action.runtime_limits {
            map.insert(compact_field("runtime_limits").to_string(), json_to_msgpack(rl));
        }
        if let Some(ref lprm) = action.locked_params {
            map.insert(
                compact_field("locked_params").to_string(),
                json_to_msgpack(lprm),
            );
        }
        if let Some(ref exmp) = action.examples {
            let arr: Vec<Value> = exmp.iter().map(json_to_msgpack).collect();
            map.insert(compact_field("examples").to_string(), Value::Array(arr));
        }
        if let Some(ref anno) = action.annotations {
            if let Ok(json) = serde_json::to_value(anno) {
                map.insert(
                    compact_field("annotations").to_string(),
                    json_to_msgpack(&json),
                );
            }
        }
        if let Some(ref shsh) = action.spec_hash {
            map.insert(compact_field("spec_hash").to_string(), nfc_string(shsh));
        }
        // HPL Phase 4.1 (2026-04-22): emit `executor_kind` only when
        // explicitly set AND not the default Host — keeps legacy
        // binding blobs byte-identical across the rollout.
        if let Some(ek) = action.executor_kind {
            if ek != crate::types::executor_kind::ExecutorKind::Host {
                map.insert(
                    compact_field("executor_kind").to_string(),
                    nfc_string(ek.as_str()),
                );
            }
        }
        // Async exec lifecycle. Every field is
        // optional and omitted when `None` so legacy execution-record
        // blobs stay byte-identical.
        if let Some(st) = action.status {
            map.insert(compact_field("status").to_string(), nfc_string(st.as_str()));
        }
        if let Some(ref cid) = action.correlation_id {
            map.insert(compact_field("correlation_id").to_string(), nfc_string(cid));
        }
        if let Some(exp) = action.expires_at_sec {
            map.insert(
                compact_field("expires_at_sec").to_string(),
                Value::Integer(exp.into()),
            );
        }
        if let Some(ref tdh) = action.transient_definition_hash {
            map.insert(
                compact_field("transient_definition_hash").to_string(),
                Value::Binary(tdh.to_vec()),
            );
        }
        if let Some(fc) = action.failure_cause {
            map.insert(
                compact_field("failure_cause").to_string(),
                nfc_string(fc.as_str()),
            );
        }
        if let Some(ref fd) = action.failure_detail {
            map.insert(compact_field("failure_detail").to_string(), nfc_string(fd));
        }
        if let Some(aex) = action.actor_execution_environment {
            map.insert(
                compact_field("actor_execution_environment").to_string(),
                nfc_string(aex.as_str()),
            );
        }
    } else if let Some(obs) = any.downcast_ref::<Observation>() {
        map.insert(
            compact_field("observer_id").to_string(),
            nfc_string(&obs.observer_id),
        );
        map.insert(
            compact_field("observer_type").to_string(),
            nfc_string(&obs.observer_type),
        );
        if let Some(ref s) = obs.subject {
            map.insert(compact_field("subject").to_string(), nfc_string(s));
        }
        if let Some(ref o) = obs.object {
            map.insert(compact_field("object").to_string(), nfc_string(o));
        }
        if let Some(ref m) = obs.observer_model {
            map.insert(compact_field("observer_model").to_string(), nfc_string(m));
        }
        if let Some(ref mode) = obs.observation_mode {
            map.insert(
                compact_field("observation_mode").to_string(),
                nfc_string(mode.as_str()),
            );
        }
        if let Some(ref scope) = obs.observation_scope {
            map.insert(
                compact_field("observation_scope").to_string(),
                nfc_string(scope.as_str()),
            );
        }
    } else if let Some(goal) = any.downcast_ref::<Goal>() {
        map.insert(
            compact_field("description").to_string(),
            nfc_string(&goal.description),
        );
        map.insert(
            compact_field("goal_state").to_string(),
            nfc_string(goal.goal_state.as_str()),
        );
        if let Some(ref s) = goal.subject {
            map.insert(compact_field("subject").to_string(), nfc_string(s));
        }
        if let Some(ref o) = goal.object {
            map.insert(compact_field("object").to_string(), nfc_string(o));
        }
        if let Some(ref p) = goal.priority {
            map.insert(
                compact_field("priority").to_string(),
                nfc_string(p.as_str()),
            );
        }
        if let Some(ref c) = goal.criteria {
            map.insert(compact_field("criteria").to_string(), nfc_string(c));
        }
        if let Some(ref pgs) = goal.parent_goals {
            let arr: Vec<Value> = pgs.iter().map(|s| nfc_string(s)).collect();
            map.insert(compact_field("parent_goals").to_string(), Value::Array(arr));
        }
        if let Some(ref sr) = goal.state_reason {
            map.insert(compact_field("state_reason").to_string(), nfc_string(sr));
        }
        if let Some(ref se) = goal.satisfaction_evidence {
            map.insert(
                compact_field("satisfaction_evidence").to_string(),
                json_to_msgpack(se),
            );
        }
        if let Some(p) = goal.progress {
            map.insert(compact_field("progress").to_string(), Value::F64(p));
        }
        if let Some(ref dto) = goal.delegate_to {
            map.insert(compact_field("delegate_to").to_string(), nfc_string(dto));
        }
        if let Some(ref dfo) = goal.delegate_from {
            map.insert(compact_field("delegate_from").to_string(), nfc_string(dfo));
        }
        // OMS §6.7 Goal fields that had struct fields and compact keys but no arm
        // here — every write silently dropped them. All are Option, so omitting
        // when unset keeps existing blobs byte-identical.
        if let Some(ref crs) = goal.criteria_structured {
            map.insert(
                compact_field("criteria_structured").to_string(),
                json_to_msgpack(crs),
            );
        }
        if let Some(ref ep) = goal.expiry_policy {
            map.insert(compact_field("expiry_policy").to_string(), json_to_msgpack(ep));
        }
        if let Some(ref rec) = goal.recurrence {
            map.insert(compact_field("recurrence").to_string(), json_to_msgpack(rec));
        }
        if let Some(evreq) = goal.evidence_required {
            map.insert(
                compact_field("evidence_required").to_string(),
                Value::Boolean(evreq),
            );
        }
        if let Some(rof) = goal.rollback_on_failure {
            map.insert(
                compact_field("rollback_on_failure").to_string(),
                Value::Boolean(rof),
            );
        }
        if let Some(ref atr) = goal.allowed_transitions {
            let arr: Vec<Value> = atr.iter().map(|s| nfc_string(s)).collect();
            map.insert(
                compact_field("allowed_transitions").to_string(),
                Value::Array(arr),
            );
        }
    } else if let Some(reasoning) = any.downcast_ref::<Reasoning>() {
        if !reasoning.premises.is_empty() {
            let arr: Vec<Value> = reasoning.premises.iter().map(|s| nfc_string(s)).collect();
            map.insert(compact_field("premises").to_string(), Value::Array(arr));
        }
        if let Some(ref c) = reasoning.conclusion {
            map.insert(compact_field("conclusion").to_string(), nfc_string(c));
        }
        if let Some(ref m) = reasoning.inference_method {
            map.insert(compact_field("inference_method").to_string(), nfc_string(m));
        }
        if !reasoning.alternatives_considered.is_empty() {
            let arr: Vec<Value> = reasoning
                .alternatives_considered
                .iter()
                .map(|s| nfc_string(s))
                .collect();
            map.insert(
                compact_field("alternatives_considered").to_string(),
                Value::Array(arr),
            );
        }
        if let Some(ref t) = reasoning.thinking_content {
            map.insert(compact_field("thinking_content").to_string(), nfc_string(t));
        }
        if let Some(tr) = reasoning.thinking_redacted {
            map.insert(
                compact_field("thinking_redacted").to_string(),
                Value::Boolean(tr),
            );
        }
    } else if let Some(consensus) = any.downcast_ref::<Consensus>() {
        if !consensus.participating_observers.is_empty() {
            let arr: Vec<Value> = consensus
                .participating_observers
                .iter()
                .map(|s| nfc_string(s))
                .collect();
            map.insert(
                compact_field("participating_observers").to_string(),
                Value::Array(arr),
            );
        }
        if let Some(t) = consensus.threshold {
            map.insert(compact_field("threshold").to_string(), Value::F64(t));
        }
        if let Some(ac) = consensus.agreement_count {
            map.insert(
                compact_field("agreement_count").to_string(),
                Value::Integer(ac.into()),
            );
        }
        if let Some(dc) = consensus.dissent_count {
            map.insert(
                compact_field("dissent_count").to_string(),
                Value::Integer(dc.into()),
            );
        }
        if !consensus.dissent_grains.is_empty() {
            let arr: Vec<Value> = consensus
                .dissent_grains
                .iter()
                .map(|s| nfc_string(s))
                .collect();
            map.insert(
                compact_field("dissent_grains").to_string(),
                Value::Array(arr),
            );
        }
        if let Some(ref ac) = consensus.agreed_content {
            map.insert(compact_field("agreed_content").to_string(), nfc_string(ac));
        }
    } else if let Some(consent) = any.downcast_ref::<Consent>() {
        map.insert(
            compact_field("subject_did").to_string(),
            nfc_string(&consent.subject_did),
        );
        if let Some(ref g) = consent.grantee_did {
            map.insert(compact_field("grantee_did").to_string(), nfc_string(g));
        }
        if let Some(ref s) = consent.scope {
            map.insert(compact_field("scope").to_string(), nfc_string(s));
        }
        if let Some(iw) = consent.is_withdrawal {
            map.insert(
                compact_field("is_withdrawal").to_string(),
                Value::Boolean(iw),
            );
        }
        if let Some(ref b) = consent.basis {
            map.insert(compact_field("basis").to_string(), nfc_string(b));
        }
        if let Some(ref j) = consent.jurisdiction {
            map.insert(compact_field("jurisdiction").to_string(), nfc_string(j));
        }
        if let Some(ref pc) = consent.prior_consent {
            map.insert(compact_field("prior_consent").to_string(), nfc_string(pc));
        }
        if !consent.witness_dids.is_empty() {
            let arr: Vec<Value> = consent.witness_dids.iter().map(|s| nfc_string(s)).collect();
            map.insert(compact_field("witness_dids").to_string(), Value::Array(arr));
        }
    } else if let Some(rec) = any.downcast_ref::<Recommendation>() {
        // OMS 1.5 §8.12 / §6.13. Note `rec_status` is deliberately absent: it is
        // index-layer state rebuilt from the audit chain, and keeping it out of
        // the blob is what makes a recommendation's content address stable
        // across its whole review lifecycle.
        map.insert(
            compact_field("target_ref").to_string(),
            nfc_string(&rec.target_ref),
        );
        {
            let mut a = BTreeMap::new();
            a.insert("id".to_string(), nfc_string(&rec.analyzer.id));
            if let Some(ref p) = rec.analyzer.params {
                a.insert("params".to_string(), json_to_msgpack(p));
            }
            map.insert(
                compact_field("analyzer").to_string(),
                btree_to_msgpack_map(a),
            );
        }
        {
            // A deterministic template id + its args — never analyzer prose
            // (rule 4), so a summary stays reproducible and translatable.
            let mut sm = BTreeMap::new();
            sm.insert(
                "template_id".to_string(),
                nfc_string(&rec.summary.template_id),
            );
            sm.insert("args".to_string(), json_to_msgpack(&rec.summary.args));
            map.insert(
                compact_field("summary").to_string(),
                btree_to_msgpack_map(sm),
            );
        }
        map.insert(
            compact_field("dedup_key").to_string(),
            nfc_string(&rec.dedup_key),
        );
        // Exactly one proposal (rule 1) — an enum here, so the invariant cannot
        // be violated by construction.
        match &rec.proposal {
            Proposal::Cal(cal) => {
                map.insert(compact_field("proposal_cal").to_string(), nfc_string(cal));
            }
            Proposal::Edit(e) => {
                map.insert(
                    compact_field("proposal_edit").to_string(),
                    json_to_msgpack(e),
                );
            }
            Proposal::Data(d) => {
                map.insert(
                    compact_field("proposal_data").to_string(),
                    json_to_msgpack(d),
                );
            }
        }
        if let Some(sev) = rec.severity {
            map.insert(
                compact_field("severity").to_string(),
                nfc_string(sev.as_str()),
            );
        }
        if let Some(ref ms) = rec.metric_snapshot {
            map.insert(
                compact_field("metric_snapshot").to_string(),
                json_to_msgpack(ms),
            );
        }
        if let Some(ref eq) = rec.evidence_query {
            map.insert(compact_field("evidence_query").to_string(), nfc_string(eq));
        }
    } else if let Some(skill) = any.downcast_ref::<Skill>() {
        // OMS 1.4 Skill (0x0B). Required name + description; optional
        // definition + learned-competence fields. Empty vecs and None
        // options are omitted. `proficiency` aliases `confidence` (D3) — the
        // `prof` key is emitted from `common.confidence` ONLY for held
        // instances; `confidence` is also carried under `c`, so the two read
        // identically by construction.
        map.insert(compact_field("name").to_string(), nfc_string(&skill.name));
        // description reuses the SHARED `desc` key (same as Goal).
        map.insert(
            compact_field("description").to_string(),
            nfc_string(&skill.description),
        );
        if let Some(ref instr) = skill.instructions {
            map.insert(compact_field("instructions").to_string(), nfc_string(instr));
        }
        if let Some(ref wtu) = skill.when_to_use {
            map.insert(compact_field("when_to_use").to_string(), nfc_string(wtu));
        }
        if let Some(ref sver) = skill.version {
            map.insert(compact_field("version").to_string(), nfc_string(sver));
        }
        if !skill.allowed_tools.is_empty() {
            let arr: Vec<Value> = skill.allowed_tools.iter().map(|s| nfc_string(s)).collect();
            map.insert(
                compact_field("allowed_tools").to_string(),
                Value::Array(arr),
            );
        }
        if !skill.resources.is_empty() {
            let arr: Vec<Value> = skill.resources.iter().map(|s| nfc_string(s)).collect();
            map.insert(compact_field("resources").to_string(), Value::Array(arr));
        }
        if !skill.dependencies.is_empty() {
            let arr: Vec<Value> = skill.dependencies.iter().map(|s| nfc_string(s)).collect();
            map.insert(compact_field("dependencies").to_string(), Value::Array(arr));
        }
        if !skill.input_modalities.is_empty() {
            let arr: Vec<Value> = skill
                .input_modalities
                .iter()
                .map(|s| nfc_string(s))
                .collect();
            map.insert(
                compact_field("input_modalities").to_string(),
                Value::Array(arr),
            );
        }
        if !skill.output_modalities.is_empty() {
            let arr: Vec<Value> = skill
                .output_modalities
                .iter()
                .map(|s| nfc_string(s))
                .collect();
            map.insert(
                compact_field("output_modalities").to_string(),
                Value::Array(arr),
            );
        }
        if let Some(ref dom) = skill.domain {
            map.insert(compact_field("domain").to_string(), nfc_string(dom));
        }
        if let Some(ref hdid) = skill.holder_did {
            map.insert(compact_field("holder_did").to_string(), nfc_string(hdid));
        }
        // proficiency (D3): emit `prof` from confidence only for held skills.
        if skill.is_held() {
            map.insert(
                compact_field("proficiency").to_string(),
                Value::F64(skill.common.confidence),
            );
        }
        if let Some(prcnt) = skill.practice_count {
            map.insert(
                compact_field("practice_count").to_string(),
                Value::Integer(prcnt.into()),
            );
        }
        if let Some(lpa) = skill.last_practiced_at {
            map.insert(
                compact_field("last_practiced_at").to_string(),
                Value::Integer(lpa.into()),
            );
        }
        if !skill.strategies.is_empty() {
            // Serialize via serde_json → rmpv. No key compaction — the
            // SkillStrategy fields (`condition`, `workflow`, `description`)
            // are serde field names, not OMS field-map entries.
            if let Ok(json) = serde_json::to_value(&skill.strategies) {
                map.insert(
                    compact_field("strategies").to_string(),
                    json_to_msgpack(&json),
                );
            }
        }
        if let Some(xfer) = skill.transferable {
            map.insert(
                compact_field("transferable").to_string(),
                Value::Boolean(xfer),
            );
        }
    }
}

/// Add common fields to the map (compacted, nulls omitted, sorted by BTreeMap).
fn add_common_fields(common: &GrainCommon, created_at: i64, map: &mut BTreeMap<String, Value>) {
    if let Some(ref adid) = common.author_did {
        map.insert(compact_field("author_did").to_string(), nfc_string(adid));
    }
    map.insert(
        compact_field("confidence").to_string(),
        Value::F64(common.confidence),
    );
    map.insert(
        compact_field("created_at").to_string(),
        Value::Integer(created_at.into()),
    );

    if let Some(ref ns) = common.namespace {
        map.insert(compact_field("namespace").to_string(), nfc_string(ns));
    }
    if let Some(ref uid) = common.user_id {
        map.insert(compact_field("user_id").to_string(), nfc_string(uid));
    }
    if !common.tags.is_empty() {
        let arr: Vec<Value> = common.tags.iter().map(|s| nfc_string(s)).collect();
        map.insert(
            compact_field("structural_tags").to_string(),
            Value::Array(arr),
        );
    }
    if let Some(ref st) = common.source_type {
        map.insert(compact_field("source_type").to_string(), nfc_string(st));
    }
    if let Some(im) = common.importance {
        map.insert(compact_field("importance").to_string(), Value::F64(im));
    }
    if let Some(ref tt) = common.temporal_type {
        let s = match tt {
            TemporalType::State => "state",
            TemporalType::Event => "event",
            TemporalType::Interval => "interval",
        };
        map.insert(compact_field("temporal_type").to_string(), nfc_string(s));
    }
    if let Some(vf) = common.valid_from {
        map.insert(
            compact_field("valid_from").to_string(),
            Value::Integer(vf.into()),
        );
    }
    if let Some(vt) = common.valid_to {
        map.insert(
            compact_field("valid_to").to_string(),
            Value::Integer(vt.into()),
        );
    }
    if let Some(svf) = common.system_valid_from {
        map.insert(
            compact_field("system_valid_from").to_string(),
            Value::Integer(svf.into()),
        );
    }
    if let Some(svt) = common.system_valid_to {
        map.insert(
            compact_field("system_valid_to").to_string(),
            Value::Integer(svt.into()),
        );
    }
    if let Some(ref df) = common.derived_from {
        map.insert(compact_field("derived_from").to_string(), nfc_string(df));
    }
    if let Some(cl) = common.consolidation_level {
        map.insert(
            compact_field("consolidation_level").to_string(),
            Value::Integer(cl.into()),
        );
    }
    if let Some(ref odid) = common.origin_did {
        map.insert(compact_field("origin_did").to_string(), nfc_string(odid));
    }
    if let Some(ref ons) = common.origin_namespace {
        map.insert(
            compact_field("origin_namespace").to_string(),
            nfc_string(ons),
        );
    }
    if let Some(ref sb) = common.superseded_by {
        map.insert(compact_field("superseded_by").to_string(), nfc_string(sb));
    }
    if let Some(ref vs) = common.verification_status {
        map.insert(
            compact_field("verification_status").to_string(),
            nfc_string(vs),
        );
    }

    // Embedding text override
    if let Some(ref et) = common.embedding_text {
        map.insert(compact_field("embedding_text").to_string(), nfc_string(et));
    }

    // Context map — compact string keys using FIELD_MAP (supports int:* profile keys).
    //
    // Type-specific fields win on key collision: OMS gives State (§8.3) a required
    // `context` field that shares this wire key, and `add_type_specific_fields` runs
    // first. Inserting unconditionally here silently replaced a State's whole
    // snapshot with the common metadata map. No other grain type writes `ctx`, so
    // the common path below is what every type but State takes.
    //
    // When the key IS taken, the common context does not simply lose — it moves to
    // `cctx`. Yielding is right (a State's `ctx` means its snapshot), but dropping
    // is not: the common context is where `merge_heads` records `merge_parents` and
    // where the import pipelines record provenance, so a State that was merged or
    // imported was losing that record at the blob boundary. Merging into the
    // snapshot was the other option and is worse — a checkpoint an agent resumes
    // from should carry the agent's state and nothing else. Only State can reach
    // this branch today, and only when it carries a common context, so no existing
    // blob changes.
    if let Some(ref ctx) = common.context {
        let key = compact_field("context").to_string();
        if map.contains_key(&key) {
            map.insert(
                compact_field("common_context").to_string(),
                json_to_msgpack_with_key_compaction(ctx),
            );
        } else {
            map.insert(key, json_to_msgpack_with_key_compaction(ctx));
        }
    }

    // Invalidation policy
    if let Some(ref ip) = common.invalidation_policy {
        let mut ip_map = BTreeMap::new();
        if let Some(ref auth) = ip.authorized {
            let arr: Vec<Value> = auth.iter().map(|s| nfc_string(s)).collect();
            ip_map.insert("authorized".to_string(), Value::Array(arr));
        }
        if let Some(ref fb) = ip.fallback_mode {
            ip_map.insert("fallback_mode".to_string(), nfc_string(fb));
        }
        if let Some(locked_until) = ip.locked_until {
            ip_map.insert(
                "locked_until".to_string(),
                Value::Integer(locked_until.into()),
            );
        }
        ip_map.insert("mode".to_string(), nfc_string(&ip.mode));
        if let Some(ref reason) = ip.protection_reason {
            ip_map.insert("protection_reason".to_string(), nfc_string(reason));
        }
        if let Some(ref scope) = ip.scope {
            ip_map.insert("scope".to_string(), nfc_string(scope));
        }
        if let Some(threshold) = ip.threshold {
            ip_map.insert("threshold".to_string(), Value::Integer(threshold.into()));
        }
        map.insert(
            compact_field("invalidation_policy").to_string(),
            btree_to_msgpack_map(ip_map),
        );
    }

    // content_refs
    if !common.content_refs.is_empty() {
        let arr: Vec<Value> = common
            .content_refs
            .iter()
            .map(|cr| {
                let mut m = BTreeMap::new();
                if let Some(ref ck) = cr.checksum {
                    m.insert(
                        compact_content_ref_field("checksum").to_string(),
                        nfc_string(ck),
                    );
                }
                if let Some(ref md) = cr.metadata {
                    m.insert(
                        compact_content_ref_field("metadata").to_string(),
                        json_to_msgpack(md),
                    );
                }
                if let Some(ref mt) = cr.mime_type {
                    m.insert(
                        compact_content_ref_field("mime_type").to_string(),
                        nfc_string(mt),
                    );
                }
                if let Some(ref mod_) = cr.modality {
                    m.insert(
                        compact_content_ref_field("modality").to_string(),
                        nfc_string(mod_),
                    );
                }
                if let Some(sz) = cr.size_bytes {
                    m.insert(
                        compact_content_ref_field("size_bytes").to_string(),
                        Value::Integer(sz.into()),
                    );
                }
                m.insert(
                    compact_content_ref_field("uri").to_string(),
                    nfc_string(&cr.uri),
                );
                btree_to_msgpack_map(m)
            })
            .collect();
        map.insert(compact_field("content_refs").to_string(), Value::Array(arr));
    }

    // embedding_refs
    if !common.embedding_refs.is_empty() {
        let arr: Vec<Value> = common
            .embedding_refs
            .iter()
            .map(|er| {
                let mut m = BTreeMap::new();
                if let Some(ref di) = er.distance_metric {
                    m.insert(
                        compact_embedding_ref_field("distance_metric").to_string(),
                        nfc_string(di),
                    );
                }
                if let Some(dm) = er.dimensions {
                    m.insert(
                        compact_embedding_ref_field("dimensions").to_string(),
                        Value::Integer(dm.into()),
                    );
                }
                if let Some(ref mo) = er.model {
                    m.insert(
                        compact_embedding_ref_field("model").to_string(),
                        nfc_string(mo),
                    );
                }
                if let Some(ref ms) = er.modality_source {
                    m.insert(
                        compact_embedding_ref_field("modality_source").to_string(),
                        nfc_string(ms),
                    );
                }
                m.insert(
                    compact_embedding_ref_field("vector_id").to_string(),
                    nfc_string(&er.vector_id),
                );
                btree_to_msgpack_map(m)
            })
            .collect();
        map.insert(
            compact_field("embedding_refs").to_string(),
            Value::Array(arr),
        );
    }

    // provenance_chain — OMS §6.1 `pc`, the derivation trail. Declared on
    // GrainCommon and given a compact key, but never serialized: anything a
    // caller put here was silently dropped at the blob boundary. Entries are
    // free-form maps and, per §6.2, are NOT compacted recursively.
    if !common.provenance_chain.is_empty() {
        let arr: Vec<Value> = common
            .provenance_chain
            .iter()
            .map(|p| json_to_msgpack(&p.data))
            .collect();
        map.insert(
            compact_field("provenance_chain").to_string(),
            Value::Array(arr),
        );
    }

    // related_to
    if !common.related_to.is_empty() {
        let arr: Vec<Value> = common
            .related_to
            .iter()
            .map(|rt| {
                let mut m = BTreeMap::new();
                m.insert(
                    compact_related_to_field("hash").to_string(),
                    nfc_string(&rt.hash),
                );
                m.insert(
                    compact_related_to_field("relation_type").to_string(),
                    nfc_string(&rt.relation_type),
                );
                if let Some(w) = rt.weight {
                    m.insert(
                        compact_related_to_field("weight").to_string(),
                        Value::F64(w),
                    );
                }
                btree_to_msgpack_map(m)
            })
            .collect();
        map.insert(compact_field("related_to").to_string(), Value::Array(arr));
    }

    // Extra fields — custom fields not in the grain type's whitelist.
    // Stored as-is (no compaction) since they're not in FIELD_MAP.
    for (key, value) in &common.extra_fields {
        // NFC-normalize the key for content-address stability (composition
        // variants must collapse), same as every other stored string.
        let key = key.nfc().collect::<String>();
        // Skip if a known field somehow ended up here (defensive).
        if map.contains_key(&key) || map.contains_key(compact_field(&key)) {
            continue;
        }
        map.insert(key, json_to_msgpack(value));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_serialize_minimal_fact() {
        // Minimal Fact
        let fact = Fact::new("user", "likes", "coffee")
            .confidence(0.9)
            .source_type("user_explicit")
            .created_at(1768471200000_i64)
            .namespace("shared")
            .author_did("did:key:z6MkhaXgBZDvotDkL5257faiztiGiC2QtKLGpbnnEGta2doK");

        let (blob, hash) = serialize_grain(&fact).unwrap();

        // Fact type string
        assert!(!hash.to_hex().is_empty());
        assert_eq!(blob[0], 0x01); // version
        assert_eq!(blob[2], 0x01); // fact type byte (0x01)
    }

    #[test]
    fn test_serialize_protected_fact() {
        // Protected Fact with invalidation_policy
        let mut fact = Fact::new(
            "agent-007",
            "constraint",
            "never delete user files without confirmation",
        )
        .confidence(1.0)
        .source_type("user_explicit")
        .created_at(1768471200000_i64)
        .namespace("safety");

        fact.common.invalidation_policy = Some(InvalidationPolicy {
            mode: "locked".to_string(),
            authorized: Some(vec![
                "did:key:z6MkhaXgBZDvotDkL5257faiztiGiC2QtKLGpbnnEGta2doK".to_string(),
            ]),
            threshold: None,
            locked_until: None,
            fallback_mode: None,
            scope: None,
            protection_reason: None,
        });

        let (blob, _hash) = serialize_grain(&fact).unwrap();
        assert!(!blob.is_empty());
        assert_eq!(blob[2], 0x01); // fact type byte
    }

    #[test]
    fn test_serialize_tool_with_output_schema() {
        use crate::format::deserialize::deserialize_blob;

        let schema = serde_json::json!({
            "type": "object",
            "properties": {
                "id": { "type": "integer" },
                "number": { "type": "integer" },
                "html_url": { "type": "string" }
            }
        });

        let tool = Tool::new("github:create-issue")
            .output_schema(schema)
            .created_at(1768471200000_i64)
            .namespace("example:connectors:github");

        let (blob, _hash) = serialize_grain(&tool).unwrap();
        assert_eq!(blob[2], 0x05); // Tool type byte

        // Roundtrip: deserialize and verify output_schema is present
        let deserialized = deserialize_blob(&blob).unwrap();
        assert_eq!(deserialized.grain_type, GrainType::Tool);
        assert_eq!(
            deserialized.get_str("tool_name"),
            Some("github:create-issue")
        );
        let osch = deserialized.fields.get("output_schema");
        assert!(
            osch.is_some(),
            "output_schema should be present after deserialization"
        );
        let osch_obj = osch.unwrap().as_object().unwrap();
        assert_eq!(osch_obj.get("type").unwrap(), "object");
    }

    #[test]
    fn test_serialize_tool_without_output_schema() {
        // output_schema=None should not appear in serialized blob
        let tool = Tool::new("calculator")
            .content("42")
            .created_at(1768471200000_i64);

        let (blob, _hash) = serialize_grain(&tool).unwrap();
        // Verify it doesn't contain the "osch" key by checking the raw blob
        let blob_str = String::from_utf8_lossy(&blob);
        assert!(
            !blob_str.contains("osch"),
            "osch should be omitted when output_schema is None"
        );
    }

    #[test]
    fn test_serialize_context_with_integration_keys() {
        use crate::format::deserialize::deserialize_blob;

        let mut tool = Tool::new("github:create-issue")
            .created_at(1768471200000_i64)
            .namespace("example:connectors:github");

        // Set context map with int:* keys
        tool.common.context = Some(serde_json::json!({
            "int:base_url": "https://api.github.com",
            "int:http_method": "POST",
            "int:http_path": "/repos/{owner}/{repo}/issues",
            "int:connector": "github",
            "int:auth_type": "api_key:bearer"
        }));

        let (blob, _hash) = serialize_grain(&tool).unwrap();
        let deserialized = deserialize_blob(&blob).unwrap();

        // Context map keys should be expanded back to int:* names
        let ctx = deserialized
            .fields
            .get("context")
            .unwrap()
            .as_object()
            .unwrap();
        assert_eq!(ctx.get("int:base_url").unwrap(), "https://api.github.com");
        assert_eq!(ctx.get("int:http_method").unwrap(), "POST");
        assert_eq!(
            ctx.get("int:http_path").unwrap(),
            "/repos/{owner}/{repo}/issues"
        );
        assert_eq!(ctx.get("int:connector").unwrap(), "github");
        assert_eq!(ctx.get("int:auth_type").unwrap(), "api_key:bearer");
    }

    #[test]
    fn test_serialize_event() {
        // Event
        let ev = Event::new("User discussed vacation plans for March")
            .namespace("conversations")
            .created_at(1768471200000_i64);

        let (blob, _hash) = serialize_grain(&ev).unwrap();
        assert!(!blob.is_empty());
        assert_eq!(blob[0], 0x01); // version
        assert_eq!(blob[2], 0x02); // event type byte (0x02)
    }

    #[test]
    fn test_extra_fields_round_trip() {
        use crate::format::deserialize::deserialize_blob;

        // Goal with custom extra fields (the Atmatic use case)
        let mut goal = Goal::new("complete benchmark run")
            .confidence(0.95)
            .namespace("meta-agent")
            .created_at(1768471200000_i64);

        goal.common_mut().extra_fields.insert(
            "output".to_string(),
            serde_json::json!("benchmark passed with 97% recall"),
        );
        goal.common_mut()
            .extra_fields
            .insert("total_tokens".to_string(), serde_json::json!(42500));
        goal.common_mut()
            .extra_fields
            .insert("steps_executed".to_string(), serde_json::json!(12));
        goal.common_mut()
            .extra_fields
            .insert("cost_micro_credits".to_string(), serde_json::json!(1500));
        // Nested object
        goal.common_mut().extra_fields.insert(
            "metrics".to_string(),
            serde_json::json!({"recall": 0.97, "precision": 0.92, "f1": 0.945}),
        );

        let (blob, _hash) = serialize_grain(&goal).unwrap();

        // Deserialize and verify all extra fields survived
        let deserialized = deserialize_blob(&blob).unwrap();
        assert_eq!(deserialized.grain_type, GrainType::Goal);

        // Known fields present
        assert_eq!(
            deserialized.get_str("description"),
            Some("complete benchmark run")
        );
        assert_eq!(
            deserialized
                .fields
                .get("confidence")
                .and_then(|v| v.as_f64()),
            Some(0.95)
        );

        // Extra fields present
        assert_eq!(
            deserialized.fields.get("output").and_then(|v| v.as_str()),
            Some("benchmark passed with 97% recall"),
        );
        assert_eq!(
            deserialized
                .fields
                .get("total_tokens")
                .and_then(|v| v.as_i64()),
            Some(42500),
        );
        assert_eq!(
            deserialized
                .fields
                .get("steps_executed")
                .and_then(|v| v.as_i64()),
            Some(12),
        );
        assert_eq!(
            deserialized
                .fields
                .get("cost_micro_credits")
                .and_then(|v| v.as_i64()),
            Some(1500),
        );

        // Nested object
        let metrics = deserialized
            .fields
            .get("metrics")
            .unwrap()
            .as_object()
            .unwrap();
        assert_eq!(metrics.get("recall").and_then(|v| v.as_f64()), Some(0.97));
        assert_eq!(
            metrics.get("precision").and_then(|v| v.as_f64()),
            Some(0.92)
        );
    }

    #[test]
    fn test_extra_fields_empty_no_overhead() {
        use crate::format::deserialize::deserialize_blob;

        // Fact with no extra fields — blob should be identical to before
        let fact = Fact::new("user", "likes", "rust")
            .confidence(0.9)
            .created_at(1768471200000_i64);

        assert!(fact.common().extra_fields.is_empty());

        let (blob, _) = serialize_grain(&fact).unwrap();
        let deserialized = deserialize_blob(&blob).unwrap();

        // No extra fields in deserialized output
        let known_keys: std::collections::HashSet<&str> = [
            "type",
            "subject",
            "relation",
            "object",
            "confidence",
            "created_at",
        ]
        .into_iter()
        .collect();
        for key in deserialized.fields.keys() {
            assert!(
                known_keys.contains(key.as_str()),
                "unexpected field in deserialized output: {key}"
            );
        }
    }

    #[test]
    fn test_extra_field_builder() {
        use crate::format::deserialize::deserialize_blob;

        // Test the .extra_field() builder method
        let event = Event::new("agent completed task")
            .namespace("tasks")
            .created_at(1768471200000_i64)
            .extra_field("task_id", serde_json::json!("task-42"))
            .extra_field("duration_sec", serde_json::json!(3.15));

        let (blob, _) = serialize_grain(&event).unwrap();
        let deserialized = deserialize_blob(&blob).unwrap();

        assert_eq!(
            deserialized.fields.get("task_id").and_then(|v| v.as_str()),
            Some("task-42"),
        );
        assert_eq!(
            deserialized
                .fields
                .get("duration_sec")
                .and_then(|v| v.as_f64()),
            Some(3.15),
        );
    }

    #[test]
    fn test_event_chat_fields_roundtrip() {
        use crate::format::deserialize::deserialize_blob;

        let blocks = vec![
            ContentBlock::Text {
                text: "let me check".into(),
            },
            ContentBlock::ToolUse {
                id: "toolu_01".into(),
                name: "calculator".into(),
                input: serde_json::json!({"x": 2, "y": 3}),
            },
        ];
        let tu = TokenUsage {
            input_tokens: 42,
            output_tokens: 17,
            cache_read_tokens: None,
            cache_creation_tokens: None,
        };
        let ev = Event::new("hello from assistant")
            .role(Role::Assistant)
            .session("conv-abc".into())
            .parent_message("deadbeef".into())
            .content_blocks(blocks.clone())
            .model("claude-opus-4.7".into())
            .stop_reason("end_turn".into())
            .token_usage(tu)
            .run_id("run-99".into());
        let mut ev = ev;
        ev.common.namespace = Some("harness:slug:conv-abc".into());
        ev.common.user_id = Some("alice".into());
        ev.common.created_at = Some(1_768_471_200_000);

        let (blob, _) = serialize_grain(&ev).unwrap();
        let des = deserialize_blob(&blob).unwrap();
        let back = des.to_event().unwrap();

        assert_eq!(back.content, "hello from assistant");
        assert_eq!(back.role, Some(Role::Assistant));
        assert_eq!(back.session_id.as_deref(), Some("conv-abc"));
        assert_eq!(back.parent_message_id.as_deref(), Some("deadbeef"));
        assert_eq!(back.model_id.as_deref(), Some("claude-opus-4.7"));
        assert_eq!(back.stop_reason.as_deref(), Some("end_turn"));
        assert_eq!(back.run_id.as_deref(), Some("run-99"));
        assert_eq!(back.token_usage, Some(tu));
        assert_eq!(back.content_blocks.as_ref().unwrap(), &blocks);
    }

    #[test]
    fn test_event_compact_keys_present_in_blob() {
        let ev = Event::new("hi")
            .role(Role::User)
            .session("conv-x".into())
            .parent_message("abc".into())
            .model("gpt-5".into())
            .stop_reason("end_turn".into())
            .token_usage(TokenUsage::default());

        let (blob, _) = serialize_grain(&ev).unwrap();
        // Raw msgpack payload starts after the 9-byte header.
        let payload = &blob[9..];
        // Verify compact names appear as-is (short forms, not long forms).
        assert!(has_str(payload, "role"), "role key missing");
        assert!(has_str(payload, "sid2"), "sid2 key missing");
        assert!(has_str(payload, "pmid"), "pmid key missing");
        assert!(has_str(payload, "mdl"), "mdl key missing");
        assert!(has_str(payload, "stopr"), "stopr key missing");
        assert!(has_str(payload, "toku"), "toku key missing");
    }

    #[test]
    fn test_event_no_new_fields_legacy_shape_deserializes() {
        use crate::format::deserialize::deserialize_blob;

        let ev = Event::new("legacy").subject("alice");
        let mut ev = ev;
        ev.common.namespace = Some("ns".into());
        ev.common.user_id = Some("u1".into());
        ev.common.created_at = Some(1_768_471_200_000);

        let (blob, _) = serialize_grain(&ev).unwrap();
        let des = deserialize_blob(&blob).unwrap();
        let back = des.to_event().unwrap();

        assert_eq!(back.content, "legacy");
        assert_eq!(back.subject.as_deref(), Some("alice"));
        assert!(back.role.is_none());
        assert!(back.session_id.is_none());
        assert!(back.content_blocks.is_none());
        assert!(back.token_usage.is_none());
    }

    fn has_str(payload: &[u8], needle: &str) -> bool {
        payload
            .windows(needle.len())
            .any(|w| w == needle.as_bytes())
    }
}
