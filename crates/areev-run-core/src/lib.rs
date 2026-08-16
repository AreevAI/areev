//! # areev-run-core — the pure scheduler of `areev run`
//!
//! Sans-IO by construction (governed-agents proposal §4): this crate cannot
//! observe the world. It exports [`step`] — a pure function from
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

pub mod cond;
pub mod error;
pub mod plan;
pub mod state;
pub mod step;
pub mod types;

pub use error::{BudgetAxis, Result, RunError};
pub use plan::{PlanEdge, PlanGraph};
pub use state::{EdgeRes, NodeState, PendingAsk, Phase, SchedulerState, Spent};
pub use step::{step, StepEnv, StepOutcome};
pub use types::{
    Ask, Budgets, Command, DecisionRecord, EdgeOutcome, EffectKind, EffectOutcome, EventIn,
    FailCause, JournalKey, NodeExecutor, OfferedTool, RunOutcome,
};
