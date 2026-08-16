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
}

impl TestSubstrate {
    pub fn new() -> Self {
        TestSubstrate {
            inner: ReferenceSubstrate::new(),
            namespaces: vec![],
            outcomes: vec![],
            clock: 0,
            tel: TelemetryView::default(),
        }
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
        );
        analyzer.analyze(&ctx).expect("analyze ok")
    }
}
