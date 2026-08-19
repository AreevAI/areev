//! OMS §8.4 execution records: `mg:step_action:<node_id>`.
//!
//! A Workflow grain is immutable and content-addressed, so it can never
//! accumulate run state. The spec's answer is to point the execution records at
//! the plan instead — a Tool grain carrying a `related_to` link of type
//! `mg:step_action:<node_id>` whose target is the Workflow's hash.
//!
//! These links were serialized but never indexed (`related_to` appeared nowhere
//! in this crate), so they were unreachable. They are now indexed into
//! `triples`/`osp` — and deliberately **not** into `heads`/`entity_latest`,
//! because OMS §15.3 is normative that a `related_to` link MUST NOT change the
//! target grain's supersession state.

use areev_core::types::*;
use areev_store::{Areev, Direction};
use tempfile::TempDir;

fn open_mem() -> (Areev, TempDir) {
    let d = TempDir::new().unwrap();
    let m = Areev::open(d.path().join("m.db").to_str().unwrap()).unwrap();
    (m, d)
}

/// A three-step plan, stored.
fn plan(m: &mut Areev) -> areev_core::error::Hash {
    let wf = Workflow::new(vec!["build".into(), "test".into(), "deploy".into()])
        .edge("build", "test")
        .edge("test", "deploy")
        .created_at(1_700_000_000_000)
        .namespace("ci");
    m.add(&wf).unwrap()
}

/// One execution record for `node`, linked to the plan.
fn ran(m: &mut Areev, wf: &areev_core::error::Hash, node: &str, at: i64) -> areev_core::error::Hash {
    let tool = Tool::new(node)
        .created_at(at)
        .namespace("ci")
        .step_action(&wf.to_hex(), node);
    m.add(&tool).unwrap()
}

#[test]
fn step_actions_link_execution_records_to_the_plan() {
    let (mut m, _d) = open_mem();
    let wf = plan(&mut m);
    let build = ran(&mut m, &wf, "build", 1_700_000_001_000);
    let test = ran(&mut m, &wf, "test", 1_700_000_002_000);

    let got = m.step_actions("ci", &wf, None, 100).unwrap();
    assert_eq!(got.len(), 2, "{got:?}");
    assert!(got.contains(&("build".to_string(), build)));
    assert!(got.contains(&("test".to_string(), test)));
}

#[test]
fn step_actions_narrow_to_one_node() {
    let (mut m, _d) = open_mem();
    let wf = plan(&mut m);
    ran(&mut m, &wf, "build", 1_700_000_001_000);
    let test = ran(&mut m, &wf, "test", 1_700_000_002_000);

    let got = m.step_actions("ci", &wf, Some("test"), 100).unwrap();
    assert_eq!(got, vec![("test".to_string(), test)]);
}

#[test]
fn repeated_attempts_at_one_node_all_survive() {
    // Retries are the normal case for a workflow node, and every attempt is its
    // own immutable grain — none of them supersedes another.
    let (mut m, _d) = open_mem();
    let wf = plan(&mut m);
    let a1 = ran(&mut m, &wf, "test", 1_700_000_001_000);
    let a2 = ran(&mut m, &wf, "test", 1_700_000_002_000);
    let a3 = ran(&mut m, &wf, "test", 1_700_000_003_000);

    let got = m.step_actions("ci", &wf, Some("test"), 100).unwrap();
    assert_eq!(got.len(), 3, "every attempt is a distinct record: {got:?}");
    for h in [a1, a2, a3] {
        assert!(got.iter().any(|(_, g)| *g == h), "missing attempt {h:?}");
    }
}

#[test]
fn step_action_does_not_touch_the_workflow_supersession_state() {
    // OMS §15.3, normative: a `related_to` link is an annotation. Indexing it
    // must not mark the target superseded, contradicted, or closed — otherwise
    // recording a run would silently retire the plan it ran.
    let (mut m, _d) = open_mem();
    let wf = plan(&mut m);
    for node in ["build", "test", "deploy"] {
        ran(&mut m, &wf, node, 1_700_000_001_000);
    }

    let g = m.get(&wf).unwrap();
    assert_eq!(g.grain_type, GrainType::Workflow);
    assert!(
        !g.fields.contains_key("superseded_by"),
        "execution records must not supersede the plan"
    );

    // And the plan is still the live head of its own namespace.
    let live = m.recent_live("ci", Some(GrainType::Workflow), 10).unwrap();
    assert_eq!(live.len(), 1, "plan must remain a live workflow");
}

#[test]
fn links_are_traversable_in_both_directions() {
    // The link is indexed as a triple subject-ed on the executing grain's own
    // hash, so the generic graph API reaches it from either end.
    let (mut m, _d) = open_mem();
    let wf = plan(&mut m);
    let build = ran(&mut m, &wf, "build", 1_700_000_001_000);
    let rel = step_action_relation("build");

    let out = m
        .related("ci", &build.to_hex(), &[rel.as_str()], Direction::Out, 1, 10)
        .unwrap();
    assert_eq!(out, vec![wf.to_hex()], "record -> plan");

    let back = m
        .related("ci", &wf.to_hex(), &[rel.as_str()], Direction::In, 1, 10)
        .unwrap();
    assert_eq!(back, vec![build.to_hex()], "plan -> record");
}

#[test]
fn unrelated_workflows_do_not_bleed_into_each_other() {
    let (mut m, _d) = open_mem();
    let a = plan(&mut m);
    let b = m
        .add(
            &Workflow::new(vec!["build".into()])
                .created_at(1_700_000_009_000)
                .namespace("ci"),
        )
        .unwrap();
    assert_ne!(a, b);

    ran(&mut m, &a, "build", 1_700_000_001_000);

    assert_eq!(m.step_actions("ci", &a, None, 100).unwrap().len(), 1);
    assert!(
        m.step_actions("ci", &b, None, 100).unwrap().is_empty(),
        "a record links to exactly the plan it names"
    );
}

#[test]
fn step_actions_survive_reopen() {
    let d = TempDir::new().unwrap();
    let path = d.path().join("m.db");
    let wf;
    let rec;
    {
        let mut m = Areev::open(path.to_str().unwrap()).unwrap();
        wf = plan(&mut m);
        rec = ran(&mut m, &wf, "build", 1_700_000_001_000);
    }
    let mut m = Areev::open(path.to_str().unwrap()).unwrap();
    assert_eq!(
        m.step_actions("ci", &wf, None, 100).unwrap(),
        vec![("build".to_string(), rec)]
    );
}

#[test]
fn empty_and_unknown_inputs_return_nothing_rather_than_erroring() {
    let (mut m, _d) = open_mem();
    let wf = plan(&mut m);

    assert!(m.step_actions("ci", &wf, None, 100).unwrap().is_empty());
    assert!(m.step_actions("nope", &wf, None, 100).unwrap().is_empty());
    assert!(m
        .step_actions("ci", &wf, Some("no-such-node"), 100)
        .unwrap()
        .is_empty());
}

#[test]
fn step_action_relation_parses_round_trip() {
    assert_eq!(step_action_relation("build"), "mg:step_action:build");
    assert_eq!(step_action_node("mg:step_action:build"), Some("build"));
    // A node id may itself contain a colon; only the prefix is structural.
    assert_eq!(step_action_node("mg:step_action:a:b"), Some("a:b"));
    // Not an execution record.
    assert_eq!(step_action_node("mg:knows"), None);
    // No node named — an execution record must say which step it ran.
    assert_eq!(step_action_node("mg:step_action:"), None);
}

// ---------------------------------------------------------------------------
// OMS 1.5 forward-compatibility posture
// ---------------------------------------------------------------------------

/// `deserialize_blob` errors on an unknown grain type byte rather than skipping
/// it, so a file carrying a post-1.4 grain is unreadable to an older build —
/// not merely partially readable. The file records that requirement itself.
#[test]
fn a_new_grain_type_stamps_min_reader_version() {
    let d = TempDir::new().unwrap();
    let path = d.path().join("m.db");
    {
        let mut m = Areev::open(path.to_str().unwrap()).unwrap();
        // A file of only pre-1.5 grains makes no such claim.
        m.add(
            &areev_core::types::Fact::new("a", "b", "c")
                .created_at(1_700_000_000_000)
                .namespace("ns"),
        )
        .unwrap();
        assert_eq!(m.meta_get("min_reader_version").unwrap(), None);

        let rec = Recommendation::new(
            "entity:ns/a",
            Analyzer::new("loop.dup/1"),
            Summary {
                template_id: "t".into(),
                args: serde_json::json!({}),
            },
            Proposal::Cal("x".into()),
        )
        .created_at(1_700_000_001_000)
        .namespace("ns");
        m.add(&rec).unwrap();
        assert!(
            m.meta_get("min_reader_version").unwrap().is_some(),
            "a 0x0C grain must stamp the requirement"
        );
    }
    // Reopening with a build that satisfies it is silent.
    let m = Areev::open(path.to_str().unwrap()).unwrap();
    assert!(
        !m.open_warnings()
            .iter()
            .any(|w| w.contains("min_reader_version")),
        "this build satisfies its own stamp: {:?}",
        m.open_warnings()
    );
}

#[test]
fn a_file_needing_a_newer_reader_warns_at_open() {
    let d = TempDir::new().unwrap();
    let path = d.path().join("m.db");
    {
        let m = Areev::open(path.to_str().unwrap()).unwrap();
        // Simulate a file written by a future build.
        m.meta_put("min_reader_version", "99.0.0").unwrap();
    }
    let m = Areev::open(path.to_str().unwrap()).unwrap();
    let w = m.open_warnings();
    assert!(
        w.iter().any(|x| x.contains("99.0.0")),
        "open must say the file needs a newer reader, got {w:?}"
    );
}

#[test]
fn step_actions_are_newest_first_across_nodes_and_stable_under_a_cap() {
    // With no `node_id` the predicate set comes from a dictionary prefix scan
    // over a HashMap, and each predicate was queried and capped on its own. So
    // results arrived grouped by node in an order that varied per process, and
    // the cap then kept whichever group happened to come first — not the newest
    // records. Interleave the nodes in time so per-node grouping cannot pass.
    let (mut m, _d) = open_mem();
    let wf = plan(&mut m);
    let build = ran(&mut m, &wf, "build", 1_700_000_001_000);
    let test = ran(&mut m, &wf, "test", 1_700_000_002_000);
    let deploy = ran(&mut m, &wf, "deploy", 1_700_000_003_000);
    let build2 = ran(&mut m, &wf, "build", 1_700_000_004_000);

    let all = m.step_actions("ci", &wf, None, 100).unwrap();
    assert_eq!(
        all,
        vec![
            ("build".to_string(), build2),
            ("deploy".to_string(), deploy),
            ("test".to_string(), test),
            ("build".to_string(), build),
        ],
        "newest first, regardless of which node each record belongs to"
    );

    // Under a cap the newest survive — not one node's whole group.
    let capped = m.step_actions("ci", &wf, None, 2).unwrap();
    assert_eq!(
        capped,
        vec![
            ("build".to_string(), build2),
            ("deploy".to_string(), deploy)
        ],
        "the cap keeps the newest records, not the first predicate scanned"
    );

    // Same answer every time: nothing here may depend on hash iteration order.
    for _ in 0..8 {
        assert_eq!(m.step_actions("ci", &wf, None, 2).unwrap(), capped);
    }
}

/// The paginated twin: complete, oldest-first, cursor-disciplined.
#[test]
fn step_actions_page_pages_in_seq_order() {
    let (mut m, _d) = open_mem();
    let wf = plan(&mut m);
    let t0 = 1_700_000_000_000;
    let mut expect: Vec<areev_core::error::Hash> = Vec::new();
    for (i, node) in ["build", "build", "test", "deploy", "test"].iter().enumerate() {
        expect.push(ran(&mut m, &wf, node, t0 + i as i64));
    }

    let mut got: Vec<(String, areev_core::error::Hash)> = Vec::new();
    let mut cursor = 0i64;
    loop {
        let page = m.step_actions_page("ci", &wf, None, cursor, 2).unwrap();
        if page.is_empty() {
            break;
        }
        assert!(page.iter().all(|(seq, _, _)| *seq > cursor));
        assert!(page.windows(2).all(|w| w[0].0 < w[1].0));
        cursor = page.last().unwrap().0;
        got.extend(page.into_iter().map(|(_, n, h)| (n, h)));
    }
    assert_eq!(got.len(), 5, "pagination must return every record");
    assert_eq!(
        got.iter().map(|(_, h)| *h).collect::<Vec<_>>(),
        expect,
        "oldest first, insertion order"
    );

    // Node filter composes with the cursor.
    let builds = m.step_actions_page("ci", &wf, Some("build"), 0, 10).unwrap();
    assert_eq!(builds.len(), 2);
    assert!(builds.iter().all(|(_, n, _)| n == "build"));
}

/// The supersession × step_action interaction the original suite never
/// exercised (`repeated_attempts_at_one_node_all_survive` deliberately never
/// supersedes). Superseding a grain flips its link rows to `cur=0`, so an
/// intent's `mg:step_action` link vanishes from `step_actions` the moment its
/// result lands — the execution record survives **only** if the superseding
/// result re-states the link (and `run_id`). This is the §5.1 journal rule of
/// the governed-agents proposal, pinned at the store layer: with re-statement
/// the head is queryable; without it the node has no execution record at all.
#[test]
fn superseding_an_execution_record_needs_the_link_restated() {
    let (mut m, _d) = open_mem();
    let wf = plan(&mut m);
    let t0 = 1_700_000_000_000;

    // Attempt A: intent carries the link + run_id; its result RE-STATES both.
    let mut intent_a = Tool::new("build").created_at(t0).namespace("ci").step_action(&wf.to_hex(), "build");
    intent_a.status = Some(ExecutionStatus::Pending);
    intent_a.common.extra_fields.insert("run_id".into(), serde_json::json!("run-1"));
    let ia = m.add(&intent_a).unwrap();

    let mut result_a = Tool::new("build").created_at(t0 + 1).namespace("ci").step_action(&wf.to_hex(), "build");
    result_a.status = Some(ExecutionStatus::Completed);
    result_a.content = Some("ok".into());
    result_a.common.extra_fields.insert("run_id".into(), serde_json::json!("run-1"));
    let ra = m.supersede(&ia, &mut result_a).unwrap();

    // Attempt B on another node: the result FORGETS to re-state the link.
    let mut intent_b = Tool::new("test").created_at(t0 + 2).namespace("ci").step_action(&wf.to_hex(), "test");
    intent_b.status = Some(ExecutionStatus::Pending);
    intent_b.common.extra_fields.insert("run_id".into(), serde_json::json!("run-1"));
    let ib = m.add(&intent_b).unwrap();

    let mut result_b = Tool::new("test").created_at(t0 + 3).namespace("ci");
    result_b.status = Some(ExecutionStatus::Completed);
    result_b.common.extra_fields.insert("run_id".into(), serde_json::json!("run-1"));
    let _rb = m.supersede(&ib, &mut result_b).unwrap();

    let records = m.step_actions("ci", &wf, None, 100).unwrap();
    let hashes: Vec<_> = records.iter().map(|(_, h)| *h).collect();
    assert!(hashes.contains(&ra), "the re-stating result is the queryable record");
    assert!(!hashes.contains(&ia), "the superseded intent's link is no longer current");
    assert!(
        !records.iter().any(|(n, _)| n == "test"),
        "a result that forgets the link erases the node's execution record — \
         the trap the runtime's re-statement rule exists for"
    );

    // The run journal keeps BOTH intent and result (run_idx rows never flip
    // on supersession): the paginated read sees the full lifecycle.
    let journal = m.run_grains("ci", "run-1", 0, 100).unwrap();
    assert_eq!(journal.len(), 4, "two intents + two results, all journaled");
}
