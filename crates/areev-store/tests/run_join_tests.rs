//! The join: querying across execution history and semantic memory.
//!
//! Every agent stack keeps these apart — a checkpointer holds in-thread
//! execution state, a memory store holds cross-thread facts — and nothing can
//! query across the seam. Here they are the same substrate, so "what did this
//! run produce" and "which runs touched this fact" are one lookup each.
//!
//! Two indexes make that cheap: `run_idx` (run_id -> grains) and `prov_idx`
//! (parent address -> grains derived from it). Before `prov_idx`,
//! `grains_derived_from` read and deserialized *every grain in the store* on
//! each call.

use areev_core::types::*;
use areev_store::Areev;
use tempfile::TempDir;

fn open_mem() -> (Areev, TempDir) {
    let d = TempDir::new().unwrap();
    let m = Areev::open(d.path().join("m.db").to_str().unwrap()).unwrap();
    (m, d)
}

/// One run: two transcript Events, and a Fact distilled from the first.
/// Returns `(event1, event2, fact)`.
fn a_run(
    m: &mut Areev,
    run: &str,
    at: i64,
) -> (
    areev_core::error::Hash,
    areev_core::error::Hash,
    areev_core::error::Hash,
) {
    let e1 = m
        .add(
            &Event::new("caller asked about refunds")
                .run_id(run.to_string())
                .created_at(at)
                .namespace("ops"),
        )
        .unwrap();
    let e2 = m
        .add(
            &Event::new("agent quoted the 30-day policy")
                .run_id(run.to_string())
                .created_at(at + 1)
                .namespace("ops"),
        )
        .unwrap();
    let mut fact = Fact::new("refunds", "window_days", "30");
    fact.common.derived_from = Some(e1.to_hex());
    fact.common.created_at = Some(at + 2);
    fact.common.namespace = Some("ops".into());
    let f = m.add(&fact).unwrap();
    (e1, e2, f)
}

#[test]
fn run_trace_returns_only_that_runs_grains() {
    let (mut m, _d) = open_mem();
    a_run(&mut m, "run-a", 1_700_000_000_000);
    a_run(&mut m, "run-b", 1_700_000_010_000);

    let trace = m.run_trace("ops", "run-a", 100).unwrap();
    assert_eq!(trace.len(), 2, "two events, and the fact is not in the run");
    for g in &trace {
        assert_eq!(g.get_str("run_id"), Some("run-a"));
    }
}

#[test]
fn run_yield_crosses_from_execution_history_into_memory() {
    // The join: the run's transcript is Events; what it *produced* is a Fact
    // that is not itself part of the run.
    let (mut m, _d) = open_mem();
    let (_e1, _e2, fact) = a_run(&mut m, "run-a", 1_700_000_000_000);

    let produced = m.run_yield("ops", "run-a", 100).unwrap();
    assert_eq!(produced.len(), 1, "{produced:?}");
    assert_eq!(produced[0].hash, fact);
    assert_eq!(produced[0].get_str("object"), Some("30"));
}

#[test]
fn runs_touching_walks_back_from_a_fact_into_the_runs_that_made_it() {
    let (mut m, _d) = open_mem();
    let (_e1, _e2, fact) = a_run(&mut m, "run-a", 1_700_000_000_000);
    a_run(&mut m, "run-b", 1_700_000_010_000);

    let runs = m.runs_touching("ops", &fact, 4).unwrap();
    assert_eq!(runs, vec!["run-a".to_string()], "only the run that made it");
}

#[test]
fn runs_touching_follows_a_refinement_chain_across_runs() {
    // A later run supersedes/refines the fact. Both runs touched it, and the
    // chain is what connects them — neither grain names the other run.
    let (mut m, _d) = open_mem();
    let (_e1, _e2, fact) = a_run(&mut m, "run-a", 1_700_000_000_000);

    let e3 = m
        .add(
            &Event::new("policy changed to 45 days")
                .run_id("run-b".to_string())
                .created_at(1_700_000_020_000)
                .namespace("ops"),
        )
        .unwrap();
    let mut refined = Fact::new("refunds", "window_days", "45");
    refined.common.derived_from = Some(fact.to_hex());
    refined.common.created_at = Some(1_700_000_021_000);
    refined.common.namespace = Some("ops".into());
    let refined_hash = m.add(&refined).unwrap();
    assert_ne!(refined_hash, e3);

    // From the original fact, walking down reaches the refinement; walking up
    // from the refinement reaches the original and its run.
    let mut runs = m.runs_touching("ops", &fact, 4).unwrap();
    runs.sort();
    assert_eq!(runs, vec!["run-a".to_string()]);

    // The refinement's own lineage reaches run-a through the fact it refined.
    let runs2 = m.runs_touching("ops", &refined_hash, 4).unwrap();
    assert!(runs2.contains(&"run-a".to_string()), "{runs2:?}");
}

#[test]
fn runs_touching_is_bounded_by_depth() {
    // A long-lived memory's lineage is unbounded, so the walk must be capped.
    let (mut m, _d) = open_mem();
    let (e1, _e2, _f) = a_run(&mut m, "run-a", 1_700_000_000_000);

    // A chain of derivations, each in its own run, hanging off the first event.
    let mut parent = e1;
    for i in 0..5 {
        let mut f = Fact::new("chain", "step", &i.to_string());
        f.common.derived_from = Some(parent.to_hex());
        f.common.created_at = Some(1_700_000_100_000 + i);
        f.common.namespace = Some("ops".into());
        parent = m.add(&f).unwrap();
    }

    let shallow = m.runs_touching("ops", &parent, 1).unwrap();
    let deep = m.runs_touching("ops", &parent, 8).unwrap();
    assert!(
        deep.len() >= shallow.len(),
        "a deeper walk cannot see less: {shallow:?} vs {deep:?}"
    );
    assert!(deep.contains(&"run-a".to_string()), "{deep:?}");
}

#[test]
fn grains_derived_from_is_served_by_the_index() {
    let (mut m, _d) = open_mem();
    let (e1, _e2, fact) = a_run(&mut m, "run-a", 1_700_000_000_000);

    let kids = m.grains_derived_from(&e1).unwrap();
    assert_eq!(kids.len(), 1);
    assert_eq!(kids[0].hash, fact);

    // A grain nothing derives from answers empty, not everything.
    assert!(m.grains_derived_from(&fact).unwrap().is_empty());
}

#[test]
fn the_join_survives_reopen() {
    let d = TempDir::new().unwrap();
    let path = d.path().join("m.db");
    let fact;
    {
        let mut m = Areev::open(path.to_str().unwrap()).unwrap();
        let (_e1, _e2, f) = a_run(&mut m, "run-a", 1_700_000_000_000);
        fact = f;
    }
    let mut m = Areev::open(path.to_str().unwrap()).unwrap();
    assert_eq!(m.run_trace("ops", "run-a", 100).unwrap().len(), 2);
    assert_eq!(m.run_yield("ops", "run-a", 100).unwrap().len(), 1);
    assert_eq!(m.runs_touching("ops", &fact, 4).unwrap(), vec!["run-a"]);
}

#[test]
fn unknown_runs_and_namespaces_answer_empty_rather_than_erroring() {
    let (mut m, _d) = open_mem();
    a_run(&mut m, "run-a", 1_700_000_000_000);

    assert!(m.run_trace("ops", "no-such-run", 100).unwrap().is_empty());
    assert!(m.run_trace("no-such-ns", "run-a", 100).unwrap().is_empty());
    assert!(m.run_yield("ops", "no-such-run", 100).unwrap().is_empty());
}

#[test]
fn rebuild_link_indexes_backfills_and_is_idempotent() {
    // A file written before these indexes existed answers provenance and run
    // questions with nothing until it is reindexed.
    let (mut m, _d) = open_mem();
    let (_e1, _e2, fact) = a_run(&mut m, "run-a", 1_700_000_000_000);

    let first = m.rebuild_link_indexes().unwrap();
    assert!(first >= 3, "two run rows + one provenance row: {first}");
    assert_eq!(m.run_trace("ops", "run-a", 100).unwrap().len(), 2);
    assert_eq!(m.runs_touching("ops", &fact, 4).unwrap(), vec!["run-a"]);

    // Running it twice must not double-count or duplicate rows.
    let second = m.rebuild_link_indexes().unwrap();
    assert_eq!(first, second);
    assert_eq!(m.run_trace("ops", "run-a", 100).unwrap().len(), 2);
    assert_eq!(m.grains_derived_from(&_e1).unwrap().len(), 1);
}

#[test]
fn forget_clears_the_link_indexes_so_a_reused_seq_inherits_nothing() {
    // `seq` is re-derived as MAX(seq)+1 on every open, so forgetting the newest
    // grain hands its seq straight to the next write. An index row that outlives
    // its grain therefore does not merely leak — it re-attaches a stranger to the
    // forgotten grain's parent and run, and the join answers with a grain that
    // has neither `derived_from` nor `run_id` set.
    let d = TempDir::new().unwrap();
    let path = d.path().join("m.db");
    let path = path.to_str().unwrap();

    let (parent, forgotten) = {
        let mut m = Areev::open(path).unwrap();
        let parent = m
            .add(&Fact::new("refunds", "policy", "30 days").namespace("ops"))
            .unwrap();

        let mut child = Fact::new("refunds", "window_days", "30");
        child.common.derived_from = Some(parent.to_hex());
        child.common.namespace = Some("ops".into());
        let child = m.add(&child).unwrap();

        let evt = m
            .add(
                &Event::new("the turn that produced it")
                    .run_id("run-x".to_string())
                    .namespace("ops"),
            )
            .unwrap();

        assert_eq!(m.grains_derived_from(&parent).unwrap().len(), 1);
        assert_eq!(m.run_trace("ops", "run-x", 10).unwrap().len(), 1);

        // Forget both; they hold the two highest seqs in the file.
        m.forget(&child).unwrap();
        m.forget(&evt).unwrap();
        assert!(m.grains_derived_from(&parent).unwrap().is_empty());
        assert!(m.run_trace("ops", "run-x", 10).unwrap().is_empty());
        (parent, child)
    };

    // Reopen so next_seq is recomputed, then write grains that have nothing to
    // do with either the parent or the run.
    let mut m = Areev::open(path).unwrap();
    m.add(&Fact::new("shipping", "carrier", "dhl").namespace("ops"))
        .unwrap();
    m.add(&Fact::new("shipping", "eta_days", "3").namespace("ops"))
        .unwrap();

    assert!(
        m.grains_derived_from(&parent).unwrap().is_empty(),
        "a stale prov_idx row re-attached an unrelated grain to {}",
        parent.to_hex()
    );
    assert!(
        m.run_trace("ops", "run-x", 10).unwrap().is_empty(),
        "a stale run_idx row put an unrelated grain in run-x"
    );
    assert!(m.get(&forgotten).is_err(), "the forgotten grain stays gone");
}

#[test]
fn open_heals_a_file_written_before_the_link_indexes_existed() {
    // The failure this prevents is silence: an unhealed file answers every
    // provenance and run question with an empty result, which reads exactly like
    // an honest "nothing was derived from this".
    let d = TempDir::new().unwrap();
    let path = d.path().join("m.db");
    let path = path.to_str().unwrap();

    let (e1, fact) = {
        let (mut m, _) = (Areev::open(path).unwrap(), ());
        let (e1, _e2, fact) = a_run(&mut m, "run-a", 1_700_000_000_000);
        (e1, fact)
    };

    // Simulate a file written before these indexes existed by rolling its
    // file-truth back to an older version stamp.
    {
        let m = Areev::open(path).unwrap();
        assert_eq!(
            m.meta_get("link_index").unwrap().as_deref(),
            Some("2"),
            "a file written by this build declares its link indexes current"
        );
        m.meta_put("link_index", "0").unwrap();
    }

    {
        let mut m = Areev::open(path).unwrap();
        assert!(
            m.open_warnings().iter().any(|w| w.contains("link indexes rebuilt")),
            "the rebuild must announce itself: {:?}",
            m.open_warnings()
        );
        assert_eq!(m.run_trace("ops", "run-a", 100).unwrap().len(), 2);
        assert_eq!(m.grains_derived_from(&e1).unwrap().len(), 1);
        assert_eq!(m.runs_touching("ops", &fact, 4).unwrap(), vec!["run-a"]);
    }

    // Stamped, so the next open does not re-scan the corpus. Scoped, because
    // one memory is one handle — the reopen has to be a *re*open.
    let m2 = Areev::open(path).unwrap();
    assert!(
        !m2.open_warnings().iter().any(|w| w.contains("link indexes rebuilt")),
        "second open must be quiet: {:?}",
        m2.open_warnings()
    );
}

/// `run_trace` clamps at 1024 with no cursor — fine for a transcript peek, a
/// correctness hole for a runtime resuming a long run: past ~340 supersteps
/// (intent + result + checkpoint per step) the journal would silently
/// truncate and a resume would re-execute effects it can no longer see.
/// `run_grains` is the complete, cursor-paginated read. This drives a run
/// past the cap and proves completeness, ordering, and cursor discipline.
#[test]
fn run_grains_pages_the_whole_run_past_the_trace_cap() {
    let (mut m, _d) = open_mem();
    const N: usize = 1100;
    for i in 0..N {
        m.add(
            &Event::new(&format!("step {i}"))
                .run_id("big".to_string())
                .created_at(1_700_000_000_000 + i as i64)
                .namespace("ops"),
        )
        .unwrap();
    }

    // The old read really does truncate — that is why the paginated one exists.
    assert_eq!(m.run_trace("ops", "big", usize::MAX).unwrap().len(), 1024);

    let mut all: Vec<i64> = Vec::new();
    let mut cursor = 0i64;
    let mut pages = 0;
    loop {
        let page = m.run_grains("ops", "big", cursor, 500).unwrap();
        if page.is_empty() {
            break;
        }
        pages += 1;
        // Strictly ascending, and strictly after the cursor.
        assert!(page.iter().all(|(seq, _)| *seq > cursor));
        assert!(page.windows(2).all(|w| w[0].0 < w[1].0));
        cursor = page.last().unwrap().0;
        all.extend(page.iter().map(|(seq, _)| *seq));
        if page.len() < 500 {
            break;
        }
    }
    assert_eq!(all.len(), N, "pagination must return the complete journal");
    assert_eq!(pages, 3, "1100 rows at 500/page is 3 pages");
    // No duplicates across page boundaries.
    let unique: std::collections::HashSet<i64> = all.iter().copied().collect();
    assert_eq!(unique.len(), N);
}

#[test]
fn run_grains_handles_unknown_runs_and_exhausted_cursors() {
    let (mut m, _d) = open_mem();
    let (_e1, _e2, _f) = a_run(&mut m, "run-a", 1_700_000_000_000);
    assert!(m.run_grains("ops", "no-such-run", 0, 10).unwrap().is_empty());
    assert!(m.run_grains("nope", "run-a", 0, 10).unwrap().is_empty());
    // Two Events carry the run_id; the derived Fact is the run's *yield*,
    // not its trace — `run_grains` mirrors `run_trace` membership exactly.
    let page = m.run_grains("ops", "run-a", 0, 10).unwrap();
    assert_eq!(page.len(), 2);
    let last = page.last().unwrap().0;
    assert!(m.run_grains("ops", "run-a", last, 10).unwrap().is_empty());
}
