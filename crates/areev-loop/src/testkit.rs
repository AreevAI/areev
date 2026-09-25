//! Test-only conveniences over `ReferenceSubstrate`: grain builders and a
//! params-resolving `analyze` helper so each analyzer runs with its manifest
//! defaults (not empty params). Compiled only under `cfg(test)`.

use crate::analyzer::{AnalyzeCtx, Analyzer, OutcomeInput};
use crate::model::GrainRecord;
use crate::recommendation::RecDraft;
use crate::reference::ReferenceSubstrate;
use crate::substrate::{BudgetUsage, GrainAccess, QueryUsage, TelemetryView};
use serde_json::{json, Map, Value};

pub struct TestSubstrate {
    pub inner: ReferenceSubstrate,
    namespaces: Vec<String>,
    outcomes: Vec<OutcomeInput>,
    clock: i64,
    tel: TelemetryView,
    verdicts: std::collections::BTreeMap<String, String>,
}

impl TestSubstrate {
    pub fn new() -> Self {
        TestSubstrate {
            inner: ReferenceSubstrate::new(),
            namespaces: vec![],
            outcomes: vec![],
            clock: 0,
            tel: TelemetryView::default(),
            verdicts: Default::default(),
        }
    }

    /// Pretend the Verify gate recorded `verdict` for the grain `hash` (what
    /// the engine hands analyzers as `AnalyzeCtx::verdict_for`).
    pub fn set_verdict(&mut self, hash: &str, verdict: &str) {
        self.verdicts.insert(hash.to_string(), verdict.to_string());
    }

    fn tick(&mut self) -> i64 {
        self.clock += 1;
        self.clock * 1000
    }

    pub fn add_fact(&mut self, subject: &str, relation: &str, object: &str) -> String {
        let created = self.tick();
        self.push_fact("test", subject, relation, object, created, None)
    }

    pub fn add_fact_valid_to(
        &mut self,
        subject: &str,
        relation: &str,
        object: &str,
        valid_to_ms: i64,
    ) -> String {
        let created = self.tick();
        self.push_fact(
            "test",
            subject,
            relation,
            object,
            created,
            Some(valid_to_ms),
        )
    }

    /// Add a fact at an explicit `created_at` and namespace — for
    /// age-window analyzers (retention) where the clock IS the input.
    pub fn add_fact_at(
        &mut self,
        ns: &str,
        subject: &str,
        relation: &str,
        object: &str,
        created_ms: i64,
    ) -> String {
        self.push_fact(ns, subject, relation, object, created_ms, None)
    }

    fn push_fact(
        &mut self,
        ns: &str,
        subject: &str,
        relation: &str,
        object: &str,
        created: i64,
        valid_to: Option<i64>,
    ) -> String {
        let mut fields = Map::new();
        fields.insert("subject".into(), json!(subject));
        fields.insert("relation".into(), json!(relation));
        fields.insert("object".into(), json!(object));
        fields.insert("namespace".into(), json!(ns));
        self.inner.insert(GrainRecord {
            hash: String::new(),
            grain_type: "fact".into(),
            namespace: ns.into(),
            created_at_ms: created,
            valid_to_ms: valid_to,
            superseded_by: None,
            fields,
        })
    }

    /// Add a tool call at an explicit `created_at` (for outcome-window tests).
    pub fn add_tool_call_at(&mut self, tool: &str, is_error: bool, content: &str, created: i64) -> String {
        let mut fields = Map::new();
        fields.insert("tool_name".into(), json!(tool));
        fields.insert("is_error".into(), json!(is_error));
        fields.insert("content".into(), json!(content));
        fields.insert("namespace".into(), json!("test"));
        self.inner.insert(GrainRecord {
            hash: String::new(),
            grain_type: "tool".into(),
            namespace: "test".into(),
            created_at_ms: created,
            valid_to_ms: None,
            superseded_by: None,
            fields,
        })
    }

    pub fn add_tool_call(&mut self, tool: &str, is_error: bool, content: &str) -> String {
        let created = self.tick();
        let mut fields = Map::new();
        fields.insert("tool_name".into(), json!(tool));
        fields.insert("is_error".into(), json!(is_error));
        fields.insert("content".into(), json!(content));
        fields.insert("namespace".into(), json!("test"));
        self.inner.insert(GrainRecord {
            hash: String::new(),
            grain_type: "tool".into(),
            namespace: "test".into(),
            created_at_ms: created,
            valid_to_ms: None,
            superseded_by: None,
            fields,
        })
    }

    /// A Tool DEFINITION grain declaring which evalset grades its revisions —
    /// Rule E1's pin, resolved from the substrate and never from the model.
    pub fn add_tool_def(&mut self, tool: &str, evalset_hash: Option<&str>) -> String {
        let created = self.tick();
        let mut fields = Map::new();
        fields.insert("tool_name".into(), json!(tool));
        fields.insert("kind".into(), json!("definition"));
        fields.insert("namespace".into(), json!("test"));
        if let Some(e) = evalset_hash {
            fields.insert("evalset_hash".into(), json!(e));
        }
        self.inner.insert(GrainRecord {
            hash: String::new(),
            grain_type: "tool".into(),
            namespace: "test".into(),
            created_at_ms: created,
            valid_to_ms: None,
            superseded_by: None,
            fields,
        })
    }

    /// A Workflow plan grain: three nodes with one bounded review cycle — the
    /// shape a `plan_revision` edits (a `cond`, a `max_cycles`, a retry).
    pub fn add_workflow(&mut self) -> String {
        let created = self.tick();
        let Value::Object(fields) = json!({
            "nodes": ["fetch", "review", "post"],
            "edges": [
                {"src": "fetch", "dst": "review"},
                {"src": "review", "dst": "fetch", "cond": "confidence < 0.9", "max_cycles": 2},
                {"src": "review", "dst": "post"}
            ],
            "bindings": {"fetch": "sha256:tool1"},
            "retries": {"fetch": 1},
            "namespace": "test"
        }) else {
            unreachable!()
        };
        self.inner.insert(GrainRecord {
            hash: String::new(),
            grain_type: "workflow".into(),
            namespace: "test".into(),
            created_at_ms: created,
            valid_to_ms: None,
            superseded_by: None,
            fields,
        })
    }

    pub fn add_observation(&mut self, ns: &str, body: &str) -> String {
        let created = self.tick();
        let mut fields = Map::new();
        fields.insert("body".into(), json!(body));
        fields.insert("namespace".into(), json!(ns));
        self.inner.insert(GrainRecord {
            hash: String::new(),
            grain_type: "observation".into(),
            namespace: ns.into(),
            created_at_ms: created,
            valid_to_ms: None,
            superseded_by: None,
            fields,
        })
    }

    /// A human's note, shaped the way the real store shapes one: the text in
    /// `object` under a `subject`, with no `relation`.
    ///
    /// Distinct from `add_observation`, which puts the text in `body`. That
    /// helper matched the *renderer* rather than the store, so the one grain
    /// shape a real memory actually produces went untested — and rendered to
    /// an empty string all the way to the model.
    pub fn add_human_note(&mut self, ns: &str, subject: &str, observer: &str, text: &str) -> String {
        let created = self.tick();
        let mut fields = Map::new();
        fields.insert("subject".into(), json!(subject));
        fields.insert("object".into(), json!(text));
        fields.insert("observer_id".into(), json!(observer));
        fields.insert("observer_type".into(), json!("human"));
        fields.insert("namespace".into(), json!(ns));
        self.inner.insert(GrainRecord {
            hash: String::new(),
            grain_type: "observation".into(),
            namespace: ns.into(),
            created_at_ms: created,
            valid_to_ms: None,
            superseded_by: None,
            fields,
        })
    }

    pub fn add_fork(&mut self, entity: &str, heads: &[&str]) {
        self.inner.register_fork(entity, heads);
    }

    pub fn add_skill(&mut self, name: &str, proficiency: f64, practice_count: i64) -> String {
        let created = self.tick();
        let mut fields = Map::new();
        fields.insert("name".into(), json!(name));
        fields.insert("proficiency".into(), json!(proficiency));
        fields.insert("practice_count".into(), json!(practice_count));
        fields.insert("namespace".into(), json!("test"));
        self.inner.insert(GrainRecord {
            hash: String::new(),
            grain_type: "skill".into(),
            namespace: "test".into(),
            created_at_ms: created,
            valid_to_ms: None,
            superseded_by: None,
            fields,
        })
    }

    pub fn add_goal(&mut self, subject: &str, state: &str, progress: f64, created_at: i64) -> String {
        let mut fields = Map::new();
        fields.insert("subject".into(), json!(subject));
        fields.insert("goal_state".into(), json!(state));
        fields.insert("progress".into(), json!(progress));
        fields.insert("namespace".into(), json!("test"));
        self.inner.insert(GrainRecord {
            hash: String::new(),
            grain_type: "goal".into(),
            namespace: "test".into(),
            created_at_ms: created_at,
            valid_to_ms: None,
            superseded_by: None,
            fields,
        })
    }

    pub fn set_outcome_inputs(&mut self, outcomes: Vec<OutcomeInput>) {
        self.outcomes = outcomes;
    }

    /// Insert an Observation grain into an explicit namespace with the given
    /// fields (feeds `run_outcome`, whose datasource is `agent:harness`).
    pub fn put_observation(&mut self, namespace: &str, fields: &[(&str, Value)]) -> String {
        let created = self.tick();
        let mut map = Map::new();
        for (k, v) in fields {
            map.insert((*k).into(), v.clone());
        }
        map.insert("namespace".into(), json!(namespace));
        self.inner.insert(GrainRecord {
            hash: String::new(),
            grain_type: "observation".into(),
            namespace: namespace.into(),
            created_at_ms: created,
            valid_to_ms: None,
            superseded_by: None,
            fields: map,
        })
    }

    /// Mark a grain as recalled `recall_count` times (feeds `cold_grains`: a
    /// grain absent from telemetry has never been recalled). Turns on the
    /// `telemetry` capability.
    pub fn telemetry_recall(&mut self, hash: &str, recall_count: i64) {
        self.tel.access.push(GrainAccess {
            hash: hash.to_string(),
            recall_count,
            last_ms: self.clock * 1000,
        });
        self.inner.set_telemetry(self.tel.clone());
    }

    /// Record a recurring recall question (feeds `coverage_gap`).
    pub fn telemetry_query(&mut self, sample: &str, run_count: i64, empty_count: i64) {
        self.tel.queries.push(QueryUsage {
            sample: sample.to_string(),
            run_count,
            empty_count,
            sum_results: 0,
            last_ms: self.clock * 1000,
        });
        self.inner.set_telemetry(self.tel.clone());
    }

    /// Set the assembly-budget rollup (feeds `budget_pressure`).
    pub fn telemetry_budget(&mut self, sample_count: i64, overflow_count: i64) {
        self.tel.budget = BudgetUsage {
            sample_count,
            overflow_count,
        };
        self.inner.set_telemetry(self.tel.clone());
    }

    /// Run an analyzer with its manifest-default params.
    pub fn analyze(&self, analyzer: &dyn Analyzer, now_ms: i64) -> Vec<RecDraft> {
        self.analyze_with(analyzer, now_ms, &[])
    }

    /// Run an analyzer with its manifest-default params and a decision
    /// backend in the context (what a production pass hands it).
    pub fn analyze_decided(
        &self,
        analyzer: &dyn Analyzer,
        now_ms: i64,
        decider: &crate::decide::Decider,
    ) -> Vec<RecDraft> {
        let params = analyzer
            .manifest()
            .resolve_params(&Map::new())
            .expect("valid params");
        let ctx = AnalyzeCtx::new(
            &self.inner,
            &params,
            &self.namespaces,
            None,
            now_ms,
            &self.outcomes,
            &self.verdicts,
        )
        .with_decider(Some(decider));
        analyzer.analyze(&ctx).expect("analyze ok")
    }

    /// An error tool call carrying extra fields (`failure_cause`,
    /// `failure_detail`, …).
    pub fn add_tool_error_with(&mut self, tool: &str, content: &str, extra: &[(&str, &str)]) -> String {
        let created = self.tick();
        let mut fields = Map::new();
        fields.insert("tool_name".into(), json!(tool));
        fields.insert("is_error".into(), json!(true));
        fields.insert("content".into(), json!(content));
        fields.insert("namespace".into(), json!("test"));
        for (k, v) in extra {
            fields.insert((*k).to_string(), json!(v));
        }
        self.inner.insert(GrainRecord {
            hash: String::new(),
            grain_type: "tool".into(),
            namespace: "test".into(),
            created_at_ms: created,
            valid_to_ms: None,
            superseded_by: None,
            fields,
        })
    }

    /// Run an analyzer with parameter overrides.
    pub fn analyze_with(
        &self,
        analyzer: &dyn Analyzer,
        now_ms: i64,
        overrides: &[(&str, Value)],
    ) -> Vec<RecDraft> {
        let mut ov = Map::new();
        for (k, v) in overrides {
            ov.insert((*k).to_string(), v.clone());
        }
        let params = analyzer
            .manifest()
            .resolve_params(&ov)
            .expect("valid params");
        let ctx = AnalyzeCtx::new(
            &self.inner,
            &params,
            &self.namespaces,
            None,
            now_ms,
            &self.outcomes,
            &self.verdicts,
        );
        analyzer.analyze(&ctx).expect("analyze ok")
    }
}

/// A scripted, deterministic decision backend for tests. `answer` maps a
/// question id plus the whole request to that question's wire answer
/// (`{"type": "noul", "noul": p}` or a choice); `fail` makes every call
/// return `LOP-E051`. Every request is logged so a test can count calls and
/// inspect what was asked.
pub struct FakeDecider {
    pub calibrated: bool,
    pub fail: bool,
    #[allow(clippy::type_complexity)]
    pub answer: Box<dyn Fn(&str, &Value) -> Value + Send + Sync>,
    pub log: std::sync::Arc<std::sync::Mutex<Vec<Value>>>,
}

impl FakeDecider {
    /// Every question answered as a `noul` with `p`.
    pub fn constant(p: f64) -> Self {
        Self::by(move |_, _| json!({"type": "noul", "noul": p}))
    }

    pub fn by(f: impl Fn(&str, &Value) -> Value + Send + Sync + 'static) -> Self {
        FakeDecider {
            calibrated: true,
            fail: false,
            answer: Box::new(f),
            log: Default::default(),
        }
    }

    pub fn uncalibrated(mut self) -> Self {
        self.calibrated = false;
        self
    }

    pub fn failing(mut self) -> Self {
        self.fail = true;
        self
    }

    pub fn calls(&self) -> std::sync::Arc<std::sync::Mutex<Vec<Value>>> {
        self.log.clone()
    }
}

impl crate::decide::DecideBackend for FakeDecider {
    fn decide(&self, request_json: &str) -> crate::error::Result<String> {
        let req: Value = serde_json::from_str(request_json).expect("the engine sends JSON");
        self.log.lock().unwrap().push(req.clone());
        if self.fail {
            return Err(crate::error::Error::DecideBackend("scripted failure".into()));
        }
        let mut answers = Map::new();
        for id in req["questions"].as_object().expect("questions").keys() {
            answers.insert(id.clone(), (self.answer)(id, &req));
        }
        Ok(json!({
            "answers": answers,
            "provider": "fake",
            "model": "fake-1",
            "calibrated": self.calibrated,
            "latency_ms": 7,
        })
        .to_string())
    }
    fn calibrated(&self) -> bool {
        self.calibrated
    }
    fn describe(&self) -> String {
        "fake:fake-1".into()
    }
}
