//! In-run recall (#342): the as-of recall a plan's `reads` entry answers
//! with, and the runtime's `k` ceiling around it.
//!
//! Two layers, both backends. `Areev::recall_at` is STORE semantics — it is
//! defined as `entity_at` per relation, so the two must agree identically
//! wherever the store runs. The ceiling is the RUNTIME's, and it is asserted
//! here through a real `areev_run::Runner` over each backend rather than only
//! in `areev-run`'s own tests (which run on the embedded tier alone): a
//! bounded read is only a guarantee if it holds on the tier where tools could
//! otherwise have opened the memory themselves.

use crate::Backend;
use areev_core::types::{Fact, Grain, Tool, ToolKind, Workflow};
use areev_store::Axis;
use serde_json::{json, Value};
use std::sync::Arc;

const DAY: i64 = 86_400_000;

fn dated(ns: &str, s: &str, p: &str, o: &str, valid_from: i64, created: i64) -> Fact {
    Fact::new(s, p, o)
        .namespace(ns)
        .valid_from(valid_from)
        .created_at(created)
}

/// `recall_at` IS `entity_at` per relation: a named relation with k = 1 gives
/// the as-of read's answer, an unnamed one gives one answer per relation
/// newest-first on the asked clock, and `k` bounds the list.
pub fn recall_at_is_entity_at_per_relation(b: &dyn Backend) {
    let mut m = b.open_named("recall_at");
    let ns = "ledger";
    let t0 = 1_000 * DAY;
    // Three relations; `limit` restated (the knowledge clock learns it late).
    let limit = m.add(&dated(ns, "ACC", "limit", "500", t0, t0)).unwrap();
    m.add(&dated(ns, "ACC", "owner", "acme", t0 + DAY, t0 + DAY)).unwrap();
    m.add(&dated(ns, "ACC", "tier", "gold", t0 + 2 * DAY, t0 + 2 * DAY)).unwrap();
    m.supersede(&limit, &mut dated(ns, "ACC", "limit", "900", t0, t0 + 10 * DAY))
        .unwrap();
    let at = t0 + 5 * DAY;

    for axis in [Axis::World, Axis::Knowledge] {
        for r in ["limit", "owner", "tier", "nothing"] {
            let one = m.recall_at(ns, "ACC", Some(r), 1, at, axis).unwrap();
            let as_of = m.entity_at(ns, "ACC", r, at, axis).unwrap();
            assert_eq!(
                one.first().map(|g| g.hash),
                as_of.map(|g| g.hash),
                "[{}] recall_at({r}, k=1) == entity_at on {axis:?}",
                b.name()
            );
        }
    }
    let objects = |gs: Vec<areev_core::format::DeserializedGrain>| -> Vec<String> {
        gs.iter()
            .map(|g| g.get_str("object").unwrap_or_default().to_string())
            .collect()
    };
    // World: the restatement is true since t0; newest valid_from first.
    assert_eq!(
        objects(m.recall_at(ns, "ACC", None, 16, at, Axis::World).unwrap()),
        vec!["gold", "acme", "900"],
        "[{}] world",
        b.name()
    );
    // Knowledge at t0+5d: the restatement was not yet known — 500 was.
    assert_eq!(
        objects(m.recall_at(ns, "ACC", None, 16, at, Axis::Knowledge).unwrap()),
        vec!["gold", "acme", "500"],
        "[{}] knowledge",
        b.name()
    );
    assert_eq!(m.recall_at(ns, "ACC", None, 2, at, Axis::World).unwrap().len(), 2);
    assert!(m.recall_at(ns, "ACC", None, 0, at, Axis::World).unwrap().is_empty());
    assert!(m.recall_at(ns, "NOBODY", None, 8, at, Axis::World).unwrap().is_empty());
    assert!(
        m.recall_at("ledger.*", "ACC", None, 8, at, Axis::World).is_err(),
        "[{}] an as-of recall takes an exact namespace",
        b.name()
    );
}

struct NoTools;
impl areev_run::HostToolExecutor for NoTools {
    fn execute(&self, tool: &str, _h: &str, _i: &Value, _k: &str) -> areev_run::ExecResult {
        areev_run::ExecResult::Ok(json!({ format!("{tool}_done"): true }))
    }
}

/// A plan's `op: recall` over each backend: the run reads at most `k` of its
/// own records, journaled with their hashes and verified from the journal;
/// a plan asking for more than the ceiling is refused before the run exists.
pub fn run_recall_is_bounded_and_refused_past_its_ceiling(b: &dyn Backend) {
    let mut m = b.open_named("run_recall");
    for i in 0..70 {
        m.add(
            &Fact::new("ACC", "line", &format!("line {i}"))
                .namespace("ops.ledger")
                .created_at(1_000 + i),
        )
        .unwrap();
    }
    let def = m
        .add(
            &Tool::new("close")
                .kind(ToolKind::Definition)
                .tool_description("conformance tool")
                .namespace("ops")
                .created_at(500),
        )
        .unwrap();
    let mut plan_with = |k: Value| {
        let mut wf = Workflow::new(vec!["lines".into(), "close".into()])
            .edge("lines", "close")
            .bind("close", &def.to_hex())
            .namespace("ops")
            .created_at(600);
        wf.common.extra_fields.insert(
            "reads".into(),
            json!({"lines": {"op": "recall", "ns": "ops.ledger", "subject_from": "/account",
                             "relation": "line", "k": k}}),
        );
        m.add(&wf).unwrap()
    };
    let bounded = plan_with(json!(64));
    let over = plan_with(json!(65));

    let runner = areev_run::Runner {
        facade: Arc::new(areev_cal::AreevFacade::new(m)),
        clock: Arc::new(areev_run::ScriptedClock::new(
            (0..200).map(|i| 1_785_000_000_000 + i * 10).collect(),
        )),
        executor: Arc::new(NoTools),
        llm: None,
        observer: None,
        ns: "ops".into(),
        principal: "user:conformance".into(),
    };
    let opts = areev_run::RunOptions { workers: 1, ..Default::default() };

    // Past the ceiling: refused at start, no run left behind.
    let err = runner
        .start(&over, "over", json!({"account": "ACC"}), &opts)
        .unwrap_err();
    assert!(
        err.to_string().contains("`k` must be an integer from 1 to 64"),
        "[{}] {err}",
        b.name()
    );
    assert!(runner.inspect("over").is_err(), "[{}] no run left behind", b.name());

    // At the ceiling: 70 stored, 64 read, every hash journaled.
    runner
        .start(&bounded, "at-max", json!({"account": "ACC"}), &opts)
        .unwrap();
    let read = runner
        .facade
        .with_store(|m| m.run_trace("ops", "at-max", 1024))
        .unwrap()
        .into_iter()
        .find(|g| g.get_str("tool_name") == Some("mg:recall") && g.fields.contains_key("read"))
        .expect("the read's result grain");
    let record = &read.fields["read"];
    assert_eq!(record["k"], 64, "[{}]", b.name());
    assert_eq!(record["grains"].as_array().map(Vec::len), Some(64), "[{}]", b.name());
    let content: Value = serde_json::from_str(read.get_str("tool_content").unwrap()).unwrap();
    assert_eq!(content["lines"].as_array().map(Vec::len), Some(64), "[{}]", b.name());
    assert!(
        runner.verify("at-max").unwrap().verified,
        "[{}] the read replays from the journal",
        b.name()
    );
}
