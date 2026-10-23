//! #405 — `CONTAINS NOCASE` / `STARTS WITH NOCASE`: a case-insensitive text
//! match.
//!
//! Records keep a name as their source spelt it (upper-case from a bank
//! narration, mixed case from an invoice), and a person types neither. The
//! text operators compare bytes, so a host had to upper-case both sides by
//! hand. The modifier folds both sides (Unicode simple case folding) on the
//! residual evaluator, which every backend shares.

use areev_cal::errors::CalError;
use areev_cal::executor::{CalExecResult, CalResultPayload};
use areev_cal::{AreevFacade, CalExecutor, CalExecutorConfig};
use areev_core::types::{Fact, Grain};
use areev_store::Areev;
use tempfile::TempDir;

const NS: &str = "statements.transactions";

fn ledger(d: &TempDir) -> AreevFacade {
    let mut m = Areev::open(d.path().join("t.db").to_str().unwrap()).unwrap();
    let rows = [
        ("Malabar-1", "Malabar Cardamom Traders"),
        ("MALABAR-2", "MALABAR CARDAMOM TRADERS INV MCT"),
        ("coorg-3", "Coorg Pepper Works"),
    ];
    for (i, (subject, name)) in rows.into_iter().enumerate() {
        let object = serde_json::json!({ "supplier_name": name }).to_string();
        let mut f = Fact::new(subject, "supplied", &object).confidence(1.0);
        f.common.namespace = Some(NS.to_string());
        f.common.valid_from = Some(1_700_000_000_000 + i as i64);
        m.add(&f).unwrap();
    }
    AreevFacade::with_session(m, Some(NS.into()), None)
}

fn exec(src: &str, facade: &AreevFacade) -> Result<CalExecResult, CalError> {
    CalExecutor::new(CalExecutorConfig::default()).execute(src, facade)
}

/// The subjects of a grain result, sorted.
fn subjects(res: &CalExecResult) -> Vec<String> {
    let CalResultPayload::Grains { grains, .. } = &res.result else {
        panic!("expected Grains, got {:?}", res.result);
    };
    let mut out: Vec<String> = grains
        .iter()
        .map(|g| g.fields["subject"].as_str().unwrap().to_string())
        .collect();
    out.sort();
    out
}

fn rows(src: &str, f: &AreevFacade) -> Vec<String> {
    subjects(&exec(src, f).unwrap_or_else(|e| panic!("{src}: {e}")))
}

#[test]
fn contains_nocase_on_a_dotted_field_with_a_literal_and_a_parameter() {
    let d = TempDir::new().unwrap();
    let f = ledger(&d);

    // Ad hoc, literal: both Malabar spellings, NOT the pepper row.
    assert_eq!(
        rows(r#"RECALL facts WHERE object.supplier_name CONTAINS NOCASE "malabar cardamom""#, &f),
        vec!["MALABAR-2", "Malabar-1"]
    );
    // Without the modifier the match stays case-sensitive.
    assert!(rows(r#"RECALL facts WHERE object.supplier_name CONTAINS "malabar cardamom""#, &f)
        .is_empty());

    // In a saved query, through a parameter — the issue's shape.
    exec(
        r#"DEFINE QUERY "by_supplier"($name) AS { RECALL facts WHERE namespace = "statements.transactions" AND object.supplier_name CONTAINS NOCASE $name LIMIT 100 }"#,
        &f,
    )
    .expect("DEFINE must accept CONTAINS NOCASE $name");
    assert_eq!(
        rows(r#"RUN "by_supplier"($name = "malabar cardamom")"#, &f),
        vec!["MALABAR-2", "Malabar-1"]
    );
    assert_eq!(rows(r#"RUN "by_supplier"($name = "PEPPER")"#, &f), vec!["coorg-3"]);
}

#[test]
fn starts_with_nocase_with_and_without_with_and_with_a_parameter() {
    let d = TempDir::new().unwrap();
    let f = ledger(&d);
    assert_eq!(
        rows(r#"RECALL facts WHERE object.supplier_name STARTS WITH NOCASE "MALABAR c""#, &f),
        vec!["MALABAR-2", "Malabar-1"]
    );
    // A substring that is not a prefix does not match.
    assert!(rows(r#"RECALL facts WHERE object.supplier_name STARTS WITH NOCASE "cardamom""#, &f)
        .is_empty());
    exec(
        r#"DEFINE QUERY "by_prefix"($p) AS { RECALL facts WHERE object.supplier_name STARTS WITH NOCASE $p }"#,
        &f,
    )
    .expect("DEFINE must accept STARTS WITH NOCASE $p");
    assert_eq!(rows(r#"RUN "by_prefix"($p = "coorg")"#, &f), vec!["coorg-3"]);
}

#[test]
fn nocase_on_a_top_level_field_is_not_pushed_down_byte_exact() {
    // `subject CONTAINS` is pushed into the engine's byte-exact substring leg;
    // the folded form must not be, or it would silently stay case-sensitive.
    let d = TempDir::new().unwrap();
    let f = ledger(&d);
    assert_eq!(
        rows(r#"RECALL facts WHERE subject CONTAINS NOCASE "malabar""#, &f),
        vec!["MALABAR-2", "Malabar-1"]
    );
    assert_eq!(rows(r#"RECALL facts WHERE subject STARTS WITH NOCASE "COORG""#, &f), vec!["coorg-3"]);
    assert_eq!(rows(r#"RECALL facts WHERE subject CONTAINS "malabar""#, &f), Vec::<String>::new());
}

#[test]
fn nocase_composes_under_and_or_not() {
    let d = TempDir::new().unwrap();
    let f = ledger(&d);
    assert_eq!(
        rows(
            r#"RECALL facts WHERE object.supplier_name CONTAINS NOCASE "malabar" AND object.supplier_name CONTAINS NOCASE "inv""#,
            &f
        ),
        vec!["MALABAR-2"]
    );
    assert_eq!(
        rows(
            r#"RECALL facts WHERE object.supplier_name CONTAINS NOCASE "traders inv" OR object.supplier_name STARTS WITH NOCASE "coorg""#,
            &f
        ),
        vec!["MALABAR-2", "coorg-3"]
    );
    assert_eq!(
        rows(r#"RECALL facts WHERE NOT (object.supplier_name CONTAINS NOCASE "MALABAR")"#, &f),
        vec!["coorg-3"]
    );
}

#[test]
fn nocase_text_parameter_unbound_is_e008() {
    let d = TempDir::new().unwrap();
    let f = ledger(&d);
    for src in [
        r#"RECALL facts WHERE object.supplier_name CONTAINS NOCASE $name"#,
        r#"RECALL facts WHERE subject CONTAINS NOCASE $name"#,
        r#"RECALL facts WHERE subject = "x" OR subject STARTS WITH NOCASE $name"#,
    ] {
        let err = exec(src, &f).expect_err(src);
        assert_eq!(err.code(), "CAL-E008", "{src}: {err}");
    }
}

#[test]
fn folding_is_unicode_simple_case_folding() {
    let d = TempDir::new().unwrap();
    let mut m = Areev::open(d.path().join("u.db").to_str().unwrap()).unwrap();
    for (s, name) in [("greek", "ΟΔΥΣΣΕΥΣ"), ("german", "Straße"), ("cyr", "МОСКВА")] {
        let object = serde_json::json!({ "n": name }).to_string();
        let mut g = Fact::new(s, "named", &object).confidence(1.0);
        g.common.namespace = Some(NS.to_string());
        m.add(&g).unwrap();
    }
    let f = AreevFacade::with_session(m, Some(NS.into()), None);
    // Final sigma folds with the capital and the medial form.
    assert_eq!(rows(r#"RECALL facts WHERE object.n CONTAINS NOCASE "οδυσσευς""#, &f), vec!["greek"]);
    assert_eq!(rows(r#"RECALL facts WHERE object.n STARTS WITH NOCASE "москва""#, &f), vec!["cyr"]);
    assert_eq!(rows(r#"RECALL facts WHERE object.n CONTAINS NOCASE "STRASSE""#, &f), Vec::<String>::new(),
        "simple folding: ß is not expanded to ss");
    assert_eq!(rows(r#"RECALL facts WHERE object.n CONTAINS NOCASE "STRAẞE""#, &f), vec!["german"]);
}

#[test]
fn nocase_is_on_the_json_cal_wire_only_when_set() {
    let q = areev_cal::parser::parse(r#"RECALL facts WHERE subject CONTAINS NOCASE "a""#).unwrap();
    let wire = serde_json::to_string(&q.statement).unwrap();
    assert!(wire.contains(r#""nocase":true"#), "{wire}");
    let q = areev_cal::parser::parse(r#"RECALL facts WHERE subject CONTAINS "a""#).unwrap();
    let wire = serde_json::to_string(&q.statement).unwrap();
    assert!(!wire.contains("nocase"), "{wire}");
}
