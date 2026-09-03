//! Host policy — the optional `loop-policy.json` (proposal §6.2). It is the
//! **only** place auto-apply is granted, and it is host config (per-process,
//! never persisted in a memory file). All fields default-closed; the whole
//! struct rejects unknown keys, so a policy that tries to register an
//! executable (`--analyzer-cmd`) or touch a trust-floor field fails to load —
//! a stolen or committed policy file must be inert.
//!
//! Precedence (enforced by the engine): engine ceilings > host CLI flags >
//! this policy file > memory-file config. "The file selects and restricts;
//! only the host grants."

use crate::error::{Error, Result};
use crate::model::Severity;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Telemetry sidecar mode (host-only).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TelemetryMode {
    Off,
    #[default]
    Aggregate,
    Full,
}

/// What DISCOVER optimizes for (`docs/loop-reflection.md` §5.1). Host config
/// like everything else here: it changes the scoring rule the proposer is
/// given, never the gates — every draft still has to survive GROUND, VERIFY,
/// the confidence floor and a human review with a BECAUSE.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiscoverObjective {
    /// The review-queue objective: "nothing to report" is a zero-penalty
    /// answer and a wrong finding costs twice a right one. Right for a queue
    /// a person triages — it keeps the queue clean at the price of drafts
    /// the model was not sure enough about.
    #[default]
    ReviewQueue,
    /// The learner objective: the agent has to improve from THIS pass, so
    /// abstaining in the face of a recurring failure, repeated rejections or
    /// a person's instruction is penalized like a wrong lesson. Measured
    /// need: under the review-queue rule a cheap model authored a lesson on
    /// fewer than half of its passes over evidence that plainly held one.
    Learner,
}

/// One auto-apply grant: an analyzer family may auto-apply to these target
/// classes up to (and including) `max_severity`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AutoApplyGrant {
    /// Analyzer family (e.g. `loop.duplicate_sweep`) or full id; matched by
    /// family so a version bump keeps the grant.
    pub analyzer: String,
    /// Eligible target classes: `memory` and/or `query` only (prompt/host are
    /// never auto-appliable and are rejected at eval time regardless).
    pub targets: Vec<String>,
    /// Highest severity this grant covers.
    pub max_severity: Severity,
}

/// The parsed host policy. Everything default-closed.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Policy {
    /// Master opt-in (same posture as `allow_destructive_ops`: default off).
    /// Auto-apply never fires unless this is true AND a grant matches.
    #[serde(default)]
    pub auto_apply_enabled: bool,
    /// Auto-apply grants (default: none).
    #[serde(default)]
    pub auto_apply: Vec<AutoApplyGrant>,
    /// Analyzer families the host disables entirely.
    #[serde(default)]
    pub deny: Vec<String>,
    /// Per-analyzer severity floors (family → floor); combined with the
    /// file's floors by taking the stricter of the two.
    #[serde(default)]
    pub severity_floors: BTreeMap<String, Severity>,
    #[serde(default)]
    pub telemetry: TelemetryMode,
    /// The DISCOVER scoring rule (default: the review-queue objective).
    #[serde(default)]
    pub discover_objective: DiscoverObjective,
}

impl Policy {
    /// Parse a policy JSON string. Unknown keys are rejected (fail-closed).
    pub fn from_json(s: &str) -> Result<Self> {
        serde_json::from_str(s).map_err(|e| Error::InvalidProposal(format!("policy: {e}")))
    }

    /// Is this analyzer family denied by the host?
    pub fn denies(&self, family: &str) -> bool {
        self.deny.iter().any(|d| crate::manifest::analyzer_family(d) == family)
    }

    /// The host severity floor for a family, if any.
    pub fn severity_floor(&self, family: &str) -> Option<Severity> {
        self.severity_floors
            .iter()
            .find(|(k, _)| crate::manifest::analyzer_family(k) == family)
            .map(|(_, v)| *v)
    }

    /// Does a grant permit auto-applying this family to `target_class` at
    /// `severity`? Only the `memory` class is ever eligible.
    ///
    /// `query` was eligible until definition rewrites became executable
    /// (issue #28). A grain edit changes one remembered value; a saved-query
    /// or template rewrite changes what EVERY future context contains — the
    /// blast radius is every turn from now on, not one fact. So a definition
    /// rewrite always requires a human APPROVE + APPLY with `BECAUSE`, and
    /// the class is excluded here by name, exactly as `code`/`evalset` are.
    pub fn grants_auto_apply(&self, family: &str, target_class: &str, severity: Severity) -> bool {
        if !self.auto_apply_enabled || target_class != "memory" {
            return false;
        }
        self.auto_apply.iter().any(|g| {
            crate::manifest::analyzer_family(&g.analyzer) == family
                && g.targets.iter().any(|t| t == target_class)
                && severity <= g.max_severity
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// §7.4's stated invariant, pinned: code and evalset targets are
    /// excluded from auto-apply BY NAME — even a policy that explicitly
    /// names those classes in a grant is inert, because
    /// `grants_auto_apply` hard-codes memory|query.
    #[test]
    fn code_targets_never_auto_apply_even_when_granted() {
        let p = Policy::from_json(
            r#"{"auto_apply_enabled": true,
                "auto_apply": [{"analyzer": "loop.codegen", "targets": ["code", "evalset", "memory"], "max_severity": "high"}]}"#,
        )
        .unwrap();
        assert!(!p.grants_auto_apply("loop.codegen", "code", Severity::Info));
        assert!(!p.grants_auto_apply("loop.codegen", "evalset", Severity::Info));
        assert!(
            p.grants_auto_apply("loop.codegen", "memory", Severity::Low),
            "the same grant's memory leg still works — the exclusion is by class"
        );
    }

    #[test]
    fn default_policy_grants_nothing() {
        let p = Policy::default();
        assert!(!p.grants_auto_apply("loop.duplicate_sweep", "memory", Severity::Info));
        assert!(!p.denies("loop.staleness"));
        assert_eq!(p.telemetry, TelemetryMode::Aggregate);
    }

    #[test]
    fn parses_and_grants() {
        let p = Policy::from_json(
            r#"{"auto_apply_enabled": true,
                "auto_apply": [{"analyzer": "loop.duplicate_sweep", "targets": ["memory"], "max_severity": "low"}],
                "deny": ["loop.staleness"],
                "severity_floors": {"loop.contradiction_sweep": "high"}}"#,
        )
        .unwrap();
        assert!(p.grants_auto_apply("loop.duplicate_sweep", "memory", Severity::Low));
        assert!(!p.grants_auto_apply("loop.duplicate_sweep", "memory", Severity::High), "above max_severity");
        assert!(!p.grants_auto_apply("loop.duplicate_sweep", "query", Severity::Low), "query not granted");
        assert!(p.denies("loop.staleness"));
        assert_eq!(p.severity_floor("loop.contradiction_sweep"), Some(Severity::High));
    }

    #[test]
    fn prompt_and_host_targets_never_granted() {
        let p = Policy::from_json(
            r#"{"auto_apply_enabled": true,
                "auto_apply": [{"analyzer": "x", "targets": ["prompt", "host"], "max_severity": "high"}]}"#,
        )
        .unwrap();
        assert!(!p.grants_auto_apply("x", "prompt", Severity::Info));
        assert!(!p.grants_auto_apply("x", "host", Severity::Info));
    }

    #[test]
    fn discover_objective_defaults_to_the_review_queue_rule() {
        assert_eq!(Policy::default().discover_objective, DiscoverObjective::ReviewQueue);
        let p = Policy::from_json(r#"{"discover_objective": "learner"}"#).unwrap();
        assert_eq!(p.discover_objective, DiscoverObjective::Learner);
        assert!(
            Policy::from_json(r#"{"discover_objective": "eager"}"#).is_err(),
            "an unknown objective must not load as the default"
        );
    }

    #[test]
    fn unknown_keys_rejected() {
        // A trust-floor field or an executable registration must not load.
        assert!(Policy::from_json(r#"{"analyzer_cmd": "evil"}"#).is_err());
        assert!(Policy::from_json(r#"{"auto_apply_free_text": true}"#).is_err());
    }
}
