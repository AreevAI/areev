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
    /// RUN-E021 — the run lease was lost mid-flight: another driver took this
    /// run over while we were advancing it.
    ///
    /// Before this existed, two drivers advancing one run last-write-wins in
    /// the journal — silently. The doc comment on `Tainted` claimed forked tips
    /// were detected; they were not. Refusing here fails safe *by prevention*
    /// rather than by after-the-fact detection.
    ///
    /// Note the code: the design proposal called this RUN-E017, but that is
    /// `RetentionRefused` and codes are append-only.
    LeaseLost { run_id: String },
    /// RUN-E022 — a host command's outbound call was refused: a destination
    /// outside the run's allowlist, a method its grant does not permit, a
    /// credential it may not spend, a request header it may not set —
    /// undeclared, or one the broker owns (#105) — or a CAS blob read without
    /// the `blob` capability (#106). One code for "the broker said no",
    /// whichever of its doors was knocked on.
    ///
    /// The trigger evaluator reports the same condition as `TRG-E009`, the way
    /// a storage failure is `TRG-E010` there and `RUN-E020` here. One
    /// condition, one code per subsystem that reports it.
    EgressRefused { destination: String },
    /// RUN-E023 — an anonymization policy covers this run's namespace but
    /// cannot replay, so the model boundary would make `verify` diverge.
    ///
    /// Session scope numbers tokens by order of appearance, so the same input
    /// yields different placeholders on a replay. Only value-derived (HMAC)
    /// tokens are stable across handles and processes — that is D8 — and the
    /// key for them is HKDF-derived from the page key, which is why this also
    /// means an encrypted memory.
    AnonReplayUnsafe { ns: String, scope: String },
    /// RUN-E024 — an abstract node's transcript is over `llm_context_tokens`
    /// and there is nothing left to fold: the node's input and the kept tail
    /// alone exceed the ceiling, or a fold already ran and did not help.
    ///
    /// Deliberately NOT a retry and not a second fold. A fold that cannot find
    /// a foldable middle will never find one by trying again, and looping on it
    /// would spend the effect budget summarizing summaries. The honest answer
    /// is a failed node naming the ceiling it could not fit under — raise it,
    /// bound the tool results (`llm_tool_result_chars`), or split the node.
    /// `ceiling` is `None` when no ceiling was configured and the PROVIDER
    /// refused the transcript — the limit was learned from the rejection
    /// rather than predicted.
    ContextExceeded { node: String, tokens: u64, ceiling: Option<u64> },
    /// RUN-E025 — the model configuration this run STARTED under is not the
    /// one on offer now (#287).
    ///
    /// Raised before the lease is taken and before any grain is written: a
    /// resume that would have finished a parked run on a different model,
    /// provider or region simply does not begin. `areev run fork` is the
    /// sanctioned way through — a fork writes a new manifest carrying the new
    /// pin and records what it forked from, so the change is a recorded
    /// decision rather than an undocumented drift.
    ModelMismatch { pinned: String, offered: String },
    /// RUN-E026 — this run was written by a scheduler generation whose
    /// decisions differ from this build's (#288).
    ///
    /// A patch upgrade must not strand parked approval runs, so the VERSION
    /// string is not what is compared — only the scheduler epoch, which moves
    /// exactly when a change makes an existing journal replay differently.
    EngineMismatch { written_by: String, epoch: u32, this_epoch: u32 },
    /// RUN-E027 — starting this run would exceed a concurrency cap (#296).
    ///
    /// RETRYABLE by nature: the cap is a backstop beneath the host's own
    /// dispatcher, not a verdict on the run. Nothing is written under the run
    /// id, so the same id starts once a slot frees, and a trigger firing
    /// refused here leaves its item unconsumed (the #129 rule for RUN-E018).
    ConcurrencyLimit { scope: String, limit: u32 },
    /// RUN-E028 — a Tool declares a brokered-transfer ceiling
    /// (`runtime_limits.max_response_bytes` / `max_request_bytes`) that is
    /// malformed, zero, or above the 32 MiB hard maximum (#339).
    ///
    /// Refused at run start — before any upstream I/O — and never clamped: a
    /// tool that declared more than the host will carry would otherwise fail
    /// on the first document between the two sizes, with nothing pointing at
    /// its own declaration.
    TransferLimitInvalid { node: String, detail: String },
    /// RUN-E029 — a pause was asked of a run that cannot be paused (#344): it
    /// already finished (completed, failed, stalled, canceled, or out of
    /// budget), or a cancel is pending against it — cancel wins over pause.
    ///
    /// Pausing an already-paused (or already-pause-requested) run is NOT this
    /// error; it is idempotent and answers with the standing request.
    NotPausable { run_id: String, why: String },
    /// RUN-E030 — a plan binds a decision node (a Tool Definition whose
    /// `executor_uri` is `areev://decide`) and this host has no decision
    /// backend installed.
    ///
    /// Refused at run start (the V7 freeze) and on resume — before the lease
    /// and before any grain is written — naming the node, so a run never
    /// starts down a path whose branch point cannot be answered here. The
    /// scheduler's OPTIONAL asks (the decision-guided fold, the tool-offer
    /// narrowing) never raise this: without a backend they are not asked.
    NoDecider { node: String },
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
    /// Settled effects across the WHOLE run (#295). `--max-effects` bounds
    /// one node ATTEMPT; a run's total was bounded only by
    /// nodes × retries × cycles × fan-out × that number.
    Effects,
    /// Host tool calls across the whole run (#295) — plan-bound or
    /// model-issued. Client asks and memory reads do not count: an ask is a
    /// person, and a memory read is not an outbound call.
    ToolCalls,
}

impl BudgetAxis {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Supersteps => "supersteps",
            Self::Tokens => "tokens",
            Self::Usd => "usd",
            Self::WallMs => "wall_ms",
            Self::Storage => "storage",
            Self::Effects => "effects",
            Self::ToolCalls => "tool_calls",
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
            Self::LeaseLost { .. } => "RUN-E021",
            Self::EgressRefused { .. } => "RUN-E022",
            Self::AnonReplayUnsafe { .. } => "RUN-E023",
            Self::ContextExceeded { .. } => "RUN-E024",
            Self::ModelMismatch { .. } => "RUN-E025",
            Self::EngineMismatch { .. } => "RUN-E026",
            Self::ConcurrencyLimit { .. } => "RUN-E027",
            Self::TransferLimitInvalid { .. } => "RUN-E028",
            Self::NotPausable { .. } => "RUN-E029",
            Self::NoDecider { .. } => "RUN-E030",
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
            Self::EgressRefused { destination } => write!(
                f,
                "{code}: outbound call to '{destination}' was refused by this run's egress policy"
            ),
            Self::AnonReplayUnsafe { ns, scope } => write!(
                f,
                "{code}: namespace '{ns}' has an anonymization policy with scope \
                 '{scope}', which cannot be replayed — an abstract node would \
                 pseudonymize differently on every run and verify would diverge. \
                 Use scope \"memory\" (value-derived tokens, which needs an \
                 encrypted memory), or run this plan without abstract nodes"
            ),
            Self::LeaseLost { run_id } => write!(
                f,
                "{code}: lease on run '{run_id}' was lost — another driver took it over \
                 while this one was advancing it; this driver's writes are refused"
            ),
            Self::ContextExceeded { node, tokens, ceiling } => write!(
                f,
                "{code}: node '{node}' has a transcript the model reported at \
                 {tokens} prompt tokens against {}, and nothing is left to \
                 fold — the node's input and the kept tail alone exceed it. \
                 Raise --llm-context-tokens, bound oversized tool results \
                 (--llm-tool-result-chars), or split the node",
                match ceiling {
                    Some(c) => format!("a ceiling of {c}"),
                    None => "the provider's own limit, which it refused the \
                             request against"
                        .to_string(),
                }
            ),
            Self::ModelMismatch { pinned, offered } => write!(
                f,
                "{code}: this run started under {pinned} and is being resumed \
                 under {offered} — a parked run must finish on the \
                 configuration it started on. Resume under the pinned model, \
                 or `areev run fork` to continue under the new one (the fork \
                 records both)"
            ),
            Self::EngineMismatch { written_by, epoch, this_epoch } => write!(
                f,
                "{code}: this run was written by engine {written_by} at \
                 scheduler epoch {epoch}, and this build is at epoch \
                 {this_epoch} — the two schedulers do not make the same \
                 decisions, so resuming would produce a journal neither can \
                 verify whole. Run it on an engine at epoch {epoch}, or \
                 `areev run fork` to continue under this one"
            ),
            Self::ConcurrencyLimit { scope, limit } => write!(
                f,
                "{code}: {limit} concurrent runs already executing for \
                 {scope} — nothing was written, so this run id is still free. \
                 Retry once a slot frees, or raise the cap"
            ),
            Self::TransferLimitInvalid { node, detail } => write!(
                f,
                "{code}: node '{node}' declares an invalid transfer ceiling: \
                 {detail}. Declare 1..=33554432 bytes (32 MiB), or omit it for \
                 the 1048576-byte (1 MiB) default"
            ),
            Self::NotPausable { run_id, why } => write!(
                f,
                "{code}: run '{run_id}' cannot be paused: {why}. A pause only \
                 applies to a run that can still advance"
            ),
            Self::NoDecider { node } => write!(
                f,
                "{code}: node '{node}' is a decision node (executor_uri \
                 \"areev://decide\") and this host has no decision backend — \
                 configure one (`--decide <chain>` / `--decide-cmd`, \
                 $AREEV_DECIDE, or `Runner::with_decider`), or bind the node to \
                 an ordinary tool"
            ),
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
            RunError::LeaseLost { run_id: "r".into() },
            RunError::EgressRefused { destination: "https://x/".into() },
            RunError::AnonReplayUnsafe { ns: "n".into(), scope: "session".into() },
            RunError::ContextExceeded { node: "n".into(), tokens: 9, ceiling: Some(8) },
            RunError::TransferLimitInvalid { node: "n".into(), detail: "d".into() },
            RunError::NotPausable { run_id: "r".into(), why: "w".into() },
            RunError::NoDecider { node: "n".into() },
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
        assert_eq!(seen.len(), 27);
    }
}
