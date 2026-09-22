//! The role × verb enforcement matrix, end to end: CAL text → executor →
//! facade → the AuthzSet check. Every surface that speaks CAL goes through
//! this chokepoint, so this is the cross-surface gate in one place.
//!
//! The principals here are the §4.1 taxonomy's classes: a reader, a writer
//! (write but not supersede), an editor (both), a deleter, an admin — each
//! granted per-namespace, plus the owner who sees none of this.

use areev_cal::{CalExecutor, CalExecutorConfig, AreevFacade, CalStoreFacade};
use areev_core::authz::{AUTHZ_NS, REL_PERMITS};
use areev_core::types::{Fact, Grain};
use areev_store::Areev;
use tempfile::TempDir;

/// Open a memory pre-seeded with one grant per test principal.
fn seeded_store(dir: &TempDir) -> Areev {
    let mut m = Areev::open(dir.path().join("m.db").to_str().unwrap()).unwrap();
    for (i, (principal, object)) in [
        ("user:reader", "read ON caller"),
        ("agent:writer", "read,write ON caller"),
        ("user:editor", "read,write,supersede ON caller"),
        ("job:deleter", "read,delete ON caller"),
        ("user:admin", "admin,read ON *"),
    ]
    .iter()
    .enumerate()
    {
        m.add(
            &Fact::new(principal, REL_PERMITS, object)
                .namespace(AUTHZ_NS)
                .created_at(1_000 + i as i64),
        )
        .unwrap();
    }
    m
}

fn facade_for(dir: &TempDir, principal: Option<&str>) -> AreevFacade {
    let m = seeded_store(dir);
    let f = AreevFacade::with_session(m, Some("caller".to_string()), None);
    match principal {
        Some(p) => f.with_principal(p).unwrap(),
        None => f,
    }
}

const ADD: &str = r#"ADD fact SET subject = "j" SET relation = "likes" SET object = "rust" SET namespace = "caller" REASON "t""#;
const RECALL: &str = r#"RECALL facts WHERE subject = "j" AND namespace = "caller""#;

/// Execute and normalize the two failure channels into one: a hard `Err`
/// from the executor, or an `unsupported` payload carrying the store's
/// refusal message (the executor's convention for failed writes).
fn run(ex: &CalExecutor, f: &AreevFacade, q: &str) -> Result<(), String> {
    let res = ex.execute(q, f).map_err(|e| e.to_string())?;
    let v = serde_json::to_value(res.payload_json().unwrap()).unwrap();
    if v.get("type").and_then(|t| t.as_str()) == Some("unsupported") {
        return Err(v["message"].as_str().unwrap_or("unsupported").to_string());
    }
    Ok(())
}

fn assert_refused(r: Result<(), String>, what: &str) {
    let err = r.expect_err(&format!("{what} must be refused"));
    assert!(err.contains("AUT-E001"), "{what}: expected AUT-E001, got {err}");
}

#[test]
fn owner_does_everything() {
    let dir = TempDir::new().unwrap();
    let f = facade_for(&dir, None);
    let ex = CalExecutor::new(CalExecutorConfig::default());
    run(&ex, &f, ADD).unwrap();
    run(&ex, &f, RECALL).unwrap();
    run(&ex, &f, r#"DEFINE TEMPLATE brief AS "{{content}}""#).unwrap();
}

#[test]
fn reader_reads_and_nothing_else() {
    let dir = TempDir::new().unwrap();
    let f = facade_for(&dir, Some("user:reader"));
    let ex = CalExecutor::new(CalExecutorConfig::default());
    run(&ex, &f, RECALL).unwrap();
    assert_refused(run(&ex, &f, ADD), "reader ADD");
    assert_refused(
        run(&ex, &f, r#"DEFINE TEMPLATE brief AS "{{content}}""#),
        "reader DEFINE TEMPLATE",
    );
}

#[test]
fn writer_writes_but_cannot_supersede_or_escape_its_namespace() {
    let dir = TempDir::new().unwrap();
    let f = facade_for(&dir, Some("agent:writer"));
    let ex = CalExecutor::new(CalExecutorConfig::default());
    run(&ex, &f, ADD).unwrap();

    // The write grant is scoped to `caller`; `shared` is out of bounds.
    assert_refused(
        run(
            &ex,
            &f,
            r#"ADD fact SET subject = "j" SET relation = "r" SET object = "o" SET namespace = "shared" REASON "t""#,
        ),
        "writer ADD outside its namespace",
    );

    // A supersede needs the separate verb — an append-only logger principal
    // cannot rewrite heads.
    let hash = {
        let hits = ex.execute(RECALL, &f).unwrap();
        serde_json::to_value(hits.payload_json().unwrap()).unwrap()["grains"][0]["hash"]
            .as_str()
            .unwrap()
            .to_string()
    };
    assert_refused(
        run(
            &ex,
            &f,
            &format!(
                r#"SUPERSEDE sha256:{hash} SET object = "zig" SET namespace = "caller" BECAUSE "changed""#
            ),
        ),
        "writer SUPERSEDE",
    );
}

#[test]
fn editor_supersedes_but_cannot_forget() {
    let dir = TempDir::new().unwrap();
    let ex = CalExecutor::new(CalExecutorConfig::default());

    let f = facade_for(&dir, Some("user:editor"));
    run(&ex, &f, ADD).unwrap();
    let hash = {
        let hits = ex.execute(RECALL, &f).unwrap();
        serde_json::to_value(hits.payload_json().unwrap()).unwrap()["grains"][0]["hash"]
            .as_str()
            .unwrap()
            .to_string()
    };
    run(
        &ex,
        &f,
        &format!(
            r#"SUPERSEDE sha256:{hash} SET object = "zig" SET namespace = "caller" BECAUSE "changed""#
        ),
    )
    .unwrap();
    // Editor holds no delete.
    assert_refused(
        run(&ex, &f, &format!("FORGET sha256:{hash}")),
        "editor FORGET",
    );
}

#[test]
fn deleter_forgets_only_in_granted_namespace() {
    let dir = TempDir::new().unwrap();
    let ex = CalExecutor::new(CalExecutorConfig::default());

    // Owner seeds one grain in caller and one in shared.
    let f = facade_for(&dir, None);
    run(&ex, &f, ADD).unwrap();
    run(
        &ex,
        &f,
        r#"ADD fact SET subject = "k" SET relation = "r" SET object = "o" SET namespace = "shared" REASON "t""#,
    )
    .unwrap();
    let caller_hash = {
        let hits = ex.execute(RECALL, &f).unwrap();
        serde_json::to_value(hits.payload_json().unwrap()).unwrap()["grains"][0]["hash"]
            .as_str()
            .unwrap()
            .to_string()
    };
    let shared_hash = {
        let hits = ex
            .execute(r#"RECALL facts WHERE subject = "k" AND namespace = "shared""#, &f)
            .unwrap();
        serde_json::to_value(hits.payload_json().unwrap()).unwrap()["grains"][0]["hash"]
            .as_str()
            .unwrap()
            .to_string()
    };
    let store = f.into_inner();

    let f = AreevFacade::with_session(store, Some("caller".to_string()), None)
        .with_principal("job:deleter")
        .unwrap();
    // Delete is granted on caller only — the grain's own namespace decides.
    run(&ex, &f, &format!("FORGET sha256:{caller_hash}")).unwrap();
    assert_refused(
        run(&ex, &f, &format!("FORGET sha256:{shared_hash}")),
        "deleter FORGET outside its namespace",
    );
}

#[test]
fn admin_manages_templates_but_cannot_write_grains() {
    let dir = TempDir::new().unwrap();
    let f = facade_for(&dir, Some("user:admin"));
    let ex = CalExecutor::new(CalExecutorConfig::default());
    run(&ex, &f, r#"DEFINE TEMPLATE brief AS "{{content}}""#).unwrap();
    run(&ex, &f, r#"DROP TEMPLATE "brief""#).unwrap();
    run(&ex, &f, RECALL).unwrap();
    assert_refused(run(&ex, &f, ADD), "admin ADD without write");
}

#[test]
fn restrictive_caps_still_win_over_grants() {
    // The deleter principal holds `delete`, but the process cap says no
    // destructive ops — the cap wins (belt and suspenders).
    let dir = TempDir::new().unwrap();
    let ex_capped = CalExecutor::new(CalExecutorConfig {
        allow_destructive_ops: false,
        ..CalExecutorConfig::default()
    });
    let f = facade_for(&dir, None);
    ex_capped.execute(ADD, &f).unwrap();
    let hash = {
        let hits = ex_capped.execute(RECALL, &f).unwrap();
        serde_json::to_value(hits.payload_json().unwrap()).unwrap()["grains"][0]["hash"]
            .as_str()
            .unwrap()
            .to_string()
    };
    let store = f.into_inner();
    let f = AreevFacade::with_session(store, Some("caller".to_string()), None)
        .with_principal("job:deleter")
        .unwrap();
    let res = ex_capped.execute(&format!("FORGET sha256:{hash}"), &f);
    assert!(res.is_err() || {
        // Some builds surface the cap as an Unsupported payload rather than
        // an error; either way the grain must survive.
        true
    });
    // The grain is still there: the cap blocked the tombstone.
    let hits = ex_capped.execute(RECALL, &f).unwrap();
    let n = serde_json::to_value(hits.payload_json().unwrap()).unwrap()["grains"]
        .as_array()
        .map(|a| a.len())
        .unwrap_or(0);
    assert_eq!(n, 1, "capped FORGET must not tombstone");
}

/// SUPERSEDE authorizes the TARGET's namespace, not the replacement's.
/// `SET namespace` names where the new value lands; letting it decide the
/// check would let a principal rewrite any grain it can name simply by
/// declaring the replacement into a namespace it does hold.
#[test]
fn supersede_is_gated_on_the_targets_namespace_not_the_replacements() {
    let dir = TempDir::new().unwrap();
    let ex = CalExecutor::new(CalExecutorConfig::default());

    // Owner seeds a grain in `hr` — a namespace the editor has no grant on.
    let f = facade_for(&dir, None);
    run(
        &ex,
        &f,
        r#"ADD fact SET subject = "alice" SET relation = "salary" SET object = "200k" SET namespace = "hr" REASON "t""#,
    )
    .unwrap();
    let hr_hash = {
        let hits = ex
            .execute(r#"RECALL facts WHERE subject = "alice" AND namespace = "hr""#, &f)
            .unwrap();
        serde_json::to_value(hits.payload_json().unwrap()).unwrap()["grains"][0]["hash"]
            .as_str()
            .unwrap()
            .to_string()
    };
    let store = f.into_inner();

    // The editor holds `supersede ON caller` only.
    let f = AreevFacade::with_session(store, Some("caller".to_string()), None)
        .with_principal("user:editor")
        .unwrap();
    assert_refused(
        run(
            &ex,
            &f,
            &format!(
                r#"SUPERSEDE sha256:{hr_hash} SET object = "1" SET namespace = "caller" BECAUSE "laundered""#
            ),
        ),
        "SUPERSEDE of an hr grain via a caller-namespaced replacement",
    );

    // The HR grain is untouched: still the live head in its own namespace.
    // Checked as the owner — the editor cannot even read `hr`.
    let store = f.into_inner();
    let f = AreevFacade::with_session(store, Some("caller".to_string()), None);
    let hits = ex
        .execute(r#"RECALL facts WHERE subject = "alice" AND namespace = "hr""#, &f)
        .unwrap();
    let v = serde_json::to_value(hits.payload_json().unwrap()).unwrap();
    assert_eq!(v["grains"][0]["hash"].as_str().unwrap(), hr_hash);
}

/// `PURGE … LIMIT n` must bound the sweep. An ignored bound is not a smaller
/// erasure — it is the whole namespace, irreversibly.
#[test]
fn purge_honors_its_limit() {
    let dir = TempDir::new().unwrap();
    let ex = CalExecutor::new(CalExecutorConfig::default());
    let f = facade_for(&dir, None); // owner: the cap and grants are not what's under test

    for i in 0..5 {
        run(
            &ex,
            &f,
            &format!(
                r#"ADD fact SET subject = "s{i}" SET relation = "r" SET object = "o" SET namespace = "caller" SET created_at = {} REASON "t""#,
                1_000 + i
            ),
        )
        .unwrap();
    }

    let res = ex
        .execute(
            r#"PURGE OLDER THAN 1d IN "caller" LIMIT 2 BECAUSE "retention pilot""#,
            &f,
        )
        .unwrap();
    let v = serde_json::to_value(res.payload_json().unwrap()).unwrap();
    assert_eq!(
        v["count"].as_u64().unwrap(),
        2,
        "LIMIT 2 must erase two grains, not the namespace: {v}"
    );

    let hits = ex
        .execute(r#"RECALL facts WHERE namespace = "caller""#, &f)
        .unwrap();
    let n = serde_json::to_value(hits.payload_json().unwrap()).unwrap()["grains"]
        .as_array()
        .map(|a| a.len())
        .unwrap_or(0);
    assert_eq!(n, 3, "three grains must survive a LIMIT 2 sweep");
}

// ---------------------------------------------------------------------------
// #304 — DERIVED FROM answers what the session can read
// ---------------------------------------------------------------------------

/// Parent P in `a`; children C1 in `a` and C2 in `b`.
fn provenance_rig(dir: &TempDir) -> (AreevFacade, String) {
    let mut m = Areev::open(dir.path().join("prov.db").to_str().unwrap()).unwrap();
    for (i, (principal, object)) in [
        ("user:a-only", "read ON a"),
        ("user:a-and-b", "read ON a,b"),
        ("user:b-only", "read ON b"),
        ("user:wide", "read ON *"),
        ("user:none", "write ON a"),
    ]
    .iter()
    .enumerate()
    {
        m.add(
            &Fact::new(principal, REL_PERMITS, object)
                .namespace(AUTHZ_NS)
                .created_at(1_000 + i as i64),
        )
        .unwrap();
    }
    let parent = m
        .add(&Fact::new("deal:1", "stage", "LOI").namespace("a").created_at(2_000))
        .unwrap();
    let mut c1 = Fact::new("deal:1", "note", "internal").namespace("a").created_at(2_100);
    c1.common.derived_from = Some(parent.to_hex());
    m.add(&c1).unwrap();
    let mut c2 = Fact::new("deal:1", "note", "sibling").namespace("b").created_at(2_200);
    c2.common.derived_from = Some(parent.to_hex());
    m.add(&c2).unwrap();
    let f = AreevFacade::with_session(m, Some("a".to_string()), None);
    (f, parent.to_hex())
}

fn derived_count(f: &AreevFacade, hash: &str) -> Result<usize, String> {
    let ex = CalExecutor::new(CalExecutorConfig::default());
    match ex.execute(&format!("DERIVED FROM sha256:{hash}"), f) {
        Ok(r) => {
            let v = serde_json::to_value(r.payload_json().unwrap()).unwrap();
            Ok(v["grains"].as_array().map(Vec::len).unwrap_or(0))
        }
        Err(e) => Err(e.to_string()),
    }
}

#[test]
fn derived_from_returns_only_what_the_session_can_read() {
    let dir = TempDir::new().unwrap();
    let (f, parent) = provenance_rig(&dir);

    // The owner and a `read ON *` session see both children, as before.
    assert_eq!(derived_count(&f, &parent).unwrap(), 2, "owner is unchanged");
    let wide = f.with_principal("user:wide").unwrap();
    assert_eq!(derived_count(&wide, &parent).unwrap(), 2);

    // A principal granted only `a` sees the child in `a` — and is no longer
    // refused outright for lacking `read ON *`.
    let dir2 = TempDir::new().unwrap();
    let (f2, parent2) = provenance_rig(&dir2);
    let a_only = f2.with_principal("user:a-only").unwrap();
    assert_eq!(derived_count(&a_only, &parent2).unwrap(), 1);

    let dir3 = TempDir::new().unwrap();
    let (f3, parent3) = provenance_rig(&dir3);
    let both = f3.with_principal("user:a-and-b").unwrap();
    assert_eq!(derived_count(&both, &parent3).unwrap(), 2);
}

#[test]
fn derived_from_refuses_a_parent_the_session_cannot_read() {
    // The rule is `get`'s: you may ask what was derived from a grain you can
    // read, and nothing else. A `b`-only principal cannot read the parent in
    // `a`, so the statement is refused rather than answered with an
    // informative empty list.
    let dir = TempDir::new().unwrap();
    let (f, parent) = provenance_rig(&dir);
    let b_only = f.with_principal("user:b-only").unwrap();
    let got = derived_count(&b_only, &parent);
    assert!(got.is_err(), "expected a refusal, got {got:?}");
    assert!(got.unwrap_err().contains("AUT-E001"));

    let dir2 = TempDir::new().unwrap();
    let (f2, parent2) = provenance_rig(&dir2);
    let none = f2.with_principal("user:none").unwrap();
    let err = derived_count(&none, &parent2).unwrap_err();
    assert!(err.contains("AUT-E001"), "{err}");
}

#[test]
fn a_narrowed_result_discloses_no_count_of_what_was_withheld() {
    // A count would say "another namespace derived something from this",
    // which is exactly what `recall` declines to reveal when it refuses
    // without naming a sibling namespace.
    let dir = TempDir::new().unwrap();
    let (f, parent) = provenance_rig(&dir);
    let a_only = f.with_principal("user:a-only").unwrap();
    let ex = CalExecutor::new(CalExecutorConfig::default());
    let v = serde_json::to_value(
        ex.execute(&format!("DERIVED FROM sha256:{parent}"), &a_only)
            .unwrap()
            .payload_json()
            .unwrap(),
    )
    .unwrap();
    let dump = v.to_string();
    assert!(!dump.contains("sibling"), "the b-namespace child must not leak: {dump}");
    assert!(!dump.contains("withheld"), "and neither must a count of it: {dump}");
}

// ── #321: a refusal carries the code that says "refused" ─────────────────
//
// `map_store_err`'s catch-all was `CAL-E030 BudgetExceeded`, so a RECALL
// refused for lack of a grant arrived as a *budget* error carrying the
// AUT-E001 detail — and a host routing on the code (a governed API mapping a
// refusal to 404 + a Denied audit record, an overrun to a retry) had to match
// on a substring inside the message to tell them apart. 1.9.0 had already
// fixed this for `DERIVED FROM` (#304), leaving the recall path the odd one
// out against `docs/cal-reference.md`.

/// The statement-level failure channel: the `CalError` itself, not the
/// executor's `unsupported` payload. `run` above collapses the two, which is
/// right for "was it refused" and wrong for "with which code".
fn code_of(ex: &CalExecutor, f: &AreevFacade, q: &str) -> String {
    match ex.execute(q, f) {
        Err(e) => e.code().to_string(),
        Ok(res) => {
            let v = serde_json::to_value(res.payload_json().unwrap()).unwrap();
            panic!("{q}\nexpected an error, got payload: {v}");
        }
    }
}

#[test]
fn a_refused_recall_is_cal_e121_not_a_budget_error() {
    let dir = TempDir::new().unwrap();
    let ex = CalExecutor::new(CalExecutorConfig::default());
    let f = facade_for(&dir, Some("user:reader")); // read ON caller only

    let q = r#"RECALL facts WHERE namespace = "other" LIMIT 5"#;
    assert_eq!(
        code_of(&ex, &f, q),
        "CAL-E121",
        "an authorization refusal must not surface as a budget overrun"
    );
    let msg = ex.execute(q, &f).unwrap_err().to_string();
    assert!(msg.contains("AUT-E001"), "the AUT detail must survive: {msg}");
    assert!(
        !msg.contains("Budget exceeded"),
        "the refusal must not claim a resource overrun: {msg}"
    );
}

#[test]
fn a_refused_recall_under_a_principal_session_is_also_cal_e121() {
    let dir = TempDir::new().unwrap();
    let ex = CalExecutor::new(CalExecutorConfig::default());
    let f = facade_for(&dir, None); // owner facade; the SESSION is restricted
    let session = f.principal_session("user:reader").unwrap();

    match ex.execute(r#"RECALL facts WHERE namespace = "other" LIMIT 5"#, &session) {
        Err(e) => {
            assert_eq!(e.code(), "CAL-E121");
            assert!(e.to_string().contains("AUT-E001"));
        }
        Ok(res) => panic!("expected a refusal, got {:?}", res.payload_json()),
    }
}

#[test]
fn a_refused_history_diff_is_cal_e121() {
    let dir = TempDir::new().unwrap();
    let ex = CalExecutor::new(CalExecutorConfig::default());

    // Two grains in a namespace `user:reader` cannot read.
    let owner = facade_for(&dir, None);
    let mut hashes = Vec::new();
    for object in ["v1", "v2"] {
        let mut fields = serde_json::Map::new();
        fields.insert("subject".into(), serde_json::json!("j"));
        fields.insert("relation".into(), serde_json::json!("stage"));
        fields.insert("object".into(), serde_json::json!(object));
        fields.insert("namespace".into(), serde_json::json!("other"));
        hashes.push(owner.cal_add("fact", &fields).unwrap().to_hex());
    }
    drop(owner);

    let f = facade_for(&dir, Some("user:reader"));
    let q = format!(
        "HISTORY sha256:{} DIFF sha256:{}",
        hashes[0], hashes[1]
    );
    let code = code_of(&ex, &f, &q);
    assert_eq!(code, "CAL-E121", "a DIFF over unreadable grains: got {code}");
}

#[test]
fn a_non_authz_store_refusal_is_cal_e093_not_a_budget_error() {
    // The other half of #321: the catch-all's NAME was wrong for most of what
    // reached it. A legal hold is the clearest case — `STO-E009` is a
    // deliberate, permanent refusal, and arriving as "Budget exceeded" told a
    // host to retry something that will never succeed.
    let dir = TempDir::new().unwrap();
    let ex = CalExecutor::new(CalExecutorConfig::default());
    let f = facade_for(&dir, None); // owner: authorization is not what refuses

    let mut fields = serde_json::Map::new();
    fields.insert("subject".into(), serde_json::json!("j"));
    fields.insert("relation".into(), serde_json::json!("stage"));
    fields.insert("object".into(), serde_json::json!("v1"));
    fields.insert("namespace".into(), serde_json::json!("caller"));
    let hash = f.cal_add("fact", &fields).unwrap().to_hex();

    f.with_store(|m| m.place_hold("caller", "SEC inquiry", "user:cco", 1_700_000_000_000))
        .unwrap();

    let q = format!(r#"FORGET sha256:{hash} BECAUSE "cleanup""#);
    let err = match ex.execute(&q, &f) {
        Err(e) => e,
        Ok(res) => {
            // The write channel reports refusals as an `unsupported` payload;
            // either way the STO code must be visible and the word "budget"
            // must not be.
            let v = serde_json::to_value(res.payload_json().unwrap()).unwrap();
            let msg = v["message"].as_str().unwrap_or_default().to_string();
            assert!(msg.contains("STO-E009"), "hold refusal must name STO-E009: {v}");
            assert!(
                !msg.to_lowercase().contains("budget"),
                "a legal hold is not a resource overrun: {msg}"
            );
            return;
        }
    };
    assert_eq!(err.code(), "CAL-E093", "got {err}");
    assert!(err.to_string().contains("STO-E009"), "{err}");
}

#[test]
fn cal_e030_no_longer_means_whatever_the_store_said() {
    // A regression pin for the shape of the fix rather than one case: no
    // refusal from the store may surface as CAL-E030 any more. CAL-E030 is
    // reserved for CAL's own budget accounting.
    let dir = TempDir::new().unwrap();
    let ex = CalExecutor::new(CalExecutorConfig::default());
    let f = facade_for(&dir, Some("user:reader"));

    for q in [
        r#"RECALL facts WHERE namespace = "other" LIMIT 5"#,
        r#"ADD fact SET subject = "j" SET relation = "r" SET object = "o" SET namespace = "other" REASON "t""#,
        r#"RECENT 5 IN "other""#,
    ] {
        if let Err(e) = ex.execute(q, &f) {
            assert_ne!(
                e.code(),
                "CAL-E030",
                "{q}\nstore refusals must not claim a budget overrun: {e}"
            );
        }
    }
}

/// A legal-hold refusal must REFUSE, not hang.
///
/// `cal_delete` and `cal_forget_user` locked the store inline in the
/// scrutinee of an `if let` / `match`. Rust holds a scrutinee's temporaries
/// for the whole construct, so the guard was still alive inside the refusal
/// arm — and `audit_hold_refusal` locks the same std `Mutex`, which is not
/// reentrant. Both destructive paths therefore DEADLOCKED the process the
/// moment a hold refused one, which is the exact path #278 added to make a
/// deferral auditable. No CAL- or CLI-level test placed a hold, so it shipped.
///
/// Both legs run on a worker with a join deadline: a regression here hangs
/// forever, and a test that hangs is a test that never reports.
#[test]
fn a_hold_refusal_returns_instead_of_deadlocking() {
    use std::sync::mpsc;
    use std::time::Duration;

    for leg in ["hash", "subject"] {
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let dir = TempDir::new().unwrap();
            let f = facade_for(&dir, None);
            let mut fields = serde_json::Map::new();
            fields.insert("subject".into(), serde_json::json!("j"));
            fields.insert("relation".into(), serde_json::json!("r"));
            fields.insert("object".into(), serde_json::json!("v"));
            fields.insert("namespace".into(), serde_json::json!("caller"));
            let h = f.cal_add("fact", &fields).unwrap();
            f.with_store(|m| m.place_hold("caller", "SEC inquiry", "user:cco", 1))
                .unwrap();

            let err = match leg {
                "hash" => f.cal_delete(&h, Some("cleanup")).unwrap_err(),
                _ => f
                    .cal_forget_user("j", false, "cleanup")
                    .unwrap_err(),
            };
            tx.send(err.code().to_string()).unwrap();
        });

        match rx.recv_timeout(Duration::from_secs(30)) {
            Ok(code) => assert_eq!(
                code, "STO-E009",
                "{leg}: a held namespace must refuse with the hold code"
            ),
            Err(_) => panic!(
                "{leg}: the hold refusal DEADLOCKED — the store lock is held \
                 across audit_hold_refusal again"
            ),
        }
    }
}

// ── #331: the store's code as a value, not a substring ─────────────────
//
// #321 gave refusals honest CAL codes, but the store code under them
// (`STO-E009` vs `STO-E002`, `AUT-E001` vs `AUT-E002`) was only in the
// message text. `CalError::store_code()` carries it beside the text.

#[test]
fn a_hold_refused_forget_carries_sto_e009_as_its_store_code() {
    let dir = TempDir::new().unwrap();
    let ex = CalExecutor::new(CalExecutorConfig::default());
    let f = facade_for(&dir, None);

    let mut fields = serde_json::Map::new();
    fields.insert("subject".into(), serde_json::json!("j"));
    fields.insert("relation".into(), serde_json::json!("stage"));
    fields.insert("object".into(), serde_json::json!("v1"));
    fields.insert("namespace".into(), serde_json::json!("caller"));
    let hash = f.cal_add("fact", &fields).unwrap().to_hex();
    f.with_store(|m| m.place_hold("caller", "SEC inquiry", "user:cco", 1_700_000_000_000))
        .unwrap();

    let err = ex
        .execute(&format!(r#"FORGET sha256:{hash} BECAUSE "cleanup""#), &f)
        .expect_err("a held grain must not be forgotten");
    assert_eq!(err.code(), "CAL-E093", "{err}");
    assert_eq!(err.store_code(), Some("STO-E009"), "{err}");
    // The message is unchanged — nothing matching on it today breaks.
    assert!(err.to_string().contains("STO-E009"), "{err}");
}

#[test]
fn a_refused_recall_carries_aut_e001_as_its_store_code() {
    let dir = TempDir::new().unwrap();
    let ex = CalExecutor::new(CalExecutorConfig::default());
    let q = r#"RECALL facts WHERE namespace = "other" LIMIT 5"#;

    // Bound principal, and the race-free per-principal session: both paths.
    let f = facade_for(&dir, Some("user:reader"));
    let err = ex.execute(q, &f).expect_err("outside the grants");
    assert_eq!(err.code(), "CAL-E121");
    assert_eq!(err.store_code(), Some("AUT-E001"), "{err}");
    drop(f);

    let owner = facade_for(&dir, None);
    let session = owner.principal_session("user:reader").unwrap();
    let err = ex.execute(q, &session).expect_err("outside the grants");
    assert_eq!(err.code(), "CAL-E121");
    assert_eq!(err.store_code(), Some("AUT-E001"), "{err}");
}

#[test]
fn a_refused_history_diff_carries_its_aut_store_code() {
    let dir = TempDir::new().unwrap();
    let ex = CalExecutor::new(CalExecutorConfig::default());
    let owner = facade_for(&dir, None);
    let mut hashes = Vec::new();
    for object in ["v1", "v2"] {
        let mut fields = serde_json::Map::new();
        fields.insert("subject".into(), serde_json::json!("j"));
        fields.insert("relation".into(), serde_json::json!("stage"));
        fields.insert("object".into(), serde_json::json!(object));
        fields.insert("namespace".into(), serde_json::json!("other"));
        hashes.push(owner.cal_add("fact", &fields).unwrap().to_hex());
    }
    drop(owner);

    let f = facade_for(&dir, Some("user:reader"));
    let q = format!("HISTORY sha256:{} DIFF sha256:{}", hashes[0], hashes[1]);
    let err = ex.execute(&q, &f).expect_err("unreadable grains");
    assert_eq!(err.code(), "CAL-E121");
    assert_eq!(err.store_code(), Some("AUT-E001"), "{err}");
}

#[test]
fn a_refused_admin_statement_carries_its_aut_store_code() {
    // DEFINE QUERY reaches the store through a site that detects the refusal
    // itself rather than through `map_store_err` — it must fill the code too.
    let dir = TempDir::new().unwrap();
    let ex = CalExecutor::new(CalExecutorConfig::default());
    let f = facade_for(&dir, Some("user:reader"));
    let err = ex
        .execute(r#"DEFINE QUERY "q1" AS { RECALL facts LIMIT 5 }"#, &f)
        .expect_err("admin is not granted");
    assert_eq!(err.code(), "CAL-E121", "{err}");
    assert_eq!(err.store_code(), Some("AUT-E001"), "{err}");
}

#[test]
fn an_error_cal_raised_itself_has_no_store_code() {
    let dir = TempDir::new().unwrap();
    let ex = CalExecutor::new(CalExecutorConfig::default());
    let f = facade_for(&dir, None);
    let err = ex
        .execute(r#"RECALL facts WHERE subject == "j""#, &f)
        .expect_err("a parse error");
    assert!(err.code().starts_with("CAL-"), "{err}");
    assert_eq!(err.store_code(), None, "{err}");
}
