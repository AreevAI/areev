//! OpenTelemetry export (Wave 5): a §6.10 observer that turns run events
//! into OTLP/HTTP **JSON** spans — hand-rolled over `std::net::TcpStream`
//! (dependency-light policy: no HTTP client crate, no OTel SDK). One POST
//! per run, flushed at `RunFinished`, so a collector outage costs one
//! buffered batch, never scheduler time — the bus already guarantees the
//! run never waits on an observer.
//!
//! Span model: one ROOT span per run (`areev.run`, the full wall interval),
//! one child span per journaled effect (task_path/attempt/effect_seq as
//! attributes; error status on failed effects), and — for an abstract node —
//! one synthesized `invoke_agent` span in between, so the node's model turns
//! and the tools those turns called hang off ONE agent invocation instead of
//! sitting flat under the run. Trace id = first 16 bytes of sha256(run_id) —
//! stable across resumes, so a resumed run's spans land in the SAME trace.
//!
//! **OpenTelemetry GenAI semantic conventions.** Effect spans carry the
//! current `gen_ai.*` attributes, so a GenAI-aware backend (Grafana, Langfuse,
//! Arize, Datadog LLM Observability, …) classifies model spend, tool calls
//! and finish reasons with **no Areev-specific code** — the operation is
//! `chat` / `execute_tool` / `invoke_agent`, the span name is semconv's
//! `{operation} {target}`, and a `chat` span is a CLIENT span because the
//! model is a remote peer. The internal argument for aligning here rather
//! than inventing a vocabulary is `docs/areev-adaptive-agents-proposal.md`
//! §"Observability".
//!
//! The `areev.*` attributes stay on every span beside them. They are the
//! run-provenance join — superstep, task_path, attempt, effect_seq — and no
//! GenAI attribute expresses any of it: `gen_ai.*` says what the model did,
//! `areev.*` says which journaled effect it was, which is what makes a span
//! addressable back into the journal (`areev run-trace`).
//!
//! Everything a span says arrives inside a [`RunEvent`], never from the
//! store: this observer runs on the bus's own thread while the driver holds
//! the memory's single writer, so a journal read from here is not merely
//! slow, it is impossible.
//!
//! Endpoint: `http://host:port` (the standard collector `:4318`; the
//! `/v1/traces` path is appended when absent). TLS is the collector's job
//! in this profile — point at a local sidecar/agent, which is the normal
//! OTel deployment shape anyway.

use crate::stream::{RunEvent, RunObserver};
use sha2::Digest;
use std::io::{Read, Write};
use std::sync::Mutex;

pub struct OtelObserver {
    host: String,
    port: u16,
    path: String,
    state: Mutex<OtelState>,
}

#[derive(Default)]
struct OtelState {
    /// Per-run event buffers, attributed to the INNERMOST live run (a
    /// subgraph child runs inline between its parent's events; a flat
    /// buffer would let the child's RunFinished flush — and mis-trace —
    /// the parent's spans). The observer thread stamps time — export is
    /// observational; journaled clocks stay the replay truth.
    stack: Vec<(String, Vec<(RunEvent, u128)>)>,
}

fn now_ns() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
}

fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        use std::fmt::Write as _;
        let _ = write!(s, "{b:02x}");
    }
    s
}

fn trace_id(run_id: &str) -> String {
    let d = sha2::Sha256::digest(run_id.as_bytes());
    hex(&d[..16])
}

fn span_id(seed: &str) -> String {
    let d = sha2::Sha256::digest(seed.as_bytes());
    hex(&d[..8])
}

impl OtelObserver {
    /// Parse `http://host:port[/path]`. Anything else is refused loudly —
    /// silently exporting nowhere would be worse than an error.
    pub fn new(endpoint: &str) -> Result<Self, String> {
        let rest = endpoint
            .strip_prefix("http://")
            .ok_or_else(|| format!("--otel-endpoint must be http:// (got {endpoint}); terminate TLS at a local collector"))?;
        let (authority, path) = match rest.split_once('/') {
            Some((a, p)) => (a, format!("/{p}")),
            None => (rest, "/v1/traces".to_string()),
        };
        let (host, port) = match authority.rsplit_once(':') {
            Some((h, p)) => (h.to_string(), p.parse::<u16>().map_err(|_| format!("bad port in {endpoint}"))?),
            None => (authority.to_string(), 4318),
        };
        if host.is_empty() {
            return Err(format!("no host in {endpoint}"));
        }
        Ok(OtelObserver { host, port, path, state: Mutex::new(OtelState::default()) })
    }

    fn flush(&self, run_id: &str, outcome: &str) {
        let events = {
            let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
            match st.stack.iter().rposition(|(id, _)| id == run_id) {
                Some(pos) => st.stack.remove(pos).1,
                None => return,
            }
        };
        let payload = build_otlp(run_id, outcome, &events);
        let body = payload.to_string();
        // Fire-and-forget with a bounded timeout: telemetry must never wedge
        // the observer thread (which would fill the bus and start dropping).
        let addr = format!("{}:{}", self.host, self.port);
        let req = format!(
            "POST {} HTTP/1.1\r\nHost: {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            self.path, self.host, body.len(), body
        );
        let timeout = std::time::Duration::from_secs(5);
        // connect_timeout, never a bare connect: a blackholed collector
        // route must cost seconds, not the OS's minutes — EventBus::drop
        // joins this thread on drive's return.
        let resolved = std::net::ToSocketAddrs::to_socket_addrs(&addr.as_str())
            .ok()
            .and_then(|mut it| it.next());
        let Some(sockaddr) = resolved else { return };
        if let Ok(mut stream) = std::net::TcpStream::connect_timeout(&sockaddr, timeout) {
            let _ = stream.set_write_timeout(Some(timeout));
            let _ = stream.set_read_timeout(Some(timeout));
            if stream.write_all(req.as_bytes()).is_ok() {
                let mut buf = [0u8; 512];
                let _ = stream.read(&mut buf); // drain the status line; best-effort
            }
        }
    }
}

impl RunObserver for OtelObserver {
    fn event(&self, ev: &RunEvent) {
        {
            let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
            match ev {
                RunEvent::RunStarted { run_id } | RunEvent::RunResumed { run_id }
                    if !st.stack.iter().any(|(id, _)| id == run_id) =>
                {
                    st.stack.push((run_id.clone(), Vec::new()));
                }
                _ => {}
            }
            // Attribute to the innermost live run (last on the stack).
            if let Some((_, events)) = st.stack.last_mut() {
                events.push((ev.clone(), now_ns()));
            } else {
                st.stack.push((String::new(), vec![(ev.clone(), now_ns())]));
            }
        }
        if let RunEvent::RunFinished { run_id, outcome, .. } = ev {
            self.flush(run_id, outcome);
        }
    }
}

// ---- OTLP/JSON attribute constructors --------------------------------------

fn s_attr(key: &str, v: &str) -> serde_json::Value {
    serde_json::json!({"key": key, "value": {"stringValue": v}})
}

fn i_attr(key: &str, v: u64) -> serde_json::Value {
    // OTLP/JSON encodes int64 as a STRING (proto3 JSON mapping) — a bare
    // number silently loses precision in JavaScript collectors.
    serde_json::json!({"key": key, "value": {"intValue": v.to_string()}})
}

fn d_attr(key: &str, v: f64) -> serde_json::Value {
    serde_json::json!({"key": key, "value": {"doubleValue": v}})
}

fn sarr_attr(key: &str, vs: &[&str]) -> serde_json::Value {
    serde_json::json!({
        "key": key,
        "value": {"arrayValue": {"values":
            vs.iter().map(|v| serde_json::json!({"stringValue": v})).collect::<Vec<_>>()}},
    })
}

/// OTel span kinds we emit. A model call reaches a remote peer, so it is a
/// CLIENT span; everything else happens inside this process.
const KIND_INTERNAL: u8 = 1;
const KIND_CLIENT: u8 = 3;

/// Deterministic id for the synthesized agent span covering one abstract
/// node's activation. Keyed by attempt, not by node: a retried node is a
/// second invocation of the agent, not a longer first one.
fn agent_key(run_id: &str, task_path: &str, node: &str, attempt: u32) -> String {
    format!("agent:{run_id}/{task_path}/{node}/{attempt}")
}

/// The OTLP/JSON payload (`resourceSpans` shape). Public-for-tests: the
/// fixtures assert the exact structure — attribute set, span names, span
/// kinds — without a socket.
pub fn build_otlp(run_id: &str, outcome: &str, events: &[(RunEvent, u128)]) -> serde_json::Value {
    use serde_json::json;
    let tid = trace_id(run_id);
    let root_sid = span_id(run_id);
    let start = events.first().map(|(_, t)| *t).unwrap_or_else(now_ns);
    let end = events.last().map(|(_, t)| *t).unwrap_or(start);

    // Pass 1: the agent invocations, collected from the dispatches that
    // declared one. Keyed by the agent key so a node's model turns and the
    // tools they called land in the same bucket; the interval is the union of
    // its effects', so a parent always encloses its children.
    struct Agent {
        name: String,
        start: u128,
        end: u128,
        superstep: u64,
        task_path: String,
        attempt: u32,
        model: Option<String>,
        provider: Option<String>,
        ok: bool,
    }
    let mut agents: std::collections::BTreeMap<String, Agent> = Default::default();

    let mut spans = Vec::new();
    // Effect spans: NodeDispatched opens, the matching EffectSettled closes.
    for (ev, at) in events {
        let RunEvent::NodeDispatched {
            superstep,
            node,
            task_path,
            attempt,
            effect_seq,
            effect_kind,
            executor_kind,
            agent_name,
            tool_name,
            tool_call_id,
            model,
            provider,
            max_tokens,
            temperature,
        } = ev
        else {
            continue;
        };
        let key = format!("{run_id}/{task_path}/{node}/{attempt}/{effect_seq}");
        let settle = events.iter().find_map(|(e2, t2)| match e2 {
            RunEvent::EffectSettled {
                node: n2,
                task_path: p2,
                attempt: a2,
                effect_seq: s2,
                ok,
                input_tokens,
                output_tokens,
                finish_reason,
                error_type,
                ..
            } if n2 == node && p2 == task_path && a2 == attempt && s2 == effect_seq => Some((
                *t2,
                *ok,
                *input_tokens,
                *output_tokens,
                finish_reason.clone(),
                error_type.clone(),
            )),
            _ => None,
        });
        let (end_ns, ok, in_tok, out_tok, finish_reason, error_type) =
            settle.unwrap_or((*at, true, None, None, None, None));

        // Classification. `chat` is any model turn; `execute_tool` is a tool
        // an agent's model asked for — a plain bound workflow node is not a
        // GenAI operation and deliberately stays an unlabeled INTERNAL span
        // rather than being dressed up as one.
        let is_chat = effect_kind.as_deref() == Some("llm");
        let is_tool_call = effect_kind.as_deref() == Some("tool") && agent_name.is_some();
        let operation = if is_chat {
            Some("chat")
        } else if is_tool_call {
            Some("execute_tool")
        } else {
            None
        };
        // semconv span naming: `{operation} {target}`, and the operation
        // alone when the target is unknown.
        let name = match (operation, model.as_deref(), tool_name.as_deref()) {
            (Some("chat"), Some(m), _) => format!("chat {m}"),
            (Some("chat"), None, _) => "chat".to_string(),
            (Some("execute_tool"), _, Some(t)) => format!("execute_tool {t}"),
            (Some("execute_tool"), _, None) => "execute_tool".to_string(),
            _ => node.clone(),
        };

        let mut attrs = vec![
            i_attr("areev.superstep", *superstep),
            s_attr("areev.task_path", task_path),
            i_attr("areev.attempt", *attempt as u64),
            i_attr("areev.effect_seq", *effect_seq as u64),
        ];
        if let Some(k) = effect_kind {
            attrs.push(s_attr("areev.effect_kind", k));
        }
        if let Some(k) = executor_kind {
            attrs.push(s_attr("areev.executor_kind", k));
        }
        if let Some(op) = operation {
            attrs.push(s_attr("gen_ai.operation.name", op));
        }
        if operation.is_some() {
            if let Some(p) = provider {
                attrs.push(s_attr("gen_ai.provider.name", p));
            }
        }
        if is_chat {
            if let Some(m) = model {
                attrs.push(s_attr("gen_ai.request.model", m));
                // The provider echoes the model it actually served in its
                // response body, but that reply is parsed in the executor pool
                // and only its text/tool_calls/stop_reason reach the journal,
                // so the request model is the honest best answer here. When a
                // provider aliases (`gpt-4o` → a dated build) the two differ,
                // and this attribute will say so the day the pool carries it
                // through.
                attrs.push(s_attr("gen_ai.response.model", m));
            }
            if let Some(mt) = max_tokens {
                attrs.push(i_attr("gen_ai.request.max_tokens", *mt as u64));
            }
            if let Some(t) = temperature {
                attrs.push(d_attr("gen_ai.request.temperature", *t));
            }
            if let Some(v) = in_tok {
                attrs.push(i_attr("gen_ai.usage.input_tokens", v));
            }
            if let Some(v) = out_tok {
                attrs.push(i_attr("gen_ai.usage.output_tokens", v));
            }
            // An ARRAY even for one reason: semconv types it that way, and a
            // backend that unpacks it must not have to special-case us.
            if let Some(fr) = &finish_reason {
                attrs.push(sarr_attr("gen_ai.response.finish_reasons", &[fr.as_str()]));
            }
            attrs.push(s_attr("gen_ai.conversation.id", run_id));
        }
        if is_tool_call {
            if let Some(t) = tool_name {
                attrs.push(s_attr("gen_ai.tool.name", t));
            }
            // The MODEL's call id — `PendingToolCall::model_call_id`, the one
            // the transcript's tool_result addresses. Areev's own
            // `JournalKey::tool_call_id()` is a different identifier and is
            // NOT what semconv means here.
            if let Some(id) = tool_call_id {
                attrs.push(s_attr("gen_ai.tool.call.id", id));
            }
            attrs.push(s_attr("gen_ai.tool.type", "function"));
        }
        // No `gen_ai.usage.cost`: Core prices nothing (`usd_micros` is
        // always 0), and an always-zero cost attribute reads as "this run was
        // free" rather than "nobody priced it".
        let mut parent = root_sid.clone();
        if let Some(agent) = agent_name {
            let ak = agent_key(run_id, task_path, node, *attempt);
            parent = span_id(&ak);
            attrs.push(s_attr("gen_ai.agent.name", agent));
            attrs.push(s_attr("gen_ai.agent.id", &ak));
            let e = agents.entry(ak).or_insert_with(|| Agent {
                name: agent.clone(),
                start: *at,
                end: end_ns,
                superstep: *superstep,
                task_path: task_path.clone(),
                attempt: *attempt,
                model: None,
                provider: None,
                ok: true,
            });
            e.start = e.start.min(*at);
            e.end = e.end.max(end_ns);
            e.ok &= ok;
            if is_chat && e.model.is_none() {
                e.model.clone_from(model);
            }
            if e.provider.is_none() {
                e.provider.clone_from(provider);
            }
        }
        if let Some(et) = &error_type {
            attrs.push(s_attr("error.type", et));
        }

        spans.push(json!({
            "traceId": tid,
            "spanId": span_id(&key),
            "parentSpanId": parent,
            "name": name,
            "kind": if is_chat { KIND_CLIENT } else { KIND_INTERNAL },
            "startTimeUnixNano": at.to_string(),
            "endTimeUnixNano": end_ns.to_string(),
            "attributes": attrs,
            "status": if ok { json!({"code": 1}) } else { json!({"code": 2, "message": "effect failed"}) },
        }));
    }

    // Pass 2: one `invoke_agent` span per abstract-node activation, between
    // the run and its effects.
    for (ak, a) in &agents {
        let mut attrs = vec![
            i_attr("areev.superstep", a.superstep),
            s_attr("areev.task_path", &a.task_path),
            i_attr("areev.attempt", a.attempt as u64),
            s_attr("gen_ai.operation.name", "invoke_agent"),
            s_attr("gen_ai.agent.name", &a.name),
            s_attr("gen_ai.agent.id", ak),
            s_attr("gen_ai.conversation.id", run_id),
        ];
        if let Some(p) = &a.provider {
            attrs.push(s_attr("gen_ai.provider.name", p));
        }
        if let Some(m) = &a.model {
            attrs.push(s_attr("gen_ai.request.model", m));
        }
        // Usage is deliberately NOT summed onto this span: a backend that
        // rolls children up would then count every token twice.
        spans.push(json!({
            "traceId": tid,
            "spanId": span_id(ak),
            "parentSpanId": root_sid,
            "name": format!("invoke_agent {}", a.name),
            "kind": KIND_INTERNAL,
            "startTimeUnixNano": a.start.to_string(),
            "endTimeUnixNano": a.end.to_string(),
            "attributes": attrs,
            "status": if a.ok { json!({"code": 1}) } else { json!({"code": 2, "message": "agent node failed"}) },
        }));
    }

    spans.push(json!({
        "traceId": tid,
        "spanId": root_sid,
        "name": "areev.run",
        "kind": KIND_INTERNAL,
        "startTimeUnixNano": start.to_string(),
        "endTimeUnixNano": end.to_string(),
        "attributes": [
            s_attr("areev.run_id", run_id),
            s_attr("areev.outcome", outcome),
            // The run IS the conversation: one journal, one transcript, and a
            // resume continues both — which is why the trace id derives from
            // the run id too.
            s_attr("gen_ai.conversation.id", run_id),
        ],
        "status": if outcome.contains("Completed") { json!({"code": 1}) } else { json!({"code": 2, "message": outcome}) },
    }));

    json!({
        "resourceSpans": [{
            "resource": {"attributes": [
                {"key": "service.name", "value": {"stringValue": "areev-run"}},
            ]},
            "scopeSpans": [{
                "scope": {"name": "areev-run", "version": env!("CARGO_PKG_VERSION")},
                "spans": spans,
            }],
        }],
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stream::RunEvent;

    #[test]
    fn endpoint_parsing_is_strict() {
        let o = OtelObserver::new("http://localhost:4318").unwrap();
        assert_eq!((o.host.as_str(), o.port, o.path.as_str()), ("localhost", 4318, "/v1/traces"));
        let o = OtelObserver::new("http://col.internal/custom/traces").unwrap();
        assert_eq!((o.port, o.path.as_str()), (4318, "/custom/traces"));
        assert!(OtelObserver::new("https://secure:4318").is_err(), "TLS is the collector's job");
        assert!(OtelObserver::new("localhost").is_err());
    }

    /// Read one attribute off a span, as a string, whatever OTLP value shape
    /// it uses — the fixtures assert MEANING, not encoding.
    fn attr(span: &serde_json::Value, key: &str) -> Option<String> {
        span["attributes"].as_array()?.iter().find(|a| a["key"] == key).map(|a| {
            let v = &a["value"];
            v.get("stringValue")
                .and_then(|s| s.as_str())
                .map(str::to_string)
                .or_else(|| v.get("intValue").and_then(|s| s.as_str()).map(str::to_string))
                .or_else(|| v.get("doubleValue").map(|d| d.to_string()))
                .unwrap_or_else(|| v.to_string())
        })
    }

    fn span_named<'a>(p: &'a serde_json::Value, name: &str) -> &'a serde_json::Value {
        p["resourceSpans"][0]["scopeSpans"][0]["spans"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["name"] == name)
            .unwrap_or_else(|| panic!("no span named {name}"))
    }

    /// A dispatch with no GenAI detail — a plain bound workflow node.
    fn plain_dispatch(node: &str, effect_seq: u32) -> RunEvent {
        RunEvent::NodeDispatched {
            superstep: 1,
            node: node.into(),
            task_path: String::new(),
            attempt: 1,
            effect_seq,
            effect_kind: Some("tool".into()),
            executor_kind: Some("host".into()),
            agent_name: None,
            tool_name: Some(node.into()),
            tool_call_id: None,
            model: None,
            provider: None,
            max_tokens: None,
            temperature: None,
        }
    }

    fn settled(node: &str, effect_seq: u32, ok: bool) -> RunEvent {
        RunEvent::EffectSettled {
            superstep: 1,
            node: node.into(),
            task_path: String::new(),
            attempt: 1,
            effect_seq,
            ok,
            input_tokens: None,
            output_tokens: None,
            finish_reason: None,
            error_type: (!ok).then(|| "executor_error".to_string()),
        }
    }

    #[test]
    fn otlp_payload_carries_root_and_effect_spans() {
        let events = vec![
            (RunEvent::RunStarted { run_id: "r1".into() }, 1_000),
            (plain_dispatch("greet", 0), 2_000),
            (settled("greet", 0, false), 3_000),
            (
                RunEvent::RunFinished {
                    run_id: "r1".into(), outcome: "Failed".into(), dropped_events: 0,
                },
                4_000,
            ),
        ];
        let p = build_otlp("r1", "Failed", &events);
        let spans = p["resourceSpans"][0]["scopeSpans"][0]["spans"].as_array().unwrap();
        assert_eq!(spans.len(), 2, "one effect + the root");
        let effect = &spans[0];
        // A plain bound node is NOT a GenAI operation: it keeps the node name
        // and gains no gen_ai attribute.
        assert_eq!(effect["name"], "greet");
        assert_eq!(effect["kind"], 1);
        assert_eq!(attr(effect, "gen_ai.operation.name"), None);
        assert_eq!(attr(effect, "error.type").as_deref(), Some("executor_error"));
        assert_eq!(effect["status"]["code"], 2, "failed effect exports error status");
        assert_eq!(effect["endTimeUnixNano"], "3000");
        let root = &spans[1];
        assert_eq!(root["name"], "areev.run");
        assert_eq!(effect["parentSpanId"], root["spanId"]);
        assert_eq!(effect["traceId"], root["traceId"]);
        // Stable trace identity: a resume exports into the same trace.
        assert_eq!(root["traceId"], build_otlp("r1", "x", &[])["resourceSpans"][0]["scopeSpans"][0]["spans"][0]["traceId"]);
    }

    /// The run-provenance join must survive the GenAI work: every one of the
    /// four `areev.*` attributes an effect span carried before is still there,
    /// because nothing in `gen_ai.*` can say which journaled effect a span is.
    #[test]
    fn areev_provenance_attributes_survive_on_every_effect_span() {
        let events = vec![(plain_dispatch("greet", 3), 2_000), (settled("greet", 3, true), 3_000)];
        let p = build_otlp("r1", "Completed", &events);
        let effect = &p["resourceSpans"][0]["scopeSpans"][0]["spans"][0];
        assert_eq!(attr(effect, "areev.superstep").as_deref(), Some("1"));
        assert_eq!(attr(effect, "areev.task_path").as_deref(), Some(""));
        assert_eq!(attr(effect, "areev.attempt").as_deref(), Some("1"));
        assert_eq!(attr(effect, "areev.effect_seq").as_deref(), Some("3"));
        let root = span_named(&p, "areev.run");
        assert_eq!(attr(root, "areev.run_id").as_deref(), Some("r1"));
        assert_eq!(attr(root, "areev.outcome").as_deref(), Some("Completed"));
    }

    /// One abstract node: a model turn, the tool it called, a closing turn.
    fn agent_events() -> Vec<(RunEvent, u128)> {
        let turn = |seq: u32| RunEvent::NodeDispatched {
            superstep: 1,
            node: "summarize".into(),
            task_path: String::new(),
            attempt: 1,
            effect_seq: seq,
            effect_kind: Some("llm".into()),
            executor_kind: Some("abstract".into()),
            agent_name: Some("summarize".into()),
            tool_name: None,
            tool_call_id: None,
            model: Some("claude-sonnet-4".into()),
            provider: Some("anthropic".into()),
            max_tokens: Some(1024),
            temperature: Some(0.0),
        };
        vec![
            (RunEvent::RunStarted { run_id: "a1".into() }, 1_000),
            (turn(0), 2_000),
            (
                RunEvent::EffectSettled {
                    superstep: 1,
                    node: "summarize".into(),
                    task_path: String::new(),
                    attempt: 1,
                    effect_seq: 0,
                    ok: true,
                    input_tokens: Some(10),
                    output_tokens: Some(5),
                    finish_reason: Some("tool_use".into()),
                    error_type: None,
                },
                3_000,
            ),
            (
                RunEvent::NodeDispatched {
                    superstep: 1,
                    node: "summarize".into(),
                    task_path: String::new(),
                    attempt: 1,
                    effect_seq: 1,
                    effect_kind: Some("tool".into()),
                    executor_kind: Some("host".into()),
                    agent_name: Some("summarize".into()),
                    tool_name: Some("fetch".into()),
                    tool_call_id: Some("call_1".into()),
                    model: None,
                    provider: Some("anthropic".into()),
                    max_tokens: None,
                    temperature: None,
                },
                4_000,
            ),
            (settled("summarize", 1, true), 5_000),
            (turn(2), 6_000),
            (
                RunEvent::EffectSettled {
                    superstep: 1,
                    node: "summarize".into(),
                    task_path: String::new(),
                    attempt: 1,
                    effect_seq: 2,
                    ok: true,
                    input_tokens: Some(20),
                    output_tokens: Some(7),
                    finish_reason: Some("end_turn".into()),
                    error_type: None,
                },
                7_000,
            ),
        ]
    }

    #[test]
    fn chat_span_carries_the_genai_attribute_set() {
        let p = build_otlp("a1", "Completed", &agent_events());
        let chat = span_named(&p, "chat claude-sonnet-4");
        assert_eq!(chat["kind"], 3, "a model call is a CLIENT span");
        assert_eq!(attr(chat, "gen_ai.operation.name").as_deref(), Some("chat"));
        assert_eq!(attr(chat, "gen_ai.provider.name").as_deref(), Some("anthropic"));
        assert_eq!(attr(chat, "gen_ai.request.model").as_deref(), Some("claude-sonnet-4"));
        assert_eq!(attr(chat, "gen_ai.response.model").as_deref(), Some("claude-sonnet-4"));
        assert_eq!(attr(chat, "gen_ai.request.max_tokens").as_deref(), Some("1024"));
        assert_eq!(attr(chat, "gen_ai.request.temperature").as_deref(), Some("0.0"));
        assert_eq!(attr(chat, "gen_ai.usage.input_tokens").as_deref(), Some("10"));
        assert_eq!(attr(chat, "gen_ai.usage.output_tokens").as_deref(), Some("5"));
        assert_eq!(attr(chat, "gen_ai.agent.name").as_deref(), Some("summarize"));
        assert_eq!(attr(chat, "gen_ai.conversation.id").as_deref(), Some("a1"));
        // finish_reasons is an ARRAY even with one value.
        let fr = chat["attributes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|a| a["key"] == "gen_ai.response.finish_reasons")
            .unwrap();
        assert_eq!(fr["value"]["arrayValue"]["values"][0]["stringValue"], "tool_use");
        // The provenance join rides along.
        assert_eq!(attr(chat, "areev.effect_seq").as_deref(), Some("0"));
        // Cost is never claimed: Core prices nothing.
        assert_eq!(attr(chat, "gen_ai.usage.cost"), None);
    }

    #[test]
    fn execute_tool_span_uses_the_models_call_id() {
        let p = build_otlp("a1", "Completed", &agent_events());
        let tool = span_named(&p, "execute_tool fetch");
        assert_eq!(tool["kind"], 1);
        assert_eq!(attr(tool, "gen_ai.operation.name").as_deref(), Some("execute_tool"));
        assert_eq!(attr(tool, "gen_ai.tool.name").as_deref(), Some("fetch"));
        assert_eq!(attr(tool, "gen_ai.tool.type").as_deref(), Some("function"));
        // The MODEL's id, not the journal-key digest.
        assert_eq!(attr(tool, "gen_ai.tool.call.id").as_deref(), Some("call_1"));
        assert_eq!(attr(tool, "gen_ai.provider.name").as_deref(), Some("anthropic"));
        // A tool span makes no model request, so it claims none.
        assert_eq!(attr(tool, "gen_ai.request.model"), None);
    }

    #[test]
    fn invoke_agent_span_parents_the_nodes_turns_and_tools() {
        let p = build_otlp("a1", "Completed", &agent_events());
        let agent = span_named(&p, "invoke_agent summarize");
        assert_eq!(agent["kind"], 1);
        assert_eq!(attr(agent, "gen_ai.operation.name").as_deref(), Some("invoke_agent"));
        assert_eq!(attr(agent, "gen_ai.agent.name").as_deref(), Some("summarize"));
        assert_eq!(
            attr(agent, "gen_ai.agent.id").as_deref(),
            Some("agent:a1//summarize/1"),
            "the agent id is the deterministic per-attempt key"
        );
        assert_eq!(attr(agent, "gen_ai.request.model").as_deref(), Some("claude-sonnet-4"));
        // It encloses its children and hangs off the run.
        assert_eq!(agent["startTimeUnixNano"], "2000");
        assert_eq!(agent["endTimeUnixNano"], "7000");
        let root = span_named(&p, "areev.run");
        assert_eq!(agent["parentSpanId"], root["spanId"]);
        for child in ["chat claude-sonnet-4", "execute_tool fetch"] {
            assert_eq!(
                span_named(&p, child)["parentSpanId"],
                agent["spanId"],
                "{child} must hang off the agent, not the run"
            );
        }
        // Never summed: a rollup would double-count every token.
        assert_eq!(attr(agent, "gen_ai.usage.input_tokens"), None);
    }

    /// A dispatch from a subscriber that carries no GenAI detail at all (the
    /// pre-1.6 shape) still exports — the mapping is all-optional.
    #[test]
    fn a_dispatch_without_genai_detail_still_exports() {
        let events = vec![
            (
                RunEvent::NodeDispatched {
                    superstep: 2,
                    node: "step".into(),
                    task_path: "p/0000".into(),
                    attempt: 1,
                    effect_seq: 0,
                    effect_kind: None,
                    executor_kind: None,
                    agent_name: None,
                    tool_name: None,
                    tool_call_id: None,
                    model: None,
                    provider: None,
                    max_tokens: None,
                    temperature: None,
                },
                1_000,
            ),
            (
                RunEvent::EffectSettled {
                    superstep: 2,
                    node: "step".into(),
                    task_path: "p/0000".into(),
                    attempt: 1,
                    effect_seq: 0,
                    ok: true,
                    input_tokens: None,
                    output_tokens: None,
                    finish_reason: None,
                    error_type: None,
                },
                2_000,
            ),
        ];
        let p = build_otlp("r2", "Completed", &events);
        let spans = p["resourceSpans"][0]["scopeSpans"][0]["spans"].as_array().unwrap();
        assert_eq!(spans.len(), 2);
        assert_eq!(spans[0]["name"], "step");
        assert_eq!(attr(&spans[0], "areev.task_path").as_deref(), Some("p/0000"));
    }
}
