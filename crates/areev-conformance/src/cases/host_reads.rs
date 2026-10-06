//! Host reads added together (#368, #369, #370), on both backends:
//! - #368 — `SUM`/`MIN`/`MAX`/`AVG`, `GROUP BY` and `ORDER BY` on a dotted
//!   path, and a `max_limit` above 1,000 honoured only on a read-only handle;
//! - #369 — a read-only mount opened by its locator (a file path or a
//!   postgres DSN), read through CAL, never written through;
//! - #370 — a plan's `op: query` read, pinned to the saved query's body at
//!   run start.
//!
//! The aggregates are computed over grains the backend returns, so the case
//! pins what each backend must hand the executor: the full matching set, and
//! a Fact object stored as a JSON document that the dotted path can navigate.

use crate::Backend;
use areev_cal::executor::CalResultPayload;
use areev_cal::{AreevFacade, CalExecutor, CalExecutorConfig};
use areev_core::types::{Fact, Grain, Tool, ToolKind, Workflow};
use serde_json::{json, Value};
use std::sync::{Arc, OnceLock};

const LEDGER: &str = "ledger.tx";

fn txn(i: usize, counterparty: &str, amount_minor: i64) -> Fact {
    let object = json!({"amount_minor": amount_minor, "counterparty": counterparty}).to_string();
    Fact::new(&format!("txn-{i}"), "transaction", &object)
        .namespace(LEDGER)
        .created_at(1_000 + i as i64)
}

fn cal(facade: &AreevFacade, q: &str) -> areev_cal::executor::CalExecResult {
    CalExecutor::new(CalExecutorConfig::default()).execute(q, facade).unwrap()
}

/// #368: the three reads a ledger host needs, answered by CAL alone.
pub fn cal_aggregates_over_dotted_paths(b: &dyn Backend) {
    let mut m = b.open_named("cal_aggregates");
    for (i, (cp, amt)) in [("ACME", 1200), ("ACME", 300), ("ACME", 4500), ("Globex", 800), ("Globex", 50)]
        .into_iter()
        .enumerate()
    {
        m.add(&txn(i, cp, amt)).unwrap();
    }
    let facade = AreevFacade::with_session(m, Some(LEDGER.into()), None);
    let base = format!(r#"RECALL facts WHERE namespace = "{LEDGER}""#);

    let sum = cal(&facade, &format!(r#"{base} AND object.counterparty = "ACME" | SUM object.amount_minor"#));
    match sum.result {
        CalResultPayload::Aggregate { value, counted, .. } => {
            assert_eq!(value, json!(6000), "[{}] SUM is an exact integer", b.name());
            assert!(value.is_i64(), "[{}]", b.name());
            assert_eq!(counted, 3, "[{}]", b.name());
        }
        other => panic!("[{}] expected Aggregate, got {other:?}", b.name()),
    }

    let grouped = cal(&facade, &format!("{base} GROUP BY object.counterparty | SUM object.amount_minor"));
    let CalResultPayload::GroupAggregates { groups, total_available, .. } = grouped.result else {
        panic!("[{}] expected GroupAggregates", b.name());
    };
    assert_eq!(total_available, Some(2), "[{}]", b.name());
    let rows: Vec<(String, i64)> = groups
        .iter()
        .map(|g| (g.fields["key"].as_str().unwrap().into(), g.fields["value"].as_i64().unwrap()))
        .collect();
    assert_eq!(rows, vec![("ACME".into(), 6000), ("Globex".into(), 850)], "[{}]", b.name());

    let top = cal(&facade, &format!("{base} ORDER BY object.amount_minor DESC LIMIT 2"));
    let CalResultPayload::Grains { grains, .. } = top.result else {
        panic!("[{}] expected Grains", b.name());
    };
    let subjects: Vec<&str> = grains.iter().map(|g| g.fields["subject"].as_str().unwrap()).collect();
    assert_eq!(subjects, vec!["txn-2", "txn-0"], "[{}] numeric, largest first", b.name());
}

/// #368: a scan window above 1,000 is honoured only on a read-only handle,
/// and `CAL-W015` names the window that applied.
pub fn max_limit_above_default_applies_only_read_only(b: &dyn Backend) {
    let name = "cal_max_limit";
    {
        let mut m = b.open_named(name);
        for i in 0..1_010 {
            m.add(&txn(i, "ACME", 1)).unwrap();
        }
    }
    let q = format!(r#"RECALL facts WHERE namespace = "{LEDGER}" | SUM object.amount_minor"#);
    let raised = CalExecutor::new(CalExecutorConfig { max_limit: 2_000, ..Default::default() });
    let value = |res: &areev_cal::executor::CalExecResult| match &res.result {
        CalResultPayload::Aggregate { value, .. } => value.clone(),
        other => panic!("expected Aggregate, got {other:?}"),
    };
    {
        let facade = AreevFacade::with_session(b.open_named(name), Some(LEDGER.into()), None);
        let res = raised.execute(&q, &facade).unwrap();
        assert_eq!(value(&res), json!(1000), "[{}] a read-write handle is clamped", b.name());
        assert!(
            res.warnings.iter().any(|w| w.starts_with("CAL-W015") && w.contains("max_limit, 1000")),
            "[{}] {:?}",
            b.name(),
            res.warnings
        );
    }
    let ro = b.open_named_with(
        name,
        areev_store::AreevOptions { read_only: true, ..Default::default() },
    );
    let facade = AreevFacade::with_session(ro, Some(LEDGER.into()), None);
    let res = raised.execute(&q, &facade).unwrap();
    assert_eq!(value(&res), json!(1010), "[{}] read-only sees the whole set", b.name());
}

/// #377: `ORDER BY created_at` is pushed into the backend's sort, and the
/// `LIMIT` after it is a pipeline stage — so the scan must be sized by that
/// stage, not by the 50-row default page it used to run over. Above
/// `max_limit` the answer is bounded and `CAL-W015` says so.
pub fn order_by_created_at_honors_a_pipeline_limit(b: &dyn Backend) {
    let mut m = b.open_named("cal_order_by_limit");
    for i in 0..120 {
        m.add(&txn(i, "ACME", 1)).unwrap();
    }
    let facade = AreevFacade::with_session(m, Some(LEDGER.into()), None);
    let base = format!(r#"RECALL facts WHERE namespace = "{LEDGER}" AND relation = "transaction""#);
    let subjects = |res: &areev_cal::executor::CalExecResult| -> Vec<String> {
        let CalResultPayload::Grains { grains, .. } = &res.result else {
            panic!("[{}] expected Grains, got {:?}", b.name(), res.result);
        };
        grains.iter().map(|g| g.fields["subject"].as_str().unwrap().to_string()).collect()
    };
    for (n, want) in [(49, 49), (50, 50), (51, 51), (100, 100), (500, 120)] {
        let res = cal(&facade, &format!("{base} ORDER BY created_at DESC LIMIT {n} FORMAT json"));
        let got = subjects(&res);
        assert_eq!(got.len(), want, "[{}] LIMIT {n}", b.name());
        assert_eq!(got[0], "txn-119", "[{}] LIMIT {n} is newest first", b.name());
        assert_eq!(got[want - 1], format!("txn-{}", 120 - want), "[{}] LIMIT {n}", b.name());
        assert!(res.warnings.iter().all(|w| !w.starts_with("CAL-W015")), "[{}] {:?}", b.name(), res.warnings);
    }
    // Ascending, and an OFFSET ahead of the LIMIT, size the scan the same way.
    let asc = subjects(&cal(&facade, &format!("{base} ORDER BY created_at ASC | OFFSET 60 | LIMIT 55")));
    assert_eq!(asc.len(), 55, "[{}]", b.name());
    assert_eq!((asc[0].as_str(), asc[54].as_str()), ("txn-60", "txn-114"), "[{}]", b.name());
    // A COUNT after the pushed-down sort counts the whole set, not a page.
    let count = cal(&facade, &format!("{base} ORDER BY created_at DESC | COUNT"));
    match count.result {
        CalResultPayload::Count { count } => assert_eq!(count, 120, "[{}]", b.name()),
        other => panic!("[{}] expected Count, got {other:?}", b.name()),
    }

    // Past the ceiling the answer is bounded, and says so.
    let small = CalExecutor::new(CalExecutorConfig { max_limit: 100, ..Default::default() });
    let res = small.execute(&format!("{base} ORDER BY created_at DESC LIMIT 500"), &facade).unwrap();
    assert_eq!(subjects(&res).len(), 100, "[{}]", b.name());
    assert!(
        res.warnings.iter().any(|w| w.starts_with("CAL-W015") && w.contains("100")),
        "[{}] {:?}",
        b.name(),
        res.warnings
    );
}

/// #385: non-matching rows must not consume a date-ordered result page.
pub fn order_by_created_at_filters_before_limit(b: &dyn Backend) {
    let mut m = b.open_named("cal_filtered_order");
    for i in 0..4 {
        m.add(&Fact::new(&format!("s{i}"), "r", "o").namespace("a.ops").created_at(100 + i)).unwrap();
    }
    // More than the old default page, on both sides of the matching rows.
    for i in 0..60 {
        for stamp in [i, 200 + i] {
            m.add(&Fact::new(&format!("noise-{stamp}"), "note", "other")
                .namespace("a.ops").created_at(stamp)).unwrap();
        }
    }
    // A superseded version newer than all matching heads must not take a slot.
    let old = m.add(&Fact::new("versioned", "r", "o").namespace("a.ops").created_at(400)).unwrap();
    let mut new = Fact::new("versioned", "r", "o").namespace("a.ops").created_at(99);
    m.supersede(&old, &mut new).unwrap();
    m.add(&Fact::new("sibling-noise", "note", "other").namespace("a.other").created_at(500)).unwrap();
    let facade = AreevFacade::with_session(m, Some("a.ops".into()), None);
    let subjects = |res: &areev_cal::executor::CalExecResult| -> Vec<String> {
        let CalResultPayload::Grains { grains, .. } = &res.result else { panic!("expected grains"); };
        grains.iter().map(|g| g.fields["subject"].as_str().unwrap().to_string()).collect()
    };
    for predicate in [r#"relation = "r""#, r#"relation IN ("r")"#, r#"object = "o""#, r#"object IN ("o")"#] {
        let base = format!(r#"RECALL facts WHERE namespace = "a.ops" AND {predicate}"#);
        for (tail, expected) in [
            ("ORDER BY created_at DESC LIMIT 2", vec!["s3", "s2"]),
            ("ORDER BY created_at DESC LIMIT 4", vec!["s3", "s2", "s1", "s0"]),
            ("ORDER BY created_at ASC LIMIT 2", vec!["versioned", "s0"]),
            ("ORDER BY created_at DESC | OFFSET 2 | LIMIT 2", vec!["s1", "s0"]),
            ("ORDER BY created_at DESC | FIRST", vec!["s3"]),
        ] {
            let q = format!("{base} {tail}");
            let res = cal(&facade, &q);
            assert_eq!(subjects(&res), expected, "[{}] {q}", b.name());
            assert!(!res.warnings.iter().any(|w| w.starts_with("CAL-W015")), "{:?}", res.warnings);
        }
        // The ceiling measures candidates BEFORE the facade drops non-matches.
        let small = CalExecutor::new(CalExecutorConfig { max_limit: 20, ..Default::default() });
        let res = small.execute(&format!("{base} ORDER BY created_at DESC LIMIT 2"), &facade).unwrap();
        assert!(subjects(&res).is_empty());
        assert!(res.warnings.iter().any(|w| w.starts_with("CAL-W015") && w.contains("20")), "{:?}", res.warnings);
    }
    let history = cal(&facade, r#"RECALL facts WHERE relation = "r" ORDER BY created_at DESC LIMIT 2 WITH superseded"#);
    assert_eq!(subjects(&history), vec!["versioned", "s3"]);
    for scope in [r#"namespace IN ("a.ops", "a.other")"#, r#"namespace = "a.*""#] {
        let res = cal(&facade, &format!(r#"RECALL facts WHERE {scope} AND relation = "r" ORDER BY created_at DESC LIMIT 2"#));
        assert_eq!(subjects(&res), vec!["s3", "s2"], "[{}] {scope}", b.name());
    }
    // The diagnostic read must retain PrincipalSession's authorization scope.
    facade.set_grants("reader", &[areev_core::authz::Grant {
        verbs: vec![areev_core::authz::Verb::Read], namespaces: vec!["a.ops".into()],
    }], "regression test").unwrap();
    let session = facade.principal_session("reader").unwrap();
    let executor = CalExecutor::new(CalExecutorConfig { max_limit: 20, ..Default::default() });
    let res = executor.execute(r#"RECALL facts WHERE namespace = "a.ops" AND relation = "r" ORDER BY created_at DESC LIMIT 2"#, &session).unwrap();
    assert!(subjects(&res).is_empty());
    assert!(res.warnings.iter().any(|w| w.starts_with("CAL-W015")));
    assert!(executor.execute(r#"RECALL facts WHERE namespace = "a.other" ORDER BY created_at DESC LIMIT 2"#, &session).is_err());
}

/// #369: a mount opened by LOCATOR — a file on the embedded backend, a DSN
/// on postgres — is read through CAL and refuses a write addressed to it.
pub fn mount_by_locator_reads_and_refuses_writes(b: &dyn Backend) {
    {
        let mut other = b.open_named("mount_target");
        other.add(&Fact::new("refunds", "window_days", "45").namespace("policies")).unwrap();
    }
    let primary = b.open_named("mount_primary");
    let mut facade = AreevFacade::with_session(primary, Some("caller".into()), None);
    facade
        .mount_read_only("org", &b.locator("mount_target"), Some(&b.locator("mount_primary")))
        .unwrap();
    // Mounting the primary itself is refused on every backend (postgres by
    // the locator check, the embedded backend by its one-handle rule).
    assert!(
        facade.mount_read_only("self", &b.locator("mount_primary"), Some(&b.locator("mount_primary"))).is_err(),
        "[{}] mounting the primary is refused",
        b.name()
    );

    let res = cal(&facade, r#"RECALL facts WHERE namespace = "org.policies""#);
    let CalResultPayload::Grains { grains, .. } = res.result else {
        panic!("[{}] expected Grains", b.name());
    };
    assert_eq!(grains.len(), 1, "[{}]", b.name());

    let before = facade.with_store(|m| m.count().unwrap());
    let add = cal(
        &facade,
        r#"ADD fact SET namespace = "org.policies" SET subject = "refunds" SET relation = "window_days" SET object = "1" REASON "t""#,
    );
    let CalResultPayload::Unsupported { message, .. } = add.result else {
        panic!("[{}] a write through a mount must be refused", b.name());
    };
    assert!(message.contains("STO-E004"), "[{}] {message}", b.name());
    assert_eq!(facade.with_store(|m| m.count().unwrap()), before, "[{}]", b.name());
}

struct Redefine {
    facade: OnceLock<Arc<AreevFacade>>,
}

impl areev_run::HostToolExecutor for Redefine {
    fn execute(&self, tool: &str, _h: &str, _i: &Value, _k: &str) -> areev_run::ExecResult {
        if tool == "prep" {
            let f = self.facade.get().expect("facade set before the run");
            CalExecutor::new(CalExecutorConfig::default())
                .execute(
                    r#"DEFINE QUERY "by_cp"($cp) AS { RECALL facts WHERE relation = "paid_to" AND object = "Globex" }"#,
                    &**f,
                )
                .unwrap();
        }
        areev_run::ExecResult::Ok(json!({ format!("{tool}_done"): true }))
    }
}

/// #370: `op: query` binds the saved query's parameters, is journaled with
/// the body hash, replays from the journal — and reads the body pinned at run
/// start although a tool redefines the query mid-run.
pub fn run_query_reads_the_body_pinned_at_start(b: &dyn Backend) {
    let mut m = b.open_named("run_query");
    for (i, cp) in ["ACME", "ACME", "Globex"].iter().enumerate() {
        m.add(&Fact::new(&format!("txn-{i}"), "paid_to", cp).namespace("ops").created_at(1_000 + i as i64))
            .unwrap();
    }
    let prep = m
        .add(&Tool::new("prep").kind(ToolKind::Definition).tool_description("c").namespace("ops").created_at(500))
        .unwrap();
    let mut wf = Workflow::new(vec!["prep".into(), "rows".into()])
        .edge("prep", "rows")
        .bind("prep", &prep.to_hex())
        .namespace("ops")
        .created_at(600);
    wf.common.extra_fields.insert(
        "reads".into(),
        json!({"rows": {"op": "query", "name": "by_cp", "params_from": {"cp": "/cp"}}}),
    );
    let plan = m.add(&wf).unwrap();
    let facade = Arc::new(AreevFacade::with_session(m, Some("ops".into()), None));
    cal(
        &facade,
        r#"DEFINE QUERY "by_cp"($cp) AS { RECALL facts WHERE relation = "paid_to" AND object = $cp }"#,
    );
    let pinned_hash = areev_cal::queries::query_body_hash(
        &areev_cal::CalStoreFacade::get_query(&*facade, "by_cp").unwrap().body,
    );
    let exec = Arc::new(Redefine { facade: OnceLock::new() });
    let _ = exec.facade.set(Arc::clone(&facade));
    let runner = areev_run::Runner {
        facade: Arc::clone(&facade),
        clock: Arc::new(areev_run::ScriptedClock::new(
            (0..200).map(|i| 1_785_000_000_000 + i * 10).collect(),
        )),
        executor: exec,
        llm: None,
        observer: None,
        ns: "ops".into(),
        principal: "user:conformance".into(),
    };
    let opts = areev_run::RunOptions { workers: 1, ..Default::default() };
    runner.start(&plan, "rq", json!({"cp": "ACME"}), &opts).unwrap();

    let read = facade
        .with_store(|m| m.run_trace("ops", "rq", 1024))
        .unwrap()
        .into_iter()
        .find(|g| g.get_str("tool_name") == Some("mg:query") && g.fields.contains_key("read"))
        .expect("the read's result grain");
    let record = &read.fields["read"];
    assert_eq!(record["body_hash"], json!(pinned_hash), "[{}]", b.name());
    assert_eq!(record["params"], json!({"cp": "ACME"}), "[{}]", b.name());
    let content: Value = serde_json::from_str(read.get_str("tool_content").unwrap()).unwrap();
    let mut subjects: Vec<&str> = content["rows"]["grains"]
        .as_array()
        .unwrap()
        .iter()
        .map(|g| g["fields"]["subject"].as_str().unwrap())
        .collect();
    subjects.sort();
    assert_eq!(subjects, vec!["txn-0", "txn-1"], "[{}] the pinned body, not the redefined one", b.name());
    assert!(runner.verify("rq").unwrap().verified, "[{}] replays from the journal", b.name());
}

/// #373: `RUN` of a saved query answers exactly what its body answers inline
/// — the body's aggregate, `COUNT`, `GROUP BY … SUM` and `ORDER BY … LIMIT`
/// all run, and its `FORMAT` renders — on a default handle and on a
/// read-only one with a raised `max_limit`; a call-site stage composes after
/// the body's, in one pass (a body's `GROUP BY` stays open for it).
pub fn run_applies_the_saved_body_stages(b: &dyn Backend) {
    let name = "cal_run_stages";
    {
        let mut m = b.open_named(name);
        // 1,200 transactions, 800 of them ACME: past the default scan window
        // (1,000), so the read-only handle's raised max_limit is load-bearing.
        for i in 0..1_200 {
            let cp = if i % 3 == 2 { "Globex" } else { "ACME" };
            m.add(&txn(i, cp, i as i64 + 1)).unwrap();
        }
        let facade = AreevFacade::with_session(m, Some(LEDGER.into()), None);
        for def in [
            format!(r#"DEFINE QUERY "total"($cp) AS {{ RECALL facts WHERE namespace = "{LEDGER}" AND object.counterparty = $cp | SUM object.amount_minor }}"#),
            format!(r#"DEFINE QUERY "n"() AS {{ RECALL facts WHERE namespace = "{LEDGER}" | COUNT }}"#),
            format!(r#"DEFINE QUERY "by_cp"() AS {{ RECALL facts WHERE namespace = "{LEDGER}" GROUP BY object.counterparty | SUM object.amount_minor }}"#),
            format!(r#"DEFINE QUERY "latest"($cp, $limit) AS {{ RECALL facts WHERE namespace = "{LEDGER}" AND object.counterparty = $cp ORDER BY object.amount_minor DESC LIMIT $limit FORMAT json }}"#),
            format!(r#"DEFINE QUERY "rows_by_cp"() AS {{ RECALL facts WHERE namespace = "{LEDGER}" GROUP BY object.counterparty }}"#),
            format!(r#"DEFINE QUERY "top_md"() AS {{ RECALL facts WHERE namespace = "{LEDGER}" ORDER BY object.amount_minor DESC LIMIT 3 FORMAT markdown }}"#),
        ] {
            cal(&facade, &def);
        }
    }

    let base = format!(r#"RECALL facts WHERE namespace = "{LEDGER}""#);
    let pairs = [
        (r#"RUN "total"($cp = "ACME")"#.to_string(), format!(r#"{base} AND object.counterparty = "ACME" | SUM object.amount_minor"#)),
        (r#"RUN "n"()"#.to_string(), format!("{base} | COUNT")),
        (r#"RUN "by_cp"()"#.to_string(), format!("{base} GROUP BY object.counterparty | SUM object.amount_minor")),
        (
            r#"RUN "latest"($cp = "ACME", $limit = 5)"#.to_string(),
            format!(r#"{base} AND object.counterparty = "ACME" ORDER BY object.amount_minor DESC LIMIT 5 FORMAT json"#),
        ),
        // A body's bare GROUP BY stays open for the call site's aggregate.
        (
            r#"RUN "rows_by_cp"() SUM object.amount_minor"#.to_string(),
            format!("{base} GROUP BY object.counterparty | SUM object.amount_minor"),
        ),
        // The body's FORMAT renders when the call site names none.
        (r#"RUN "top_md"()"#.to_string(), format!("{base} ORDER BY object.amount_minor DESC LIMIT 3 FORMAT markdown")),
    ];
    let payload = |r: &areev_cal::executor::CalExecResult| serde_json::to_value(&r.result).unwrap();
    let subjects = |r: &areev_cal::executor::CalExecResult| match &r.result {
        CalResultPayload::Grains { grains, .. } => grains
            .iter()
            .map(|g| g.fields["subject"].as_str().unwrap().to_string())
            .collect::<Vec<_>>(),
        other => panic!("[{}] expected Grains, got {other:?}", b.name()),
    };

    let check = |facade: &AreevFacade, ex: &CalExecutor, handle: &str, expect_n: usize, expect_sum: i64| {
        for (run, inline) in &pairs {
            let ran = ex.execute(run, facade).unwrap();
            let direct = ex.execute(inline, facade).unwrap();
            assert_eq!(payload(&ran), payload(&direct), "[{}/{handle}] {run} vs inline", b.name());
        }
        let n = ex.execute(r#"RUN "n"()"#, facade).unwrap();
        assert!(
            matches!(n.result, CalResultPayload::Count { count } if count == expect_n),
            "[{}/{handle}] {:?}",
            b.name(),
            n.result
        );
        let total = ex.execute(r#"RUN "total"($cp = "ACME")"#, facade).unwrap();
        assert!(
            matches!(&total.result, CalResultPayload::Aggregate { value, .. } if *value == json!(expect_sum)),
            "[{}/{handle}] {:?}",
            b.name(),
            total.result
        );
        let latest = ex.execute(r#"RUN "latest"($cp = "ACME", $limit = 5)"#, facade).unwrap();
        assert_eq!(subjects(&latest).len(), 5, "[{}/{handle}] the body's LIMIT bounds RUN", b.name());
        // A call-site stage composes AFTER the body's: the body's top 5, then 2.
        let capped = ex.execute(r#"RUN "latest"($cp = "ACME", $limit = 5) LIMIT 2"#, facade).unwrap();
        assert_eq!(subjects(&capped), subjects(&latest)[..2].to_vec(), "[{}/{handle}]", b.name());
        let summed = ex
            .execute(r#"RUN "latest"($cp = "ACME", $limit = 5) SUM object.amount_minor"#, facade)
            .unwrap();
        assert!(
            matches!(&summed.result, CalResultPayload::Aggregate { counted: 5, .. }),
            "[{}/{handle}] a call-site SUM reads the body's 5 rows: {:?}",
            b.name(),
            summed.result
        );
    };

    {
        let facade = AreevFacade::with_session(b.open_named(name), Some(LEDGER.into()), None);
        let ex = CalExecutor::new(CalExecutorConfig::default());
        // The default window (1,000) sees the newest 1,000 rows — ids 200..1200.
        let (n, sum) = (1_000, (200..1_200).filter(|i| i % 3 != 2).map(|i| i as i64 + 1).sum());
        check(&facade, &ex, "default", n, sum);
    }
    let ro = b.open_named_with(name, areev_store::AreevOptions { read_only: true, ..Default::default() });
    let facade = AreevFacade::with_session(ro, Some(LEDGER.into()), None);
    let ex = CalExecutor::new(CalExecutorConfig { max_limit: 20_000, ..Default::default() });
    let sum = (0..1_200).filter(|i| i % 3 != 2).map(|i| i as i64 + 1).sum();
    check(&facade, &ex, "read-only", 1_200, sum);
}
