//! CAL 1.3 §8.16 governance, end to end against the real engine:
//! `RUN LOOP` → `RECALL recommendations` → `APPROVE`/`APPLY` — one
//! language, with the four gates enforced by the engine underneath and
//! identity derived from the bound session.

use std::sync::Arc;

use areev_cal::{CalExecutor, CalExecutorConfig, AreevFacade};
use areev_core::authz::{AUTHZ_NS, REL_PERMITS};
use areev_core::types::{Fact, Grain};
use areev_loop_adapter::LoopGovernance;
use areev_store::Areev;
use tempfile::TempDir;

fn setup() -> (CalExecutor, AreevFacade, TempDir) {
    let dir = TempDir::new().unwrap();
    let m = Areev::open(dir.path().join("m.db").to_str().unwrap()).unwrap();
    let facade = AreevFacade::with_session(m, Some("caller".to_string()), None);
    let ex = CalExecutor::new(CalExecutorConfig::default())
        .with_governance(Arc::new(LoopGovernance::new()));
    (ex, facade, dir)
}

fn payload(ex: &CalExecutor, f: &AreevFacade, q: &str) -> serde_json::Value {
    serde_json::to_value(ex.execute(q, f).unwrap().payload_json().unwrap()).unwrap()
}

/// Seed a contradiction (two live deploy_target values) so the
/// deterministic contradiction analyzer has something to find.
fn seed_contradiction(f: &AreevFacade) {
    f.with_store(|m| {
        m.add(&Fact::new("acme", "deploy_target", "us-east-1").namespace("caller").created_at(1_000))
            .unwrap();
        m.add(&Fact::new("acme", "deploy_target", "eu-west-1").namespace("caller").created_at(2_000))
            .unwrap();
    });
}

#[test]
fn the_loop_lifecycle_runs_in_one_language() {
    let (ex, f, _dir) = setup();
    seed_contradiction(&f);

    // RUN LOOP — the analysis pass.
    let v = payload(&ex, &f, "RUN LOOP");
    assert_eq!(v["type"], "loop_ran", "{v}");
    assert!(v["run"]["stored"].as_u64().unwrap() >= 1, "{v}");

    // The queue is CAL-visible via DESCRIBE LOOP — the in-language
    // `areev loop list`; take the contradiction rec's hash from it.
    let recs = payload(&ex, &f, "DESCRIBE LOOP");
    let hash = recs["info"]["pending_recommendations"][0]["hash"]
        .as_str()
        .unwrap()
        .to_string();

    // APPROVE ... BECAUSE — the reason is syntax.
    let v = payload(
        &ex,
        &f,
        &format!(r#"APPROVE sha256:{hash} BECAUSE "resolve to the newest region""#),
    );
    assert_eq!(v["type"], "reviewed", "{v}");
    assert_eq!(v["decision"], "approve");

    // APPLY ... BECAUSE — executes the proposal (a supersession).
    let v = payload(
        &ex,
        &f,
        &format!(r#"APPLY sha256:{hash} BECAUSE "approved in review""#),
    );
    assert_eq!(v["type"], "rec_applied", "{v}");

    // The contradiction is resolved: every live value is the newest one —
    // the losing head is superseded and gone from recall.
    let facts = payload(&ex, &f, r#"RECALL facts WHERE subject = "acme""#);
    let objects: Vec<&str> = facts["grains"]
        .as_array()
        .unwrap()
        .iter()
        .map(|g| g["fields"]["object"].as_str().unwrap())
        .collect();
    assert!(!objects.is_empty(), "{facts}");
    assert!(
        objects.iter().all(|o| *o == "eu-west-1"),
        "the losing value must be superseded: {objects:?}"
    );
}

#[test]
fn because_is_syntax_on_every_governance_statement() {
    let (ex, f, _dir) = setup();
    for q in [
        "APPROVE sha256:684c6c9bda818630a870119d0726e4d242ed537af061658ef6f3acb158a2c67d",
        "REJECT sha256:684c6c9bda818630a870119d0726e4d242ed537af061658ef6f3acb158a2c67d",
        "APPLY sha256:684c6c9bda818630a870119d0726e4d242ed537af061658ef6f3acb158a2c67d",
        "ROLLBACK sha256:684c6c9bda818630a870119d0726e4d242ed537af061658ef6f3acb158a2c67d",
    ] {
        let err = ex.execute(q, &f).expect_err(q);
        assert!(err.to_string().contains("CAL-E018"), "{q}: {err}");
    }
}

#[test]
fn review_requires_the_loop_review_grant() {
    let (ex, f, _dir) = setup();
    seed_contradiction(&f);
    // Grants: a bystander (read-only) and a reviewer (loop verbs on the
    // loop namespace, read everywhere for the recall).
    f.with_store(|m| {
        m.add(
            &Fact::new("user:bystander", REL_PERMITS, "read ON *")
                .namespace(AUTHZ_NS)
                .created_at(3_000),
        )
        .unwrap();
        m.add(
            &Fact::new("user:reviewer", REL_PERMITS, "loop.review,read ON *")
                .namespace(AUTHZ_NS)
                .created_at(4_000),
        )
        .unwrap();
    });
    payload(&ex, &f, "RUN LOOP");
    let recs = payload(&ex, &f, "DESCRIBE LOOP");
    let hash = recs["info"]["pending_recommendations"][0]["hash"]
        .as_str()
        .unwrap()
        .to_string();

    let f = AreevFacade::with_session(f.into_inner(), Some("caller".to_string()), None)
        .with_principal("user:bystander")
        .unwrap();
    let v = payload(&ex, &f, &format!(r#"APPROVE sha256:{hash} BECAUSE "sneaky""#));
    assert_eq!(v["type"], "unsupported", "{v}");
    assert!(
        v["message"].as_str().unwrap().contains("LOP-"),
        "the engine's scope refusal must surface: {v}"
    );

    let f = AreevFacade::with_session(f.into_inner(), Some("caller".to_string()), None)
        .with_principal("user:reviewer")
        .unwrap();
    let v = payload(&ex, &f, &format!(r#"APPROVE sha256:{hash} BECAUSE "checked the fork""#));
    assert_eq!(v["type"], "reviewed", "{v}");
}

#[test]
fn describe_loop_reads_and_unwired_executors_say_so() {
    let (ex, f, _dir) = setup();
    seed_contradiction(&f);
    payload(&ex, &f, "RUN LOOP FULL");

    let health = payload(&ex, &f, "DESCRIBE LOOP");
    assert!(health["info"]["pending"].as_u64().unwrap() >= 1, "{health}");
    let analyzers = payload(&ex, &f, "DESCRIBE ANALYZERS");
    assert!(!analyzers["info"].as_array().unwrap().is_empty(), "{analyzers}");
    let policy = payload(&ex, &f, "DESCRIBE POLICY");
    assert_eq!(policy["info"]["attached"], false, "{policy}");

    // An executor with no governance host refuses honestly.
    let bare = CalExecutor::new(CalExecutorConfig::default());
    let v = serde_json::to_value(bare.execute("RUN LOOP", &f).unwrap().payload_json().unwrap())
        .unwrap();
    assert_eq!(v["type"], "unsupported");
    assert!(v["message"].as_str().unwrap().contains("governance is not wired"), "{v}");
}

#[test]
fn run_loop_options_parse_and_gate() {
    let (ex, f, _dir) = setup();
    seed_contradiction(&f);
    // A high min_new gate skips the run.
    let v = payload(&ex, &f, "RUN LOOP WITH min_new(100000)");
    assert_eq!(v["type"], "loop_ran", "{v}");
    assert_eq!(v["run"]["outcome"], "skipped", "{v}");
}
