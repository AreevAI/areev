//! CAL 1.3 §8.15 Tier-3 DCL, end to end: GRANT / REVOKE / SHOW GRANTS /
//! DESCRIBE PRINCIPAL through text → executor → facade → grant grains in
//! the file — including the headline proof sequence: an agent's write
//! bounces, an admin GRANTs, the agent acts, a REVOKE puts it back.

use areev_cal::{CalExecutor, CalExecutorConfig, AreevFacade};
use areev_store::Areev;
use tempfile::TempDir;

fn setup() -> (CalExecutor, AreevFacade, TempDir) {
    let dir = TempDir::new().unwrap();
    let m = Areev::open(dir.path().join("m.db").to_str().unwrap()).unwrap();
    let facade = AreevFacade::with_session(m, Some("caller".to_string()), None);
    (CalExecutor::new(CalExecutorConfig::default()), facade, dir)
}

fn payload(ex: &CalExecutor, f: &AreevFacade, q: &str) -> serde_json::Value {
    serde_json::to_value(ex.execute(q, f).unwrap().payload_json().unwrap()).unwrap()
}

/// Rebind the same store to a different principal.
fn rebind(f: AreevFacade, principal: Option<&str>) -> AreevFacade {
    let store = f.into_inner();
    let fresh = AreevFacade::with_session(store, Some("caller".to_string()), None);
    match principal {
        Some(p) => fresh.with_principal(p).unwrap(),
        None => fresh,
    }
}

const ADD: &str = r#"ADD fact SET subject = "j" SET relation = "likes" SET object = "rust" SET namespace = "caller" REASON "t""#;

#[test]
fn the_proof_sequence_grant_act_revoke_bounce() {
    let (ex, f, _dir) = setup();

    // The agent starts with nothing: fail closed.
    let f = rebind(f, Some("agent:support-bot"));
    let v = payload(&ex, &f, ADD);
    assert_eq!(v["type"], "unsupported");
    assert!(v["message"].as_str().unwrap().contains("AUT-E001"), "{v}");

    // The owner grants read+write on caller.
    let f = rebind(f, None);
    let v = payload(
        &ex,
        &f,
        r#"GRANT read, write ON caller TO "agent:support-bot" WITH because("support rotation")"#,
    );
    assert_eq!(v["type"], "granted", "{v}");
    assert_eq!(v["object"], "read,write ON caller");

    // The agent can now act — inside its namespace only.
    let f = rebind(f, Some("agent:support-bot"));
    assert_eq!(payload(&ex, &f, ADD)["type"], "added");
    let escaped = payload(
        &ex,
        &f,
        r#"ADD fact SET subject = "x" SET relation = "r" SET object = "o" SET namespace = "shared" REASON "t""#,
    );
    assert_eq!(escaped["type"], "unsupported", "namespace escape must bounce");

    // The owner revokes write; the agent is read-only again.
    let f = rebind(f, None);
    let v = payload(
        &ex,
        &f,
        r#"REVOKE write ON caller FROM "agent:support-bot" WITH because("offboarded")"#,
    );
    assert_eq!(v["type"], "revoked", "{v}");
    assert_eq!(v["grants_touched"], 1);

    let f = rebind(f, Some("agent:support-bot"));
    assert_eq!(payload(&ex, &f, ADD)["type"], "unsupported", "write must be gone");
    assert_eq!(
        payload(&ex, &f, r#"RECALL facts WHERE subject = "j""#)["type"],
        "grains",
        "read must survive the partial revoke"
    );
}

#[test]
fn show_grants_and_describe_principal_report_the_live_state() {
    let (ex, f, _dir) = setup();
    payload(&ex, &f, r#"GRANT read ON caller TO "user:anna""#);
    payload(&ex, &f, r#"GRANT erase ON * TO "job:retention" WITH because("nightly sweep")"#);

    let all = payload(&ex, &f, "SHOW GRANTS");
    let rows = all["grants"].as_array().unwrap();
    assert_eq!(rows.len(), 2, "{all}");

    let one = payload(&ex, &f, r#"SHOW GRANTS FOR "user:anna""#);
    let rows = one["grants"].as_array().unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["object"], "read ON caller");
    assert_eq!(rows[0]["principal"], "user:anna");

    let d = payload(&ex, &f, r#"DESCRIBE PRINCIPAL "job:retention""#);
    let info = &d["info"];
    assert_eq!(info["principal"], "job:retention");
    assert_eq!(info["grants"][0]["verbs"][0], "erase");
    assert_eq!(info["grants"][0]["namespaces"][0], "*");

    // A never-granted principal describes as exactly nothing.
    let d = payload(&ex, &f, r#"DESCRIBE PRINCIPAL "user:nobody""#);
    assert_eq!(d["info"]["grants"].as_array().unwrap().len(), 0);
}

#[test]
fn full_revoke_retracts_and_history_survives() {
    let (ex, f, _dir) = setup();
    payload(&ex, &f, r#"GRANT delete ON caller TO "user:anna""#);
    let v = payload(&ex, &f, r#"REVOKE delete ON caller FROM "user:anna""#);
    assert_eq!(v["grants_touched"], 1);

    // No live rights…
    let rows = payload(&ex, &f, r#"SHOW GRANTS FOR "user:anna""#);
    assert_eq!(rows["grants"].as_array().unwrap().len(), 0);
    let f2 = rebind(f, Some("user:anna"));
    assert_eq!(payload(&ex, &f2, ADD)["type"], "unsupported");

    // …but the grant grain is history, not deleted: the supersession chain
    // survives as ordinary recallable facts (WITH superseded).
    let f = rebind(f2, None);
    let hist = payload(
        &ex,
        &f,
        r#"RECALL facts WHERE namespace = "agent:authz" WITH superseded"#,
    );
    let n = hist["grains"].as_array().unwrap().len();
    assert!(n >= 2, "grant + retraction record must both exist: {hist}");
}

#[test]
fn revoke_wider_than_the_grant_scope_is_refused_by_name() {
    let (ex, f, _dir) = setup();
    payload(&ex, &f, r#"GRANT write ON caller, shared TO "agent:bot""#);
    let v = payload(&ex, &f, r#"REVOKE write ON caller FROM "agent:bot""#);
    assert_eq!(v["type"], "unsupported", "{v}");
    assert!(
        v["message"].as_str().unwrap().contains("revoke at the grant's own scope"),
        "{v}"
    );
    // Nothing changed.
    let rows = payload(&ex, &f, r#"SHOW GRANTS FOR "agent:bot""#);
    assert_eq!(rows["grants"].as_array().unwrap().len(), 1);
}

#[test]
fn dcl_requires_admin_and_erase_is_admin_grantable() {
    let (ex, f, _dir) = setup();
    // A writer principal cannot grant.
    payload(&ex, &f, r#"GRANT read, write ON caller TO "agent:writer""#);
    // An admin principal can — including granting erase (D8).
    payload(&ex, &f, r#"GRANT admin, read ON * TO "user:opsadmin""#);

    let f = rebind(f, Some("agent:writer"));
    let v = payload(&ex, &f, r#"GRANT read ON caller TO "agent:friend""#);
    assert_eq!(v["type"], "unsupported");
    assert!(v["message"].as_str().unwrap().contains("AUT-E001"), "{v}");

    let f = rebind(f, Some("user:opsadmin"));
    let v = payload(
        &ex,
        &f,
        r#"GRANT erase ON caller TO "job:retention" WITH because("delegated sweep")"#,
    );
    assert_eq!(v["type"], "granted", "admin may grant erase (D8): {v}");
}

#[test]
fn unknown_verbs_and_saved_query_dcl_are_refused() {
    let (ex, f, _dir) = setup();
    let v = payload(&ex, &f, r#"GRANT fly ON caller TO "agent:bot""#);
    assert_eq!(v["type"], "unsupported", "{v}");
    assert!(v["message"].as_str().unwrap().contains("unknown verb"), "{v}");

    // No DCL inside saved-query bodies (confused-deputy guard).
    let err = ex
        .execute(
            r#"DEFINE QUERY "sneaky"() AS { GRANT admin ON * TO "agent:evil" }"#,
            &f,
        )
        .expect_err("a stored GRANT must not define");
    assert!(err.to_string().contains("GRANT"), "{err}");

    // SHOW GRANTS is a read and stays fine in read-only classification.
    assert!(areev_cal::classify::query_is_read_only("SHOW GRANTS"));
    assert!(!areev_cal::classify::query_is_read_only(
        r#"GRANT read ON caller TO "x""#
    ));
}

#[test]
fn grants_are_ordinary_recallable_facts() {
    let (ex, f, _dir) = setup();
    payload(&ex, &f, r#"GRANT read ON caller TO "user:anna" WITH because("audit me")"#);
    // The DCL is sugar; the grain is queryable like anything else.
    let v = payload(
        &ex,
        &f,
        r#"RECALL facts WHERE namespace = "agent:authz" AND subject = "user:anna""#,
    );
    let grains = v["grains"].as_array().unwrap();
    assert_eq!(grains.len(), 1, "{v}");
    assert_eq!(grains[0]["fields"]["relation"], "mg:permits");
    assert_eq!(grains[0]["fields"]["object"], "read ON caller");
    assert_eq!(grains[0]["fields"]["context"]["because"], "audit me");
}

#[test]
fn run_verbs_are_grantable_despite_run_being_a_keyword() {
    // RUN lexes as a keyword token (RUN LOOP), so `run.execute` needs its
    // own head arm in parse_dcl_verb — without it the driver enforced
    // run.respond while GRANT could not spell it, leaving an approval
    // boundary nobody could be admitted through (post-review find).
    let (ex, f, _dir) = setup();
    let v = payload(
        &ex,
        &f,
        r#"GRANT run.respond ON ops TO "user:officer" WITH because("refund approvals")"#,
    );
    assert_eq!(v["type"], "granted", "{v}");
    assert_eq!(v["object"], "run.respond ON ops");

    let v = payload(
        &ex,
        &f,
        r#"GRANT run.execute, run.cancel ON ops TO "agent:runner" WITH because("the runner")"#,
    );
    assert_eq!(v["type"], "granted", "{v}");
    assert_eq!(v["object"], "run.cancel,run.execute ON ops"); // verbs canonicalize sorted

    // The grant reads back through SHOW GRANTS…
    let v = payload(&ex, &f, r#"SHOW GRANTS FOR "user:officer""#);
    let listed = serde_json::to_string(&v).unwrap();
    assert!(listed.contains("run.respond"), "{v}");

    // …and revokes cleanly.
    let v = payload(
        &ex,
        &f,
        r#"REVOKE run.respond ON ops FROM "user:officer" WITH because("rotated out")"#,
    );
    assert_eq!(v["type"], "revoked", "{v}");

    // A bogus dotted run-verb still fails verb-registry validation (the
    // refusal is an `unsupported` result naming the verb, not a parse error).
    let v = payload(&ex, &f, r#"GRANT run.everything ON ops TO "agent:x""#);
    assert_eq!(v["type"], "unsupported", "{v}");
    assert!(
        v["message"].as_str().unwrap().contains("run.everything"),
        "{v}"
    );
}
