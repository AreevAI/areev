//! #432/#433: the Tool index (`tool_idx`) and the Loop's count reads, on both
//! backends. The per-tool counts, the error rows, the Definitions, the
//! per-namespace type counts and the distinct sessions must equal what reading
//! the grains would say — liveness, namespace scope, `forget` and a rebuild
//! included — while decoding only the grains the answer IS (the errors, the
//! Definitions), never the successful calls beside them. A handle whose index
//! is not built (a read-only open of a memory written before it) must answer
//! `None` for the counts, so the caller folds, and still return every
//! Definition through the fallback scan.

use crate::Backend;
use areev_core::types::{Event, Grain, GrainType, Tool, ToolKind};
use areev_store::{AreevOptions, ToolCount};

const T0: i64 = 1_700_000_000_000;

fn count(rows: &[ToolCount], ns: &str, tool: &str) -> (u64, u64) {
    rows.iter()
        .find(|r| r.namespace == ns && r.tool == tool)
        .map_or((0, 0), |r| (r.calls, r.errors))
}

pub fn tool_index_answers_counts_errors_and_definitions_without_reading_calls(b: &dyn Backend) {
    let name = "tool_index";
    let mut m = b.open_named(name);
    // Behind the window: never counted, never read.
    for i in 0..20i64 {
        m.add(&Tool::new("crm.lookup").namespace("desk.a").content(&format!("old {i}")).created_at(T0 + i))
            .unwrap();
    }
    let since = Some(T0 + 1_000);
    for i in 0..6i64 {
        m.add(&Tool::new("crm.lookup").namespace("desk.a").content(&format!("ok {i}")).created_at(T0 + 1_000 + i))
            .unwrap();
    }
    let mut errs = Vec::new();
    for i in 0..3i64 {
        errs.push(
            m.add(
                &Tool::new("crm.lookup")
                    .namespace("desk.a")
                    .content(&format!("timeout {i}"))
                    .is_error(true)
                    .created_at(T0 + 1_100 + i),
            )
            .unwrap(),
        );
    }
    for i in 0..2i64 {
        m.add(&Tool::new("mail.send").namespace("desk.b").content(&format!("sent {i}")).created_at(T0 + 1_200 + i))
            .unwrap();
    }
    m.add(&Tool::new("crm.lookup").namespace("desk.a").kind(ToolKind::Definition).created_at(T0 + 1_300))
        .unwrap();
    for (i, s) in ["conv-1", "conv-1", "conv-2"].iter().enumerate() {
        m.add(&Event::new(&format!("turn {i}")).session(s.to_string()).namespace("desk.a").created_at(T0 + 1_400 + i as i64))
            .unwrap();
    }
    m.add(&Event::new("no session").namespace("desk.a").created_at(T0 + 1_500)).unwrap();

    // Counts: no blob read. The Definition has a tool name, so it counts as a
    // call — what the Loop's fold always did.
    let before = m.blobs_decoded();
    let c = m.tool_counts(None, since, true).unwrap().expect("index built on a read-write open");
    assert_eq!(count(&c, "desk.a", "crm.lookup"), (10, 3), "[{}] 6 ok + 3 errors + 1 definition", b.name());
    assert_eq!(count(&c, "desk.b", "mail.send"), (2, 0), "[{}]", b.name());
    assert_eq!(c.len(), 2, "[{}] nothing from before the window", b.name());
    let per_ns = m.count_of_type_since(GrainType::Tool, since).unwrap();
    assert_eq!(
        per_ns,
        vec![("desk.a".to_string(), 10), ("desk.b".to_string(), 2)],
        "[{}] per-namespace type count",
        b.name()
    );
    let sessions = m.sessions_of_type_since(GrainType::Event, since).unwrap();
    let distinct: std::collections::BTreeSet<i64> = sessions.iter().map(|(_, s)| *s).collect();
    assert_eq!(distinct.len(), 2, "[{}] two sessions; a session-less Event is none", b.name());
    assert_eq!(m.blobs_decoded() - before, 0, "[{}] counts decode nothing", b.name());

    // The error rows: exactly they are decoded.
    let before = m.blobs_decoded();
    let e = m.tool_errors(None, since, true).unwrap().unwrap();
    assert_eq!(e.len(), 3, "[{}]", b.name());
    assert_eq!(m.blobs_decoded() - before, 3, "[{}] decoded the errors, not the 6 successes", b.name());

    // The Definitions: exactly they are decoded.
    let before = m.blobs_decoded();
    let d = m.tool_definitions(None).unwrap();
    assert_eq!(d.len(), 1, "[{}]", b.name());
    assert_eq!(m.blobs_decoded() - before, 1, "[{}] the call grains are never read", b.name());
    assert_eq!(m.tool_definitions(Some("desk.b")).unwrap().len(), 0, "[{}] scoped", b.name());

    // Namespace scope.
    let only_b = m.tool_counts(Some("desk.*"), since, true).unwrap().unwrap();
    assert_eq!(only_b.len(), 2, "[{}] prefix scope covers both", b.name());
    let only_b = m.tool_counts(Some("desk.b"), since, true).unwrap().unwrap();
    assert_eq!(only_b.len(), 1, "[{}] exact namespace", b.name());
    assert!(m.tool_counts(Some("nobody"), since, true).unwrap().unwrap().is_empty(), "[{}]", b.name());

    // Liveness: a supersession in the window retires its target.
    m.supersede(
        &errs[0],
        &mut Tool::new("crm.lookup").namespace("desk.a").content("retried ok").created_at(T0 + 1_600),
    )
    .unwrap();
    let live = m.tool_counts(None, since, true).unwrap().unwrap();
    assert_eq!(count(&live, "desk.a", "crm.lookup"), (10, 2), "[{}] one error retired, one ok added", b.name());
    let all = m.tool_counts(None, since, false).unwrap().unwrap();
    assert_eq!(count(&all, "desk.a", "crm.lookup"), (11, 3), "[{}] live_only = false keeps it", b.name());
    let e = m.tool_errors(None, since, true).unwrap().unwrap();
    assert!(e.iter().all(|g| g.hash != errs[0]), "[{}] retired error not returned", b.name());
    assert_eq!(e.len(), 2, "[{}]", b.name());

    // forget removes the row.
    m.forget(&errs[1]).unwrap();
    let after = m.tool_counts(None, since, false).unwrap().unwrap();
    assert_eq!(count(&after, "desk.a", "crm.lookup"), (10, 2), "[{}] a forgotten grain is not counted", b.name());

    // A rebuild reproduces what the write path maintained.
    m.rebuild_tool_index().unwrap();
    assert_eq!(m.tool_counts(None, since, true).unwrap().unwrap(), live_after_forget(&live), "[{}] rebuild", b.name());
    assert_eq!(m.tool_counts(None, since, false).unwrap().unwrap(), after, "[{}] rebuild", b.name());

    // A handle whose index is not built: the counts are None (fold), the
    // Definitions still complete through the fallback scan.
    m.meta_delete("tool_index").unwrap();
    drop(m);
    let mut ro = b.open_named_with(name, AreevOptions { read_only: true, ..AreevOptions::default() });
    assert!(ro.tool_counts(None, since, true).unwrap().is_none(), "[{}] unbuilt index answers None", b.name());
    assert!(ro.tool_errors(None, since, true).unwrap().is_none(), "[{}]", b.name());
    assert_eq!(ro.tool_definitions(None).unwrap().len(), 1, "[{}] fallback finds the Definition", b.name());
    drop(ro);
    // The next read-write open heals it.
    let mut m = b.open_named(name);
    assert_eq!(m.tool_counts(None, since, false).unwrap().unwrap(), after, "[{}] healed on open", b.name());
}

/// The live counts after `forget(errs[1])`: `errs[1]` was a live error.
fn live_after_forget(live: &[ToolCount]) -> Vec<ToolCount> {
    live.iter()
        .map(|r| {
            let mut r = r.clone();
            if r.namespace == "desk.a" && r.tool == "crm.lookup" {
                r.calls -= 1;
                r.errors -= 1;
            }
            r
        })
        .collect()
}
