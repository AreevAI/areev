//! `op: query` (#370): a plan node reads a saved query with parameters, and
//! the run reads the body PINNED at run start — a body redefined or dropped
//! mid-run does not change a running run.

use areev_cal::{AreevFacade, CalExecutor, CalExecutorConfig};
use areev_core::types::{Fact, Grain, Tool, ToolKind, Workflow};
use areev_run::{ExecResult, HostToolExecutor, RunOptions, Runner, ScriptedClock};
use areev_store::Areev;
use serde_json::{json, Value};
use std::sync::{Arc, OnceLock};
use tempfile::TempDir;

const NS: &str = "ops";

/// Runs `prep` by redefining the saved query the NEXT node reads — the
/// mid-run supersession the pin exists for. Every other tool just succeeds.
struct Redefiner {
    facade: OnceLock<Arc<AreevFacade>>,
    redefine_to: Option<&'static str>,
}

impl HostToolExecutor for Redefiner {
    fn execute(&self, tool: &str, _h: &str, _i: &Value, _k: &str) -> ExecResult {
        if tool == "prep" {
            if let (Some(f), Some(stmt)) = (self.facade.get(), self.redefine_to) {
                let ex = CalExecutor::new(CalExecutorConfig::default());
                let res = ex.execute(stmt, &**f).unwrap();
                assert!(
                    !matches!(res.result, areev_cal::executor::CalResultPayload::Unsupported { .. }),
                    "{:?}",
                    res.result
                );
            }
        }
        ExecResult::Ok(json!({ format!("{tool}_done"): true }))
    }
}

const BY_COUNTERPARTY: &str = r#"DEFINE QUERY "by_counterparty"($counterparty)
  DESCRIPTION "transactions with one counterparty"
AS { RECALL facts WHERE relation = "paid_to" AND object = $counterparty }"#;

struct Rig {
    _dir: TempDir,
    facade: Arc<AreevFacade>,
    exec: Arc<Redefiner>,
}

impl Rig {
    fn new(redefine_to: Option<&'static str>) -> Self {
        let dir = TempDir::new().unwrap();
        let mut m = Areev::open(dir.path().join("m.db").to_str().unwrap()).unwrap();
        for (i, cp) in ["ACME", "ACME", "Globex"].iter().enumerate() {
            m.add(&Fact::new(&format!("txn-{i}"), "paid_to", cp).namespace(NS).created_at(1_000 + i as i64))
                .unwrap();
        }
        // A grain in another namespace the saved query would match if it
        // could leave the run's namespace.
        m.add(&Fact::new("txn-x", "paid_to", "ACME").namespace("elsewhere")).unwrap();
        let facade = Arc::new(AreevFacade::with_session(m, Some(NS.into()), None));
        let ex = CalExecutor::new(CalExecutorConfig::default());
        ex.execute(BY_COUNTERPARTY, &*facade).unwrap();
        let exec = Arc::new(Redefiner { facade: OnceLock::new(), redefine_to });
        let _ = exec.facade.set(Arc::clone(&facade));
        Rig { _dir: dir, facade, exec }
    }

    fn runner(&self) -> Runner {
        Runner {
            facade: Arc::clone(&self.facade),
            clock: Arc::new(ScriptedClock::new(
                (0..400).map(|i| 1_785_000_000_000 + i * 10).collect(),
            )),
            executor: Arc::clone(&self.exec) as Arc<dyn HostToolExecutor>,
            llm: None,
            observer: None,
            ns: NS.into(),
            principal: "user:test".into(),
        }
    }

    /// prep → rows (the read) → close.
    fn plan(&self, read: Value) -> areev_core::error::Hash {
        self.facade
            .with_store(|m| {
                let mut def = |name: &str, at: i64| {
                    m.add(
                        &Tool::new(name)
                            .kind(ToolKind::Definition)
                            .tool_description("test tool")
                            .namespace(NS)
                            .created_at(at),
                    )
                };
                let prep = def("prep", 500)?;
                let close = def("close", 501)?;
                let mut wf = Workflow::new(vec!["prep".into(), "rows".into(), "close".into()])
                    .edge("prep", "rows")
                    .edge("rows", "close")
                    .bind("prep", &prep.to_hex())
                    .bind("close", &close.to_hex())
                    .namespace(NS)
                    .created_at(600);
                wf.common.extra_fields.insert("reads".into(), json!({ "rows": read }));
                m.add(&wf)
            })
            .unwrap()
    }

    fn read_grain(&self, run_id: &str) -> areev_core::format::DeserializedGrain {
        self.facade
            .with_store(|m| m.run_trace(NS, run_id, 1024))
            .unwrap()
            .into_iter()
            .find(|g| g.get_str("tool_name") == Some("mg:query") && g.fields.contains_key("read"))
            .expect("the read's result grain")
    }
}

fn body_of(rig: &Rig) -> String {
    areev_cal::CalStoreFacade::get_query(&*rig.facade, "by_counterparty").unwrap().body
}

fn opts() -> RunOptions {
    RunOptions { workers: 1, ..Default::default() }
}

fn subjects(content: &Value) -> Vec<String> {
    let mut v: Vec<String> = content["rows"]["grains"]
        .as_array()
        .unwrap()
        .iter()
        .map(|g| g["fields"]["subject"].as_str().unwrap().to_string())
        .collect();
    v.sort();
    v
}

#[test]
fn a_query_read_binds_params_and_is_journaled_with_its_body_hash() {
    let rig = Rig::new(None);
    let plan = rig.plan(json!({"op": "query", "name": "by_counterparty",
                               "params_from": {"counterparty": "/cp"}}));
    let runner = rig.runner();
    runner.start(&plan, "q1", json!({"cp": "ACME"}), &opts()).unwrap();

    let read = rig.read_grain("q1");
    let content: Value = serde_json::from_str(read.get_str("tool_content").unwrap()).unwrap();
    // Two ACME rows in the run's namespace — not Globex, and not the ACME row
    // in "elsewhere": the read is pinned to the run's namespace.
    assert_eq!(subjects(&content), vec!["txn-0", "txn-1"]);
    let record = &read.fields["read"];
    assert_eq!(record["op"], "query");
    assert_eq!(record["name"], "by_counterparty");
    assert_eq!(record["params"], json!({"counterparty": "ACME"}));
    assert_eq!(record["grains"].as_array().map(Vec::len), Some(2));
    let body = body_of(&rig);
    assert_eq!(record["body_hash"], json!(areev_cal::queries::query_body_hash(&body)));
    assert!(runner.verify("q1").unwrap().verified, "the read replays from the journal");
}

/// The acceptance case: `prep` redefines the saved query while the run is in
/// flight, and the read still answers from the body pinned at start.
#[test]
fn a_body_redefined_mid_run_does_not_change_the_running_run() {
    let rig = Rig::new(Some(
        r#"DEFINE QUERY "by_counterparty"($counterparty) AS { RECALL facts WHERE relation = "paid_to" AND object = "Globex" }"#,
    ));
    let plan = rig.plan(json!({"op": "query", "name": "by_counterparty",
                               "params": {"counterparty": "ACME"}}));
    let original = areev_cal::queries::query_body_hash(&body_of(&rig));
    rig.runner().start(&plan, "pinned", json!({}), &opts()).unwrap();

    // The row did change mid-run…
    let now = body_of(&rig);
    assert!(now.contains("Globex"), "prep redefined the query: {now}");
    // …and the run read the ORIGINAL body.
    let read = rig.read_grain("pinned");
    let content: Value = serde_json::from_str(read.get_str("tool_content").unwrap()).unwrap();
    assert_eq!(subjects(&content), vec!["txn-0", "txn-1"]);
    assert_eq!(read.fields["read"]["body_hash"], json!(original));
}

#[test]
fn a_query_that_is_missing_or_misbound_refuses_at_start() {
    let rig = Rig::new(None);
    let runner = rig.runner();
    let cases = [
        (json!({"op": "query", "name": "nope"}), "no saved query has that name"),
        (json!({"op": "query", "name": "by_counterparty"}), "required parameter `counterparty`"),
        (
            json!({"op": "query", "name": "by_counterparty",
                   "params": {"counterparty": "ACME", "cp": "x"}}),
            "declares no parameter `cp`",
        ),
    ];
    for (i, (read, want)) in cases.into_iter().enumerate() {
        let plan = rig.plan(read);
        let run_id = format!("bad-{i}");
        let err = runner.start(&plan, &run_id, json!({}), &opts()).unwrap_err();
        assert_eq!(err.code(), "RUN-E031", "{err}");
        assert!(err.to_string().contains(want), "{err}");
        assert!(runner.inspect(&run_id).is_err(), "no run left behind");
    }
    // Malformed declarations are RUN-E019, like every other read.
    for read in [
        json!({"op": "query"}),
        json!({"op": "query", "name": "by_counterparty", "params": {"counterparty": {"a": 1}}}),
        json!({"op": "query", "name": "by_counterparty", "params_from": {"counterparty": "cp"}}),
        json!({"op": "query", "name": "by_counterparty", "params": {"counterparty": "A"},
               "params_from": {"counterparty": "/cp"}}),
        json!({"op": "query", "name": "by_counterparty", "k": 3}),
    ] {
        let plan = rig.plan(read.clone());
        let err = runner.start(&plan, "malformed", json!({}), &opts()).unwrap_err();
        assert_eq!(err.code(), "RUN-E019", "{read}: {err}");
    }
}

/// A pin that no longer matches its hash (a hand-edited manifest) fails the
/// node with RUN-E031 instead of reading whatever the manifest now says.
#[test]
fn a_tampered_pin_fails_the_read_with_run_e031() {
    let rig = Rig::new(None);
    let spec = json!({"op": "query", "ns": NS, "into": "rows", "name": "by_counterparty",
        "params": {"counterparty": "ACME"}, "params_from": {},
        "body": "RECALL facts", "body_hash": "00", "declared": []});
    let r = areev_run::memread::execute(&rig.facade, NS, &spec, &json!({}));
    match r.outcome {
        areev_run_core::EffectOutcome::Failed { detail, .. } => {
            assert!(detail.starts_with("RUN-E031") && detail.contains("hash"), "{detail}")
        }
        other => panic!("expected a failed read, got {other:?}"),
    }
}
