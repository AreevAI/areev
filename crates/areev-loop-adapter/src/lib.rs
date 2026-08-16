//! # areev-loop-adapter
//!
//! The Areev substrate adapter for the [`areev_loop`] engine. It implements
//! [`areev_loop::OmsSubstrate`] over [`areev_cal::AreevFacade`], so the governed
//! self-improvement loop runs against real Areev `.mg`/Turso memory files.
//!
//! ```no_run
//! use areev_loop_adapter::{AreevSubstrate, now_ms};
//! use areev_store::Areev;
//! use areev_loop::{Engine, RunOptions};
//!
//! let store = Areev::open("agent.db").unwrap();
//! let mut sub = AreevSubstrate::new(store, None);
//! let engine = Engine::with_builtins();
//! let result = engine.run(&mut sub, &RunOptions::default(), now_ms()).unwrap();
//! println!("proposed {} recommendation(s)", result.stored);
//! ```

mod governance;
mod substrate;

pub use governance::LoopGovernance;
pub use substrate::{BorrowedSubstrate, AreevSubstrate};

use std::time::{SystemTime, UNIX_EPOCH};

/// Map a session's grants onto the loop engine's scope set — the one
/// translation between Areev's verb model (`areev_core::authz`) and
/// `areev_loop::Scope`, so surfaces stop handing out `ScopeSet::all()`
/// unconditionally. Loop verbs are checked against the loop's own namespace
/// (`areev_loop::LOOP_NS`), which a grant covers by naming it or `*`.
/// An owner session maps to every scope — the CLI's local-root-of-trust
/// behavior, unchanged.
pub fn scopes_for(authz: &areev_core::authz::AuthzSet) -> areev_loop::ScopeSet {
    use areev_loop::Scope;
    use areev_loop::LOOP_NS;
    use areev_core::authz::Verb;
    let mut scopes = Vec::new();
    for (verb, scope) in [
        (Verb::Read, Scope::Read),
        (Verb::Write, Scope::Write),
        (Verb::LoopReview, Scope::Review),
        (Verb::LoopApply, Scope::Apply),
        (Verb::Admin, Scope::Admin),
    ] {
        if authz.allows(verb, LOOP_NS) {
            scopes.push(scope);
        }
    }
    areev_loop::ScopeSet::of(&scopes)
}

/// The observer type an actor label implies, used where no credential record
/// declares one (the credential map will carry an explicit `observer` field
/// when the multi-token surfaces land; this prefix heuristic is the interim
/// derivation — a *statement* must never be able to claim humanity, so the
/// answer always comes from the host-held actor label, not request text).
pub fn observer_for_principal(actor: &str) -> areev_loop::ObserverType {
    match areev_core::authz::observer_kind(actor) {
        "agent" => areev_loop::ObserverType::Agent,
        _ => areev_loop::ObserverType::Human,
    }
}

/// Wall-clock now in epoch milliseconds — the `now_ms` the engine's `run`,
/// `review`, `apply`, and `rollback` take. Kept out of `areev-loop` itself so the
/// engine stays deterministic (the caller supplies the clock).
///
/// `AREEV_LOOP_NOW_MS` (epoch ms) overrides the wall clock — the simulation seam
/// that makes a run through the real binary a pure function of (file, policy,
/// time). The golden E2E suite uses it to pin analyzer output and to step time
/// across outcome-review horizons and rejection cooldowns without sleeping.
/// A set-but-unparseable value panics: the caller asked for simulated time,
/// and silently running at wall time instead would defeat the point.
pub fn now_ms() -> i64 {
    if let Ok(v) = std::env::var("AREEV_LOOP_NOW_MS") {
        return v
            .trim()
            .parse()
            .unwrap_or_else(|_| panic!("AREEV_LOOP_NOW_MS is set but not epoch milliseconds: {v:?}"));
    }
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod authz_mapping_tests {
    use super::*;
    use areev_core::authz::{AuthzSet, Grant, Verb};

    #[test]
    fn owner_maps_to_every_scope() {
        let scopes = scopes_for(&AuthzSet::owner("user:local"));
        for s in [
            areev_loop::Scope::Read,
            areev_loop::Scope::Write,
            areev_loop::Scope::Review,
            areev_loop::Scope::Apply,
            areev_loop::Scope::Admin,
        ] {
            assert!(scopes.has(s), "{s:?} missing for owner");
        }
    }

    #[test]
    fn loop_verbs_map_one_to_one_and_namespace_scoping_holds() {
        // A reviewer granted on `*` gets exactly Review (plus nothing else).
        let reviewer = AuthzSet::restricted(
            "user:rev",
            vec![Grant { verbs: vec![Verb::LoopReview], namespaces: vec!["*".into()] }],
        );
        let scopes = scopes_for(&reviewer);
        assert!(scopes.has(areev_loop::Scope::Review));
        for s in [
            areev_loop::Scope::Read,
            areev_loop::Scope::Write,
            areev_loop::Scope::Apply,
            areev_loop::Scope::Admin,
        ] {
            assert!(!scopes.has(s), "{s:?} must not be granted");
        }

        // Loop verbs are checked against the loop's own namespace — a grant
        // scoped to some data namespace does not reach the review queue.
        let elsewhere = AuthzSet::restricted(
            "user:misscoped",
            vec![Grant { verbs: vec![Verb::LoopReview], namespaces: vec!["caller".into()] }],
        );
        assert!(!scopes_for(&elsewhere).has(areev_loop::Scope::Review));
        let on_loop_ns = AuthzSet::restricted(
            "user:scoped",
            vec![Grant {
                verbs: vec![Verb::LoopReview],
                namespaces: vec![areev_loop::LOOP_NS.into()],
            }],
        );
        assert!(scopes_for(&on_loop_ns).has(areev_loop::Scope::Review));
    }

    #[test]
    fn observer_derives_from_the_actor_label() {
        use areev_loop::ObserverType;
        assert_eq!(observer_for_principal("agent:mcp"), ObserverType::Agent);
        assert_eq!(observer_for_principal("bot:sweeper"), ObserverType::Agent);
        assert_eq!(observer_for_principal("job:retention"), ObserverType::Agent);
        assert_eq!(observer_for_principal("engine:loop.llm/1"), ObserverType::Agent);
        assert_eq!(observer_for_principal("user:anna"), ObserverType::Human);
        assert_eq!(observer_for_principal("anna"), ObserverType::Human);
    }
}
