//! An in-process fake `/v1/systemone` decision server for the conformance
//! cases that drive a real `DecisionRerank` → `areev_llm` HTTP chain. Copied
//! from `areev-llm/tests/decide.rs`: a hand-rolled HTTP/1.1 stub on a std
//! `TcpListener`, one thread per connection, that reads each request body to
//! `Content-Length` before answering (an unread body turns the close into an
//! RST, which on macOS discards the reply — `areev-testing` rule 6).

use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};

use serde_json::Value;

/// What the script answers: an HTTP status and a JSON body.
pub struct Reply {
    pub status: u16,
    pub body: String,
}

impl Reply {
    pub fn ok(body: Value) -> Self {
        Reply { status: 200, body: body.to_string() }
    }
    pub fn status(code: u16, body: &str) -> Self {
        Reply { status: code, body: body.to_string() }
    }
}

/// A running fake. `url` is its base (`http://127.0.0.1:<port>`); `bodies`
/// holds every request body it received, in order.
pub struct FakeSystemOne {
    pub url: String,
    bodies: Arc<Mutex<Vec<Value>>>,
}

impl FakeSystemOne {
    /// Start a fake that answers every request with `script(&body)`.
    pub fn start(script: impl Fn(&Value) -> Reply + Send + Sync + 'static) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind fake systemone");
        let url = format!("http://{}", listener.local_addr().unwrap());
        let bodies = Arc::new(Mutex::new(Vec::new()));
        let (log, script) = (Arc::clone(&bodies), Arc::new(script));
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut s) = stream else { continue };
                let (log, script) = (Arc::clone(&log), Arc::clone(&script));
                std::thread::spawn(move || {
                    let mut buf = Vec::new();
                    let mut tmp = [0u8; 8192];
                    let body = loop {
                        let n = s.read(&mut tmp).unwrap_or(0);
                        if n == 0 {
                            return;
                        }
                        buf.extend_from_slice(&tmp[..n]);
                        let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") else {
                            continue;
                        };
                        let head = String::from_utf8_lossy(&buf[..end]).to_string();
                        let len = head
                            .lines()
                            .find_map(|l| {
                                l.to_ascii_lowercase()
                                    .strip_prefix("content-length:")
                                    .map(|v| v.trim().parse().unwrap_or(0))
                            })
                            .unwrap_or(0usize);
                        if buf.len() >= end + 4 + len {
                            break buf[end + 4..end + 4 + len].to_vec();
                        }
                    };
                    let body: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
                    log.lock().unwrap().push(body.clone());
                    let r = script(&body);
                    let _ = s.write_all(
                        format!(
                            "HTTP/1.1 {} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                            r.status,
                            r.body.len(),
                            r.body
                        )
                        .as_bytes(),
                    );
                });
            }
        });
        FakeSystemOne { url, bodies }
    }

    /// Requests received so far.
    pub fn hits(&self) -> usize {
        self.bodies.lock().unwrap().len()
    }

    /// Every request body received, in order.
    pub fn bodies(&self) -> Vec<Value> {
        self.bodies.lock().unwrap().clone()
    }
}

/// A `/v1/systemone` answer to a rerank request: every `c<n>` question is a
/// one-hot score at `level(candidate text)`, over the question's own levels.
pub fn score_candidates(req: &Value, level: impl Fn(&str) -> usize) -> Value {
    let mut answers = serde_json::Map::new();
    let cands = req["state"]["candidates"].as_array().cloned().unwrap_or_default();
    for (id, q) in req["questions"].as_object().cloned().unwrap_or_default() {
        let n: usize = id.trim_start_matches('c').parse().unwrap_or(0);
        let levels = q["criteria"].as_array().map(|a| a.len()).unwrap_or(2);
        let text = cands.get(n).and_then(|c| c["text"].as_str()).unwrap_or("");
        let lvl = level(text).min(levels - 1);
        let probs: serde_json::Map<String, Value> =
            (0..levels).map(|k| (k.to_string(), Value::from(if k == lvl { 1.0 } else { 0.0 }))).collect();
        answers.insert(
            id,
            serde_json::json!({"type": "score", "score": lvl as f64, "probabilities": probs}),
        );
    }
    serde_json::json!({"model": "fake-jev-1", "answers": answers,
                       "usage": {"input_tokens": 100, "output_tokens": 5}})
}
