//! `PrincipalSession` — the rebind-race-free, attributed write path
//! (governed-agents §6.8). `bind_principal` swaps one process-wide slot and
//! is safe only for hosts that serialize requests; a runtime journaling as a
//! run's principal while an approver responds as theirs needs concurrent
//! per-principal rights. These tests pin: fail-closed resolution, per-write
//! authorization, author attribution on the grain itself, the shared slot
//! staying untouched, and the extended `record_tool_call` reaching typed
//! fields end to end through the facade.

use areev_cal::{AreevFacade, CalExecutor, CalExecutorConfig};
use areev_core::authz::{AUTHZ_NS, REL_PERMITS};
use areev_core::types::{Fact, Grain};
use areev_store::Areev;
use tempfile::TempDir;

fn facade_with_grants() -> (AreevFacade, TempDir) {
    let dir = TempDir::new().unwrap();
    let mut m = Areev::open(dir.path().join("m.db").to_str().unwrap()).unwrap();
    for (principal, object) in [
        ("user:amy", "read,write ON ops"),
        ("user:bob", "read,write ON ops"),
    ] {
        m.add(
            &Fact::new(principal, REL_PERMITS, object)
                .namespace(AUTHZ_NS)
                .created_at(1_000),
        )
        .unwrap();
    }
    (AreevFacade::new(m), dir)
}

fn fields(json: serde_json::Value) -> serde_json::Map<String, serde_json::Value> {
    json.as_object().unwrap().clone()
}

#[test]
fn session_writes_are_authorized_and_attributed() {
    let (facade, _d) = facade_with_grants();
    let amy = facade.principal_session("user:amy").unwrap();

    let h = amy
        .cal_add(
            "fact",
            &fields(serde_json::json!({
                "subject": "s", "relation": "r", "object": "o", "namespace": "ops"
            })),
        )
        .unwrap();
    // Attribution lives ON the grain, not in ambient state.
    let g = facade.with_store(|m| m.get(&h)).unwrap();
    assert_eq!(g.get_str("author_did"), Some("user:amy"));

    // Out-of-grant namespace: refused by THIS session's rights.
    let err = amy
        .cal_add(
            "fact",
            &fields(serde_json::json!({
                "subject": "s", "relation": "r", "object": "o", "namespace": "secret"
            })),
        )
        .unwrap_err();
    assert!(err.to_string().contains("secret"), "{err}");
}

#[test]
fn unknown_principal_fails_closed_on_every_write() {
    let (facade, _d) = facade_with_grants();
    let ghost = facade.principal_session("user:ghost").unwrap();
    assert!(ghost
        .cal_add(
            "fact",
            &fields(serde_json::json!({
                "subject": "s", "relation": "r", "object": "o", "namespace": "ops"
            })),
        )
        .is_err());
}

/// The reason this type exists: two principals write concurrently, each
/// attributed, and the facade's shared authz slot never changes — no rebind,
/// no race window.
#[test]
fn concurrent_sessions_do_not_touch_the_shared_slot() {
    let (facade, _d) = facade_with_grants();
    assert!(facade.authz().is_owner(), "precondition: owner default");

    std::thread::scope(|s| {
        for principal in ["user:amy", "user:bob"] {
            let facade = &facade;
            s.spawn(move || {
                let session = facade.principal_session(principal).unwrap();
                for i in 0..20 {
                    session
                        .cal_add(
                            "fact",
                            &fields(serde_json::json!({
                                "subject": format!("{principal}-{i}"),
                                "relation": "wrote",
                                "object": "x",
                                "namespace": "ops",
                            })),
                        )
                        .unwrap();
                }
            });
        }
    });

    assert!(
        facade.authz().is_owner(),
        "sessions must never rebind the shared slot"
    );
    // Every write is attributed to the session that made it.
    for principal in ["user:amy", "user:bob"] {
        for i in 0..20 {
            let res = facade
                .with_store(|m| m.recall("ops", &format!("{principal}-{i}"), None, 5))
                .unwrap();
            assert_eq!(res.len(), 1);
            assert_eq!(res[0].get_str("author_did"), Some(principal));
        }
    }
}

#[test]
fn explicit_author_did_is_preserved_not_overwritten() {
    let (facade, _d) = facade_with_grants();
    let amy = facade.principal_session("user:amy").unwrap();
    let h = amy
        .cal_add(
            "fact",
            &fields(serde_json::json!({
                "subject": "s", "relation": "r", "object": "o", "namespace": "ops",
                "author_did": "did:key:upstream"
            })),
        )
        .unwrap();
    let g = facade.with_store(|m| m.get(&h)).unwrap();
    assert_eq!(g.get_str("author_did"), Some("did:key:upstream"));
}

/// The extended record_tool_call, end to end through the session: run
/// correlation + step_action link + lifecycle enums land as typed, indexed
/// state; the half-pair and unknown-enum cases are refused.
#[test]
fn session_record_tool_call_journals_with_link_and_lifecycle() {
    let (facade, _d) = facade_with_grants();

    // A plan to link against.
    let wf_hash = facade.with_store(|m| {
        m.add(
            &areev_core::types::Workflow::new(vec!["fetch".into()])
                .namespace("ops")
                .created_at(1_000),
        )
    })
    .unwrap();

    let amy = facade.principal_session("user:amy").unwrap();
    let h = amy
        .record_tool_call(
            "ops",
            "http_get",
            Some(r#"{"url":"https://example.com"}"#),
            "200 OK",
            false,
            None,
            Some("call-1"),
            Some("run-77"),
            Some(&wf_hash.to_hex()),
            Some("fetch"),
            Some("pending"),
            None,
            Some("host"),
            Some("corr-9"),
        )
        .unwrap();

    // Typed state, indexed both ways.
    let g = facade.with_store(|m| m.get(&h)).unwrap();
    assert_eq!(g.get_str("author_did"), Some("user:amy"));
    let journal = facade.with_store(|m| m.run_grains("ops", "run-77", 0, 10)).unwrap();
    assert_eq!(journal.len(), 1, "run_id must reach run_idx from this path");
    let records = facade
        .with_store(|m| m.step_actions("ops", &wf_hash, Some("fetch"), 10))
        .unwrap();
    assert_eq!(records, vec![("fetch".to_string(), h)]);

    // Half a link is refused, not guessed at.
    let err = amy
        .record_tool_call(
            "ops", "t", None, "r", false, None, None, None,
            Some(&wf_hash.to_hex()), None, None, None, None, None,
        )
        .unwrap_err();
    assert!(err.to_string().contains("both or neither"), "{err}");

    // Unknown enum strings error naming the accepted set — through the whole
    // facade path, not just the builder unit test.
    let err = amy
        .record_tool_call(
            "ops", "t", None, "r", false, None, None, None, None, None,
            Some("done"), None, None, None,
        )
        .unwrap_err();
    assert!(err.to_string().contains("pending, completed, failed"), "{err}");
}

/// The run_id contract holds on this surface too: malformed ids are refused
/// before anything is written.
#[test]
fn malformed_run_ids_are_refused() {
    let (facade, _d) = facade_with_grants();
    let amy = facade.principal_session("user:amy").unwrap();
    for bad in [" ".to_string(), "r".repeat(129), "a\u{0007}b".to_string()] {
        let err = amy
            .record_tool_call(
                "ops", "t", None, "r", false, None, None, Some(&bad),
                None, None, None, None, None, None,
            )
            .unwrap_err();
        assert!(err.to_string().contains("run_id"), "{bad:?}: {err}");
    }
}

// ---------------------------------------------------------------------------
// #302 — a session authorizes READS as itself too
// ---------------------------------------------------------------------------

/// A memory where amy may read `a` and bob may read `b`, with one grain in
/// each.
fn two_principal_rig(dir: &TempDir) -> AreevFacade {
    let mut m = Areev::open(dir.path().join("two.db").to_str().unwrap()).unwrap();
    for (i, (p, obj)) in [
        ("user:amy", "read,write ON a"),
        ("user:bob", "read,write ON b"),
    ]
    .iter()
    .enumerate()
    {
        m.add(
            &Fact::new(p, REL_PERMITS, obj)
                .namespace(AUTHZ_NS)
                .created_at(1_000 + i as i64),
        )
        .unwrap();
    }
    m.add(&Fact::new("deal:1", "stage", "in-a").namespace("a").created_at(2_000))
        .unwrap();
    m.add(&Fact::new("deal:2", "stage", "in-b").namespace("b").created_at(2_001))
        .unwrap();
    AreevFacade::with_session(m, Some("a".to_string()), None)
}

fn recall_through(session: &areev_cal::PrincipalSession<'_>, cal: &str) -> Result<usize, String> {
    let ex = CalExecutor::new(CalExecutorConfig::default());
    match ex.execute(cal, session) {
        Ok(r) => {
            let v = serde_json::to_value(r.payload_json().unwrap()).unwrap();
            Ok(v["grains"].as_array().map(Vec::len).unwrap_or(0))
        }
        Err(e) => Err(e.to_string()),
    }
}

#[test]
fn session_reads_use_the_sessions_own_rights() {
    let dir = TempDir::new().unwrap();
    let f = two_principal_rig(&dir);
    let amy = f.principal_session("user:amy").unwrap();
    let bob = f.principal_session("user:bob").unwrap();

    // amy cannot read b; bob can.
    let err = recall_through(&amy, r#"RECALL facts WHERE namespace = "b" LIMIT 10"#).unwrap_err();
    assert!(err.contains("AUT-E001"), "{err}");
    assert_eq!(
        recall_through(&bob, r#"RECALL facts WHERE namespace = "b" LIMIT 10"#).unwrap(),
        1
    );
    // And the reverse.
    assert_eq!(
        recall_through(&amy, r#"RECALL facts WHERE namespace = "a" LIMIT 10"#).unwrap(),
        1
    );
    let err = recall_through(&bob, r#"RECALL facts WHERE namespace = "a" LIMIT 10"#).unwrap_err();
    assert!(err.contains("AUT-E001"), "{err}");

    // The facade's shared slot is untouched: still the owner.
    assert!(f.authz().is_owner(), "a session never writes the shared slot");
}

#[test]
fn session_covers_every_gated_read() {
    let dir = TempDir::new().unwrap();
    let f = two_principal_rig(&dir);
    let amy = f.principal_session("user:amy").unwrap();
    let ex = CalExecutor::new(CalExecutorConfig::default());

    // A grain amy may not read, by hash.
    let in_b = f
        .with_store(|m| m.recall("b", "deal:2", None, 1))
        .unwrap()
        .first()
        .map(|g| g.hash)
        .unwrap();
    let hex = in_b.to_hex();

    for (label, cal, must_refuse) in [
        ("exists", format!("EXISTS sha256:{hex}"), true),
        ("history", format!("HISTORY sha256:{hex}"), true),
        (
            "recall-in-a",
            r#"RECALL facts WHERE namespace = "a" LIMIT 5"#.to_string(),
            false,
        ),
    ] {
        let got = ex.execute(&cal, &amy);
        let refused = match &got {
            Err(e) => e.to_string().contains("AUT-E"),
            Ok(r) => serde_json::to_value(r.payload_json().unwrap())
                .unwrap()
                .to_string()
                .contains("AUT-E"),
        };
        assert_eq!(refused, must_refuse, "{label}: {got:?}");
    }
}

#[test]
fn concurrent_read_sessions_never_cross() {
    // The race `bind_principal` has is that it swaps ONE process-wide slot.
    // A session's rights are installed per thread for the duration of its
    // own call, so two threads alternating principals over ONE facade can
    // never see each other's.
    let dir = TempDir::new().unwrap();
    let f = std::sync::Arc::new(two_principal_rig(&dir));
    let mut handles = Vec::new();
    for t in 0..8 {
        let f = std::sync::Arc::clone(&f);
        handles.push(std::thread::spawn(move || {
            for _ in 0..50 {
                let (who, ns, expect_ok) = if t % 2 == 0 {
                    ("user:amy", "a", true)
                } else {
                    ("user:bob", "a", false)
                };
                let s = f.principal_session(who).unwrap();
                let got = recall_through(&s, &format!(r#"RECALL facts WHERE namespace = "{ns}" LIMIT 5"#));
                assert_eq!(
                    got.is_ok(),
                    expect_ok,
                    "{who} reading {ns} must be {}: {got:?}",
                    if expect_ok { "allowed" } else { "refused" }
                );
            }
        }));
    }
    for h in handles {
        h.join().unwrap();
    }
    assert!(f.authz().is_owner());
}

#[test]
fn session_default_namespace_scopes_graph_reads() {
    let dir = TempDir::new().unwrap();
    let f = two_principal_rig(&dir);
    // Without an override, bob's namespace-defaulting reads use the
    // facade's "a" — which bob may not read.
    let bob = f.principal_session("user:bob").unwrap();
    let ex = CalExecutor::new(CalExecutorConfig::default());
    let got = ex.execute(r#"RELATED "deal:2" VIA "related_to" DEPTH 1"#, &bob);
    let refused = match &got {
        Err(e) => e.to_string().contains("AUT-E"),
        Ok(r) => serde_json::to_value(r.payload_json().unwrap())
            .unwrap()
            .to_string()
            .contains("AUT-E"),
    };
    assert!(refused, "the facade default is a, which bob cannot read: {got:?}");

    // With one, they walk their own namespace.
    let bob = f.principal_session("user:bob").unwrap().in_namespace("b");
    let got = ex.execute(r#"RELATED "deal:2" VIA "related_to" DEPTH 1"#, &bob);
    assert!(got.is_ok(), "{got:?}");
}

#[test]
fn a_session_never_hands_out_the_unscoped_facade() {
    // A downcast to `AreevFacade` would let a caller read past this
    // session's grants, which is the whole thing the type prevents.
    use areev_cal::CalStoreFacade;
    let dir = TempDir::new().unwrap();
    let f = two_principal_rig(&dir);
    let amy = f.principal_session("user:amy").unwrap();
    assert!(amy.as_any().is_none());
}

#[test]
fn the_session_scope_is_popped_even_when_a_call_fails() {
    // A refusal must not leave one principal's rights installed for the
    // next call on this thread.
    let dir = TempDir::new().unwrap();
    let f = two_principal_rig(&dir);
    {
        let bob = f.principal_session("user:bob").unwrap();
        let _ = recall_through(&bob, r#"RECALL facts WHERE namespace = "a" LIMIT 5"#);
    }
    // The facade, used directly again, is the owner.
    let ex = CalExecutor::new(CalExecutorConfig::default());
    let r = ex
        .execute(r#"RECALL facts WHERE namespace = "a" LIMIT 5"#, &f)
        .unwrap();
    let v = serde_json::to_value(r.payload_json().unwrap()).unwrap();
    assert_eq!(v["grains"].as_array().unwrap().len(), 1);
}

// ── #324: cached rights, a typed write, and a readable grant set ──────────

use areev_core::authz::{AuthzSet, GrantedNamespaces, Verb};

#[test]
fn a_session_built_from_cached_rights_decides_identically() {
    // A host serving many principals paid a grant read under the store mutex
    // on EVERY request, because `principal_session` was the only way to get a
    // session. #309 gave it `authz_epoch` to cache against; this gives it
    // something to cache.
    let (f, _d) = facade_with_grants();

    let resolved = f.resolve_rights("user:amy").unwrap();
    let cached = f.session_with(resolved.clone());
    let fresh = f.principal_session("user:amy").unwrap();

    for (verb, ns, want) in [
        (Verb::Read, "ops", true),
        (Verb::Write, "ops", true),
        (Verb::Delete, "ops", false),
        (Verb::Read, "secret", false),
    ] {
        assert_eq!(
            cached.authz().allows(verb, ns),
            want,
            "cached session: {verb} on {ns}"
        );
        assert_eq!(
            cached.authz().allows(verb, ns),
            fresh.authz().allows(verb, ns),
            "cached and freshly resolved must agree: {verb} on {ns}"
        );
    }
    assert_eq!(cached.principal(), "user:amy");
}

#[test]
fn a_cached_set_is_a_snapshot_and_the_epoch_says_so() {
    // The cache-invalidation contract: narrowing a principal does NOT reach a
    // set already handed out (same rule `principal_session` has always had),
    // and `authz_epoch` moves so a host knows to re-resolve.
    let (f, _d) = facade_with_grants();

    let before_epoch = f.authz_epoch().unwrap();
    let stale = f.resolve_rights("user:amy").unwrap();
    assert!(stale.allows(Verb::Write, "ops"));

    f.set_grants(
        "user:amy",
        &[areev_core::authz::Grant {
            verbs: vec![Verb::Read],
            namespaces: vec!["ops".to_string()],
        }],
        "narrowing amy to read-only",
    )
    .unwrap();

    assert!(
        stale.allows(Verb::Write, "ops"),
        "a set already resolved is a snapshot, by design"
    );
    let after_epoch = f.authz_epoch().unwrap();
    assert_ne!(before_epoch, after_epoch, "the epoch must move so a cache can notice");

    let re_resolved = f.resolve_rights("user:amy").unwrap();
    assert!(re_resolved.allows(Verb::Read, "ops"));
    assert!(
        !re_resolved.allows(Verb::Write, "ops"),
        "a freshly resolved set reflects the narrowing"
    );
}

#[test]
fn typed_add_authorizes_and_attributes_like_cal_add() {
    let (f, _d) = facade_with_grants();
    let s = f.principal_session("user:amy").unwrap();

    let h = s.add(&Fact::new("deal:1", "stage", "diligence").namespace("ops")).unwrap();

    let g = f.with_store(|m| m.get(&h)).unwrap();
    assert_eq!(
        g.get_str("author_did"),
        Some("user:amy"),
        "a session write is attributed to its principal"
    );
    assert_eq!(g.get_str("namespace"), Some("ops"));
    assert_eq!(g.get_str("object"), Some("diligence"));
}

#[test]
fn typed_add_refuses_an_ungranted_namespace_and_writes_nothing() {
    let (f, _d) = facade_with_grants();
    let s = f.principal_session("user:amy").unwrap();

    let before = f.with_store(|m| m.stats()).unwrap().grains;
    let err = s
        .add(&Fact::new("deal:2", "stage", "secret-stage").namespace("secret"))
        .unwrap_err();
    assert_eq!(err.code(), "AUT-E001", "got {err}");
    let after = f.with_store(|m| m.stats()).unwrap().grains;
    assert_eq!(before, after, "a refused write must write nothing");
}

#[test]
fn typed_add_keeps_an_explicit_author() {
    // Attribution is stamped only when absent — a host relaying a write on
    // someone else's behalf must be able to say so.
    let (f, _d) = facade_with_grants();
    let s = f.principal_session("user:amy").unwrap();
    let h = s
        .add(
            &Fact::new("deal:3", "stage", "closed")
                .namespace("ops")
                .author_did("did:key:external"),
        )
        .unwrap();
    let g = f.with_store(|m| m.get(&h)).unwrap();
    assert_eq!(g.get_str("author_did"), Some("did:key:external"));
}

#[test]
fn authz_set_can_say_which_namespaces_it_covers() {
    // "Which namespaces may this principal read" used to be answerable only
    // by probing every namespace the host knew — O(namespaces) per principal
    // per epoch, when the grants are the short list.
    let grants = vec![
        areev_core::authz::Grant {
            verbs: vec![Verb::Read],
            namespaces: vec!["a".into(), "b".into()],
        },
        areev_core::authz::Grant {
            verbs: vec![Verb::Write],
            namespaces: vec!["c".into()],
        },
    ];
    let set = AuthzSet::restricted("user:amy", grants);

    match set.namespaces(Verb::Read) {
        GrantedNamespaces::Exact(ns) => {
            assert_eq!(ns.iter().cloned().collect::<Vec<_>>(), vec!["a", "b"]);
        }
        other => panic!("expected Exact, got {other:?}"),
    }
    match set.namespaces(Verb::Write) {
        GrantedNamespaces::Exact(ns) => {
            assert_eq!(ns.iter().cloned().collect::<Vec<_>>(), vec!["c"]);
        }
        other => panic!("expected Exact, got {other:?}"),
    }
    assert!(
        set.namespaces(Verb::Delete).is_empty(),
        "a verb granted nowhere is an empty set, not All"
    );
}

#[test]
fn a_wildcard_grant_and_the_owner_both_answer_all() {
    // `All` is not a snapshot of the namespaces that exist — a wildcard grant
    // covers ones nothing has written yet, so collapsing it to a list would go
    // stale on the next write.
    let starred = AuthzSet::restricted(
        "user:root",
        vec![areev_core::authz::Grant {
            verbs: vec![Verb::Read],
            namespaces: vec!["*".into()],
        }],
    );
    assert_eq!(starred.namespaces(Verb::Read), GrantedNamespaces::All);
    assert!(starred.namespaces(Verb::Read).exact().is_none());

    // A grant naming no namespace means the same thing.
    let bare = AuthzSet::restricted(
        "user:root",
        vec![areev_core::authz::Grant {
            verbs: vec![Verb::Read],
            namespaces: vec![],
        }],
    );
    assert_eq!(bare.namespaces(Verb::Read), GrantedNamespaces::All);

    assert_eq!(
        AuthzSet::owner("root@localhost").namespaces(Verb::Erase),
        GrantedNamespaces::All
    );
}
