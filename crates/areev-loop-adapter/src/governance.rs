//! The real [`GovernanceHost`] — CAL's loop lifecycle statements executed
//! through the Areev Loop engine (CAL 1.3 §8.16).
//!
//! This crate knows both sides of the seam: `areev-cal` (the facade the
//! executor runs against) and `areev-loop` (the engine). Every call receives
//! the executor's own facade — never a second store handle, which the
//! single-writer registry would refuse — and derives the session identity
//! from it: actor = the bound principal, scopes = [`crate::scopes_for`],
//! observer = [`crate::observer_for_principal`]. A statement cannot claim
//! to be someone; the four gates run in the engine exactly as on every
//! other surface, including the co-creator self-approval block (the
//! session principal rides `RunOptions::triggering_actor`).

use areev_cal::governance::{GovernanceHost, LoopInfo, ReviewDecision, RunLoopOptions};
use areev_cal::{CalStoreFacade, AreevFacade};
use areev_core::error::{AreevError, Result};
use areev_loop::{Decision, Engine, Policy, RunOptions};

/// Governance over `Engine::with_builtins()` plus an optional host policy.
/// LLM backends deliberately have no seam here: `RUN LOOP` from CAL is the
/// deterministic pass — model-attached runs stay on the host surfaces where
/// credentials live.
pub struct LoopGovernance {
    policy: Option<Policy>,
}

impl LoopGovernance {
    pub fn new() -> Self {
        LoopGovernance { policy: None }
    }

    /// Attach the same `loop-policy.json` the CLI takes (`--policy`) — a
    /// CAL-triggered run honors the same auto-apply grants, denies, and
    /// severity floors.
    pub fn with_policy(policy: Policy) -> Self {
        LoopGovernance { policy: Some(policy) }
    }

    fn engine(&self) -> Engine {
        match &self.policy {
            Some(p) => Engine::with_builtins().with_policy(p.clone()),
            None => Engine::with_builtins(),
        }
    }

    /// The concrete facade back from the trait object. Only the real
    /// Areev facade can host governance — a mock or foreign store answers
    /// `None` from `as_any` and gets a clean refusal.
    fn facade<'a>(&self, store: &'a dyn CalStoreFacade) -> Result<&'a AreevFacade> {
        store
            .as_any()
            .and_then(|a| a.downcast_ref::<AreevFacade>())
            .ok_or_else(|| {
                AreevError::Internal(
                    "governance requires the Areev facade — this store cannot host the loop"
                        .into(),
                )
            })
    }

    fn wrap(e: areev_loop::Error) -> AreevError {
        // The engine's LOP-Ennn code is the leading token of its Display —
        // keep it visible through the wrap.
        AreevError::Validation(e.to_string())
    }
}

impl Default for LoopGovernance {
    fn default() -> Self {
        Self::new()
    }
}

impl GovernanceHost for LoopGovernance {
    fn run_loop(
        &self,
        store: &dyn CalStoreFacade,
        opts: &RunLoopOptions,
    ) -> Result<serde_json::Value> {
        let facade = self.facade(store)?;
        // A run analyzes the whole memory and writes recommendation, audit,
        // and loop-state grains, so it is gated exactly as `POST
        // /api/loop/run` is: `loop.run` on the loop namespace (owner
        // passes). Without this the CAL surface is a way around the HTTP
        // gate — a read-only principal refused at the route could still
        // write grains via `RUN LOOP`.
        facade
            .authz()
            .check(areev_core::authz::Verb::LoopRun, areev_loop::LOOP_NS)?;
        let mut sub = crate::BorrowedSubstrate::new(facade);
        let run_opts = RunOptions {
            min_new: opts.min_new,
            if_stale_ms: opts.if_stale_ms,
            full_sweep: opts.full_sweep,
            // The session principal is the trigger — the self-approval
            // block extends to whatever this run's external analyzers
            // author (Builtin findings stay engine-created).
            triggering_actor: Some(facade.authz().principal().to_string()),
            ..RunOptions::default()
        };
        let res = self
            .engine()
            .run(&mut sub, &run_opts, crate::now_ms())
            .map_err(Self::wrap)?;
        serde_json::to_value(&res).map_err(|e| AreevError::Internal(e.to_string()))
    }

    fn review(
        &self,
        store: &dyn CalStoreFacade,
        rec_hash: &str,
        decision: ReviewDecision,
        because: &str,
    ) -> Result<()> {
        let facade = self.facade(store)?;
        let actor = facade.authz().principal().to_string();
        let scopes = crate::scopes_for(&facade.authz());
        let observer = crate::observer_for_principal(&actor);
        let mut sub = crate::BorrowedSubstrate::new(facade);
        let d = match decision {
            ReviewDecision::Approve => Decision::Approve,
            ReviewDecision::Reject => Decision::Reject,
        };
        self.engine()
            .review(&mut sub, rec_hash, d, &actor, observer, &scopes, because, crate::now_ms())
            .map_err(Self::wrap)
    }

    fn apply(
        &self,
        store: &dyn CalStoreFacade,
        rec_hash: &str,
        because: &str,
        destructive_cap: bool,
    ) -> Result<bool> {
        let facade = self.facade(store)?;
        let authz = facade.authz();
        let actor = authz.principal().to_string();
        let scopes = crate::scopes_for(&authz);
        let observer = crate::observer_for_principal(&actor);
        // The two-key rule, verb-shaped: a destructive apply needs
        // `loop.apply` (in scopes) AND the session's own destruction verbs —
        // and, above both, the process-wide cap, which is restrictive over
        // any grant. A `--no-destructive-ops` session must not be able to
        // route a FORGET through a recommendation.
        let allow_destructive = destructive_cap
            && (authz.allows(areev_core::authz::Verb::Delete, "*")
                || authz.allows(areev_core::authz::Verb::Erase, "*"));
        let mut sub = crate::BorrowedSubstrate::new(facade);
        let applied = self
            .engine()
            .apply(
                &mut sub,
                rec_hash,
                &actor,
                observer,
                &scopes,
                because,
                allow_destructive,
                crate::now_ms(),
            )
            .map_err(Self::wrap)?;
        Ok(applied.rollbackable)
    }

    fn rollback(&self, store: &dyn CalStoreFacade, rec_hash: &str, because: &str) -> Result<()> {
        let facade = self.facade(store)?;
        let actor = facade.authz().principal().to_string();
        let scopes = crate::scopes_for(&facade.authz());
        let observer = crate::observer_for_principal(&actor);
        let mut sub = crate::BorrowedSubstrate::new(facade);
        self.engine()
            .rollback(&mut sub, rec_hash, &actor, observer, &scopes, because, crate::now_ms())
            .map_err(Self::wrap)
    }

    fn describe(&self, store: &dyn CalStoreFacade, what: LoopInfo) -> Result<serde_json::Value> {
        let facade = self.facade(store)?;
        // The loop reads live in the loop's namespace — the same read gate
        // SHOW GRANTS uses for authz: read on areev-loop (owner passes).
        facade
            .authz()
            .check(areev_core::authz::Verb::Read, areev_loop::LOOP_NS)?;
        let sub = crate::BorrowedSubstrate::new(facade);
        let engine = self.engine();
        let v = match what {
            LoopInfo::Loop => {
                // Health plus the pending queue itself — `DESCRIBE LOOP` is
                // the in-language `areev loop list`, so the hashes a
                // reviewer needs for APPROVE/APPLY are right here.
                let health = engine.health(&sub, crate::now_ms()).map_err(Self::wrap)?;
                let pending = engine
                    .recommendations(&sub, Some(areev_loop::RecStatus::Pending))
                    .map_err(Self::wrap)?;
                serde_json::to_value(health).map(|mut v| {
                    v["pending_recommendations"] = serde_json::Value::Array(
                        pending
                            .iter()
                            .map(|r| {
                                serde_json::json!({
                                    "hash": r.hash,
                                    "analyzer": r.analyzer,
                                    "severity": format!("{:?}", r.severity).to_lowercase(),
                                    "summary": r.summary.render(),
                                })
                            })
                            .collect(),
                    );
                    v
                })
            }
            LoopInfo::Analyzers => Ok(serde_json::Value::Array(
                engine
                    .analyzers()
                    .iter()
                    .map(|a| {
                        let m = a.manifest();
                        serde_json::json!({
                            "id": m.id,
                            "family": m.family(),
                            "default_on": m.default_on,
                        })
                    })
                    .collect(),
            )),
            LoopInfo::Outcomes => {
                serde_json::to_value(engine.outcomes(&sub).map_err(Self::wrap)?)
            }
            LoopInfo::Policy => Ok(match &self.policy {
                None => serde_json::json!({
                    "attached": false,
                    "note": "no host policy attached — the closed default: nothing \
                             auto-applies, nothing is denied",
                }),
                Some(p) => serde_json::json!({
                    "attached": true,
                    "auto_apply_enabled": p.auto_apply_enabled,
                    "denied_families": p.deny,
                }),
            }),
        };
        v.map_err(|e| AreevError::Internal(e.to_string()))
    }
}
