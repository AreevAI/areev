//! The run-correlation contract (governed-agents §2 item 3 / §5.1): a
//! top-level `run_id` on ANY grain type reaches `run_idx`; `run_grains` pages
//! the complete journal; and an execution record survives supersession only
//! by re-statement. Storage semantics — so they must hold identically on
//! both backends, which is exactly where the runtime's resume correctness
//! lives.

use crate::Backend;
use areev_core::types::{ExecutionStatus, Fact, Grain, State, Tool, ToolKind, Workflow};

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
    // Unnamed node: the predicate set comes from a dictionary PREFIX scan
    // rather than one lookup. A prefix scan has no miss to fall through on, so
    // a backend whose handle does not hold the whole dictionary must answer
    // from the database — otherwise this reads as "no step actions", which is
    // indistinguishable from a run that recorded none.
    let all = m.step_actions("ops", &wf, None, 10).unwrap();
    assert_eq!(
        all,
        vec![("fetch".to_string(), rh)],
        "[{}] every step action of a plan is found without naming the node",
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

/// The execution edge behind `areev tool provenance`: a journal entry names
/// the Definition it ran in `spec_hash`, and the run join reads that backwards.
///
/// Storage semantics, so both backends must answer identically — and this one
/// is a scan rather than an index read, which is exactly the kind of thing
/// that behaves differently when one backend's `recent` orders or windows
/// differently from the other's.
pub fn runs_executing_reads_the_spec_hash_edge(b: &dyn Backend) {
    let mut m = b.open();
    let def = m
        .add(
            &Tool::new("screen")
                .kind(ToolKind::Definition)
                .tool_description("the pinned rule")
                .created_at(1_000)
                .namespace("ops"),
        )
        .unwrap();
    let unrelated = m
        .add(
            &Tool::new("post")
                .kind(ToolKind::Definition)
                .tool_description("another tool")
                .created_at(1_001)
                .namespace("ops"),
        )
        .unwrap();

    for (run, spec, at) in [("run-a", &def, 2_000), ("run-b", &def, 2_001), ("run-c", &unrelated, 2_002)] {
        let mut entry = Tool::new("screen").spec_hash(&spec.to_hex()).created_at(at).namespace("ops");
        entry
            .common_mut()
            .extra_fields
            .insert("run_id".into(), serde_json::json!(run));
        m.add(&entry).unwrap();
    }

    let mut runs = m.runs_touching("ops", &def, 4).unwrap();
    runs.sort();
    assert_eq!(
        runs,
        vec!["run-a".to_string(), "run-b".to_string()],
        "[{}] the runs that executed this definition, and only those",
        b.name()
    );
    assert!(
        m.runs_executing("ops", &def).unwrap().len() == 2,
        "[{}] the narrow read agrees with the walk",
        b.name()
    );
    // A Fact can never be named by a spec_hash, so the scan is skipped for it.
    let f = m.add(&Fact::new("s", "r", "o").namespace("ops").created_at(3_000)).unwrap();
    assert!(
        m.runs_executing("ops", &f).unwrap().is_empty(),
        "[{}] only a Tool Definition has an execution edge",
        b.name()
    );
}
