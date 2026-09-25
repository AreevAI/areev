//! `areev decide` and the global `--decide*` flags, through the real binary
//! (docs/decision-model-proposal.md §5). Keyless and deterministic: HTTP
//! entries hit an in-process fake `/v1/systemone` server (a hand-rolled
//! HTTP/1.1 stub that reads each body to `Content-Length` before replying —
//! `areev-testing` rule 6), and the command entry is a small Python script,
//! mirroring `areev-llm/tests/decide.rs`.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::process::Command;
use std::sync::{Arc, Mutex};

use serde_json::{json, Value};
use tempfile::TempDir;

/// Run the binary with a scrubbed decision environment, so a developer's own
/// `$AREEV_DECIDE*` can never leak into what a test observes. `HOME` points
/// at an empty directory so a verb that wrongly resolved the default memory
/// would leave `~/.areev/` behind for the test to find.
fn areev(home: &TempDir, args: &[&str], env: &[(&str, &str)]) -> (bool, Value, String) {
    let mut c = Command::new(env!("CARGO_BIN_EXE_areev"));
    c.args(args)
        .env("HOME", home.path())
        .env("USERPROFILE", home.path())
        .env_remove("AREEV_DB")
        .env_remove("AREEV_DECIDE")
        .env_remove("AREEV_DECIDE_CMD")
        .env_remove("AREEV_DECIDE_TIMEOUT_MS")
        .env_remove("AREEV_DECIDE_API_KEY")
        .env_remove("TYPESAFE_API_KEY");
    for (k, v) in env {
        c.env(k, v);
    }
    let out = c.output().expect("spawn areev");
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    let json = serde_json::from_str(stdout.trim()).unwrap_or(Value::Null);
    (out.status.success(), json, String::from_utf8_lossy(&out.stderr).to_string())
}

// ---- the fake /v1/systemone server --------------------------------------------

struct Fake {
    url: String,
    seen: Arc<Mutex<Vec<(String, Value)>>>,
}

impl Fake {
    fn requests(&self) -> Vec<(String, Value)> {
        self.seen.lock().unwrap().clone()
    }
}

/// Answer every request with `script(n, &body)` — `n` is the 0-based request
/// index — as `(status, extra headers, body)`.
fn fake(script: impl Fn(usize, &Value) -> (u16, Vec<(&'static str, String)>, Value) + Send + Sync + 'static) -> Fake {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let seen = Arc::new(Mutex::new(Vec::new()));
    let (log, script) = (Arc::clone(&seen), Arc::new(script));
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut s) = stream else { continue };
            let mut buf = Vec::new();
            let mut tmp = [0u8; 8192];
            let (head, body) = loop {
                let n = s.read(&mut tmp).unwrap_or(0);
                if n == 0 {
                    break (String::new(), Vec::new());
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
            if head.is_empty() {
                continue;
            }
            let path = head.split_whitespace().nth(1).unwrap_or("").to_string();
            let body: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
            let n = {
                let mut l = log.lock().unwrap();
                l.push((path, body.clone()));
                l.len() - 1
            };
            let (status, headers, reply) = script(n, &body);
            let reply = reply.to_string();
            let mut extra = String::new();
            for (k, v) in headers {
                extra.push_str(&format!("{k}: {v}\r\n"));
            }
            let _ = s.write_all(
                format!(
                    "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\n{extra}Connection: close\r\n\r\n{reply}",
                    reply.len()
                )
                .as_bytes(),
            );
        }
    });
    Fake { url, seen }
}

/// A well-formed System One answer to whatever was asked: noul 0.9, the
/// first (sorted) choice at 0.8, and a score leaning on the top level.
fn answer_everything(body: &Value) -> Value {
    let mut answers = serde_json::Map::new();
    for (id, q) in body["questions"].as_object().cloned().unwrap_or_default() {
        let a = match q["type"].as_str() {
            Some("noul") => json!({"type": "noul", "noul": 0.9}),
            Some("choice") => {
                let mut keys: Vec<String> = q["criteria"].as_object().unwrap().keys().cloned().collect();
                keys.sort();
                let rest = 0.2 / (keys.len() - 1) as f64;
                let probs: serde_json::Map<String, Value> = keys
                    .iter()
                    .enumerate()
                    .map(|(i, k)| (k.clone(), json!(if i == 0 { 0.8 } else { rest })))
                    .collect();
                json!({"type": "choice", "choice": keys[0], "probabilities": probs})
            }
            _ => {
                let n = q["criteria"].as_array().unwrap().len();
                let probs: serde_json::Map<String, Value> = (0..n)
                    .map(|i| (i.to_string(), json!(if i + 1 == n { 1.0 } else { 0.0 })))
                    .collect();
                json!({"type": "score", "score": (n - 1) as f64, "probabilities": probs})
            }
        };
        answers.insert(id, a);
    }
    json!({"model": "jev-fake-1", "answers": answers, "usage": {"input_tokens": 7, "output_tokens": 3}})
}

fn systemone() -> Fake {
    fake(|_, body| (200, vec![], answer_everything(body)))
}

fn spec(f: &Fake) -> String {
    format!("systemone:{}", f.url)
}

/// The provenance every answer carries (proposal §2 rule 4).
fn assert_provenance(out: &Value, provider: &str, calibrated: bool) {
    assert_eq!(out["provider"], provider, "{out}");
    assert_eq!(out["calibrated"], calibrated, "{out}");
    assert!(out["latency_ms"].is_u64(), "{out}");
    assert!(out["model"].is_string(), "{out}");
}

// ---- the shorthand forms, over HTTP ------------------------------------------------

#[test]
fn noul_shorthand_asks_one_question_q_and_prints_provenance() {
    let home = TempDir::new().unwrap();
    let f = systemone();
    let (ok, out, err) = areev(
        &home,
        &["decide", "--decide", &spec(&f), "--state", "Customer asked twice for a window seat.",
          "--noul", "The customer has a seating preference"],
        &[],
    );
    assert!(ok, "{err}");
    assert_provenance(&out, "systemone", true);
    assert_eq!(out["model"], "jev-fake-1");
    assert_eq!(out["answers"]["q"]["type"], "noul", "{out}");
    assert!((out["answers"]["q"]["noul"].as_f64().unwrap() - 0.9).abs() < 1e-6, "{out}");
    assert_eq!(out["usage"]["input_tokens"], 7);

    // What left the process: the wire shape, at /v1/systemone, one question.
    let reqs = f.requests();
    assert_eq!(reqs.len(), 1);
    assert_eq!(reqs[0].0, "/v1/systemone");
    assert_eq!(reqs[0].1["state"], "Customer asked twice for a window seat.");
    assert_eq!(
        reqs[0].1["questions"],
        json!({"q": {"type": "noul", "instructions": "The customer has a seating preference"}})
    );
    // `decide` names no memory: it must not fall back to (and create) the default.
    assert!(!home.path().join(".areev").exists(), "decide resolved a default memory");
}

#[test]
fn choice_shorthand_collects_every_repeated_option() {
    let home = TempDir::new().unwrap();
    let f = systemone();
    let (ok, out, err) = areev(
        &home,
        &["decide", "--decide", &spec(&f), "--state", r#"{"ticket": "I was charged twice"}"#,
          "--choice", "Route this ticket", "--option", "billing=money owed or charged",
          "--option", "support=the product misbehaves", "--option", "sales=wants to buy more"],
        &[],
    );
    assert!(ok, "{err}");
    assert_provenance(&out, "systemone", true);
    assert_eq!(out["answers"]["q"]["choice"], "billing", "{out}");
    let sent = &f.requests()[0].1;
    // --state parsed as JSON because it is a JSON object.
    assert_eq!(sent["state"], json!({"ticket": "I was charged twice"}));
    assert_eq!(
        sent["questions"]["q"]["criteria"],
        json!({"billing": "money owed or charged", "support": "the product misbehaves", "sales": "wants to buy more"}),
        "all three --option values, not just the last: {sent}"
    );
}

#[test]
fn score_shorthand_keeps_levels_in_order() {
    let home = TempDir::new().unwrap();
    let f = systemone();
    let (ok, out, err) = areev(
        &home,
        &["decide", "--decide", &spec(&f), "--state", "Refund is due on the 3rd.",
          "--score", "How urgent is this?", "--level", "not urgent", "--level", "soon", "--level", "today"],
        &[],
    );
    assert!(ok, "{err}");
    assert_eq!(out["answers"]["q"]["type"], "score", "{out}");
    assert!((out["answers"]["q"]["score"].as_f64().unwrap() - 2.0).abs() < 1e-6, "{out}");
    assert_eq!(f.requests()[0].1["questions"]["q"]["criteria"], json!(["not urgent", "soon", "today"]));
}

#[test]
fn questions_and_state_from_files() {
    let home = TempDir::new().unwrap();
    let dir = TempDir::new().unwrap();
    let qpath = dir.path().join("questions.json");
    std::fs::write(
        &qpath,
        json!({
            "pref": {"type": "noul", "instructions": "Has a seating preference"},
            "team": {"type": "choice", "instructions": "Who owns it", "criteria": {"a": "accounts", "b": "backend"}}
        })
        .to_string(),
    )
    .unwrap();
    let spath = dir.path().join("ticket.txt");
    std::fs::write(&spath, "window seat please, LHR leg").unwrap();
    let f = systemone();
    let (ok, out, err) = areev(
        &home,
        &["decide", "--decide", &spec(&f),
          "--state", &format!("@{}", spath.display()), "--questions", &format!("@{}", qpath.display())],
        &[],
    );
    assert!(ok, "{err}");
    assert_eq!(out["answers"]["pref"]["type"], "noul", "{out}");
    assert_eq!(out["answers"]["team"]["choice"], "a", "{out}");
    let sent = &f.requests()[0].1;
    assert_eq!(sent["state"], "window seat please, LHR leg", "a non-JSON file is the state as text");
}

#[test]
fn the_chain_falls_through_to_the_next_entry_and_reports_who_answered() {
    let home = TempDir::new().unwrap();
    let down = fake(|_, _| (503, vec![], json!({"error": "overloaded"})));
    let up = systemone();
    let chain = format!("{},systemone:{}#jev-second", spec(&down), up.url);
    let (ok, out, err) = areev(&home, &["decide", "--decide", &chain, "--state", "x", "--noul", "is it?"], &[]);
    assert!(ok, "{err}");
    assert_eq!(down.requests().len(), 1);
    assert_eq!(up.requests().len(), 1);
    assert_eq!(up.requests()[0].1["model"], "jev-second");
    assert_provenance(&out, "systemone", true);
}

#[test]
fn a_429_with_retry_after_is_retried_once() {
    let home = TempDir::new().unwrap();
    let f = fake(|n, body| match n {
        0 => (429, vec![("Retry-After", "0".to_string())], json!({"error": "slow down"})),
        _ => (200, vec![], answer_everything(body)),
    });
    let (ok, out, err) = areev(&home, &["decide", "--decide", &spec(&f), "--state", "x", "--noul", "is it?"], &[]);
    assert!(ok, "{err}");
    assert!(err.contains("DEC-E007") && err.contains("retrying once"), "{err}");
    assert_eq!(f.requests().len(), 2);
    assert_eq!(out["answers"]["q"]["type"], "noul");

    // Twice rate limited: one retry only, then the coded failure.
    let always = fake(|_, _| (429, vec![("Retry-After", "0".to_string())], json!({})));
    let (ok, _, err) = areev(&home, &["decide", "--decide", &spec(&always), "--state", "x", "--noul", "is it?"], &[]);
    assert!(!ok);
    assert_eq!(always.requests().len(), 2, "exactly one retry");
    assert!(err.contains("DEC-E005") && err.contains("DEC-E007"), "{err}");
}

// ---- configuration: env fallback, bad specs, missing backend ---------------------------

#[test]
fn the_environment_supplies_the_chain_when_the_flag_is_absent() {
    let home = TempDir::new().unwrap();
    let f = systemone();
    let (ok, out, err) = areev(
        &home,
        &["decide", "--state", "x", "--noul", "is it?"],
        &[("AREEV_DECIDE", &spec(&f)), ("AREEV_DECIDE_TIMEOUT_MS", "5000")],
    );
    assert!(ok, "{err}");
    assert_provenance(&out, "systemone", true);
    assert_eq!(f.requests().len(), 1);
}

#[test]
fn a_bad_spec_fails_every_verb_at_startup_with_dec_e001() {
    let home = TempDir::new().unwrap();
    let (ok, _, err) = areev(&home, &["decide", "--decide", "nosuchprovider:x", "--state", "x", "--noul", "q"], &[]);
    assert!(!ok);
    assert!(err.contains("DEC-E001"), "{err}");

    // A missing provider key names the variable.
    let (ok, _, err) = areev(&home, &["decide", "--decide", "typesafe:jev-latest", "--state", "x", "--noul", "q"], &[]);
    assert!(!ok);
    assert!(err.contains("DEC-E001") && err.contains("TYPESAFE_API_KEY"), "{err}");

    // Not only `decide`: a store verb refuses too, before opening anything.
    let dir = TempDir::new().unwrap();
    let db = dir.path().join("m.db");
    let (ok, _, err) = areev(
        &home,
        &["add", "--db", db.to_str().unwrap(), "a", "b", "c"],
        &[("AREEV_DECIDE", "nosuchprovider:x")],
    );
    assert!(!ok);
    assert!(err.contains("DEC-E001"), "{err}");
    assert!(!db.exists(), "a refused startup must not create the memory");

    let (ok, _, err) = areev(
        &home,
        &["decide", "--decide", "systemone:http://127.0.0.1:9", "--decide-timeout-ms", "soon", "--state", "x", "--noul", "q"],
        &[],
    );
    assert!(!ok);
    assert!(err.contains("DEC-E001") && err.contains("decide-timeout-ms"), "{err}");
}

#[test]
fn decide_without_a_backend_or_with_a_bad_question_is_coded() {
    let home = TempDir::new().unwrap();
    let (ok, _, err) = areev(&home, &["decide", "--state", "x", "--noul", "q"], &[]);
    assert!(!ok);
    assert!(err.contains("DEC-E001"), "{err}");

    let f = systemone();
    // A choice needs at least two options: DEC-E006, and nothing is sent.
    let (ok, _, err) = areev(
        &home,
        &["decide", "--decide", &spec(&f), "--state", "x", "--choice", "pick", "--option", "only=one"],
        &[],
    );
    assert!(!ok);
    assert!(err.contains("DEC-E006"), "{err}");
    // Two question forms at once is ambiguous.
    let (ok, _, err) = areev(&home, &["decide", "--decide", &spec(&f), "--state", "x", "--noul", "a", "--score", "b"], &[]);
    assert!(!ok);
    assert!(err.contains("exactly one of"), "{err}");
    assert!(f.requests().is_empty(), "an invalid question never leaves the process");
}

// ---- the command backend ---------------------------------------------------------------

fn find_python() -> Option<&'static str> {
    ["python3", "python"].into_iter().find(|c| {
        Command::new(c).arg("--version").output().is_ok_and(|o| o.status.success())
    })
}

/// Answers each question by type, from the wire request on stdin.
const DECIDER_PY: &str = r#"
import sys, json
req = json.load(sys.stdin)
assert "model" not in req and "state" in req, req
ans = {}
for qid, q in req["questions"].items():
    if q["type"] == "noul":
        ans[qid] = {"type": "noul", "noul": 0.25}
    elif q["type"] == "choice":
        keys = sorted(q["criteria"])
        ans[qid] = {"type": "choice", "choice": keys[-1],
                    "probabilities": {k: (0.9 if i == len(keys) - 1 else 0.1 / (len(keys) - 1)) for i, k in enumerate(keys)}}
    else:
        n = len(q["criteria"])
        ans[qid] = {"type": "score", "probabilities": {str(i): 1.0 / n for i in range(n)}}
print(json.dumps({"model": "toy-decider", "answers": ans}))
"#;

#[test]
fn decide_cmd_answers_every_shorthand_and_is_the_last_chain_entry() {
    let Some(py) = find_python() else {
        eprintln!("skipping: no python on PATH");
        return;
    };
    let home = TempDir::new().unwrap();
    let dir = TempDir::new().unwrap();
    let script = dir.path().join("decider.py");
    std::fs::write(&script, DECIDER_PY).unwrap();
    let cmd = format!("{py} {}", script.display());
    // What this test measures is the ANSWERS, not the interpreter's start-up
    // time: under the default 2 s decision deadline a cold Python on a shared
    // Windows runner was killed before it printed anything (DEC-E004), and
    // the failure read as a backend bug. The default's tightness is pinned by
    // the deadline tests above; here the budget is generous on purpose.
    const SLOW_RUNNER_MS: &str = "30000";

    let (ok, out, err) = areev(
        &home,
        &["decide", "--decide-cmd", &cmd, "--decide-timeout-ms", SLOW_RUNNER_MS, "--state", "x", "--noul", "is it?"],
        &[],
    );
    assert!(ok, "{err}");
    assert_provenance(&out, "cmd", true);
    assert_eq!(out["model"], "toy-decider");
    assert!((out["answers"]["q"]["noul"].as_f64().unwrap() - 0.25).abs() < 1e-6, "{out}");

    let (ok, out, err) = areev(
        &home,
        &["decide", "--decide-cmd", &cmd, "--decide-timeout-ms", SLOW_RUNNER_MS, "--state", "x", "--choice", "pick", "--option", "a=first", "--option", "b=second"],
        &[],
    );
    assert!(ok, "{err}");
    assert_eq!(out["answers"]["q"]["choice"], "b", "{out}");

    let (ok, out, err) = areev(
        &home,
        &["decide", "--decide-cmd", &cmd, "--decide-timeout-ms", SLOW_RUNNER_MS, "--state", "x", "--score", "how much", "--level", "low", "--level", "high"],
        &[],
    );
    assert!(ok, "{err}");
    assert!((out["answers"]["q"]["score"].as_f64().unwrap() - 0.5).abs() < 1e-6, "{out}");

    // Behind a failing HTTP entry, the command answers (it is appended last).
    let down = fake(|_, _| (503, vec![], json!({})));
    let (ok, out, err) = areev(
        &home,
        &["decide", "--decide", &spec(&down), "--state", "x", "--noul", "is it?"],
        &[("AREEV_DECIDE_CMD", &cmd), ("AREEV_DECIDE_TIMEOUT_MS", SLOW_RUNNER_MS)],
    );
    assert!(ok, "{err}");
    assert_eq!(down.requests().len(), 1);
    assert_eq!(out["provider"], "cmd", "{out}");
}

// ---- egress: what a remote backend receives is pseudonymized ----------------------------

#[test]
fn under_anonymize_egress_the_backend_receives_pseudonymized_state() {
    let home = TempDir::new().unwrap();
    let f = systemone();
    let state = "Jane asked to be emailed at jane.doe@example.com about the refund.";

    // Positive control: without the floor the raw address does leave.
    let (ok, _, err) = areev(&home, &["decide", "--decide", &spec(&f), "--state", state, "--noul", "is it a refund?"], &[]);
    assert!(ok, "{err}");
    assert!(f.requests()[0].1.to_string().contains("jane.doe@example.com"));

    let (ok, out, err) = areev(
        &home,
        &["decide", "--decide", &spec(&f), "--anonymize-egress", "--state", state, "--noul", "is it a refund?"],
        &[],
    );
    assert!(ok, "{err}");
    let sent = f.requests()[1].1.to_string();
    assert!(!sent.contains("jane.doe@example.com"), "raw email left the process: {sent}");
    assert!(sent.contains("refund"), "the rest of the state still goes: {sent}");
    assert_eq!(out["answers"]["q"]["type"], "noul", "{out}");
}

// ---- recall: the chain as reranker, the command reranker, the deadline -------------------

fn areev_raw(home: &TempDir, args: &[&str], env: &[(&str, &str)]) -> (bool, String, String) {
    let mut c = Command::new(env!("CARGO_BIN_EXE_areev"));
    c.args(args)
        .env("HOME", home.path())
        .env("USERPROFILE", home.path())
        .env_remove("AREEV_DB")
        .env_remove("AREEV_DECIDE")
        .env_remove("AREEV_DECIDE_CMD")
        .env_remove("AREEV_RERANK_CMD")
        .env_remove("AREEV_RECALL_DEADLINE_MS");
    for (k, v) in env {
        c.env(k, v);
    }
    let out = c.output().expect("spawn areev");
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).to_string(),
        String::from_utf8_lossy(&out.stderr).to_string(),
    )
}

/// A memory with three seat-related facts; only one mentions "window".
fn seat_memory(home: &TempDir, dir: &TempDir) -> String {
    let db = dir.path().join("m.db").to_str().unwrap().to_string();
    for (s, o) in [
        ("john", "prefers an aisle seat near the front"),
        ("john", "asked for a window seat on the LHR leg"),
        ("john", "seat upgrades are declined by default"),
    ] {
        let (ok, _, err) = areev_raw(home, &["add", "--db", &db, "--ns", "caller", s, "seat_note", o], &[]);
        assert!(ok, "{err}");
    }
    db
}

/// A System One fake that rates the candidate mentioning "window" at the
/// top level and every other candidate at the bottom one.
fn window_ranker() -> Fake {
    fake(|_, body| {
        let cands = body["state"]["candidates"].as_array().cloned().unwrap_or_default();
        let mut answers = serde_json::Map::new();
        for (id, q) in body["questions"].as_object().cloned().unwrap_or_default() {
            // The reranker asks `c<n>`; the context assembler (row A2/A3)
            // asks `rel_<n>`, `verbatim_<n>` and `intent` in the same
            // process — answer every shape so one fake serves the whole hook.
            let i: Option<usize> = id.rsplit(|ch: char| !ch.is_ascii_digit()).next().and_then(|d| d.parse().ok());
            let text = i
                .and_then(|i| cands.iter().find(|c| c["i"] == i))
                .map(|c| c["text"].to_string())
                .unwrap_or_default();
            let is_window = text.contains("window");
            match q["type"].as_str().unwrap_or("") {
                "score" => {
                    let n = q["criteria"].as_array().unwrap().len();
                    let top = if is_window { n - 1 } else { 0 };
                    let probs: serde_json::Map<String, Value> =
                        (0..n).map(|l| (l.to_string(), json!(if l == top { 1.0 } else { 0.0 }))).collect();
                    answers.insert(id, json!({"type": "score", "score": top as f64, "probabilities": probs}));
                }
                "noul" => {
                    answers.insert(id, json!({"type": "noul", "noul": if is_window { 0.9 } else { 0.1 }}));
                }
                "choice" => {
                    let opts = q["criteria"].as_object().cloned().unwrap_or_default();
                    let pick = if opts.contains_key("general") { "general".to_string() } else { opts.keys().next().cloned().unwrap_or_default() };
                    let probs: serde_json::Map<String, Value> =
                        opts.keys().map(|k| (k.clone(), json!(if *k == pick { 1.0 } else { 0.0 }))).collect();
                    answers.insert(id, json!({"type": "choice", "choice": pick, "probabilities": probs, "confidence": 1.0}));
                }
                _ => {}
            }
        }
        (200, vec![], json!({"model": "jev-fake-1", "answers": answers}))
    })
}

fn rows(stdout: &str) -> Vec<Value> {
    stdout.lines().filter(|l| !l.trim().is_empty()).map(|l| serde_json::from_str(l).unwrap()).collect()
}

fn object(row: &Value) -> String {
    row["fields"].to_string()
}

#[test]
fn search_with_decide_reranks_by_the_chain_and_reports_scores() {
    let home = TempDir::new().unwrap();
    let dir = TempDir::new().unwrap();
    let db = seat_memory(&home, &dir);
    let f = window_ranker();

    let (ok, out, err) = areev_raw(&home, &["search", "--db", &db, "--ns", "caller", "--query", "seat", "-k", "10"], &[]);
    assert!(ok, "{err}");
    let plain = rows(&out);
    assert_eq!(plain.len(), 3, "{out}");
    assert!((plain[0]["score"].as_f64().unwrap() - 1.0).abs() < 1e-6, "fusion top = 1.0: {out}");
    assert!(f.requests().is_empty());

    let (ok, out, err) = areev_raw(
        &home,
        &["search", "--db", &db, "--ns", "caller", "--query", "seat", "-k", "10", "--decide", &spec(&f)],
        &[],
    );
    assert!(ok, "{err}");
    let ranked = rows(&out);
    assert_eq!(ranked.len(), 3, "reranking never omits: {out}");
    assert!(object(&ranked[0]).contains("window"), "the chain's top pick leads: {out}");
    assert!((ranked[0]["score"].as_f64().unwrap() - 1.0).abs() < 1e-6, "{out}");
    assert!(ranked[1]["score"].as_f64().unwrap() < 0.5, "{out}");
    assert!(!f.requests().is_empty(), "the chain was asked");
    assert_eq!(f.requests()[0].1["state"]["query"], "seat");
}

#[test]
fn search_under_an_egress_policy_sends_the_reranker_pseudonymized_candidates() {
    let home = TempDir::new().unwrap();
    let dir = TempDir::new().unwrap();
    let db = dir.path().join("m.db").to_str().unwrap().to_string();
    for o in ["write to jane.doe@example.com about the seat", "seat map printed at the gate"] {
        let (ok, _, err) = areev_raw(&home, &["add", "--db", &db, "--ns", "caller", "john", "seat_note", o], &[]);
        assert!(ok, "{err}");
    }
    // Positive control: with no policy the raw address does reach the backend.
    let raw = systemone();
    let (ok, _, err) = areev_raw(
        &home,
        &["search", "--db", &db, "--ns", "caller", "--query", "seat", "--decide", &spec(&raw)],
        &[],
    );
    assert!(ok, "{err}");
    assert!(raw.requests().iter().any(|(_, b)| b.to_string().contains("jane.doe@example.com")));

    let (ok, _, err) = areev_raw(
        &home,
        &["anonymize", "set", "--db", &db, "--ns", "caller", "--policy", r#"{"mode": "egress"}"#],
        &[],
    );
    assert!(ok, "{err}");
    let f = systemone();
    let (ok, _, err) = areev_raw(
        &home,
        &["search", "--db", &db, "--ns", "caller", "--query", "seat", "--decide", &spec(&f)],
        &[],
    );
    assert!(ok, "{err}");
    let reqs = f.requests();
    assert!(!reqs.is_empty(), "positive control: the reranker was asked");
    for (_, body) in reqs {
        let s = body.to_string();
        assert!(s.contains("seat"), "{s}");
        assert!(!s.contains("jane.doe@example.com"), "raw email reached the backend: {s}");
    }
}

#[test]
fn an_explicit_rerank_cmd_wins_over_decide() {
    let Some(py) = find_python() else {
        eprintln!("skipping: no python on PATH");
        return;
    };
    let home = TempDir::new().unwrap();
    let dir = TempDir::new().unwrap();
    let db = seat_memory(&home, &dir);
    // Scores "upgrades" highest, everything else low.
    let script = dir.path().join("rr.py");
    std::fs::write(
        &script,
        "import sys, json\nr = json.load(sys.stdin)\nprint(json.dumps([5.0 if 'upgrades' in d else 1.0 for d in r['docs']]))\n",
    )
    .unwrap();
    let f = window_ranker();
    let (ok, out, err) = areev_raw(
        &home,
        &["search", "--db", &db, "--ns", "caller", "--query", "seat", "--decide", &spec(&f),
          "--rerank-cmd", &format!("{py} {}", script.display()), "--recall-deadline-ms", "30000"],
        &[],
    );
    assert!(ok, "{err}");
    let ranked = rows(&out);
    assert!(object(&ranked[0]).contains("upgrades"), "the command reranker ordered it: {out}");
    assert!(f.requests().is_empty(), "the chain is not the reranker when a command is given");
}

#[test]
fn a_bad_recall_deadline_fails_at_startup() {
    let home = TempDir::new().unwrap();
    let dir = TempDir::new().unwrap();
    let db = dir.path().join("m.db");
    let (ok, _, err) = areev_raw(
        &home,
        &["add", "--db", db.to_str().unwrap(), "a", "b", "c"],
        &[("AREEV_RECALL_DEADLINE_MS", "soon")],
    );
    assert!(!ok);
    assert!(err.contains("recall-deadline-ms"), "{err}");
    assert!(!db.exists());
}

#[test]
fn recall_hook_reranks_with_the_chain() {
    let home = TempDir::new().unwrap();
    let dir = TempDir::new().unwrap();
    let db = seat_memory(&home, &dir);
    let f = window_ranker();
    let mut c = Command::new(env!("CARGO_BIN_EXE_areev"));
    // Two hits: the assembler asks its questions only for ≥ 2 candidates.
    c.args(["recall-hook", "--db", &db, "--ns", "caller", "-k", "2", "--decide", &spec(&f)])
        .env("HOME", home.path())
        .env_remove("AREEV_DECIDE")
        .env_remove("AREEV_RERANK_CMD")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let mut child = c.spawn().unwrap();
    child.stdin.take().unwrap().write_all(br#"{"prompt": "which seat"}"#).unwrap();
    let out = child.wait_with_output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(!f.requests().is_empty(), "the hook asked the chain");
    assert!(stdout.contains("window"), "the reranked top hit is the one injected: {stdout}");
    // Row A5: the hook hands the same chain to the context assembler, which
    // asks the intent question about the prompt and the per-candidate
    // disclosure questions through it.
    let reqs = f.requests();
    let asked_intent = reqs.iter().any(|(_, body)| body["questions"].get("intent").is_some());
    let asked_disclosure = reqs.iter().any(|(_, body)| body["questions"].get("rel_0").is_some());
    assert!(asked_intent, "the assembler asked the intent question through the hook's chain");
    assert!(asked_disclosure, "the assembler asked the disclosure questions through the hook's chain");
    // The fake is calibrated and rates the aisle note off-topic, so the
    // allocator omits it regardless of budget (rule 1: code omits on a
    // calibrated number).
    assert!(!stdout.contains("aisle"), "off-topic candidate omitted: {stdout}");
}

// ---- the decision node, through `run start --decide` (proposal row C3) ----------

/// A pack carrying a decision node (`areev://decide`) and two host nodes,
/// installed into a fresh memory. Returns the memory path and the plan hash.
fn install_triage_pack(dir: &TempDir, home: &TempDir) -> (String, String) {
    let pack = dir.path().join("pack");
    std::fs::create_dir_all(pack.join("grains")).unwrap();
    std::fs::write(
        pack.join("grains/010-triage.json"),
        r#"{ "id": "triage", "type": "tool", "tool_name": "triage", "kind": "definition",
             "tool_description": "route an item", "created_at": 1788134400000,
             "executor_uri": "areev://decide", "strict": true,
             "input_schema": {"type": "object",
                              "properties": {"state": {}, "questions": {"type": "object"}},
                              "required": ["state", "questions"]},
             "decide": {"questions": {"route": {"type": "choice",
                          "instructions": "Does this item need a person today?",
                          "criteria": {"escalate": "a person must act today",
                                       "ignore": "routine"}}}} }"#,
    )
    .unwrap();
    for (n, name) in [("011", "escalate"), ("012", "file")] {
        std::fs::write(
            pack.join(format!("grains/{n}-{name}.json")),
            format!(
                r#"{{ "id": "{name}", "type": "tool", "tool_name": "{name}", "kind": "definition",
                     "tool_description": "{name} the item", "created_at": 1788134400000 }}"#
            ),
        )
        .unwrap();
    }
    std::fs::write(
        pack.join("grains/020-workflow.json"),
        r#"{ "id": "plan", "type": "workflow", "name": "triage", "created_at": 1788134400000,
             "nodes": ["triage", "escalate", "file"],
             "edges": [{"src": "triage", "dst": "escalate", "cond": "triage.answers.route.choice == \"escalate\""},
                       {"src": "triage", "dst": "file", "cond": "triage.answers.route.choice == \"ignore\""}],
             "bindings": {"triage": "grain:triage", "escalate": "grain:escalate", "file": "grain:file"} }"#,
    )
    .unwrap();
    std::fs::write(
        pack.join("pack.json"),
        r#"{ "pack": "triage", "version": "1.0.0", "namespace": "ops",
             "grains": [ "grains/010-triage.json", "grains/011-escalate.json",
                         "grains/012-file.json", "grains/020-workflow.json" ] }"#,
    )
    .unwrap();
    let db = dir.path().join("triage.db").to_str().unwrap().to_string();
    let (ok, out, err) = areev(
        home,
        &["pack", "install", pack.to_str().unwrap(), "--db", &db, "--format", "json"],
        &[],
    );
    assert!(ok, "pack install failed: {err}");
    // A decision node is neither code nor data: nothing to pin, no warning.
    assert_eq!(out["allow_executor"].as_array().map(Vec::len), Some(0), "{out}");
    let warnings = out["warnings"].as_array().cloned().unwrap_or_default();
    assert!(
        !warnings.iter().any(|w| w.to_string().contains("not a content address")),
        "the reserved decide URI must not warn: {warnings:?}"
    );
    let plan = out["grains"]
        .as_array()
        .unwrap()
        .iter()
        .find(|g| g["type"] == "workflow")
        .and_then(|g| g["hash"].as_str())
        .expect("the plan hash")
        .to_string();
    (db, plan)
}

#[cfg(not(windows))]
#[test]
fn a_decision_node_runs_through_run_start_with_decide_and_refuses_without_it() {
    let dir = TempDir::new().unwrap();
    let home = TempDir::new().unwrap();
    let (db, plan) = install_triage_pack(&dir, &home);
    let tool_cmd = r#"printf '{"done":true}'"#;

    // No backend on this host: refused at start, naming the node, before a
    // run exists (RUN-E030).
    let (ok, _out, err) = areev(
        &home,
        &["run", "--db", &db, "--ns", "ops", "start", "--workflow", &plan, "--run-id", "t-none",
          "--input", r#"{"state": "Payout failing for three days, customer furious"}"#,
          "--tool-cmd", tool_cmd],
        &[],
    );
    assert!(!ok, "a decision node without a backend must refuse");
    assert!(err.contains("RUN-E030") && err.contains("triage"), "{err}");

    // With a chain: the fake picks `escalate` (its first sorted choice), the
    // edge branches on the journaled answer, and the run finishes through
    // the escalate host node.
    let f = systemone();
    let (ok, out, err) = areev(
        &home,
        &["run", "--db", &db, "--ns", "ops", "start", "--workflow", &plan, "--run-id", "t-1",
          "--input", r#"{"state": "Payout failing for three days, customer furious"}"#,
          "--tool-cmd", tool_cmd, "--decide", &spec(&f)],
        &[],
    );
    assert!(ok, "start with --decide failed: {err}");
    assert!(out["finished"].as_str().unwrap_or("").contains("Completed"), "{out}");
    let reqs = f.requests();
    assert_eq!(reqs.len(), 1, "one decision per node visit: {reqs:?}");
    assert_eq!(reqs[0].1["state"], "Payout failing for three days, customer furious");
    assert!(reqs[0].1["questions"]["route"].is_object(), "{}", reqs[0].1);

    // The answer is an ordinary Tool execution grain in the journal, so the
    // run ↔ memory join sees it; the branch taken was the escalate one.
    let (ok, text, err) = areev_raw(&home, &["run-trace", "--db", &db, "--ns", "ops", "--run-id", "t-1"], &[]);
    assert!(ok, "run-trace failed: {err}");
    assert!(text.contains("Tool"), "the decision is a Tool execution grain in the journal: {text}");
    // `step-actions` joins the plan to its execution records, one
    // `node<TAB>hash` line each: the decision node has one, the branch it
    // chose has one, the other branch none.
    let (ok, text, err) =
        areev_raw(&home, &["step-actions", "--db", &db, "--ns", "ops", "--workflow", &plan], &[]);
    assert!(ok, "step-actions failed: {err}");
    let nodes: Vec<&str> = text.lines().filter_map(|l| l.split('\t').next()).collect();
    assert_eq!(nodes.iter().filter(|n| **n == "triage").count(), 1, "{text}");
    assert_eq!(nodes.iter().filter(|n| **n == "escalate").count(), 1, "{text}");
    assert!(!nodes.contains(&"file"), "the ignore branch did not run: {text}");

    // `verify` replays from the journal and never asks the backend again.
    let (ok, _out, err) = areev(&home, &["run", "--db", &db, "--ns", "ops", "verify", "--run-id", "t-1", "--decide", &spec(&f)], &[]);
    assert!(ok, "verify failed: {err}");
    assert_eq!(f.requests().len(), 1, "verify re-asked the backend");
}

// ---- the loop, through `loop run --decide` (proposal rows E1–E3) --------------

#[test]
fn loop_run_with_decide_reports_the_backend_and_asks_the_contradiction_sweep() {
    let home = TempDir::new().unwrap();
    let dir = TempDir::new().unwrap();
    // Three `john seat_note …` facts with distinct objects: a relation outside
    // the seeded functional set, so only a decision backend is asked whether
    // the pairs can both be true (docs/loop.md, "Decision backend").
    let db = seat_memory(&home, &dir);
    let f = systemone();
    let (ok, out, err) = areev(
        &home,
        &["loop", "--db", &db, "--ns", "caller", "run", "--decide", &spec(&f), "--min-new", "1",
          "--telemetry", "off", "--format", "json"],
        &[],
    );
    assert!(ok, "loop run --decide failed: {err}");
    // Provenance in the run report (rule 4): which backend, calibrated, how
    // many calls, how many failed.
    let report = &out["decider"];
    assert!(report["backend"].as_str().unwrap_or("").starts_with("systemone:"), "{out}");
    assert_eq!(report["calibrated"], true, "{out}");
    assert!(report["calls"].as_u64().unwrap_or(0) >= 1, "{out}");
    assert_eq!(report["failed_calls"], 0, "{out}");
    // The contradiction sweep batched the seat_note pairs into one state.
    let asked_pairs = f.requests().iter().any(|(_, b)| b["state"]["pairs"].is_object());
    assert!(asked_pairs, "the contradiction sweep asked the chain about the pairs: {:?}", f.requests());
    // The fake says "both can be true" at 0.9, so 1 − p < 0.75: nothing is
    // proposed from the judgment, and the run stays a plain deterministic pass.
    assert_eq!(out["auto_applied"], 0, "{out}");
}
