//! #406 — `DESCRIBE QUERY "name"` returns a saved query's stored definition,
//! and `DESCRIBE QUERIES WITH BODIES` the listing with each body.
//!
//! A host composing a derived read from an installed query (one operator
//! loosened, one stage appended) has to read the definition back rather than
//! carry a second copy of every body.

use areev_cal::executor::{CalExecResult, CalResultPayload};
use areev_cal::{AreevFacade, CalExecutor, CalExecutorConfig};
use areev_store::{Areev, AreevOptions};
use tempfile::TempDir;

const BODY: &str = r#"RECALL facts WHERE namespace = "statements.transactions" AND object.counterparty_canonical CONTAINS NOCASE $name ORDER BY valid_from DESC LIMIT $limit"#;

fn exec(src: &str, facade: &AreevFacade) -> CalExecResult {
    CalExecutor::new(CalExecutorConfig::default())
        .execute(src, facade)
        .unwrap_or_else(|e| panic!("{src}: {e}"))
}

fn describe(res: &CalExecResult) -> &serde_json::Value {
    match &res.result {
        CalResultPayload::Describe { info } => info,
        other => panic!("expected Describe, got {other:?}"),
    }
}

fn define(f: &AreevFacade) {
    exec(
        &format!(
            r#"DEFINE QUERY "by_name"($name, $limit = 50) DESCRIPTION "rows by counterparty" AS {{ {BODY} }}"#
        ),
        f,
    );
}

#[test]
fn describe_query_returns_the_stored_body_and_parameter_names() {
    let d = TempDir::new().unwrap();
    let f = AreevFacade::new(Areev::open(d.path().join("q.db").to_str().unwrap()).unwrap());
    define(&f);

    let res = exec(r#"DESCRIBE QUERY "by_name""#, &f);
    let info = describe(&res);
    assert_eq!(info["name"], "by_name");
    assert_eq!(info["description"], "rows by counterparty");
    // Verbatim, not a re-rendering.
    assert_eq!(info["body"], BODY);
    let names: Vec<&str> = info["params"].as_array().unwrap().iter().map(|p| p["name"].as_str().unwrap()).collect();
    assert_eq!(names, vec!["name", "limit"]);
    assert_eq!(info["builtin"], false);
    assert!(info["updated_at"].as_u64().is_some(), "{info}");
    assert!(info["last_run_at"].is_null(), "never run yet: {info}");

    exec(r#"RUN "by_name"($name = "x")"#, &f);
    let res = exec(r#"DESCRIBE QUERY "by_name""#, &f);
    assert!(describe(&res)["last_run_at"].as_u64().is_some());
}

#[test]
fn an_unregistered_name_is_the_same_code_run_gives() {
    let d = TempDir::new().unwrap();
    let f = AreevFacade::new(Areev::open(d.path().join("q.db").to_str().unwrap()).unwrap());
    let ex = CalExecutor::new(CalExecutorConfig::default());
    let describe_err = ex.execute(r#"DESCRIBE QUERY "nope""#, &f).unwrap_err();
    let run_err = ex.execute(r#"RUN "nope""#, &f).unwrap_err();
    assert_eq!(describe_err.code(), "CAL-E051");
    assert_eq!(describe_err.code(), run_err.code());
}

#[test]
fn the_listing_stays_cheap_unless_bodies_are_asked_for() {
    let d = TempDir::new().unwrap();
    let f = AreevFacade::new(Areev::open(d.path().join("q.db").to_str().unwrap()).unwrap());
    define(&f);
    let entry = |res: &CalExecResult| {
        describe(res)["queries"]
            .as_array()
            .unwrap()
            .iter()
            .find(|q| q["name"] == "by_name")
            .cloned()
            .unwrap()
    };

    let plain = entry(&exec("DESCRIBE QUERIES", &f));
    assert!(plain.get("body").is_none(), "{plain}");
    assert_eq!(plain["param_count"], 2);
    assert_eq!(plain["body_size"], BODY.len());

    let full = entry(&exec("DESCRIBE QUERIES WITH BODIES", &f));
    assert_eq!(full["body"], BODY);
    assert_eq!(full["params"], serde_json::json!([{ "name": "name" }, { "name": "limit" }]));
    // Same row otherwise.
    assert_eq!(full["param_count"], 2);
    assert_eq!(full["updated_at"], plain["updated_at"]);
}

#[test]
fn a_read_only_handle_may_describe() {
    let d = TempDir::new().unwrap();
    let path = d.path().join("q.db");
    {
        let f = AreevFacade::new(Areev::open(path.to_str().unwrap()).unwrap());
        define(&f);
    }
    let ro = Areev::open_with(
        path.to_str().unwrap(),
        AreevOptions { read_only: true, ..AreevOptions::default() },
    )
    .unwrap();
    let f = AreevFacade::new(ro);
    let res = exec(r#"DESCRIBE QUERY "by_name""#, &f);
    assert_eq!(describe(&res)["body"], BODY);
    let res = exec("DESCRIBE QUERIES WITH BODIES", &f);
    assert!(describe(&res)["queries"].as_array().unwrap().iter().any(|q| q["body"] == BODY));
}
