//! #165: the run index is paged, scoped by session namespace, and reports
//! its total — so a quiet tenant's runs (open approvals included) cannot
//! vanish behind a busier tenant's newest page, and a surface can say
//! "showing N of M" instead of implying N is all there is.

use areev_cal::AreevFacade;
use areev_core::authz::HARNESS_NS;
use areev_core::error::Hash;
use areev_core::types::{Fact, Grain, Tool, ToolKind, Workflow};
use areev_run::{
    ns_in_scope, BudgetsSpec, ExecResult, HostToolExecutor, OnDangling, RunOptions, RunSession,
    Runner, ScriptedClock,
};
use areev_run_core::RunOutcome;
use areev_store::Areev;
use serde_json::{json, Value};
use std::sync::Arc;
use tempfile::TempDir;

struct OkExec;

impl HostToolExecutor for OkExec {
    fn execute(&self, tool_name: &str, _h: &str, _in: &Value, _idem: &str) -> ExecResult {
        ExecResult::Ok(json!({tool_name.to_string(): true}))
    }
}

fn opts() -> RunOptions {
    RunOptions {
        budgets: BudgetsSpec::default(),
        ask_ttl_sec: None,
        workers: 2,
        on_dangling: OnDangling::Redispatch,
        llm_max_tokens: None,
        inject_crash: None,
    }
}

fn runner(facade: &Arc<AreevFacade>, ns: &str, t0: u64) -> Runner {
    Runner {
        facade: Arc::clone(facade),
        clock: Arc::new(ScriptedClock::new((0..400).map(|i| t0 + i * 10).collect())),
        executor: Arc::new(OkExec),
        llm: None,
        observer: None,
        ns: ns.into(),
        principal: "user:runner".into(),
    }
}

fn ids(page: &areev_run::RunListPage) -> Vec<&str> {
    page.runs.iter().map(|r| r.run_id.as_str()).collect()
}

#[test]
fn run_index_scopes_by_session_namespace_and_reports_truncation() {
    let dir = TempDir::new().unwrap();
    let m = Areev::open(dir.path().join("m.db").to_str().unwrap()).unwrap();
    let facade = Arc::new(AreevFacade::new(m));

    let mut wf = Workflow::new(vec!["a".into(), "b".into()]).edge("a", "b");
    for n in ["a", "b"] {
        let def = Tool::new(n)
            .kind(ToolKind::Definition)
            .tool_description("t")
            .created_at(500)
            .namespace("ops");
        let dh = facade.with_store(|m| m.add(&def)).unwrap();
        wf = wf.bind(n, &dh.to_hex());
    }
    let plan: Hash = facade
        .with_store(|m| m.add(&wf.created_at(600).namespace("ops")))
        .unwrap();

    // The shape that hid the bug: the quiet tenant runs FIRST (older), then
    // a busier tenant fills the newest page on its own.
    let quiet = runner(&facade, "tenant.quiet", 1_757_000_000_000);
    for id in ["q-1", "q-2", "q-3"] {
        let s = quiet.start(&plan, id, json!({}), &opts()).unwrap();
        assert!(matches!(s, RunSession::Finished { outcome: RunOutcome::Completed, .. }));
    }
    let busy = runner(&facade, "tenant.busy", 1_757_000_100_000);
    for id in ["b-1", "b-2", "b-3", "b-4", "b-5"] {
        let s = busy.start(&plan, id, json!({}), &opts()).unwrap();
        assert!(matches!(s, RunSession::Finished { outcome: RunOutcome::Completed, .. }));
    }

    // Unscoped: everything, with the session namespace on every row and
    // the outcome resolved in one pass.
    let all = busy.list_runs(None, 0, 100).unwrap();
    assert_eq!(all.total, 8);
    assert!(!all.truncated);
    assert_eq!(all.runs.len(), 8);
    assert!(all.runs.iter().all(|r| r.outcome == "completed"), "{all:?}");
    assert_eq!(
        all.runs.iter().filter(|r| r.ns.as_deref() == Some("tenant.quiet")).count(),
        3
    );
    assert_eq!(
        all.runs.iter().filter(|r| r.ns.as_deref() == Some("tenant.busy")).count(),
        5
    );

    // A page the size of the old fixed window, smaller than the busy
    // tenant's run count: it is ALL busy — a client-side filter over it
    // would show the quiet tenant as having no runs.
    let page = busy.list_runs(None, 0, 4).unwrap();
    assert_eq!(page.runs.len(), 4);
    assert_eq!(page.total, 8);
    assert!(page.truncated);
    assert!(
        page.runs.iter().all(|r| r.run_id.starts_with("b-")),
        "newest page is all busy: {:?}",
        ids(&page)
    );

    // Scoped on the server side, the quiet tenant is whole inside that
    // same page size.
    let q = busy.list_runs(Some("tenant.quiet"), 0, 4).unwrap();
    assert_eq!(q.total, 3);
    assert!(!q.truncated);
    let mut got = ids(&q);
    got.sort_unstable();
    assert_eq!(got, ["q-1", "q-2", "q-3"]);

    // A prefix scope covers both tenants; offset + limit page through it.
    let pre = busy.list_runs(Some("tenant.*"), 2, 3).unwrap();
    assert_eq!(pre.total, 8);
    assert_eq!(pre.runs.len(), 3);
    assert!(pre.truncated);
    assert_eq!((pre.offset, pre.limit), (2, 3));
    let last = busy.list_runs(Some("tenant.*"), 6, 3).unwrap();
    assert_eq!(last.runs.len(), 2);
    assert!(!last.truncated);

    // An unknown namespace is an empty answer, not a wrong one.
    let none = busy.list_runs(Some("tenant.nobody"), 0, 10).unwrap();
    assert_eq!((none.total, none.runs.len(), none.truncated), (0, 0, false));

    // `*` and `""` mean unscoped.
    assert_eq!(busy.list_runs(Some("*"), 0, 100).unwrap().total, 8);
    assert_eq!(busy.list_runs(Some(""), 0, 100).unwrap().total, 8);

    // A link written before the namespace stamp existed (no `run_ns` field)
    // is not KNOWN to be in any scope. A scoped read excludes it and counts
    // it; the unscoped read — the default — still shows it. Neither guesses.
    facade
        .with_store(|m| {
            let mut f = Fact::new("run:legacy", "mg:harness", "00")
                .namespace(HARNESS_NS)
                .created_at(1);
            f.common_mut().extra_fields.insert("run_id".into(), json!("legacy"));
            m.add(&f)
        })
        .unwrap();
    let q2 = busy.list_runs(Some("tenant.quiet"), 0, 10).unwrap();
    assert_eq!(q2.total, 3, "scoping stays exact: {q2:?}");
    assert_eq!(q2.unattributed, 1, "and says what it left out");
    assert!(q2.runs.iter().all(|r| r.run_id != "legacy"));
    // Scoping to a namespace nothing ran in is empty, not "every old run".
    let nobody = busy.list_runs(Some("tenant.nobody"), 0, 10).unwrap();
    assert_eq!((nobody.total, nobody.unattributed), (0, 1), "{nobody:?}");
    // Unscoped, it is present, unattributed, and has no outcome census.
    let all2 = busy.list_runs(None, 0, 100).unwrap();
    assert_eq!((all2.total, all2.unattributed), (9, 0));
    let legacy = all2.runs.iter().find(|r| r.run_id == "legacy").expect("legacy link listed");
    assert!(legacy.ns.is_none());
    assert_eq!(legacy.outcome, "open", "no outcome census for it");

    // `recent_runs` is the unscoped first page, ids only.
    let recent = busy.recent_runs(3).unwrap();
    assert_eq!(recent.len(), 3);
    assert!(recent.iter().all(|id| id.starts_with("b-")), "{recent:?}");
}

/// `recent_runs` reads a bounded window and widens it only when the window
/// held fewer runs than asked for. `agent:harness` also carries cancel, fork
/// and audit Facts, so the newest N Facts can contain NO run links at all —
/// the original defect (#165). The window must escalate past the dilution
/// rather than reporting an empty or short list, and it must still stop
/// early on an ordinary memory instead of reading the whole namespace.
#[test]
fn recent_runs_widens_its_window_past_harness_traffic() {
    let dir = TempDir::new().unwrap();
    let m = Areev::open(dir.path().join("m.db").to_str().unwrap()).unwrap();
    let facade = Arc::new(AreevFacade::new(m));
    let r = runner(&facade, "ops", 1_757_000_000_000);

    for i in 0..6 {
        facade
            .with_store(|m| {
                let mut f = Fact::new(&format!("run:r-{i}"), "mg:harness", "00")
                    .namespace(HARNESS_NS)
                    .created_at(1_000 + i);
                f.common_mut().extra_fields.insert("run_id".into(), json!(format!("r-{i}")));
                f.common_mut().extra_fields.insert("run_ns".into(), json!("ops"));
                m.add(&f)
            })
            .unwrap();
    }
    // 300 NEWER non-link Facts: the default window (max(64, limit*8)) now
    // holds nothing but these.
    for i in 0..300 {
        facade
            .with_store(|m| {
                let mut f = Fact::new(&format!("run:r-{}", i % 6), "mg:cancel", "operator stopped it")
                    .namespace(HARNESS_NS)
                    .created_at(2_000 + i);
                f.common_mut().extra_fields.insert("run_id".into(), json!(format!("r-{}", i % 6)));
                m.add(&f)
            })
            .unwrap();
    }

    let ids = r.recent_runs(6).unwrap();
    assert_eq!(ids.len(), 6, "escalation must see past the dilution: {ids:?}");
    let mut sorted = ids.clone();
    sorted.sort_unstable();
    assert_eq!(sorted, ["r-0", "r-1", "r-2", "r-3", "r-4", "r-5"]);

    // The full census agrees, and none of the diluting Facts is a run.
    let page = r.list_runs(None, 0, 100).unwrap();
    assert_eq!(page.total, 6);
    assert!(page.runs.iter().all(|row| row.outcome == "open"), "{page:?}");
}

/// The outcome record is a run's LAST `agent:harness` grain, behind one
/// Observation per brokered call, refusal, blob read and redelivery. A
/// single first-page read reported any run with more of those than the page
/// holds as open forever. `run_outcome` must page to exhaustion.
#[test]
fn outcome_is_found_behind_hundreds_of_per_call_harness_records() {
    let dir = TempDir::new().unwrap();
    let m = Areev::open(dir.path().join("m.db").to_str().unwrap()).unwrap();
    let facade = Arc::new(AreevFacade::new(m));
    let r = runner(&facade, "ops", 1_757_000_000_000);

    facade
        .with_store(|m| {
            let mut link = Fact::new("run:busy", "mg:harness", "00")
                .namespace(HARNESS_NS)
                .created_at(1_000);
            link.common_mut().extra_fields.insert("run_id".into(), json!("busy"));
            link.common_mut().extra_fields.insert("run_ns".into(), json!("ops"));
            m.add(&link)?;
            // 700 per-call records — more than one page (512) — BEFORE the
            // outcome, exactly as the broker journals them.
            for i in 0..700u64 {
                let mut obs = areev_core::types::Observation::new("user:runner", "system")
                    .subject("run:busy")
                    .object("https://api.example.com/page")
                    .namespace(HARNESS_NS)
                    .created_at(2_000 + i as i64);
                let ex = &mut obs.common.extra_fields;
                ex.insert("run_id".into(), json!("busy"));
                ex.insert("observation_kind".into(), json!("egress_call"));
                ex.insert("page".into(), json!(i));
                m.add(&obs)?;
            }
            let mut done = areev_core::types::Observation::new("user:runner", "system")
                .subject("run:busy")
                .object("completed")
                .namespace(HARNESS_NS)
                .created_at(9_000);
            let ex = &mut done.common.extra_fields;
            ex.insert("run_id".into(), json!("busy"));
            ex.insert("observation_kind".into(), json!("run_outcome"));
            ex.insert("spent_input_tokens".into(), json!(40));
            ex.insert("spent_output_tokens".into(), json!(2));
            ex.insert("spent_usd_micros".into(), json!(1234));
            m.add(&done)
        })
        .unwrap();

    let page = r.list_runs(None, 0, 10).unwrap();
    let row = page.runs.iter().find(|x| x.run_id == "busy").expect("listed");
    assert_eq!(row.outcome, "completed", "{row:?}");
    assert_eq!(row.spent_tokens, Some(42));
    assert_eq!(row.spent_usd_micros, Some(1234));
}

#[test]
fn ns_scope_matching_is_exact_or_dotted_prefix() {
    assert!(ns_in_scope("ops", "ops"));
    assert!(!ns_in_scope("ops", "ops.eu"));
    assert!(ns_in_scope("org.*", "org"));
    assert!(ns_in_scope("org.*", "org.eu"));
    assert!(ns_in_scope("org.*", "org.eu.fr"));
    assert!(!ns_in_scope("org.*", "organic"));
    assert!(!ns_in_scope("org.*", "other"));
}
