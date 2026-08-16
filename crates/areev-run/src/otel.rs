//! OpenTelemetry export (Wave 5): a §6.10 observer that turns run events
//! into OTLP/HTTP **JSON** spans — hand-rolled over `std::net::TcpStream`
//! (dependency-light policy: no HTTP client crate, no OTel SDK). One POST
//! per run, flushed at `RunFinished`, so a collector outage costs one
//! buffered batch, never scheduler time — the bus already guarantees the
//! run never waits on an observer.
//!
//! Span model: one ROOT span per run (`areev.run`, the full wall interval),
//! one child span per journaled effect (`node` name; task_path/attempt/
//! effect_seq as attributes; error status on failed effects). Trace id =
//! first 16 bytes of sha256(run_id) — stable across resumes, so a resumed
//! run's spans land in the SAME trace.
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

/// The OTLP/JSON payload (`resourceSpans` shape, span kind INTERNAL).
/// Public-for-tests: the fixture asserts the exact structure without a
/// socket.
pub fn build_otlp(run_id: &str, outcome: &str, events: &[(RunEvent, u128)]) -> serde_json::Value {
    use serde_json::json;
    let tid = trace_id(run_id);
    let root_sid = span_id(run_id);
    let start = events.first().map(|(_, t)| *t).unwrap_or_else(now_ns);
    let end = events.last().map(|(_, t)| *t).unwrap_or(start);

    let mut spans = Vec::new();
    // Effect spans: NodeDispatched opens, the matching EffectSettled closes.
    for (ev, at) in events {
        if let RunEvent::NodeDispatched { superstep, node, task_path, attempt, effect_seq } = ev {
            let key = format!("{run_id}/{task_path}/{node}/{attempt}/{effect_seq}");
            let settle = events.iter().find_map(|(e2, t2)| match e2 {
                RunEvent::EffectSettled {
                    node: n2, task_path: p2, attempt: a2, effect_seq: s2, ok, ..
                } if n2 == node && p2 == task_path && a2 == attempt && s2 == effect_seq => {
                    Some((*t2, *ok))
                }
                _ => None,
            });
            let (end_ns, ok) = settle.unwrap_or((*at, true));
            spans.push(json!({
                "traceId": tid,
                "spanId": span_id(&key),
                "parentSpanId": root_sid,
                "name": node,
                "kind": 1,
                "startTimeUnixNano": at.to_string(),
                "endTimeUnixNano": end_ns.to_string(),
                "attributes": [
                    {"key": "areev.superstep", "value": {"intValue": superstep.to_string()}},
                    {"key": "areev.task_path", "value": {"stringValue": task_path}},
                    {"key": "areev.attempt", "value": {"intValue": attempt.to_string()}},
                    {"key": "areev.effect_seq", "value": {"intValue": effect_seq.to_string()}},
                ],
                "status": if ok { json!({"code": 1}) } else { json!({"code": 2, "message": "effect failed"}) },
            }));
        }
    }
    spans.push(json!({
        "traceId": tid,
        "spanId": root_sid,
        "name": "areev.run",
        "kind": 1,
        "startTimeUnixNano": start.to_string(),
        "endTimeUnixNano": end.to_string(),
        "attributes": [
            {"key": "areev.run_id", "value": {"stringValue": run_id}},
            {"key": "areev.outcome", "value": {"stringValue": outcome}},
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

    #[test]
    fn otlp_payload_carries_root_and_effect_spans() {
        let events = vec![
            (RunEvent::RunStarted { run_id: "r1".into() }, 1_000),
            (
                RunEvent::NodeDispatched {
                    superstep: 1, node: "greet".into(), task_path: String::new(),
                    attempt: 1, effect_seq: 0,
                },
                2_000,
            ),
            (
                RunEvent::EffectSettled {
                    superstep: 1, node: "greet".into(), task_path: String::new(),
                    attempt: 1, effect_seq: 0, ok: false,
                },
                3_000,
            ),
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
        assert_eq!(effect["name"], "greet");
        assert_eq!(effect["status"]["code"], 2, "failed effect exports error status");
        assert_eq!(effect["endTimeUnixNano"], "3000");
        let root = &spans[1];
        assert_eq!(root["name"], "areev.run");
        assert_eq!(effect["parentSpanId"], root["spanId"]);
        assert_eq!(effect["traceId"], root["traceId"]);
        // Stable trace identity: a resume exports into the same trace.
        assert_eq!(root["traceId"], build_otlp("r1", "x", &[])["resourceSpans"][0]["scopeSpans"][0]["spans"][0]["traceId"]);
    }
}
