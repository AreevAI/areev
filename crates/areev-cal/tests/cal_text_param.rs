//! #402 — `CONTAINS` and `STARTS WITH` take a `$param` in a saved query.
//!
//! A person's words for a counterparty rarely equal the canonical name a
//! classifier stored, so a saved query needs a tolerant match whose text is a
//! parameter. Until 1.13 the body was refused at DEFINE (`CAL-E059`), because
//! the text operators took a string literal only.

use areev_cal::executor::CalResultPayload;
use areev_cal::{AreevFacade, CalExecutor, CalExecutorConfig};
use areev_core::types::{Fact, Grain};
use areev_store::Areev;
use tempfile::TempDir;

const NS: &str = "statements.transactions";

fn ledger(d: &TempDir) -> AreevFacade {
    let mut m = Areev::open(d.path().join("t.db").to_str().unwrap()).unwrap();
    let rows = [
        "MALABAR CARDAMOM TRADERS INV MCT",
        "MALABAR CARDAMOM ESTATES",
        "GUNTUR CHILLI MART",
    ];
    for (i, cp) in rows.into_iter().enumerate() {
        let object = serde_json::json!({ "counterparty_canonical": cp }).to_string();
        let mut f = Fact::new(&format!("txn-{i}"), "transaction", &object).confidence(1.0);
        f.common.namespace = Some(NS.to_string());
        f.common.valid_from = Some(1_700_000_000_000 + i as i64);
        m.add(&f).unwrap();
    }
    AreevFacade::with_session(m, Some(NS.into()), None)
}

fn exec(src: &str, facade: &AreevFacade) -> Result<areev_cal::executor::CalExecResult, areev_cal::errors::CalError> {
    CalExecutor::new(CalExecutorConfig::default()).execute(src, facade)
}

/// The subjects of a grain result, sorted.
fn subjects(res: &areev_cal::executor::CalExecResult) -> Vec<String> {
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

#[test]
fn contains_takes_a_parameter_in_a_saved_query_on_a_dotted_field() {
    let d = TempDir::new().unwrap();
    let f = ledger(&d);
    // The issue's body verbatim: a numeric parameter (`LIMIT $limit`) beside
    // a text one, which the DEFINE-time shape probe must tell apart.
    exec(
        r#"DEFINE QUERY "by_name"($name, $limit) AS { RECALL facts WHERE namespace = "statements.transactions" AND object.counterparty_canonical CONTAINS $name ORDER BY valid_from DESC LIMIT $limit FORMAT json }"#,
        &f,
    )
    .expect("DEFINE must accept CONTAINS $name");
    exec(
        r#"DEFINE QUERY "by_name_rows"($name, $limit) AS { RECALL facts WHERE namespace = "statements.transactions" AND object.counterparty_canonical CONTAINS $name LIMIT $limit }"#,
        &f,
    )
    .unwrap();

    let res = exec(r#"RUN "by_name_rows"($name = "MALABAR CARDAMOM", $limit = 100)"#, &f).unwrap();
    // Both Malabar rows, and NOT the chilli row.
    assert_eq!(subjects(&res), vec!["txn-0", "txn-1"]);

    // Case-sensitive, exactly as the literal is.
    let res = exec(r#"RUN "by_name_rows"($name = "malabar", $limit = 100)"#, &f).unwrap();
    assert!(subjects(&res).is_empty());

    // The FORMAT json form runs too and carries both names.
    let res = exec(r#"RUN "by_name"($name = "MALABAR CARDAMOM", $limit = 100)"#, &f).unwrap();
    let text = serde_json::to_string(&res.result).unwrap();
    assert!(text.contains("TRADERS INV MCT") && text.contains("ESTATES"), "{text}");
    assert!(!text.contains("CHILLI"), "{text}");
}

#[test]
fn starts_with_takes_a_parameter_on_a_top_level_and_a_dotted_field() {
    let d = TempDir::new().unwrap();
    let f = ledger(&d);
    exec(
        r#"DEFINE QUERY "prefix"($p) AS { RECALL facts WHERE object.counterparty_canonical STARTS WITH $p }"#,
        &f,
    )
    .unwrap();
    exec(r#"DEFINE QUERY "subj"($p) AS { RECALL facts WHERE subject STARTS WITH $p }"#, &f).unwrap();

    let res = exec(r#"RUN "prefix"($p = "GUNTUR")"#, &f).unwrap();
    assert_eq!(subjects(&res), vec!["txn-2"]);
    // A substring that is not a prefix selects nothing.
    let res = exec(r#"RUN "prefix"($p = "CHILLI")"#, &f).unwrap();
    assert!(subjects(&res).is_empty());

    let res = exec(r#"RUN "subj"($p = "txn-1")"#, &f).unwrap();
    assert_eq!(subjects(&res), vec!["txn-1"]);
}

#[test]
fn a_parameter_with_the_same_name_as_a_literal_value_matches_the_literal_semantics() {
    let d = TempDir::new().unwrap();
    let f = ledger(&d);
    exec(r#"DEFINE QUERY "c"($n) AS { RECALL facts WHERE object.counterparty_canonical CONTAINS $n }"#, &f).unwrap();
    let bound = subjects(&exec(r#"RUN "c"($n = "CARDAMOM")"#, &f).unwrap());
    let literal = subjects(
        &exec(r#"RECALL facts WHERE object.counterparty_canonical CONTAINS "CARDAMOM""#, &f).unwrap(),
    );
    assert_eq!(bound, literal);
    assert_eq!(bound, vec!["txn-0", "txn-1"]);
}

#[test]
fn a_non_string_binding_is_a_typed_error_not_a_silent_non_match() {
    let d = TempDir::new().unwrap();
    let f = ledger(&d);
    exec(r#"DEFINE QUERY "c"($n) AS { RECALL facts WHERE object.counterparty_canonical CONTAINS $n }"#, &f).unwrap();
    let err = exec(r#"RUN "c"($n = 42)"#, &f).expect_err("a number is not a text operand");
    assert!(err.to_string().contains("string literal"), "{err}");
}

#[test]
fn an_unbound_parameter_in_an_ad_hoc_recall_is_cal_e008() {
    let d = TempDir::new().unwrap();
    let f = ledger(&d);
    // Pushed down (top-level `object`), residual (dotted), STARTS WITH, and
    // under OR/NOT — every path refuses rather than matching nothing.
    for q in [
        r#"RECALL facts WHERE object CONTAINS $name"#,
        r#"RECALL facts WHERE object.counterparty_canonical CONTAINS $name"#,
        r#"RECALL facts WHERE subject STARTS WITH $name"#,
        r#"RECALL facts WHERE subject = "txn-0" OR object.counterparty_canonical CONTAINS $name"#,
        r#"RECALL facts WHERE NOT (subject STARTS WITH $name)"#,
    ] {
        let err = exec(q, &f).expect_err(q);
        assert!(err.to_string().starts_with("CAL-E008"), "{q}: {err}");
    }
}

#[test]
fn an_undeclared_text_parameter_is_still_refused_at_run() {
    let d = TempDir::new().unwrap();
    let f = ledger(&d);
    // `$other` is not declared, so RUN leaves it in the text and the parsed
    // body carries it unbound.
    exec(r#"DEFINE QUERY "c"($n) AS { RECALL facts WHERE object.counterparty_canonical CONTAINS $other }"#, &f)
        .unwrap();
    let err = exec(r#"RUN "c"($n = "X")"#, &f).expect_err("unbound");
    assert!(err.to_string().starts_with("CAL-E008"), "{err}");
}
