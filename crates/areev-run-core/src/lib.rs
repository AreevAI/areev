//! # areev-run-core — the pure scheduler of `areev run`
//!
//! Sans-IO by construction (governed-agents proposal §4): this crate cannot
//! observe the world. It exports [`step()`] — a pure function from
//! `(plan, env, state, events)` to `(commands, state)` — and the vocabulary
//! around it. The driver crate (`areev-run`) owns the store, the clock, the
//! executors, and the journal; it materializes journal lookups into
//! [`EventIn`]s and performs [`Command`]s. Replay IS this function: feed the
//! journaled events back in and assert the same commands come out.
//!
//! Purity is enforced mechanically, not aspirationally: the dependency tree
//! carries no clock/rand/net crate (CI checks `cargo tree`), and this
//! crate's `clippy.toml` disallows `SystemTime::now`/`Instant::now`.
//! Scheduler state uses `Vec`/`BTreeMap` exclusively — hash-iteration order
//! is the classic silent replay-divergence source and has no business here.

/// The scheduler's replay generation (#288).
///
/// Bumped EXACTLY when a change makes an existing journal replay differently
/// — the #251 class, where 1.8.3's dotted-tool fix meant a run recorded by
/// 1.8.2 no longer verifies and no fix was possible without keeping the
/// defect alive behind a per-run epoch. A verifier holding such a run needs
/// to be able to tell tampering from "written by an older scheduler", and
/// the version string alone cannot say that: most releases change nothing
/// here.
///
/// Every bump gets a CHANGELOG line. A run whose pinned epoch differs from
/// this one is refused on `resume` (RUN-E026) and labelled — not silently
/// accepted — on `verify`.
pub const SCHEDULER_EPOCH: u32 = 1;

pub mod cond;
pub mod error;
pub mod plan;
pub mod state;
pub mod step;
pub mod types;

pub use error::{BudgetAxis, Result, RunError};
pub use plan::{PlanEdge, PlanGraph};
pub use state::{
    DecideInFlight, EdgeRes, FoldInFlight, NodeState, PendingAsk, Phase, SchedulerState, Spent,
};
pub use step::{
    bound_tool_content, flow_key, step, DecideEnv, StepEnv, StepOutcome,
    DEFAULT_MAX_EFFECTS_PER_ATTEMPT,
};
pub use types::{
    Ask, Budgets, Command, DecisionRecord, EdgeOutcome, EffectKind, EffectOutcome, EventIn,
    FailCause, FoldRecord, JournalKey, NodeExecutor, OfferedTool, RunOutcome, DECIDE_TOOL,
    DECIDE_URI, INBOX, PARKED_ASKS,
};
