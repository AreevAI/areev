//! The cross-surface parity gate (CAL 1.3): **every governed operation has
//! a CAL spelling** — the truth condition behind "CAL is all you need". If
//! an operation gains a host surface without a CAL spelling, add it here or
//! consciously document the exception; this table is the claim, executable.
//!
//! Plus REMEMBER end to end — the onboarding verb.

use areev_cal::classify::{classify, StatementClass};
use areev_cal::parser::parse;
use areev_cal::{CalExecutor, CalExecutorConfig, AreevFacade};
use areev_core::authz::{AUTHZ_NS, REL_PERMITS};
use areev_core::types::{Fact, Grain};
use areev_store::Areev;
use tempfile::TempDir;

const H: &str = "sha256:684c6c9bda818630a870119d0726e4d242ed537af061658ef6f3acb158a2c67d";

#[test]
fn every_governed_operation_has_a_cal_spelling() {
    use StatementClass::*;
    let table: &[(&str, String, StatementClass)] = &[
        // ── memory operations ─────────────────────────────────────────
        ("recall", r#"RECALL facts WHERE subject = "j""#.into(), Read),
        ("assemble", r#"ASSEMBLE "topic" FROM facts WHERE subject = "j""#.into(), Read),
        ("history", format!("HISTORY OF {H}"), Read),
        ("exists", format!("EXISTS {H}"), Read),
        ("add", r#"ADD fact SET subject = "j" SET relation = "r" SET object = "o" REASON "t""#.into(), Evolve),
        ("supersede", format!(r#"SUPERSEDE {H} SET object = "x" BECAUSE "t""#), Evolve),
        ("revert", format!(r#"REVERT {H} BECAUSE "t""#), Evolve),
        ("remember", r#"REMEMBER "the caller prefers email" WITH session("s1"), role("user"), run("r1")"#.into(), Evolve),
        // ── the DSAR read (GDPR Art. 15/20 — the erasure selector in
        //    show-me mode; OMS 1.6 draft) ───────────────────────────────
        ("subject-report", r#"REPORT SUBJECT "pat" WITH text_mentions"#.into(), Read),
        // ── destruction (Tier 2) ──────────────────────────────────────
        ("forget", format!(r#"FORGET {H} BECAUSE "t""#), Destructive),
        ("forget-subject", r#"FORGET SUBJECT "pat" WITH text_mentions BECAUSE "gdpr""#.into(), Destructive),
        ("purge-older-than", r#"PURGE OLDER THAN 90d TYPE event IN "caller" BECAUSE "retention""#.into(), Destructive),
        // ── access control (Tier 3 DCL) ───────────────────────────────
        ("grant", r#"GRANT read, write ON caller TO "agent:bot" WITH because("t")"#.into(), Control),
        ("revoke", r#"REVOKE write ON caller FROM "agent:bot""#.into(), Control),
        ("show-grants", r#"SHOW GRANTS FOR "agent:bot""#.into(), Read),
        ("describe-principal", r#"DESCRIBE PRINCIPAL "agent:bot""#.into(), Read),
        // ── governance (Tier 3) ───────────────────────────────────────
        ("loop-run", r#"RUN LOOP FULL WITH min_new(1), if_stale("6h")"#.into(), Control),
        ("loop-approve", format!(r#"APPROVE {H} BECAUSE "t""#), Control),
        ("loop-reject", format!(r#"REJECT {H} BECAUSE "t""#), Control),
        ("loop-apply", format!(r#"APPLY {H} BECAUSE "t""#), Control),
        ("loop-rollback", format!(r#"ROLLBACK {H} BECAUSE "t""#), Control),
        ("loop-list", "DESCRIBE LOOP".into(), Read),
        ("loop-analyzers", "DESCRIBE ANALYZERS".into(), Read),
        ("loop-outcomes", "DESCRIBE OUTCOMES".into(), Read),
        ("loop-policy", "DESCRIBE POLICY".into(), Read),
        // ── the wave-2 reads ──────────────────────────────────────────
        ("entity-at", r#"ENTITY "alice" RELATION "employer" AT 1700000000000 AXIS knowledge"#.into(), Read),
        ("run-trace", r#"RUN TRACE "run-42" LIMIT 50"#.into(), Read),
        ("runs-touching", format!("RUNS TOUCHING {H} DEPTH 3"), Read),
        ("provenance", format!("DERIVED FROM {H}"), Read),
        ("forks", "SHOW FORKS".into(), Read),
        ("merge", r#"MERGE "acme" RELATION "tier" TO "enterprise" CONFIDENCE 0.9 BECAUSE "resolved""#.into(), Evolve),
        ("related", r#"RELATED "alice" VIA "reports_to, mg:knows" DIRECTION both DEPTH 2 LIMIT 50"#.into(), Read),
        ("novelty", r#"NOVELTY "prefers a window seat" SUBJECT "john" LIMIT 5"#.into(), Read),
        ("stats", "DESCRIBE STATS".into(), Read),
        ("verify", "DESCRIBE INTEGRITY".into(), Read),
        // ── host metadata (admin) ─────────────────────────────────────
        ("define-template", r#"DEFINE TEMPLATE brief AS "{{content}}""#.into(), Control),
        ("drop-template", r#"DROP TEMPLATE "brief""#.into(), Control),
        ("define-query", r#"DEFINE QUERY "q"() AS { RECALL facts WHERE subject = "j" }"#.into(), Control),
        ("drop-query", r#"DROP QUERY "q""#.into(), Control),
    ];
    for (op, spelling, expected) in table {
        let q = parse(spelling).unwrap_or_else(|e| panic!("{op}: {spelling:?} must parse: {e}"));
        let got = classify(&q.statement);
        assert_eq!(got, *expected, "{op}: {spelling:?} classified {got:?}");
    }
}

#[test]
fn remember_captures_an_event_with_its_metadata() {
    let dir = TempDir::new().unwrap();
    let m = Areev::open(dir.path().join("m.db").to_str().unwrap()).unwrap();
    let f = AreevFacade::with_session(m, Some("caller".to_string()), None);
    let ex = CalExecutor::new(CalExecutorConfig::default());

    let v = serde_json::to_value(
        ex.execute(
            r#"REMEMBER "caller asked about the refund" WITH session("s1"), role("user"), run("run-9")"#,
            &f,
        )
        .unwrap()
        .payload_json()
        .unwrap(),
    )
    .unwrap();
    assert_eq!(v["type"], "remembered", "{v}");
    assert!(v["hash"].as_str().unwrap().len() == 64);

    let events = serde_json::to_value(
        ex.execute(r#"RECALL events WHERE session_id = "s1" RECENT 10"#, &f)
            .unwrap()
            .payload_json()
            .unwrap(),
    )
    .unwrap();
    let grains = events["grains"].as_array().unwrap();
    assert_eq!(grains.len(), 1, "{events}");
    assert_eq!(grains[0]["fields"]["run_id"], "run-9");
}

#[test]
fn remember_requires_the_write_grant() {
    let dir = TempDir::new().unwrap();
    let mut m = Areev::open(dir.path().join("m.db").to_str().unwrap()).unwrap();
    m.add(
        &Fact::new("user:reader", REL_PERMITS, "read ON caller")
            .namespace(AUTHZ_NS)
            .created_at(1_000),
    )
    .unwrap();
    let f = AreevFacade::with_session(m, Some("caller".to_string()), None)
        .with_principal("user:reader")
        .unwrap();
    let ex = CalExecutor::new(CalExecutorConfig::default());
    let v = serde_json::to_value(
        ex.execute(r#"REMEMBER "sneaky write""#, &f).unwrap().payload_json().unwrap(),
    )
    .unwrap();
    assert_eq!(v["type"], "unsupported", "{v}");
    assert!(v["message"].as_str().unwrap().contains("AUT-E001"), "{v}");
}
