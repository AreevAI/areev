//! Retention sweep (T0): grains older than a declared maximum age.
//!
//! The *governed* half of storage limitation (GDPR Art. 5(1)(e)). The host
//! can enforce retention directly on a cron (`areev retention sweep`); this
//! analyzer instead routes the same deletion through the review queue, so
//! retention inherits BECAUSE, separation of duties, and the audit chain —
//! the product's own governance story applied to compliance.
//!
//! **Why single-grain FORGETs rather than one PURGE.** A `PURGE OLDER THAN`
//! line has no audited execution path through the substrate (it is refused
//! there by design — bulk erasure from a proposal is exactly the shape the
//! destructive gate exists to stop). Emitting one FORGET per grain means the
//! reviewer sees precisely what disappears, every removal gets its own
//! Tier-2 audit record, and the batch is stamped destructive so it needs
//! admin + `allow_destructive` to apply.
//!
//! Off by default and never auto-appliable: a deletion policy is a decision
//! the deployment makes, not one an analyzer infers.

use crate::analyzer::{AnalyzeCtx, Analyzer};
use crate::cal;
use crate::error::Result;
use crate::manifest::*;
use crate::model::{ActionKind, Severity};
use crate::recommendation::{Proposal, RecDraft, Summary};
use serde_json::{json, Map};

/// Upper bound on grains named by one proposal, so a first run over a large
/// memory produces a reviewable unit rather than a 50k-line batch. Anything
/// beyond it is reported in the summary — never silently dropped — and the
/// next run picks it up.
const DEFAULT_MAX_GRAINS: i64 = 500;

/// Namespaces a retention proposal must never name: the loop's own state and
/// the OMS `agent:*` reserved space (`agent:authz` grants + audit records,
/// `agent:identity`, …). Substrate-agnostic — the engine cannot know which
/// concrete namespaces a host reserves, but these two shapes are spec-level.
fn is_reserved_ns(ns: &str) -> bool {
    ns == crate::LOOP_NS || ns.starts_with("agent:")
}

pub struct RetentionSweep {
    manifest: AnalyzerManifest,
}

impl RetentionSweep {
    pub fn new() -> Self {
        RetentionSweep {
            manifest: AnalyzerManifest {
                id: "loop.retention_sweep/1".into(),
                title: "Retention sweep".into(),
                description: "Proposes tombstoning grains past a declared retention age \
                              (storage limitation), through the review queue."
                    .into(),
                tier: Tier::T0,
                cadence: CadenceClass::Slow,
                requires: vec![],
                target_classes: vec![TargetClass::Memory],
                // Deletion never auto-applies, whatever the policy grants.
                auto_apply: AutoApplyClass::Never,
                trust_class: TrustClass::Builtin,
                params: vec![
                    ParamSpec::Int {
                        name: "max_age_days".into(),
                        // 0 = no policy declared → the analyzer proposes
                        // nothing. Retention must be stated, never inferred.
                        default: 0,
                        min: 0,
                        max: 36_500,
                        description: "Erase grains older than this many days. 0 disables \
                                      the analyzer (no retention policy declared)."
                            .into(),
                    },
                    ParamSpec::Str {
                        name: "grain_type".into(),
                        default: String::new(),
                        max_len: 32,
                        description: "Restrict the sweep to one grain type (e.g. \"event\"). \
                                      Empty sweeps every type."
                            .into(),
                    },
                    ParamSpec::Int {
                        name: "max_grains".into(),
                        default: DEFAULT_MAX_GRAINS,
                        min: 1,
                        max: 5_000,
                        description: "Maximum grains named by one proposal.".into(),
                    },
                ],
                default_on: false,
            },
        }
    }
}

impl Default for RetentionSweep {
    fn default() -> Self {
        Self::new()
    }
}

impl Analyzer for RetentionSweep {
    fn manifest(&self) -> &AnalyzerManifest {
        &self.manifest
    }

    fn analyze(&self, ctx: &AnalyzeCtx) -> Result<Vec<RecDraft>> {
        let max_age_days = ctx.params().get_int("max_age_days");
        if max_age_days <= 0 {
            return Ok(Vec::new()); // no policy declared
        }
        let cutoff = ctx.now_ms() - max_age_days * 86_400_000;
        let want_type = ctx.params().get_str("grain_type").trim().to_string();
        let max_grains = ctx.params().get_int("max_grains").max(1) as usize;

        // Facts and observations are the retention-bearing types the reader
        // exposes; an explicit grain_type narrows to one of them.
        let mut grains = Vec::new();
        if want_type.is_empty() || want_type == crate::model::grain_type::FACT {
            grains.extend(ctx.facts()?);
        }
        if want_type.is_empty() || want_type == crate::model::grain_type::OBSERVATION {
            grains.extend(ctx.observations()?);
        }

        // Group by namespace so each proposal is one reviewable policy unit.
        let mut by_ns: std::collections::BTreeMap<String, Vec<&crate::model::GrainRecord>> =
            std::collections::BTreeMap::new();
        for g in &grains {
            // Reserved namespaces are the memory's own governance state —
            // grants, audit records, identity — not user data under a
            // retention policy. Tombstoning them locks principals out of the
            // file and destroys the accountability record. A substrate
            // SHOULD already withhold them; this is the second lock, because
            // an analyzer that can propose erasing the audit trail is the
            // one place a reader bug becomes unrecoverable.
            if is_reserved_ns(&g.namespace) {
                continue;
            }
            // created_at_ms 0 means "unknown" on this reader — never treat a
            // missing timestamp as infinitely old.
            if g.created_at_ms > 0 && g.created_at_ms < cutoff {
                by_ns.entry(g.namespace.clone()).or_default().push(g);
            }
        }

        let mut drafts = Vec::new();
        for (ns, mut over_age) in by_ns {
            // Oldest first, so a capped batch removes the most overdue.
            over_age.sort_by_key(|g| (g.created_at_ms, g.hash.clone()));
            let total = over_age.len();
            let named = over_age.iter().take(max_grains).collect::<Vec<_>>();
            let lines: Vec<String> = named.iter().map(|g| cal::forget(&g.hash)).collect();
            let oldest_days = named
                .first()
                .map(|g| (ctx.now_ms() - g.created_at_ms) / 86_400_000)
                .unwrap_or(0);

            let mut args = Map::new();
            args.insert("namespace".into(), json!(ns));
            args.insert("count".into(), json!(named.len()));
            args.insert("max_age_days".into(), json!(max_age_days));
            args.insert("oldest_days".into(), json!(oldest_days));
            // Truncation is stated, never silent: the reviewer must be able
            // to tell "this is all of it" from "this is the first 500".
            args.insert("remaining".into(), json!(total.saturating_sub(named.len())));

            drafts.push(
                RecDraft::new(
                    format!("host:retention/{ns}"),
                    ActionKind::Expire,
                    Summary::new("retention.overdue", args),
                    Proposal::Cal { cal: cal::batch(&lines) },
                )
                .severity(Severity::Low)
                .evidence(named.iter().map(|g| g.hash.clone()).collect()),
            );
        }
        Ok(drafts)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit::TestSubstrate;

    const DAY: i64 = 86_400_000;

    #[test]
    fn proposes_only_grains_past_the_declared_age() {
        let mut sub = TestSubstrate::new();
        sub.add_fact_at("caller", "old", "note", "ancient", DAY);
        sub.add_fact_at("caller", "recent", "note", "fresh", 95 * DAY);
        let now = 100 * DAY;

        // No policy declared → nothing proposed, whatever the data says.
        assert!(sub.analyze(&RetentionSweep::new(), now).is_empty());

        let drafts = sub.analyze_with(
            &RetentionSweep::new(),
            now,
            &[("max_age_days", serde_json::json!(30))],
        );
        assert_eq!(drafts.len(), 1, "one proposal per namespace");
        let Proposal::Cal { cal } = &drafts[0].proposal else { panic!("expected CAL") };
        assert_eq!(cal.lines().count(), 1, "only the over-age grain: {cal}");
        assert!(cal.starts_with("FORGET "), "single-grain tombstones: {cal}");
        assert_eq!(drafts[0].evidence.len(), 1);
    }

    #[test]
    fn caps_the_batch_and_says_how_many_remain() {
        let mut sub = TestSubstrate::new();
        for i in 0..5 {
            sub.add_fact_at("caller", &format!("s{i}"), "note", "old", DAY + i);
        }
        let drafts = sub.analyze_with(
            &RetentionSweep::new(),
            100 * DAY,
            &[
                ("max_age_days", serde_json::json!(30)),
                ("max_grains", serde_json::json!(2)),
            ],
        );
        assert_eq!(drafts.len(), 1);
        let Proposal::Cal { cal } = &drafts[0].proposal else { panic!("expected CAL") };
        assert_eq!(cal.lines().count(), 2, "batch is capped");
        let rendered = drafts[0].summary.render();
        assert!(rendered.contains('3'), "the remainder is stated: {rendered}");
    }

    #[test]
    fn never_proposes_erasing_governance_state() {
        let mut sub = TestSubstrate::new();
        sub.add_fact_at("agent:authz", "user:bot", "mg:permits", "read ON *", DAY);
        sub.add_fact_at("agent:harness", "run:r1", "mg:harness", "config", DAY);
        sub.add_fact_at("caller", "old", "note", "ancient", DAY);
        let drafts = sub.analyze_with(
            &RetentionSweep::new(),
            100 * DAY,
            &[("max_age_days", serde_json::json!(30))],
        );
        assert_eq!(drafts.len(), 1, "only the user namespace: {drafts:?}");
        let Proposal::Cal { cal } = &drafts[0].proposal else { panic!("expected CAL") };
        assert_eq!(
            cal.lines().count(),
            1,
            "reserved grant and harness grains are not named: {cal}"
        );
    }

    #[test]
    fn unknown_creation_time_is_never_treated_as_ancient() {
        let mut sub = TestSubstrate::new();
        sub.add_fact_at("caller", "notime", "note", "x", 0);
        let drafts = sub.analyze_with(
            &RetentionSweep::new(),
            100 * DAY,
            &[("max_age_days", serde_json::json!(1))],
        );
        assert!(drafts.is_empty(), "a missing timestamp must not mean 'delete me'");
    }
}
