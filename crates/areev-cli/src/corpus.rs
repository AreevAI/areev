//! Governed corpus writer: selected CAL grains → OpenAI chat JSONL plus the
//! provenance/loss fields Areev can represent and ordinary trainers cannot.

use areev_core::verification::Trust;
use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;

use areev_cal::executor::CalGrainResult;
use serde_json::{json, Map, Value};

/// The full governed export, shared by `areev corpus` and `areev tune
/// --select`: validate the selector is read-only CAL, authorize (the read
/// grant and the registry write are separate capabilities — fail before
/// creating an output file), execute, stream the JSONL to `destination`
/// (`"stdout"` or a file path), and record the immutable export manifest.
pub fn export(
    facade: &areev_cal::AreevFacade,
    selector: &str,
    destination: &str,
    recipient: Option<&str>,
    exported_at_ms: i64,
) -> Result<(CorpusSummary, areev_core::Hash), String> {
    use areev_cal::classify::{classify, StatementClass};
    use areev_cal::executor::CalResultPayload;
    use areev_cal::{CalExecutor, CalExecutorConfig};
    use std::io::BufWriter;

    let parsed = areev_cal::parse(selector).map_err(|e| e.to_string())?;
    if classify(&parsed.statement) != StatementClass::Read {
        return Err("corpus selector must classify as a read-only CAL statement".into());
    }
    facade.authorize_corpus_export().map_err(|e| e.to_string())?;
    let ex = CalExecutor::new(CalExecutorConfig {
        allow_destructive_ops: false,
        ..CalExecutorConfig::default()
    });
    let result = ex.execute(selector, facade).map_err(|e| e.to_string())?;
    let grains = match result.result {
        CalResultPayload::Grains { grains, .. }
        | CalResultPayload::Assembled { grains, .. }
        | CalResultPayload::Formatted { grains, .. }
        | CalResultPayload::MultiFormatted { grains, .. } => grains,
        other => {
            return Err(format!(
                "corpus selector must return grains, got {}",
                serde_json::to_value(other)
                    .ok()
                    .and_then(|v| v.get("type").and_then(|t| t.as_str()).map(str::to_string))
                    .unwrap_or_else(|| "non-grain result".into())
            ));
        }
    };
    let summary = if destination == "stdout" {
        let stdout = std::io::stdout();
        write_jsonl(grains, BufWriter::new(stdout.lock()))?
    } else {
        let file = std::fs::File::create(destination)
            .map_err(|e| format!("{destination}: {e}"))?;
        write_jsonl(grains, BufWriter::new(file))?
    };
    let manifest = facade
        .record_corpus_export(
            selector,
            destination,
            recipient,
            exported_at_ms,
            &summary.subject_fingerprints,
            &summary.source_hashes,
        )
        .map_err(|e| e.to_string())?;
    Ok((summary, manifest))
}

#[derive(Debug, Clone)]
pub struct CorpusSummary {
    pub rows: usize,
    pub source_hashes: Vec<String>,
    pub subject_fingerprints: Vec<String>,
}

fn field<'a>(grain: &'a CalGrainResult, name: &str) -> Option<&'a Value> {
    grain.fields.as_object()?.get(name)
}

fn str_field<'a>(grain: &'a CalGrainResult, name: &str) -> Option<&'a str> {
    field(grain, name)?.as_str()
}

fn subject_fingerprints<'a>(grains: impl IntoIterator<Item = &'a CalGrainResult>) -> Vec<String> {
    let mut out = BTreeSet::new();
    for grain in grains {
        // Session/run ids identify trajectories, not people. They already ride
        // in `binding.trace`; treating them as subjects makes erasure reports
        // stale unrelated corpora when an identifier happens to collide.
        for name in ["user_id", "subject", "subject_id", "data_subject"] {
            if let Some(value) = str_field(grain, name).filter(|s| !s.trim().is_empty()) {
                out.insert(areev_core::authz::subject_fingerprint(value));
            }
        }
    }
    out.into_iter().collect()
}

fn tool_definitions(grains: &[CalGrainResult]) -> Vec<Value> {
    let mut tools = Vec::new();
    for grain in grains {
        if grain.grain_type != "tool" || str_field(grain, "kind") != Some("definition") {
            continue;
        }
        let Some(name) = str_field(grain, "tool_name") else {
            continue;
        };
        let mut function = Map::new();
        function.insert("name".into(), json!(name));
        if let Some(description) = str_field(grain, "tool_description") {
            function.insert("description".into(), json!(description));
        }
        function.insert(
            "parameters".into(),
            field(grain, "input_schema")
                .cloned()
                .unwrap_or_else(|| json!({"type":"object","properties":{}})),
        );
        if let Some(strict) = field(grain, "strict").and_then(Value::as_bool) {
            function.insert("strict".into(), json!(strict));
        }
        tools.push(json!({"type":"function", "function": function}));
    }
    tools.sort_by_key(Value::to_string);
    tools
}

fn ordered_events<'a>(events: &[&'a CalGrainResult]) -> Vec<&'a CalGrainResult> {
    let hashes: BTreeSet<&str> = events.iter().map(|g| g.hash.as_str()).collect();
    let mut remaining: Vec<&CalGrainResult> = events.to_vec();
    remaining.sort_by(|a, b| {
        let at = str_field(a, "created_at")
            .and_then(|s| s.parse::<i64>().ok())
            .or_else(|| field(a, "created_at").and_then(Value::as_i64))
            .unwrap_or(0);
        let bt = str_field(b, "created_at")
            .and_then(|s| s.parse::<i64>().ok())
            .or_else(|| field(b, "created_at").and_then(Value::as_i64))
            .unwrap_or(0);
        at.cmp(&bt).then_with(|| a.hash.cmp(&b.hash))
    });
    let mut ordered = Vec::with_capacity(remaining.len());
    let mut emitted = BTreeSet::new();
    while !remaining.is_empty() {
        let before = remaining.len();
        let mut next = Vec::new();
        for event in remaining {
            let parent = str_field(event, "parent_message_id");
            if parent.is_none_or(|p| !hashes.contains(p) || emitted.contains(p)) {
                emitted.insert(event.hash.as_str());
                ordered.push(event);
            } else {
                next.push(event);
            }
        }
        if next.len() == before {
            // Broken/cyclic parent links must not make an export hang. The
            // deterministic timestamp/hash order is the honest fallback.
            ordered.extend(next);
            break;
        }
        remaining = next;
    }
    ordered
}

/// Zero the loss weight of every tool call whose result came back an error.
///
/// This is the step-level masking the corpus exists to provide. Training on a
/// failed run wholesale measurably underperforms training on its successes
/// alone; masking only the harmful steps beats both. The harmful step is the
/// model's *call*, not the environment's reply — and the two arrive in
/// different events, so the correlation can only run once the session is whole.
fn mask_failed_tool_calls(steps: &mut [Value]) {
    let failed: BTreeSet<String> = steps
        .iter()
        .filter(|s| s.get("kind").and_then(Value::as_str) == Some("tool_result"))
        .filter(|s| s.get("quality").and_then(Value::as_str) == Some("failed"))
        .filter_map(|s| s.get("tool_call_id").and_then(Value::as_str))
        .filter(|id| !id.is_empty())
        .map(str::to_string)
        .collect();
    if failed.is_empty() {
        return;
    }
    for step in steps.iter_mut() {
        if step.get("kind").and_then(Value::as_str) != Some("tool_call") {
            continue;
        }
        let matched = step
            .get("tool_call_id")
            .and_then(Value::as_str)
            .is_some_and(|id| failed.contains(id));
        if !matched {
            continue;
        }
        if let Some(obj) = step.as_object_mut() {
            obj.insert("quality".into(), json!("failed"));
            obj.insert("loss_weight".into(), json!(0.0));
        }
    }
}

fn quality(grain: &CalGrainResult, block_error: bool) -> (&'static str, f64) {
    if block_error {
        return ("failed", 0.0);
    }
    match Trust::from_field(str_field(grain, "verification_status")) {
        Trust::Retracted => ("rejected", 0.0),
        Trust::Contested => ("contested", 0.0),
        Trust::Unverified | Trust::Verified => ("accepted", 1.0),
    }
}

fn push_event_messages(
    event: &CalGrainResult,
    messages: &mut Vec<Value>,
    steps: &mut Vec<Value>,
    elisions: &mut Vec<Value>,
) {
    let Some(role) = str_field(event, "role") else {
        return; // occurrence Event, not a chat turn
    };
    // `capture-stop` records elisions as a top-level extra field; accept the
    // nested spelling too so a host that puts them in `context` still reports
    // them. Reading only `context.elisions` silently emitted an empty list for
    // every corpus this engine produced.
    if let Some(items) = field(event, "elisions")
        .or_else(|| {
            field(event, "context")
                .and_then(Value::as_object)
                .and_then(|c| c.get("elisions"))
        })
        .and_then(Value::as_array)
    {
        for item in items {
            elisions.push(json!({"source_hash": event.hash, "detail": item}));
        }
    }
    let blocks = field(event, "content_blocks").and_then(Value::as_array);
    if blocks.is_none() {
        let message_index = messages.len();
        messages.push(json!({
            "role": role,
            "content": str_field(event, "content").unwrap_or_default(),
        }));
        let (label, base) = quality(event, false);
        let weight = if role == "assistant" { base } else { 0.0 };
        steps.push(json!({
            "source_hash": event.hash,
            "message_index": message_index,
            "segment": 0,
            "kind": "text",
            "quality": label,
            "loss_weight": weight,
        }));
        return;
    }

    let mut text = Vec::new();
    let mut tool_calls = Vec::new();
    let mut deferred_results = Vec::new();
    for (segment, block) in blocks.unwrap().iter().enumerate() {
        match block.get("type").and_then(Value::as_str) {
            Some("text") => {
                text.push(block.get("text").and_then(Value::as_str).unwrap_or_default());
                let (label, base) = quality(event, false);
                steps.push(json!({
                    "source_hash": event.hash,
                    "message_index": messages.len(),
                    "segment": segment,
                    "kind": "text",
                    "quality": label,
                    "loss_weight": if role == "assistant" { base } else { 0.0 },
                }));
            }
            Some("tool_use") => {
                let id = block.get("id").and_then(Value::as_str).unwrap_or_default();
                let name = block.get("name").and_then(Value::as_str).unwrap_or("tool");
                let arguments = block.get("input").cloned().unwrap_or(Value::Null).to_string();
                tool_calls.push(json!({
                    "id": id,
                    "type": "function",
                    "function": {"name": name, "arguments": arguments},
                }));
                let (label, base) = quality(event, false);
                steps.push(json!({
                    "source_hash": event.hash,
                    "message_index": messages.len(),
                    "segment": segment,
                    "kind": "tool_call",
                    // Carried so the failing result can find this step: the
                    // result arrives in a later event, so the harmful call can
                    // only be masked after the whole session is walked.
                    "tool_call_id": id,
                    "quality": label,
                    "loss_weight": if role == "assistant" { base } else { 0.0 },
                }));
            }
            Some("tool_result") => deferred_results.push((segment, block)),
            _ => {}
        }
    }
    if !text.is_empty() || !tool_calls.is_empty() {
        let mut message = Map::new();
        message.insert("role".into(), json!(role));
        message.insert(
            "content".into(),
            if text.is_empty() { Value::Null } else { json!(text.join("\n")) },
        );
        if !tool_calls.is_empty() {
            message.insert("tool_calls".into(), Value::Array(tool_calls));
        }
        messages.push(Value::Object(message));
    }
    for (segment, block) in deferred_results {
        let message_index = messages.len();
        let is_error = block.get("is_error").and_then(Value::as_bool).unwrap_or(false);
        messages.push(json!({
            "role": "tool",
            "tool_call_id": block.get("tool_use_id").and_then(Value::as_str).unwrap_or_default(),
            "content": block.get("content").and_then(Value::as_str).unwrap_or_default(),
        }));
        let (label, _) = quality(event, is_error);
        steps.push(json!({
            "source_hash": event.hash,
            "message_index": message_index,
            "segment": segment,
            "kind": "tool_result",
            "tool_call_id": block.get("tool_use_id").and_then(Value::as_str).unwrap_or_default(),
            "quality": label,
            // An environment observation is never a training target, error or
            // not. The signal a failure carries belongs on the *call* that
            // caused it — see `mask_failed_tool_calls`.
            "loss_weight": 0.0,
        }));
    }
}

/// Stream one JSON object per session/run to `writer`. The selected grains are
/// already materialized by CAL, but no full output string is ever built.
pub fn write_jsonl<W: Write>(
    grains: Vec<CalGrainResult>,
    mut writer: W,
) -> Result<CorpusSummary, String> {
    let source_hashes: Vec<String> = grains.iter().map(|g| g.hash.clone()).collect();
    let fingerprints = subject_fingerprints(&grains);
    let tools = tool_definitions(&grains);
    let mut groups: BTreeMap<String, Vec<&CalGrainResult>> = BTreeMap::new();
    for grain in &grains {
        if grain.grain_type != "event" {
            continue;
        }
        let group = str_field(grain, "session_id")
            .or_else(|| str_field(grain, "run_id"))
            .map(str::to_string)
            .unwrap_or_else(|| format!("ungrouped:{}", grain.hash));
        groups.entry(group).or_default().push(grain);
    }

    let mut rows = 0;
    for (trace, events) in groups {
        let events = ordered_events(&events);
        let row_fingerprints = subject_fingerprints(events.iter().copied());
        let mut messages = Vec::new();
        let mut steps = Vec::new();
        let mut elisions = Vec::new();
        let mut models = BTreeSet::new();
        let mut policies = BTreeSet::new();
        let mut trace_sources = Vec::new();
        for event in events {
            trace_sources.push(event.hash.clone());
            if let Some(model) = str_field(event, "model_id") {
                models.insert(model.to_string());
            }
            if let Some(policy) = field(event, "context")
                .and_then(Value::as_object)
                .and_then(|c| c.get("policy_version"))
                .and_then(Value::as_str)
            {
                policies.insert(policy.to_string());
            }
            push_event_messages(event, &mut messages, &mut steps, &mut elisions);
        }
        if messages.is_empty() {
            continue;
        }
        mask_failed_tool_calls(&mut steps);
        let loss_steps: Vec<Value> = steps
            .iter()
            .filter_map(|step| {
                Some(json!({
                    "message_index": step.get("message_index")?,
                    "segment": step.get("segment")?,
                    "weight": step.get("loss_weight")?,
                }))
            })
            .collect();
        let row = json!({
            "messages": messages,
            "tools": tools.clone(),
            "loss": {"intent": "assistant_completion", "steps": loss_steps},
            "steps": steps,
            "observation_elisions": elisions,
            "binding": {
                "trace": trace,
                "agent_versions": models.into_iter().collect::<Vec<_>>(),
                "policy_versions": policies.into_iter().collect::<Vec<_>>(),
                "data_subject_fingerprints": row_fingerprints,
                "source_hashes": trace_sources,
            },
        });
        serde_json::to_writer(&mut writer, &row).map_err(|e| e.to_string())?;
        writer.write_all(b"\n").map_err(|e| e.to_string())?;
        rows += 1;
    }
    writer.flush().map_err(|e| e.to_string())?;
    Ok(CorpusSummary {
        rows,
        source_hashes,
        subject_fingerprints: fingerprints,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(hash: &str, role: &str, content: Value) -> CalGrainResult {
        CalGrainResult {
            hash: hash.into(),
            grain_type: "event".into(),
            score: 1.0,
            fields: json!({
                "namespace": "caller",
                "session_id": "s1",
                "role": role,
                "subject": "person:7",
                "content": "fallback",
                "content_blocks": content,
                "created_at": 1,
            }),
            score_breakdown: None,
            explanation: None,
            relative_time: None,
            is_deterministic: true,
            contested_by: None,
        }
    }

    #[test]
    fn writes_openai_messages_and_step_masking() {
        let grains = vec![
            event("a", "user", json!([{"type":"text","text":"do it"}])),
            event(
                "b",
                "assistant",
                json!([
                    {"type":"tool_use","id":"c1","name":"shell","input":{"cmd":"false"}},
                    {"type":"tool_result","tool_use_id":"c1","content":"failed","is_error":true}
                ]),
            ),
        ];
        let mut out = Vec::new();
        let summary = write_jsonl(grains, &mut out).unwrap();
        assert_eq!(summary.rows, 1);
        let row: Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(row["messages"][1]["tool_calls"][0]["function"]["name"], "shell");
        assert!(row["steps"]
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s["quality"] == "failed" && s["loss_weight"] == 0.0));
        let fingerprints = row["binding"]["data_subject_fingerprints"].as_array().unwrap();
        assert_eq!(fingerprints.len(), 1);
        assert!(fingerprints.contains(&json!(areev_core::authz::subject_fingerprint(
            "person:7"
        ))));
    }

    #[test]
    fn row_subject_bindings_are_trace_scoped() {
        let mut alice = event("a", "user", json!([{"type":"text","text":"a"}]));
        alice.fields["session_id"] = json!("alice-session");
        alice.fields["subject"] = json!("alice");
        let mut bob = event("b", "user", json!([{"type":"text","text":"b"}]));
        bob.fields["session_id"] = json!("bob-session");
        bob.fields["subject"] = json!("bob");
        let mut out = Vec::new();
        let summary = write_jsonl(vec![alice, bob], &mut out).unwrap();
        assert_eq!(summary.subject_fingerprints.len(), 2, "registry keeps the union");
        let rows: Vec<Value> = String::from_utf8(out)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(rows.len(), 2);
        for row in rows {
            let trace = row["binding"]["trace"].as_str().unwrap();
            let expected = if trace == "alice-session" { "alice" } else { "bob" };
            assert_eq!(
                row["binding"]["data_subject_fingerprints"],
                json!([areev_core::authz::subject_fingerprint(expected)])
            );
        }
    }
}
