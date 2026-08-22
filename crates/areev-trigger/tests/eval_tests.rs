//! The evaluation cycle end to end, against a real store.
//!
//! The claims worth pinning are the ones that are expensive to get wrong and
//! invisible when they are: a first poll must not replay history, the same item
//! must not start two runs, a failing connector must back off instead of
//! hot-looping, and a trigger that has never fired must be visible rather than
//! quietly doing nothing.

use areev_cal::AreevFacade;
use areev_core::types::{Grain, Trigger, TriggerKind};
use areev_run::{ExecResult, HostToolExecutor};
use areev_store::Areev;
use areev_trigger::{
    clock::FixedClock, Clock, EvalOptions, Evaluator, RunStarter, StartResult,
};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};

const NS: &str = "ops";
const WF: &str = "a1b2c3d4e5f60718293a4b5c6d7e8f90a1b2c3d4e5f60718293a4b5c6d7e8f90";
const T0: i64 = 1_767_225_600_000; // 2026-01-01T00:00:00Z

/// A connector whose answers a test scripts, recording what it was asked.
struct FakeConnector {
    replies: Mutex<Vec<ExecResult>>,
    requests: Mutex<Vec<Value>>,
}

impl FakeConnector {
    fn new(replies: Vec<ExecResult>) -> Arc<Self> {
        Arc::new(FakeConnector { replies: Mutex::new(replies), requests: Mutex::new(Vec::new()) })
    }
}

impl HostToolExecutor for FakeConnector {
    fn execute(&self, _name: &str, _hash: &str, input: &Value, _idem: &str) -> ExecResult {
        self.requests.lock().unwrap().push(input.clone());
        let mut r = self.replies.lock().unwrap();
        if r.is_empty() {
            ExecResult::Ok(json!({ "items": [] }))
        } else {
            r.remove(0)
        }
    }
}

fn ok(items: Value, cursor: Option<&str>, more: bool) -> ExecResult {
    let mut v = json!({ "items": items, "more": more });
    if let Some(c) = cursor {
        v["cursor"] = json!(c);
    }
    ExecResult::Ok(v)
}

/// A starter that records ids and enforces the duplicate rule the runtime does.
#[derive(Default)]
struct FakeStarter {
    started: Mutex<Vec<String>>,
}

impl RunStarter for FakeStarter {
    fn start(&self, _workflow: &str, run_id: &str, _input: Value) -> StartResult {
        let mut s = self.started.lock().unwrap();
        if s.iter().any(|id| id == run_id) {
            // Exactly what `areev run start` does with an existing id.
            return StartResult::Duplicate;
        }
        s.push(run_id.to_string());
        StartResult::Started
    }
}

struct Rig {
    _dir: tempfile::TempDir,
    facade: Arc<AreevFacade>,
    clock: Arc<FixedClock>,
    starter: Arc<FakeStarter>,
}

impl Rig {
    fn new() -> Self {
        let dir = tempfile::TempDir::new().unwrap();
        let m = Areev::open(dir.path().join("m.db").to_str().unwrap()).unwrap();
        Rig {
            _dir: dir,
            facade: Arc::new(AreevFacade::new(m)),
            clock: Arc::new(FixedClock::new(T0)),
            starter: Arc::new(FakeStarter::default()),
        }
    }

    fn declare(&self, t: Trigger) -> String {
        let t = t.created_at(T0).namespace(NS);
        self.facade.with_store(|m| m.add(&t)).unwrap().to_hex()
    }

    fn evaluator(&self, connector: Option<Arc<dyn HostToolExecutor>>) -> Evaluator {
        Evaluator {
            facade: Arc::clone(&self.facade),
            clock: Arc::clone(&self.clock) as Arc<dyn areev_trigger::Clock>,
            connector,
            starter: Some(Arc::clone(&self.starter) as Arc<dyn RunStarter>),
            credentials: Default::default(),
            ns: NS.into(),
            principal: "user:test".into(),
        }
    }

    fn state(&self, hash: &str) -> areev_store::TriggerState {
        self.facade.with_store(|m| m.trigger_state(hash)).unwrap().map(|(s, _)| s).unwrap_or_default()
    }
}

fn opts() -> EvalOptions {
    EvalOptions { node: "node-A".into(), ..Default::default() }
}

// ---- clocked triggers -----------------------------------------------------

#[test]
fn an_interval_trigger_fires_then_schedules_its_next() {
    let rig = Rig::new();
    let h = rig.declare(Trigger::new(TriggerKind::Interval, WF).interval_secs(120));
    let ev = rig.evaluator(None);

    let r = ev.run(&opts()).unwrap();
    assert_eq!(r.claimed, 1, "a never-evaluated trigger is due immediately");
    assert_eq!(r.runs_started, 1);
    assert!(r.errors.is_empty(), "{:?}", r.errors);

    let st = rig.state(&h);
    assert_eq!(st.next_due_at, Some(T0 + 120_000));
    assert_eq!(st.last_fired_at, Some(T0));
    assert!(st.claimed_by.is_none(), "the lease is released after firing");

    // Immediately again: not due.
    let r2 = ev.run(&opts()).unwrap();
    assert_eq!(r2.claimed, 0);
    assert_eq!(r2.skipped_not_due, 1);

    // After the interval: due again.
    rig.clock.advance(120_000);
    let r3 = ev.run(&opts()).unwrap();
    assert_eq!(r3.claimed, 1);
}

#[test]
fn ingest_only_mode_is_still_idempotent() {
    // Without a runtime wired in nothing executes, but the same item must still
    // not be re-ingested on every poll — an Event's created_at differs per
    // firing, so content addressing alone does not collapse them.
    let rig = Rig::new();
    rig.declare(
        Trigger::new(TriggerKind::Polling, WF).connector("gmail").interval_secs(60).dedup_key("/id"),
    );
    let item = json!([{ "id": "dup", "payload": { "id": "m-1" } }]);
    let conn = FakeConnector::new(vec![
        ok(json!([]), Some("c1"), false),
        ok(item.clone(), Some("c2"), false),
        ok(item, Some("c3"), false),
    ]);
    let ev = Evaluator {
        facade: Arc::clone(&rig.facade),
        clock: Arc::clone(&rig.clock) as Arc<dyn Clock>,
        connector: Some(conn as Arc<dyn HostToolExecutor>),
        starter: None, // ingest-only
        credentials: Default::default(),
        ns: NS.into(),
        principal: "user:test".into(),
    };

    ev.run(&opts()).unwrap(); // seed
    rig.clock.advance(60_000);
    let first = ev.run(&opts()).unwrap();
    rig.clock.advance(60_000);
    let second = ev.run(&opts()).unwrap();

    assert_eq!(first.ingested, 1);
    assert_eq!(first.runs_started, 0, "nothing executes without a runtime");
    assert_eq!(second.ingested, 0, "the replay must not be re-ingested");
    assert_eq!(second.duplicates, 1);
}

#[test]
fn a_disabled_declaration_never_fires() {
    let rig = Rig::new();
    rig.declare(Trigger::new(TriggerKind::Interval, WF).interval_secs(60).enabled(false));
    let r = rig.evaluator(None).run(&opts()).unwrap();
    assert_eq!(r.claimed, 0);
    assert_eq!(r.runs_started, 0);
}

#[test]
fn dry_run_reports_what_would_fire_and_touches_nothing() {
    let rig = Rig::new();
    let h = rig.declare(Trigger::new(TriggerKind::Interval, WF).interval_secs(60));

    let r = rig.evaluator(None).run(&EvalOptions { dry_run: true, ..opts() }).unwrap();
    assert_eq!(r.claimed, 1, "it reports the trigger as due…");
    assert_eq!(r.runs_started, 0, "…without starting anything");
    assert_eq!(rig.state(&h), areev_store::TriggerState::default(), "and writes no state");
}

// ---- polling --------------------------------------------------------------

#[test]
fn the_first_poll_seeds_the_cursor_and_fires_nothing() {
    // Otherwise declaring a mailbox trigger starts a run for every message in
    // history. Zapier primes on activation for exactly this reason.
    let rig = Rig::new();
    let h = rig.declare(
        Trigger::new(TriggerKind::Polling, WF).connector("gmail").interval_secs(60).dedup_key("/id"),
    );
    let conn = FakeConnector::new(vec![ok(
        json!([{ "id": "old-1", "payload": { "id": "old-1" } }]),
        Some("cursor-1"),
        false,
    )]);
    let ev = rig.evaluator(Some(conn.clone() as Arc<dyn HostToolExecutor>));

    let r = ev.run(&opts()).unwrap();
    assert_eq!(r.runs_started, 0, "history must not be replayed on first contact");
    assert_eq!(rig.state(&h).cursor.as_deref(), Some("cursor-1"), "but the position is recorded");
}

#[test]
fn items_after_the_seed_start_one_run_each() {
    let rig = Rig::new();
    let h = rig.declare(
        Trigger::new(TriggerKind::Polling, WF).connector("gmail").interval_secs(60).dedup_key("/id"),
    );
    let conn = FakeConnector::new(vec![
        ok(json!([]), Some("c1"), false),
        ok(
            json!([
                { "id": "a", "payload": { "id": "m-a" } },
                { "id": "b", "payload": { "id": "m-b" } }
            ]),
            Some("c2"),
            false,
        ),
    ]);
    let ev = rig.evaluator(Some(conn.clone() as Arc<dyn HostToolExecutor>));

    ev.run(&opts()).unwrap(); // seed
    rig.clock.advance(60_000);
    let r = ev.run(&opts()).unwrap();

    assert_eq!(r.items, 2);
    assert_eq!(r.runs_started, 2);
    assert_eq!(rig.state(&h).cursor.as_deref(), Some("c2"));

    // The connector was handed the stored cursor on the second call.
    let reqs = conn.requests.lock().unwrap();
    assert_eq!(reqs[1]["cursor"], json!("c1"));
}

#[test]
fn the_same_item_twice_starts_one_run_and_reports_a_skip() {
    // The idempotency guarantee: connector replay, overlapping cursors, or two
    // nodes racing must never produce two runs for one item.
    let rig = Rig::new();
    rig.declare(
        Trigger::new(TriggerKind::Polling, WF).connector("gmail").interval_secs(60).dedup_key("/id"),
    );
    let item = json!([{ "id": "dup", "payload": { "id": "m-1" } }]);
    let conn = FakeConnector::new(vec![
        ok(json!([]), Some("c1"), false),
        ok(item.clone(), Some("c2"), false),
        ok(item, Some("c3"), false),
    ]);
    let ev = rig.evaluator(Some(conn as Arc<dyn HostToolExecutor>));

    ev.run(&opts()).unwrap(); // seed
    rig.clock.advance(60_000);
    let first = ev.run(&opts()).unwrap();
    rig.clock.advance(60_000);
    let second = ev.run(&opts()).unwrap();

    assert_eq!(first.runs_started, 1);
    assert_eq!(second.runs_started, 0, "the replay must not start a second run");
    assert_eq!(second.duplicates, 1, "and the skip is reported, not hidden");
    assert_eq!(rig.starter.started.lock().unwrap().len(), 1);
}

#[test]
fn a_backlog_drains_at_once_instead_of_waiting_out_the_interval() {
    let rig = Rig::new();
    let h = rig.declare(
        Trigger::new(TriggerKind::Polling, WF)
            .connector("gmail")
            .interval_secs(3600) // an hour: a cold start must not take a day
            .dedup_key("/id"),
    );
    let conn = FakeConnector::new(vec![
        ok(json!([]), Some("c1"), false),
        ok(json!([{ "id": "a", "payload": { "id": "a" } }]), Some("c2"), true),
    ]);
    let ev = rig.evaluator(Some(conn as Arc<dyn HostToolExecutor>));

    ev.run(&opts()).unwrap();
    rig.clock.advance(3_600_000);
    let r = ev.run(&opts()).unwrap();

    assert!(r.draining.contains(&h), "a connector reporting more must be flagged");
    assert_eq!(rig.state(&h).next_due_at, Some(rig.clock.now_ms()), "due again immediately");
}

#[test]
fn an_item_with_no_usable_identity_is_reported_not_dropped() {
    let rig = Rig::new();
    rig.declare(
        Trigger::new(TriggerKind::Polling, WF)
            .connector("gmail")
            .interval_secs(60)
            .dedup_key("/message_id"),
    );
    let conn = FakeConnector::new(vec![
        ok(json!([]), Some("c1"), false),
        // No `message_id` anywhere: the declared pointer cannot resolve.
        ok(json!([{ "id": "x", "payload": { "subject": "hi" } }]), Some("c2"), false),
    ]);
    let ev = rig.evaluator(Some(conn as Arc<dyn HostToolExecutor>));

    ev.run(&opts()).unwrap();
    rig.clock.advance(60_000);
    let r = ev.run(&opts()).unwrap();

    assert_eq!(r.runs_started, 0);
    assert_eq!(r.unidentifiable, 1, "an unnameable item is a connector bug, and must show as one");
}

// ---- failure handling -----------------------------------------------------

#[test]
fn a_due_polling_trigger_with_no_connector_fails_loudly() {
    // A poll that silently does nothing is indistinguishable from a healthy
    // source with no new items — the worst possible failure mode.
    let rig = Rig::new();
    rig.declare(Trigger::new(TriggerKind::Polling, WF).connector("gmail").interval_secs(60));
    let r = rig.evaluator(None).run(&opts()).unwrap();

    assert_eq!(r.runs_started, 0);
    assert_eq!(r.errors.len(), 1);
    assert!(r.errors[0].starts_with("TRG-E003"), "{}", r.errors[0]);
}

#[test]
fn a_failing_connector_backs_off_instead_of_hot_looping() {
    let rig = Rig::new();
    let h = rig.declare(
        Trigger::new(TriggerKind::Polling, WF).connector("gmail").interval_secs(60).dedup_key("/id"),
    );
    let conn = FakeConnector::new(vec![ExecResult::Err {
        cause: areev_run_core::FailCause::ExecutorError,
        detail: "connection refused".into(),
    }]);
    let ev = rig.evaluator(Some(conn as Arc<dyn HostToolExecutor>));

    let r = ev.run(&opts()).unwrap();
    assert_eq!(r.errors.len(), 1);
    assert!(r.errors[0].starts_with("TRG-E004"), "{}", r.errors[0]);

    let st = rig.state(&h);
    assert_eq!(st.consecutive_failures, 1);
    assert!(st.last_error.is_some(), "the reason must be visible in status");
    assert!(
        st.next_due_at.unwrap() >= T0 + 60_000,
        "backed off at least the declared interval, not retried immediately"
    );
    assert!(st.claimed_by.is_none(), "and the lease is released even on failure");
}

#[test]
fn one_broken_trigger_does_not_stop_the_others() {
    let rig = Rig::new();
    rig.declare(Trigger::new(TriggerKind::Polling, WF).connector("gmail").interval_secs(60));
    rig.declare(Trigger::new(TriggerKind::Interval, WF).interval_secs(60));

    let r = rig.evaluator(None).run(&opts()).unwrap();
    assert_eq!(r.errors.len(), 1, "the polling one failed");
    assert_eq!(r.runs_started, 1, "…and the interval one still fired");
}

// ---- claiming -------------------------------------------------------------

#[test]
fn a_held_lease_is_skipped_not_stolen() {
    let rig = Rig::new();
    let h = rig.declare(Trigger::new(TriggerKind::Interval, WF).interval_secs(60));

    // Another node holds it, not yet expired.
    rig.facade
        .with_store(|m| {
            m.put_trigger_state(
                &h,
                None,
                &areev_store::TriggerState {
                    claimed_by: Some("node-B".into()),
                    lease_until: Some(T0 + 300_000),
                    ..Default::default()
                },
            )
        })
        .unwrap();

    let r = rig.evaluator(None).run(&opts()).unwrap();
    assert_eq!(r.skipped_locked, 1, "losing a race is the steady state, not an error");
    assert_eq!(r.claimed, 0);
    assert!(r.errors.is_empty());
}

#[test]
fn an_expired_lease_is_reclaimed() {
    let rig = Rig::new();
    let h = rig.declare(Trigger::new(TriggerKind::Interval, WF).interval_secs(60));
    rig.facade
        .with_store(|m| {
            m.put_trigger_state(
                &h,
                None,
                &areev_store::TriggerState {
                    claimed_by: Some("node-B".into()),
                    lease_until: Some(T0 - 1), // already gone
                    fence: 4,
                    ..Default::default()
                },
            )
        })
        .unwrap();

    let r = rig.evaluator(None).run(&opts()).unwrap();
    assert_eq!(r.claimed, 1, "a node that died must not park a trigger forever");
    assert_eq!(rig.state(&h).fence, 5, "and the fence advanced past the dead claim");
}

// ---- visibility -----------------------------------------------------------

#[test]
fn status_reports_a_trigger_that_has_never_fired() {
    let rig = Rig::new();
    rig.declare(Trigger::new(TriggerKind::Interval, WF).interval_secs(60));

    let st = rig.evaluator(None).status().unwrap();
    assert_eq!(st.len(), 1);
    assert!(st[0].never_fired, "an unnoticed misconfiguration must be visible");
    assert!(st[0].due);
    assert_eq!(st[0].leased_by, None);
}

#[test]
fn a_firing_is_journaled() {
    let rig = Rig::new();
    let h = rig.declare(Trigger::new(TriggerKind::Interval, WF).interval_secs(60));
    rig.evaluator(None).run(&opts()).unwrap();

    let obs = rig
        .facade
        .with_store(|m| {
            m.recent_live_scoped(
                &[areev_trigger::eval::TRIGGER_NS.to_string()],
                Some(areev_core::types::GrainType::Observation),
                10,
            )
        })
        .unwrap();
    assert_eq!(obs.len(), 1, "every firing leaves a record");
    assert_eq!(obs[0].get_str("trigger"), Some(h.as_str()));
    assert_eq!(obs[0].get_i64("runs_started"), Some(1));
}

// ---- memory triggers ------------------------------------------------------

fn memory_trigger(where_src: &str) -> Trigger {
    let cond = areev_trigger::predicate::parse_predicate(where_src).unwrap();
    Trigger::new(TriggerKind::Memory, WF)
        .predicate(areev_trigger::predicate::to_value(&cond).unwrap())
}

#[test]
fn a_memory_trigger_seeds_at_the_head_and_ignores_history() {
    // Declaring one on an established memory must not fire once per matching
    // grain already in it.
    let rig = Rig::new();
    for i in 0..5 {
        let f = areev_core::types::Fact::new(&format!("s{i}"), "mg:kind", "invoice")
            .namespace(NS)
            .created_at(T0);
        rig.facade.with_store(|m| m.add(&f)).unwrap();
    }
    rig.declare(memory_trigger(r#"object = "invoice""#));

    let r = rig.evaluator(None).run(&opts()).unwrap();
    assert_eq!(r.runs_started, 0, "history must not fire");
    assert_eq!(r.items, 0);
}

#[test]
fn a_memory_trigger_fires_for_grains_that_appear_after_it() {
    let rig = Rig::new();
    let h = rig.declare(memory_trigger(r#"object = "invoice""#));
    let ev = rig.evaluator(None);

    ev.run(&opts()).unwrap(); // seed at the head

    // Two matching, one not.
    for (s, o) in [("a", "invoice"), ("b", "receipt"), ("c", "invoice")] {
        let f = areev_core::types::Fact::new(s, "mg:kind", o).namespace(NS).created_at(T0);
        rig.facade.with_store(|m| m.add(&f)).unwrap();
    }

    let r = ev.run(&opts()).unwrap();
    assert_eq!(r.items, 2, "only the matching grains are items");
    assert_eq!(r.runs_started, 2);

    // The cursor advanced, so a second pass sees nothing new.
    let again = ev.run(&opts()).unwrap();
    assert_eq!(again.items, 0, "the op cursor must not re-deliver");
    assert!(rig.state(&h).op_cursor.is_some());
}

#[test]
fn a_memory_trigger_uses_the_content_address_as_the_item_identity() {
    // Re-adding identical content is a no-op returning the same hash, so the
    // same grain can never mint two runs even if it is re-added.
    let rig = Rig::new();
    rig.declare(memory_trigger(r#"object = "invoice""#));
    let ev = rig.evaluator(None);
    ev.run(&opts()).unwrap();

    let f = areev_core::types::Fact::new("a", "mg:kind", "invoice").namespace(NS).created_at(T0);
    rig.facade.with_store(|m| m.add(&f)).unwrap();
    let first = ev.run(&opts()).unwrap();
    assert_eq!(first.runs_started, 1);

    rig.facade.with_store(|m| m.add(&f)).unwrap(); // identical content
    let second = ev.run(&opts()).unwrap();
    assert_eq!(second.runs_started, 0, "the same grain must not fire twice");
}

#[test]
fn a_memory_trigger_with_a_boolean_predicate_respects_precedence() {
    let rig = Rig::new();
    rig.declare(memory_trigger(
        r#"(relation = "mg:kind" AND object = "invoice") OR object = "urgent""#,
    ));
    let ev = rig.evaluator(None);
    ev.run(&opts()).unwrap();

    for (s, r, o) in [
        ("a", "mg:kind", "invoice"), // left branch
        ("b", "other", "urgent"),    // right branch
        ("c", "other", "receipt"),   // neither
    ] {
        let f = areev_core::types::Fact::new(s, r, o).namespace(NS).created_at(T0);
        rig.facade.with_store(|m| m.add(&f)).unwrap();
    }

    let out = ev.run(&opts()).unwrap();
    assert_eq!(out.items, 2, "both branches match, the third does not");
}

// ---- composite gates ------------------------------------------------------

fn manual_member() -> Trigger {
    // A member that fires on every pass, so a test can drive the gate directly.
    Trigger::new(TriggerKind::Interval, WF).interval_secs(1)
}

#[test]
fn an_and_gate_waits_for_every_member() {
    let rig = Rig::new();
    let a = rig.declare(manual_member());
    let b = rig.declare(manual_member());

    let cond = areev_trigger::predicate::parse_predicate("invoice = true AND po = true").unwrap();
    let composite = rig.declare(
        Trigger::new(TriggerKind::Composite, WF)
            .member("invoice", &a)
            .member("po", &b)
            .predicate(areev_trigger::predicate::to_value(&cond).unwrap()),
    );

    // Both members are due on the first pass, so the gate completes.
    let r = rig.evaluator(None).run(&opts()).unwrap();
    assert!(r.errors.is_empty(), "{:?}", r.errors);

    let st = rig.state(&composite);
    assert!(st.last_fired_at.is_some(), "an AND gate with both members must fire");
}

#[test]
fn a_gate_naming_an_undeclared_member_is_refused() {
    // It could never be satisfied, so the trigger would be silently dead —
    // exactly the failure mode with no symptom.
    let rig = Rig::new();
    let a = rig.declare(manual_member());
    let b = rig.declare(manual_member());
    let cond =
        areev_trigger::predicate::parse_predicate("invoice = true AND ghost = true").unwrap();
    // TWO members declared, so the composite is otherwise coherent and the
    // undeclared `ghost` in the gate is the only thing wrong with it. With one
    // member it was ALSO "needs at least two members" (TRG-E001), so the test
    // could pass while the gate check it names never ran.
    rig.declare(
        Trigger::new(TriggerKind::Composite, WF)
            .member("invoice", &a)
            .member("po", &b)
            .predicate(areev_trigger::predicate::to_value(&cond).unwrap()),
    );

    let r = rig.evaluator(None).run(&opts()).unwrap();
    // `contains`, not `starts_with`: the report names which declaration is
    // unusable before the reason, which matters once more than one exists.
    assert!(
        r.errors.iter().any(|e| e.contains("TRG-E008")),
        "expected TRG-E008, got {:?}",
        r.errors
    );
}

#[test]
fn a_partial_match_expires_with_its_window() {
    // Monday's A must not pair with Tuesday's B. Argo Events' failure mode,
    // avoided by keying partials with a per-match expiry rather than needing a
    // reset cron.
    let rig = Rig::new();
    let a = rig.declare(Trigger::new(TriggerKind::Interval, WF).interval_secs(3600));
    let b = rig.declare(Trigger::new(TriggerKind::Interval, WF).interval_secs(86_400));

    let cond = areev_trigger::predicate::parse_predicate("slow = true AND fast = true").unwrap();
    let composite = rig.declare(
        Trigger::new(TriggerKind::Composite, WF)
            .member("fast", &a)
            .member("slow", &b)
            .predicate(areev_trigger::predicate::to_value(&cond).unwrap())
            .window_ms(60_000), // one minute
    );

    let ev = rig.evaluator(None);
    ev.run(&opts()).unwrap(); // both members fire, gate completes
    let fired_once = rig.state(&composite).last_fired_at;
    assert!(fired_once.is_some());

    // Far past the window: any partial left behind must be gone.
    rig.clock.advance(3_600_000);
    ev.run(&opts()).unwrap();
    let st = rig.state(&composite);
    assert!(
        st.partials.values().all(|p| rig.clock.now_ms() - p.started_ms <= 60_000),
        "stale partials must be pruned, found {:?}",
        st.partials
    );
}

#[test]
fn correlation_keeps_unrelated_work_apart() {
    // Two members firing about *different* entities must not satisfy one gate.
    let rig = Rig::new();
    let a = rig.declare(
        Trigger::new(TriggerKind::Polling, WF)
            .connector("src-a")
            .interval_secs(1)
            .dedup_key("/id"),
    );
    let cond = areev_trigger::predicate::parse_predicate("src = true").unwrap();
    let composite = rig.declare(
        Trigger::new(TriggerKind::Composite, WF)
            .member("src", &a)
            .predicate(areev_trigger::predicate::to_value(&cond).unwrap())
            .correlate("/thread"),
    );

    let conn = FakeConnector::new(vec![
        ok(json!([]), Some("c1"), false),
        ok(
            json!([
                { "id": "1", "payload": { "id": "1", "thread": "T1" } },
                { "id": "2", "payload": { "id": "2", "thread": "T2" } }
            ]),
            Some("c2"),
            false,
        ),
    ]);
    let ev = rig.evaluator(Some(conn as Arc<dyn HostToolExecutor>));
    ev.run(&opts()).unwrap(); // seed
    rig.clock.advance(1_000);
    ev.run(&opts()).unwrap();

    let st = rig.state(&composite);
    let keys: Vec<_> = st.partials.keys().cloned().collect();
    assert!(
        keys.contains(&"T1".to_string()) || st.last_fired_at.is_some(),
        "correlation values become distinct partial keys, got {keys:?}"
    );
}

#[test]
fn a_once_trigger_fires_exactly_once_and_then_never_again() {
    // `once` means once. After firing, `advance_after_firing` has no next
    // instant to give — and "no next instant" must not be read as "never
    // evaluated, so fire now", which is what an absent `next_due_at` means for
    // every other clocked kind.
    let rig = Rig::new();
    let h = rig.declare(Trigger::new(TriggerKind::Once, WF).at_ms(T0 + 1_000));
    let ev = rig.evaluator(None);

    // Before its instant: not due.
    let early = ev.run(&opts()).unwrap();
    assert_eq!(early.claimed, 0, "a once trigger must wait for its instant");

    // At its instant: fires.
    rig.clock.advance(1_000);
    let fired = ev.run(&opts()).unwrap();
    assert_eq!(fired.claimed, 1);
    assert_eq!(fired.runs_started, 1);

    // Every pass after: nothing, forever.
    for step in 0..5 {
        rig.clock.advance(60_000);
        let again = ev.run(&opts()).unwrap();
        assert_eq!(again.claimed, 0, "re-fired on pass {step}");
        assert_eq!(again.runs_started, 0, "re-fired on pass {step}");
    }
    assert_eq!(rig.state(&h).last_fired_at, Some(T0 + 1_000), "only the first firing counts");
}

#[test]
fn a_cron_trigger_waits_for_its_boundary_rather_than_firing_on_declaration() {
    // `0 9 * * *` must not fire at midnight merely because it was just
    // declared. An absent `next_due_at` means "never evaluated" — for a
    // relative cadence that means fire now, for an absolute schedule it does
    // not.
    let rig = Rig::new(); // clock is at T0 = 2026-01-01T00:00:00Z
    let h = rig.declare(Trigger::new(TriggerKind::Schedule, WF).cron("0 9 * * *"));
    let ev = rig.evaluator(None);

    let early = ev.run(&opts()).unwrap();
    assert_eq!(early.claimed, 0, "midnight is not 9am");

    // 09:00 the same day.
    rig.clock.advance(9 * 3_600_000);
    let fired = ev.run(&opts()).unwrap();
    assert_eq!(fired.claimed, 1, "should fire on the boundary");
    assert!(!rig.state(&h).exhausted, "a recurring schedule is never exhausted");

    // An hour later it is not due again; the next boundary is tomorrow.
    rig.clock.advance(3_600_000);
    assert_eq!(ev.run(&opts()).unwrap().claimed, 0);
}

#[test]
fn an_interval_trigger_still_fires_immediately_on_first_evaluation() {
    // The other half of the same rule: a relative cadence has no baseline to
    // wait out, so it fires at once — the loop gate's behaviour, and what makes
    // a polling trigger seed its cursor promptly.
    let rig = Rig::new();
    rig.declare(Trigger::new(TriggerKind::Interval, WF).interval_secs(3600));
    assert_eq!(rig.evaluator(None).run(&opts()).unwrap().claimed, 1);
}

#[test]
fn display_helpers_do_not_panic_on_a_short_or_non_ascii_workflow() {
    // A declaration can arrive by bundle import from an implementation we did
    // not write, so a `&s[..12]` in a status line is a crash waiting for a
    // grain we never authored.
    let rig = Rig::new();
    for wf in ["ab", "", "日本語のワークフロー"] {
        let t = Trigger::new(TriggerKind::Interval, WF).interval_secs(60).created_at(T0).namespace(NS);
        let mut t = t;
        t.workflow = wf.to_string();
        // `add` is the raw path — `incoherence()` would refuse an empty
        // workflow at the CLI, but a foreign grain never passed through it.
        let _ = rig.facade.with_store(|m| m.add(&t));
    }
    // The point is that this returns rather than panics.
    let status = rig.evaluator(None).status().unwrap();
    assert!(!status.is_empty());
}

// ---- what a firing record has to explain ----------------------------------

/// Every Observation this evaluator has written, newest first.
fn firings(rig: &Rig) -> Vec<areev_core::format::deserialize::DeserializedGrain> {
    rig.facade
        .with_store(|m| {
            m.recent_live_scoped(
                &[areev_trigger::eval::TRIGGER_NS.to_string()],
                Some(areev_core::types::GrainType::Observation),
                10,
            )
        })
        .unwrap()
}

#[test]
fn a_firing_that_identified_nothing_records_why() {
    // Regression: the record carried items/runs_started/duplicates and dropped
    // every count that explains them. "items 5, runs_started 0, duplicates 0"
    // reads as a mystery, and the zero duplicates actively misleads — it says
    // the items were not skipped as duplicates without saying why they were
    // skipped at all.
    let rig = Rig::new();
    rig.declare(
        Trigger::new(TriggerKind::Polling, WF)
            .connector("gmail")
            .interval_secs(60)
            .dedup_key("/message_id"),
    );
    let conn = FakeConnector::new(vec![
        ok(json!([]), Some("c1"), false),
        ok(
            json!([
                { "id": "a", "payload": { "subject": "one" } },
                { "id": "b", "payload": { "subject": "two" } }
            ]),
            Some("c2"),
            false,
        ),
    ]);
    let ev = rig.evaluator(Some(conn as Arc<dyn HostToolExecutor>));
    ev.run(&opts()).unwrap();
    rig.clock.advance(60_000);
    ev.run(&opts()).unwrap();

    let obs = firings(&rig);
    let latest = &obs[0];
    assert_eq!(latest.get_i64("items"), Some(2));
    assert_eq!(latest.get_i64("runs_started"), Some(0));
    assert_eq!(
        latest.get_i64("unidentifiable"),
        Some(2),
        "the record must say why two items produced no runs"
    );
}

#[test]
fn an_ordinary_firing_does_not_carry_the_diagnostic_counts() {
    // The counterpart to the above: emitted only when non-zero, so a healthy
    // firing's record stays exactly as small as it was.
    let rig = Rig::new();
    rig.declare(Trigger::new(TriggerKind::Interval, WF).interval_secs(60));
    rig.evaluator(None).run(&opts()).unwrap();

    let obs = firings(&rig);
    assert_eq!(obs[0].get_i64("runs_started"), Some(1));
    assert_eq!(obs[0].get_i64("unidentifiable"), None);
    assert_eq!(obs[0].get_i64("ingested"), None);
}

#[test]
fn a_delivery_that_names_nothing_is_still_journaled() {
    // Regression: `deliver` returned before reaching the journal, so a webhook
    // whose payload shape had drifted left no evidence it had ever arrived.
    // That is the case the record is most needed for.
    let rig = Rig::new();
    let h = rig.declare(Trigger::new(TriggerKind::Webhook, WF).dedup_key("/message_id"));
    let ev = rig.evaluator(None);

    let report = ev.deliver(&h, json!({ "subject": "no message_id here" })).unwrap();
    assert_eq!(report.runs_started, 0);
    assert_eq!(report.unidentifiable, 1);

    let obs = firings(&rig);
    assert_eq!(obs.len(), 1, "an unusable delivery still has to leave a record");
    assert_eq!(obs[0].get_str("trigger"), Some(h.as_str()));
    assert_eq!(obs[0].get_i64("unidentifiable"), Some(1));
    assert_eq!(obs[0].get_str("note"), Some("delivered"));
}

#[test]
fn a_composite_naming_an_undeclared_member_is_refused_at_declaration() {
    // Firing already refused this with TRG-E008, but only once the trigger was
    // due — so the declaration sat in the memory looking live. A gate that
    // names a member it does not declare can never be satisfied, and a dead
    // trigger's only symptom is nothing happening.
    let cond =
        areev_trigger::predicate::parse_predicate("invoice = true AND ghost = true").unwrap();
    let t = Trigger::new(TriggerKind::Composite, WF)
        .member("invoice", WF)
        .member("purchase_order", WF)
        .predicate(areev_trigger::predicate::to_value(&cond).unwrap());

    let err = areev_trigger::schedule::validate(&t).unwrap_err();
    assert_eq!(err.code(), "TRG-E008");
    assert!(err.to_string().contains("ghost"), "{err}");

    // The same declaration with every member declared is accepted.
    let ok_cond =
        areev_trigger::predicate::parse_predicate("invoice = true AND purchase_order = true")
            .unwrap();
    let good = Trigger::new(TriggerKind::Composite, WF)
        .member("invoice", WF)
        .member("purchase_order", WF)
        .predicate(areev_trigger::predicate::to_value(&ok_cond).unwrap());
    assert!(areev_trigger::schedule::validate(&good).is_ok());
}

/// A declaration that can never fire is reported as such, not as "waiting".
///
/// #67: an unusable trigger was folded into `not due`, where it is
/// indistinguishable from a healthy one waiting its turn — so the work simply
/// never happened and every report stayed green. Write-path validation alone
/// cannot close this: `Rig::declare` writes straight through the store, which
/// is exactly the shape of a declaration arriving by bundle import from an
/// implementation that validated differently, or one written before the check
/// existed. The evaluator has to notice on its own.
#[test]
fn an_unusable_declaration_is_reported_distinctly_not_as_waiting() {
    let rig = Rig::new();

    // Stored without validation, as a bundle import would be.
    let bad = rig.declare(
        Trigger::new(TriggerKind::Schedule, WF)
            .cron("0 9 * * *")
            .config(json!({ "int:timezone": "Asia/Kolkata" })),
    );
    let good = rig.declare(Trigger::new(TriggerKind::Interval, WF).interval_secs(60));

    let ev = rig.evaluator(None);

    let status = ev.status().unwrap();
    let bad_row = status.iter().find(|s| s.trigger == bad).unwrap();
    let why = bad_row.unusable.as_deref().expect("the reason must be reported, not inferred");
    assert!(why.contains("TRG-E006"), "the reason must name the code: {why}");
    assert!(!bad_row.due, "an unusable declaration must never be reported as due");

    // The healthy one is unaffected — this must not become a blanket alarm.
    let good_row = status.iter().find(|s| s.trigger == good).unwrap();
    assert!(good_row.unusable.is_none());
    assert!(good_row.due);

    // And the pass counts it apart from `not due`, with the reason surfaced.
    let report = ev.run(&opts()).unwrap();
    assert_eq!(report.unusable, 1, "an unusable trigger must not hide in not-due");
    assert_eq!(report.claimed, 1, "only the healthy trigger fires");
    assert!(
        report.errors.iter().any(|e| e.contains("TRG-E006")),
        "the reason must reach the operator: {:?}",
        report.errors
    );
}

// ---- declared context (#85) ------------------------------------------------

/// A starter that keeps the input it was handed, so a test can look inside.
#[derive(Default)]
struct CapturingStarter {
    inputs: Mutex<Vec<Value>>,
    run_ids: Mutex<Vec<String>>,
}

impl RunStarter for CapturingStarter {
    fn start(&self, _workflow: &str, run_id: &str, input: Value) -> StartResult {
        self.inputs.lock().unwrap().push(input);
        self.run_ids.lock().unwrap().push(run_id.to_string());
        StartResult::Started
    }
}

/// A trigger naming a `context_query` fires with the saved query's result in
/// the run input — the evaluator assembles it, because it is the one party
/// that already holds the memory the run's own tools cannot open.
#[test]
fn a_context_query_trigger_carries_assembled_context() {
    let rig = Rig::new();
    // Knowledge the run should see, and the saved query that selects it.
    rig.facade
        .with_store(|m| {
            m.add(
                &areev_core::types::Fact::new("acme", "deploy_target", "k8s")
                    .namespace(NS)
                    .created_at(T0),
            )
        })
        .unwrap();
    let ex = areev_cal::CalExecutor::new(areev_cal::CalExecutorConfig::default());
    ex.execute(
        r#"DEFINE QUERY "triage_ctx"() AS { RECALL facts WHERE subject = "acme" AND namespace = "ops" }"#,
        rig.facade.as_ref(),
    )
    .expect("DEFINE QUERY must succeed");

    let h = rig.declare(
        Trigger::new(TriggerKind::Interval, WF).interval_secs(120).context_query("triage_ctx"),
    );
    let starter = Arc::new(CapturingStarter::default());
    let mut ev = rig.evaluator(None);
    ev.starter = Some(Arc::clone(&starter) as Arc<dyn RunStarter>);

    let r = ev.run(&opts()).unwrap();
    assert_eq!(r.runs_started, 1, "errors: {:?}", r.errors);
    let inputs = starter.inputs.lock().unwrap();
    let ctx = &inputs[0]["context"];
    assert!(
        ctx.to_string().contains("deploy_target"),
        "the run input must carry the assembled context: {}",
        inputs[0]
    );
    assert_eq!(inputs[0]["trigger"], json!(h), "the declared payload survives beside it");
}

/// Fail closed: a trigger that declared context must not fire blind. A
/// missing saved query refuses the firing (retried on the evaluator's normal
/// cadence) instead of starting a run without the promised context.
#[test]
fn a_missing_context_query_refuses_the_firing() {
    let rig = Rig::new();
    rig.declare(
        Trigger::new(TriggerKind::Interval, WF).interval_secs(120).context_query("not_defined"),
    );
    let starter = Arc::new(CapturingStarter::default());
    let mut ev = rig.evaluator(None);
    ev.starter = Some(Arc::clone(&starter) as Arc<dyn RunStarter>);

    let r = ev.run(&opts()).unwrap();
    assert_eq!(r.runs_started, 0, "a declared-context trigger must not fire blind");
    assert!(
        r.errors.iter().any(|e| e.contains("not_defined")),
        "the refusal names the query: {:?}",
        r.errors
    );
    assert!(starter.inputs.lock().unwrap().is_empty(), "no run input was handed over");
}

// ---- parameterized declared context (#92) ----------------------------------

/// The declaration binds saved-query parameters from the FIRING ITEM:
/// `--context-query 'thread_ctx($session = /session)'`. The evaluator
/// resolves the pointer against the item's payload and runs the query with
/// that binding, so a message-driven run can carry its own thread.
#[test]
fn a_context_query_binds_parameters_from_the_firing_item() {
    let rig = Rig::new();
    // The thread the firing message belongs to, and a decoy thread that
    // must NOT be selected.
    rig.facade
        .with_store(|m| {
            m.add(
                &areev_core::types::Fact::new("sess-42", "corrects", "invoice-7")
                    .namespace(NS)
                    .created_at(T0),
            )?;
            m.add(
                &areev_core::types::Fact::new("sess-99", "corrects", "invoice-9")
                    .namespace(NS)
                    .created_at(T0),
            )
        })
        .unwrap();
    let ex = areev_cal::CalExecutor::new(areev_cal::CalExecutorConfig::default());
    ex.execute(
        r#"DEFINE QUERY "thread_ctx"($session) AS { RECALL facts WHERE subject = $session AND namespace = "ops" }"#,
        rig.facade.as_ref(),
    )
    .expect("DEFINE QUERY must succeed");

    rig.declare(
        Trigger::new(TriggerKind::Polling, WF)
            .connector("gmail")
            .interval_secs(60)
            .dedup_key("/id")
            .context_query("thread_ctx($session = /session)"),
    );
    let conn = FakeConnector::new(vec![
        ok(json!([]), Some("c1"), false),
        ok(
            json!([{ "id": "m-1", "payload": { "id": "m-1", "session": "sess-42" } }]),
            Some("c2"),
            false,
        ),
    ]);
    let starter = Arc::new(CapturingStarter::default());
    let ev = Evaluator {
        facade: Arc::clone(&rig.facade),
        clock: Arc::clone(&rig.clock) as Arc<dyn Clock>,
        connector: Some(conn as Arc<dyn HostToolExecutor>),
        starter: Some(Arc::clone(&starter) as Arc<dyn RunStarter>),
        credentials: Default::default(),
        ns: NS.into(),
        principal: "user:test".into(),
    };

    ev.run(&opts()).unwrap(); // priming poll seeds the cursor
    rig.clock.advance(60_000);
    let r = ev.run(&opts()).unwrap();
    assert_eq!(r.runs_started, 1, "errors: {:?}", r.errors);

    let inputs = starter.inputs.lock().unwrap();
    let ctx = inputs[0]["context"].to_string();
    assert!(
        ctx.contains("invoice-7"),
        "the bound thread's fact must be in the context: {}",
        inputs[0]
    );
    assert!(
        !ctx.contains("invoice-9"),
        "the decoy thread must be EXCLUDED — the binding scoped the query: {}",
        inputs[0]
    );
}

/// Fail closed, with `--dedup-key`'s precedent: a pointer that does not
/// resolve against the firing item refuses the firing rather than running
/// the query with a hole in it.
#[test]
fn an_unresolvable_context_pointer_refuses_the_firing() {
    let rig = Rig::new();
    let ex = areev_cal::CalExecutor::new(areev_cal::CalExecutorConfig::default());
    ex.execute(
        r#"DEFINE QUERY "thread_ctx"($session) AS { RECALL facts WHERE subject = $session AND namespace = "ops" }"#,
        rig.facade.as_ref(),
    )
    .unwrap();
    rig.declare(
        Trigger::new(TriggerKind::Polling, WF)
            .connector("gmail")
            .interval_secs(60)
            .dedup_key("/id")
            .context_query("thread_ctx($session = /session)"),
    );
    let conn = FakeConnector::new(vec![
        ok(json!([]), Some("c1"), false),
        // No `session` key: the declared pointer cannot resolve.
        ok(json!([{ "id": "m-1", "payload": { "id": "m-1" } }]), Some("c2"), false),
    ]);
    let starter = Arc::new(CapturingStarter::default());
    let ev = Evaluator {
        facade: Arc::clone(&rig.facade),
        clock: Arc::clone(&rig.clock) as Arc<dyn Clock>,
        connector: Some(conn as Arc<dyn HostToolExecutor>),
        starter: Some(Arc::clone(&starter) as Arc<dyn RunStarter>),
        credentials: Default::default(),
        ns: NS.into(),
        principal: "user:test".into(),
    };

    ev.run(&opts()).unwrap();
    rig.clock.advance(60_000);
    let r = ev.run(&opts()).unwrap();
    assert_eq!(r.runs_started, 0, "a run must not start with a hole in its context");
    assert!(
        r.errors.iter().any(|e| e.contains("/session")),
        "the refusal names the pointer: {:?}",
        r.errors
    );
    assert!(starter.inputs.lock().unwrap().is_empty());
}

/// A pointer landing on an object or array refuses too — parameters bind
/// scalars, and stringifying a subtree silently would corrupt the query.
#[test]
fn a_non_scalar_context_binding_refuses_the_firing() {
    let rig = Rig::new();
    let ex = areev_cal::CalExecutor::new(areev_cal::CalExecutorConfig::default());
    ex.execute(
        r#"DEFINE QUERY "thread_ctx"($session) AS { RECALL facts WHERE subject = $session AND namespace = "ops" }"#,
        rig.facade.as_ref(),
    )
    .unwrap();
    rig.declare(
        Trigger::new(TriggerKind::Polling, WF)
            .connector("gmail")
            .interval_secs(60)
            .dedup_key("/id")
            .context_query("thread_ctx($session = /email)"),
    );
    let conn = FakeConnector::new(vec![
        ok(json!([]), Some("c1"), false),
        ok(
            json!([{ "id": "m-1", "payload": { "id": "m-1", "email": { "from": "a@b.c" } } }]),
            Some("c2"),
            false,
        ),
    ]);
    let starter = Arc::new(CapturingStarter::default());
    let ev = Evaluator {
        facade: Arc::clone(&rig.facade),
        clock: Arc::clone(&rig.clock) as Arc<dyn Clock>,
        connector: Some(conn as Arc<dyn HostToolExecutor>),
        starter: Some(Arc::clone(&starter) as Arc<dyn RunStarter>),
        credentials: Default::default(),
        ns: NS.into(),
        principal: "user:test".into(),
    };

    ev.run(&opts()).unwrap();
    rig.clock.advance(60_000);
    let r = ev.run(&opts()).unwrap();
    assert_eq!(r.runs_started, 0);
    assert!(
        r.errors.iter().any(|e| e.contains("scalar")),
        "the refusal says why: {:?}",
        r.errors
    );
}

/// A malformed spelling is refused at authoring time by
/// `schedule::validate` — a trigger that can never fire must not be stored.
#[test]
fn a_malformed_context_query_spelling_is_refused_at_declaration() {
    let t = Trigger::new(TriggerKind::Interval, WF)
        .interval_secs(60)
        .context_query("ctx($session = session)"); // pointer missing '/'
    let err = areev_trigger::schedule::validate(&t).expect_err("must refuse");
    assert!(err.to_string().contains("context_query"), "{err}");
}

// ---- connector blobs (#93) -------------------------------------------------

/// The whole #93 loop: the connector hands back a blob, the EVALUATOR (the
/// party holding the writer) stores it in the CAS, rewrites the payload
/// reference to the address, and attaches a `content_refs` entry to the
/// Event it writes — so the trigger path stores attachments exactly like
/// the host-driven path.
#[test]
fn connector_blobs_land_in_the_cas_and_the_payload_is_rewritten() {
    let rig = Rig::new();
    let h = rig.declare(
        Trigger::new(TriggerKind::Polling, WF)
            .connector("gmail")
            .interval_secs(60)
            .dedup_key("/id"),
    );
    // "Zm9vYmFy" = b"foobar".
    let conn = FakeConnector::new(vec![
        ok(json!([]), Some("c1"), false),
        ok(
            json!([{
                "id": "m-1",
                "payload": {
                    "id": "m-1",
                    "attachments": [{ "filename": "inv.pdf", "blob": "@0" }],
                },
                "blobs": [{ "filename": "inv.pdf", "mime": "application/pdf", "b64": "Zm9vYmFy" }],
            }]),
            Some("c2"),
            false,
        ),
    ]);
    let starter = Arc::new(CapturingStarter::default());
    let ev = Evaluator {
        facade: Arc::clone(&rig.facade),
        clock: Arc::clone(&rig.clock) as Arc<dyn Clock>,
        connector: Some(conn as Arc<dyn HostToolExecutor>),
        starter: Some(Arc::clone(&starter) as Arc<dyn RunStarter>),
        credentials: Default::default(),
        ns: NS.into(),
        principal: "user:test".into(),
    };

    ev.run(&opts()).unwrap();
    rig.clock.advance(60_000);
    let r = ev.run(&opts()).unwrap();
    assert_eq!(r.runs_started, 1, "errors: {:?}", r.errors);

    // The run input carries an ordinary cas:// address, not bytes.
    let inputs = starter.inputs.lock().unwrap();
    let uri = inputs[0]["item"]["attachments"][0]["blob"]
        .as_str()
        .expect("blob ref must be a string")
        .to_string();
    assert!(uri.starts_with("cas://sha256:"), "rewritten to an address: {uri}");
    assert!(
        !inputs[0].to_string().contains("Zm9vYmFy"),
        "the base64 must not reach the run input"
    );

    // The bytes are retrievable by address, and dedup is content-addressed.
    let bytes = rig.facade.with_store(|m| m.get_blob(&uri)).unwrap();
    assert_eq!(bytes, b"foobar");

    // The Event references the blob through content_refs — what keeps it
    // alive through GC and reachable by erasure.
    let run_id = starter.run_ids.lock().unwrap()[0].clone();
    let grains = rig.facade.with_store(|m| m.run_grains(NS, &run_id, 0, 10)).unwrap();
    assert!(
        grains.iter().any(|(_, g)| format!("{:?}", g.fields).contains(&uri)),
        "the ingest Event must carry a content_ref to {uri}; got {} grain(s): {h}",
        grains.len()
    );
}

/// A blob over budget refuses the WHOLE poll — TRG-E011, cursor unmoved —
/// rather than truncating or dropping the attachment: a silently dropped
/// attachment is an invoice posting without evidence, and a lost item is
/// worse.
#[test]
fn an_over_budget_blob_refuses_the_whole_poll_and_keeps_the_cursor() {
    let rig = Rig::new();
    let h = rig.declare(
        Trigger::new(TriggerKind::Polling, WF)
            .connector("gmail")
            .interval_secs(60)
            .dedup_key("/id"),
    );
    let conn = FakeConnector::new(vec![
        ok(json!([]), Some("c1"), false),
        ok(
            json!([{
                "id": "m-1",
                "payload": { "id": "m-1", "a": { "blob": "@0" } },
                "blobs": [{ "filename": "big.bin", "b64": "Zm9vYmFy" }],
            }]),
            Some("c2"),
            false,
        ),
    ]);
    let starter = Arc::new(CapturingStarter::default());
    let ev = Evaluator {
        facade: Arc::clone(&rig.facade),
        clock: Arc::clone(&rig.clock) as Arc<dyn Clock>,
        connector: Some(conn as Arc<dyn HostToolExecutor>),
        starter: Some(Arc::clone(&starter) as Arc<dyn RunStarter>),
        credentials: Default::default(),
        ns: NS.into(),
        principal: "user:test".into(),
    };

    ev.run(&opts()).unwrap(); // priming seeds cursor c1
    rig.clock.advance(60_000);
    let tight = EvalOptions { max_blob_bytes_per_item: 3, ..opts() };
    let r = ev.run(&tight).unwrap();
    assert_eq!(r.runs_started, 0);
    assert!(
        r.errors.iter().any(|e| e.contains("TRG-E011")),
        "the refusal carries its code: {:?}",
        r.errors
    );
    let st = rig.state(&h);
    assert_eq!(st.cursor.as_deref(), Some("c1"), "the cursor must not advance past evidence");
    assert!(st.consecutive_failures > 0, "the failure is counted for backoff");
    assert!(starter.inputs.lock().unwrap().is_empty(), "no run started");
}

/// A dangling `"@N"` payload reference is a connector bug, not something to
/// hand to a run: the poll refuses whole.
#[test]
fn a_dangling_blob_reference_refuses_the_poll() {
    let rig = Rig::new();
    let h = rig.declare(
        Trigger::new(TriggerKind::Polling, WF)
            .connector("gmail")
            .interval_secs(60)
            .dedup_key("/id"),
    );
    let conn = FakeConnector::new(vec![
        ok(json!([]), Some("c1"), false),
        ok(
            json!([{
                "id": "m-1",
                "payload": { "id": "m-1", "a": { "blob": "@3" } },
                "blobs": [{ "b64": "Zm9vYmFy" }],
            }]),
            Some("c2"),
            false,
        ),
    ]);
    let ev = rig_polling_evaluator(&rig, conn);

    ev.run(&opts()).unwrap();
    rig.clock.advance(60_000);
    let r = ev.run(&opts()).unwrap();
    assert_eq!(r.runs_started, 0);
    assert!(r.errors.iter().any(|e| e.contains("TRG-E011")), "{:?}", r.errors);
    assert_eq!(rig.state(&h).cursor.as_deref(), Some("c1"));
}

/// Undecodable base64 refuses too — salvaging a corrupt attachment would
/// store evidence that matches nothing.
#[test]
fn undecodable_blob_base64_refuses_the_poll() {
    let rig = Rig::new();
    rig.declare(
        Trigger::new(TriggerKind::Polling, WF)
            .connector("gmail")
            .interval_secs(60)
            .dedup_key("/id"),
    );
    let conn = FakeConnector::new(vec![
        ok(json!([]), Some("c1"), false),
        ok(
            json!([{
                "id": "m-1",
                "payload": { "id": "m-1" },
                "blobs": [{ "filename": "x", "b64": "!!! not base64 !!!" }],
            }]),
            Some("c2"),
            false,
        ),
    ]);
    let ev = rig_polling_evaluator(&rig, conn);

    ev.run(&opts()).unwrap();
    rig.clock.advance(60_000);
    let r = ev.run(&opts()).unwrap();
    assert_eq!(r.runs_started, 0);
    assert!(r.errors.iter().any(|e| e.contains("TRG-E011")), "{:?}", r.errors);
}

/// Shared constructor for the blob failure tests.
fn rig_polling_evaluator(rig: &Rig, conn: Arc<FakeConnector>) -> Evaluator {
    Evaluator {
        facade: Arc::clone(&rig.facade),
        clock: Arc::clone(&rig.clock) as Arc<dyn Clock>,
        connector: Some(conn as Arc<dyn HostToolExecutor>),
        starter: None,
        credentials: Default::default(),
        ns: NS.into(),
        principal: "user:test".into(),
    }
}
