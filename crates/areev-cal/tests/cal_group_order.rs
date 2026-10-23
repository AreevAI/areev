//! #409 — `ORDER BY` and `LIMIT` on a grouped projection.
//!
//! A host wants the newest N groups of a grouped recall ("the six most recent
//! months a statement was converted"). Under `GROUP BY … SUM`, `LIMIT n`
//! returned the groups the projection ranked first, an `ORDER BY` on the key
//! was accepted and ignored, and `RECENT n` before the grouping was widened
//! away. An `ORDER BY` over grouped rows now orders the rows (by `key`,
//! `count`, `value`, or a grouped field), and one the rows cannot carry is
//! refused (`CAL-E124`).

use areev_cal::errors::CalError;
use areev_cal::executor::{CalExecResult, CalResultPayload};
use areev_cal::{AreevFacade, CalExecutor, CalExecutorConfig};
use areev_core::types::{Fact, Grain};
use areev_store::Areev;
use tempfile::TempDir;

const NS: &str = "statements.summaries";

/// The fourteen months 2026-09 … 2027-10.
fn months() -> Vec<String> {
    (0..14)
        .map(|i| {
            let m = 9 + i;
            format!("{}-{:02}", 2026 + (m - 1) / 12, (m - 1) % 12 + 1)
        })
        .collect()
}

/// One summary per month; month `i` holds `i + 1` transactions, written in
/// month order so the newest grain is the newest month.
fn summaries(d: &TempDir) -> AreevFacade {
    let mut m = Areev::open(d.path().join("s.db").to_str().unwrap()).unwrap();
    for (i, month) in months().iter().enumerate() {
        let object = serde_json::json!({
            "month": month,
            "statement_id": "stmt-1",
            "transaction_count": i + 1,
        })
        .to_string();
        let mut f = Fact::new(&format!("summary-{month}"), "summarises", &object).confidence(1.0);
        f.common.namespace = Some(NS.to_string());
        f.common.created_at = Some(1_800_000_000_000 + i as i64 * 1_000);
        m.add(&f).unwrap();
    }
    AreevFacade::with_session(m, Some(NS.into()), None)
}

fn exec(src: &str, facade: &AreevFacade) -> Result<CalExecResult, CalError> {
    CalExecutor::new(CalExecutorConfig::default()).execute(src, facade)
}

/// Each grouped row's first key part (the month), in row order.
fn group_months(res: &CalExecResult) -> Vec<String> {
    let groups = match &res.result {
        CalResultPayload::GroupAggregates { groups, .. } | CalResultPayload::GroupCounts { groups, .. } => groups,
        other => panic!("expected grouped rows, got {other:?}"),
    };
    groups
        .iter()
        .map(|g| match g.fields.get("keys") {
            Some(keys) => keys[0].as_str().unwrap().to_string(),
            None => g.fields["key"].as_str().unwrap().to_string(),
        })
        .collect()
}

fn run(src: &str, f: &AreevFacade) -> Vec<String> {
    group_months(&exec(src, f).unwrap_or_else(|e| panic!("{src}: {e}")))
}

const GROUPED: &str = r#"RECALL facts WHERE namespace = "statements.summaries" GROUP BY object.month, object.statement_id SUM object.transaction_count"#;

#[test]
fn order_by_key_desc_limit_returns_the_newest_groups() {
    let d = TempDir::new().unwrap();
    let f = summaries(&d);
    let newest_seven: Vec<String> = months().into_iter().rev().take(7).collect();
    assert_eq!(newest_seven.first().unwrap(), "2027-10");
    assert_eq!(newest_seven.last().unwrap(), "2027-04");

    // By the whole key, and by the grouped field — after the aggregate.
    assert_eq!(run(&format!("{GROUPED} ORDER BY key DESC LIMIT 7"), &f), newest_seven);
    assert_eq!(run(&format!("{GROUPED} ORDER BY object.month DESC LIMIT 7"), &f), newest_seven);
    // …and written between GROUP BY and the aggregate, where it used to sort
    // the grains and split the groups.
    assert_eq!(
        run(
            r#"RECALL facts GROUP BY object.month, object.statement_id ORDER BY object.month DESC SUM object.transaction_count LIMIT 7"#,
            &f
        ),
        newest_seven
    );
    // ASC is the oldest seven.
    let oldest_seven: Vec<String> = months().into_iter().take(7).collect();
    assert_eq!(run(&format!("{GROUPED} ORDER BY key ASC LIMIT 7"), &f), oldest_seven);
}

#[test]
fn the_total_is_still_the_number_of_groups() {
    let d = TempDir::new().unwrap();
    let f = summaries(&d);
    let res = exec(&format!("{GROUPED} ORDER BY key DESC LIMIT 7"), &f).unwrap();
    let CalResultPayload::GroupAggregates { groups, total_available, .. } = &res.result else {
        panic!("{:?}", res.result)
    };
    assert_eq!(groups.len(), 7);
    assert_eq!(*total_available, Some(14));
}

#[test]
fn order_by_value_and_count() {
    let d = TempDir::new().unwrap();
    let f = summaries(&d);
    // Month i holds i+1 transactions, so the largest sums are the newest.
    assert_eq!(
        run(&format!("{GROUPED} ORDER BY value DESC LIMIT 3"), &f),
        vec!["2027-10", "2027-09", "2027-08"]
    );
    assert_eq!(
        run(&format!("{GROUPED} ORDER BY value ASC LIMIT 2"), &f),
        vec!["2026-09", "2026-10"]
    );
    // A grouped COUNT orders by key too; count ties fall back to key ASC.
    assert_eq!(
        run(r#"RECALL facts GROUP BY object.month COUNT ORDER BY key DESC LIMIT 2"#, &f),
        vec!["2027-10", "2027-09"]
    );
    assert_eq!(
        run(r#"RECALL facts GROUP BY object.month COUNT ORDER BY count DESC LIMIT 2"#, &f),
        vec!["2026-09", "2026-10"]
    );
}

#[test]
fn recent_before_group_by_is_the_aggregate_window() {
    let d = TempDir::new().unwrap();
    let f = summaries(&d);
    let got = run(
        r#"RECALL facts RECENT 6 GROUP BY object.month SUM object.transaction_count ORDER BY key DESC"#,
        &f,
    );
    assert_eq!(got, months().into_iter().rev().take(6).collect::<Vec<_>>());
    // A COUNT over the window counts the window.
    let res = exec(r#"RECALL facts RECENT 4 | COUNT"#, &f).unwrap();
    assert!(matches!(res.result, CalResultPayload::Count { count: 4 }), "{:?}", res.result);
}

#[test]
fn an_order_the_rows_cannot_carry_is_refused() {
    let d = TempDir::new().unwrap();
    let f = summaries(&d);
    for src in [
        // A grain field after the projection.
        format!("{GROUPED} ORDER BY valid_from DESC LIMIT 7"),
        // `value` on a grouped COUNT.
        r#"RECALL facts GROUP BY object.month COUNT ORDER BY value DESC"#.to_string(),
        // Before the GROUP BY with no bound between: the grouping discards it.
        r#"RECALL facts ORDER BY valid_from DESC GROUP BY object.month SUM object.transaction_count LIMIT 7"#.to_string(),
    ] {
        let err = exec(&src, &f).expect_err(&src);
        assert_eq!(err.code(), "CAL-E124", "{src}: {err}");
    }
    // Before the GROUP BY WITH a bound between, it picks the window: allowed.
    let got = run(
        r#"RECALL facts ORDER BY created_at DESC LIMIT 3 GROUP BY object.month SUM object.transaction_count ORDER BY key DESC"#,
        &f,
    );
    assert_eq!(got, vec!["2027-10", "2027-09", "2027-08"]);
}

#[test]
fn works_in_a_saved_query_body() {
    let d = TempDir::new().unwrap();
    let f = summaries(&d);
    exec(
        r#"DEFINE QUERY "recent_months"($n) AS { RECALL facts WHERE namespace = "statements.summaries" GROUP BY object.month, object.statement_id SUM object.transaction_count ORDER BY key DESC LIMIT $n }"#,
        &f,
    )
    .unwrap();
    assert_eq!(
        run(r#"RUN "recent_months"($n = 6)"#, &f),
        months().into_iter().rev().take(6).collect::<Vec<_>>()
    );
    // A body with an order the rows cannot carry is refused when it runs.
    exec(
        r#"DEFINE QUERY "bad_order" AS { RECALL facts GROUP BY object.month COUNT ORDER BY valid_from }"#,
        &f,
    )
    .unwrap();
    assert_eq!(exec(r#"RUN "bad_order""#, &f).unwrap_err().code(), "CAL-E124");
}

#[test]
fn a_call_site_order_by_sorts_a_grouped_body() {
    let d = TempDir::new().unwrap();
    let f = summaries(&d);
    exec(
        r#"DEFINE QUERY "by_month" AS { RECALL facts WHERE namespace = "statements.summaries" GROUP BY object.month SUM object.transaction_count }"#,
        &f,
    )
    .unwrap();
    assert_eq!(
        run(r#"RUN "by_month" ORDER BY key DESC LIMIT 3"#, &f),
        vec!["2027-10", "2027-09", "2027-08"]
    );
    assert_eq!(exec(r#"RUN "by_month" ORDER BY valid_from"#, &f).unwrap_err().code(), "CAL-E124");
}
