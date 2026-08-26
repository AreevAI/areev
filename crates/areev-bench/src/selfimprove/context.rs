//! context — the passive-memory arms (SELFIMPROVE.md "Bench 2: the
//! passive-memory arms", whose "context-provider contract" is frozen).
//!
//! Four arms answer "does the LOOP add value over the STORE?". They all read
//! the SAME captured experience (Tool grains only — lessons live as Facts, so
//! nothing leaks between arms) and differ only in what reaches the prompt:
//!
//!   m-steel  per-error structured retrieval at the decision point
//!   m-all    the whole failure history at task start (information upper bound)
//!   m-llm    the history summarized once into operator notes (cost recorded)
//!   m-cmd    any external provider over the newline-delimited JSON protocol
//!
//! ## Two rules the whole module is built around
//!
//! **Ingest happens at construction.** Every provider is immutable afterwards
//! and its trait methods take `&self`, which is what makes parallel eval
//! workers safe. `CmdProvider` keeps a `Mutex` purely to serialize calls into
//! its one subprocess — the provider's *content* is still frozen at spawn.
//!
//! **No silent caps.** Every truncation appends a line saying how much was
//! dropped. A provider that quietly cut its own history would make the arm
//! look weaker than the retrieval it stands for, which is exactly the
//! criticism these arms exist to pre-empt.
//!
//! ## Prompt framing belongs to the provider, not the caller
//!
//! `task_start` returns a COMPLETE section (heading included) and
//! `on_tool_error` a COMPLETE block (prefix included), mirroring
//! `memory::lessons_markdown`, which owns its own `## LESSONS` heading. Two
//! reasons: the four arms then produce structurally identical prompt bytes,
//! which is what makes them comparable; and the mock agent keys on the
//! [`MEMORY_SECTION_HEADING`] / [`MEMORY_INJECTION_PREFIX`] markers, so a
//! provider that emitted a bare body would read as "no memory at all" rather
//! than failing loudly.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::{mpsc, Mutex};
use std::thread;
use std::time::Duration;

use serde_json::{json, Value};

use super::agent::{ChatBackend, CmdBackend};
use super::{ChatMessage, Usage};

/// The system-prompt heading every arm's `task_start` output carries.
pub const MEMORY_SECTION_HEADING: &str = "## MEMORY (from passive recall)";
/// The prefix every arm's non-empty `on_tool_error` output carries.
pub const MEMORY_INJECTION_PREFIX: &str = "[memory] Relevant past experience:";

/// `m-steel`: chars per injection.
const STEEL_CAP: usize = 4_000;
/// `m-all`: chars per task-start section.
const ALL_CAP: usize = 24_000;
/// `m-llm`: chars of summarizer notes carried into every prompt.
const LLM_NOTES_CAP: usize = 4_000;
/// `m-llm`: chars of history handed to the summarizer.
const LLM_INPUT_CAP: usize = 200_000;
/// Chars of a tool-result body kept in a rendered line.
const BODY_EXCERPT_CHARS: usize = 160;
/// A wedged external provider must not hang a 900-task run.
const PROVIDER_TIMEOUT: Duration = Duration::from_secs(120);

// ---------------------------------------------------------------------------
// The frozen contract
// ---------------------------------------------------------------------------

/// One experience-phase tool call, as the arms see it.
#[derive(Debug, Clone, PartialEq)]
pub struct ExperienceGrain {
    pub task_id: String,
    pub tool: String,
    pub input_json: String,
    pub output_json: String,
    pub is_error: bool,
    /// The frozen error code (mod.rs contract) when `is_error`.
    pub code: Option<String>,
    /// The one-line form the arms render this call as ([`render_line`]).
    pub rendered: String,
}

impl ExperienceGrain {
    /// Wire form for the `--context-cmd` ingest payload.
    pub fn to_json(&self) -> Value {
        json!({
            "task_id": self.task_id,
            "tool": self.tool,
            "input_json": self.input_json,
            "output_json": self.output_json,
            "is_error": self.is_error,
            "code": self.code,
            "rendered": self.rendered,
        })
    }
}

/// A passive-memory arm. `Sync + Send` with `&self` methods: providers are
/// immutable after construction, so eval workers can share one.
pub trait ContextProvider: Sync + Send {
    fn label(&self) -> &'static str;
    /// The `## MEMORY (from passive recall)` section for the system prompt at
    /// task start; empty string = no section.
    fn task_start(&self, task_prompt: &str) -> Result<String, String>;
    /// The `[memory]` block appended to a failing tool result; empty = nothing.
    fn on_tool_error(
        &self,
        task_prompt: &str,
        tool: &str,
        code: &str,
        body: &str,
    ) -> Result<String, String>;
}

// ---------------------------------------------------------------------------
// Rendering — one mechanical line per grain
// ---------------------------------------------------------------------------

/// The one-line form of a recorded tool call, shared by every arm and by
/// `memory::experience_grains`.
///
/// Deliberately NOT `areev_cal::render_grain_text_line`: the product's
/// one-line form for a Tool grain is `refund [FAIL]` (the `"tool"` arm of
/// `known_type_summary`), which carries neither the error code nor any of the
/// body. Those two are the entire payload the m-arms exist to inject, so the
/// product renderer would hand every arm an empty comparison. The shape below
/// mirrors the lesson renderer's (`` `tool` ``, backticked code, then detail)
/// so the arms and the governed lessons read alike in a prompt.
///
/// The result is always ONE line: the body excerpt is whitespace-collapsed and
/// capped, so a multi-line JSON error can never break the bullet list.
pub fn render_line(tool: &str, is_error: bool, code: Option<&str>, body: &str) -> String {
    if !is_error {
        return format!("- `{tool}` call succeeded");
    }
    let excerpt = truncate_chars(&collapse_ws(body), BODY_EXCERPT_CHARS);
    match code {
        Some(code) => format!("- `{tool}` call failed with `{code}`: {excerpt}"),
        None => format!("- `{tool}` call failed: {excerpt}"),
    }
}

/// Whitespace-collapse to a single line (a "rendered line" must be one line).
fn collapse_ws(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn truncate_chars(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        return s.to_string();
    }
    let kept: String = s.chars().take(n).collect();
    format!("{kept}…")
}

/// Join `lines` (each newline-terminated) up to `cap` chars, appending
/// `note(n)` when `n` lines had to be dropped. Returns the text and that
/// dropped count.
///
/// The cap is enforced by dropping WHOLE lines — nothing is ever sliced, so no
/// output can split a UTF-8 sequence or a half-written bullet. Room for the
/// note is reserved before each line is admitted, so the returned string is
/// `<= cap` whenever `cap` exceeds the longest note the caller can produce.
///
/// `note` is called once per candidate line to size that reserve, so it must
/// be PURE — a caller that wants to log a truncation logs it off the returned
/// count, not from inside the closure.
fn cap_lines(lines: &[String], cap: usize, note: &dyn Fn(usize) -> String) -> (String, usize) {
    let mut out = String::new();
    let mut used = 0usize;
    for (i, line) in lines.iter().enumerate() {
        let line_chars = line.chars().count() + 1; // + '\n'
        let left_after = lines.len() - i - 1;
        let reserve = if left_after == 0 {
            0
        } else {
            note(left_after).chars().count()
        };
        if used + line_chars + reserve > cap {
            let dropped = lines.len() - i;
            out.push_str(&note(dropped));
            return (out, dropped);
        }
        out.push_str(line);
        out.push('\n');
        used += line_chars;
    }
    (out, 0)
}

/// Wrap a body as the task-start section; empty body ⇒ empty section.
fn section(body: &str) -> String {
    if body.trim().is_empty() {
        return String::new();
    }
    format!("{MEMORY_SECTION_HEADING}\n{body}")
}

/// Wrap a body as the tool-error injection; empty body ⇒ nothing appended.
fn injection(body: &str) -> String {
    if body.trim().is_empty() {
        return String::new();
    }
    format!("{MEMORY_INJECTION_PREFIX}\n{body}")
}

/// `(tool → total calls, tool → errors)`, sorted by tool for stable bytes.
fn tool_counts(grains: &[ExperienceGrain]) -> BTreeMap<&str, (usize, usize)> {
    let mut counts: BTreeMap<&str, (usize, usize)> = BTreeMap::new();
    for g in grains {
        let e = counts.entry(g.tool.as_str()).or_insert((0, 0));
        e.0 += 1;
        if g.is_error {
            e.1 += 1;
        }
    }
    counts
}

fn counts_line(tool: &str, calls: usize, errors: usize) -> String {
    format!("`{tool}`: {calls} calls, {errors} errors")
}

// ---------------------------------------------------------------------------
// m-steel — per-error structured retrieval
// ---------------------------------------------------------------------------

/// The "retrieval with a perfect hook" arm: on a tool failure, every past
/// grain matching that exact `(tool, code)` is injected at the decision point.
/// A better hook than semantic similarity would get, by construction.
#[derive(Debug)]
pub struct SteelProvider {
    /// `(tool, code)` → rendered lines in RECORDING order (oldest first).
    index: BTreeMap<(String, String), Vec<String>>,
}

impl SteelProvider {
    /// `grains` arrive in recording order; "most recent" is therefore later.
    /// Only error grains carry a code, so only they are indexed — a lookup is
    /// always by `(tool, code)`.
    pub fn build(grains: Vec<ExperienceGrain>) -> SteelProvider {
        let mut index: BTreeMap<(String, String), Vec<String>> = BTreeMap::new();
        for g in grains {
            if let Some(code) = g.code.filter(|_| g.is_error) {
                index
                    .entry((g.tool, code))
                    .or_default()
                    .push(g.rendered);
            }
        }
        SteelProvider { index }
    }
}

impl ContextProvider for SteelProvider {
    fn label(&self) -> &'static str {
        "m-steel"
    }

    fn task_start(&self, _task_prompt: &str) -> Result<String, String> {
        // Nothing at task start: this arm's whole claim is that the hook fires
        // at the decision point, not that the prompt is pre-loaded.
        Ok(String::new())
    }

    fn on_tool_error(
        &self,
        _task_prompt: &str,
        tool: &str,
        code: &str,
        _body: &str,
    ) -> Result<String, String> {
        let Some(hits) = self.index.get(&(tool.to_string(), code.to_string())) else {
            return Ok(String::new());
        };
        // Most recent first: recording order reversed.
        let lines: Vec<String> = hits.iter().rev().cloned().collect();
        let (body, _) = cap_lines(&lines, STEEL_CAP, &|n| {
            format!("… ({n} earlier occurrences not shown)\n")
        });
        Ok(injection(&body))
    }
}

// ---------------------------------------------------------------------------
// m-all — the information upper bound
// ---------------------------------------------------------------------------

/// The "you under-retrieved" arm: the entire failure history at task start,
/// grouped by tool, on a generous budget.
#[derive(Debug)]
pub struct AllProvider {
    /// Precomputed at build — the provider is immutable afterwards.
    body: String,
}

impl AllProvider {
    pub fn build(grains: Vec<ExperienceGrain>) -> AllProvider {
        let counts = tool_counts(&grains);
        // Group errors by tool; `BTreeMap` + recording order within a group
        // keeps the prompt bytes stable across runs.
        let mut by_tool: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
        for g in grains.iter().filter(|g| g.is_error) {
            by_tool.entry(g.tool.as_str()).or_default().push(&g.rendered);
        }
        let mut lines = vec![
            "Every tool-call failure recorded in earlier runs.".to_string(),
            String::new(),
        ];
        for (tool, rendered) in &by_tool {
            let (calls, errors) = counts.get(tool).copied().unwrap_or((0, 0));
            lines.push(counts_line(tool, calls, errors));
            lines.extend(rendered.iter().map(|r| (*r).to_string()));
            lines.push(String::new());
        }
        let body = if by_tool.is_empty() {
            String::new()
        } else {
            cap_lines(&lines, ALL_CAP, &|n| {
                format!("… ({n} more history lines not shown — {ALL_CAP}-char cap)\n")
            })
            .0
        };
        AllProvider { body }
    }
}

impl ContextProvider for AllProvider {
    fn label(&self) -> &'static str {
        "m-all"
    }

    fn task_start(&self, _task_prompt: &str) -> Result<String, String> {
        Ok(section(&self.body))
    }

    fn on_tool_error(
        &self,
        _task_prompt: &str,
        _tool: &str,
        _code: &str,
        _body: &str,
    ) -> Result<String, String> {
        // The whole history is already in the prompt: injecting again would
        // price this arm's tokens twice.
        Ok(String::new())
    }
}

// ---------------------------------------------------------------------------
// m-llm — extraction at write time
// ---------------------------------------------------------------------------

const SUMMARIZER_SYSTEM: &str = "You are building memory notes for a support-desk agent. \
From the tool-call history below, write concise markdown bullet notes capturing recurring \
failure patterns worth remembering for future tasks.";

/// The "LLM-memory product" arm: ONE summarizer call over the raw history at
/// construction, its notes into every prompt. The call's cost is retained
/// (`usage`) because it is part of what this arm charges and the governed
/// arm does not.
#[derive(Debug)]
pub struct LlmProvider {
    notes: String,
    usage: Usage,
}

impl LlmProvider {
    /// One summarizer call over the agent chat-adapter protocol
    /// (SELFIMPROVE.md "Runner protocol"), reusing [`CmdBackend`]. `model` is
    /// empty: the adapter command pins the model, as it does for the agent.
    pub fn build(chat_cmd: &str, grains: Vec<ExperienceGrain>) -> Result<LlmProvider, String> {
        let input = summarizer_input(&grains);
        let mut backend = CmdBackend { cmd: chat_cmd.to_string(), model: String::new() };
        let messages = [ChatMessage::system(SUMMARIZER_SYSTEM), ChatMessage::user(&input)];
        let (msg, usage) = backend
            .chat(&messages, &json!([]))
            .map_err(|e| format!("m-llm summarizer: {e}"))?;
        let raw = msg.content.unwrap_or_default();
        let notes = cap_notes(&raw);
        Ok(LlmProvider { notes, usage })
    }

    /// The keyless deterministic summarizer for `--mock` CI: one bullet per
    /// `(tool, code)` cluster, sorted, no subprocess and no cost. It proves
    /// the plumbing; it is never a summarization claim.
    pub fn build_mock(grains: Vec<ExperienceGrain>) -> LlmProvider {
        let mut clusters: BTreeMap<(&str, &str), usize> = BTreeMap::new();
        for g in grains.iter().filter(|g| g.is_error) {
            if let Some(code) = g.code.as_deref() {
                *clusters.entry((g.tool.as_str(), code)).or_insert(0) += 1;
            }
        }
        let notes = clusters
            .iter()
            .map(|((tool, code), n)| format!("- `{tool}` failed {n} times with `{code}`\n"))
            .collect::<String>();
        LlmProvider { notes, usage: Usage::default() }
    }

    /// What the summarizer cost. Zero for [`build_mock`](Self::build_mock).
    pub fn usage(&self) -> Usage {
        self.usage
    }
}

impl ContextProvider for LlmProvider {
    fn label(&self) -> &'static str {
        "m-llm"
    }

    fn task_start(&self, _task_prompt: &str) -> Result<String, String> {
        Ok(section(&self.notes))
    }

    fn on_tool_error(
        &self,
        _task_prompt: &str,
        _tool: &str,
        _code: &str,
        _body: &str,
    ) -> Result<String, String> {
        // Extraction-at-write-time: the notes are the whole product of this
        // arm, and they are already in the prompt.
        Ok(String::new())
    }
}

/// The summarizer's user content: per-tool call counts + every error line.
/// Capped, with the truncation logged to stderr — the cost of this arm is a
/// published number, so a silently shortened input would misprice it.
fn summarizer_input(grains: &[ExperienceGrain]) -> String {
    let mut lines: Vec<String> = vec!["Tool-call counts:".to_string()];
    for (tool, (calls, errors)) in tool_counts(grains) {
        lines.push(counts_line(tool, calls, errors));
    }
    lines.push(String::new());
    lines.push("Failures, oldest first:".to_string());
    lines.extend(
        grains
            .iter()
            .filter(|g| g.is_error)
            .map(|g| g.rendered.clone()),
    );
    let (out, dropped) = cap_lines(&lines, LLM_INPUT_CAP, &|n| {
        format!("… ({n} more history lines not shown — {LLM_INPUT_CAP}-char cap)\n")
    });
    if dropped > 0 {
        eprintln!(
            "m-llm: summarizer input hit the {LLM_INPUT_CAP}-char cap; \
             {dropped} history lines dropped"
        );
    }
    out
}

/// Notes cap. Slices on a char boundary and says so.
fn cap_notes(raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.chars().count() <= LLM_NOTES_CAP {
        return trimmed.to_string();
    }
    let note = format!("\n… (notes truncated at the {LLM_NOTES_CAP}-char cap)");
    let keep = LLM_NOTES_CAP - note.chars().count();
    let kept: String = trimmed.chars().take(keep).collect();
    format!("{kept}{note}")
}

// ---------------------------------------------------------------------------
// m-cmd — the external-provider seam
// ---------------------------------------------------------------------------

/// One persistent `sh -c` subprocess speaking the frozen newline-delimited
/// JSON protocol, so any vendor can plug their memory into this experiment.
///
/// The `Mutex` serializes calls into the single stdio pair; it does NOT make
/// the provider mutable — ingest already happened at spawn.
#[derive(Debug)]
pub struct CmdProvider {
    cmd: String,
    io: Mutex<ProviderIo>,
}

#[derive(Debug)]
struct ProviderIo {
    child: Child,
    stdin: ChildStdin,
    /// Lines from a reader thread. The sender drops when the child's stdout
    /// closes, so a dead provider surfaces as `Disconnected` at once rather
    /// than after the timeout.
    lines: mpsc::Receiver<String>,
}

impl CmdProvider {
    /// Spawn, ingest, and verify the ack. `Err` on anything the caller would
    /// otherwise mistake for "this arm had no memory to offer".
    pub fn spawn(cmd: &str, grains: &[ExperienceGrain]) -> Result<CmdProvider, String> {
        let mut child = Command::new("sh")
            .arg("-c")
            .arg(cmd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            // Inherited: provider diagnostics belong on the bench's stderr, and
            // a pipe nobody drains would deadlock a chatty provider.
            .stderr(Stdio::inherit())
            .spawn()
            .map_err(|e| format!("spawn `sh -c {cmd}`: {e}"))?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| "context provider stdin not captured".to_string())?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| "context provider stdout not captured".to_string())?;
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                match line {
                    Ok(l) => {
                        if tx.send(l).is_err() {
                            return;
                        }
                    }
                    Err(_) => return,
                }
            }
        });
        let provider = CmdProvider {
            cmd: cmd.to_string(),
            io: Mutex::new(ProviderIo { child, stdin, lines: rx }),
        };
        let ack = provider.call(json!({
            "selfimprove": 1,
            "op": "ingest",
            "grains": grains.iter().map(ExperienceGrain::to_json).collect::<Vec<Value>>(),
        }))?;
        if ack.get("ok").and_then(Value::as_bool) != Some(true) {
            return Err(format!(
                "context provider did not ack ingest with {{\"ok\":true}}: {ack}"
            ));
        }
        Ok(provider)
    }

    /// One request → one response. Blank lines are skipped (framing slack);
    /// anything else unparseable is a garbled provider, which is an error.
    fn call(&self, request: Value) -> Result<Value, String> {
        let mut io = self
            .io
            .lock()
            .map_err(|_| format!("context provider `{}` mutex poisoned", self.cmd))?;
        let mut line = request.to_string();
        line.push('\n');
        // A provider that has already exited fails HERE with EPIPE rather than
        // at the read below — which of the two notices first is a race with
        // the child's exit, so both must report provider death the same way or
        // the caller's error handling depends on scheduling.
        if let Err(e) = io
            .stdin
            .write_all(line.as_bytes())
            .and_then(|()| io.stdin.flush())
        {
            if e.kind() == std::io::ErrorKind::BrokenPipe {
                return Err(match io.child.try_wait() {
                    Ok(Some(st)) => format!("context provider `{}` exited with {st}", self.cmd),
                    _ => format!("context provider `{}` closed its stdin", self.cmd),
                });
            }
            return Err(format!("write to context provider `{}`: {e}", self.cmd));
        }
        loop {
            let got = match io.lines.recv_timeout(PROVIDER_TIMEOUT) {
                Ok(l) => l,
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    return Err(format!(
                        "context provider `{}` timed out after {}s",
                        self.cmd,
                        PROVIDER_TIMEOUT.as_secs()
                    ))
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    let status = io.child.wait().ok();
                    return Err(match status {
                        Some(st) => {
                            format!("context provider `{}` exited with {st}", self.cmd)
                        }
                        None => format!("context provider `{}` closed its stdout", self.cmd),
                    });
                }
            };
            if got.trim().is_empty() {
                continue;
            }
            return serde_json::from_str(&got)
                .map_err(|e| format!("context provider `{}` sent unparseable JSON ({e})", self.cmd));
        }
    }

    fn context(&self, request: Value) -> Result<String, String> {
        let v = self.call(request)?;
        Ok(v.get("context")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string())
    }
}

impl ContextProvider for CmdProvider {
    fn label(&self) -> &'static str {
        "m-cmd"
    }

    fn task_start(&self, task_prompt: &str) -> Result<String, String> {
        let body = self.context(json!({
            "op": "context",
            "stage": "task_start",
            "task_prompt": task_prompt,
        }))?;
        // The harness owns the framing for every arm — see the module docs.
        Ok(section(&body))
    }

    fn on_tool_error(
        &self,
        task_prompt: &str,
        tool: &str,
        code: &str,
        body: &str,
    ) -> Result<String, String> {
        let ctx = self.context(json!({
            "op": "context",
            "stage": "tool_error",
            "task_prompt": task_prompt,
            "tool": tool,
            "code": code,
            "body": body,
        }))?;
        Ok(injection(&ctx))
    }
}

impl Drop for CmdProvider {
    fn drop(&mut self) {
        // One process per eval pass: a provider that ignores EOF must not
        // outlive the run.
        if let Ok(mut io) = self.io.lock() {
            let _ = io.child.kill();
            let _ = io.child.wait();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn grain(tool: &str, code: Option<&str>, marker: &str) -> ExperienceGrain {
        let is_error = code.is_some();
        let body = match code {
            Some(c) => format!(r#"{{"error":{{"code":"{c}","message":"{marker}"}}}}"#),
            None => format!(r#"{{"ok":"{marker}"}}"#),
        };
        ExperienceGrain {
            task_id: format!("task-{marker}"),
            tool: tool.to_string(),
            input_json: "{}".to_string(),
            output_json: body.clone(),
            is_error,
            code: code.map(str::to_string),
            rendered: render_line(tool, is_error, code, &body),
        }
    }

    // --- rendering ---------------------------------------------------------

    #[test]
    fn rendered_lines_carry_the_code_and_stay_one_line() {
        let line = render_line(
            "refund",
            true,
            Some("rate_limited"),
            "{\n  \"error\": {\n    \"code\": \"rate_limited\"\n  }\n}",
        );
        assert_eq!(
            line,
            "- `refund` call failed with `rate_limited`: { \"error\": { \"code\": \"rate_limited\" } }"
        );
        assert!(!line.contains('\n'), "multi-line bodies must collapse: {line:?}");
        assert_eq!(render_line("refund", false, None, "{}"), "- `refund` call succeeded");
        assert_eq!(
            render_line("refund", true, None, "boom"),
            "- `refund` call failed: boom"
        );
    }

    #[test]
    fn a_long_body_is_excerpted_not_dropped() {
        let body = "x".repeat(500);
        let line = render_line("log_case", true, Some("invalid_timestamp"), &body);
        assert!(line.ends_with('…'), "excerpt must be marked: {line:?}");
        assert!(line.contains("invalid_timestamp"));
        assert_eq!(line.chars().filter(|c| *c == 'x').count(), BODY_EXCERPT_CHARS);
    }

    // --- cap_lines ---------------------------------------------------------

    #[test]
    fn cap_lines_drops_whole_lines_and_says_how_many() {
        let lines: Vec<String> = (0..10).map(|i| format!("line-{i}")).collect();
        let note = |n: usize| format!("[{n} dropped]\n");
        // Wide enough for everything: no note at all.
        let (full, dropped) = cap_lines(&lines, 10_000, &note);
        assert!(!full.contains("dropped"), "{full:?}");
        assert_eq!(full.lines().count(), 10);
        assert_eq!(dropped, 0);
        // Tight: the note is present, counts the remainder, and the whole
        // output still respects the cap.
        let (cut, dropped) = cap_lines(&lines, 40, &note);
        assert!(cut.chars().count() <= 40, "cap breached: {cut:?}");
        let kept = cut.lines().filter(|l| l.starts_with("line-")).count();
        assert!(kept > 0 && kept < 10, "expected a partial keep: {cut:?}");
        assert_eq!(dropped, 10 - kept, "reported count must match: {cut:?}");
        assert!(cut.contains(&format!("[{dropped} dropped]")), "{cut:?}");
    }

    /// The note closure sizes the reserve, so it runs once per candidate line.
    /// A caller that logged from inside it would spam one line per input line
    /// — the dropped count exists so truncation is logged exactly once.
    #[test]
    fn cap_lines_note_is_called_for_sizing_not_only_for_output() {
        use std::cell::Cell;
        let lines: Vec<String> = (0..6).map(|i| format!("line-{i}")).collect();
        let calls = Cell::new(0usize);
        let (_, dropped) = cap_lines(&lines, 10_000, &|n| {
            calls.set(calls.get() + 1);
            format!("[{n}]\n")
        });
        assert_eq!(dropped, 0, "nothing dropped at this cap");
        assert!(
            calls.get() > 1,
            "note is a sizing function, not an emit hook: {} call(s)",
            calls.get()
        );
    }

    #[test]
    fn the_summarizer_system_prompt_is_the_frozen_one() {
        assert_eq!(
            SUMMARIZER_SYSTEM,
            "You are building memory notes for a support-desk agent. From the tool-call \
             history below, write concise markdown bullet notes capturing recurring failure \
             patterns worth remembering for future tasks."
        );
    }

    // --- m-steel -----------------------------------------------------------

    #[test]
    fn steel_matches_on_tool_and_code_only() {
        let p = SteelProvider::build(vec![
            grain("refund", Some("rate_limited"), "a"),
            grain("refund", Some("approval_required"), "b"),
            grain("log_case", Some("rate_limited"), "c"),
            grain("refund", None, "d"),
        ]);
        assert_eq!(p.label(), "m-steel");
        assert_eq!(p.task_start("t").unwrap(), "", "steel injects at the hook, not at start");

        let hit = p.on_tool_error("t", "refund", "rate_limited", "body").unwrap();
        assert!(hit.starts_with(MEMORY_INJECTION_PREFIX), "{hit:?}");
        assert!(hit.contains("\"a\""), "the matching grain: {hit:?}");
        // Negative assertions: a filter that is ignored fails open.
        assert!(!hit.contains("\"b\""), "other code leaked: {hit:?}");
        assert!(!hit.contains("\"c\""), "other tool leaked: {hit:?}");
        assert!(!hit.contains("succeeded"), "successes are not indexed: {hit:?}");

        assert_eq!(p.on_tool_error("t", "refund", "invalid_id", "b").unwrap(), "");
        assert_eq!(p.on_tool_error("t", "wait", "rate_limited", "b").unwrap(), "");
    }

    #[test]
    fn steel_renders_most_recent_first() {
        let p = SteelProvider::build(vec![
            grain("refund", Some("rate_limited"), "oldest"),
            grain("refund", Some("rate_limited"), "middle"),
            grain("refund", Some("rate_limited"), "newest"),
        ]);
        let out = p.on_tool_error("t", "refund", "rate_limited", "b").unwrap();
        let order: Vec<&str> = ["newest", "middle", "oldest"]
            .into_iter()
            .filter(|m| out.contains(m))
            .collect();
        assert_eq!(order, vec!["newest", "middle", "oldest"], "{out:?}");
        let newest = out.find("newest").unwrap();
        let oldest = out.find("oldest").unwrap();
        assert!(newest < oldest, "recording order must be reversed: {out:?}");
    }

    #[test]
    fn steel_caps_and_reports_the_dropped_occurrences() {
        // Each rendered line is ~200 chars, so 100 of them blow the 4k cap.
        let big = "y".repeat(BODY_EXCERPT_CHARS);
        let grains: Vec<ExperienceGrain> = (0..100)
            .map(|i| {
                let mut g = grain("refund", Some("rate_limited"), &format!("g{i}"));
                g.rendered = render_line("refund", true, Some("rate_limited"), &format!("{i}-{big}"));
                g
            })
            .collect();
        let out = SteelProvider::build(grains)
            .on_tool_error("t", "refund", "rate_limited", "b")
            .unwrap();
        assert!(out.chars().count() <= STEEL_CAP + MEMORY_INJECTION_PREFIX.len() + 1);
        assert!(out.contains("earlier occurrences not shown"), "silent cap: {out:?}");
        // The kept ones are the most recent, and the count adds up.
        assert!(out.contains("99-"), "newest must survive the cap: {out:?}");
        let kept = out.lines().filter(|l| l.starts_with("- `refund`")).count();
        assert!(
            out.contains(&format!("({} earlier occurrences not shown)", 100 - kept)),
            "dropped count must match kept={kept}: {out:?}"
        );
    }

    #[test]
    fn steel_with_no_experience_is_silent() {
        let p = SteelProvider::build(vec![]);
        assert_eq!(p.on_tool_error("t", "refund", "rate_limited", "b").unwrap(), "");
    }

    // --- m-all -------------------------------------------------------------

    #[test]
    fn all_groups_by_tool_with_a_counts_line() {
        let mut grains = vec![
            grain("refund", Some("rate_limited"), "r1"),
            grain("refund", None, "r2"),
            grain("refund", Some("approval_required"), "r3"),
            grain("log_case", Some("invalid_timestamp"), "l1"),
        ];
        grains.push(grain("log_case", None, "l2"));
        let p = AllProvider::build(grains);
        assert_eq!(p.label(), "m-all");
        let out = p.task_start("t").unwrap();
        assert!(out.starts_with(MEMORY_SECTION_HEADING), "{out:?}");
        assert!(out.contains("`log_case`: 2 calls, 1 errors"), "{out:?}");
        assert!(out.contains("`refund`: 3 calls, 2 errors"), "{out:?}");
        // Failure history only: successes never render as bullets.
        assert!(!out.contains("succeeded"), "successes leaked: {out:?}");
        assert!(out.contains("\"r1\"") && out.contains("\"r3\"") && out.contains("\"l1\""));
        // Grouped: log_case sorts before refund, and its bullets stay with it.
        let log_at = out.find("`log_case`: 2").unwrap();
        let refund_at = out.find("`refund`: 3").unwrap();
        assert!(log_at < out.find("\"l1\"").unwrap());
        assert!(out.find("\"l1\"").unwrap() < refund_at);
        // The whole history is at task start, so nothing is injected again.
        assert_eq!(p.on_tool_error("t", "refund", "rate_limited", "b").unwrap(), "");
    }

    #[test]
    fn all_with_no_failures_emits_no_section() {
        let p = AllProvider::build(vec![grain("refund", None, "ok")]);
        assert_eq!(p.task_start("t").unwrap(), "");
    }

    #[test]
    fn all_caps_and_reports_the_dropped_lines() {
        let big = "z".repeat(BODY_EXCERPT_CHARS);
        let grains: Vec<ExperienceGrain> = (0..400)
            .map(|i| {
                let mut g = grain("refund", Some("rate_limited"), &format!("g{i}"));
                g.rendered = render_line("refund", true, Some("rate_limited"), &format!("{i}-{big}"));
                g
            })
            .collect();
        let out = AllProvider::build(grains).task_start("t").unwrap();
        assert!(out.chars().count() <= ALL_CAP + MEMORY_SECTION_HEADING.len() + 1);
        assert!(out.contains("more history lines not shown"), "silent cap: {out:?}");
        // The counts line survives: the agent still learns the scale it lost.
        assert!(out.contains("`refund`: 400 calls, 400 errors"), "{out:?}");
    }

    // --- m-llm -------------------------------------------------------------

    #[test]
    fn mock_llm_bullets_are_deterministic_sorted_and_free() {
        let grains = vec![
            grain("refund", Some("rate_limited"), "a"),
            grain("log_case", Some("invalid_timestamp"), "b"),
            grain("refund", Some("rate_limited"), "c"),
            grain("refund", Some("approval_required"), "d"),
            grain("refund", None, "e"),
        ];
        let p = LlmProvider::build_mock(grains.clone());
        assert_eq!(p.label(), "m-llm");
        let out = p.task_start("t").unwrap();
        assert_eq!(
            out,
            format!(
                "{MEMORY_SECTION_HEADING}\n\
                 - `log_case` failed 1 times with `invalid_timestamp`\n\
                 - `refund` failed 1 times with `approval_required`\n\
                 - `refund` failed 2 times with `rate_limited`\n"
            ),
            "one sorted bullet per (tool, code) cluster"
        );
        assert_eq!(p.usage().prompt_tokens, 0);
        assert_eq!(p.usage().completion_tokens, 0);
        // Same input ⇒ same bytes, and input order must not matter.
        let mut shuffled = grains;
        shuffled.reverse();
        assert_eq!(LlmProvider::build_mock(shuffled).task_start("t").unwrap(), out);
        assert_eq!(p.on_tool_error("t", "refund", "rate_limited", "b").unwrap(), "");
    }

    #[test]
    fn mock_llm_with_no_failures_emits_no_section() {
        assert_eq!(
            LlmProvider::build_mock(vec![grain("refund", None, "ok")])
                .task_start("t")
                .unwrap(),
            ""
        );
    }

    #[test]
    fn summarizer_input_carries_counts_and_every_error_line() {
        let input = summarizer_input(&[
            grain("refund", Some("rate_limited"), "a"),
            grain("refund", None, "b"),
            grain("log_case", Some("invalid_timestamp"), "c"),
        ]);
        assert!(input.contains("`refund`: 2 calls, 1 errors"), "{input:?}");
        assert!(input.contains("`log_case`: 1 calls, 1 errors"), "{input:?}");
        assert!(input.contains("\"a\"") && input.contains("\"c\""));
        assert!(!input.contains("succeeded"), "successes are not summarized: {input:?}");
    }

    #[test]
    fn notes_cap_is_announced_not_silent() {
        let short = cap_notes("- one\n- two");
        assert_eq!(short, "- one\n- two");
        let long = cap_notes(&"q".repeat(LLM_NOTES_CAP * 2));
        assert!(long.chars().count() <= LLM_NOTES_CAP, "{}", long.chars().count());
        assert!(long.contains("notes truncated"), "silent cap");
    }

    // Subprocess round-trips need a POSIX `sh` (the protocol's own contract);
    // every other arm above covers all platforms.
    #[cfg(unix)]
    mod subprocess {
        use super::*;

        /// Echo provider: acks ingest, then answers each context call with a
        /// line naming the stage and echoing the request back.
        const ECHO: &str = r#"python3 -c 'import sys,json
first = json.loads(sys.stdin.readline())
assert first["op"] == "ingest" and first["selfimprove"] == 1, first
n = len(first["grains"])
sys.stdout.write(json.dumps({"ok": True}) + "\n"); sys.stdout.flush()
for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    req = json.loads(line)
    if req["stage"] == "task_start":
        ctx = "- ingested %d grains for: %s" % (n, req["task_prompt"])
    else:
        ctx = "- saw %s/%s body=%s" % (req["tool"], req["code"], req["body"])
    sys.stdout.write(json.dumps({"context": ctx}) + "\n"); sys.stdout.flush()'"#;

        #[test]
        fn cmd_provider_round_trips_an_echo_provider() {
            let grains = vec![
                grain("refund", Some("rate_limited"), "a"),
                grain("refund", None, "b"),
            ];
            let p = CmdProvider::spawn(ECHO, &grains).expect("spawn + ingest ack");
            assert_eq!(p.label(), "m-cmd");

            let start = p.task_start("refund cus_1").unwrap();
            assert_eq!(
                start,
                format!("{MEMORY_SECTION_HEADING}\n- ingested 2 grains for: refund cus_1")
            );

            let hit = p.on_tool_error("refund cus_1", "refund", "rate_limited", "429").unwrap();
            assert_eq!(
                hit,
                format!("{MEMORY_INJECTION_PREFIX}\n- saw refund/rate_limited body=429")
            );

            // The process is persistent: a second call reuses it, and the
            // ingest count proves the state from spawn is still there.
            let again = p.task_start("second task").unwrap();
            assert!(again.contains("ingested 2 grains for: second task"), "{again:?}");
        }

        #[test]
        fn cmd_provider_returns_empty_when_the_provider_has_nothing() {
            let cmd = r#"python3 -c 'import sys,json
sys.stdin.readline()
sys.stdout.write(json.dumps({"ok": True}) + "\n"); sys.stdout.flush()
for line in sys.stdin:
    sys.stdout.write(json.dumps({"context": ""}) + "\n"); sys.stdout.flush()'"#;
            let p = CmdProvider::spawn(cmd, &[]).expect("spawn");
            assert_eq!(p.task_start("t").unwrap(), "", "empty ⇒ no section at all");
            assert_eq!(p.on_tool_error("t", "refund", "x", "b").unwrap(), "");
        }

        #[test]
        fn cmd_provider_refuses_a_missing_ingest_ack() {
            let cmd = r#"python3 -c 'import sys,json
sys.stdin.readline()
sys.stdout.write(json.dumps({"ok": False, "why": "nope"}) + "\n"); sys.stdout.flush()
for line in sys.stdin:
    pass'"#;
            let err = CmdProvider::spawn(cmd, &[]).unwrap_err();
            assert!(err.contains("did not ack ingest"), "{err}");
        }

        #[test]
        fn cmd_provider_errors_when_the_provider_dies() {
            // Acks, then exits: the next call must fail, not hang or return "".
            let cmd = r#"python3 -c 'import sys,json
sys.stdin.readline()
sys.stdout.write(json.dumps({"ok": True}) + "\n"); sys.stdout.flush()'"#;
            let p = CmdProvider::spawn(cmd, &[]).expect("ingest ack");
            let err = p.task_start("t").unwrap_err();
            // Whether the write (EPIPE) or the read (EOF) notices the death
            // first is a race with the child's exit — under load the write
            // wins, on an idle machine the read usually does. Both paths must
            // say "died", so this assertion is scheduling-independent.
            assert!(
                err.contains("exited") || err.contains("closed its std"),
                "a dead provider must report death: {err}"
            );
        }

        #[test]
        fn cmd_provider_reports_death_when_the_write_loses_the_race() {
            // The other death test races the child's exit; this one removes
            // the race by guaranteeing the process is gone before the first
            // context call, so the EPIPE-on-write path is always the one
            // exercised. Both paths must produce the same class of message.
            let cmd = r#"python3 -c 'import sys,json
sys.stdin.readline()
sys.stdout.write(json.dumps({"ok": True}) + "\n"); sys.stdout.flush()'"#;
            let p = CmdProvider::spawn(cmd, &[]).expect("ingest ack");
            // Wait for the child to actually exit before calling.
            for _ in 0..200 {
                if p.io.lock().unwrap().child.try_wait().ok().flatten().is_some() {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            let err = p.task_start("t").unwrap_err();
            assert!(
                err.contains("exited") || err.contains("closed its std"),
                "a provider dead before the call must report death: {err}"
            );
        }

        #[test]
        fn cmd_provider_errors_on_a_garbled_response() {
            let cmd = r#"python3 -c 'import sys
sys.stdin.readline()
sys.stdout.write("{\"ok\": true}\n"); sys.stdout.flush()
for line in sys.stdin:
    sys.stdout.write("not-json\n"); sys.stdout.flush()'"#;
            let p = CmdProvider::spawn(cmd, &[]).expect("ingest ack");
            let err = p.task_start("t").unwrap_err();
            assert!(err.contains("unparseable JSON"), "{err}");
        }

        #[test]
        fn cmd_provider_refuses_a_command_that_cannot_start() {
            let err = CmdProvider::spawn("exit 7", &[]).unwrap_err();
            assert!(err.contains("exited") || err.contains("closed"), "{err}");
        }

        /// The live `m-llm` path over the agent chat-adapter protocol: one
        /// call, notes cached, usage retained for the cost column.
        #[test]
        fn llm_provider_makes_one_summarizer_call_and_keeps_its_usage() {
            let cmd = r#"python3 -c 'import sys,json
req = json.loads(sys.stdin.readline())
assert req["op"] == "chat" and req["model"] == "" and req["temperature"] == 0, req
assert req["tools"] == [], req
sys.stdout.write(json.dumps({"message": {"role": "assistant", "content": "- refund rate limits: wait first\nSAW:" + str(len(req["messages"]))}, "usage": {"prompt_tokens": 900, "completion_tokens": 30}}))'"#;
            let p = LlmProvider::build(cmd, vec![grain("refund", Some("rate_limited"), "a")])
                .expect("summarizer");
            let out = p.task_start("t").unwrap();
            assert!(out.starts_with(MEMORY_SECTION_HEADING), "{out:?}");
            assert!(out.contains("- refund rate limits: wait first"), "{out:?}");
            assert!(out.contains("SAW:2"), "system + user message: {out:?}");
            assert_eq!(p.usage().prompt_tokens, 900);
            assert_eq!(p.usage().completion_tokens, 30);
        }

        #[test]
        fn llm_provider_reports_a_broken_summarizer() {
            let err = LlmProvider::build("echo not-json", vec![]).unwrap_err();
            assert!(err.starts_with("m-llm summarizer:"), "{err}");
        }
    }
}
