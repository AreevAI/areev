//! §8 acceptance: single-statement ASSEMBLE across user + mounted org
//! memories — "one statement gives the entire prompt".

use areev_cal::executor::CalResultPayload;
use areev_cal::{CalExecutor, CalExecutorConfig, AreevFacade};
use areev_core::types::{Fact, Grain};
use areev_store::Areev;
use tempfile::TempDir;

fn fact(ns: &str, s: &str, r: &str, o: &str) -> Fact {
    let mut f = Fact::new(s, r, o).confidence(0.9);
    f.common.namespace = Some(ns.to_string());
    f
}

#[test]
fn assemble_spans_user_and_org_files() {
    let d = TempDir::new().unwrap();

    // org replica (would be maintained by `areev follow` in production)
    let mut org = Areev::open(d.path().join("org.db").to_str().unwrap()).unwrap();
    org.add(&fact("policies", "refunds", "window_days", "45")).unwrap();
    org.add(&fact("policies", "refunds", "requires", "receipt")).unwrap();

    // user memory
    let mut user = Areev::open(d.path().join("user.db").to_str().unwrap()).unwrap();
    user.add(&fact("caller", "john", "prefers", "email contact")).unwrap();
    user.add(&fact("caller", "john", "plan", "enterprise")).unwrap();

    let mut facade = AreevFacade::with_session(user, Some("caller".to_string()), None);
    facade.mount("org", org);
    let ex = CalExecutor::new(CalExecutorConfig::default());

    // ONE statement: org policy + user profile, per-source, budgeted engine
    let res = ex
        .execute(
            r#"ASSEMBLE "prompt" FROM
                 policies: (RECALL facts WHERE namespace = "org.policies" AND subject = "refunds"),
                 profile:  (RECALL facts WHERE subject = "john")"#,
            &facade,
        )
        .unwrap();
    match res.result {
        CalResultPayload::Assembled { grains, sources, .. } => {
            assert_eq!(sources.len(), 2, "two sources");
            let all = serde_json::to_string(&grains).unwrap();
            assert!(all.contains("window_days") && all.contains("45"), "org fact present: {all}");
            assert!(all.contains("enterprise"), "user fact present: {all}");
        }
        other => panic!("expected Assembled, got {other:?}"),
    }

    // mounted replicas are read-only by construction: CAL writes route to
    // the session store only — org file remains untouched
    ex.execute(
        r#"ADD fact SET subject = "john" SET relation = "note" SET object = "vip" REASON "t""#,
        &facade,
    )
    .unwrap();
    facade.with_store(|_| ()); // user store touched, org not reachable for writes
    let recall_org = ex
        .execute(r#"RECALL facts WHERE namespace = "org.policies" AND subject = "john""#, &facade)
        .unwrap();
    match recall_org.result {
        CalResultPayload::Grains { grains, .. } => assert!(grains.is_empty(), "no user data in org"),
        other => panic!("unexpected: {other:?}"),
    }
}

/// #369 — `mount_read_only` is the checked, host-surface form: it opens the
/// target read-only itself, and a write addressed to a mounted namespace is
/// REFUSED (`STO-E004`) instead of landing in the primary under the mount's
/// name, where a read of that namespace (routed to the mount) never sees it.
#[test]
fn mount_read_only_serves_reads_and_refuses_writes_through_the_mount() {
    let d = TempDir::new().unwrap();
    let org_path = d.path().join("org.db");
    {
        let mut org = Areev::open(org_path.to_str().unwrap()).unwrap();
        org.add(&fact("policies", "refunds", "window_days", "45")).unwrap();
    }
    let user = Areev::open(d.path().join("user.db").to_str().unwrap()).unwrap();
    let mut facade = AreevFacade::with_session(user, Some("caller".to_string()), None);
    facade.mount_read_only("org", org_path.to_str().unwrap(), None).unwrap();
    assert_eq!(facade.mount_aliases(), vec!["org".to_string()]);
    let ex = CalExecutor::new(CalExecutorConfig::default());

    let res = ex
        .execute(r#"RECALL facts WHERE namespace = "org.policies""#, &facade)
        .unwrap();
    let CalResultPayload::Grains { grains, .. } = res.result else { panic!("expected grains") };
    assert_eq!(grains.len(), 1);

    let before = facade.with_store(|m| m.count().unwrap());
    let e = ex
        .execute(
            r#"ADD fact SET namespace = "org.policies" SET subject = "refunds" SET relation = "window_days" SET object = "10" REASON "t""#,
            &facade,
        )
        .unwrap();
    // ADD reports a refused write as an Unsupported payload (CAL's Tier-1
    // convention), carrying the store's code.
    let CalResultPayload::Unsupported { message, .. } = e.result else {
        panic!("a write through a mount is refused, got {:?}", e.result);
    };
    assert!(message.contains("STO-E004") && message.contains("mount"), "{message}");
    // …and nothing reached the primary under the mount's name.
    assert_eq!(facade.with_store(|m| m.count().unwrap()), before);
    // A namespace merely sharing a prefix with no mount is untouched.
    ex.execute(
        r#"ADD fact SET namespace = "organic.notes" SET subject = "x" SET relation = "y" SET object = "z" REASON "t""#,
        &facade,
    )
    .unwrap();
}

#[test]
fn mount_read_only_refuses_bad_aliases_duplicates_and_missing_files() {
    let d = TempDir::new().unwrap();
    let org_path = d.path().join("org.db");
    Areev::open(org_path.to_str().unwrap()).unwrap();
    let user = Areev::open(d.path().join("user.db").to_str().unwrap()).unwrap();
    let mut facade = AreevFacade::with_session(user, Some("caller".to_string()), None);
    let org = org_path.to_str().unwrap();

    let e = facade.mount_read_only("a.b", org, None).unwrap_err();
    assert_eq!(e.code(), "VAL-E001", "{e}");
    let missing = d.path().join("missing.db");
    let e = facade.mount_read_only("gone", missing.to_str().unwrap(), None).unwrap_err();
    assert_eq!(e.code(), "STO-E005", "{e}");
    assert!(!missing.exists(), "a refused mount creates nothing");
    facade.mount_read_only("org", org, None).unwrap();
    let e = facade.mount_read_only("org", org, None).unwrap_err();
    assert_eq!(e.code(), "VAL-E001", "{e}");
}
