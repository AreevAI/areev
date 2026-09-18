//! `RELATED` / `ENTITY … AT` over a SET of namespaces (#303).
//!
//! A cross-namespace as-of read is N calls and merely inconvenient. A WALK is
//! not composable from per-namespace calls at all: the frontier, `seen`, the
//! depth counter and the cap are shared state, so a host doing it itself
//! re-implements the BFS and its depth and cap mean something different from
//! Areev's.

use areev_cal::{AreevFacade, CalExecutor, CalExecutorConfig};
use areev_core::authz::{AUTHZ_NS, REL_PERMITS};
use areev_core::types::{Fact, Grain};
use areev_store::{Areev, AreevOptions};
use tempfile::TempDir;

fn rig(dir: &TempDir) -> AreevFacade {
    let mut rels = AreevOptions::default().entity_relations;
    rels.insert("advises".to_string());
    let mut m = Areev::open_with(
        dir.path().join("m.db").to_str().unwrap(),
        AreevOptions { entity_relations: rels, ..Default::default() },
    )
    .unwrap();
    for (i, (p, obj)) in [
        ("user:both", "read ON n1,n2"),
        ("user:one", "read ON n1"),
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
    m.add(&Fact::new("a", "advises", "b").namespace("n1").created_at(2_000))
        .unwrap();
    m.add(&Fact::new("b", "advises", "c").namespace("n2").created_at(2_001))
        .unwrap();
    let mut loi = Fact::new("deal:1", "stage", "LOI").namespace("n1").created_at(2_100);
    loi.common.valid_from = Some(1_000);
    m.add(&loi).unwrap();
    let mut closed = Fact::new("deal:1", "stage", "Closed").namespace("n2").created_at(2_200);
    closed.common.valid_from = Some(1_000);
    m.add(&closed).unwrap();
    AreevFacade::with_session(m, Some("n1".to_string()), None)
}

fn run(f: &AreevFacade, cal: &str) -> Result<serde_json::Value, String> {
    let ex = CalExecutor::new(CalExecutorConfig::default());
    match ex.execute(cal, f) {
        Ok(r) => Ok(serde_json::to_value(r.payload_json().unwrap()).unwrap()),
        Err(e) => Err(e.to_string()),
    }
}

#[test]
fn a_walk_crosses_the_named_namespaces() {
    let dir = TempDir::new().unwrap();
    let f = rig(&dir);
    // One namespace stops where its edges stop.
    let v = run(&f, r#"RELATED "a" VIA "advises" DEPTH 4"#).unwrap();
    assert_eq!(v["entities"], serde_json::json!(["b"]), "{v}");
    // The set carries the walk through.
    let v = run(
        &f,
        r#"RELATED "a" VIA "advises" DEPTH 4 WHERE namespace IN ("n1", "n2")"#,
    )
    .unwrap();
    assert_eq!(v["entities"], serde_json::json!(["b", "c"]), "{v}");
    // A namespace with no edges adds nothing.
    let v = run(
        &f,
        r#"RELATED "a" VIA "advises" DEPTH 4 WHERE namespace IN ("n1", "n3")"#,
    )
    .unwrap();
    assert_eq!(v["entities"], serde_json::json!(["b"]), "{v}");
}

#[test]
fn every_named_namespace_is_read_checked_and_one_refusal_refuses_the_statement() {
    let dir = TempDir::new().unwrap();
    let both = rig(&dir).with_principal("user:both").unwrap();
    let v = run(
        &both,
        r#"RELATED "a" VIA "advises" DEPTH 4 WHERE namespace IN ("n1", "n2")"#,
    )
    .unwrap();
    assert_eq!(v["entities"], serde_json::json!(["b", "c"]), "{v}");

    // `user:one` holds n1 only: the statement is refused whole, with no
    // partial walk — a walk that quietly covered less than it was asked to
    // is an answer that means something different from what it says.
    let dir2 = TempDir::new().unwrap();
    let one = rig(&dir2).with_principal("user:one").unwrap();
    let err = run(
        &one,
        r#"RELATED "a" VIA "advises" DEPTH 4 WHERE namespace IN ("n1", "n2")"#,
    )
    .unwrap_err();
    assert!(err.contains("CAL-E121") || err.contains("AUT-E001"), "{err}");
    // And its own namespace still works.
    assert!(run(&one, r#"RELATED "a" VIA "advises" DEPTH 4 WHERE namespace IN ("n1")"#).is_ok());
}

#[test]
fn a_namespace_pattern_is_refused() {
    let dir = TempDir::new().unwrap();
    let f = rig(&dir);
    let err = run(&f, r#"RELATED "a" VIA "advises" WHERE namespace IN ("n.*")"#).unwrap_err();
    assert!(err.contains("exact namespace"), "{err}");
}

#[test]
fn more_than_a_hundred_terms_is_refused_by_the_shared_in_set_cap() {
    let dir = TempDir::new().unwrap();
    let f = rig(&dir);
    let terms: Vec<String> = (0..101).map(|i| format!("\"n{i}\"")).collect();
    let cal = format!(
        r#"RELATED "a" VIA "advises" WHERE namespace IN ({})"#,
        terms.join(", ")
    );
    let err = run(&f, &cal).unwrap_err();
    assert!(err.contains("CAL-E011"), "{err}");
}

#[test]
fn an_as_of_set_answers_each_namespace_independently() {
    let dir = TempDir::new().unwrap();
    let f = rig(&dir);
    // The single-namespace shape is unchanged.
    let v = run(&f, r#"ENTITY "deal:1" RELATION "stage" AT 5000 AXIS world"#).unwrap();
    assert_eq!(v["grain"]["fields"]["object"], "LOI", "{v}");
    assert!(v.get("grains").is_none() || v["grains"].as_array().unwrap().is_empty());

    // The set shape names its namespace per row, and invents no precedence.
    let v = run(
        &f,
        r#"ENTITY "deal:1" RELATION "stage" AT 5000 AXIS world WHERE namespace IN ("n1", "n2")"#,
    )
    .unwrap();
    let rows = v["grains"].as_array().expect("row per namespace");
    assert_eq!(rows.len(), 2, "{v}");
    assert_eq!(rows[0]["namespace"], "n1");
    assert_eq!(rows[0]["grain"]["fields"]["object"], "LOI");
    assert_eq!(rows[1]["namespace"], "n2");
    assert_eq!(rows[1]["grain"]["fields"]["object"], "Closed");
}

#[test]
fn without_the_clause_nothing_changes() {
    let dir = TempDir::new().unwrap();
    let f = rig(&dir);
    let v = run(&f, r#"RELATED "a" VIA "advises" DEPTH 2"#).unwrap();
    assert_eq!(v["entities"], serde_json::json!(["b"]));
    let v = run(&f, r#"ENTITY "deal:1" RELATION "stage" AT 5000"#).unwrap();
    assert_eq!(v["grain"]["fields"]["object"], "LOI");
}
