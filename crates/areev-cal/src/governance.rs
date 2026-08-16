//! The governance seam (CAL 1.3 §8.16) — how loop lifecycle statements
//! reach the Areev Loop engine without `areev-cal` depending on it.
//!
//! `areev-cal` sits below `areev-loop` in the workspace, so the executor
//! cannot call the engine directly. Instead the host attaches a
//! [`GovernanceHost`] to [`crate::CalExecutorConfig`]; `areev-loop-adapter`
//! provides the real implementation over the same facade the executor is
//! executing against (passed per call — never a second store handle, which
//! the single-writer registry would refuse). No host attached →
//! governance statements return `Unsupported`, the crate's convention for
//! reachable-but-unbacked surface.
//!
//! Identity never rides the statement: the host derives actor, scopes, and
//! observer from the facade's bound session, so a query cannot claim to be
//! someone.

use crate::facade::CalStoreFacade;
use areev_core::error::Result;

/// A review decision carried by `APPROVE`/`REJECT`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReviewDecision {
    Approve,
    Reject,
}

/// The read side of the loop (`DESCRIBE LOOP|ANALYZERS|OUTCOMES|POLICY`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoopInfo {
    /// Health: last run, queue depth, approval rate.
    Loop,
    /// Registered analyzers and their effective config.
    Analyzers,
    /// The Verify gate's measured outcomes.
    Outcomes,
    /// The effective host policy (read-only — policy WRITES never enter
    /// CAL: the policy is what gates CAL, and CAL editing it would be
    /// self-licensing).
    Policy,
}

/// Options for `RUN LOOP` — the analysis trigger. Deliberately free of
/// credentials and model names: LLM backends are host configuration.
#[derive(Debug, Clone, Default)]
pub struct RunLoopOptions {
    /// `RUN LOOP FULL` — re-analyze the whole memory (a reflect sweep).
    pub full_sweep: bool,
    /// `WITH min_new(N)`.
    pub min_new: Option<u64>,
    /// `WITH if_stale("6h")`, parsed to milliseconds.
    pub if_stale_ms: Option<i64>,
}

/// What a host must provide for governance statements to execute. Every
/// method receives the facade the executor is running against — the host
/// wraps it in its own substrate view and derives the session identity
/// from it.
pub trait GovernanceHost: Send + Sync {
    /// `RUN LOOP` — run the analysis pass. Returns the run result as JSON.
    fn run_loop(
        &self,
        store: &dyn CalStoreFacade,
        opts: &RunLoopOptions,
    ) -> Result<serde_json::Value>;

    /// `APPROVE`/`REJECT <hash> BECAUSE "…"`.
    fn review(
        &self,
        store: &dyn CalStoreFacade,
        rec_hash: &str,
        decision: ReviewDecision,
        because: &str,
    ) -> Result<()>;

    /// `APPLY <hash> BECAUSE "…"`. Returns whether the apply is
    /// rollbackable.
    ///
    /// `destructive_cap` is the session's `allow_destructive_ops` — the
    /// process-wide restrictive cap (`--no-destructive-ops`) that sits over
    /// every grant. An apply can execute a recommendation's `FORGET`, so the
    /// cap has to reach it: without this the cap gates `FORGET` typed into
    /// CAL but not the same `FORGET` routed through a recommendation.
    fn apply(
        &self,
        store: &dyn CalStoreFacade,
        rec_hash: &str,
        because: &str,
        destructive_cap: bool,
    ) -> Result<bool>;

    /// `ROLLBACK <hash> BECAUSE "…"`.
    fn rollback(&self, store: &dyn CalStoreFacade, rec_hash: &str, because: &str) -> Result<()>;

    /// The `DESCRIBE` reads. Returns the info as JSON.
    fn describe(&self, store: &dyn CalStoreFacade, what: LoopInfo) -> Result<serde_json::Value>;
}
