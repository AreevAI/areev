//! The 1.9.0 governance wave: model and engine pins (#287, #288),
//! initiator-aware separation of duties (#293), confirmation asks (#294),
//! run-level effect ceilings (#295), concurrency slots (#296), configurable
//! leases (#299) and host-qualified lease holders (#300).

use areev_cal::AreevFacade;
use areev_core::error::Hash;
use areev_core::types::{Grain, Tool, ToolKind, Workflow};
use areev_llm::{
    StopReason, ToolCallLlm, ToolCallRequest, ToolCallResponse, Usage,
};
use areev_run::{ExecResult, HostToolExecutor, RunOptions, RunSession, Runner, ScriptedClock};
use areev_run_core::{RunError, RunOutcome};
use areev_store::Areev;
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use tempfile::TempDir;

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

struct Echo;
impl HostToolExecutor for Echo {
    fn execute(&self, tool: &str, _h: &str, _i: &Value, _k: &str) -> ExecResult {
        ExecResult::Ok(json!({ tool: true }))
    }
}

/// A transport with a scripted identity and an optional price.
struct FakeLlm {
    provider: &'static str,
    model: String,
    region: Option<String>,
    tag: Option<String>,
    price: Option<u64>,
    /// Each call returns the next scripted response, then repeats the last.
    script: Mutex<Vec<ToolCallResponse>>,
}

impl FakeLlm {
    fn new(provider: &'static str, model: &str) -> Self {
        FakeLlm {
            provider,
            model: model.into(),
            region: None,
            tag: None,
            price: None,
            script: Mutex::new(Vec::new()),
        }
    }
    fn priced(mut self, usd_micros: u64) -> Self {
        self.price = Some(usd_micros);
        self
    }
    fn region(mut self, r: &str) -> Self {
        self.region = Some(r.into());
        self
    }
    fn tag(mut self, t: &str) -> Self {
        self.tag = Some(t.into());
        self
    }
}

impl ToolCallLlm for FakeLlm {
    fn model(&self) -> &str {
        &self.model
    }
    fn provider(&self) -> &'static str {
        self.provider
    }
    fn region(&self) -> Option<&str> {
        self.region.as_deref()
    }
    fn pin_tag(&self) -> Option<&str> {
        self.tag.as_deref()
    }
    fn price_usd_micros(&self, _u: &Usage) -> Option<u64> {
        self.price
    }
    fn call(&self, _req: &ToolCallRequest<'_>) -> std::result::Result<ToolCallResponse, areev_llm::ToolCallError> {
        let mut script = self.script.lock().unwrap();
        if script.len() > 1 {
            Ok(script.remove(0))
        } else if let Some(one) = script.first() {
            Ok(one.clone())
        } else {
            Ok(ToolCallResponse::new(
                Some("done".into()),
                vec![],
                StopReason::EndTurn,
                Usage { input_tokens: 10, output_tokens: 5, cache_read_tokens: None },
            ))
        }
    }
}

struct Rig {
    _dir: TempDir,
    facade: Arc<AreevFacade>,
}

impl Rig {
    fn new() -> Self {
        let dir = TempDir::new().unwrap();
        let m = Areev::open(dir.path().join("m.db").to_str().unwrap()).unwrap();
        Rig { _dir: dir, facade: Arc::new(AreevFacade::new(m)) }
    }

    /// `client` nodes become Client tools; `confirm` nodes become Client
    /// tools declaring `ask_kind: "confirmation"`.
    fn plan(&self, nodes: &[&str], edges: &[(&str, &str)], client: &[&str], confirm: &[&str]) -> Hash {
        let mut wf = Workflow::new(nodes.iter().map(|s| s.to_string()).collect());
        for (a, b) in edges {
            wf = wf.edge(a, b);
        }
        for n in nodes {
            let mut def = Tool::new(n)
                .kind(ToolKind::Definition)
                .tool_description("test tool")
                .created_at(500)
                .namespace("ops");
            if client.contains(n) || confirm.contains(n) {
                def = def.executor_kind(areev_core::types::ExecutorKind::Client);
            }
            if confirm.contains(n) {
                def.common
                    .extra_fields
                    .insert("ask_kind".into(), json!("confirmation"));
            }
            let dh = self.facade.with_store(|m| m.add(&def)).unwrap();
            wf = wf.bind(n, &dh.to_hex());
        }
        let wf = wf.created_at(600).namespace("ops");
        self.facade.with_store(|m| m.add(&wf)).unwrap()
    }

    fn runner_with(&self, llm: Option<Arc<dyn ToolCallLlm>>, principal: &str) -> Runner {
        Runner {
            facade: Arc::clone(&self.facade),
            clock: Arc::new(ScriptedClock::new(clocks())),
            executor: Arc::new(Echo) as Arc<dyn HostToolExecutor>,
            llm,
            observer: None,
            ns: "ops".into(),
            principal: principal.into(),
        }
    }

    fn runner(&self) -> Runner {
        self.runner_with(None, "user:runner")
    }
}

fn clocks() -> Vec<u64> {
    (0..400).map(|i| 1_755_000_000_000 + i * 10).collect()
}

fn opts() -> RunOptions {
    RunOptions { workers: 2, ..Default::default() }
}

fn manifest_of(rig: &Rig, run_id: &str) -> areev_run::RunManifest {
    rig.facade
        .with_store(|m| areev_run::RunManifest::load(m, run_id))
        .unwrap()
}

// ---------------------------------------------------------------------------
// #287 — the model is frozen with the run
// ---------------------------------------------------------------------------

#[test]
fn a_tool_only_run_pins_no_model_and_resumes_under_anything() {
    let rig = Rig::new();
    let plan = rig.plan(&["a", "b"], &[("a", "b")], &["b"], &[]);
    let runner = rig.runner();
    runner.start(&plan, "run-nopin", json!({}), &opts()).unwrap();
    let m = manifest_of(&rig, "run-nopin");
    assert!(m.llm.is_none(), "no transport means no pin");
    // And a pinned engine, which is separate.
    assert_eq!(
        m.engine.as_ref().unwrap().scheduler_epoch,
        areev_run_core::SCHEDULER_EPOCH
    );
}

#[test]
fn a_parked_run_refuses_to_finish_on_a_different_model() {
    let rig = Rig::new();
    let plan = rig.plan(&["auto", "approve"], &[("auto", "approve")], &["approve"], &[]);
    let m1: Arc<dyn ToolCallLlm> = Arc::new(FakeLlm::new("anthropic", "m1"));
    let runner = rig.runner_with(Some(Arc::clone(&m1)), "user:runner");
    let session = runner.start(&plan, "run-pin", json!({}), &opts()).unwrap();
    assert!(matches!(session, RunSession::Parked { .. }), "parks on the ask");

    let pinned = manifest_of(&rig, "run-pin").llm.expect("a pin");
    assert_eq!(pinned.provider, "anthropic");
    assert_eq!(pinned.model, "m1");

    // Days later, a different model.
    let m2: Arc<dyn ToolCallLlm> = Arc::new(FakeLlm::new("anthropic", "m2"));
    let other = rig.runner_with(Some(m2), "user:runner");
    let err = other.resume("run-pin", &opts()).unwrap_err();
    assert_eq!(err.code(), "RUN-E025", "{err}");
    assert!(err.to_string().contains("m1") && err.to_string().contains("m2"), "{err}");

    // The SAME model continues.
    let same = rig.runner_with(Some(Arc::clone(&m1)), "user:runner");
    assert!(same.resume("run-pin", &opts()).is_ok());
}

#[test]
fn a_changed_region_or_tag_is_refused_the_same_way() {
    for (label, llm) in [
        (
            "region",
            FakeLlm::new("anthropic", "m1").region("eu"),
        ),
        ("tag", FakeLlm::new("anthropic", "m1").tag("cfg-v2")),
    ] {
        let rig = Rig::new();
        let plan = rig.plan(&["auto", "approve"], &[("auto", "approve")], &["approve"], &[]);
        let base: Arc<dyn ToolCallLlm> = Arc::new(FakeLlm::new("anthropic", "m1"));
        rig.runner_with(Some(base), "user:runner")
            .start(&plan, "run-x", json!({}), &opts())
            .unwrap();
        let changed: Arc<dyn ToolCallLlm> = Arc::new(llm);
        let err = rig
            .runner_with(Some(changed), "user:runner")
            .resume("run-x", &opts())
            .unwrap_err();
        assert_eq!(err.code(), "RUN-E025", "{label}: {err}");
    }
}

#[test]
fn resuming_a_pinned_run_with_no_llm_is_a_mismatch_not_a_missing_llm() {
    // RUN-E006 would send an operator looking for a configuration problem
    // instead of a pin.
    let rig = Rig::new();
    let plan = rig.plan(&["auto", "approve"], &[("auto", "approve")], &["approve"], &[]);
    let llm: Arc<dyn ToolCallLlm> = Arc::new(FakeLlm::new("anthropic", "m1"));
    rig.runner_with(Some(llm), "user:runner")
        .start(&plan, "run-nl", json!({}), &opts())
        .unwrap();
    let err = rig.runner().resume("run-nl", &opts()).unwrap_err();
    assert_eq!(err.code(), "RUN-E025", "{err}");
    assert!(err.to_string().contains("no LLM configured"), "{err}");
}

#[test]
fn a_pre_pin_manifest_resumes_under_any_model() {
    // A 1.8.5-shaped manifest carries no `llm`, and must keep resuming.
    let rig = Rig::new();
    let plan = rig.plan(&["auto", "approve"], &[("auto", "approve")], &["approve"], &[]);
    let runner = rig.runner();
    runner.start(&plan, "run-old", json!({}), &opts()).unwrap();
    let mut m = manifest_of(&rig, "run-old");
    assert!(m.llm.is_none());
    m.engine = None; // and no engine pin either
    rig.facade
        .with_store(|mm| m.persist_in_namespace(mm, "ops"))
        .unwrap();
    let any: Arc<dyn ToolCallLlm> = Arc::new(FakeLlm::new("openai", "whatever"));
    assert!(rig
        .runner_with(Some(any), "user:runner")
        .resume("run-old", &opts())
        .is_ok());
}

// ---------------------------------------------------------------------------
// #288 — the engine that wrote the run
// ---------------------------------------------------------------------------

#[test]
fn a_run_from_another_scheduler_epoch_refuses_to_resume() {
    let rig = Rig::new();
    let plan = rig.plan(&["auto", "approve"], &[("auto", "approve")], &["approve"], &[]);
    let runner = rig.runner();
    runner.start(&plan, "run-ep", json!({}), &opts()).unwrap();

    let mut m = manifest_of(&rig, "run-ep");
    let pin = m.engine.as_mut().unwrap();
    pin.scheduler_epoch = areev_run_core::SCHEDULER_EPOCH.saturating_sub(1);
    pin.version = "1.8.2".into();
    let stale_epoch = pin.scheduler_epoch;
    rig.facade
        .with_store(|mm| m.persist_in_namespace(mm, "ops"))
        .unwrap();

    if stale_epoch != areev_run_core::SCHEDULER_EPOCH {
        let err = rig.runner().resume("run-ep", &opts()).unwrap_err();
        assert_eq!(err.code(), "RUN-E026", "{err}");
        assert!(err.to_string().contains("1.8.2"), "{err}");
    }
}

#[test]
fn a_patch_upgrade_does_not_strand_a_parked_run() {
    // Only the EPOCH is compared: most releases change nothing here, and a
    // version-string comparison would refuse every parked approval run on
    // every patch release.
    let rig = Rig::new();
    let plan = rig.plan(&["auto", "approve"], &[("auto", "approve")], &["approve"], &[]);
    rig.runner().start(&plan, "run-patch", json!({}), &opts()).unwrap();
    let mut m = manifest_of(&rig, "run-patch");
    m.engine.as_mut().unwrap().version = "1.8.4".into();
    rig.facade
        .with_store(|mm| m.persist_in_namespace(mm, "ops"))
        .unwrap();
    assert!(rig.runner().resume("run-patch", &opts()).is_ok());
}

// ---------------------------------------------------------------------------
// #293 — the initiator
// ---------------------------------------------------------------------------

#[test]
fn an_approval_refuses_the_initiator_as_well_as_the_principal() {
    let rig = Rig::new();
    let plan = rig.plan(&["auto", "approve"], &[("auto", "approve")], &["approve"], &[]);
    let runner = rig.runner_with(None, "svc:agent");
    let opts = RunOptions {
        initiator: Some("user:ana".into()),
        ..opts()
    };
    let session = runner.start(&plan, "run-init", json!({}), &opts).unwrap();
    let RunSession::Parked { envelope, .. } = session else { panic!("expected park") };
    let ask = envelope["asks"][0]["tool_call_id"].as_str().unwrap().to_string();

    // The person the service acted for may not approve their own request.
    let err = runner
        .respond("run-init", &ask, json!({"ok": true}), false, "user:ana")
        .unwrap_err();
    assert!(matches!(err, RunError::Unauthorized { .. }), "{err}");
    assert!(err.to_string().contains("initiator"), "{err}");

    // Nor may the service principal itself.
    let err = runner
        .respond("run-init", &ask, json!({"ok": true}), false, "svc:agent")
        .unwrap_err();
    assert!(matches!(err, RunError::Unauthorized { .. }), "{err}");

    // Anyone else may.
    runner
        .respond("run-init", &ask, json!({"ok": true}), false, "user:ben")
        .unwrap();
}

#[test]
fn an_initiator_that_names_no_principal_never_matches() {
    // The field is free-form attribution: a trigger occurrence id is a
    // perfectly good value and must not accidentally refuse a responder.
    let rig = Rig::new();
    let plan = rig.plan(&["auto", "approve"], &[("auto", "approve")], &["approve"], &[]);
    let runner = rig.runner_with(None, "svc:agent");
    let opts = RunOptions {
        initiator: Some("trigger:abc#42".into()),
        ..opts()
    };
    let session = runner.start(&plan, "run-occ", json!({}), &opts).unwrap();
    let RunSession::Parked { envelope, .. } = session else { panic!("expected park") };
    let ask = envelope["asks"][0]["tool_call_id"].as_str().unwrap().to_string();
    runner
        .respond("run-occ", &ask, json!({"ok": true}), false, "user:ana")
        .unwrap();
}

#[test]
fn a_manifest_without_an_initiator_is_unchanged() {
    let rig = Rig::new();
    let plan = rig.plan(&["a"], &[], &[], &[]);
    rig.runner().start(&plan, "run-noinit", json!({}), &opts()).unwrap();
    let m = manifest_of(&rig, "run-noinit");
    assert!(m.initiator.is_none());
    let json = serde_json::to_value(&m).unwrap();
    assert!(
        json.get("initiator").is_none(),
        "an absent initiator must not appear in the serialized manifest"
    );
}

// ---------------------------------------------------------------------------
// #294 — confirmation asks
// ---------------------------------------------------------------------------

#[test]
fn a_confirmation_plan_refuses_to_start_without_the_opt_in() {
    // A Definition can arrive in a bundle or a pack, and a weakening
    // delivered with the thing it weakens is not a permission.
    let rig = Rig::new();
    let plan = rig.plan(&["auto", "confirm"], &[("auto", "confirm")], &[], &["confirm"]);
    let err = rig
        .runner()
        .start(&plan, "run-conf", json!({}), &opts())
        .unwrap_err();
    assert_eq!(err.code(), "RUN-E018", "{err}");
    assert!(err.to_string().contains("--allow-confirmation-asks"), "{err}");
    // Nothing was written.
    assert!(rig
        .facade
        .with_store(|m| areev_run::RunManifest::load(m, "run-conf"))
        .is_err());
}

#[test]
fn with_the_opt_in_the_initiator_may_answer_a_confirmation() {
    let rig = Rig::new();
    let plan = rig.plan(&["auto", "confirm"], &[("auto", "confirm")], &[], &["confirm"]);
    let runner = rig.runner_with(None, "svc:agent");
    let opts = RunOptions {
        allow_confirmation_asks: true,
        initiator: Some("user:ana".into()),
        ..opts()
    };
    let session = runner.start(&plan, "run-ok", json!({}), &opts).unwrap();
    let RunSession::Parked { envelope, .. } = session else { panic!("expected park") };
    assert_eq!(
        envelope["asks"][0]["approval"], false,
        "a confirmation is not an approval boundary"
    );
    let ask = envelope["asks"][0]["tool_call_id"].as_str().unwrap().to_string();
    runner
        .respond("run-ok", &ask, json!({"ok": true}), false, "user:ana")
        .unwrap();
    let session = runner.resume("run-ok", &opts).unwrap();
    assert!(matches!(session, RunSession::Finished { outcome: RunOutcome::Completed, .. }));
    assert!(runner.verify("run-ok").unwrap().verified);
}

#[test]
fn an_approval_beside_a_confirmation_still_refuses_the_triggering_principal() {
    let rig = Rig::new();
    let plan = rig.plan(
        &["auto", "confirm", "approve"],
        &[("auto", "confirm"), ("confirm", "approve")],
        &["approve"],
        &["confirm"],
    );
    let runner = rig.runner_with(None, "svc:agent");
    let opts = RunOptions { allow_confirmation_asks: true, ..opts() };
    let session = runner.start(&plan, "run-both", json!({}), &opts).unwrap();
    let RunSession::Parked { envelope, .. } = session else { panic!("expected park") };
    let ask = envelope["asks"][0]["tool_call_id"].as_str().unwrap().to_string();
    // The confirmation: the principal may answer it.
    runner
        .respond("run-both", &ask, json!({"ok": true}), false, "svc:agent")
        .unwrap();
    let session = runner.resume("run-both", &opts).unwrap();
    let RunSession::Parked { envelope, .. } = session else { panic!("expected a second park") };
    assert_eq!(envelope["asks"][0]["approval"], true, "the approval is still one");
    let ask2 = envelope["asks"][0]["tool_call_id"].as_str().unwrap().to_string();
    let err = runner
        .respond("run-both", &ask2, json!({"ok": true}), false, "svc:agent")
        .unwrap_err();
    assert!(matches!(err, RunError::Unauthorized { .. }), "{err}");
}

#[test]
fn an_unknown_ask_kind_is_refused_at_resolve() {
    let rig = Rig::new();
    let mut wf = Workflow::new(vec!["only".into()]);
    let mut def = Tool::new("only")
        .kind(ToolKind::Definition)
        .tool_description("x")
        .created_at(500)
        .namespace("ops")
        .executor_kind(areev_core::types::ExecutorKind::Client);
    def.common
        .extra_fields
        .insert("ask_kind".into(), json!("whatever"));
    let dh = rig.facade.with_store(|m| m.add(&def)).unwrap();
    wf = wf.bind("only", &dh.to_hex());
    let wf = wf.created_at(600).namespace("ops");
    let plan = rig.facade.with_store(|m| m.add(&wf)).unwrap();
    let err = rig
        .runner()
        .start(&plan, "run-bad", json!({}), &opts())
        .unwrap_err();
    assert_eq!(err.code(), "RUN-E019", "{err}");
    assert!(err.to_string().contains("whatever"), "{err}");
}

// ---------------------------------------------------------------------------
// #295 — run-level ceilings
// ---------------------------------------------------------------------------

#[test]
fn a_run_effect_ceiling_exhausts_the_run_rather_than_failing_a_node() {
    let rig = Rig::new();
    let plan = rig.plan(&["a", "b", "c"], &[("a", "b"), ("b", "c")], &[], &[]);
    let budgets = areev_run::BudgetsSpec { max_effects: Some(1), ..Default::default() };
    let session = rig
        .runner()
        .start(&plan, "run-eff", json!({}), &RunOptions { budgets, ..opts() })
        .unwrap();
    let RunSession::Finished { outcome, .. } = session else { panic!("expected a finish") };
    assert_eq!(
        outcome,
        RunOutcome::BudgetExhausted { axis: areev_run_core::BudgetAxis::Effects },
        "a budget stop, never a node failure"
    );
}

#[test]
fn a_run_with_no_ceilings_keeps_a_byte_identical_spent() {
    // The new counter is `skip_serializing_if` zero and only incremented
    // when a cap is set, so a pre-existing run replays unchanged.
    let rig = Rig::new();
    let plan = rig.plan(&["a", "b"], &[("a", "b")], &[], &[]);
    rig.runner().start(&plan, "run-plain", json!({}), &opts()).unwrap();
    let ckpts = rig
        .facade
        .with_store(|m| areev_run::journal::load(m, "ops", "run-plain"))
        .unwrap()
        .checkpoints;
    for c in &ckpts {
        let spent = &c.scheduler["spent"];
        assert!(
            spent.get("tool_calls").is_none(),
            "an unused counter must not appear in a checkpoint: {spent}"
        );
    }
    assert!(rig.runner().verify("run-plain").unwrap().verified);
}

#[test]
fn a_manifest_without_the_new_caps_serializes_without_them() {
    let rig = Rig::new();
    let plan = rig.plan(&["a"], &[], &[], &[]);
    rig.runner().start(&plan, "run-nocap", json!({}), &opts()).unwrap();
    let m = manifest_of(&rig, "run-nocap");
    let json = serde_json::to_value(&m).unwrap();
    assert!(json["budgets"].get("max_effects").is_none());
    assert!(json["budgets"].get("max_tool_calls").is_none());
}

// ---------------------------------------------------------------------------
// #296 — concurrency slots
// ---------------------------------------------------------------------------

#[test]
fn a_third_start_is_refused_at_a_cap_of_two_and_writes_nothing() {
    let rig = Rig::new();
    let plan = rig.plan(&["auto", "approve"], &[("auto", "approve")], &["approve"], &[]);
    let runner = rig.runner();
    let capped = RunOptions { max_concurrent_runs: Some(2), ..opts() };

    // Two runs that PARK hold nothing — a parked run releases its slot — so
    // drive the cap with the slot rows directly, which is what a live run
    // holds while it executes.
    let now = 1_755_000_000_000i64;
    let mut a = areev_run::lease::RunSlots::claim(
        &rig.facade, "run-a", "user:runner", now, 60_000, Some(2), None,
    )
    .unwrap();
    let mut b = areev_run::lease::RunSlots::claim(
        &rig.facade, "run-b", "user:runner", now, 60_000, Some(2), None,
    )
    .unwrap();

    let err = runner.start(&plan, "run-c", json!({}), &capped).unwrap_err();
    assert_eq!(err.code(), "RUN-E027", "{err}");
    assert!(err.to_string().contains("this memory"), "{err}");
    // Nothing exists under the refused run id.
    assert!(rig
        .facade
        .with_store(|m| areev_run::RunManifest::load(m, "run-c"))
        .is_err());

    // Once a slot frees, the same id starts.
    a.release(&rig.facade);
    assert!(runner.start(&plan, "run-c", json!({}), &capped).is_ok());
    b.release(&rig.facade);
}

#[test]
fn the_per_principal_cap_is_independent_of_the_memory_cap() {
    let rig = Rig::new();
    let now = 1_755_000_000_000i64;
    let mut held = areev_run::lease::RunSlots::claim(
        &rig.facade, "r1", "svc:a", now, 60_000, None, Some(1),
    )
    .unwrap();
    // Same principal: refused.
    let err = match areev_run::lease::RunSlots::claim(
        &rig.facade, "r2", "svc:a", now, 60_000, None, Some(1),
    ) {
        Err(e) => e,
        Ok(_) => panic!("the per-principal cap must refuse a second run"),
    };
    assert_eq!(err.code(), "RUN-E027", "{err}");
    // A different principal: fine.
    let mut other = areev_run::lease::RunSlots::claim(
        &rig.facade, "r3", "svc:b", now, 60_000, None, Some(1),
    )
    .unwrap();
    held.release(&rig.facade);
    other.release(&rig.facade);
}

#[test]
fn a_crashed_holders_slot_is_reclaimed_after_the_ttl() {
    let rig = Rig::new();
    let now = 1_755_000_000_000i64;
    let _dead = areev_run::lease::RunSlots::claim(
        &rig.facade, "r1", "svc:a", now, 1_000, Some(1), None,
    )
    .unwrap();
    // Still live: refused.
    assert!(areev_run::lease::RunSlots::claim(
        &rig.facade, "r2", "svc:a", now + 500, 1_000, Some(1), None,
    )
    .is_err());
    // Past the TTL: taken over.
    assert!(areev_run::lease::RunSlots::claim(
        &rig.facade, "r2", "svc:a", now + 5_000, 1_000, Some(1), None,
    )
    .is_ok());
}

#[test]
fn a_refusal_in_the_second_scope_strands_no_slot_in_the_first() {
    let rig = Rig::new();
    let now = 1_755_000_000_000i64;
    let mut blocker = areev_run::lease::RunSlots::claim(
        &rig.facade, "r0", "svc:a", now, 60_000, None, Some(1),
    )
    .unwrap();
    // Memory cap has room; the principal cap does not.
    assert!(areev_run::lease::RunSlots::claim(
        &rig.facade, "r1", "svc:a", now, 60_000, Some(4), Some(1),
    )
    .is_err());
    // The memory-scope slot it took on the way must have been given back.
    let occ = areev_run::lease::RunSlots::occupancy(&rig.facade, now).unwrap();
    assert!(
        !occ.iter().any(|(_, holder)| holder == "r1"),
        "a refused claim must strand nothing: {occ:?}"
    );
    blocker.release(&rig.facade);
}

// ---------------------------------------------------------------------------
// #299 / #300 — lease TTL and holder identity
// ---------------------------------------------------------------------------

#[test]
fn a_lease_below_the_floor_is_refused() {
    let rig = Rig::new();
    let plan = rig.plan(&["a"], &[], &[], &[]);
    let err = rig
        .runner()
        .start(&plan, "run-lease", json!({}), &RunOptions { lease_ms: Some(1_000), ..opts() })
        .unwrap_err();
    assert!(err.to_string().contains("floor"), "{err}");
}

#[test]
fn a_short_lease_produces_the_same_journal_as_the_default() {
    let rig = Rig::new();
    let plan = rig.plan(&["a", "b"], &[("a", "b")], &[], &[]);
    rig.runner().start(&plan, "run-d", json!({}), &opts()).unwrap();
    rig.runner()
        .start(&plan, "run-s", json!({}), &RunOptions { lease_ms: Some(5_000), ..opts() })
        .unwrap();
    let ck = |id: &str| {
        rig.facade
            .with_store(|m| areev_run::journal::load(m, "ops", id))
            .unwrap()
            .checkpoints
            .len()
    };
    assert_eq!(ck("run-d"), ck("run-s"), "the lease is host-local state");
    assert!(rig.runner().verify("run-s").unwrap().verified);
}

#[test]
fn two_drivers_with_the_same_principal_and_different_nodes_exclude_each_other() {
    // #300: `principal#pid` made two containers running as PID 1 under one
    // service principal the SAME holder, so the second one re-entered a live
    // lease by design and both drivers advanced the run.
    let a = areev_run::lease::holder_for("svc:agent", Some("pod-a/1"));
    let b = areev_run::lease::holder_for("svc:agent", Some("pod-b/1"));
    assert_ne!(a, b, "two pods must not produce one holder");
    assert!(a.starts_with("svc:agent#"));

    let rig = Rig::new();
    let _held =
        areev_run::lease::RunLease::acquire(&rig.facade, "r1", &a, 1_000, 60_000).unwrap();
    let taken = areev_run::lease::RunLease::acquire(&rig.facade, "r1", &b, 1_000, 60_000);
    assert!(matches!(taken, Err(RunError::LeaseLost { .. })), "a second pod must not re-enter a live lease");
    // The same driver still re-enters its own.
    assert!(areev_run::lease::RunLease::acquire(&rig.facade, "r1", &a, 2_000, 60_000).is_ok());
}

#[test]
fn no_production_holder_is_built_without_a_host_component() {
    // The default carries the host name (or the documented fallback) plus
    // the pid — never the pid alone.
    let id = areev_run::lease::default_node_id();
    assert!(id.contains('/'), "node id must be host/pid, got {id:?}");
    let holder = areev_run::lease::holder_for("p", None);
    assert!(holder.contains('#') && holder.contains('/'), "{holder}");
}

#[test]
fn run_inspect_can_report_the_holder_and_expiry() {
    let rig = Rig::new();
    let holder = areev_run::lease::holder_for("svc:a", Some("pod-a/1"));
    let _l = areev_run::lease::RunLease::acquire(&rig.facade, "r1", &holder, 1_000, 60_000)
        .unwrap();
    let (who, until) = areev_run::lease::RunLease::peek(&rig.facade, "r1")
        .unwrap()
        .expect("a held lease");
    assert_eq!(who, holder);
    assert_eq!(until, 61_000, "so a status surface can say WHEN takeover is possible");
    assert!(areev_run::lease::RunLease::peek(&rig.facade, "nope").unwrap().is_none());
}

// ---------------------------------------------------------------------------
// #291 — pricing
// ---------------------------------------------------------------------------

#[test]
fn a_priced_transport_stamps_usd_on_every_llm_effect() {
    let rig = Rig::new();
    // One abstract node: no binding and an LLM configured.
    let wf = Workflow::new(vec!["think".into()]).created_at(600).namespace("ops");
    let plan = rig.facade.with_store(|m| m.add(&wf)).unwrap();
    let llm: Arc<dyn ToolCallLlm> = Arc::new(FakeLlm::new("anthropic", "m1").priced(1_500));
    let runner = rig.runner_with(Some(llm), "user:runner");
    runner.start(&plan, "run-priced", json!({}), &opts()).unwrap();

    let entries = rig
        .facade
        .with_store(|m| areev_run::journal::load(m, "ops", "run-priced"))
        .unwrap();
    let priced = entries.entries.values().any(|e| {
        e.result
            .as_ref()
            .and_then(|(h, _)| rig.facade.with_store(|m| m.get(h)).ok())
            .and_then(|g| g.fields.get("usage_usd_micros").and_then(|v| v.as_i64()))
            .is_some_and(|v| v == 1_500)
    });
    assert!(priced, "the journaled effect carries the price");
    assert!(runner.verify("run-priced").unwrap().verified);
}

#[test]
fn an_unpriced_transport_leaves_the_journal_as_it_was() {
    let rig = Rig::new();
    let wf = Workflow::new(vec!["think".into()]).created_at(600).namespace("ops");
    let plan = rig.facade.with_store(|m| m.add(&wf)).unwrap();
    let llm: Arc<dyn ToolCallLlm> = Arc::new(FakeLlm::new("anthropic", "m1"));
    let runner = rig.runner_with(Some(llm), "user:runner");
    runner.start(&plan, "run-unpriced", json!({}), &opts()).unwrap();
    let view = rig
        .facade
        .with_store(|m| areev_run::journal::load(m, "ops", "run-unpriced"))
        .unwrap();
    for e in view.entries.values() {
        if let Some((h, _)) = &e.result {
            let g = rig.facade.with_store(|m| m.get(h)).unwrap();
            // Unpriced is not free: no marker, and the figure stays 0.
            assert!(
                g.fields
                    .get("result")
                    .and_then(|r| r.get("usd_priced"))
                    .is_none(),
                "an unpriced effect must carry no priced marker"
            );
        }
    }
}

#[test]
fn a_usd_budget_exhausts_once_effects_are_priced() {
    let rig = Rig::new();
    // TWO abstract nodes: the first superstep prices above the ceiling, and
    // the pre-flight for the SECOND refuses to open it. Before #291 a
    // positive `--max-usd` could never exhaust at all, because every effect
    // was stamped `usd_micros: 0`.
    let wf = Workflow::new(vec!["think".into(), "again".into()])
        .edge("think", "again")
        .created_at(600)
        .namespace("ops");
    let plan = rig.facade.with_store(|m| m.add(&wf)).unwrap();
    let llm: Arc<dyn ToolCallLlm> = Arc::new(FakeLlm::new("anthropic", "m1").priced(1_500));
    let runner = rig.runner_with(Some(llm), "user:runner");
    let budgets = areev_run::BudgetsSpec { max_usd_micros: Some(1_000), ..Default::default() };
    let session = runner
        .start(&plan, "run-usd", json!({}), &RunOptions { budgets, ..opts() })
        .unwrap();
    match session {
        RunSession::Finished { outcome, .. } => assert_eq!(
            outcome,
            RunOutcome::BudgetExhausted { axis: areev_run_core::BudgetAxis::Usd }
        ),
        other => panic!("expected a budget stop, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// #301 — run evidence follows the run's namespace
// ---------------------------------------------------------------------------

#[test]
fn an_input_placed_in_the_run_namespace_leaves_no_content_in_the_harness() {
    const MARKER: &str = "zz-confidential-zz";
    let rig = Rig::new();
    let plan = rig.plan(&["a", "b"], &[("a", "b")], &[], &[]);
    let runner = rig.runner();
    let opts = RunOptions { input_in_run_namespace: true, ..opts() };
    runner
        .start(&plan, "run-inref", json!({ "matter": MARKER }), &opts)
        .unwrap();

    // Nothing in the memory-wide harness carries the marker.
    let harness_hits = rig.facade.with_store(|m| {
        m.run_trace(areev_core::authz::HARNESS_NS, "run-inref", 200)
            .unwrap()
            .into_iter()
            .filter(|g| serde_json::to_string(&g.fields).unwrap().contains(MARKER))
            .count()
    });
    assert_eq!(harness_hits, 0, "the input is not in agent:harness");

    // The manifest names the input by address, and still resolves it.
    let m = manifest_of(&rig, "run-inref");
    assert!(m.input_ref.is_some(), "the manifest stores a reference");
    assert_eq!(m.input["matter"], json!(MARKER), "and load() hydrates it");

    // Verify and fork both work through the reference.
    assert!(runner.verify("run-inref").unwrap().verified);
}

#[test]
fn a_run_whose_input_grain_was_erased_refuses_rather_than_replaying_against_null() {
    let rig = Rig::new();
    let plan = rig.plan(&["a"], &[], &[], &[]);
    let runner = rig.runner();
    let opts = RunOptions { input_in_run_namespace: true, ..opts() };
    runner.start(&plan, "run-gone", json!({"q": 1}), &opts).unwrap();
    let hex = manifest_of(&rig, "run-gone").input_ref.unwrap();
    let h = Hash::from_hex(&hex).unwrap();
    rig.facade.with_store(|m| m.forget(&h)).unwrap();

    let err = rig
        .facade
        .with_store(|m| areev_run::RunManifest::load(m, "run-gone"))
        .unwrap_err();
    assert!(
        err.to_string().contains("no longer readable"),
        "a missing input must refuse, never replay against null: {err}"
    );
}

#[test]
fn by_default_the_input_still_rides_the_manifest() {
    // Existing invocations change in no way.
    let rig = Rig::new();
    let plan = rig.plan(&["a"], &[], &[], &[]);
    rig.runner().start(&plan, "run-byval", json!({"q": 1}), &opts()).unwrap();
    let m = manifest_of(&rig, "run-byval");
    assert!(m.input_ref.is_none());
    assert_eq!(m.input["q"], json!(1));
    let json = serde_json::to_value(&m).unwrap();
    assert!(json.get("input_ref").is_none(), "absent, so bytes are unchanged");
}

#[test]
fn the_harness_namespace_helper_builds_a_dotted_child() {
    use areev_run::journal::harness_ns_for;
    assert_eq!(harness_ns_for(None), "agent:harness");
    assert_eq!(harness_ns_for(Some("")), "agent:harness");
    assert_eq!(harness_ns_for(Some("deal.alpha")), "agent:harness.deal.alpha");
}
