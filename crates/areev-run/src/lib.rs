//! # areev-run — the `areev run` driver
//!
//! A host over Areev (governed-agents §4), peer of `areev-mcp`: executes
//! OMS Workflow grains through the pure scheduler (`areev-run-core`),
//! journaling every effect as content-addressed grains — intent before
//! dispatch, result as a supersession re-stating identity and links — with
//! checkpoints per superstep, resume from the file alone, HITL respond with
//! separation of duties, budgets, cancel, and journal-consistent `verify`.
//!
//! The replay contract (§5, stated honestly): the **journal** is
//! exactly-once — a result-journaled effect is never re-executed or
//! double-counted, across interrupts and across crashes. The **external
//! effect** is at-most-once per attempt in normal operation and
//! at-least-once across the dispatch-to-result crash window: a dangling
//! intent re-delivers under the same idempotency key and the redelivery is
//! journaled as an Observation — recorded, never silent.

pub mod broker;
pub mod clock;
pub mod egress;
pub mod executor;
pub mod journal;
pub mod lease;
pub mod manifest;
pub mod otel;
pub mod reducers;
pub mod runner;
pub mod stream;

pub use clock::{Clock, ScriptedClock, SystemClock};
pub use broker::{
    BlobRead, Broker, CallerGrant, CapabilityLimits, Credential, CredentialDenied,
    CredentialSource, EgressCall, EgressGrants, DEFAULT_CREDENTIAL_TTL,
};
// The URL-prefix grammar a `--allow-host` entry and a credential↔host pairing
// share, re-exported for the same reason the capability vocabulary is: a host
// building grants should not have to know which crate the parser lives in.
pub use areev_core::types::capability::AllowedHost;
// The capability vocabulary lives in areev-core, beside the grain field it
// validates, because `areev-cal`'s write path sits BELOW this crate and has to
// reach the same parser (#101). Re-exported so a host writing against the
// driver does not have to know that.
pub use areev_core::types::capability::{CapabilityDenied, Declaration};
pub use egress::{EgressDenied, EgressPolicy};
pub use executor::{
    is_sandbox_runtime, runtime_allows_capabilities, CodeExecutor, CommandExecutor, EgressHandle,
    ExecResult, ExecutorRegistry, HostToolExecutor, PreparedCode,
};
pub use manifest::{abstract_nodes, BudgetsSpec, ForkBase, PinnedTool, RunManifest};
pub use runner::{ns_in_scope, CrashPoint, OnDangling, RunOptions, Runner};
pub use otel::OtelObserver;
pub use stream::{RunEvent, RunObserver};
// Downstream hosts (MCP, bindings) speak the run vocabulary without a
// direct run-core dependency.
pub use areev_run_core::{FailCause, RunError as CoreRunError, RunOutcome as CoreRunOutcome};

use areev_core::error::AreevError;
use areev_run_core::{RunError, RunOutcome};

/// Map a store failure into the RUN domain (the underlying `DOMAIN-Ennn`
/// message survives in the detail).
pub fn err_run(e: AreevError) -> RunError {
    RunError::Storage { detail: e.to_string() }
}

/// What a drive returned to its caller.
#[derive(Debug, Clone)]
pub enum RunSession {
    Finished { outcome: RunOutcome, run_id: String },
    /// Parked on Client asks; the envelope is the §6.6 `requires_action`
    /// shape, addressed by `tool_call_id`.
    Parked { envelope: serde_json::Value, run_id: String },
}

/// One verify finding (§5.5 tiers, labeled output).
#[derive(Debug, Clone, serde::Serialize)]
pub struct VerifyStep {
    pub superstep: u64,
    pub verdict: String,
    pub ok: bool,
}

/// The verify report: journal-consistency per checkpoint, honest about what
/// was NOT verifiable (missing results, parks).
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct VerifyReport {
    pub verified: bool,
    pub steps: Vec<VerifyStep>,
}

/// One run's row in a shadow evaluation (§8 Wave 4): replayed entirely from
/// its journal — zero effect dispatches by construction (the shadow path
/// holds no executor, no pool, and no LLM; there is nothing to dispatch TO).
#[derive(Debug, Clone, serde::Serialize)]
pub struct ShadowRun {
    pub run_id: String,
    /// Journal-consistent under replay?
    pub consistent: bool,
    /// Effects answered FROM THE JOURNAL during the replay.
    pub effects_replayed: u64,
    /// Per-step verdicts (the verify report's rows).
    pub steps: Vec<VerifyStep>,
}

/// A shadow evaluation over N journaled runs — the §7.4 pre-apply check for
/// Tier-B code ("shadow replay of N recent journaled runs, zero effect
/// dispatches, before Apply").
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct ShadowReport {
    pub runs: Vec<ShadowRun>,
    /// Every run journal-consistent?
    pub all_consistent: bool,
    /// Always 0 — stated in the report so the gate's claim is explicit in
    /// the artifact, not implied by the code.
    pub effect_dispatches: u64,
}

/// One run's full picture: the frozen manifest, pinned resolutions, spend,
/// journal size, pending asks, fork lineage. What `areev run inspect`
/// prints (issue #34) — a tenant-deployed Python/Node service needs it
/// in-process, since installing the CLI purely to shell out for a
/// read-only report is a second artifact to ship and sign per deployment.
#[derive(Debug, Clone, serde::Serialize)]
pub struct InspectReport {
    pub run_id: String,
    pub plan_hash: String,
    pub principal: String,
    pub pinned: Vec<serde_json::Value>,
    pub budgets: serde_json::Value,
    pub fork_of: Option<serde_json::Value>,
    pub checkpoints: usize,
    pub journal_entries: usize,
    pub superstep: Option<serde_json::Value>,
    pub phase: Option<String>,
    pub spent: Option<serde_json::Value>,
    pub pending_asks: serde_json::Value,
}

/// One row of the run index: what `areev run list` and the console's Runs
/// tab show per run, with the terminal outcome and spend when the
/// run-outcome Observation has been recorded (`"open"` otherwise).
#[derive(Debug, Clone, serde::Serialize)]
pub struct RunRow {
    pub run_id: String,
    /// The run's session namespace, from its `run:<id> mg:harness` link
    /// Fact. `None` for a link written before the namespace was stamped on
    /// it — such a run appears only in an UNSCOPED listing, and a scoped one
    /// counts it in `RunListPage::unattributed` rather than guessing.
    pub ns: Option<String>,
    pub created_at: i64,
    pub outcome: String,
    pub spent_usd_micros: Option<u64>,
    pub spent_tokens: Option<u64>,
}

/// A page of the run index (#165): the rows asked for, the total that
/// matched, and whether the page cut the set short — so a surface can say
/// "showing 50 of 214" instead of implying 50 is all there is.
#[derive(Debug, Clone, serde::Serialize)]
pub struct RunListPage {
    pub runs: Vec<RunRow>,
    pub total: usize,
    pub truncated: bool,
    pub offset: usize,
    pub limit: usize,
    /// Runs a SCOPED listing excluded because their index entry predates the
    /// session-namespace stamp, so nothing is known about where they ran.
    /// Always 0 unscoped. Report it rather than dropping it silently: those
    /// runs are still in the unscoped listing.
    pub unattributed: usize,
}

/// The EU AI Act Article 14 answers for one run, measured from the journal
/// rather than asserted: where a human can intervene, who is authorized to,
/// what expires when, and how fast the kill switch actually drained. What
/// `areev run oversight-report` prints (issue #34) — a compliance artifact
/// customers ask for by name, wanted in-process (a governance console, a
/// quarterly pack) rather than reconstructed from CLI stdout.
#[derive(Debug, Clone, serde::Serialize)]
pub struct OversightReport {
    pub run_id: String,
    pub plan_hash: String,
    pub human_gates: serde_json::Value,
    pub authorized_responders: serde_json::Value,
    pub budgets: serde_json::Value,
    pub kill_switch: serde_json::Value,
}
pub use lease::{RunLease, DEFAULT_RUN_LEASE_MS};
