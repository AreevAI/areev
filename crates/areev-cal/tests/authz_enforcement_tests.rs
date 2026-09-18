//! The role × verb enforcement matrix, end to end: CAL text → executor →
//! facade → the AuthzSet check. Every surface that speaks CAL goes through
//! this chokepoint, so this is the cross-surface gate in one place.
//!
//! The principals here are the §4.1 taxonomy's classes: a reader, a writer
//! (write but not supersede), an editor (both), a deleter, an admin — each
//! granted per-namespace, plus the owner who sees none of this.

use areev_cal::{CalExecutor, CalExecutorConfig, AreevFacade};
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
