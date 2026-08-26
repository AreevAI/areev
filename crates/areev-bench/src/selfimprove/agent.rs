//! The agent loop: subprocess chat adapter + the deterministic mock agent.
//!
//! See ../../SELFIMPROVE.md and the frozen cross-module contract in mod.rs.
//! The loop drives any [`ChatBackend`] against any [`AgentEnv`]; `env::Env`
//! is the real environment, tests substitute a scripted double.

use std::io::{Read, Write};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use serde_json::{json, Value};

use super::{ChatMessage, RecordedCall, RuleId, TaskRunRecord, ToolCall, ToolOutcome, Usage};

/// One model turn: full message history + tool schemas in, one assistant
/// message + usage out. `Err` is a transport/adapter failure, never a tool
/// error (those are `role:"tool"` results in the history).
pub trait ChatBackend {
    fn chat(&mut self, messages: &[ChatMessage], tools: &Value) -> Result<(ChatMessage, Usage), String>;
}

/// The slice of `env::Env` the agent loop needs. The bench bin passes the
/// real environment (`run_task(backend, &mut env, &task.id, &task.prompt, …)`);
/// keeping this a trait is what lets the loop be tested without env internals.
pub trait AgentEnv {
    /// Tool schemas advertised to the model (the adapter request's `tools`).
    fn tools(&self) -> Value;
    /// Execute one tool call for the current task.
    fn execute(&mut self, tool: &str, arguments_json: &str) -> ToolOutcome;
    /// Score the final answer against ground truth: `Err` is the failure reason.
    fn score(&mut self, final_answer: &str) -> Result<(), String>;
    /// (rule, error-count) for every hidden rule tripped since the task began.
    fn rule_failures(&self) -> Vec<(RuleId, u32)>;
}

/// The real environment satisfies the loop's seam via its inherent methods.
impl AgentEnv for super::env::Env {
    fn tools(&self) -> Value {
        super::env::tool_schemas()
    }
    fn execute(&mut self, tool: &str, arguments_json: &str) -> ToolOutcome {
        Self::execute(self, tool, arguments_json)
    }
    fn score(&mut self, final_answer: &str) -> Result<(), String> {
        match Self::score(self, final_answer) {
            (true, _) => Ok(()),
            (false, reason) => Err(reason),
        }
    }
    fn rule_failures(&self) -> Vec<(RuleId, u32)> {
        Self::rule_failures(self)
    }
}

// ---------------------------------------------------------------------------
// CmdBackend — one `sh -c` subprocess per chat call (the --agent-cmd path)
// ---------------------------------------------------------------------------

/// Subprocess adapter: writes one request line on the child's stdin, reads
/// one response line from its stdout (SELFIMPROVE.md "Runner protocol").
pub struct CmdBackend {
    pub cmd: String,
    pub model: String,
}

/// A stuck adapter must not hang a 900-task run.
const ADAPTER_TIMEOUT: Duration = Duration::from_secs(120);

impl ChatBackend for CmdBackend {
    fn chat(&mut self, messages: &[ChatMessage], tools: &Value) -> Result<(ChatMessage, Usage), String> {
        let request = json!({
            "op": "chat",
            "model": self.model,
            "messages": messages.iter().map(|m| m.to_json()).collect::<Vec<Value>>(),
            "tools": tools,
            "temperature": 0,
        });
        let mut child = Command::new("sh")
            .arg("-c")
            .arg(&self.cmd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("spawn `sh -c {}`: {e}", self.cmd))?;
        if let Some(mut stdin) = child.stdin.take() {
            // A dead adapter surfaces via its exit status below, not this write.
            let _ = stdin.write_all(request.to_string().as_bytes());
            let _ = stdin.write_all(b"\n");
        }
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| "adapter stdout not captured".to_string())?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| "adapter stderr not captured".to_string())?;
        // Reader threads + recv_timeout: blocking reads on the calling thread
        // could hang forever on a wedged subprocess.
        let (out_tx, out_rx) = mpsc::channel();
        thread::spawn(move || {
            let mut stdout = stdout;
            let mut buf = String::new();
            let _ = stdout.read_to_string(&mut buf);
            let _ = out_tx.send(buf);
        });
        let (err_tx, err_rx) = mpsc::channel();
        thread::spawn(move || {
            let mut stderr = stderr;
            let mut buf = String::new();
            let _ = stderr.read_to_string(&mut buf);
            let _ = err_tx.send(buf);
        });
        let out = match out_rx.recv_timeout(ADAPTER_TIMEOUT) {
            Ok(s) => s,
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!(
                    "adapter timed out after {}s: {}",
                    ADAPTER_TIMEOUT.as_secs(),
                    self.cmd
                ));
            }
        };
        // stdout is closed, so the adapter should exit promptly; bounded wait
        // so a lingering child cannot hang the run either.
        let status = {
            let mut waited_ms = 0u64;
            loop {
                match child.try_wait() {
                    Ok(Some(st)) => break st,
                    Ok(None) if waited_ms < 10_000 => {
                        thread::sleep(Duration::from_millis(50));
                        waited_ms += 50;
                    }
                    Ok(None) => {
                        let _ = child.kill();
                        let _ = child.wait();
                        return Err("adapter closed stdout but did not exit".to_string());
                    }
                    Err(e) => return Err(format!("adapter wait failed: {e}")),
                }
            }
        };
        let err = err_rx.recv_timeout(Duration::from_secs(5)).unwrap_or_default();
        if !status.success() {
            return Err(format!("adapter exited with {status}: {}", excerpt(&err)));
        }
        let line = out
            .lines()
            .find(|l| !l.trim().is_empty())
            .ok_or_else(|| format!("adapter printed no output; stderr: {}", excerpt(&err)))?;
        let v: Value = serde_json::from_str(line)
            .map_err(|e| format!("unparseable adapter output ({e}); stderr: {}", excerpt(&err)))?;
        let msg = v
            .get("message")
            .and_then(ChatMessage::from_json)
            .ok_or_else(|| format!("adapter output has no message; stderr: {}", excerpt(&err)))?;
        let usage = Usage {
            prompt_tokens: v.pointer("/usage/prompt_tokens").and_then(Value::as_u64).unwrap_or(0),
            completion_tokens: v
                .pointer("/usage/completion_tokens")
                .and_then(Value::as_u64)
                .unwrap_or(0),
        };
        Ok((msg, usage))
    }
}

fn excerpt(s: &str) -> String {
    let t = s.trim();
    if t.chars().count() > 400 {
        let cut: String = t.chars().take(400).collect();
        format!("{cut}…")
    } else {
        t.to_string()
    }
}

// ---------------------------------------------------------------------------
// MockBackend — the deterministic keyless agent (CI's --mock path)
// ---------------------------------------------------------------------------

/// Deterministic agent: naive against the hidden rules unless the context it
/// is given names a frozen error code, which unlocks the corresponding
/// learned behavior (mod.rs "The fixed cross-module contract"). "Context"
/// means any of the places the harness can legitimately put it: the system
/// prompt's `## LESSONS` section (governed lessons), its `## MEMORY` section
/// (passive-recall arms), or a `[memory]` block appended to a tool result.
/// The mock models "an agent that uses whatever context it is given" — so
/// mock M arms prove the provider plumbing end-to-end and NEVER a comparison
/// between curation and retrieval (a real model decides what to do with
/// context; the mock obeys it by construction).
/// State is reconstructed from the messages slice alone — no env access, no
/// randomness, no clock. R2 (`invalid_id`) never fires here: ids are always
/// taken verbatim from search results, so there is no naive id-guessing to fix.
#[derive(Debug, Clone, Copy, Default)]
pub struct MockBackend;

impl ChatBackend for MockBackend {
    fn chat(&mut self, messages: &[ChatMessage], _tools: &Value) -> Result<(ChatMessage, Usage), String> {
        let system = messages
            .iter()
            .find(|m| m.role == "system")
            .and_then(|m| m.content.as_deref())
            .unwrap_or("");
        let user = messages
            .iter()
            .find(|m| m.role == "user")
            .and_then(|m| m.content.as_deref())
            .unwrap_or("");
        let lessons = Lessons::from_context(&context_text(system, messages));
        let intent = Intent::from_prompt(user);
        let pairs = collect_pairs(messages);
        let n = messages
            .iter()
            .filter(|m| m.role == "assistant")
            .flat_map(|m| m.tool_calls.iter())
            .count()
            + 1;
        Ok((decide(&lessons, &intent, &pairs).into_message(n), Usage::default()))
    }
}

/// Which learned behaviors the context unlocks (one per frozen code;
/// `invalid_id` is recognized in the contract but has no mock behavior).
/// "Context" is whatever [`context_text`] gathered — a governed `## LESSONS`
/// section, a passive-recall `## MEMORY` section, or a `[memory]` block on a
/// tool result. The map is identical for all three: the mock does not care
/// where a code came from, only that it was given one.
#[derive(Debug, Default)]
struct Lessons {
    customer_not_found: bool,
    approval_required: bool,
    cancel_before_refund: bool,
    invalid_timestamp: bool,
    rate_limited: bool,
}

impl Lessons {
    fn from_context(context: &str) -> Self {
        Self {
            customer_not_found: context.contains("customer_not_found"),
            approval_required: context.contains("approval_required"),
            cancel_before_refund: context.contains("cancel_before_refund"),
            invalid_timestamp: context.contains("invalid_timestamp"),
            rate_limited: context.contains("rate_limited"),
        }
    }
}

/// Everything the harness legitimately handed the agent as *context*, joined
/// for code scanning. Each chunk starts at its marker and runs to the end of
/// its string, which is what keeps the scan honest: a tool result's own error
/// body sits BEFORE its `[memory]` block, so an error the agent just hit can
/// never unlock the behavior by itself — only a provider that chose to recall
/// something can. (`## MEMORY` is appended after `## LESSONS`, so when both
/// exist the first slice already covers the second; the union is the same.)
fn context_text(system: &str, messages: &[ChatMessage]) -> String {
    let mut buf = String::new();
    for marker in ["## LESSONS", super::context::MEMORY_SECTION_HEADING] {
        if let Some(i) = system.find(marker) {
            buf.push_str(&system[i..]);
            buf.push('\n');
        }
    }
    for m in messages.iter().filter(|m| m.role == "tool") {
        let content = m.content.as_deref().unwrap_or("");
        if let Some(i) = content.find(super::context::MEMORY_INJECTION_PREFIX) {
            buf.push_str(&content[i..]);
            buf.push('\n');
        }
    }
    buf
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Refund,
    RefundAndCancel,
    LogCase,
    Lookup,
}

/// Task intent scraped from the user prompt. Parsing assumptions, aligned
/// with env.rs's templates (both splits) and kept forgiving of paraphrase:
/// the email names the target and appears `(wrapped)`, `<wrapped>`, quoted,
/// or bare; amounts are `$`-prefixed; a refund task always carries an amount
/// (the eval paraphrase drops the word "refund"); cancellation shows as
/// "cancel", "shut down", or "subscription"; local timestamps are
/// `YYYY-MM-DD[T ]HH:MM[:SS]` with either an attached zone or a detached
/// `UTC±H[H]:MM` elsewhere in the prompt; the case note is the first
/// single-quoted (else last double-quoted) string.
#[derive(Debug)]
struct Intent {
    kind: Kind,
    email: Option<String>,
    amount: f64,
    timestamp: Option<String>,
    /// Detached `UTC±H:MM` offset, minutes east of UTC.
    tz_offset_min: Option<i64>,
    note: String,
    query: String,
}

impl Intent {
    fn from_prompt(prompt: &str) -> Self {
        let quoted = quoted_strings(prompt, '"');
        let email = quoted.iter().find(|q| q.contains('@')).cloned().or_else(|| {
            prompt.split_whitespace().find(|t| t.contains('@')).map(|t| {
                t.trim_matches(|c: char| !(c.is_ascii_alphanumeric() || "@._+-".contains(c)))
                    .to_string()
            })
        });
        let name = quoted.iter().find(|q| !q.contains('@')).cloned();
        let note = quoted_strings(prompt, '\'')
            .iter()
            .find(|q| !q.trim().is_empty() && !q.contains('@'))
            .cloned()
            .or_else(|| {
                quoted
                    .iter()
                    .rev()
                    .find(|q| !q.contains('@') && Some(q.as_str()) != name.as_deref())
                    .cloned()
            })
            .unwrap_or_else(|| "case note".to_string());
        let amount = find_dollar_amount(prompt).unwrap_or(0.0);
        let timestamp = find_timestamp(prompt);
        let tz_offset_min = find_utc_offset_min(prompt);
        let lc = prompt.to_ascii_lowercase();
        let refundish = amount > 0.0 || lc.contains("refund");
        let cancelish =
            lc.contains("cancel") || lc.contains("shut down") || lc.contains("subscription");
        let kind = if refundish && cancelish {
            Kind::RefundAndCancel
        } else if refundish {
            Kind::Refund
        } else if lc.contains("case") || lc.contains("log") || timestamp.is_some() {
            Kind::LogCase
        } else {
            Kind::Lookup
        };
        let query = name.or_else(|| email.clone()).unwrap_or_default();
        Self { kind, email, amount, timestamp, tz_offset_min, note, query }
    }
}

/// One executed (call, result) pair reconstructed from the history.
struct Pair {
    name: String,
    args: Value,
    result: Value,
    is_error: bool,
    code: Option<String>,
}

fn collect_pairs(messages: &[ChatMessage]) -> Vec<Pair> {
    let mut results: std::collections::HashMap<&str, &str> = std::collections::HashMap::new();
    for m in messages {
        if m.role == "tool" {
            if let (Some(id), Some(body)) = (m.tool_call_id.as_deref(), m.content.as_deref()) {
                results.insert(id, body);
            }
        }
    }
    let mut pairs = Vec::new();
    for m in messages {
        if m.role != "assistant" {
            continue;
        }
        for tc in &m.tool_calls {
            let body = results.get(tc.id.as_str()).copied().unwrap_or("{}");
            // A passive arm's per-error hook appends a `[memory]` block AFTER
            // the env's JSON body, so the message is no longer parseable as
            // one value. Split it off: the mock reads its state from the
            // env's result and its lessons from the block (`context_text`),
            // and conflating them would make the whole result read as `{}` —
            // an unrecognized error, silently steering the flow logic.
            let json_part = body
                .split(super::context::MEMORY_INJECTION_PREFIX)
                .next()
                .unwrap_or(body);
            let result: Value = serde_json::from_str(json_part).unwrap_or_else(|_| json!({}));
            let args: Value = serde_json::from_str(&tc.arguments).unwrap_or_else(|_| json!({}));
            let is_error = result.get("error").is_some();
            let code = result.pointer("/error/code").and_then(Value::as_str).map(String::from);
            pairs.push(Pair { name: tc.name.clone(), args, result, is_error, code });
        }
    }
    pairs
}

enum Action {
    Call(String, Value),
    Finish(String),
}

impl Action {
    fn call(name: &str, args: Value) -> Self {
        Self::Call(name.to_string(), args)
    }
    fn finish(text: impl Into<String>) -> Self {
        Self::Finish(text.into())
    }
    fn into_message(self, n: usize) -> ChatMessage {
        match self {
            Self::Call(name, args) => ChatMessage {
                role: "assistant".to_string(),
                content: None,
                tool_calls: vec![ToolCall {
                    id: format!("mock_{n}"),
                    name,
                    arguments: args.to_string(),
                }],
                tool_call_id: None,
            },
            Self::Finish(text) => ChatMessage {
                role: "assistant".to_string(),
                content: Some(text),
                tool_calls: vec![],
                tool_call_id: None,
            },
        }
    }
}

fn decide(lessons: &Lessons, intent: &Intent, pairs: &[Pair]) -> Action {
    if let Some(last) = pairs.last() {
        // R6 gates everything: a trailing 429 is answered before flow logic.
        if last.code.as_deref() == Some("rate_limited") {
            if lessons.rate_limited {
                let secs =
                    last.result.pointer("/error/retry_after_s").and_then(Value::as_f64).unwrap_or(1.0);
                return Action::call("wait", json!({ "seconds": secs }));
            }
            // Naive: immediate retries, three attempts total, then give up.
            let attempts = pairs
                .iter()
                .rev()
                .take_while(|p| p.name == last.name && p.code.as_deref() == Some("rate_limited"))
                .count();
            if attempts >= 3 {
                return Action::finish("could not complete: rate limited");
            }
            return Action::call(&last.name, last.args.clone());
        }
        // Learned R6: after the wait, re-issue the call that was limited.
        if last.name == "wait" && !last.is_error && pairs.len() >= 2 {
            let prev = &pairs[pairs.len() - 2];
            if prev.code.as_deref() == Some("rate_limited") {
                return Action::call(&prev.name, prev.args.clone());
            }
        }
        // Codes without a recovery path end the task; the handled ones fall
        // through to the flow logic below.
        if let Some(code) = last.code.as_deref() {
            if !matches!(code, "approval_required" | "cancel_before_refund" | "invalid_timestamp") {
                return Action::finish(format!("could not complete: {code}"));
            }
        }
    }

    // Customer selection over everything searched so far. Naive stops at the
    // first page (R1); the customer_not_found lesson exhausts pagination.
    let searches: Vec<&Pair> =
        pairs.iter().filter(|p| p.name == "search_customers" && !p.is_error).collect();
    let mut seen: Vec<(String, String, String)> = Vec::new();
    for s in &searches {
        if let Some(arr) = s.result.get("customers").and_then(Value::as_array) {
            for c in arr {
                seen.push((
                    c.get("id").and_then(Value::as_str).unwrap_or_default().to_string(),
                    c.get("name").and_then(Value::as_str).unwrap_or_default().to_string(),
                    c.get("email").and_then(Value::as_str).unwrap_or_default().to_string(),
                ));
            }
        }
    }
    let matched = intent
        .email
        .as_deref()
        .and_then(|t| seen.iter().find(|(_, _, e)| e.eq_ignore_ascii_case(t)));
    let (cus_id, cus_name) = match matched {
        Some((id, name, _)) => (id.clone(), name.clone()),
        None => {
            if searches.is_empty() {
                return Action::call("search_customers", json!({ "query": intent.query }));
            }
            let last_s = searches[searches.len() - 1];
            if lessons.customer_not_found
                && last_s.result.get("has_more").and_then(Value::as_bool).unwrap_or(false)
            {
                let next = last_s.result.get("next_page").and_then(Value::as_u64).unwrap_or_else(
                    || last_s.args.get("page").and_then(Value::as_u64).unwrap_or(1) + 1,
                );
                return Action::call(
                    "search_customers",
                    json!({ "query": intent.query, "page": next }),
                );
            }
            match seen.first() {
                Some((id, name, _)) => (id.clone(), name.clone()),
                None => return Action::finish("could not complete: customer not found"),
            }
        }
    };

    match intent.kind {
        Kind::Lookup => {
            if let Some(g) = pairs.iter().rev().find(|p| p.name == "get_customer" && !p.is_error) {
                let balance = g.result.get("balance").and_then(Value::as_f64).unwrap_or(0.0);
                let plan =
                    g.result.pointer("/subscription/plan").and_then(Value::as_str).unwrap_or("?");
                let status =
                    g.result.pointer("/subscription/status").and_then(Value::as_str).unwrap_or("?");
                return Action::finish(format!(
                    "{cus_name}: balance ${balance:.2}, subscription {plan} ({status})"
                ));
            }
            Action::call("get_customer", json!({ "id": cus_id }))
        }
        Kind::Refund => match refund_step(lessons, intent, &cus_id, pairs) {
            Some(a) => a,
            None => Action::finish(format!("refund of ${:.2} issued for {cus_name}", intent.amount)),
        },
        Kind::RefundAndCancel => {
            let cancelled = pairs.iter().any(|p| p.name == "cancel_subscription" && !p.is_error);
            let cancel_hard_error = pairs
                .iter()
                .rev()
                .find(|p| p.name == "cancel_subscription")
                .is_some_and(|p| p.code.as_deref().is_some_and(|c| c != "rate_limited"));
            if cancel_hard_error {
                return Action::finish("could not complete: cancellation failed");
            }
            if lessons.cancel_before_refund {
                if let Some(a) = refund_step(lessons, intent, &cus_id, pairs) {
                    return a;
                }
                if !cancelled {
                    return Action::call("cancel_subscription", json!({ "customer_id": cus_id }));
                }
                Action::finish(format!("refund issued and subscription cancelled for {cus_name}"))
            } else {
                // Naive ordering: cancel first (R4). The cancel itself
                // succeeds — it is the refund afterwards that errors, and
                // refund_step gives up on that code.
                if !cancelled {
                    return Action::call("cancel_subscription", json!({ "customer_id": cus_id }));
                }
                match refund_step(lessons, intent, &cus_id, pairs) {
                    Some(a) => a,
                    None => Action::finish(format!(
                        "refund issued and subscription cancelled for {cus_name}"
                    )),
                }
            }
        }
        Kind::LogCase => {
            if let Some(done) = pairs.iter().rev().find(|p| p.name == "log_case" && !p.is_error) {
                let case_id = done.result.get("case_id").and_then(Value::as_str).unwrap_or("?");
                return Action::finish(format!("case {case_id} logged for {cus_name}"));
            }
            if pairs
                .iter()
                .any(|p| p.name == "log_case" && p.is_error && p.code.as_deref() != Some("rate_limited"))
            {
                // Naive R5 gives up on the validation error; the learned path
                // should never hit it — bail either way rather than loop.
                return Action::finish("could not complete: case rejected");
            }
            let raw = intent.timestamp.clone().unwrap_or_default();
            let ts = if lessons.invalid_timestamp {
                to_utc_z_off(&raw, intent.tz_offset_min)
            } else {
                raw
            };
            Action::call(
                "log_case",
                json!({ "customer_id": cus_id, "note": intent.note, "timestamp": ts }),
            )
        }
    }
}

/// Next refund-flow action, or None once a refund has succeeded. Naive trips
/// R3 and self-recovers (request approval, retry); the approval_required
/// lesson pre-approves amounts over $100 so the error never fires. R4 is
/// unrecoverable once billing is closed — naive gives up on that code.
fn refund_step(lessons: &Lessons, intent: &Intent, customer_id: &str, pairs: &[Pair]) -> Option<Action> {
    if pairs.iter().any(|p| p.name == "refund" && !p.is_error) {
        return None;
    }
    let last_refund_code =
        pairs.iter().rev().find(|p| p.name == "refund").and_then(|p| p.code.as_deref());
    if last_refund_code == Some("cancel_before_refund") {
        return Some(Action::finish("could not complete: cancel_before_refund"));
    }
    let hard_failures = pairs
        .iter()
        .filter(|p| p.name == "refund" && p.is_error && p.code.as_deref() != Some("rate_limited"))
        .count();
    if hard_failures >= 3 {
        return Some(Action::finish("could not complete: refund kept failing"));
    }
    let token = pairs
        .iter()
        .rev()
        .find(|p| p.name == "request_approval" && !p.is_error)
        .and_then(|p| p.result.get("approval_token").and_then(Value::as_str))
        .map(String::from);
    let last_refund_needs_approval = last_refund_code == Some("approval_required");
    let preapprove = lessons.approval_required && intent.amount > 100.0;
    if token.is_none() && (preapprove || last_refund_needs_approval) {
        return Some(Action::call(
            "request_approval",
            json!({ "reason": format!("refund of ${:.2} requested by customer", intent.amount) }),
        ));
    }
    let mut args = json!({ "customer_id": customer_id, "amount": intent.amount });
    if let Some(t) = token {
        args["approval_token"] = json!(t);
    }
    Some(Action::call("refund", args))
}

// ---------------------------------------------------------------------------
// Prompt scraping + timestamp arithmetic (std only, no regex)
// ---------------------------------------------------------------------------

fn quoted_strings(s: &str, quote: char) -> Vec<String> {
    s.split(quote)
        .enumerate()
        .filter(|(i, _)| i % 2 == 1)
        .map(|(_, q)| q.to_string())
        .collect()
}

/// Detached offset like `UTC+5:30` / `UTC-8:00`, minutes east of UTC.
fn find_utc_offset_min(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    let mut i = 0;
    while i + 6 <= b.len() {
        if &b[i..i + 3] == b"UTC" && (b[i + 3] == b'+' || b[i + 3] == b'-') {
            let sign = if b[i + 3] == b'-' { -1i64 } else { 1 };
            let mut j = i + 4;
            let start = j;
            while j < b.len() && b[j].is_ascii_digit() && j - start < 2 {
                j += 1;
            }
            if j > start
                && j + 3 <= b.len()
                && b[j] == b':'
                && b[j + 1].is_ascii_digit()
                && b[j + 2].is_ascii_digit()
            {
                let h: i64 = s[start..j].parse().ok()?;
                let m: i64 = s[j + 1..j + 3].parse().ok()?;
                return Some(sign * (h * 60 + m));
            }
        }
        i += 1;
    }
    None
}

fn find_dollar_amount(s: &str) -> Option<f64> {
    let bytes = s.as_bytes();
    for (i, b) in bytes.iter().enumerate() {
        if *b != b'$' {
            continue;
        }
        let mut j = i + 1;
        let mut seen_dot = false;
        while j < bytes.len() && (bytes[j].is_ascii_digit() || (bytes[j] == b'.' && !seen_dot)) {
            if bytes[j] == b'.' {
                seen_dot = true;
            }
            j += 1;
        }
        let mut end = j;
        if end > i + 1 && bytes[end - 1] == b'.' {
            end -= 1; // sentence period, not a decimal point
        }
        if end > i + 1 {
            if let Ok(v) = s[i + 1..end].parse::<f64>() {
                return Some(v);
            }
        }
    }
    None
}

/// First `YYYY-MM-DD[T ]HH:MM[:SS][Z|±HH:MM]` in the prompt, verbatim
/// (env's log_case templates write the local time space-separated).
fn find_timestamp(s: &str) -> Option<String> {
    let b = s.as_bytes();
    let d = |i: usize| i < b.len() && b[i].is_ascii_digit();
    let mut i = 0;
    while i + 16 <= b.len() {
        if d(i)
            && d(i + 1)
            && d(i + 2)
            && d(i + 3)
            && b[i + 4] == b'-'
            && d(i + 5)
            && d(i + 6)
            && b[i + 7] == b'-'
            && d(i + 8)
            && d(i + 9)
            && (b[i + 10] == b'T' || b[i + 10] == b' ')
            && d(i + 11)
            && d(i + 12)
            && b[i + 13] == b':'
            && d(i + 14)
            && d(i + 15)
        {
            let mut j = i + 16;
            if j + 3 <= b.len() && b[j] == b':' && d(j + 1) && d(j + 2) {
                j += 3;
            }
            if j < b.len() && b[j] == b'Z' {
                j += 1;
            } else if j + 6 <= b.len()
                && (b[j] == b'+' || b[j] == b'-')
                && d(j + 1)
                && d(j + 2)
                && b[j + 3] == b':'
                && d(j + 4)
                && d(j + 5)
            {
                j += 6;
            }
            return Some(s[i..j].to_string());
        }
        i += 1;
    }
    None
}

/// Fixed-offset → UTC `Z` conversion (the invalid_timestamp lesson). The
/// detached offset (env's "we're UTC+5:30" phrasing) applies only when the
/// timestamp itself carries no zone; an attached zone always wins. Malformed
/// input passes through verbatim; no zone at all is taken as already UTC.
fn to_utc_z_off(ts: &str, detached_offset_min: Option<i64>) -> String {
    let b = ts.as_bytes();
    if b.len() < 16 || (b[10] != b'T' && b[10] != b' ') {
        return ts.to_string();
    }
    let num = |a: usize, len: usize| -> Option<i64> { ts.get(a..a + len)?.parse::<i64>().ok() };
    let (Some(y), Some(mo), Some(da), Some(hh), Some(mi)) =
        (num(0, 4), num(5, 2), num(8, 2), num(11, 2), num(14, 2))
    else {
        return ts.to_string();
    };
    let mut j = 16;
    let mut ss = 0i64;
    if b.len() >= 19 && b[16] == b':' {
        ss = num(17, 2).unwrap_or(0);
        j = 19;
    }
    let mut off_min = detached_offset_min.unwrap_or(0);
    if j < b.len() {
        match b[j] {
            b'Z' => off_min = 0,
            b'+' | b'-' if b.len() >= j + 6 => {
                let sign = if b[j] == b'-' { -1 } else { 1 };
                if let (Some(oh), Some(om)) = (num(j + 1, 2), num(j + 4, 2)) {
                    off_min = sign * (oh * 60 + om);
                }
            }
            _ => {}
        }
    }
    let total = days_from_civil(y, mo, da) * 86_400 + hh * 3_600 + mi * 60 + ss - off_min * 60;
    let days = total.div_euclid(86_400);
    let rem = total.rem_euclid(86_400);
    let (yy, mm, dd) = civil_from_days(days);
    format!(
        "{yy:04}-{mm:02}-{dd:02}T{:02}:{:02}:{:02}Z",
        rem / 3_600,
        (rem % 3_600) / 60,
        rem % 60
    )
}

// Howard Hinnant's civil-date algorithms (proleptic Gregorian, days since
// 1970-01-01) — exact integer arithmetic, no clock.
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = y - i64::from(m <= 2);
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let doy = (153 * (m + if m > 2 { -3 } else { 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = mp + if mp < 10 { 3 } else { -9 };
    (y + i64::from(m <= 2), m, d)
}

// ---------------------------------------------------------------------------
// The agent loop
// ---------------------------------------------------------------------------

/// Fixed operator role; when lessons exist they are appended verbatim (the
/// string already carries its `## LESSONS` heading — mod.rs contract).
pub const AGENT_SYSTEM_PROMPT: &str = "You are a support-desk operator. Resolve the user's request with the available tools.\n\
Call one tool at a time and read each result before deciding the next call.\n\
When the task is done, reply with a short final answer and no tool calls.\n\
If the task cannot be completed, say so briefly in the final answer.";

/// Frame a provider's output so the prompt carries its marker EXACTLY once.
///
/// SELFIMPROVE.md's frozen contract reads as "the provider returns the body,
/// the caller adds the heading"; `context.rs` reads it the other way and has
/// each provider emit a complete section — deliberately, so the four arms
/// produce structurally identical prompt bytes the way `lessons_markdown`
/// owns its own `## LESSONS` heading. Both are defensible and the difference
/// is invisible here: the marker is added only when the provider did not
/// supply it, so a bare body still reaches the model framed (and the mock,
/// which keys on the marker, can still see it) and a complete section is
/// never double-headed.
fn framed(text: &str, marker: &str) -> String {
    if text.starts_with(marker) {
        text.to_string()
    } else {
        format!("{marker}\n{text}")
    }
}

/// Run one task to completion. `steps` counts model turns. A backend `Err`
/// records `failure_reason: "backend_error: …"` and returns — a flaky
/// provider must not kill a 900-task run. `on_event` receives one row per
/// executed tool call ({task_id, turn, tool, args, is_error, code}), a
/// {task_id, turn, error} row on backend failure, and a
/// {task_id, turn, provider_error} row whenever `context` returns `Err`.
///
/// `lessons_md` and `context` are the two ways memory can reach the prompt,
/// and the bench passes exactly one of them: the governed states (A0/B/A1/B2)
/// carry lessons and no provider, the passive arms carry a provider and no
/// lessons. Mixing them would make an arm's numbers unattributable, so the
/// XOR is asserted here rather than trusted.
///
/// A provider `Err` never fails the task: it is logged to the transcript and
/// the turn proceeds without that context — the same fail-soft philosophy as
/// the engine's LLM stages, and the report stays honest because the failure
/// is in the transcript rather than silently absorbed.
///
/// Callable from worker threads: everything borrowed across the call is per
/// worker (`backend`), per task (`env`, `on_event`), or `Sync` (`lessons_md`,
/// and `ContextProvider`'s own `Sync + Send` bound).
// Eight arguments, each one a distinct axis of a single call (backend, env,
// identity, prompt, the two context sources, the turn cap, the sink). A
// parameter struct would only move the same list behind a name that no
// caller reuses — there are four call sites in the tree, all in this crate.
#[allow(clippy::too_many_arguments)]
pub fn run_task<E: AgentEnv + ?Sized>(
    backend: &mut dyn ChatBackend,
    env: &mut E,
    task_id: &str,
    task_prompt: &str,
    lessons_md: &str,
    context: Option<&dyn super::context::ContextProvider>,
    max_turns: u32,
    mut on_event: impl FnMut(&Value),
) -> TaskRunRecord {
    debug_assert!(
        lessons_md.is_empty() || context.is_none(),
        "lessons XOR provider: a passive arm must run with the LESSONS section empty"
    );
    let mut system = if lessons_md.is_empty() {
        AGENT_SYSTEM_PROMPT.to_string()
    } else {
        format!("{AGENT_SYSTEM_PROMPT}\n\n{lessons_md}")
    };
    if let Some(provider) = context {
        match provider.task_start(task_prompt) {
            Ok(text) if !text.is_empty() => {
                system.push_str("\n\n");
                system.push_str(&framed(&text, super::context::MEMORY_SECTION_HEADING));
            }
            Ok(_) => {}
            // turn 0: before the first model call.
            Err(e) => {
                on_event(&json!({ "task_id": task_id, "turn": 0, "provider_error": e }))
            }
        }
    }
    let tools = env.tools();
    let mut messages = vec![ChatMessage::system(&system), ChatMessage::user(task_prompt)];
    let mut calls: Vec<RecordedCall> = Vec::new();
    let mut usage = Usage::default();
    let mut tool_errors = 0u32;
    let mut steps = 0u32;
    let mut final_answer = String::new();
    let mut finished = false;
    let mut backend_error: Option<String> = None;

    for turn in 1..=max_turns {
        let (msg, u) = match backend.chat(&messages, &tools) {
            Ok(ok) => ok,
            Err(e) => {
                on_event(&json!({ "task_id": task_id, "turn": turn, "error": e }));
                backend_error = Some(e);
                break;
            }
        };
        steps += 1;
        usage.prompt_tokens += u.prompt_tokens;
        usage.completion_tokens += u.completion_tokens;
        let tool_calls = msg.tool_calls.clone();
        let content = msg.content.clone();
        messages.push(msg);
        if tool_calls.is_empty() {
            final_answer = content.unwrap_or_default();
            finished = true;
            break;
        }
        for tc in &tool_calls {
            let outcome = env.execute(&tc.name, &tc.arguments);
            let args_val: Value = serde_json::from_str(&tc.arguments)
                .unwrap_or_else(|_| Value::String(tc.arguments.clone()));
            on_event(&json!({
                "task_id": task_id,
                "turn": turn,
                "tool": tc.name,
                "args": args_val,
                "is_error": outcome.is_error,
                "code": outcome.code,
            }));
            if outcome.is_error {
                tool_errors += 1;
            }
            // The per-error hook: a provider may append recall to the FAILING
            // result the model is about to read (the m-steel injection point).
            // `RecordedCall` below keeps the env's body verbatim — what the
            // environment produced is not what the harness chose to add.
            let mut recall = String::new();
            if outcome.is_error {
                if let Some(provider) = context {
                    match provider.on_tool_error(
                        task_prompt,
                        &tc.name,
                        outcome.code.as_deref().unwrap_or(""),
                        &outcome.body,
                    ) {
                        Ok(text) if !text.is_empty() => {
                            recall = format!(
                                "\n\n{}",
                                framed(&text, super::context::MEMORY_INJECTION_PREFIX)
                            );
                        }
                        Ok(_) => {}
                        Err(e) => on_event(
                            &json!({ "task_id": task_id, "turn": turn, "provider_error": e }),
                        ),
                    }
                }
            }
            if recall.is_empty() {
                messages.push(ChatMessage::tool_result(&tc.id, &outcome.body));
            } else {
                messages.push(ChatMessage::tool_result(
                    &tc.id,
                    &format!("{}{recall}", outcome.body),
                ));
            }
            calls.push(RecordedCall {
                call_id: tc.id.clone(),
                tool: tc.name.clone(),
                input_json: tc.arguments.clone(),
                output_json: outcome.body,
                is_error: outcome.is_error,
                rule: outcome.rule,
            });
        }
    }

    let (success, failure_reason) = if let Some(e) = backend_error {
        (false, format!("backend_error: {e}"))
    } else if !finished {
        // Score for the env's own bookkeeping, but turn_limit overrides.
        let _ = env.score(&final_answer);
        (false, "turn_limit".to_string())
    } else {
        match env.score(&final_answer) {
            Ok(()) => (true, String::new()),
            Err(reason) => (false, reason),
        }
    };

    TaskRunRecord {
        task_id: task_id.to_string(),
        success,
        steps,
        tool_errors,
        rule_failures: env.rule_failures(),
        calls,
        final_answer,
        usage,
        failure_reason,
    }
}

// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Clone, Copy, PartialEq)]
    enum Goal {
        Refund,
        RefundAndCancel,
        LogCase,
    }

    /// Scripted stand-in for `env::Env` implementing the frozen tool surface
    /// (mod.rs) far enough for the mock's flows. Deterministic throughout.
    struct DeskDouble {
        pages: Vec<Vec<(&'static str, &'static str, &'static str)>>,
        target_id: &'static str,
        goal: Goal,
        want_amount: f64,
        want_ts: Option<&'static str>,
        refund_429s: u32,
        refunds: Vec<(String, f64)>,
        cancels: Vec<String>,
        cases: Vec<(String, String)>,
        waits: Vec<f64>,
        approvals: Vec<String>,
        rule_errs: Vec<(RuleId, u32)>,
    }

    /// Target "Jane Doe" <jane@example.com> is on page 2, behind two decoys.
    fn pages_two() -> Vec<Vec<(&'static str, &'static str, &'static str)>> {
        vec![
            vec![
                ("cus_100", "Jane Decoy", "jane.decoy@example.com"),
                ("cus_101", "Jan Doe", "jan@example.com"),
            ],
            vec![("cus_200", "Jane Doe", "jane@example.com")],
        ]
    }

    fn pages_one() -> Vec<Vec<(&'static str, &'static str, &'static str)>> {
        vec![vec![("cus_200", "Jane Doe", "jane@example.com")]]
    }

    impl DeskDouble {
        fn new(goal: Goal, pages: Vec<Vec<(&'static str, &'static str, &'static str)>>) -> Self {
            Self {
                pages,
                target_id: "cus_200",
                goal,
                want_amount: 0.0,
                want_ts: None,
                refund_429s: 0,
                refunds: vec![],
                cancels: vec![],
                cases: vec![],
                waits: vec![],
                approvals: vec![],
                rule_errs: vec![],
            }
        }

        fn err(&mut self, rule: RuleId, code: &str, retry_after_s: Option<f64>) -> ToolOutcome {
            match self.rule_errs.iter_mut().find(|(r, _)| *r == rule) {
                Some((_, n)) => *n += 1,
                None => self.rule_errs.push((rule, 1)),
            }
            let mut e = json!({ "error": { "code": code, "message": code } });
            if let Some(s) = retry_after_s {
                e["error"]["retry_after_s"] = json!(s);
            }
            ToolOutcome {
                body: e.to_string(),
                is_error: true,
                rule: Some(rule),
                code: Some(code.to_string()),
            }
        }

        fn ok(v: Value) -> ToolOutcome {
            ToolOutcome { body: v.to_string(), is_error: false, rule: None, code: None }
        }
    }

    impl AgentEnv for DeskDouble {
        fn tools(&self) -> Value {
            json!([])
        }

        fn execute(&mut self, tool: &str, arguments_json: &str) -> ToolOutcome {
            let a: Value = serde_json::from_str(arguments_json).unwrap_or_else(|_| json!({}));
            match tool {
                "search_customers" => {
                    let page = a.get("page").and_then(Value::as_u64).unwrap_or(1) as usize;
                    let customers: Vec<Value> = self
                        .pages
                        .get(page.saturating_sub(1))
                        .map(|p| {
                            p.iter()
                                .map(|(i, n, e)| json!({ "id": i, "name": n, "email": e }))
                                .collect()
                        })
                        .unwrap_or_default();
                    let has_more = page < self.pages.len();
                    let mut out = json!({ "customers": customers, "has_more": has_more });
                    if has_more {
                        out["next_page"] = json!(page as u64 + 1);
                    }
                    Self::ok(out)
                }
                "get_customer" => {
                    let id = a.get("id").and_then(Value::as_str).unwrap_or("");
                    match self.pages.iter().flatten().find(|(i, _, _)| *i == id) {
                        Some((i, n, e)) => Self::ok(json!({
                            "id": i, "name": n, "email": e, "balance": 42.5,
                            "subscription": { "id": "sub_1", "status": "active", "plan": "pro" }
                        })),
                        None => self.err("R1", "customer_not_found", None),
                    }
                }
                "request_approval" => {
                    let t = format!("appr_{}", self.approvals.len() + 1);
                    self.approvals.push(t.clone());
                    Self::ok(json!({ "approval_token": t }))
                }
                "refund" => {
                    if self.refund_429s > 0 {
                        self.refund_429s -= 1;
                        return self.err("R6", "rate_limited", Some(7.0));
                    }
                    let cid = a.get("customer_id").and_then(Value::as_str).unwrap_or("").to_string();
                    // env-true R4: the error fires on the refund AFTER a
                    // cancellation (billing closed), never on the cancel.
                    if self.cancels.contains(&cid) {
                        return self.err("R4", "cancel_before_refund", None);
                    }
                    let amount = a.get("amount").and_then(Value::as_f64).unwrap_or(0.0);
                    let token_ok = a
                        .get("approval_token")
                        .and_then(Value::as_str)
                        .is_some_and(|t| self.approvals.iter().any(|x| x == t));
                    if amount > 100.0 && !token_ok {
                        return self.err("R3", "approval_required", None);
                    }
                    self.refunds.push((cid, amount));
                    Self::ok(json!({ "refund_id": format!("rf_{}", self.refunds.len()) }))
                }
                "cancel_subscription" => {
                    // env-true: idempotent, always succeeds.
                    let cid = a.get("customer_id").and_then(Value::as_str).unwrap_or("").to_string();
                    self.cancels.push(cid);
                    Self::ok(json!({ "status": "cancelled" }))
                }
                "log_case" => {
                    let ts = a.get("timestamp").and_then(Value::as_str).unwrap_or("").to_string();
                    if !ts.ends_with('Z') {
                        return self.err("R5", "invalid_timestamp", None);
                    }
                    let cid = a.get("customer_id").and_then(Value::as_str).unwrap_or("").to_string();
                    self.cases.push((cid, ts));
                    Self::ok(json!({ "case_id": format!("case_{}", self.cases.len()) }))
                }
                "wait" => {
                    self.waits.push(a.get("seconds").and_then(Value::as_f64).unwrap_or(0.0));
                    Self::ok(json!({ "ok": true }))
                }
                other => Self::ok(json!({ "error": { "code": "invalid_id", "message": other } })),
            }
        }

        fn score(&mut self, _final_answer: &str) -> Result<(), String> {
            match self.goal {
                Goal::Refund => {
                    if self.refunds == vec![(self.target_id.to_string(), self.want_amount)] {
                        Ok(())
                    } else {
                        Err("refund not applied to the target customer".to_string())
                    }
                }
                Goal::RefundAndCancel => {
                    let r = self
                        .refunds
                        .iter()
                        .any(|(c, amt)| c == self.target_id && *amt == self.want_amount);
                    let c = self.cancels.iter().any(|c| c == self.target_id);
                    if r && c {
                        Ok(())
                    } else {
                        Err("refund+cancel not completed for the target".to_string())
                    }
                }
                Goal::LogCase => {
                    let want = self.want_ts.unwrap_or("");
                    if self.cases.iter().any(|(c, t)| c == self.target_id && t == want) {
                        Ok(())
                    } else {
                        Err("case not logged with the UTC timestamp".to_string())
                    }
                }
            }
        }

        fn rule_failures(&self) -> Vec<(RuleId, u32)> {
            self.rule_errs.clone()
        }
    }

    fn run_mock(env: &mut DeskDouble, prompt: &str, lessons: &str) -> (TaskRunRecord, Vec<Value>) {
        run_mock_with(env, prompt, lessons, None)
    }

    fn run_mock_with(
        env: &mut DeskDouble,
        prompt: &str,
        lessons: &str,
        context: Option<&dyn crate::selfimprove::context::ContextProvider>,
    ) -> (TaskRunRecord, Vec<Value>) {
        let mut backend = MockBackend;
        let mut events = Vec::new();
        let rec = run_task(&mut backend, env, "t1", prompt, lessons, context, 20, |e| {
            events.push(e.clone())
        });
        (rec, events)
    }

    /// Local stand-in for the real providers (`context.rs`): returns canned
    /// markdown at whichever hook it was built for, so the mock's context
    /// awareness is testable without any ingest or LLM.
    struct ProviderDouble {
        at_start: Result<String, String>,
        at_error: Result<String, String>,
    }

    impl ProviderDouble {
        fn at_start(text: &str) -> Self {
            Self { at_start: Ok(text.to_string()), at_error: Ok(String::new()) }
        }
        fn at_error(text: &str) -> Self {
            Self { at_start: Ok(String::new()), at_error: Ok(text.to_string()) }
        }
        fn failing() -> Self {
            Self {
                at_start: Err("provider ingest is broken".to_string()),
                at_error: Err("provider lookup is broken".to_string()),
            }
        }
    }

    impl crate::selfimprove::context::ContextProvider for ProviderDouble {
        fn label(&self) -> &'static str {
            "m-double"
        }
        fn task_start(&self, _task_prompt: &str) -> Result<String, String> {
            self.at_start.clone()
        }
        fn on_tool_error(
            &self,
            _task_prompt: &str,
            _tool: &str,
            _code: &str,
            _body: &str,
        ) -> Result<String, String> {
            self.at_error.clone()
        }
    }

    fn tool_seq(rec: &TaskRunRecord) -> Vec<&str> {
        rec.calls.iter().map(|c| c.tool.as_str()).collect()
    }

    const REFUND_PROMPT: &str = r#"Please refund $50 to the customer "Jane Doe" with email "jane@example.com" because the order arrived damaged."#;

    #[test]
    fn naive_mock_stops_at_page_one_and_fails_the_page_two_task() {
        let mut env = DeskDouble::new(Goal::Refund, pages_two());
        env.want_amount = 50.0;
        let (rec, events) = run_mock(&mut env, REFUND_PROMPT, "");
        assert!(!rec.success);
        assert!(!rec.failure_reason.is_empty());
        // Exactly one search, never page 2 (R1 by construction).
        let searches: Vec<&RecordedCall> =
            rec.calls.iter().filter(|c| c.tool == "search_customers").collect();
        assert_eq!(searches.len(), 1);
        assert!(!searches[0].input_json.contains("page"));
        // The refund went to the page-1 decoy, not the target.
        assert_eq!(env.refunds, vec![("cus_100".to_string(), 50.0)]);
        assert!(events.iter().all(|e| e.get("task_id").is_some()));
    }

    #[test]
    fn naive_mock_retries_429_immediately_then_gives_up() {
        let mut env = DeskDouble::new(Goal::Refund, pages_one());
        env.want_amount = 50.0;
        env.refund_429s = 10; // every refund attempt is limited
        let (rec, _) = run_mock(&mut env, REFUND_PROMPT, "");
        assert!(!rec.success);
        assert_eq!(rec.calls.iter().filter(|c| c.tool == "refund").count(), 3);
        assert!(env.waits.is_empty());
        assert!(rec.final_answer.contains("rate limited"));
        assert!(rec.rule_failures.contains(&("R6", 3)));
    }

    #[test]
    fn lessons_unlock_pagination_and_wait_retry() {
        let lessons = "## LESSONS (from prior experience)\n\
            - exhaust pagination before choosing (customer_not_found)\n\
            - on rate_limited, wait retry_after_s before retrying\n";
        let mut env = DeskDouble::new(Goal::Refund, pages_two());
        env.want_amount = 50.0;
        env.refund_429s = 1;
        let (rec, events) = run_mock(&mut env, REFUND_PROMPT, lessons);
        assert!(rec.success, "failure: {}", rec.failure_reason);
        // Pagination exhausted: page 2 was fetched and the target refunded.
        assert!(rec
            .calls
            .iter()
            .any(|c| c.tool == "search_customers" && c.input_json.contains("\"page\":2")));
        assert_eq!(env.refunds, vec![("cus_200".to_string(), 50.0)]);
        // The 429 was answered with wait(retry_after_s), not a hot retry.
        assert_eq!(env.waits, vec![7.0]);
        assert!(events.iter().any(|e| e.get("tool").and_then(Value::as_str) == Some("wait")));
    }

    const BIG_REFUND_PROMPT: &str =
        r#"Please refund $150 to the customer "Jane Doe" with email "jane@example.com"."#;

    #[test]
    fn naive_mock_recovers_from_approval_required_with_the_error_recorded() {
        let mut env = DeskDouble::new(Goal::Refund, pages_one());
        env.want_amount = 150.0;
        let (rec, _) = run_mock(&mut env, BIG_REFUND_PROMPT, "");
        assert!(rec.success, "failure: {}", rec.failure_reason);
        assert_eq!(tool_seq(&rec), vec!["search_customers", "refund", "request_approval", "refund"]);
        assert!(rec.rule_failures.contains(&("R3", 1)));
    }

    #[test]
    fn approval_lesson_preapproves_large_refunds() {
        let lessons = "## LESSONS (from prior experience)\n- approval_required for refunds over $100\n";
        let mut env = DeskDouble::new(Goal::Refund, pages_one());
        env.want_amount = 150.0;
        let (rec, _) = run_mock(&mut env, BIG_REFUND_PROMPT, lessons);
        assert!(rec.success, "failure: {}", rec.failure_reason);
        assert_eq!(tool_seq(&rec), vec!["search_customers", "request_approval", "refund"]);
        assert!(rec.rule_failures.is_empty());
    }

    const REFUND_CANCEL_PROMPT: &str = r#"Refund $40 to "Jane Doe" (email "jane@example.com") and cancel her subscription."#;

    #[test]
    fn naive_mock_cancels_first_and_gives_up_on_the_ordering_error() {
        let mut env = DeskDouble::new(Goal::RefundAndCancel, pages_one());
        env.want_amount = 40.0;
        let (rec, _) = run_mock(&mut env, REFUND_CANCEL_PROMPT, "");
        assert!(!rec.success);
        // Cancel succeeds; the refund afterwards trips R4 and the mock quits.
        assert_eq!(tool_seq(&rec), vec!["search_customers", "cancel_subscription", "refund"]);
        assert!(rec.rule_failures.contains(&("R4", 1)));
        assert!(env.refunds.is_empty());
        assert!(rec.final_answer.contains("could not complete"));
    }

    #[test]
    fn ordering_lesson_refunds_before_cancelling() {
        let lessons = "## LESSONS (from prior experience)\n- cancel_before_refund: refund first\n";
        let mut env = DeskDouble::new(Goal::RefundAndCancel, pages_one());
        env.want_amount = 40.0;
        let (rec, _) = run_mock(&mut env, REFUND_CANCEL_PROMPT, lessons);
        assert!(rec.success, "failure: {}", rec.failure_reason);
        assert_eq!(tool_seq(&rec), vec!["search_customers", "refund", "cancel_subscription"]);
        assert!(rec.rule_failures.is_empty());
    }

    // env.rs Experience log_case phrasing: space-separated local time,
    // detached offset, single-quoted note, email in parentheses.
    const LOG_CASE_PROMPT: &str = "Customer Jane Doe (jane@example.com) called about a billing \
        discrepancy. Log a case with the note 'customer disputes the last invoice', timestamped \
        2026-03-01 00:30, we're UTC+5:30.";

    #[test]
    fn naive_mock_passes_the_local_timestamp_through_and_fails() {
        let mut env = DeskDouble::new(Goal::LogCase, pages_one());
        env.want_ts = Some("2026-02-28T19:00:00Z");
        let (rec, _) = run_mock(&mut env, LOG_CASE_PROMPT, "");
        assert!(!rec.success);
        assert!(rec.rule_failures.contains(&("R5", 1)));
        assert!(rec.final_answer.contains("could not complete"));
    }

    #[test]
    fn timestamp_lesson_converts_to_utc_z() {
        let lessons = "## LESSONS (from prior experience)\n- invalid_timestamp: convert to UTC Z\n";
        let mut env = DeskDouble::new(Goal::LogCase, pages_one());
        env.want_ts = Some("2026-02-28T19:00:00Z");
        let (rec, _) = run_mock(&mut env, LOG_CASE_PROMPT, lessons);
        assert!(rec.success, "failure: {}", rec.failure_reason);
        assert_eq!(env.cases, vec![("cus_200".to_string(), "2026-02-28T19:00:00Z".to_string())]);
        assert!(rec.rule_failures.is_empty());
    }

    // --- passive-recall arms: the mock uses whatever context it is given ----

    /// A `## MEMORY` section unlocks exactly what the same codes unlock from
    /// `## LESSONS` — the arms differ in where context comes from, never in
    /// what the mock does with it.
    #[test]
    fn memory_section_unlocks_the_same_behaviors_as_lessons() {
        let recall = "- `search_customers` results were incomplete (customer_not_found)\n\
                      - `refund` returned rate_limited with retry_after_s\n";
        let provider = ProviderDouble::at_start(recall);
        let mut env = DeskDouble::new(Goal::Refund, pages_two());
        env.want_amount = 50.0;
        env.refund_429s = 1;
        let (rec, _) = run_mock_with(&mut env, REFUND_PROMPT, "", Some(&provider));
        assert!(rec.success, "failure: {}", rec.failure_reason);
        assert!(rec
            .calls
            .iter()
            .any(|c| c.tool == "search_customers" && c.input_json.contains("\"page\":2")));
        assert_eq!(env.refunds, vec![("cus_200".to_string(), 50.0)]);
        assert_eq!(env.waits, vec![7.0]);
    }

    /// The per-error hook (m-steel's shape): nothing at task start, recall
    /// appended to the failing result itself, and the mock adapts from there.
    #[test]
    fn memory_block_on_a_failing_tool_result_unlocks_the_wait_retry() {
        let provider =
            ProviderDouble::at_error("- refund previously answered rate_limited; a wait helped\n");
        let mut env = DeskDouble::new(Goal::Refund, pages_one());
        env.want_amount = 50.0;
        env.refund_429s = 1;
        let (rec, _) = run_mock_with(&mut env, REFUND_PROMPT, "", Some(&provider));
        assert!(rec.success, "failure: {}", rec.failure_reason);
        assert_eq!(env.waits, vec![7.0], "the 429 was answered with a wait, not a hot retry");
    }

    /// The negative half of the test above, and the honesty check on the
    /// scan: the 429 body itself carries the string `rate_limited`, and it
    /// must NOT unlock anything on its own — only a provider that chose to
    /// recall something can. A provider that returns empty leaves the agent
    /// exactly as naive as it was.
    #[test]
    fn an_error_body_alone_never_unlocks_a_behavior() {
        let provider = ProviderDouble::at_error("");
        let mut env = DeskDouble::new(Goal::Refund, pages_one());
        env.want_amount = 50.0;
        env.refund_429s = 10;
        let (rec, _) = run_mock_with(&mut env, REFUND_PROMPT, "", Some(&provider));
        assert!(!rec.success);
        assert!(env.waits.is_empty(), "no lesson, no wait — the hot-retry storm stands");
        assert_eq!(rec.calls.iter().filter(|c| c.tool == "refund").count(), 3);
    }

    /// Both injection points, pinned to the byte: the section heading in the
    /// system prompt and the block marker on the tool result are the contract
    /// the mock (and any live model's prompt) reads.
    #[test]
    fn context_reaches_the_prompt_in_the_documented_shape() {
        #[derive(Default)]
        struct RecordingMock {
            inner: MockBackend,
            systems: Vec<String>,
            tool_bodies: Vec<String>,
        }
        impl ChatBackend for RecordingMock {
            fn chat(
                &mut self,
                messages: &[ChatMessage],
                tools: &Value,
            ) -> Result<(ChatMessage, Usage), String> {
                if let Some(s) =
                    messages.iter().find(|m| m.role == "system").and_then(|m| m.content.clone())
                {
                    self.systems.push(s);
                }
                self.tool_bodies = messages
                    .iter()
                    .filter(|m| m.role == "tool")
                    .filter_map(|m| m.content.clone())
                    .collect();
                self.inner.chat(messages, tools)
            }
        }

        let provider = ProviderDouble {
            at_start: Ok("start-recall".to_string()),
            at_error: Ok("error-recall".to_string()),
        };
        let mut backend = RecordingMock::default();
        let mut env = DeskDouble::new(Goal::Refund, pages_one());
        env.want_amount = 50.0;
        env.refund_429s = 1;
        let rec = run_task(
            &mut backend,
            &mut env,
            "t-shape",
            REFUND_PROMPT,
            "",
            Some(&provider),
            20,
            |_| {},
        );
        assert!(rec.success, "failure: {}", rec.failure_reason);
        let system = &backend.systems[0];
        assert!(
            system.starts_with(AGENT_SYSTEM_PROMPT),
            "the operator role stays first: {system:?}"
        );
        assert!(
            system.ends_with("\n\n## MEMORY (from passive recall)\nstart-recall"),
            "memory section shape: {system:?}"
        );
        assert!(!system.contains("## LESSONS"), "an M arm carries no lessons: {system:?}");
        let errored = backend
            .tool_bodies
            .iter()
            .find(|b| b.contains("rate_limited"))
            .expect("the 429 result");
        assert!(
            errored.ends_with("\n\n[memory] Relevant past experience:\nerror-recall"),
            "memory block shape: {errored:?}"
        );
        // The env's own body is untouched in the RECORD — the harness only
        // added context to what the model reads.
        let recorded = rec.calls.iter().find(|c| c.is_error).expect("a recorded failure");
        assert!(!recorded.output_json.contains("[memory]"), "{}", recorded.output_json);
    }

    /// The other half of the framing seam, and the one the REAL providers
    /// take: `context.rs` returns a complete section (heading included), so
    /// the prompt must carry that heading exactly once — a doubled heading
    /// would still "work" (the mock scans for codes) while quietly corrupting
    /// every live arm's prompt, which is why it is asserted rather than
    /// assumed.
    #[test]
    fn a_provider_that_frames_its_own_output_is_not_double_headed() {
        let heading = crate::selfimprove::context::MEMORY_SECTION_HEADING;
        let prefix = crate::selfimprove::context::MEMORY_INJECTION_PREFIX;
        assert_eq!(
            framed(&format!("{heading}\n- already framed\n"), heading),
            format!("{heading}\n- already framed\n"),
            "a complete section passes through untouched"
        );
        assert_eq!(framed("- bare body\n", heading), format!("{heading}\n- bare body\n"));
        assert_eq!(framed("- bare body\n", prefix), format!("{prefix}\n- bare body\n"));
        // End to end through a real provider: exactly one heading, and the
        // mock still sees the codes inside it.
        let provider = crate::selfimprove::context::AllProvider::build(vec![
            crate::selfimprove::context::ExperienceGrain {
                task_id: "exp-1".to_string(),
                tool: "refund".to_string(),
                input_json: "{}".to_string(),
                output_json: r#"{"error":{"code":"rate_limited"}}"#.to_string(),
                is_error: true,
                code: Some("rate_limited".to_string()),
                rendered: "- `refund` failed with `rate_limited`".to_string(),
            },
        ]);
        let section =
            crate::selfimprove::context::ContextProvider::task_start(&provider, "any").unwrap();
        let framed_once = framed(&section, heading);
        assert_eq!(framed_once.matches(heading).count(), 1, "{framed_once:?}");
        assert!(Lessons::from_context(&framed_once).rate_limited);
    }

    /// A provider that errors is logged and stepped over — never fatal. The
    /// run must be indistinguishable from the no-provider run, because the
    /// engine's LLM stages fail soft the same way and the transcript, not a
    /// dead run, is what keeps the report honest.
    #[test]
    fn a_failing_provider_is_logged_and_the_task_continues() {
        let provider = ProviderDouble::failing();
        let mut env = DeskDouble::new(Goal::Refund, pages_one());
        env.want_amount = 50.0;
        env.refund_429s = 10;
        let (rec, events) = run_mock_with(&mut env, REFUND_PROMPT, "", Some(&provider));
        assert!(!rec.failure_reason.starts_with("backend_error"), "{}", rec.failure_reason);
        // Identical to the naive no-context run (an_error_body_alone…).
        assert_eq!(rec.calls.iter().filter(|c| c.tool == "refund").count(), 3);
        assert!(env.waits.is_empty());
        let errors: Vec<&Value> =
            events.iter().filter(|e| e.get("provider_error").is_some()).collect();
        assert_eq!(
            errors[0]["turn"],
            json!(0),
            "the task_start failure is logged before the first turn"
        );
        assert_eq!(errors[0]["task_id"], json!("t1"));
        assert!(
            errors.iter().any(|e| e["turn"].as_u64().unwrap_or(0) > 0),
            "each failing per-error lookup is logged too: {errors:?}"
        );
    }

    #[test]
    fn to_utc_z_handles_offsets_and_rollover() {
        assert_eq!(to_utc_z_off("2026-03-01T00:30:00+05:30", None), "2026-02-28T19:00:00Z");
        assert_eq!(to_utc_z_off("2026-12-31T20:00:00-08:00", None), "2027-01-01T04:00:00Z");
        assert_eq!(to_utc_z_off("2026-08-12T14:30:00Z", None), "2026-08-12T14:30:00Z");
        assert_eq!(to_utc_z_off("2026-08-12T14:30+02:00", None), "2026-08-12T12:30:00Z");
        assert_eq!(to_utc_z_off("not a timestamp", None), "not a timestamp");
        // Detached offset + space-separated local time (env's phrasing).
        assert_eq!(to_utc_z_off("2026-03-01 00:30", Some(330)), "2026-02-28T19:00:00Z");
        assert_eq!(to_utc_z_off("2026-07-09 23:45", Some(-480)), "2026-07-10T07:45:00Z");
        // An attached zone wins over a detached one.
        assert_eq!(to_utc_z_off("2026-08-12T14:30:00Z", Some(330)), "2026-08-12T14:30:00Z");
    }

    #[test]
    fn find_timestamp_scans_out_of_prose() {
        assert_eq!(
            find_timestamp("logged at 2026-03-01T00:30:00+05:30."),
            Some("2026-03-01T00:30:00+05:30".to_string())
        );
        assert_eq!(find_timestamp("meet at 2026-08-12T14:30Z ok"), Some("2026-08-12T14:30Z".to_string()));
        assert_eq!(
            find_timestamp("timestamped 2026-03-14 07:30, we're UTC+5:30."),
            Some("2026-03-14 07:30".to_string())
        );
        assert_eq!(find_timestamp("no dates here"), None);
        assert_eq!(find_utc_offset_min("we're UTC+5:30."), Some(330));
        assert_eq!(find_utc_offset_min("(our office is UTC-8:00)"), Some(-480));
        assert_eq!(find_utc_offset_min("plain UTC here"), None);
    }

    #[test]
    fn intent_parses_email_amount_and_kind() {
        let i = Intent::from_prompt(REFUND_CANCEL_PROMPT);
        assert_eq!(i.kind, Kind::RefundAndCancel);
        assert_eq!(i.email.as_deref(), Some("jane@example.com"));
        assert_eq!(i.amount, 40.0);
        assert_eq!(i.query, "Jane Doe");
    }

    /// Pins the mock's parsing to env.rs's actual template phrasing — both
    /// splits, all three template families.
    #[test]
    fn intent_parses_env_real_templates() {
        let exp_refund = "Customer Alice Chen (alice.chen@exp.example) was double-charged on \
            their last invoice. Refund $84 to their account and confirm what you did.";
        let i = Intent::from_prompt(exp_refund);
        assert_eq!(i.kind, Kind::Refund);
        assert_eq!(i.email.as_deref(), Some("alice.chen@exp.example"));
        assert_eq!(i.amount, 84.0);

        // The eval paraphrase never says "refund" — the $ amount decides.
        let eval_refund = "Rosa Novak <rosa.novak@eval.example> reports a duplicate charge. \
            Please put $120 back on their account and summarize the outcome.";
        let i = Intent::from_prompt(eval_refund);
        assert_eq!(i.kind, Kind::Refund);
        assert_eq!(i.email.as_deref(), Some("rosa.novak@eval.example"));
        assert_eq!(i.amount, 120.0);

        // The eval paraphrase says "shut down", not "cancel".
        let eval_rc = "Quinn Moreau <quinn.moreau@eval.example> has decided to leave. Issue a \
            $45 refund for the remaining period and shut down their subscription.";
        let i = Intent::from_prompt(eval_rc);
        assert_eq!(i.kind, Kind::RefundAndCancel);
        assert_eq!(i.amount, 45.0);

        let eval_log = "Zoe Sato <zoe.sato@eval.example> rang regarding login trouble. Record a \
            case with note 'customer locked out after password rotation' for 2026-07-09 23:45 \
            local time (our office is UTC-8:00).";
        let i = Intent::from_prompt(eval_log);
        assert_eq!(i.kind, Kind::LogCase);
        assert_eq!(i.email.as_deref(), Some("zoe.sato@eval.example"));
        assert_eq!(i.note, "customer locked out after password rotation");
        assert_eq!(i.timestamp.as_deref(), Some("2026-07-09 23:45"));
        assert_eq!(i.tz_offset_min, Some(-480));
    }

    struct AlwaysTool;
    impl ChatBackend for AlwaysTool {
        fn chat(&mut self, messages: &[ChatMessage], _tools: &Value) -> Result<(ChatMessage, Usage), String> {
            let n = messages.len();
            Ok((
                ChatMessage {
                    role: "assistant".to_string(),
                    content: None,
                    tool_calls: vec![ToolCall {
                        id: format!("c{n}"),
                        name: "wait".to_string(),
                        arguments: "{\"seconds\":0}".to_string(),
                    }],
                    tool_call_id: None,
                },
                Usage::default(),
            ))
        }
    }

    #[test]
    fn run_task_enforces_the_turn_limit() {
        let mut backend = AlwaysTool;
        let mut env = DeskDouble::new(Goal::Refund, pages_one());
        let rec = run_task(&mut backend, &mut env, "t-limit", "loop forever", "", None, 3, |_| {});
        assert!(!rec.success);
        assert_eq!(rec.failure_reason, "turn_limit");
        assert_eq!(rec.steps, 3);
        assert_eq!(rec.calls.len(), 3);
    }

    struct FailBackend;
    impl ChatBackend for FailBackend {
        fn chat(&mut self, _m: &[ChatMessage], _t: &Value) -> Result<(ChatMessage, Usage), String> {
            Err("boom".to_string())
        }
    }

    #[test]
    fn run_task_survives_a_backend_error() {
        let mut backend = FailBackend;
        let mut env = DeskDouble::new(Goal::Refund, pages_one());
        let mut events = Vec::new();
        let rec = run_task(&mut backend, &mut env, "t-err", "anything", "", None, 5, |e| {
            events.push(e.clone())
        });
        assert!(!rec.success);
        assert_eq!(rec.failure_reason, "backend_error: boom");
        assert_eq!(rec.steps, 0);
        assert!(events.iter().any(|e| e.get("error").is_some()));
    }

    // Subprocess round-trips need a POSIX `sh` (CmdBackend's own contract);
    // the mock/loop tests above cover every platform.
    #[cfg(unix)]
    mod subprocess {
        use super::*;

        #[test]
        fn cmd_backend_round_trips_an_echo_adapter() {
            let cmd = r#"python3 -c 'import sys,json
req=json.loads(sys.stdin.readline())
assert req["op"]=="chat" and req["temperature"]==0 and isinstance(req["messages"],list)
print(json.dumps({"message":{"role":"assistant","content":req["model"],"tool_calls":[{"id":"c1","type":"function","function":{"name":"wait","arguments":"{\"seconds\":1}"}}]},"usage":{"prompt_tokens":11,"completion_tokens":5}}))'"#;
            let mut b = CmdBackend { cmd: cmd.to_string(), model: "test-model".to_string() };
            let msgs = [ChatMessage::system("s"), ChatMessage::user("u")];
            let (msg, usage) = b.chat(&msgs, &json!([])).expect("round trip");
            assert_eq!(msg.content.as_deref(), Some("test-model"));
            assert_eq!(msg.tool_calls.len(), 1);
            assert_eq!(msg.tool_calls[0].name, "wait");
            assert_eq!(msg.tool_calls[0].arguments, "{\"seconds\":1}");
            assert_eq!(usage.prompt_tokens, 11);
            assert_eq!(usage.completion_tokens, 5);
        }

        #[test]
        fn cmd_backend_reports_nonzero_exit_with_stderr() {
            let mut b = CmdBackend {
                cmd: "echo boom >&2; exit 3".to_string(),
                model: "m".to_string(),
            };
            let err = b.chat(&[ChatMessage::user("u")], &json!([])).unwrap_err();
            assert!(err.contains("boom"), "missing stderr excerpt: {err}");
            assert!(err.contains("exited"), "missing exit status: {err}");
        }

        #[test]
        fn cmd_backend_rejects_unparseable_output() {
            let mut b = CmdBackend { cmd: "echo not-json".to_string(), model: "m".to_string() };
            let err = b.chat(&[ChatMessage::user("u")], &json!([])).unwrap_err();
            assert!(err.contains("unparseable"), "unexpected error: {err}");
        }

        #[test]
        fn cmd_backend_round_trips_the_openrouter_script_selfcheck() {
            let script = format!(
                "{}/scripts/openrouter_toolcall.py",
                env!("CARGO_MANIFEST_DIR")
            );
            let mut b = CmdBackend {
                cmd: format!("python3 '{script}' canned/model --selfcheck"),
                model: "canned/model".to_string(),
            };
            let (msg, usage) = b.chat(&[ChatMessage::user("ping")], &json!([])).expect("selfcheck");
            assert_eq!(msg.tool_calls.len(), 1);
            assert_eq!(msg.tool_calls[0].name, "search_customers");
            assert_eq!(usage.prompt_tokens, 42);
            assert_eq!(usage.completion_tokens, 7);
        }
    }
}
