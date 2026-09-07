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
use crate::recommendation::Checkpoint;
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

/// How an Observation is attributed in the evidence bundle handed to the LLM
/// (`docs/loop.md`). `Named` renders `<observer> (a person) said of
/// <subject>: <text>`; `Anonymous` renders the bare text, which is what the
/// engine did before 2026-09-04.
///
/// It is host policy for two independent reasons. An operator may not want
/// observer identities rendered into a model prompt at all — an observer id
/// can be a person's name or account — and that is a privacy decision only
/// the host can make. And it is the one variable in the receipts ablation
/// (`crates/areev-bench/RECEIPTS.md`), where naming the speaker is what
/// stopped one model reading a correction as a request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceAttribution {
    /// Name the observer on an Observation that records one.
    #[default]
    Named,
    /// Render the bare text, attributing nothing.
    Anonymous,
}

/// The evalset every LLM-authored, applicable proposal is measured against
/// after apply (`docs/loop.md`, "Evalset-backed outcomes"). An authored
/// lesson carries no built-in recurrence metric — nothing errors when a
/// lesson is merely useless — so without this the Verify gate has nothing
/// to re-measure for exactly the proposals a human was least able to judge.
/// The host names the evalset and the field; the engine takes the baseline
/// from the newest run journaled BEFORE the proposal and reads the current
/// value from runs journaled AFTER the apply. No baseline run → no metric,
/// never a fabricated one.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OutcomeEvalset {
    /// The evalset hash (the subject is `evalset:<hash>` in `agent:harness`).
    pub hash: String,
    /// The summary field to read: `passed`, `failed`, `total`, `error_rate`,
    /// or any numeric field the host's harness writes into the summary.
    pub field: String,
    /// Which direction is an improvement — `passed` and an accuracy are
    /// higher-is-better, `failed` and `error_rate` are not. Stated by the
    /// host because getting it wrong would revert an improvement.
    pub higher_is_better: bool,
    /// Checkpoints after apply, in ms. Default 1d / 7d / 30d. The older
    /// spelling; `checkpoints` wins when both are given.
    #[serde(default = "default_horizons")]
    pub horizons_ms: Vec<i64>,
    /// The schedule in the deployment's own unit — `{"after_ms": n}`,
    /// `{"after_runs": n}` or `{"after_grains": n}` (a bare integer is ms).
    /// A benchmark or CI harness wants `[{"after_runs": 1}]`: measure at the
    /// next graded run after the apply, however soon that is. Empty (the
    /// default) means `horizons_ms`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub checkpoints: Vec<Checkpoint>,
}

fn default_horizons() -> Vec<i64> {
    vec![86_400_000, 7 * 86_400_000, 30 * 86_400_000]
}

impl OutcomeEvalset {
    /// The effective schedule: `checkpoints` when set, else `horizons_ms` as
    /// time checkpoints.
    pub fn schedule(&self) -> Vec<Checkpoint> {
        let mut h: Vec<Checkpoint> = if self.checkpoints.is_empty() {
            self.horizons_ms.iter().map(|ms| Checkpoint::AfterMs(*ms)).collect()
        } else {
            self.checkpoints.clone()
        };
        h.sort_unstable();
        h.dedup();
        h
    }
}

/// When a loop pass is due — the loop's cadence, as host policy.
///
/// The engine has no clock and no scheduler of its own (ARCHITECTURE.md:
/// cadence is data, evaluation is a command); a host calls `run` and the
/// engine decides whether there is anything to do. Until now that decision
/// was only expressible as per-call flags (`--min-new`, `--if-stale`), so
/// every surface that can trigger a run — CLI, MCP, the console — had to be
/// told separately, and none of them could count the units a chat deployment
/// actually thinks in. This block is the same gate as the flags, set once in
/// the policy file, with two more units.
///
/// Each field is a threshold; the pass is due when **any** set one is met
/// (whichever comes first). Nothing set — the default — means a pass is due
/// whenever it is called, which is what every deployment had before. Explicit
/// per-call flags override the block (host CLI flags > policy file), and a
/// full sweep (`areev loop reflect`) is a command, not a tick: it always runs.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Cadence {
    /// Due when this long has passed since the last run (or it never ran).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub every_ms: Option<i64>,
    /// Due when this many grains of any kind landed since the last run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub every_grains: Option<u64>,
    /// Due when this many Event grains — turns, in a chat deployment — landed
    /// since the last run. Hermes's post-turn review fires every ten.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub every_events: Option<u64>,
    /// Due when Events from this many distinct sessions landed since the last
    /// run: "reflect once per conversation" is `1`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub every_sessions: Option<u64>,
}

impl Cadence {
    /// Whether any threshold is configured at all.
    pub fn is_set(&self) -> bool {
        self.every_ms.is_some()
            || self.every_grains.is_some()
            || self.every_events.is_some()
            || self.every_sessions.is_some()
    }
}

/// Whether, and how, DISCOVER may author a **Skill** — a reusable procedure
/// with an applicability condition and ordered steps, derived from a
/// trajectory that succeeded.
///
/// This exists because of a measured gap. On PAST-Bench the agent performed
/// the procedure correctly in the learn episode on every seed and then, asked
/// at session end whether there was anything to save, answered "nothing to
/// save" — so the store was empty at evaluation and the memory scored below
/// having none (`crates/areev-bench/PERSIST.md`). Every Skill in the memory
/// depended on the model volunteering one mid-task. Hermes does not depend on
/// that: a separate review pass writes its skills. This is Areev's equivalent,
/// and it runs through the same gates as every other draft — GROUND, VERIFY,
/// the confidence floor, a review with a BECAUSE — and is never auto-applied.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SkillAuthoring {
    /// Offer the `skill` proposal kind to the proposer at all (default: yes,
    /// under LLM enrichment; no LLM, no skills).
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Fewer ordered steps than this is a lesson, not a procedure (default 2).
    #[serde(default = "default_min_steps")]
    pub min_steps: u32,
}

fn default_true() -> bool {
    true
}
fn default_min_steps() -> u32 {
    2
}
fn default_min_evidence() -> u32 {
    1
}

impl Default for SkillAuthoring {
    fn default() -> Self {
        SkillAuthoring { enabled: true, min_steps: 2 }
    }
}

/// Whether, and how, DISCOVER may author a **plan** — a Workflow grain: named
/// steps, edges with conditions in the runtime's frozen grammar, validated
/// before a reviewer sees it — beside the Skill that carries the prose.
///
/// A skill is what a model reads; a plan is what the runtime can check and
/// run. PAST-Bench's own labels call every procedural family "ordered steps,
/// tools, conditions… a patched v2 supersedes v1" — which is a Workflow, and
/// its patch is the `plan_revision` this engine already has. Storing the
/// procedure as a plan buys structural validation (unique, reachable nodes;
/// conditions that parse; bounded cycles) at author time, and puts the
/// procedure where `areev run`, the run journal and `run_outcome` can reach
/// it. Governed like every draft; never auto-applied.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlanAuthoring {
    /// Offer the `plan` proposal kind (default: yes, under LLM enrichment).
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Fewer steps than this is a lesson, not a procedure (default 2).
    #[serde(default = "default_min_steps")]
    pub min_nodes: u32,
}

impl Default for PlanAuthoring {
    fn default() -> Self {
        PlanAuthoring { enabled: true, min_nodes: 2 }
    }
}

/// The parsed host policy. Everything default-closed — the two fields whose
/// closed state is not the zero value (`skills`, `min_evidence`) say so in
/// their own `Default`.
#[derive(Debug, Clone, Serialize, Deserialize)]
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
    /// Measure every applicable LLM-authored proposal against this evalset
    /// after apply (default: none — authored lessons carry no metric).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome_evalset: Option<OutcomeEvalset>,
    /// Whether an Observation names its observer in the evidence bundle
    /// (default: named).
    #[serde(default)]
    pub evidence_attribution: EvidenceAttribution,
    /// When a pass is due (default: whenever it is called).
    #[serde(default, skip_serializing_if = "is_default_cadence")]
    pub cadence: Cadence,
    /// Skill authoring by the LLM proposer (default: on, two steps minimum).
    #[serde(default)]
    pub skills: SkillAuthoring,
    /// The fewest distinct evidence grains an LLM draft must cite to be
    /// offered as a change rather than an advisory finding (default 1 — a
    /// single instance may become a rule). An independent audit of 88 governed
    /// decisions found that 15 of 28 approvals had generalised one instance
    /// into standing policy; `2` is the setting that audit argues for. A draft
    /// under the threshold is still stored and still reviewable — it simply
    /// carries nothing a reviewer could apply.
    #[serde(default = "default_min_evidence")]
    pub min_evidence: u32,
    /// Plan authoring by the LLM proposer (default: on, two steps minimum).
    #[serde(default)]
    pub plans: PlanAuthoring,
    /// The Verify gate's second question (default: on). An applied
    /// recommendation cites the grains it was derived from; when one of them
    /// is later superseded by a DIFFERENT value, or retracted, the premise
    /// the reviewer approved no longer holds. A lesson that outlives its
    /// premise is measured harm: on PAST-Bench a rule encoding the old
    /// regime's flag cost the governed arm 0.32 on the migration family it
    /// was learned in (`crates/areev-bench/PERSIST.md`). With this on, the
    /// gate records `drifted` and proposes the revert; a value-identical
    /// supersession (consolidation) is not drift.
    #[serde(default = "default_true")]
    pub premise_drift: bool,
}

fn is_default_cadence(c: &Cadence) -> bool {
    !c.is_set()
}

impl Default for Policy {
    fn default() -> Self {
        Policy {
            auto_apply_enabled: false,
            auto_apply: Vec::new(),
            deny: Vec::new(),
            severity_floors: BTreeMap::new(),
            telemetry: TelemetryMode::default(),
            discover_objective: DiscoverObjective::default(),
            outcome_evalset: None,
            evidence_attribution: EvidenceAttribution::default(),
            cadence: Cadence::default(),
            skills: SkillAuthoring::default(),
            min_evidence: 1,
            plans: PlanAuthoring::default(),
            premise_drift: true,
        }
    }
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
    fn outcome_evalset_parses_with_default_horizons() {
        let p = Policy::from_json(
            r#"{"outcome_evalset": {"hash": "abc123", "field": "exact", "higher_is_better": true}}"#,
        )
        .unwrap();
        let e = p.outcome_evalset.expect("parsed");
        assert_eq!((e.hash.as_str(), e.field.as_str(), e.higher_is_better), ("abc123", "exact", true));
        assert_eq!(e.horizons_ms, vec![86_400_000, 7 * 86_400_000, 30 * 86_400_000]);
        assert!(Policy::default().outcome_evalset.is_none());
        assert!(
            Policy::from_json(r#"{"outcome_evalset": {"hash": "abc123", "field": "exact"}}"#).is_err(),
            "the direction is not optional — a guessed one could revert an improvement"
        );
    }

    #[test]
    fn checkpoints_take_the_deployments_unit_and_a_bare_integer_stays_ms() {
        let p = Policy::from_json(
            r#"{"outcome_evalset": {"hash": "f", "field": "task_score", "higher_is_better": true,
                "checkpoints": [{"after_runs": 1}, 3600000, {"after_grains": 50}, {"after_ms": 86400000}]}}"#,
        )
        .unwrap();
        let e = p.outcome_evalset.unwrap();
        assert_eq!(
            e.schedule(),
            vec![
                Checkpoint::AfterMs(3_600_000),
                Checkpoint::AfterMs(86_400_000),
                Checkpoint::AfterRuns(1),
                Checkpoint::AfterGrains(50),
            ],
            "sorted, deduplicated, and the bare integer read as milliseconds"
        );
        // Nothing set: the ms defaults, as time checkpoints — the schedule
        // every deployment had before checkpoints had units.
        let p = Policy::from_json(r#"{"outcome_evalset": {"hash": "f", "field": "x", "higher_is_better": true}}"#).unwrap();
        assert_eq!(
            p.outcome_evalset.unwrap().schedule(),
            vec![
                Checkpoint::AfterMs(86_400_000),
                Checkpoint::AfterMs(7 * 86_400_000),
                Checkpoint::AfterMs(30 * 86_400_000)
            ]
        );
        for bad in [
            r#"[{"after_turns": 3}]"#,
            r#"[{"after_runs": -1}]"#,
            r#"["1d"]"#,
            r#"[{"after_runs": 1, "after_ms": 2}]"#,
        ] {
            let js = format!(r#"{{"outcome_evalset": {{"hash": "f", "field": "x", "higher_is_better": true, "checkpoints": {bad}}}}}"#);
            assert!(Policy::from_json(&js).is_err(), "{bad} must not load");
        }
    }

    #[test]
    fn cadence_defaults_to_always_due_and_parses_every_unit() {
        let p = Policy::default();
        assert!(!p.cadence.is_set());
        let p = Policy::from_json(
            r#"{"cadence": {"every_ms": 3600000, "every_events": 10, "every_sessions": 1, "every_grains": 50}}"#,
        )
        .unwrap();
        assert!(p.cadence.is_set());
        assert_eq!(p.cadence.every_events, Some(10));
        assert!(
            Policy::from_json(r#"{"cadence": {"every_turns": 10}}"#).is_err(),
            "an unknown unit must not load as always-due"
        );
        // An unset cadence does not appear in the effective policy print.
        assert!(!serde_json::to_string(&Policy::default()).unwrap().contains("cadence"));
    }

    #[test]
    fn skills_default_on_with_two_steps_and_min_evidence_defaults_to_one() {
        let p = Policy::default();
        assert!(p.skills.enabled);
        assert_eq!(p.skills.min_steps, 2);
        assert_eq!(p.min_evidence, 1, "one instance may become a rule — today's behaviour");
        let p = Policy::from_json(r#"{"skills": {"enabled": false}, "min_evidence": 2}"#).unwrap();
        assert!(!p.skills.enabled);
        assert_eq!(p.skills.min_steps, 2, "the unset field keeps its default, not zero");
        assert_eq!(p.min_evidence, 2);
        assert!(Policy::from_json(r#"{"skills": {"auto_apply": true}}"#).is_err(), "no back door");
        // The JSON default round-trips through from_json identically.
        let round = Policy::from_json(&serde_json::to_string(&Policy::default()).unwrap()).unwrap();
        assert_eq!(round.min_evidence, 1);
        assert!(round.skills.enabled);
    }

    #[test]
    fn plans_and_premise_drift_default_on_and_are_switchable() {
        let p = Policy::default();
        assert!(p.plans.enabled);
        assert_eq!(p.plans.min_nodes, 2);
        assert!(p.premise_drift);
        let p = Policy::from_json(r#"{"plans": {"enabled": false}, "premise_drift": false}"#).unwrap();
        assert!(!p.plans.enabled);
        assert_eq!(p.plans.min_nodes, 2);
        assert!(!p.premise_drift);
        assert!(Policy::from_json(r#"{"plans": {"auto_apply": true}}"#).is_err(), "no back door");
    }

    #[test]
    fn evidence_attribution_defaults_to_named() {
        assert_eq!(Policy::default().evidence_attribution, EvidenceAttribution::Named);
        let p = Policy::from_json(r#"{"evidence_attribution": "anonymous"}"#).unwrap();
        assert_eq!(p.evidence_attribution, EvidenceAttribution::Anonymous);
        assert!(
            Policy::from_json(r#"{"evidence_attribution": "redacted"}"#).is_err(),
            "an unknown mode must not load as the default"
        );
    }

    #[test]
    fn unknown_keys_rejected() {
        // A trust-floor field or an executable registration must not load.
        assert!(Policy::from_json(r#"{"analyzer_cmd": "evil"}"#).is_err());
        assert!(Policy::from_json(r#"{"auto_apply_free_text": true}"#).is_err());
    }
}
