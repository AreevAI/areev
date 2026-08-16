//! `PrincipalSession` — the rebind-race-free, attributed write path
//! (governed-agents §6.8). `bind_principal` swaps one process-wide slot and
//! is safe only for hosts that serialize requests; a runtime journaling as a
//! run's principal while an approver responds as theirs needs concurrent
//! per-principal rights. These tests pin: fail-closed resolution, per-write
//! authorization, author attribution on the grain itself, the shared slot
//! staying untouched, and the extended `record_tool_call` reaching typed
//! fields end to end through the facade.

use areev_cal::AreevFacade;
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
