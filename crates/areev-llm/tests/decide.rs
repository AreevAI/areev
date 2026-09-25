//! Decision backends (`areev_llm::decide`) against an in-process fake
//! `/v1/systemone` server — keyless and deterministic. The server is a
//! hand-rolled HTTP/1.1 stub on a std `TcpListener` that reads each request
//! body to `Content-Length` before answering (an unread body turns the close
//! into an RST, which on macOS discards the reply — `areev-testing` rule 6).

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use areev_llm::decide::{
    resolve_chain_with, Answer, Chain, CloudflareWorkersAi, CommandDecide, DecideError,
    DecideRequest, Decision, DecisionBackend, LlmEmulated, NoulCriteria, Question, SystemOneHttp,
};

// ---- the fake server ---------------------------------------------------------

#[derive(Clone, Debug)]
struct Seen {
    path: String,
    head: String,
    body: Value,
}

struct Reply {
    status: u16,
    headers: Vec<(&'static str, String)>,
    body: String,
    delay: Duration,
}

fn ok(body: Value) -> Reply {
    Reply { status: 200, headers: vec![], body: body.to_string(), delay: Duration::ZERO }
}

fn status(code: u16, body: &str) -> Reply {
    Reply { status: code, headers: vec![], body: body.to_string(), delay: Duration::ZERO }
}

struct Fake {
    url: String,
    seen: Arc<Mutex<Vec<Seen>>>,
}

impl Fake {
    fn hits(&self) -> usize {
        self.seen.lock().unwrap().len()
    }
    fn last(&self) -> Seen {
        self.seen.lock().unwrap().last().cloned().expect("the fake saw no request")
    }
}

/// Start a fake that answers every request with `script(&seen)`. Each
/// connection is served on its own thread, so a slow reply blocks nobody.
fn fake(script: impl Fn(&Seen) -> Reply + Send + Sync + 'static) -> Fake {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let seen = Arc::new(Mutex::new(Vec::new()));
    let (log, script) = (Arc::clone(&seen), Arc::new(script));
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut s) = stream else { continue };
            let (log, script) = (Arc::clone(&log), Arc::clone(&script));
            std::thread::spawn(move || {
                let mut buf = Vec::new();
                let mut tmp = [0u8; 8192];
                let (head, body) = loop {
                    let n = s.read(&mut tmp).unwrap_or(0);
                    if n == 0 {
                        return;
                    }
                    buf.extend_from_slice(&tmp[..n]);
                    let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") else { continue };
                    let head = String::from_utf8_lossy(&buf[..end]).to_string();
                    let len = head
                        .lines()
                        .find_map(|l| l.to_ascii_lowercase().strip_prefix("content-length:").map(|v| v.trim().parse().unwrap_or(0)))
                        .unwrap_or(0usize);
                    if buf.len() >= end + 4 + len {
                        break (head, buf[end + 4..end + 4 + len].to_vec());
                    }
                };
                let path = head.split_whitespace().nth(1).unwrap_or("").to_string();
                let seen = Seen { path, head, body: serde_json::from_slice(&body).unwrap_or(Value::Null) };
                log.lock().unwrap().push(seen.clone());
                let r = script(&seen);
                std::thread::sleep(r.delay);
                let mut extra = String::new();
                for (k, v) in &r.headers {
                    extra.push_str(&format!("{k}: {v}\r\n"));
                }
                let _ = s.write_all(
                    format!(
                        "HTTP/1.1 {} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\n{extra}Connection: close\r\n\r\n{}",
                        r.status,
                        r.body.len(),
                        r.body
                    )
                    .as_bytes(),
                );
            });
        }
    });
    Fake { url, seen }
}

// ---- fixtures ------------------------------------------------------------------

fn three_questions() -> BTreeMap<String, Question> {
    let mut q = BTreeMap::new();
    q.insert(
        "q1".to_string(),
        Question::Noul {
            instructions: "Is the grain about billing?".into(),
            criteria: Some(NoulCriteria { yes: "about billing".into(), no: "not about billing".into() }),
        },
    );
    q.insert("q2".to_string(), Question::choice("Which team owns it?", [("a", "accounts"), ("b", "backend")]));
    q.insert("q3".to_string(), Question::score("How relevant?", ["level0", "level1", "level2"]));
    q
}

fn req() -> DecideRequest {
    DecideRequest::new(json!({"text": "invoice 42 is overdue"}), three_questions())
}

/// The TypeSafe response from the proposal §3, with q2's confidence left out
/// so it has to be computed.
fn typesafe_body() -> Value {
    json!({
        "model": "jev-1.13.0",
        "answers": {
            "q1": {"type": "noul", "noul": 0.93},
            "q2": {"type": "choice", "choice": "a", "probabilities": {"a": 0.8, "b": 0.2}},
            "q3": {"type": "score", "score": 1.4, "probabilities": {"0": 0.1, "1": 0.4, "2": 0.5},
                   "legend": {"0": "level0", "1": "level1", "2": "level2"}, "confidence": 0.25}
        },
        "usage": {"input_tokens": 318, "output_tokens": 34}
    })
}

fn systemone(f: &Fake, provider: &str) -> SystemOneHttp {
    SystemOneHttp::new(provider, &f.url, "jev-latest", Some("sk-test".into()), vec![])
}

// ---- request serialization + answer parsing -----------------------------------

#[test]
fn request_serializes_all_three_question_types() {
    let f = fake(|_| ok(typesafe_body()));
    let b = SystemOneHttp::new(
        "typesafe",
        &f.url,
        "jev-latest",
        Some("sk-test".into()),
        vec![("X-Extra".into(), "1".into())],
    );
    b.decide(&req()).unwrap();
    let s = f.last();
    assert_eq!(s.path, "/v1/systemone");
    let head = s.head.to_ascii_lowercase();
    assert!(head.contains("\r\nauthorization: bearer sk-test"), "{head}");
    assert!(head.contains("\r\nx-extra: 1"), "{head}");
    assert_eq!(
        s.body,
        json!({
            "model": "jev-latest",
            "state": {"text": "invoice 42 is overdue"},
            "questions": {
                "q1": {"type": "noul", "instructions": "Is the grain about billing?",
                       "criteria": {"true": "about billing", "false": "not about billing"}},
                "q2": {"type": "choice", "instructions": "Which team owns it?",
                       "criteria": {"a": "accounts", "b": "backend"}},
                "q3": {"type": "score", "instructions": "How relevant?",
                       "criteria": ["level0", "level1", "level2"]}
            }
        })
    );
}

#[test]
fn no_key_sends_no_authorization_and_a_systemone_suffix_is_kept() {
    let f = fake(|_| ok(typesafe_body()));
    let b = SystemOneHttp::new("systemone", format!("{}/typesafe/v1/systemone", f.url), "m", None, vec![]);
    b.decide(&req()).unwrap();
    let s = f.last();
    assert_eq!(s.path, "/typesafe/v1/systemone");
    assert!(!s.head.to_ascii_lowercase().contains("\nauthorization:"));
}

#[test]
fn typesafe_answers_parse_with_legend_and_computed_confidence() {
    let f = fake(|_| ok(typesafe_body()));
    let d = systemone(&f, "typesafe").decide(&req()).unwrap();
    assert_eq!(d.model, "jev-1.13.0");
    assert_eq!(d.provider, "typesafe");
    assert!(d.calibrated);
    assert_eq!((d.input_tokens, d.output_tokens), (Some(318), Some(34)));
    assert_eq!(d.answers["q1"], Answer::Noul { p: 0.93 });
    let Answer::Choice { choice, probabilities, confidence } = &d.answers["q2"] else { panic!("{:?}", d.answers) };
    assert_eq!(choice, "a");
    assert_eq!(probabilities["b"], 0.2);
    assert!((confidence - 0.6).abs() < 1e-5, "(2·0.8−1)/1 computed when omitted, got {confidence}");
    let Answer::Score { score, confidence, legend, .. } = &d.answers["q3"] else { panic!() };
    assert_eq!(*score, 1.4);
    assert_eq!(*confidence, 0.25);
    assert_eq!(legend["2"], "level2");
    // The printable form carries the wire answers plus provenance.
    let out = d.to_json();
    assert_eq!(out["answers"]["q3"]["legend"]["0"], "level0");
    assert_eq!(out["answers"]["q2"]["confidence"], json!(0.6));
    assert_eq!(out["provider"], "typesafe");
    assert_eq!(out["usage"]["input_tokens"], 318);
}

/// The literal body a live probe of OpenRouter's `/api/v1/systemone`
/// returned: a dated served model, extra top-level fields (`id`, `provider`),
/// an extra usage field (`cost`), exact zeros, and the provider's own
/// (rounded) score and confidence — all of which must be accepted as sent.
#[test]
fn typesafe_via_openrouter_live_fixture() {
    const BODY: &str = r#"{"model":"typesafe/jev-1.13-20260917","answers":{"rel":{"type":"score","score":2.95,"legend":{"0":"off-topic","1":"tangential","2":"relevant","3":"directly answers"},"probabilities":{"0":0,"1":0,"2":0.04,"3":0.96},"confidence":0.95},"verbatim":{"type":"noul","noul":0.26}},"usage":{"input_tokens":375,"output_tokens":35,"cost":0.00001575},"id":"gen-dec-...","provider":"TypeSafe"}"#;
    let f = fake(|_| Reply { status: 200, headers: vec![], body: BODY.into(), delay: Duration::ZERO });
    let mut q = BTreeMap::new();
    q.insert(
        "rel".to_string(),
        Question::score("How relevant is the grain?", ["off-topic", "tangential", "relevant", "directly answers"]),
    );
    q.insert("verbatim".to_string(), Question::noul("Must it be kept verbatim?"));
    let b = SystemOneHttp::new("openrouter", format!("{}/api", f.url), "jev-latest", Some("k".into()), vec![]);
    let d = b.decide(&DecideRequest::new("the state", q)).unwrap();
    assert_eq!(f.last().path, "/api/v1/systemone");
    assert_eq!(d.model, "typesafe/jev-1.13-20260917");
    assert_eq!(d.provider, "openrouter", "the spec name, not the body's `provider`");
    assert_eq!((d.input_tokens, d.output_tokens), (Some(375), Some(35)));
    // `usage.cost` is USD; kept as micro-dollars rounded UP (15.75 → 16) so
    // summed sub-micro calls never under-charge a budget.
    assert_eq!(d.usd_micros, Some(16));
    assert_eq!(d.to_json()["usage"]["usd_micros"], 16);
    let Answer::Score { score, confidence, probabilities, legend } = &d.answers["rel"] else { panic!() };
    assert_eq!(*score, 2.95, "the provider's own score is kept");
    assert_eq!(*confidence, 0.95, "the provider's own confidence is kept");
    assert_eq!(probabilities["0"], 0.0);
    assert_eq!(probabilities["3"], 0.96);
    assert_eq!(legend["3"], "directly answers");
    assert_eq!(d.answers["verbatim"], Answer::Noul { p: 0.26 });
}

#[test]
fn cloudflare_unwraps_result_and_sends_no_model() {
    let f = fake(|_| ok(json!({"result": typesafe_body(), "success": true, "errors": [], "messages": []})));
    let b = CloudflareWorkersAi::new("acct123", "cf-token", "typesafe/jev").with_base_url(&f.url);
    let d = b.decide(&req()).unwrap();
    let s = f.last();
    assert_eq!(s.path, "/accounts/acct123/ai/run/typesafe/jev");
    assert!(s.head.to_ascii_lowercase().contains("authorization: bearer cf-token"));
    assert!(s.body.get("model").is_none(), "Cloudflare's body carries no model: {}", s.body);
    assert!(s.body.get("questions").is_some() && s.body.get("state").is_some());
    assert_eq!(d.provider, "cloudflare");
    assert_eq!(d.model, "jev-1.13.0");
    assert_eq!(d.answers.len(), 3);
    assert_eq!(b.describe(), "cloudflare:typesafe/jev");
}

#[test]
fn cloudflare_success_false_is_e002_with_the_errors_text() {
    let f = fake(|_| {
        ok(json!({"result": null, "success": false,
                  "errors": [{"code": 5007, "message": "No such model"}], "messages": []}))
    });
    let b = CloudflareWorkersAi::new("a", "t", "typesafe/jev").with_base_url(&f.url);
    let e = b.decide(&req()).unwrap_err();
    assert_eq!(e.code(), "DEC-E002");
    assert!(e.to_string().contains("5007: No such model"), "{e}");
}

// ---- error mapping ---------------------------------------------------------------

#[test]
fn rate_limit_429_carries_retry_after_and_does_not_sleep() {
    let f = fake(|_| Reply {
        status: 429,
        headers: vec![("Retry-After", "3".into())],
        body: r#"{"error":{"message":"slow down"}}"#.into(),
        delay: Duration::ZERO,
    });
    let t = Instant::now();
    let e = systemone(&f, "typesafe").decide(&req()).unwrap_err();
    assert_eq!(e.code(), "DEC-E007");
    assert_eq!(e.retry_after_secs(), Some(3));
    assert!(t.elapsed() < Duration::from_secs(3), "the adapter must not honour Retry-After by sleeping");
    assert_eq!(f.hits(), 1);
}

#[test]
fn status_errors_map_to_e002_with_the_body_message() {
    let f = fake(|_| status(503, r#"{"error":{"message":"overloaded"}}"#));
    let e = systemone(&f, "typesafe").decide(&req()).unwrap_err();
    assert_eq!(e.code(), "DEC-E002");
    assert!(matches!(e, DecideError::Provider { status: Some(503), retryable: true, .. }), "{e:?}");
    assert!(e.to_string().contains("overloaded"));
    assert!(!e.stops_chain());

    let f = fake(|_| status(401, r#"{"error":"invalid api key"}"#));
    let e = systemone(&f, "typesafe").decide(&req()).unwrap_err();
    assert!(matches!(e, DecideError::Provider { status: Some(401), retryable: false, .. }), "{e:?}");
    assert!(e.to_string().contains("invalid api key"));

    let f = fake(|_| status(422, r#"{"detail":"criteria must have 2 entries"}"#));
    let e = systemone(&f, "typesafe").decide(&req()).unwrap_err();
    assert!(matches!(e, DecideError::Provider { status: Some(422), retryable: false, .. }), "{e:?}");
    assert!(e.to_string().contains("criteria must have 2 entries"));
    assert!(e.stops_chain(), "an invalid request is not retried elsewhere");
}

#[test]
fn malformed_answers_are_e003() {
    let missing = fake(|_| {
        ok(json!({"model": "m", "answers": {"q1": {"type": "noul", "noul": 0.5},
                                           "q2": {"type": "choice", "choice": "a", "probabilities": {"a": 0.5, "b": 0.5}}}}))
    });
    let e = systemone(&missing, "t").decide(&req()).unwrap_err();
    assert_eq!(e.code(), "DEC-E003");
    assert!(e.to_string().contains("q3"), "{e}");

    let sum_two = fake(|_| {
        let mut b = typesafe_body();
        b["answers"]["q2"]["probabilities"] = json!({"a": 1.0, "b": 1.0});
        ok(b)
    });
    let e = systemone(&sum_two, "t").decide(&req()).unwrap_err();
    assert_eq!(e.code(), "DEC-E003");
    assert!(e.to_string().contains("sum to 2"), "{e}");

    let not_json = fake(|_| Reply { status: 200, headers: vec![], body: "<html>".into(), delay: Duration::ZERO });
    assert_eq!(systemone(&not_json, "t").decide(&req()).unwrap_err().code(), "DEC-E003");
}

#[test]
fn invalid_questions_are_e006_and_never_sent() {
    let f = fake(|_| ok(typesafe_body()));
    let mut q = BTreeMap::new();
    q.insert("c".to_string(), Question::choice("pick", [("only", "one option")]));
    let e = systemone(&f, "t").decide(&DecideRequest::new("s", q)).unwrap_err();
    assert_eq!(e.code(), "DEC-E006");
    assert_eq!(f.hits(), 0);
}

#[test]
fn a_slow_provider_hits_the_deadline() {
    let f = fake(|_| Reply { status: 200, headers: vec![], body: typesafe_body().to_string(), delay: Duration::from_secs(3) });
    let t = Instant::now();
    let e = systemone(&f, "t")
        .decide(&req().with_deadline(Some(Duration::from_millis(200))))
        .unwrap_err();
    assert_eq!(e.code(), "DEC-E004", "{e}");
    assert!(t.elapsed() < Duration::from_secs(2), "returned at the deadline, not the reply");
}

#[test]
fn an_unreachable_provider_is_a_retryable_e002() {
    // Bind then drop: nothing listens on the port.
    let port = TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let b = SystemOneHttp::new("systemone", format!("http://127.0.0.1:{port}"), "m", None, vec![]);
    let e = b.decide(&req()).unwrap_err();
    // Unix refuses a closed loopback port at once, so the backend reports the
    // transport error the chain retries past — the classification this test
    // pins. Windows has been observed (GitHub's runner, intermittently) to
    // stall the connect instead of refusing it; the request then meets its
    // deadline, and the same physical condition — no listener — arrives as
    // "no answer" rather than "no connection". Both are the backend declining
    // to hang, so on Windows either shape is accepted; the immediate-refusal
    // classification is asserted only where the OS guarantees the refusal.
    #[cfg(not(windows))]
    assert!(matches!(e, DecideError::Provider { status: None, retryable: true, .. }), "{e:?}");
    #[cfg(windows)]
    assert!(
        matches!(
            e,
            DecideError::Provider { status: None, retryable: true, .. } | DecideError::Deadline(_)
        ),
        "{e:?}"
    );
}

// ---- chain ---------------------------------------------------------------------------

/// A scripted in-process entry: records the deadline it was handed, sleeps,
/// then answers or fails.
struct Scripted {
    name: &'static str,
    calibrated: bool,
    sleep: Duration,
    fail: Option<DecideError>,
    seen: Arc<Mutex<Vec<Option<Duration>>>>,
}

impl Scripted {
    fn new(name: &'static str, fail: Option<DecideError>) -> Self {
        Scripted { name, calibrated: true, sleep: Duration::ZERO, fail, seen: Arc::default() }
    }
}

impl DecisionBackend for Scripted {
    fn decide(&self, req: &DecideRequest) -> Result<Decision, DecideError> {
        self.seen.lock().unwrap().push(req.deadline);
        std::thread::sleep(self.sleep);
        if let Some(e) = &self.fail {
            return Err(e.clone());
        }
        Ok(Decision {
            answers: BTreeMap::new(),
            model: format!("{}-model", self.name),
            provider: self.name.to_string(),
            calibrated: self.calibrated,
            input_tokens: None,
            output_tokens: None,
            usd_micros: None,
            latency_ms: 0,
        })
    }
    fn calibrated(&self) -> bool {
        self.calibrated
    }
    fn describe(&self) -> String {
        format!("{}:m", self.name)
    }
}

#[test]
fn chain_falls_back_in_order_over_429_503_and_answers_from_the_first_that_can() {
    let limited = fake(|_| Reply { status: 429, headers: vec![("Retry-After", "3".into())], body: "{}".into(), delay: Duration::ZERO });
    let down = fake(|_| status(503, "{}"));
    let good = fake(|_| ok(typesafe_body()));
    let never = fake(|_| ok(typesafe_body()));
    let chain = Chain::new(
        vec![
            Box::new(systemone(&limited, "typesafe")),
            Box::new(systemone(&down, "openrouter")),
            Box::new(systemone(&good, "systemone")),
            Box::new(systemone(&never, "vercel")),
        ],
        Some(Duration::from_secs(5)),
    );
    let d = chain.decide(&req()).unwrap();
    assert_eq!(d.provider, "systemone", "the Decision names the entry that answered");
    assert_eq!((limited.hits(), down.hits(), good.hits(), never.hits()), (1, 1, 1, 0));
    assert_eq!(
        chain.describe(),
        "typesafe:jev-latest,openrouter:jev-latest,systemone:jev-latest,vercel:jev-latest"
    );
}

#[test]
fn chain_does_not_retry_an_invalid_question_or_a_422() {
    let second = Scripted::new("second", None);
    let seen = Arc::clone(&second.seen);
    let chain = Chain::new(
        vec![
            Box::new(Scripted::new("first", Some(DecideError::InvalidQuestion("bad".into())))),
            Box::new(second),
        ],
        None,
    );
    assert_eq!(chain.decide(&req()).unwrap_err().code(), "DEC-E006");
    assert!(seen.lock().unwrap().is_empty(), "E006 must not move to the next entry");

    let rejecting = fake(|_| status(422, r#"{"detail":"bad criteria"}"#));
    let after = fake(|_| ok(typesafe_body()));
    let chain = Chain::new(vec![Box::new(systemone(&rejecting, "a")), Box::new(systemone(&after, "b"))], None);
    let e = chain.decide(&req()).unwrap_err();
    assert_eq!(e.status(), Some(422));
    assert_eq!(after.hits(), 0, "a 422 is not retried");
}

#[test]
fn chain_exhausted_carries_every_entry_error() {
    let chain = Chain::new(
        vec![
            Box::new(Scripted::new("a", Some(DecideError::RateLimited { provider: "a".into(), retry_after_secs: Some(3) }))),
            Box::new(Scripted::new("b", Some(DecideError::Malformed("no q1".into())))),
            Box::new(Scripted::new("c", Some(DecideError::Deadline("slow".into())))),
        ],
        None,
    );
    let e = chain.decide(&req()).unwrap_err();
    assert_eq!(e.code(), "DEC-E005");
    let DecideError::ChainExhausted(errs) = &e else { panic!("{e:?}") };
    let got: Vec<(&str, &str)> = errs.iter().map(|(w, e)| (w.as_str(), e.code())).collect();
    assert_eq!(got, vec![("a:m", "DEC-E007"), ("b:m", "DEC-E003"), ("c:m", "DEC-E004")]);
    let text = e.to_string();
    assert!(text.starts_with("DEC-E005: ") && text.contains("a:m → DEC-E007"), "{text}");
}

#[test]
fn chain_budget_shrinks_across_entries() {
    let mut first = Scripted::new("first", Some(DecideError::Deadline("slow".into())));
    first.sleep = Duration::from_millis(150);
    let second = Scripted::new("second", None);
    let (s1, s2) = (Arc::clone(&first.seen), Arc::clone(&second.seen));
    let chain = Chain::new(vec![Box::new(first), Box::new(second)], Some(Duration::from_secs(10)));
    let budget = Duration::from_millis(1000);
    let d = chain.decide(&req().with_deadline(Some(budget))).unwrap();
    assert_eq!(d.provider, "second");
    let first_got = s1.lock().unwrap()[0].expect("the chain always passes a deadline down");
    let second_got = s2.lock().unwrap()[0].expect("the chain always passes a deadline down");
    assert!(first_got <= budget, "the request deadline overrides the chain default");
    assert!(second_got <= budget - Duration::from_millis(150), "second got {second_got:?}");

    // No request deadline: the chain default is the budget.
    let probe = Scripted::new("p", None);
    let sp = Arc::clone(&probe.seen);
    Chain::new(vec![Box::new(probe)], Some(Duration::from_millis(700))).decide(&req()).unwrap();
    assert!(sp.lock().unwrap()[0].unwrap() <= Duration::from_millis(700));
}

#[test]
fn chain_never_exceeds_the_callers_deadline() {
    let slow = || fake(|_| Reply { status: 200, headers: vec![], body: typesafe_body().to_string(), delay: Duration::from_secs(3) });
    let (a, b) = (slow(), slow());
    let chain = Chain::new(vec![Box::new(systemone(&a, "a")), Box::new(systemone(&b, "b"))], None);
    let t = Instant::now();
    let e = chain.decide(&req().with_deadline(Some(Duration::from_millis(300)))).unwrap_err();
    assert!(t.elapsed() < Duration::from_secs(2), "took {:?}", t.elapsed());
    assert_eq!(e.code(), "DEC-E005");
    let DecideError::ChainExhausted(errs) = &e else { panic!() };
    assert!(errs.iter().all(|(_, e)| e.code() == "DEC-E004"), "{e}");
}

#[test]
fn chain_calibration_is_the_and_of_its_entries() {
    let mut uncal = Scripted::new("u", None);
    uncal.calibrated = false;
    assert!(!Chain::new(vec![Box::new(Scripted::new("c", None)), Box::new(uncal)], None).calibrated());
    assert!(Chain::new(vec![Box::new(Scripted::new("c", None))], None).calibrated());
    assert_eq!(Chain::new(vec![], None).decide(&req()).unwrap_err().code(), "DEC-E001");
}

// ---- resolve_chain -------------------------------------------------------------------

fn env_of(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
    let map: BTreeMap<String, String> = pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
    move |k: &str| map.get(k).cloned()
}

#[test]
fn resolve_nothing_is_the_deterministic_floor() {
    assert!(resolve_chain_with(None, None, None, env_of(&[])).unwrap().is_none());
    assert!(resolve_chain_with(Some("  "), Some(""), None, env_of(&[])).unwrap().is_none());
}

#[test]
fn resolve_parses_every_provider_with_defaults() {
    let env = env_of(&[
        ("TYPESAFE_API_KEY", "k"),
        ("OPENROUTER_API_KEY", "k"),
        ("AI_GATEWAY_API_KEY", "k"),
        ("OPENJEV_API_KEY", "k"),
        ("CLOUDFLARE_API_TOKEN", "t"),
        ("CLOUDFLARE_ACCOUNT_ID", "acct"),
    ]);
    let chain = resolve_chain_with(
        Some("typesafe, openrouter:jev-1.13, vercel, openjev, cloudflare, systemone:http://127.0.0.1:1234#m, systemone:https://kev.local"),
        Some("my-decider --flag"),
        Some(Duration::from_millis(2000)),
        env,
    )
    .unwrap()
    .unwrap();
    assert_eq!(
        chain.describe(),
        "typesafe:jev-latest,openrouter:jev-1.13,vercel:typesafe-ai/jev,openjev:openjev,\
         cloudflare:typesafe/jev,systemone:m,systemone:jev-latest,cmd:my-decider"
    );
    assert!(chain.calibrated());
}

#[test]
fn resolve_splits_on_the_first_colon_only() {
    // The URL's own colons (scheme, port) pass through to the adapter.
    let f = fake(|_| ok(typesafe_body()));
    let spec = format!("systemone:{}#served-model", f.url);
    let chain = resolve_chain_with(Some(&spec), None, None, env_of(&[("AREEV_DECIDE_API_KEY", "local-key")]))
        .unwrap()
        .unwrap();
    let d = chain.decide(&req()).unwrap();
    assert_eq!(d.provider, "systemone");
    let s = f.last();
    assert_eq!(s.path, "/v1/systemone");
    assert_eq!(s.body["model"], "served-model");
    assert!(s.head.to_ascii_lowercase().contains("authorization: bearer local-key"));

    // A nested LLM spec keeps its colons: `llm:ollama:qwen3.5:4b` resolves
    // `ollama:qwen3.5:4b` (keyless, so it resolves without a network call).
    #[cfg(feature = "ollama")]
    {
        let c = resolve_chain_with(Some("llm:ollama:qwen3.5:4b"), None, None, env_of(&[])).unwrap().unwrap();
        assert_eq!(c.describe(), "llm:ollama:qwen3.5:4b");
        assert!(!c.calibrated(), "an emulated entry makes the chain rank-only");
    }
}

#[test]
fn resolve_refusals_are_e001_and_name_the_env_var() {
    let none = || env_of(&[]);
    let err = |spec: &str| match resolve_chain_with(Some(spec), None, None, none()) {
        Err(e) => e,
        Ok(_) => panic!("{spec} should not resolve"),
    };
    for (spec, var) in [
        ("typesafe", "TYPESAFE_API_KEY"),
        ("openrouter:jev-latest", "OPENROUTER_API_KEY"),
        ("vercel", "AI_GATEWAY_API_KEY"),
        ("openjev", "OPENJEV_API_KEY"),
        ("cloudflare", "CLOUDFLARE_API_TOKEN"),
    ] {
        let e = err(spec);
        assert_eq!(e.code(), "DEC-E001");
        assert!(e.to_string().contains(var), "{spec}: {e}");
    }
    let e = resolve_chain_with(Some("cloudflare"), None, None, env_of(&[("CLOUDFLARE_API_TOKEN", "t")])).err().unwrap();
    assert!(e.to_string().contains("CLOUDFLARE_ACCOUNT_ID"), "{e}");
    let e = resolve_chain_with(Some("typesafe"), None, None, env_of(&[("TYPESAFE_API_KEY", "  ")])).err().unwrap();
    assert!(e.to_string().contains("TYPESAFE_API_KEY"), "an empty key counts as unset: {e}");
    assert!(err("nope:x").to_string().contains("unknown decision provider \"nope\""));
    assert_eq!(err("cmd:echo").code(), "DEC-E001");
    assert_eq!(err("systemone").code(), "DEC-E001");
    assert_eq!(err("systemone:ftp://x").code(), "DEC-E001");
    assert_eq!(err("llm").code(), "DEC-E001");
    assert_eq!(err("typesafe,,x").code(), "DEC-E001");
}

#[test]
fn resolve_cmd_alone_is_a_chain_of_one_and_cmd_is_last() {
    let c = resolve_chain_with(None, Some("decider"), None, env_of(&[])).unwrap().unwrap();
    assert_eq!(c.describe(), "cmd:decider");
    let c = resolve_chain_with(
        Some("systemone:http://127.0.0.1:9"),
        Some("decider"),
        None,
        env_of(&[]),
    )
    .unwrap()
    .unwrap();
    assert!(c.describe().ends_with(",cmd:decider"), "{}", c.describe());
}

// ---- CommandDecide ---------------------------------------------------------------------

fn find_python() -> Option<&'static str> {
    ["python3", "python"].into_iter().find(|c| {
        std::process::Command::new(c).arg("--version").output().is_ok_and(|o| o.status.success())
    })
}

/// Answers each question by type; echoes whether it saw the wire shape. An
/// optional argv[1] of `uncal` marks the answer uncalibrated and names a
/// provider.
const DECIDER_PY: &str = r#"
import sys, json
req = json.load(sys.stdin)
assert "model" not in req and "state" in req, req
ans = {}
for qid, q in req["questions"].items():
    if q["type"] == "noul":
        ans[qid] = {"type": "noul", "noul": 0.7}
    elif q["type"] == "choice":
        keys = sorted(q["criteria"])
        ans[qid] = {"type": "choice", "choice": keys[0],
                    "probabilities": {k: (0.75 if i == 0 else 0.25 / (len(keys) - 1)) for i, k in enumerate(keys)}}
    else:
        n = len(q["criteria"])
        ans[qid] = {"type": "score", "probabilities": {str(i): 1.0 / n for i in range(n)}}
out = {"model": "toy-decider", "answers": ans}
if len(sys.argv) > 1 and sys.argv[1] == "uncal":
    out["calibrated"] = False
    out["provider"] = "toy"
print(json.dumps(out))
"#;

#[test]
fn command_decide_round_trips_the_wire_shape() {
    let Some(py) = find_python() else {
        eprintln!("skipping: no python on PATH");
        return;
    };
    let dir = tempfile::TempDir::new().unwrap();
    let script = dir.path().join("decider.py");
    std::fs::write(&script, DECIDER_PY).unwrap();
    let b = CommandDecide::new(&format!("{py} {}", script.display())).unwrap();
    let d = b.decide(&req()).unwrap();
    assert_eq!((d.provider.as_str(), d.model.as_str(), d.calibrated), ("cmd", "toy-decider", true));
    assert_eq!(d.answers["q1"], Answer::Noul { p: 0.7 });
    let Answer::Choice { choice, confidence, .. } = &d.answers["q2"] else { panic!() };
    assert_eq!(choice, "a");
    assert!((confidence - 0.5).abs() < 1e-5, "(2·0.75−1)/1 = 0.5, got {confidence}");
    let Answer::Score { score, confidence, legend, .. } = &d.answers["q3"] else { panic!() };
    assert!((score - 1.0).abs() < 1e-5, "uniform over 0..=2 weighs to 1.0, got {score}");
    assert!(confidence.abs() < 1e-5, "uniform is zero confidence");
    assert_eq!(legend["1"], "level1", "legend built from the levels when the command sends none");
    assert!(b.calibrated());
}

#[test]
fn command_decide_honours_calibrated_false_and_provider() {
    let Some(py) = find_python() else {
        eprintln!("skipping: no python on PATH");
        return;
    };
    let dir = tempfile::TempDir::new().unwrap();
    let script = dir.path().join("decider.py");
    std::fs::write(&script, DECIDER_PY).unwrap();
    let b = CommandDecide::new(&format!("{py} {} uncal", script.display())).unwrap();
    assert!(b.calibrated(), "calibrated until the command says otherwise");
    let d = b.decide(&req()).unwrap();
    assert_eq!(d.provider, "toy");
    assert!(!d.calibrated, "the Decision carries the command's own flag");
    assert!(!b.calibrated(), "and the backend reports it from then on");
}

#[test]
fn command_decide_deadline_kills_the_child() {
    let Some(py) = find_python() else {
        eprintln!("skipping: no python on PATH");
        return;
    };
    let dir = tempfile::TempDir::new().unwrap();
    let script = dir.path().join("slow.py");
    std::fs::write(&script, "import sys, time\nsys.stdin.read()\ntime.sleep(10)\n").unwrap();
    let b = CommandDecide::new(&format!("{py} {}", script.display())).unwrap();
    let t = Instant::now();
    let e = b.decide(&req().with_deadline(Some(Duration::from_millis(300)))).unwrap_err();
    assert_eq!(e.code(), "DEC-E004", "{e}");
    assert!(t.elapsed() < Duration::from_secs(5));
}

#[test]
fn command_decide_refusals() {
    assert_eq!(CommandDecide::new("   ").err().unwrap().code(), "DEC-E001");
    let b = CommandDecide::new("definitely-not-a-real-binary-xyz").unwrap();
    assert_eq!(b.decide(&req()).unwrap_err().code(), "DEC-E002");
}

// ---- LlmEmulated -------------------------------------------------------------------------

struct FakeLlm {
    reply: String,
    sleep: Duration,
    seen: Arc<Mutex<Vec<String>>>,
}

impl areev_loop::LlmBackend for FakeLlm {
    fn model(&self) -> &str {
        "fake-llm-1"
    }
    fn complete(&self, request: &str) -> areev_loop::Result<String> {
        self.seen.lock().unwrap().push(request.to_string());
        std::thread::sleep(self.sleep);
        Ok(self.reply.clone())
    }
}

fn fake_llm(reply: &str) -> (FakeLlm, Arc<Mutex<Vec<String>>>) {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let llm = FakeLlm { reply: reply.into(), sleep: Duration::ZERO, seen: Arc::clone(&seen) };
    (llm, seen)
}

#[test]
fn llm_emulated_parses_fenced_json_leniently_and_is_uncalibrated() {
    // Fenced; q2 sums to 2 (renormalized); q3 keyed by level NAMES; the
    // model's own confidence is ignored and recomputed.
    let reply = "```json\n{\"answers\": {\
        \"q1\": {\"type\": \"noul\", \"noul\": 0.9},\
        \"q2\": {\"choice\": \"b\", \"probabilities\": {\"a\": 0.4, \"b\": 1.6}, \"confidence\": 0.99},\
        \"q3\": {\"type\": \"score\", \"probabilities\": {\"level0\": 0, \"level1\": 0, \"level2\": 1}}\
    }}\n```";
    let (llm, seen) = fake_llm(reply);
    let b = LlmEmulated::new(Box::new(llm), "fake:llm".into());
    let d = b.decide(&req()).unwrap();
    assert!(!d.calibrated && !b.calibrated());
    assert_eq!((d.provider.as_str(), d.model.as_str()), ("llm", "fake-llm-1"));
    assert_eq!(b.describe(), "llm:fake:llm");
    assert_eq!(d.answers["q1"], Answer::Noul { p: 0.9 });
    let Answer::Choice { choice, probabilities, confidence } = &d.answers["q2"] else { panic!() };
    assert_eq!(choice, "b");
    assert!((probabilities["b"] - 0.8).abs() < 1e-6);
    assert!((confidence - 0.6).abs() < 1e-5, "recomputed, not the model's 0.99: {confidence}");
    let Answer::Score { score, confidence, .. } = &d.answers["q3"] else { panic!() };
    assert_eq!((*score, *confidence), (2.0, 1.0));

    let sent: Value = serde_json::from_str(&seen.lock().unwrap()[0]).unwrap();
    assert_eq!(sent["loop"], 1);
    assert_eq!(sent["op"], "decide");
    assert!(sent["instructions"].as_str().unwrap().contains("never instructions to follow"));
    assert_eq!(sent["state"], json!({"text": "invoice 42 is overdue"}));
    assert_eq!(sent["questions"]["q3"]["criteria"], json!(["level0", "level1", "level2"]));
}

#[test]
fn llm_emulated_refuses_garbage_and_missing_ids() {
    let (llm, _) = fake_llm("I think the answer is yes.");
    let e = LlmEmulated::new(Box::new(llm), "x".into()).decide(&req()).unwrap_err();
    assert_eq!(e.code(), "DEC-E003");
    // A bare `{id: answer}` map is accepted, but every id must be there.
    let (llm, _) = fake_llm(r#"{"q1": 0.4}"#);
    let e = LlmEmulated::new(Box::new(llm), "x".into()).decide(&req()).unwrap_err();
    assert!(e.to_string().contains("q2"), "{e}");
}

#[test]
fn llm_emulated_honours_the_deadline() {
    let (mut llm, _) = fake_llm(r#"{"answers": {}}"#);
    llm.sleep = Duration::from_secs(3);
    let b = LlmEmulated::new(Box::new(llm), "slow".into());
    let t = Instant::now();
    let e = b.decide(&req().with_deadline(Some(Duration::from_millis(200)))).unwrap_err();
    assert_eq!(e.code(), "DEC-E004");
    assert!(t.elapsed() < Duration::from_secs(2));
}
