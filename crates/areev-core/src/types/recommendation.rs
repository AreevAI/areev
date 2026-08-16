use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use unicode_normalization::UnicodeNormalization;

use super::grain::{Grain, GrainCommon, GrainType};

/// The change a Recommendation proposes. Exactly one form is present
/// (OMS 1.5 §8.12 rule 1), which is why this is an enum rather than three
/// optional fields — the invariant is unrepresentable-if-broken.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Proposal {
    /// A CAL batch of Tier-1 *evolve* writes (`ADD`/`SUPERSEDE`/`REVERT`).
    /// The standard form: CAL has no destructive verb, so this is always
    /// non-destructive and always invertible.
    Cal(String),
    /// `{format, base_digest, diff}` for a `doc:` target. `base_digest` lets
    /// an applier detect a stale base before editing.
    Edit(serde_json::Value),
    /// An opaque host-config change for a `host:` target. Outside CAL, so a
    /// host must validate it against its own schema — and it may be
    /// irreversible, in which case the applier records it as not rollbackable.
    Data(serde_json::Value),
}

impl Proposal {
    /// The `action-kind` component of the dedup key: the literal `cal`, `edit`
    /// or `data` (§8.12 rule 5).
    pub fn action_kind(&self) -> &'static str {
        match self {
            Proposal::Cal(_) => "cal",
            Proposal::Edit(_) => "edit",
            Proposal::Data(_) => "data",
        }
    }
}

/// The analyzer that produced a recommendation (§8.12 `analyzer`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Analyzer {
    /// Versioned identifier `publisher.name/major`, e.g.
    /// `"loop.duplicate_sweep/1"`.
    pub id: String,
    /// Optional snapshot of the parameters it ran with — full "why" provenance
    /// without a second grain.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<serde_json::Value>,
}

impl Analyzer {
    pub fn new(id: &str) -> Self {
        Analyzer {
            id: id.to_string(),
            params: None,
        }
    }

    /// The **analyzer-family**: `id` with the `/major` suffix removed.
    ///
    /// Bumping `.../1` → `.../2` must not re-propose the whole queue as novel,
    /// so the family (not the exact version) feeds the dedup key while
    /// `analyzer.id` keeps the precise version.
    pub fn family(&self) -> &str {
        match self.id.rfind('/') {
            Some(i) => &self.id[..i],
            None => &self.id,
        }
    }
}

/// A deterministic, template-rendered description (§8.12 `summary`).
///
/// An analyzer MUST NOT store free prose here: the human-readable string is
/// produced by rendering `template_id` with `args`, so summaries stay
/// reproducible and translatable.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Summary {
    pub template_id: String,
    pub args: serde_json::Value,
}

/// Advisory triage signal (§8.12 `severity`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Info,
    Low,
    Medium,
    High,
}

impl Severity {
    pub fn as_str(&self) -> &'static str {
        match self {
            Severity::Info => "info",
            Severity::Low => "low",
            Severity::Medium => "medium",
            Severity::High => "high",
        }
    }

    /// Forward-compatible: an unknown wire value is ignored, not an error.
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "info" => Some(Severity::Info),
            "low" => Some(Severity::Low),
            "medium" => Some(Severity::Medium),
            "high" => Some(Severity::High),
            _ => None,
        }
    }
}

/// Review state of a recommendation (§8.12 `rec_status`).
///
/// **Index-layer, never stored in the blob.** Like `superseded_by` and
/// `verification_status` it is a rebuildable cache derived from the audit
/// chain (§8.12.1), which is what makes a recommendation's content address
/// stable across its whole lifecycle: propose, approve, apply and roll back
/// never re-address the grain. A writer MUST NOT set it, and a
/// `SUPERSEDE … SET rec_status` MUST be rejected (rule 3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecStatus {
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

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "pending" => Some(RecStatus::Pending),
            "approved" => Some(RecStatus::Approved),
            "rejected" => Some(RecStatus::Rejected),
            "applied" => Some(RecStatus::Applied),
            "rolled_back" => Some(RecStatus::RolledBack),
            "expired" => Some(RecStatus::Expired),
            _ => None,
        }
    }

    /// Whether `self → next` is a legal transition (§8.12.1 state machine).
    ///
    /// `pending → applied` is auto-apply and is gated on the transition's
    /// `observer_type` being `"rec:policy"`, which is a property of the audit
    /// grain rather than of the pair — so it is admitted here and enforced by
    /// the caller that holds the audit grain.
    pub fn can_transition_to(self, next: RecStatus) -> bool {
        use RecStatus::*;
        match (self, next) {
            (Pending, Approved | Rejected | Applied | Expired) => true,
            (Approved, Applied | Expired) => true,
            (Applied, RolledBack) => true,
            // Re-enter review on refreshed evidence under the existing
            // dedup_key rather than becoming a second proposal.
            (Rejected, Pending) => true,
            _ => false,
        }
    }

    /// `applied` and `rolled_back` are terminal.
    pub fn is_terminal(self) -> bool {
        matches!(self, RecStatus::Applied | RecStatus::RolledBack)
    }
}

/// Compute the normative `dedup_key` (OMS 1.5 §8.12 rule 5).
///
/// ```text
/// dedup_key = hex(sha256(nfc(casefold(analyzer_family)) || 0x00 ||
///                        nfc(target_ref)                || 0x00 ||
///                        action_kind))
/// ```
///
/// Two implementations MUST derive the same key for the same proposal or dedup
/// fails on any imported, federated or forked store — so the construction is
/// normative and this is the only place it is computed. Note `target_ref` is
/// NFC-normalized but deliberately **not** case-folded: a `grain:sha256:`
/// digest and a host-defined opaque segment may both be case-sensitive.
/// Normalization is NFC applied *after* case-folding.
///
/// `action_kind` is hashed raw — neither folded nor NFC-normalized — because it
/// is a closed ASCII vocabulary fixed by the spec, for which both operations are
/// the identity. The asymmetry is deliberate: normalizing it here would be a
/// unilateral change to a construction two implementations must agree on byte
/// for byte, and agreement is the entire purpose of the recipe.
pub fn compute_dedup_key(analyzer_family: &str, target_ref: &str, action_kind: &str) -> String {
    // Rust has no full Unicode case-folding in std; `to_lowercase` is the
    // closest available and agrees with full case-folding for every
    // analyzer-id character class in practice (ASCII identifiers).
    let family: String = analyzer_family.to_lowercase().nfc().collect();
    let target: String = target_ref.nfc().collect();

    let mut h = Sha256::new();
    h.update(family.as_bytes());
    h.update([0x00]);
    h.update(target.as_bytes());
    h.update([0x00]);
    h.update(action_kind.as_bytes());
    hex::encode(h.finalize())
}

/// A Recommendation grain (OMS 1.5 §8.12, type byte `0x0C`) — a governed,
/// auditable proposal to change memory or agent configuration.
///
/// It never mutates memory by itself: it enters a propose → review → apply →
/// roll-back lifecycle whose transitions are immutable audit Observation
/// grains, and only an approved-and-applied recommendation changes the store.
///
/// Two identities matter and they are different. The **content address** is
/// stable across lifecycle transitions (status lives in the index layer), while
/// a change in *content* — refreshed evidence, a larger cluster — is a
/// supersession that mints a new address. The identity durable across a whole
/// supersession chain is [`Recommendation::dedup_key`], not the content
/// address.
#[derive(Debug, Clone)]
pub struct Recommendation {
    /// The change target as `<scheme>:<opaque>` — `grain:sha256:<hash>`,
    /// `entity:<ns>/<subject>`, `query:`/`template:`/`doc:`/`host:`. The scheme
    /// is the target-kind discriminator and the set is open for host targets.
    pub target_ref: String,
    /// The producing logic.
    pub analyzer: Analyzer,
    /// Deterministic, template-rendered description — never analyzer prose.
    pub summary: Summary,
    /// Stable proposal identity, computed per [`compute_dedup_key`].
    pub dedup_key: String,
    /// Exactly one proposal (rule 1).
    pub proposal: Proposal,
    pub severity: Option<Severity>,
    /// The measurable claim the recommendation rests on, for outcome review.
    pub metric_snapshot: Option<serde_json::Value>,
    /// CAL that regenerates the full evidence set when `derived_from` is
    /// truncated to a representative subset.
    pub evidence_query: Option<String>,
    pub common: GrainCommon,
}

impl Recommendation {
    /// Build a recommendation, computing `dedup_key` from the analyzer family,
    /// target and proposal kind. The key is never author-chosen.
    pub fn new(target_ref: &str, analyzer: Analyzer, summary: Summary, proposal: Proposal) -> Self {
        let dedup_key = compute_dedup_key(analyzer.family(), target_ref, proposal.action_kind());
        Recommendation {
            target_ref: target_ref.to_string(),
            analyzer,
            summary,
            dedup_key,
            proposal,
            severity: None,
            metric_snapshot: None,
            evidence_query: None,
            common: GrainCommon {
                confidence: 1.0,
                ..Default::default()
            },
        }
    }

    pub fn severity(mut self, s: Severity) -> Self {
        self.severity = Some(s);
        self
    }

    pub fn metric_snapshot(mut self, m: serde_json::Value) -> Self {
        self.metric_snapshot = Some(m);
        self
    }

    pub fn evidence_query(mut self, q: &str) -> Self {
        self.evidence_query = Some(q.to_string());
        self
    }

    /// Evidence: content addresses of the grains this is grounded in.
    /// RECOMMENDED ≤ 64; `evidence_query` regenerates the full set.
    pub fn evidence(mut self, hashes: &[String]) -> Self {
        self.common.derived_from = hashes.first().cloned();
        self
    }

    /// The target scheme (`grain`, `entity`, `query`, `template`, `doc`,
    /// `host`, or a host-defined one), i.e. the target-kind discriminator.
    pub fn target_scheme(&self) -> Option<&str> {
        self.target_ref.split_once(':').map(|(s, _)| s)
    }
}

impl Grain for Recommendation {
    fn grain_type(&self) -> GrainType {
        GrainType::Recommendation
    }

    fn common(&self) -> &GrainCommon {
        &self.common
    }

    fn common_mut(&mut self) -> &mut GrainCommon {
        &mut self.common
    }

    /// Embedding/search text. The summary is a template id plus args rather
    /// than prose, so the searchable text is the target plus the rendered
    /// argument values — what a human would actually search for.
    fn text(&self) -> String {
        let mut parts = vec![self.target_ref.clone()];
        if let Some(obj) = self.summary.args.as_object() {
            for v in obj.values() {
                match v {
                    serde_json::Value::String(s) if !s.is_empty() => parts.push(s.clone()),
                    serde_json::Value::Null => {}
                    other => parts.push(other.to_string()),
                }
            }
        }
        parts.join(" ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec() -> Recommendation {
        Recommendation::new(
            "entity:caller/alice",
            Analyzer::new("loop.duplicate_sweep/1"),
            Summary {
                template_id: "dup.consolidate".into(),
                args: serde_json::json!({ "count": 3 }),
            },
            Proposal::Cal("SUPERSEDE sha256:aa BECAUSE \"dup\"".into()),
        )
    }

    #[test]
    fn dedup_key_is_stable_across_an_analyzer_major_bump() {
        // The family feeds the key, not the exact version — bumping /1 to /2
        // must not re-propose the entire queue as novel.
        let a = rec();
        let mut b = rec();
        b.analyzer = Analyzer::new("loop.duplicate_sweep/2");
        let b = Recommendation::new(&b.target_ref, b.analyzer, b.summary, b.proposal);
        assert_eq!(a.dedup_key, b.dedup_key);
    }

    #[test]
    fn dedup_key_ignores_the_proposal_body_and_evidence() {
        // A content refresh must supersede, not re-propose.
        let a = rec();
        let b = Recommendation::new(
            "entity:caller/alice",
            Analyzer::new("loop.duplicate_sweep/1"),
            Summary {
                template_id: "dup.consolidate".into(),
                args: serde_json::json!({ "count": 9 }),
            },
            Proposal::Cal("SUPERSEDE sha256:bb BECAUSE \"more dups\"".into()),
        );
        assert_eq!(a.dedup_key, b.dedup_key);
    }

    #[test]
    fn dedup_key_separates_target_analyzer_and_action_kind() {
        let a = rec();
        let other_target = Recommendation::new(
            "entity:caller/bob",
            Analyzer::new("loop.duplicate_sweep/1"),
            a.summary.clone(),
            Proposal::Cal("x".into()),
        );
        let other_analyzer = Recommendation::new(
            "entity:caller/alice",
            Analyzer::new("loop.staleness/1"),
            a.summary.clone(),
            Proposal::Cal("x".into()),
        );
        let other_kind = Recommendation::new(
            "entity:caller/alice",
            Analyzer::new("loop.duplicate_sweep/1"),
            a.summary.clone(),
            Proposal::Data(serde_json::json!({})),
        );
        assert_ne!(a.dedup_key, other_target.dedup_key);
        assert_ne!(a.dedup_key, other_analyzer.dedup_key);
        assert_ne!(a.dedup_key, other_kind.dedup_key);
    }

    #[test]
    fn dedup_key_case_folds_the_family_but_not_the_target() {
        let lower = compute_dedup_key("loop.dup", "grain:sha256:AABB", "cal");
        let upper = compute_dedup_key("LOOP.DUP", "grain:sha256:AABB", "cal");
        assert_eq!(lower, upper, "analyzer family is case-folded");

        let other_case_target = compute_dedup_key("loop.dup", "grain:sha256:aabb", "cal");
        assert_ne!(
            lower, other_case_target,
            "a target ref may be case-sensitive and must not be folded"
        );
    }

    #[test]
    fn dedup_key_is_64_hex_chars() {
        let k = rec().dedup_key;
        assert_eq!(k.len(), 64);
        assert!(k.chars().all(|c| c.is_ascii_hexdigit() && !c.is_uppercase()));
    }

    #[test]
    fn nul_separation_prevents_component_smuggling() {
        // Without NUL separators, ("ab", "c") and ("a", "bc") would collide.
        assert_ne!(
            compute_dedup_key("ab", "c", "cal"),
            compute_dedup_key("a", "bc", "cal")
        );
    }

    #[test]
    fn analyzer_family_strips_only_the_major_suffix() {
        assert_eq!(Analyzer::new("loop.dup/1").family(), "loop.dup");
        assert_eq!(Analyzer::new("loop.dup").family(), "loop.dup");
        assert_eq!(Analyzer::new("a/b/2").family(), "a/b");
    }

    #[test]
    fn lifecycle_transitions_follow_the_state_machine() {
        use RecStatus::*;
        assert!(Pending.can_transition_to(Approved));
        assert!(Pending.can_transition_to(Rejected));
        assert!(Approved.can_transition_to(Applied));
        assert!(Applied.can_transition_to(RolledBack));
        // Re-enter review on refreshed evidence.
        assert!(Rejected.can_transition_to(Pending));
        // Terminal states go nowhere.
        assert!(!Applied.can_transition_to(Approved));
        assert!(!RolledBack.can_transition_to(Pending));
        assert!(Applied.is_terminal() && RolledBack.is_terminal());
        // Expiry is reachable only from pending or approved.
        assert!(Pending.can_transition_to(Expired));
        assert!(Approved.can_transition_to(Expired));
        assert!(!Applied.can_transition_to(Expired));
    }

    #[test]
    fn target_scheme_is_the_discriminator() {
        assert_eq!(rec().target_scheme(), Some("entity"));
        let r = Recommendation::new(
            "grain:sha256:abc",
            Analyzer::new("a/1"),
            Summary {
                template_id: "t".into(),
                args: serde_json::json!({}),
            },
            Proposal::Cal("x".into()),
        );
        assert_eq!(r.target_scheme(), Some("grain"));
    }

    #[test]
    fn text_searches_the_target_and_summary_args_not_a_template_id() {
        let r = Recommendation::new(
            "entity:caller/alice",
            Analyzer::new("a/1"),
            Summary {
                template_id: "dup.consolidate".into(),
                args: serde_json::json!({ "subject": "alice", "relation": "prefers" }),
            },
            Proposal::Cal("x".into()),
        );
        let t = r.text();
        assert!(t.contains("entity:caller/alice"));
        assert!(t.contains("alice") && t.contains("prefers"));
        assert!(!t.contains("dup.consolidate"), "template id is not prose");
    }
}
