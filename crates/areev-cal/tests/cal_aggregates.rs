//! #368 — `SUM`/`MIN`/`MAX`/`AVG` stages, `GROUP BY` and `ORDER BY` on a
//! dotted path, and a `max_limit` above 1,000 on a read-only handle.
//!
//! The fixture is a ledger: one Fact per transaction, its object a JSON
//! document (`amount_minor`, `counterparty`, `month`) — the shape a host
//! keeping a business's transactions actually writes.

use areev_cal::executor::CalResultPayload;
use areev_cal::{AreevFacade, CalExecutor, CalExecutorConfig};
use areev_cal::ast::AggregateFn;
use areev_core::types::{Fact, Grain};
use areev_store::{Areev, AreevOptions};
use tempfile::TempDir;

const NS: &str = "ledger.transactions";

fn add_txn(m: &mut Areev, id: usize, counterparty: &str, amount_minor: i64, month: &str) {
    let object = serde_json::json!({
        "amount_minor": amount_minor,
        "counterparty": counterparty,
        "month": month,
    })
    .to_string();
    let mut f = Fact::new(&format!("txn-{id}"), "transaction", &object).confidence(1.0);
    f.common.namespace = Some(NS.to_string());
    m.add(&f).unwrap();
}

/// ACME: 1200 + 300 + 4500 (Jan, Jan, Feb); Globex: 800 + 50 (Jan, Feb);
/// Initech: 999 (Feb).
fn ledger(d: &TempDir) -> Areev {
    let mut m = Areev::open(d.path().join("l.db").to_str().unwrap()).unwrap();
    let rows = [
        ("ACME", 1200, "2026-01"),
        ("ACME", 300, "2026-01"),
        ("ACME", 4500, "2026-02"),
        ("Globex", 800, "2026-01"),
        ("Globex", 50, "2026-02"),
        ("Initech", 999, "2026-02"),
    ];
    for (i, (cp, amt, month)) in rows.into_iter().enumerate() {
        add_txn(&mut m, i, cp, amt, month);
    }
    m
}

fn run(src: &str, facade: &AreevFacade) -> areev_cal::executor::CalExecResult {
    CalExecutor::new(CalExecutorConfig::default()).execute(src, facade).unwrap()
}

fn scalar(res: &areev_cal::executor::CalExecResult) -> (&serde_json::Value, usize) {
    match &res.result {
        CalResultPayload::Aggregate { value, counted, .. } => (value, *counted),
        other => panic!("expected Aggregate, got {other:?}"),
    }
}

#[test]
fn sum_over_a_dotted_path_is_an_exact_integer() {
    let d = TempDir::new().unwrap();
    let facade = AreevFacade::with_session(ledger(&d), Some(NS.into()), None);
    let res = run(
        r#"RECALL facts WHERE namespace = "ledger.transactions" AND object.counterparty = "ACME" | SUM object.amount_minor"#,
        &facade,
    );
    let (value, counted) = scalar(&res);
    // An integer, not 6000.0: minor units must stay exact.
    assert_eq!(value, &serde_json::json!(6000));
    assert!(value.is_i64());
    assert_eq!(counted, 3);
    assert!(res.warnings.iter().all(|w| !w.contains("CAL-W020")), "{:?}", res.warnings);
}

#[test]
fn min_max_keep_integers_and_avg_is_a_float() {
    let d = TempDir::new().unwrap();
    let facade = AreevFacade::with_session(ledger(&d), Some(NS.into()), None);
    let base = r#"RECALL facts WHERE namespace = "ledger.transactions""#;
    let min = run(&format!("{base} | MIN object.amount_minor"), &facade);
    assert_eq!(scalar(&min).0, &serde_json::json!(50));
    let max = run(&format!("{base} MAX object.amount_minor"), &facade);
    assert_eq!(scalar(&max).0, &serde_json::json!(4500));
    let avg = run(&format!("{base} | AVG object.amount_minor"), &facade);
    let (v, n) = scalar(&avg);
    assert_eq!(n, 6);
    assert!((v.as_f64().unwrap() - 7849.0 / 6.0).abs() < 1e-9, "{v}");
    assert!(v.is_f64());
}

#[test]
fn an_empty_set_sums_to_zero_and_has_no_min() {
    let d = TempDir::new().unwrap();
    let facade = AreevFacade::with_session(ledger(&d), Some(NS.into()), None);
    let base = r#"RECALL facts WHERE namespace = "ledger.transactions" AND object.counterparty = "Nobody""#;
    assert_eq!(scalar(&run(&format!("{base} | SUM object.amount_minor"), &facade)).0, &serde_json::json!(0));
    assert!(scalar(&run(&format!("{base} | MIN object.amount_minor"), &facade)).0.is_null());
    assert!(scalar(&run(&format!("{base} | AVG object.amount_minor"), &facade)).0.is_null());
}

/// A non-numeric value is skipped — and said to be skipped, because a total
/// over a mixed set is a well-formed wrong answer.
#[test]
fn a_non_numeric_value_is_skipped_and_counted_in_a_warning() {
    let d = TempDir::new().unwrap();
    let mut m = ledger(&d);
    // A transaction whose amount is a string, and a Fact that carries no amount.
    let mut bad = Fact::new("txn-bad", "transaction", r#"{"amount_minor":"12.00","counterparty":"ACME"}"#)
        .confidence(1.0);
    bad.common.namespace = Some(NS.into());
    m.add(&bad).unwrap();
    let mut note = Fact::new("note-1", "memo", "plain text").confidence(1.0);
    note.common.namespace = Some(NS.into());
    m.add(&note).unwrap();
    let facade = AreevFacade::with_session(m, Some(NS.into()), None);

    let res = run(r#"RECALL facts WHERE namespace = "ledger.transactions" | SUM object.amount_minor"#, &facade);
    match &res.result {
        CalResultPayload::Aggregate { value, counted, skipped, missing, function, field } => {
            assert_eq!(value, &serde_json::json!(7849));
            assert_eq!((*counted, *skipped, *missing), (6, 1, 1));
            assert_eq!(*function, AggregateFn::Sum);
            assert_eq!(field, "object.amount_minor");
        }
        other => panic!("expected Aggregate, got {other:?}"),
    }
    let w = res.warnings.iter().find(|w| w.starts_with("CAL-W020")).expect("CAL-W020");
    assert!(w.contains("1 carried a value that is not a number") && w.contains("1 carried none"), "{w}");
}

#[test]
fn group_by_a_dotted_path_composes_with_sum() {
    let d = TempDir::new().unwrap();
    let facade = AreevFacade::with_session(ledger(&d), Some(NS.into()), None);
    let res = run(
        r#"RECALL facts WHERE namespace = "ledger.transactions" GROUP BY object.counterparty | SUM object.amount_minor"#,
        &facade,
    );
    let CalResultPayload::GroupAggregates { field, function, path, groups, total_available } = &res.result else {
        panic!("expected GroupAggregates, got {:?}", res.result);
    };
    assert_eq!(field, "object.counterparty");
    assert_eq!(*function, AggregateFn::Sum);
    assert_eq!(path, "object.amount_minor");
    assert_eq!(*total_available, Some(3));
    let rows: Vec<(String, i64, i64)> = groups
        .iter()
        .map(|g| {
            (
                g.fields["key"].as_str().unwrap().to_string(),
                g.fields["value"].as_i64().unwrap(),
                g.fields["count"].as_i64().unwrap(),
            )
        })
        .collect();
    // Largest total first.
    assert_eq!(
        rows,
        vec![("ACME".into(), 6000, 3), ("Initech".into(), 999, 1), ("Globex".into(), 850, 2)]
    );
    assert!(groups.iter().all(|g| g.hash.is_empty() && g.grain_type == "group"));
}

/// "Totals per counterparty per month" — a composite dotted key, bounded to
/// the top two, the total still naming how many groups the ranking has.
#[test]
fn composite_dotted_group_key_with_min_is_ascending_and_limit_bounds_it() {
    let d = TempDir::new().unwrap();
    let facade = AreevFacade::with_session(ledger(&d), Some(NS.into()), None);
    let res = run(
        r#"RECALL facts WHERE namespace = "ledger.transactions" GROUP BY object.counterparty, object.month | MIN object.amount_minor | LIMIT 2"#,
        &facade,
    );
    let CalResultPayload::GroupAggregates { groups, total_available, .. } = &res.result else {
        panic!("expected GroupAggregates, got {:?}", res.result);
    };
    assert_eq!(*total_available, Some(5));
    let rows: Vec<(serde_json::Value, i64)> =
        groups.iter().map(|g| (g.fields["keys"].clone(), g.fields["value"].as_i64().unwrap())).collect();
    assert_eq!(
        rows,
        vec![
            (serde_json::json!(["Globex", "2026-02"]), 50),
            (serde_json::json!(["ACME", "2026-01"]), 300),
        ]
    );
}

#[test]
fn order_by_a_dotted_path_ranks_the_largest_first() {
    let d = TempDir::new().unwrap();
    let facade = AreevFacade::with_session(ledger(&d), Some(NS.into()), None);
    let res = run(
        r#"RECALL facts WHERE namespace = "ledger.transactions" ORDER BY object.amount_minor DESC LIMIT 3"#,
        &facade,
    );
    let CalResultPayload::Grains { grains, .. } = &res.result else {
        panic!("expected Grains, got {:?}", res.result);
    };
    let subjects: Vec<&str> = grains.iter().map(|g| g.fields["subject"].as_str().unwrap()).collect();
    // 4500, 1200, 999 — numeric order, not the string order "999" > "4500".
    assert_eq!(subjects, vec!["txn-2", "txn-0", "txn-5"]);

    let asc = run(
        r#"RECALL facts WHERE namespace = "ledger.transactions" SORT object.amount_minor ASC | FIRST"#,
        &facade,
    );
    let CalResultPayload::Grains { grains, .. } = &asc.result else { panic!() };
    assert_eq!(grains[0].fields["subject"], "txn-4");
}

/// The aggregate sees the whole matching set, not the default 50-row page.
#[test]
fn an_aggregate_widens_past_the_default_page() {
    let d = TempDir::new().unwrap();
    let mut m = Areev::open(d.path().join("w.db").to_str().unwrap()).unwrap();
    for i in 0..120 {
        add_txn(&mut m, i, "ACME", 1, "2026-01");
    }
    let facade = AreevFacade::with_session(m, Some(NS.into()), None);
    let res = run(r#"RECALL facts WHERE namespace = "ledger.transactions" | SUM object.amount_minor"#, &facade);
    assert_eq!(scalar(&res).0, &serde_json::json!(120));
}

/// `max_limit` above 1,000 is honoured on a read-only handle, clamped on a
/// read-write one, and `CAL-W015` names the limit that applied.
#[test]
fn max_limit_above_the_default_applies_only_to_a_read_only_handle() {
    let d = TempDir::new().unwrap();
    let path = d.path().join("big.db");
    {
        let mut m = Areev::open(path.to_str().unwrap()).unwrap();
        for i in 0..1_050 {
            add_txn(&mut m, i, "ACME", 1, "2026-01");
        }
    }
    let q = r#"RECALL facts WHERE namespace = "ledger.transactions" | SUM object.amount_minor"#;
    let raised = CalExecutor::new(CalExecutorConfig { max_limit: 5_000, ..Default::default() });

    // Read-write: clamped to 1,000, and the warning says so.
    {
        let facade = AreevFacade::with_session(Areev::open(path.to_str().unwrap()).unwrap(), Some(NS.into()), None);
        let res = raised.execute(q, &facade).unwrap();
        assert_eq!(scalar(&res).0, &serde_json::json!(1000));
        let w = res.warnings.iter().find(|w| w.starts_with("CAL-W015")).expect("CAL-W015");
        assert!(w.contains("effective max_limit, 1000"), "{w}");
    }
    // Read-only: the whole year of rows.
    {
        let ro = Areev::open_with(path.to_str().unwrap(), AreevOptions { read_only: true, ..Default::default() })
            .unwrap();
        let facade = AreevFacade::with_session(ro, Some(NS.into()), None);
        let res = raised.execute(q, &facade).unwrap();
        assert_eq!(scalar(&res).0, &serde_json::json!(1050));
        assert!(res.warnings.iter().all(|w| !w.starts_with("CAL-W015")), "{:?}", res.warnings);

        // …and a read-only scan that still fills names the raised limit.
        let tight = CalExecutor::new(CalExecutorConfig { max_limit: 1_020, ..Default::default() });
        let res = tight.execute(q, &facade).unwrap();
        assert_eq!(scalar(&res).0, &serde_json::json!(1020));
        let w = res.warnings.iter().find(|w| w.starts_with("CAL-W015")).expect("CAL-W015");
        assert!(w.contains("effective max_limit, 1020"), "{w}");
    }
    assert_eq!(areev_cal::effective_max_limit(500_000, true), areev_cal::HARD_MAX_LIMIT);
    assert_eq!(areev_cal::effective_max_limit(5_000, false), areev_cal::DEFAULT_MAX_LIMIT);
}

/// The stage keywords are identifiers, not reserved words: a field named
/// `sum` or `max` stays a field.
#[test]
fn aggregate_words_stay_usable_as_field_names() {
    let q = areev_cal::parser::parse(r#"RECALL facts WHERE namespace = "x" | SELECT subject, max"#);
    assert!(q.is_ok(), "{q:?}");
}

#[test]
fn the_json_wire_form_round_trips() {
    let q = areev_cal::parser::parse(
        r#"RECALL facts WHERE namespace = "x" GROUP BY object.counterparty | AVG object.amount_minor"#,
    )
    .unwrap();
    let wire = serde_json::to_value(&q.pipeline).unwrap();
    assert_eq!(
        wire[1],
        serde_json::json!({"stage": "aggregate", "function": "avg", "field": "object.amount_minor"})
    );
    // Spans are not on the wire; everything else round-trips.
    let back: Vec<areev_cal::ast::PipelineStage> = serde_json::from_value(wire.clone()).unwrap();
    assert_eq!(serde_json::to_value(&back).unwrap(), wire);
}
