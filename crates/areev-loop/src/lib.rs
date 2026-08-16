//! # Areev Loop
//!
//! The governed self-improvement engine for AI-agent memory. Areev Loop turns an
//! agent's own history into **recommendations** — evidence-cited, reviewable,
//! undoable, measured — and governs every change through four gates (propose,
//! review, apply, verify). The deterministic core produces useful
//! recommendations with **zero model calls** by computing over declared grain
//! semantics, never raw prose.
//!
//! This crate is a **standalone engine over an [`OmsSubstrate`]** (CAL text +
//! grains) with zero Areev dependencies (serde only). Areev is the first
//! substrate; [`ReferenceSubstrate`] lets tests run with no store at all.
//!
//! ```
//! use areev_loop::{Engine, ReferenceSubstrate, RunOptions};
//!
//! let mut store = ReferenceSubstrate::new();
//! let engine = Engine::with_builtins();
//! let result = engine.run(&mut store, &RunOptions::default(), 1_000).unwrap();
//! assert!(result.ran());
//! ```
//!
//! See the design proposal (`docs/loop-proposal.md`) for the full model.

pub mod analyzer;
pub mod analyzers;
pub mod cal;
pub mod config;
pub mod engine;
pub mod error;
pub mod external;
pub mod llm;
pub mod manifest;
pub mod model;
pub mod policy;
pub mod recommendation;
pub mod reference;
pub mod substrate;

#[cfg(test)]
mod integration;
#[cfg(test)]
mod testkit;

pub use analyzer::{builtin_analyzers, AnalyzeCtx, Analyzer, OutcomeInput};
pub use engine::{
    Decision, Engine, Health, LlmMetrics, RunOptions, RunOutcome, RunResult, Scope, ScopeSet,
    SkipReason, LOOP_NS,
};
pub use config::{AnalyzerConfig, AnalyzerConfigUpdate, AnalyzerSetting};
pub use error::{Error, Result};
pub use external::CommandAnalyzer;
pub use llm::{CommandLlm, LlmBackend};
pub use manifest::{
    analyzer_family, AnalyzerManifest, AutoApplyClass, CadenceClass, Capability, ParamSpec, Params,
    TargetClass, Tier, TrustClass,
};
pub use model::{ActionKind, GrainRecord, Origin, Severity, TargetRef};
pub use policy::{AutoApplyGrant, Policy, TelemetryMode};
pub use recommendation::{
    dedup_key, validate_code_rules, AuditRecord, GatingEvidence, MetricSnapshot, ObserverType,
    OutcomeResult, Proposal, RecDraft, RecStatus, Recommendation, Summary,
};
pub use reference::ReferenceSubstrate;
pub use substrate::{
    BudgetUsage, Capabilities, GrainAccess, GrainSpec, HeadGroup, OmsSubstrate, QueryUsage,
    ReadOpts, SubstrateRead, TelemetryView,
};
