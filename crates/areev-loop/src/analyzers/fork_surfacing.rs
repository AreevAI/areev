//! Fork surfacing (T0; requires the `forks` capability). Entities with more
//! than one live head, ranked, with a proposed merge. When the substrate does
//! not provide forks the analyzer yields nothing and the manifest's
//! `requires: [forks]` drives the activation-ladder message (§8) — never a
//! silent pretend-success.

use crate::analyzer::{AnalyzeCtx, Analyzer};
use crate::analyzers::bound_evidence;
use crate::cal;
use crate::error::Result;
use crate::manifest::*;
use crate::model::{ActionKind, Severity};
use crate::recommendation::{Proposal, RecDraft, Summary};
use serde_json::{json, Map};

pub struct ForkSurfacing {
    manifest: AnalyzerManifest,
}

impl ForkSurfacing {
    pub fn new() -> Self {
        ForkSurfacing {
            manifest: AnalyzerManifest {
                id: "loop.fork_surfacing/1".into(),
                title: "Fork surfacing".into(),
                description: "Surfaces entities with multiple live heads and proposes a merge."
                    .into(),
                tier: Tier::T0,
                cadence: CadenceClass::Fast,
                requires: vec![Capability::Forks],
                target_classes: vec![TargetClass::Memory],
                // Merging heads supersedes the non-primary head(s), which can
                // drop their distinct content — a lossy choice a human must
                // make. Never auto-apply, regardless of policy.
                auto_apply: AutoApplyClass::Never,
                trust_class: TrustClass::Builtin,
                params: vec![],
                default_on: true,
            },
        }
    }
}

impl Default for ForkSurfacing {
    fn default() -> Self {
        Self::new()
    }
}

impl Analyzer for ForkSurfacing {
    fn manifest(&self) -> &AnalyzerManifest {
        &self.manifest
    }

    fn analyze(&self, ctx: &AnalyzeCtx) -> Result<Vec<RecDraft>> {
        // Degrade cleanly when the substrate can't track forks.
        if !ctx.capabilities().forks {
            return Ok(vec![]);
        }
        let mut groups = ctx.heads()?;
        groups.sort_by(|a, b| a.entity.cmp(&b.entity));

        let mut drafts = Vec::new();
        for group in groups {
            if group.heads.len() < 2 {
                continue;
            }
            let mut heads = group.heads.clone();
            heads.sort(); // deterministic primary = smallest hash
            let primary = heads[0].clone();

            // Merge: supersede the secondary heads into the primary.
            let mut statements = Vec::new();
            for secondary in &heads[1..] {
                let mut fields = Map::new();
                fields.insert("merge_into".into(), json!(primary));
                statements.push(cal::supersede(secondary, "state", &fields));
            }
            let evidence = bound_evidence(heads.clone());

            let mut args = Map::new();
            args.insert("entity".into(), json!(group.entity));
            args.insert("count".into(), json!(heads.len()));

            drafts.push(
                RecDraft::new(
                    format!("entity:{}", group.entity),
                    ActionKind::MergeHeads,
                    Summary::new("fork.multi_head", args),
                    Proposal::Cal {
                        cal: cal::batch(&statements),
                    },
                )
                .severity(Severity::Medium)
                .evidence(evidence),
            );
        }
        Ok(drafts)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit::TestSubstrate;

    #[test]
    fn surfaces_multi_head_entity() {
        let mut sub = TestSubstrate::new();
        sub.add_fork("caller/john", &["ref-a", "ref-b"]);
        let drafts = sub.analyze(&ForkSurfacing::new(), 10_000);
        assert_eq!(drafts.len(), 1);
        assert_eq!(drafts[0].action_kind, ActionKind::MergeHeads);
        assert_eq!(drafts[0].evidence.len(), 2);
    }

    #[test]
    fn degrades_when_forks_unavailable() {
        let sub = TestSubstrate::new(); // forks capability off by default
        assert!(sub.analyze(&ForkSurfacing::new(), 10_000).is_empty());
    }
}
