//! The run-correlation contract (governed-agents §2 item 3 / §5.1): a
//! top-level `run_id` on ANY grain type reaches `run_idx`; `run_grains` pages
//! the complete journal; and an execution record survives supersession only
//! by re-statement. Storage semantics — so they must hold identically on
//! both backends, which is exactly where the runtime's resume correctness
//! lives.

use crate::Backend;
use areev_core::types::{ExecutionStatus, Fact, Grain, State, Tool, Workflow};

/// `run_id` is a typed field only on Event; on every other type it rides as a
/// top-level extra field the indexer reads. That was mechanical and untested
/// — this pins it as a contract, per grain-type family.
pub fn run_id_indexes_on_any_grain_type(b: &dyn Backend) {
    let mut m = b.open();
    let mut f = Fact::new("s", "r", "o").namespace("ops").created_at(1_000);
    f.common_mut()
        .extra_fields
        .insert("run_id".into(), serde_json::json!("run-x"));
    m.add(&f).unwrap();

    let mut t = Tool::new("curl").namespace("ops").created_at(2_000);
    t.common_mut()
        .extra_fields
        .insert("run_id".into(), serde_json::json!("run-x"));
    m.add(&t).unwrap();

    let mut st = State::new(serde_json::json!({"k": 1}))
        .namespace("ops")
        .created_at(3_000);
    st.common_mut()
        .extra_fields
        .insert("run_id".into(), serde_json::json!("run-x"));
    m.add(&st).unwrap();

    let trace = m.run_trace("ops", "run-x", 100).unwrap();
    assert_eq!(
        trace.len(),
        3,
        "[{}] every grain type with a top-level run_id joins the run",
        b.name()
    );
}

/// Cursor-paginated journal reads: complete, oldest-first, no duplicates
/// across page boundaries — the read a resume depends on.
pub fn run_grains_pages_completely(b: &dyn Backend) {
    let mut m = b.open();
    for i in 0..30 {
        let mut f = Fact::new("s", "step", &format!("v{i}"))
            .namespace("ops")
            .created_at(1_000 + i);
        f.common_mut()
            .extra_fields
            .insert("run_id".into(), serde_json::json!("run-p"));
        m.add(&f).unwrap();
    }
    let mut all = Vec::new();
    let mut cursor = 0i64;
    loop {
        let page = m.run_grains("ops", "run-p", cursor, 10).unwrap();
        if page.is_empty() {
            break;
        }
        assert!(
            page.iter().all(|(seq, _)| *seq > cursor),
            "[{}] page strictly after cursor",
            b.name()
        );
        cursor = page.last().unwrap().0;
        all.extend(page.into_iter().map(|(s, _)| s));
        if all.len() > 60 {
            panic!("[{}] runaway pagination", b.name());
        }
    }
    assert_eq!(all.len(), 30, "[{}] pagination is complete", b.name());
    let mut sorted = all.clone();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(sorted, all, "[{}] ascending, no duplicates", b.name());
}

/// The supersession × execution-record interaction (§5.1's re-statement
/// rule): superseding an intent flips its link rows off the current set, so
/// the result grain must re-state the `mg:step_action` link and `run_id` or
/// the node loses its execution record while the journal keeps both.
pub fn step_action_survives_supersession_only_by_restatement(b: &dyn Backend) {
    let mut m = b.open();
    let wf = m
        .add(
            &Workflow::new(vec!["fetch".into()])
                .namespace("ops")
                .created_at(500),
        )
        .unwrap();

    let mut intent = Tool::new("fetch")
        .namespace("ops")
        .created_at(1_000)
        .step_action(&wf.to_hex(), "fetch");
    intent.status = Some(ExecutionStatus::Pending);
    intent
        .common_mut()
        .extra_fields
        .insert("run_id".into(), serde_json::json!("run-s"));
    let ih = m.add(&intent).unwrap();

    let mut result = Tool::new("fetch")
        .namespace("ops")
        .created_at(1_001)
        .step_action(&wf.to_hex(), "fetch");
    result.status = Some(ExecutionStatus::Completed);
    result.content = Some("ok".into());
    result
        .common_mut()
        .extra_fields
        .insert("run_id".into(), serde_json::json!("run-s"));
    let rh = m.supersede(&ih, &mut result).unwrap();

    let records = m.step_actions("ops", &wf, Some("fetch"), 10).unwrap();
    assert_eq!(
        records,
        vec![("fetch".to_string(), rh)],
        "[{}] the re-stating result is the sole current execution record",
        b.name()
    );
    let journal = m.run_grains("ops", "run-s", 0, 10).unwrap();
    assert_eq!(
        journal.len(),
        2,
        "[{}] the journal keeps intent AND result (run rows never flip)",
        b.name()
    );
}
