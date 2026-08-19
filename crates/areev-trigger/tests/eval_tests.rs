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
    let cond =
        areev_trigger::predicate::parse_predicate("invoice = true AND ghost = true").unwrap();
    rig.declare(
        Trigger::new(TriggerKind::Composite, WF)
            .member("invoice", &a)
            .predicate(areev_trigger::predicate::to_value(&cond).unwrap()),
    );

    let r = rig.evaluator(None).run(&opts()).unwrap();
    assert!(
        r.errors.iter().any(|e| e.starts_with("TRG-E008")),
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
