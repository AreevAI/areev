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
