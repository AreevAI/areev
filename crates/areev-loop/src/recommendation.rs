//! The recommendation object (OMS 0x0C), the deterministic summary renderer,
//! the dedup key, and the lifecycle state machine + audit records.
//!
//! Invariants enforced here:
//! - Analyzers emit a `(template_id, args)` summary, never free prose.
//! - `dedup_key`, `origin`, and the params snapshot are engine-stamped.
//! - `dedup_key` excludes proposal content and the `/major` version, so a
//!   growing cluster or an analyzer upgrade does not re-propose as novel.
//! - Lifecycle transitions are gated; `pending → applied` is policy-only.

use crate::error::{Error, Result};
use crate::model::{normalize_ident, ActionKind, Origin, Severity};
use crate::substrate::GrainSpec;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// A deterministic, template-rendered summary. The analyzer chooses a
/// `template_id` and supplies `args`; the text is produced here, so an
/// analyzer can never emit arbitrary prose into the queue.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Summary {
    pub template_id: String,
    #[serde(default)]
    pub args: Map<String, Value>,
}

impl Summary {
    pub fn new(template_id: impl Into<String>, args: Map<String, Value>) -> Self {
        Summary {
            template_id: template_id.into(),
            args,
        }
    }

    /// Render the summary. Unknown template ids fall back to a stable, honest
    /// string (never a panic) so a manifest/template mismatch is visible, not
    /// fatal.
    pub fn render(&self) -> String {
        let t = builtin_template(&self.template_id).unwrap_or("{template_id}: {summary}");
        interpolate(t, &self.args, &self.template_id)
    }
}

/// The built-in template table. Deterministic; the only place summary prose
/// lives. Keep placeholders in `{name}` form matching `args` keys.
fn builtin_template(id: &str) -> Option<&'static str> {
    Some(match id {
        "duplicate.exact" => "Consolidate {count} exact-duplicate grains for \"{subject}\"",
        "duplicate.near" => {
            "Consolidate {count} near-duplicate observations (similarity ≥ {threshold})"
        }
        "contradiction.functional" => {
            "\"{subject}\" holds {count} live values for functional relation \"{relation}\""
        }
        "tool_failure.cluster" => {
            "Tool \"{tool}\" failed {count} times ({rate}% of calls): {signature}"
        }
        "staleness.expired" => "Expire \"{subject}\": past its declared valid_to ({age_days}d ago)",
        "fork.multi_head" => "Entity \"{entity}\" has {count} competing heads",
        "skill.stall" => {
            "Skill \"{skill}\" isn't improving: practiced {practice_count}× but proficiency is still {proficiency}"
        }
        "goal.stagnation" => "Goal \"{goal}\" is stalled: active {age_days}d with {progress} progress",
        "cold.grain" => {
            "Cold memory: \"{subject}\" ({age_days}d old) has never been recalled — retire candidate"
        }
        "coverage.gap" => {
            "Recurring question with no matching memory: \"{query}\" asked {count}× ({empty_rate}% empty)"
        }
        "budget.pressure" => {
            "Assembly budget overflowed on {overflow_rate}% of {samples} recalls — raise the budget or curate memory"
        }
        "retention.overdue" => {
            "Retention: {count} grain(s) in \"{namespace}\" exceed the {max_age_days}d policy (oldest {oldest_days}d); {remaining} more await a later pass"
        }
        "outcome.regression" => {
            "Applied recommendation regressed: {metric} moved {baseline} → {current}"
        }
        "run.failures" => {
            "Workflow {workflow} failed {failed}/{runs} recent runs ({rate}%): {last_error}"
        }
        "run.cost" => {
            "Workflow {workflow} spent ${usd} across {runs} runs (avg ${avg_usd}/run)"
        }
        // origin=llm drafts carry free text (clearly marked llm) rather than a
        // deterministic template — the model's proposed summary rides in {text}.
        "llm.discover" => "{text}",
        // External command analyzers (trust class Command): free text from the
        // subprocess, rendered as-is (the trust badge, not the prose, marks it).
        "command.finding" => "{text}",
        _ => return None,
    })
}

fn interpolate(template: &str, args: &Map<String, Value>, template_id: &str) -> String {
    let mut out = String::with_capacity(template.len());
    let mut chars = template.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '{' {
            let mut key = String::new();
            for k in chars.by_ref() {
                if k == '}' {
                    break;
                }
                key.push(k);
            }
            if key == "template_id" {
                out.push_str(template_id);
            } else {
                match args.get(&key) {
                    Some(Value::String(s)) => out.push_str(s),
                    Some(v) => out.push_str(&v.to_string()),
                    None => {
                        // Missing arg — leave a visible marker rather than lie.
                        out.push('{');
                        out.push_str(&key);
                        out.push('}');
                    }
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// A reproducible metric snapshot; powers outcome review.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MetricSnapshot {
    /// Metric kind — the engine knows how to re-measure a fixed set
    /// (e.g. `tool_error_recurrence`); unknown kinds are skipped, not faked.
    pub metric: String,
    pub baseline: f64,
    pub unit: String,
    pub n: u64,
    pub window: String,
    /// The subject the metric is about (e.g. the tool name), used by the
    /// engine's typed re-measurement.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subject: Option<String>,
    /// Namespace scope for the re-measurement, normalized (case-fold/trim).
    /// `None` = all namespaces. Additive: older snapshots deserialize to
    /// `None` and existing metric kinds ignore it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub namespace: Option<String>,
    /// Relation scope for fact-shaped metrics (e.g. which functional relation
    /// a contradiction was resolved under), normalized. Additive like
    /// `namespace`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub relation: Option<String>,
    /// CAL that recomputes the metric at verify time (reproducibility /
    /// documentation; the engine re-measures with typed reads).
    pub query: String,
    /// How long after apply to re-measure, in epoch-ms delta (the first / only
    /// checkpoint when `horizons_ms` is empty).
    pub review_after_ms: i64,
    /// A schedule of checkpoints (ms after apply) to re-measure at. An outcome
    /// that `held` at an early checkpoint can `regress` at a later one, so a
    /// verdict is never final until the last horizon. Empty → `[review_after_ms]`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub horizons_ms: Vec<i64>,
    /// Which direction is an improvement. The built-in metrics are all
    /// recurrence counts, where lower is better — so this defaults to `false`
    /// and every existing snapshot deserializes unchanged. An evalset accuracy
    /// is the opposite, and getting it wrong does not merely misreport: the
    /// Verify gate would read a rule that IMPROVED accuracy as a regression and
    /// propose reverting it.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub higher_is_better: bool,
}

/// The one regression rule.
///
/// The verdict is computed in two places — once by the engine for the recorded
/// `OutcomeResult`, once by `outcome_review` when it drafts the revert — and
/// the two silently disagreeing would either revert a change that held or sit
/// on one that regressed. So both call this.
pub fn is_regression(baseline: f64, current: f64, higher_is_better: bool) -> bool {
    /// Minimum worsening to call a regression (avoids noise at n=1).
    const EPSILON: f64 = 1e-9;
    if higher_is_better {
        current < baseline - EPSILON
    } else {
        current > baseline + EPSILON
    }
}

impl MetricSnapshot {
    /// The measurement schedule, sorted — `horizons_ms` if set, else the single
    /// `review_after_ms`.
    pub fn horizons(&self) -> Vec<i64> {
        let mut h = if self.horizons_ms.is_empty() {
            vec![self.review_after_ms]
        } else {
            self.horizons_ms.clone()
        };
        h.sort_unstable();
        h
    }
}

/// A measured outcome for an applied recommendation at one checkpoint — the
/// Verify gate's output. `held` = the metric did not regress at this horizon;
/// `regressed` = it got worse (a revert is proposed). A recommendation
/// accumulates one of these per horizon, forming a time series.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OutcomeResult {
    pub rec_hash: String,
    pub metric: String,
    pub baseline: f64,
    pub current: f64,
    pub verdict: String,
    /// Which checkpoint this measurement is for (ms after apply).
    #[serde(default)]
    pub horizon_ms: i64,
    pub measured_at_ms: i64,
}

/// The proposed change. Exactly one variant per recommendation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "proposal", rename_all = "snake_case")]
pub enum Proposal {
    /// A batch of CAL Tier-1 evolve writes (ADD/SUPERSEDE), MAY contain FORGET.
    Cal { cal: String },
    /// A doc-target edit: `{format, base_digest, diff}` (base_digest enables a
    /// staleness check at apply).
    Edit {
        format: String,
        base_digest: String,
        diff: String,
    },
    /// An opaque map for host targets (applied by the host, §12.3).
    Data { data: Map<String, Value> },
}

/// What an analyzer emits. `dedup_key`, `origin`, and the params snapshot are
/// **not** here — the engine stamps them. Non-exhaustive so the engine can add
/// fields without breaking analyzers.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct RecDraft {
    pub target_ref: String,
    pub action_kind: ActionKind,
    pub summary: Summary,
    pub severity: Severity,
    pub proposal: Proposal,
    /// Bounded to ≤64 representative evidence hashes by the engine.
    pub evidence: Vec<String>,
    pub evidence_query: Option<String>,
    pub metric: Option<MetricSnapshot>,
    pub confidence: f64,
    pub importance: f64,
    /// Rule E1 pin for `code_revision` drafts (see [`validate_code_rules`]).
    pub evalset_hash: Option<String>,
}

impl RecDraft {
    pub fn new(
        target_ref: impl Into<String>,
        action_kind: ActionKind,
        summary: Summary,
        proposal: Proposal,
    ) -> Self {
        RecDraft {
            target_ref: target_ref.into(),
            action_kind,
            summary,
            severity: Severity::Low,
            proposal,
            evidence: Vec::new(),
            evidence_query: None,
            metric: None,
            confidence: 0.8,
            importance: 0.5,
            evalset_hash: None,
        }
    }

    pub fn severity(mut self, s: Severity) -> Self {
        self.severity = s;
        self
    }
    pub fn evidence(mut self, hashes: Vec<String>) -> Self {
        self.evidence = hashes;
        self
    }
    pub fn evidence_query(mut self, q: impl Into<String>) -> Self {
        self.evidence_query = Some(q.into());
        self
    }
    pub fn metric(mut self, m: MetricSnapshot) -> Self {
        self.metric = Some(m);
        self
    }
    pub fn confidence(mut self, c: f64) -> Self {
        self.confidence = c;
        self
    }
    pub fn importance(mut self, i: f64) -> Self {
        self.importance = i;
        self
    }
    pub fn evalset_hash(mut self, h: impl Into<String>) -> Self {
        self.evalset_hash = Some(h.into());
        self
    }
}

/// Maximum representative evidence hashes carried inline (proposal §7.1).
pub const MAX_EVIDENCE: usize = 64;

/// Compute the dedup key: `family ⟂ target_ref ⟂ action_kind`, case-folded.
/// Excludes proposal content and evidence by construction.
pub fn dedup_key(family: &str, target_ref: &str, action: ActionKind) -> String {
    // U+001F (unit separator) cannot appear in any of the inputs.
    format!(
        "{}\u{1f}{}\u{1f}{}",
        normalize_ident(family),
        normalize_ident(target_ref),
        action.as_str()
    )
}

/// Lifecycle status — a rebuildable index-layer cache (the recommendation's
/// content hash is stable for its whole life).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecStatus {
    #[default]
    Pending,
    Approved,
    Rejected,
    Applied,
    RolledBack,
    Expired,
}

impl RecStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            RecStatus::Pending => "pending",
            RecStatus::Approved => "approved",
            RecStatus::Rejected => "rejected",
            RecStatus::Applied => "applied",
            RecStatus::RolledBack => "rolled_back",
            RecStatus::Expired => "expired",
        }
    }

    /// Is a transition to `to` allowed from this state? `by_policy` marks the
    /// auto-apply actor, the only one permitted the reasonless
    /// `pending → applied` jump.
    pub fn can_transition_to(&self, to: RecStatus, by_policy: bool) -> bool {
        use RecStatus::*;
        match (self, to) {
            (Pending, Approved) | (Pending, Rejected) => true,
            (Pending, Applied) => by_policy, // auto-apply only
            (Approved, Applied) => true,
            (Applied, RolledBack) => true,
            // `expired` is computed from valid_to, applied to still-open recs.
            (Pending, Expired) | (Approved, Expired) => true,
            _ => false,
        }
    }
}

/// Who/what performed a transition.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ObserverType {
    Human,
    Agent,
    Policy,
    System,
}

/// One immutable audit Observation per transition, hash-chained per
/// recommendation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AuditRecord {
    pub rec_hash: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from: Option<RecStatus>,
    pub to: RecStatus,
    /// Host-asserted actor label, e.g. `user:alice`, `agent:worker-3`,
    /// `policy:auto`.
    pub actor: String,
    pub observer_type: ObserverType,
    /// Mandatory written reason (≤500 chars), the review statement's BECAUSE.
    pub because: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub previous_audit_hash: Option<String>,
    /// §7.4: present exactly when this transition applied a code revision —
    /// the evalset-run edge, recorded where the forensics look.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gating: Option<GatingEvidence>,
    pub at_ms: i64,
}

/// Maximum length of a BECAUSE reason.
pub const MAX_BECAUSE: usize = 500;

impl AuditRecord {
    /// Build the Observation grain that records this transition. `derived_from`
    /// chains `[rec_hash, previous_audit_hash]` per the lifecycle spec.
    pub fn to_grain_spec(&self, namespace: &str) -> GrainSpec {
        let mut derived_from = vec![Value::from(self.rec_hash.clone())];
        if let Some(prev) = &self.previous_audit_hash {
            derived_from.push(Value::from(prev.clone()));
        }
        let mut spec = GrainSpec::new(crate::model::grain_type::OBSERVATION, namespace)
            .with_field("observation_kind", "loop_audit")
            .with_field("rec_hash", self.rec_hash.clone())
            .with_field("to_status", self.to.as_str())
            .with_field("actor", self.actor.clone())
            .with_field(
                "observer_type",
                serde_json::to_value(self.observer_type).unwrap(),
            )
            .with_field("because", self.because.clone())
            .with_field("at_ms", self.at_ms)
            .with_field("derived_from", Value::Array(derived_from));
        if let Some(from) = self.from {
            spec.fields
                .insert("from_status".into(), Value::from(from.as_str()));
        }
        if let Some(g) = &self.gating {
            spec.fields
                .insert("gating_evalset".into(), Value::from(g.evalset_hash.clone()));
            spec.fields
                .insert("gating_run_id".into(), Value::from(g.run_id.clone()));
            spec.fields.insert("gating_passed".into(), Value::from(g.passed));
            spec.fields.insert("gating_failed".into(), Value::from(g.failed));
        }
        spec
    }
}

/// A stored recommendation. `hash` (the content address) and `status` (the
/// index-layer cache) are set by the engine, not serialized into the grain
/// body — the body is immutable content, the lifecycle lives in the state
/// index and the audit chain.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Recommendation {
    #[serde(skip)]
    pub hash: String,
    pub analyzer: String,
    pub params_snapshot: Map<String, Value>,
    pub origin: Origin,
    pub target_ref: String,
    pub action_kind: ActionKind,
    pub dedup_key: String,
    pub summary: Summary,
    pub severity: Severity,
    #[serde(flatten)]
    pub proposal: Proposal,
    pub destructive: bool,
    pub rollbackable: bool,
    #[serde(default)]
    pub evidence: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evidence_query: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metric: Option<MetricSnapshot>,
    pub confidence: f64,
    pub importance: f64,
    pub created_at_ms: i64,
    /// Optional LLM guidance: an ENRICH note on a deterministic recommendation,
    /// or an `origin = llm` draft's own rationale (§9). Whitelisted, capped
    /// text — it never replaces the engine-templated `summary`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub guidance: Option<String>,
    /// Rule E1's pin (§7.4): the evalset hash a `code_revision` was gated
    /// against — REQUIRED for code targets, forbidden elsewhere (a
    /// recommendation targets code XOR an evalset, never both, and only
    /// code carries the pin). Shown at review; checked live at apply
    /// (superseded evalset ⇒ the pin is stale ⇒ re-gate).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evalset_hash: Option<String>,
    #[serde(skip)]
    pub status: RecStatus,
}

/// The recorded evalset-run edge (§7.4): what an apply of a code revision
/// must present, and what its audit Observation carries. "Trust me, it
/// passed" is not an edge; (evalset, run, stats) is.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GatingEvidence {
    /// The evalset the gate ran — must equal the recommendation's pin.
    pub evalset_hash: String,
    /// The `areev run`/`areev eval` run id that executed the gate.
    pub run_id: String,
    pub passed: u64,
    pub failed: u64,
}

/// Rule E1's structural checks (§7.4), applied when a draft is stamped and
/// re-checked when a stored grain is loaded (a hand-authored grain must not
/// bypass the rule):
/// - `code_revision` ⇔ a `tool:` target, and it MUST pin an evalset hash.
/// - An `evalset:` target is its own always-human-approved class: never a
///   `code_revision`, and never itself pinned (the gate cannot gate itself).
/// - No other target class carries a pin.
pub fn validate_code_rules(
    action: ActionKind,
    target_class: &str,
    evalset_hash: Option<&str>,
) -> Result<()> {
    let e1 = |why: &str| Err(Error::InvalidRecommendation(format!("Rule E1: {why}")));
    match (action, target_class) {
        (ActionKind::CodeRevision, "code") => {
            if evalset_hash.is_none_or(|h| h.trim().is_empty()) {
                return e1(
                    "a code_revision must pin the evalset hash it was gated \
                     against (evalset_hash)",
                );
            }
        }
        (ActionKind::CodeRevision, other) => {
            return e1(&format!(
                "code_revision requires a tool: target, got class '{other}'"
            ));
        }
        // A REVERT of an applied code revision targets the same tool: —
        // it is the rollback inverse, not new code, so it needs no pin
        // (blocking it here would leave the highest-stakes target class
        // unable to propose its own measured rollback).
        (ActionKind::Revert, "code") => {
            if evalset_hash.is_some() {
                return e1("a code revert carries no evalset pin");
            }
        }
        (_, "code") => {
            return e1("a tool: target requires action_kind code_revision");
        }
        (_, "evalset") => {
            if evalset_hash.is_some() {
                return e1(
                    "an evalset-target recommendation cannot pin an evalset — \
                     the gate cannot gate itself",
                );
            }
        }
        _ => {
            if evalset_hash.is_some() {
                return e1("evalset_hash is only valid on code_revision recommendations");
            }
        }
    }
    Ok(())
}

impl Recommendation {
    /// Serialize the immutable body into grain fields (excludes hash/status).
    pub fn to_grain_spec(&self, namespace: &str) -> Result<GrainSpec> {
        let value = serde_json::to_value(self)
            .map_err(|e| Error::Internal(format!("serialize recommendation: {e}")))?;
        let obj = value
            .as_object()
            .ok_or_else(|| Error::Internal("recommendation did not serialize to object".into()))?
            .clone();
        Ok(GrainSpec {
            grain_type: crate::model::grain_type::RECOMMENDATION.to_string(),
            namespace: namespace.to_string(),
            fields: obj,
        })
    }

    /// Reconstruct from a stored grain body. `hash` comes from the record's
    /// address; `status` is supplied from the state index by the caller.
    /// Rule E1 is RE-CHECKED here — a hand-authored recommendation grain
    /// must not smuggle an unpinned code revision past the stamped path.
    pub fn from_fields(hash: &str, fields: &Map<String, Value>) -> Result<Self> {
        let mut rec: Recommendation = serde_json::from_value(Value::Object(fields.clone()))
            .map_err(|e| Error::InvalidRecommendation(format!("decode {hash}: {e}")))?;
        rec.hash = hash.to_string();
        let class = crate::model::TargetRef::parse(&rec.target_ref)
            .map(|t| t.target_class())
            .unwrap_or("host");
        validate_code_rules(rec.action_kind, class, rec.evalset_hash.as_deref())?;
        Ok(rec)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn summary_renders_deterministically() {
        let mut args = Map::new();
        args.insert("count".into(), json!(3));
        args.insert("subject".into(), json!("acme"));
        let s = Summary::new("duplicate.exact", args);
        assert_eq!(
            s.render(),
            "Consolidate 3 exact-duplicate grains for \"acme\""
        );
    }

    #[test]
    fn dedup_key_ignores_content_and_case() {
        let a = dedup_key(
            "loop.duplicate_sweep",
            "entity:NS/John",
            ActionKind::Consolidate,
        );
        let b = dedup_key(
            "loop.duplicate_sweep",
            "entity:ns/john",
            ActionKind::Consolidate,
        );
        assert_eq!(a, b, "case-folded to one identity");
    }

    #[test]
    fn dedup_key_distinguishes_action() {
        let a = dedup_key("f", "entity:ns/x", ActionKind::Consolidate);
        let b = dedup_key("f", "entity:ns/x", ActionKind::FlagContradiction);
        assert_ne!(a, b);
    }

    #[test]
    fn lifecycle_gates_pending_to_applied() {
        assert!(!RecStatus::Pending.can_transition_to(RecStatus::Applied, false));
        assert!(RecStatus::Pending.can_transition_to(RecStatus::Applied, true)); // policy
        assert!(RecStatus::Pending.can_transition_to(RecStatus::Approved, false));
        assert!(RecStatus::Approved.can_transition_to(RecStatus::Applied, false));
        assert!(RecStatus::Applied.can_transition_to(RecStatus::RolledBack, false));
        assert!(!RecStatus::Rejected.can_transition_to(RecStatus::Applied, true));
    }

    #[test]
    fn rule_e1_pins_code_and_only_code() {
        use crate::model::ActionKind as A;
        // code_revision on a tool target with a pin: OK.
        assert!(validate_code_rules(A::CodeRevision, "code", Some("abc")).is_ok());
        // Unpinned code revision: refused.
        assert!(validate_code_rules(A::CodeRevision, "code", None).is_err());
        assert!(validate_code_rules(A::CodeRevision, "code", Some("  ")).is_err());
        // code_revision off a tool target: refused.
        assert!(validate_code_rules(A::CodeRevision, "memory", Some("abc")).is_err());
        // A tool target with a non-code action: refused.
        assert!(validate_code_rules(A::Flag, "code", None).is_err());
        // The gate cannot gate itself.
        assert!(validate_code_rules(A::Flag, "evalset", Some("abc")).is_err());
        assert!(validate_code_rules(A::Flag, "evalset", None).is_ok());
        // Pins don't leak onto ordinary targets.
        assert!(validate_code_rules(A::Consolidate, "memory", Some("abc")).is_err());
        assert!(validate_code_rules(A::Consolidate, "memory", None).is_ok());
    }

    #[test]
    fn from_fields_rechecks_rule_e1() {
        // A hand-authored grain body carrying an unpinned code revision must
        // not decode into a usable recommendation.
        let mut rec = Recommendation {
            hash: String::new(),
            analyzer: "loop.codegen/1".into(),
            params_snapshot: Map::new(),
            origin: Origin::Builtin,
            target_ref: "tool:abc123".into(),
            action_kind: ActionKind::CodeRevision,
            dedup_key: "k".into(),
            summary: Summary::new("command.finding", Map::new()),
            severity: Severity::Low,
            proposal: Proposal::Data { data: Map::new() },
            destructive: false,
            rollbackable: false,
            evidence: vec![],
            evidence_query: None,
            metric: None,
            confidence: 0.5,
            importance: 0.5,
            created_at_ms: 0,
            guidance: None,
            evalset_hash: None, // the smuggle attempt
            status: RecStatus::Pending,
        };
        let spec = rec.to_grain_spec("ns").unwrap();
        assert!(Recommendation::from_fields("h", &spec.fields).is_err());
        rec.evalset_hash = Some("es-hash".into());
        let spec = rec.to_grain_spec("ns").unwrap();
        let back = Recommendation::from_fields("h", &spec.fields).unwrap();
        assert_eq!(back.evalset_hash.as_deref(), Some("es-hash"));
    }

    #[test]
    fn recommendation_round_trips_through_fields() {
        let rec = Recommendation {
            hash: "ignored".into(),
            analyzer: "loop.staleness/1".into(),
            params_snapshot: Map::new(),
            origin: Origin::Builtin,
            target_ref: "grain:sha256:abc".into(),
            action_kind: ActionKind::Expire,
            dedup_key: "k".into(),
            summary: Summary::new("staleness.expired", Map::new()),
            severity: Severity::Low,
            proposal: Proposal::Cal {
                cal: "FORGET sha256:abc".into(),
            },
            destructive: true,
            rollbackable: false,
            evidence: vec!["sha256:abc".into()],
            evidence_query: None,
            metric: None,
            confidence: 0.9,
            importance: 0.4,
            created_at_ms: 1000,
            guidance: None,
            evalset_hash: None,
            status: RecStatus::Pending,
        };
        let spec = rec.to_grain_spec("ns").unwrap();
        // hash and status are excluded from the immutable body.
        assert!(!spec.fields.contains_key("hash"));
        assert!(!spec.fields.contains_key("status"));
        let back = Recommendation::from_fields("realhash", &spec.fields).unwrap();
        assert_eq!(back.hash, "realhash");
        assert_eq!(back.analyzer, "loop.staleness/1");
        assert!(back.destructive);
        assert!(matches!(back.proposal, Proposal::Cal { .. }));
    }
}
