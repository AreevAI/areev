//! The `RUN` error domain (governed-agents §6.9). Codes are **append-only**
//! from day one, same rule as every other domain (`ERROR_CODES.md`): never
//! renumber, never reuse. The code is the leading token of `Display`, and
//! `code()` returns it — both pinned by tests below and by the workspace
//! uniqueness/format tests.

use std::fmt;

/// A runtime failure with a stable `RUN-Ennn` code.
#[derive(Debug, Clone, PartialEq)]
pub enum RunError {
    /// RUN-E001 — the run drained with no terminal node reaching success;
    /// names the decision node whose conditions all evaluated false.
    Stalled { node: String },
    /// RUN-E002 — an unbounded cycle: a strongly-connected component with no
    /// `max_cycles` edge.
    UnboundedCycle { nodes: Vec<String> },
    /// RUN-E003 — node(s) unreachable from the entry.
    Unreachable { nodes: Vec<String> },
    /// RUN-E004 — a binding or pinned hash does not resolve (or a journal
    /// ref is absent without an erasure receipt).
    UnresolvedRef { what: String },
    /// RUN-E005 — a condition failed validation at load.
    InvalidCondition { edge: String, why: String },
    /// RUN-E006 — an abstract node with no tool-calling LLM configured.
    NoToolLlm { node: String },
    /// RUN-E007 — a budget axis is exhausted.
    BudgetExhausted { axis: BudgetAxis },
    /// RUN-E008 — a dangling intent on resume with `on_dangling = fail`.
    DanglingIntent { key: String },
    /// RUN-E009 — replay divergence: a re-derived decision, counter, or
    /// checkpoint disagrees with the journal.
    ReplayDivergence { what: String },
    /// RUN-E010 — checkpoint does not match the run manifest.
    ManifestMismatch { why: String },
    /// RUN-E011 — a response names no pending Client ask.
    UnknownAsk { tool_call_id: String },
    /// RUN-E012 — a run operation the session's rights do not cover.
    Unauthorized { what: String },
    /// RUN-E013 — the run was canceled.
    Canceled { by: String, reason: String },
    /// RUN-E014 — a reducer violated its laws (debug sampling).
    ReducerLawViolation { key: String },
    /// RUN-E015 — a checkpoint exceeds the size cap after spill.
    CheckpointTooLarge { bytes: usize, cap: usize },
    /// RUN-E016 — the journal is tainted: forked supersession tips, or a
    /// resume of a copied file without `--fork` / the owner nonce.
    Tainted { why: String },
    /// RUN-E017 — a retention floor or legal hold refuses a destruction.
    RetentionRefused { why: String },
    /// RUN-E018 — code execution refused; names the failed gate condition.
    CodeExecRefused { condition: String },
    /// RUN-E019 — the workflow failed structural validation (V1/V2 shape
    /// errors that are not one of the specific codes above).
    InvalidPlan { why: String },
    /// RUN-E020 — a store operation failed under the runtime (wraps the
    /// underlying `DOMAIN-Ennn` message, which stays in the detail).
    Storage { detail: String },
}

/// The budget axes (§6.7). `Supersteps` is the global backstop too.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize,
)]
pub enum BudgetAxis {
    Supersteps,
    Tokens,
    Usd,
    WallMs,
    Storage,
}

impl BudgetAxis {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Supersteps => "supersteps",
            Self::Tokens => "tokens",
            Self::Usd => "usd",
            Self::WallMs => "wall_ms",
            Self::Storage => "storage",
        }
    }
}

impl RunError {
    /// The stable `RUN-Ennn` code — a permanent debugging handle.
    pub fn code(&self) -> &'static str {
        match self {
            Self::Stalled { .. } => "RUN-E001",
            Self::UnboundedCycle { .. } => "RUN-E002",
            Self::Unreachable { .. } => "RUN-E003",
            Self::UnresolvedRef { .. } => "RUN-E004",
            Self::InvalidCondition { .. } => "RUN-E005",
            Self::NoToolLlm { .. } => "RUN-E006",
            Self::BudgetExhausted { .. } => "RUN-E007",
            Self::DanglingIntent { .. } => "RUN-E008",
            Self::ReplayDivergence { .. } => "RUN-E009",
            Self::ManifestMismatch { .. } => "RUN-E010",
            Self::UnknownAsk { .. } => "RUN-E011",
            Self::Unauthorized { .. } => "RUN-E012",
            Self::Canceled { .. } => "RUN-E013",
            Self::ReducerLawViolation { .. } => "RUN-E014",
            Self::CheckpointTooLarge { .. } => "RUN-E015",
            Self::Tainted { .. } => "RUN-E016",
            Self::RetentionRefused { .. } => "RUN-E017",
            Self::CodeExecRefused { .. } => "RUN-E018",
            Self::InvalidPlan { .. } => "RUN-E019",
            Self::Storage { .. } => "RUN-E020",
        }
    }
}

impl fmt::Display for RunError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let code = self.code();
        match self {
            Self::Stalled { node } => write!(
                f,
                "{code}: run stalled — no terminal node succeeded; every \
                 condition on decision node '{node}' evaluated false (add an \
                 unconditional default edge, or check the state it tests)"
            ),
            Self::UnboundedCycle { nodes } => write!(
                f,
                "{code}: unbounded cycle through [{}] — every cycle must carry \
                 at least one edge with max_cycles",
                nodes.join(", ")
            ),
            Self::Unreachable { nodes } => write!(
                f,
                "{code}: node(s) unreachable from the entry: [{}]",
                nodes.join(", ")
            ),
            Self::UnresolvedRef { what } => {
                write!(f, "{code}: unresolved reference: {what}")
            }
            Self::InvalidCondition { edge, why } => {
                write!(f, "{code}: condition on edge {edge} is invalid: {why}")
            }
            Self::NoToolLlm { node } => write!(
                f,
                "{code}: node '{node}' is abstract (no binding, no matching \
                 tool definition) and no tool-calling LLM is configured"
            ),
            Self::BudgetExhausted { axis } => write!(
                f,
                "{code}: budget exhausted on axis '{}' — the run is \
                 checkpointed and resumable after the manifest budget is \
                 raised",
                axis.as_str()
            ),
            Self::DanglingIntent { key } => write!(
                f,
                "{code}: dangling intent {key} (crash between dispatch and \
                 result) and on_dangling = fail — re-dispatch requires an \
                 idempotency-honoring executor"
            ),
            Self::ReplayDivergence { what } => write!(
                f,
                "{code}: replay divergence — {what}. The scheduler, a \
                 condition evaluator, or a reducer is impure, or the journal \
                 was modified"
            ),
            Self::ManifestMismatch { why } => {
                write!(f, "{code}: checkpoint does not match the run manifest: {why}")
            }
            Self::UnknownAsk { tool_call_id } => write!(
                f,
                "{code}: no pending Client ask with tool_call_id \
                 '{tool_call_id}' (already settled, expired, or never asked)"
            ),
            Self::Unauthorized { what } => {
                write!(f, "{code}: not authorized: {what}")
            }
            Self::Canceled { by, reason } => {
                write!(f, "{code}: run canceled by {by}: {reason}")
            }
            Self::ReducerLawViolation { key } => write!(
                f,
                "{code}: reducer for key '{key}' violated its laws (must be \
                 pure and batching-invariant) — caught by debug sampling"
            ),
            Self::CheckpointTooLarge { bytes, cap } => write!(
                f,
                "{code}: checkpoint is {bytes} bytes after spill (cap {cap})"
            ),
            Self::Tainted { why } => write!(f, "{code}: journal tainted: {why}"),
            Self::RetentionRefused { why } => {
                write!(f, "{code}: destruction refused: {why}")
            }
            Self::CodeExecRefused { condition } => write!(
                f,
                "{code}: code execution refused — failed condition: {condition}"
            ),
            Self::InvalidPlan { why } => write!(f, "{code}: invalid workflow: {why}"),
            Self::Storage { detail } => write!(f, "{code}: store failure: {detail}"),
        }
    }
}

impl std::error::Error for RunError {}

pub type Result<T> = std::result::Result<T, RunError>;

#[cfg(test)]
mod tests {
    use super::*;

    fn representatives() -> Vec<RunError> {
        vec![
            RunError::Stalled { node: "n".into() },
            RunError::UnboundedCycle { nodes: vec!["a".into()] },
            RunError::Unreachable { nodes: vec!["b".into()] },
            RunError::UnresolvedRef { what: "x".into() },
            RunError::InvalidCondition { edge: "a->b".into(), why: "w".into() },
            RunError::NoToolLlm { node: "n".into() },
            RunError::BudgetExhausted { axis: BudgetAxis::Usd },
            RunError::DanglingIntent { key: "k".into() },
            RunError::ReplayDivergence { what: "w".into() },
            RunError::ManifestMismatch { why: "w".into() },
            RunError::UnknownAsk { tool_call_id: "t".into() },
            RunError::Unauthorized { what: "w".into() },
            RunError::Canceled { by: "u".into(), reason: "r".into() },
            RunError::ReducerLawViolation { key: "k".into() },
            RunError::CheckpointTooLarge { bytes: 2, cap: 1 },
            RunError::Tainted { why: "w".into() },
            RunError::RetentionRefused { why: "w".into() },
            RunError::CodeExecRefused { condition: "c".into() },
            RunError::InvalidPlan { why: "w".into() },
            RunError::Storage { detail: "d".into() },
        ]
    }

    /// The workspace rule: the code is the LEADING token of Display, unique
    /// per variant, format `RUN-Ennn`.
    #[test]
    fn codes_lead_display_and_are_unique() {
        let mut seen = std::collections::BTreeSet::new();
        for e in representatives() {
            let code = e.code();
            assert!(
                code.starts_with("RUN-E") && code.len() == 8,
                "bad code format: {code}"
            );
            assert!(
                e.to_string().starts_with(&format!("{code}: ")),
                "code must lead Display: {e}"
            );
            assert!(seen.insert(code), "duplicate code {code}");
        }
        assert_eq!(seen.len(), 20);
    }
}
