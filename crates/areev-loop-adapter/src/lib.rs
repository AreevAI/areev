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
pub use substrate::{loop_state_of, AreevSubstrate, BorrowedSubstrate};

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

/// The scopes a session holds over ONE recommendation, given the namespaces
/// it was derived from (#312).
///
/// The loop's reads are namespace-grant-gated; its outputs were not — every
/// recommendation went to one namespace, `areev-loop`, and rights were
/// checked against that one namespace. So one `read ON areev-loop` grant
/// disclosed the summary, proposal, guidance and evidence hashes of findings
/// derived from every namespace in the memory, and one `loop.review` grant
/// decided all of them, striking a memory-wide cooldown each time.
///
/// The rule: a principal covers a recommendation when its grants allow the
/// verb on EVERY namespace in the recommendation's scope.
///
/// Two deliberate escape hatches, both of which keep existing deployments
/// working unchanged:
///
/// * A grant on `areev-loop` itself (or `*`) means the WHOLE queue, so owner
///   sessions and today's operator grants behave exactly as before.
/// * An EMPTY scope — an unscoped pass, and every recommendation written
///   before this existed — is covered only by such a whole-queue grant. Fail
///   closed: "derived from we-don't-know-where" must not be readable by
///   someone holding one namespace.
pub fn scopes_for_rec(
    authz: &areev_core::authz::AuthzSet,
    scope: &[String],
) -> areev_loop::ScopeSet {
    use areev_core::authz::Verb;
    use areev_loop::Scope;
    use areev_loop::LOOP_NS;
    // The whole-queue grant short-circuits: unchanged behaviour.
    let whole_queue = scopes_for(authz);
    if scope.is_empty() {
        return whole_queue;
    }
    let mut scopes = Vec::new();
    for (verb, s) in [
        (Verb::Read, Scope::Read),
        (Verb::Write, Scope::Write),
        (Verb::LoopReview, Scope::Review),
        (Verb::LoopApply, Scope::Apply),
        (Verb::Admin, Scope::Admin),
    ] {
        // Either the whole queue, or coverage of every namespace the
        // finding was derived from. Never a partial read: a finding derived
        // from `a` and `b` discloses both, so holding `a` alone is not
        // enough.
        if authz.allows(verb, LOOP_NS) || scope.iter().all(|ns| authz.allows(verb, ns)) {
            scopes.push(s);
        }
    }
    areev_loop::ScopeSet::of(&scopes)
}

/// Whether this session may SEE a recommendation with this scope (#312).
///
/// A recommendation the caller does not cover answers "not found" rather
/// than "not authorized", so its existence is not disclosed — the same
/// reasoning `recall` applies when it declines to name a sibling namespace
/// in a refusal.
pub fn covers_rec(authz: &areev_core::authz::AuthzSet, scope: &[String]) -> bool {
    scopes_for_rec(authz, scope).has(areev_loop::Scope::Read)
}

/// Every recommendation a session may SEE, filtered by coverage (#312).
///
/// ONE filtered read that every surface goes through — the CLI, the server,
/// MCP, both bindings and `DESCRIBE LOOP` — so a surface added later cannot
/// forget the check by calling `Engine::recommendations` directly and
/// listing the whole namespace.
pub fn visible_recommendations<S: areev_loop::OmsSubstrate>(
    engine: &areev_loop::Engine,
    sub: &S,
    authz: &areev_core::authz::AuthzSet,
    status: Option<areev_loop::RecStatus>,
) -> areev_loop::Result<Vec<areev_loop::Recommendation>> {
    let all = engine.recommendations(sub, status)?;
    Ok(all.into_iter().filter(|r| covers_rec(authz, &r.scope)).collect())
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

    fn granted(principal: &str, verbs: Vec<Verb>, ns: &[&str]) -> AuthzSet {
        AuthzSet::restricted(
            principal,
            vec![Grant {
                verbs,
                namespaces: ns.iter().map(|s| (*s).to_string()).collect(),
            }],
        )
    }

    // ---- #312: coverage over a recommendation's own scope ----------------

    #[test]
    fn a_namespace_grant_covers_a_finding_derived_from_that_namespace() {
        // The new capability: a reviewer granted on the namespace they work
        // in can read and decide findings derived from it — without a grant
        // on `areev-loop`, which would hand them the whole queue.
        let amy = granted("user:amy", vec![Verb::Read, Verb::LoopReview], &["a"]);
        let s = scopes_for_rec(&amy, &["a".into()]);
        assert!(s.has(areev_loop::Scope::Read));
        assert!(s.has(areev_loop::Scope::Review));
        assert!(covers_rec(&amy, &["a".into()]));
    }

    #[test]
    fn a_namespace_grant_does_not_cover_another_namespaces_finding() {
        let amy = granted("user:amy", vec![Verb::Read, Verb::LoopReview], &["a"]);
        assert!(!covers_rec(&amy, &["b".into()]));
        let s = scopes_for_rec(&amy, &["b".into()]);
        assert!(!s.has(areev_loop::Scope::Read));
        assert!(!s.has(areev_loop::Scope::Review));
    }

    #[test]
    fn coverage_needs_every_namespace_a_finding_was_derived_from() {
        // A finding derived from `a` and `b` discloses both, so holding `a`
        // alone is not enough — it is not a partial read.
        let amy = granted("user:amy", vec![Verb::Read], &["a"]);
        assert!(!covers_rec(&amy, &["a".into(), "b".into()]));
        let both = granted("user:both", vec![Verb::Read], &["a", "b"]);
        assert!(covers_rec(&both, &["a".into(), "b".into()]));
    }

    #[test]
    fn a_whole_queue_grant_still_means_the_whole_queue() {
        // Owner sessions and existing operator grants are unchanged.
        let op = granted(
            "user:op",
            vec![Verb::Read, Verb::LoopReview],
            &[areev_loop::LOOP_NS],
        );
        assert!(covers_rec(&op, &["a".into()]));
        assert!(covers_rec(&op, &["a".into(), "b".into()]));
        assert!(scopes_for_rec(&op, &["zzz".into()]).has(areev_loop::Scope::Review));
        assert!(covers_rec(&AuthzSet::owner("user:local"), &["a".into()]));
    }

    #[test]
    fn an_unscoped_finding_needs_a_whole_queue_grant() {
        // An unscoped pass — and every recommendation written before the
        // scope existed — is "derived from we-don't-know-where". Fail closed.
        let amy = granted("user:amy", vec![Verb::Read, Verb::LoopReview], &["a"]);
        assert!(!covers_rec(&amy, &[]));
        let op = granted("user:op", vec![Verb::Read], &[areev_loop::LOOP_NS]);
        assert!(covers_rec(&op, &[]));
        assert!(covers_rec(&AuthzSet::owner("user:local"), &[]));
    }

    #[test]
    fn coverage_is_per_verb() {
        // Read but not review: they can see it, and cannot decide it.
        let amy = granted("user:amy", vec![Verb::Read], &["a"]);
        let s = scopes_for_rec(&amy, &["a".into()]);
        assert!(s.has(areev_loop::Scope::Read));
        assert!(!s.has(areev_loop::Scope::Review));
        assert!(!s.has(areev_loop::Scope::Apply));
    }

    #[test]
    fn scope_normalization_is_order_and_duplicate_independent() {
        use areev_loop::normalize_scope;
        assert_eq!(
            normalize_scope(&["b".into(), "a".into(), "a".into(), "  ".into()]),
            vec!["a".to_string(), "b".to_string()]
        );
    }

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
