//! selfimprove — the self-improvement benches (see ../../SELFIMPROVE.md).
//!
//! Shared contract for the `selfimprove_*` bins. The submodules divide as:
//!   env    — the hidden-rules support-desk environment (tools, tasks, scoring)
//!   agent  — the agent loop: subprocess chat adapter + the deterministic mock
//!   memory — the Areev bridge: record → loop → review → apply/rollback → lessons
//!   report — report.json / report.md / transcripts.jsonl emit
//!
//! ## The fixed cross-module contract
//!
//! Tool surface (names, argument shapes, result shapes) — `env` implements,
//! `agent`'s mock and the live adapters call:
//!
//!   search_customers {query, page?}        → {customers:[{id,name,email}], has_more, next_page?}
//!   get_customer     {id}                  → {id,name,email,balance,subscription:{id,status,plan}}
//!   request_approval {reason}              → {approval_token}
//!   refund           {customer_id, amount, approval_token?} → {refund_id}
//!   cancel_subscription {customer_id}      → {status:"cancelled"}
//!   log_case         {customer_id, note, timestamp} → {case_id}
//!   wait             {seconds}             → {ok:true}
//!
//! A task ends when the assistant replies with content and no tool calls —
//! that content is the final answer.
//!
//! Tool errors are `{"error":{"code":C,"message":M[,"retry_after_s":N]}}`
//! with `is_error = true`. The `code` values are load-bearing: they are what
//! the loop's tool-failure clustering normalizes into lesson signatures, so
//! they are FROZEN per hidden rule:
//!
//!   R1 pagination not exhausted  → downstream "customer_not_found"
//!   R2 non-canonical id          → "invalid_id"
//!   R3 refund > $100 unapproved  → "approval_required"
//!   R4 cancel before refund      → "cancel_before_refund"
//!   R5 non-UTC timestamp         → "invalid_timestamp"
//!   R6 rate limit                → "rate_limited" (+ retry_after_s)
//!
//! Lessons enter the prompt as a system-prompt section assembled from LIVE
//! memory by `memory::lessons_markdown` — never from a harness flag:
//!
//!   ## LESSONS (from prior experience)
//!   - <rendered lesson>
//!
//! The mock agent keys its "learned" behaviors on the error codes appearing
//! anywhere in that section, which is exactly how a rendered tool-failure
//! lesson (or an LLM finding grounded in one) surfaces them.

pub mod agent;
pub mod env;
pub mod memory;
pub mod report;

use serde_json::{json, Value};

/// Stable id of a hidden rule ("R1".."R6" today; the list is data, not code).
pub type RuleId = &'static str;

/// One message in the chat protocol (OpenAI-compatible shape, hand-rolled —
/// the bench crate deliberately has no serde derive).
#[derive(Debug, Clone)]
pub struct ChatMessage {
    pub role: String,
    pub content: Option<String>,
    pub tool_calls: Vec<ToolCall>,
    /// Set on role == "tool" messages: the call this result answers.
    pub tool_call_id: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    /// JSON-encoded arguments object, exactly as the model produced it.
    pub arguments: String,
}

impl ChatMessage {
    pub fn system(text: &str) -> Self {
        Self { role: "system".into(), content: Some(text.into()), tool_calls: vec![], tool_call_id: None }
    }
    pub fn user(text: &str) -> Self {
        Self { role: "user".into(), content: Some(text.into()), tool_calls: vec![], tool_call_id: None }
    }
    pub fn tool_result(call_id: &str, body: &str) -> Self {
        Self { role: "tool".into(), content: Some(body.into()), tool_calls: vec![], tool_call_id: Some(call_id.into()) }
    }

    pub fn to_json(&self) -> Value {
        let mut m = json!({ "role": self.role });
        if let Some(c) = &self.content {
            m["content"] = json!(c);
        }
        if !self.tool_calls.is_empty() {
            m["tool_calls"] = Value::Array(
                self.tool_calls
                    .iter()
                    .map(|t| {
                        json!({
                            "id": t.id,
                            "type": "function",
                            "function": { "name": t.name, "arguments": t.arguments }
                        })
                    })
                    .collect(),
            );
        }
        if let Some(id) = &self.tool_call_id {
            m["tool_call_id"] = json!(id);
        }
        m
    }

    /// Parse an assistant message out of an adapter response (`message` value).
    pub fn from_json(v: &Value) -> Option<Self> {
        let role = v.get("role")?.as_str()?.to_string();
        let content = v.get("content").and_then(|c| c.as_str()).map(|s| s.to_string());
        let mut tool_calls = Vec::new();
        if let Some(arr) = v.get("tool_calls").and_then(|t| t.as_array()) {
            for tc in arr {
                let f = tc.get("function").unwrap_or(tc);
                tool_calls.push(ToolCall {
                    id: tc.get("id").and_then(|i| i.as_str()).unwrap_or("call_0").to_string(),
                    name: f.get("name")?.as_str()?.to_string(),
                    arguments: match f.get("arguments") {
                        Some(Value::String(s)) => s.clone(),
                        Some(other) => other.to_string(),
                        None => "{}".to_string(),
                    },
                });
            }
        }
        Some(Self { role, content, tool_calls, tool_call_id: None })
    }
}

/// Model-call usage, aggregated per task run.
#[derive(Debug, Clone, Copy, Default)]
pub struct Usage {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
}

/// What one tool execution produced (env → agent loop).
#[derive(Debug, Clone)]
pub struct ToolOutcome {
    /// JSON body handed back to the model as the tool result.
    pub body: String,
    pub is_error: bool,
    /// The hidden rule this error is attributable to, when it is one.
    pub rule: Option<RuleId>,
    /// The frozen error code (see module docs), when is_error.
    pub code: Option<String>,
}

/// One executed tool call, with everything the memory bridge needs to
/// record it (`record_tool_call`) and the report needs to transcribe it.
#[derive(Debug, Clone)]
pub struct RecordedCall {
    pub call_id: String,
    pub tool: String,
    pub input_json: String,
    pub output_json: String,
    pub is_error: bool,
    pub rule: Option<RuleId>,
}

/// The full result of running one task with one agent backend.
#[derive(Debug, Clone)]
pub struct TaskRunRecord {
    pub task_id: String,
    pub success: bool,
    pub steps: u32,
    pub tool_errors: u32,
    /// (rule, error-count) for every hidden rule this run tripped.
    pub rule_failures: Vec<(RuleId, u32)>,
    pub calls: Vec<RecordedCall>,
    pub final_answer: String,
    pub usage: Usage,
    /// Why scoring failed, for the transcript (empty on success).
    pub failure_reason: String,
}

/// Which hidden rules a finished task MISHANDLED — the recurrence metric.
///
/// A raw error count is the wrong metric: R6 fires once per armed task no
/// matter how good the agent is (it cannot know the limiter is armed before
/// the first 429), so "≥1 error" would read 100% forever. Mishandled means
/// the agent failed to adapt:
///   - the same rule errored ≥2 times in one task (no adaptation), or
///   - it errored once AND the task failed for a reason attributable to
///     that rule (the agent gave up on exactly that wall).
///
/// Silent failures (R1's wrong-customer side effects) reach this through
/// env-side attribution in `rule_failures` plus the failure_reason map.
/// Backend errors attribute to no rule — an infra flake is not mishandling.
pub fn mishandled_rules(rec: &TaskRunRecord) -> Vec<RuleId> {
    let attributed: &[RuleId] = if rec.success || rec.failure_reason.starts_with("backend_error") {
        &[]
    } else {
        match rec.failure_reason.as_str() {
            "refunded_wrong_customer" | "cancelled_wrong_customer" | "case_on_wrong_customer" => {
                &["R1"]
            }
            "wrong_timestamp" => &["R5"],
            "no_refund" => &["R3", "R4", "R6"],
            "not_cancelled" => &["R4", "R6"],
            "no_case_logged" => &["R5", "R6"],
            // Turn-cap and empty-answer exits: whatever it tripped, it never
            // got past.
            "no_final_answer" | "turn_limit" => &["R1", "R2", "R3", "R4", "R5", "R6"],
            _ => &[],
        }
    };
    rec.rule_failures
        .iter()
        .filter(|&&(rule, n)| n >= 2 || (n >= 1 && attributed.contains(&rule)))
        .map(|&(rule, _)| rule)
        .collect()
}

/// Per-rule recurrence figures for one eval state.
#[derive(Debug, Clone)]
pub struct RuleStat {
    pub rule: RuleId,
    /// Tasks in the set that exercise this rule at all.
    pub opportunities: u32,
    /// Tasks that tripped it at least once.
    pub failures: u32,
}

/// Aggregate of one eval pass (one A/B/A/B state over the held-out set).
#[derive(Debug, Clone)]
pub struct EvalSummary {
    /// "A0" | "B" | "A1" | "B2" (or an arm label for the curve bench).
    pub state: String,
    pub n: u32,
    pub successes: u32,
    pub tool_errors: u32,
    pub total_steps: u32,
    pub per_rule: Vec<RuleStat>,
    pub usage: Usage,
}

impl EvalSummary {
    pub fn success_rate(&self) -> f64 {
        if self.n == 0 { 0.0 } else { self.successes as f64 / self.n as f64 }
    }
}

#[cfg(test)]
mod mishandled_tests {
    use super::*;

    fn rec(success: bool, reason: &str, fails: &[(RuleId, u32)]) -> TaskRunRecord {
        TaskRunRecord {
            task_id: "t".into(),
            success,
            steps: 1,
            tool_errors: fails.iter().map(|f| f.1).sum(),
            rule_failures: fails.to_vec(),
            calls: vec![],
            final_answer: String::new(),
            usage: Usage::default(),
            failure_reason: reason.into(),
        }
    }

    #[test]
    fn one_handled_429_on_a_successful_task_is_not_mishandling() {
        assert!(mishandled_rules(&rec(true, "", &[("R6", 1)])).is_empty());
    }

    #[test]
    fn repeated_same_rule_errors_are_mishandling_even_on_success() {
        assert_eq!(mishandled_rules(&rec(true, "", &[("R6", 3)])), vec!["R6"]);
    }

    #[test]
    fn single_error_counts_only_when_the_failure_is_attributable() {
        // Gave up before any refund landed: both walls it touched are
        // attributed — counts alone can't tell a handled 429 from a
        // quit-on-429, so attribution stays conservative (against us).
        let r = rec(false, "no_refund", &[("R4", 1), ("R6", 1)]);
        assert_eq!(mishandled_rules(&r), vec!["R4", "R6"]);
        // Unrelated failure reason: a single handled error stays clean.
        let r = rec(false, "wrong_timestamp", &[("R6", 1), ("R5", 2)]);
        assert_eq!(mishandled_rules(&r), vec!["R5"]);
    }

    #[test]
    fn silent_wrong_customer_attributes_r1() {
        let r = rec(false, "refunded_wrong_customer", &[("R1", 1)]);
        assert_eq!(mishandled_rules(&r), vec!["R1"]);
    }

    #[test]
    fn backend_errors_attribute_to_no_rule() {
        let r = rec(false, "backend_error: boom", &[("R6", 1)]);
        assert!(mishandled_rules(&r).is_empty());
    }
}

/// One row of the governance ledger — every recommendation the LEARN phase
/// saw, including the ones that were rejected or not executable. The ledger
/// ships in the report verbatim: the failures are part of the evidence.
#[derive(Debug, Clone)]
pub struct LedgerEntry {
    pub hash: String,
    /// Analyzer id or "llm" origin marker.
    pub source: String,
    pub summary: String,
    /// "applied" | "rejected" | "advisory" | "apply_failed".
    pub disposition: String,
    pub because: String,
}

#[derive(Debug, Clone, Default)]
pub struct Ledger {
    pub entries: Vec<LedgerEntry>,
}

impl Ledger {
    pub fn count(&self, disposition: &str) -> usize {
        self.entries.iter().filter(|e| e.disposition == disposition).count()
    }
}
