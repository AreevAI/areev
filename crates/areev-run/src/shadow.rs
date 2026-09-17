//! Plan-change shadow — re-drive journaled runs through the pure scheduler
//! under a CANDIDATE plan, answering every effect from the journal and
//! dispatching nothing.
//!
//! `verify` re-drives a run under its own manifest and byte-compares the
//! checkpoints; `shadow_eval` batches that. Neither takes a candidate, so
//! until this a plan edit was validated structurally (V1–V7) and never
//! behaviourally: the first evidence about a revision was production damage.
//! Dream-RSI (arXiv 2609.14858 §3) scores candidate policies over recorded
//! trees at zero agent cost, with one scope rule this module keeps verbatim:
//! replay can only answer effects the journal recorded. A candidate that asks
//! for an effect outside the recorded support — a renamed or rebound node, a
//! branch the live run never took — is **out of support** and earns no
//! score; it is a report field, never an error.
//!
//! Zero writes and zero dispatches are structural: the path holds no
//! executor and no model, reads the journal through the facade, and the
//! only state it builds is the scheduler's, in memory.

use crate::journal;
use crate::manifest::RunManifest;
use crate::runner::{builtin_eval, make_validate_args, Runner};
use crate::RunError;
use areev_core::error::Hash;
use areev_core::types::{Workflow, WorkflowEdge};
use areev_run_core::{
    step, Command, EffectOutcome, EventIn, JournalKey, NodeExecutor, PlanGraph, RunOutcome,
    SchedulerState, StepEnv,
};
use serde_json::{Map, Value};
use std::collections::BTreeSet;

/// The plan to rehearse: a stored Workflow grain, or an unstored draft body
/// (validated by `PlanGraph::build` before any run is touched).
#[derive(Debug, Clone)]
pub enum PlanCandidate {
    Hash(Hash),
    Body(Map<String, Value>),
}

/// What one run spent — the sum over the journaled results a replay
/// consumed, or over every journaled result for the incumbent.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize)]
pub struct ShadowSpend {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub usd_micros: u64,
}

impl ShadowSpend {
    fn add(&mut self, outcome: &EffectOutcome) {
        if let EffectOutcome::Completed { input_tokens, output_tokens, usd_micros, .. } = outcome {
            self.input_tokens += input_tokens;
            self.output_tokens += output_tokens;
            self.usd_micros += usd_micros;
        }
    }
}

/// One run rehearsed under the candidate.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ShadowPlanRun {
    pub run_id: String,
    /// The terminal label the live run reached (`open` when it has not).
    pub incumbent_outcome: String,
    /// The terminal label the replay reached under the candidate, or
    /// `out_of_support` when it asked for an effect the journal never
    /// recorded.
    pub candidate_outcome: String,
    pub supersteps: u64,
    /// Effects answered from the journal.
    pub effects_replayed: u64,
    /// Effects the candidate asked for that the journal never recorded —
    /// `node@attempt#seq/kind`, in the order they were asked.
    pub out_of_support: Vec<String>,
    pub incumbent_spent: ShadowSpend,
    pub candidate_spent: ShadowSpend,
    /// When the candidate IS the incumbent plan, the replay is also a
    /// verify: every checkpoint it wrote is byte-compared with the stored
    /// one. `None` under a different plan.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub identity: Option<ShadowIdentity>,
    /// `same`, `better`, `worse` (by terminal outcome: completed beats
    /// everything else), or `out_of_support` (no score).
    pub verdict: String,
    /// What stopped the replay when it did not reach a terminal state.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct ShadowIdentity {
    pub checkpoints_compared: usize,
    pub consistent: bool,
}

#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct ShadowPlanTotals {
    pub runs: u64,
    pub same: u64,
    pub better: u64,
    pub worse: u64,
    pub out_of_support: u64,
    pub incumbent_completed: u64,
    pub candidate_completed: u64,
    pub incumbent_spent: ShadowSpend,
    pub candidate_spent: ShadowSpend,
}

/// The rehearsal of N journaled runs under one candidate plan.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ShadowPlanReport {
    pub candidate_plan: String,
    /// Whether the candidate was an unstored draft.
    pub candidate_is_draft: bool,
    pub runs: Vec<ShadowPlanRun>,
    pub totals: ShadowPlanTotals,
    /// No scored run is worse than the incumbent, and at least one was scored.
    pub no_worse: bool,
    /// Out-of-support runs over all runs, 0..1.
    pub out_of_support_fraction: f64,
    /// Always 0 — stated in the artifact so the claim is explicit.
    pub effect_dispatches: u64,
    /// Always 0 — the replay path reaches no writer.
    pub writes: u64,
}

fn outcome_label(o: Option<&RunOutcome>) -> String {
    match o {
        None => "open".into(),
        Some(RunOutcome::Completed) => "completed".into(),
        Some(RunOutcome::Stalled { .. }) => "stalled".into(),
        Some(RunOutcome::Failed { .. }) => "failed".into(),
        Some(RunOutcome::Canceled { .. }) => "canceled".into(),
        Some(RunOutcome::BudgetExhausted { .. }) => "budget_exhausted".into(),
    }
}

fn key_label(k: &JournalKey) -> String {
    let path = if k.task_path.is_empty() { String::new() } else { format!("{}:", k.task_path) };
    format!("{path}{}@{}#{}/{}", k.node, k.attempt, k.effect_seq, k.kind.as_str())
}

/// Read a Workflow out of a draft body, STRICTLY — a malformed edge fails
/// rather than being skipped, because the plan under rehearsal must be the
/// plan the reviewer is looking at.
pub fn workflow_from_body(fields: &Map<String, Value>) -> Result<Workflow, RunError> {
    let bad = |why: &str| RunError::InvalidPlan { why: why.into() };
    let nodes: Vec<String> = match fields.get("nodes") {
        Some(Value::Array(a)) => a
            .iter()
            .map(|v| v.as_str().map(str::to_string).ok_or_else(|| bad("every workflow node must be a string")))
            .collect::<Result<_, _>>()?,
        _ => return Err(bad("a workflow needs a 'nodes' array")),
    };
    let mut wf = Workflow::new(nodes);
    if let Some(v) = fields.get("edges") {
        let arr = v.as_array().ok_or_else(|| bad("workflow 'edges' must be an array"))?;
        for e in arr {
            let (Some(src), Some(dst)) = (e.get("src").and_then(Value::as_str), e.get("dst").and_then(Value::as_str)) else {
                return Err(bad("every workflow edge needs a 'src' and a 'dst'"));
            };
            let cond = match e.get("cond") {
                None | Some(Value::Null) => None,
                Some(Value::String(c)) => Some(c.clone()),
                Some(_) => return Err(bad("an edge 'cond' must be a string")),
            };
            let max_cycles = match e.get("max_cycles") {
                None | Some(Value::Null) => None,
                Some(v) => Some(
                    v.as_u64()
                        .and_then(|n| u32::try_from(n).ok())
                        .ok_or_else(|| bad("an edge 'max_cycles' must be a non-negative integer"))?,
                ),
            };
            wf.edges.push(WorkflowEdge { src: src.into(), dst: dst.into(), cond, max_cycles });
        }
    }
    if let Some(v) = fields.get("bindings") {
        let obj = v.as_object().ok_or_else(|| bad("workflow 'bindings' must be an object"))?;
        for (node, h) in obj {
            let h = h.as_str().ok_or_else(|| bad("a binding must be a content address string"))?;
            wf = wf.bind(node, h);
        }
    }
    if let Some(v) = fields.get("retries") {
        let obj = v.as_object().ok_or_else(|| bad("workflow 'retries' must be an object"))?;
        for (node, n) in obj {
            let n = n
                .as_u64()
                .and_then(|n| u32::try_from(n).ok())
                .ok_or_else(|| bad("a retry count must be a non-negative integer"))?;
            wf = wf.retry(node, n);
        }
    }
    for (k, v) in fields {
        if !matches!(k.as_str(), "nodes" | "edges" | "bindings" | "retries") {
            wf.common.extra_fields.insert(k.clone(), v.clone());
        }
    }
    Ok(wf)
}

impl Runner {
    /// Rehearse `run_ids` under `candidate`: for each run, resolve a manifest
    /// for the candidate the way `fork --plan` does (V3/V7 re-validation),
    /// seeded from the run's recorded input; re-drive through `step`,
    /// answering each effect from the journal by its exact key; report the
    /// terminal label, effects replayed and out of support, and the spend
    /// the candidate consumed — beside the incumbent's. Zero dispatches,
    /// zero writes.
    pub fn shadow_plan(
        &self,
        run_ids: &[String],
        candidate: &PlanCandidate,
    ) -> Result<ShadowPlanReport, RunError> {
        shadow_plan_over(&self.facade, &self.ns, &self.principal, run_ids, candidate)
    }
}

/// The newest `limit` runs of `plan_hash` — the runs a plan revision is
/// rehearsed against — as `(run_id, namespace)`. Read from the harness link
/// Facts and each run's frozen manifest; runs of other plans are not the
/// incumbent's evidence, and `ns = Some(..)` keeps to one session namespace.
pub fn runs_of_plan(
    facade: &areev_cal::AreevFacade,
    ns: Option<&str>,
    plan_hash: &str,
    limit: usize,
) -> Result<Vec<(String, String)>, RunError> {
    let scan = limit.saturating_mul(16).max(256);
    let links = facade
        .with_store(|m| m.recent(areev_core::authz::HARNESS_NS, Some(areev_core::types::GrainType::Fact), scan))
        .map_err(|e| RunError::Storage { detail: e.to_string() })?;
    let mut out: Vec<(String, String)> = Vec::new();
    for g in links {
        if g.get_str("relation") != Some("mg:harness") {
            continue;
        }
        let Some(id) = g.get_str("run_id") else { continue };
        let Some(run_ns) = g.get_str("run_ns") else { continue };
        if ns.is_some_and(|want| want != run_ns) {
            continue;
        }
        if out.iter().any(|(i, _)| i == id) {
            continue;
        }
        let Ok(manifest) = facade.with_store(|m| RunManifest::load(m, id)) else { continue };
        if manifest.plan_hash == plan_hash {
            out.push((id.to_string(), run_ns.to_string()));
        }
        if out.len() >= limit {
            break;
        }
    }
    Ok(out)
}

/// [`Runner::shadow_plan`] over a borrowed facade, every run in one
/// namespace — what a host that holds no `Runner` calls.
pub fn shadow_plan_over(
    facade: &areev_cal::AreevFacade,
    ns: &str,
    principal: &str,
    run_ids: &[String],
    candidate: &PlanCandidate,
) -> Result<ShadowPlanReport, RunError> {
    let runs: Vec<(String, String)> = run_ids.iter().map(|id| (id.clone(), ns.to_string())).collect();
    shadow_plan_scoped(facade, principal, &runs, candidate)
}

/// The same over `(run_id, namespace)` pairs — the shape `runs_of_plan`
/// returns, so the loop's adapter rehearses each run in the namespace it
/// was journaled in.
pub fn shadow_plan_scoped(
    facade: &areev_cal::AreevFacade,
    principal: &str,
    runs: &[(String, String)],
    candidate: &PlanCandidate,
) -> Result<ShadowPlanReport, RunError> {
    let ctx = ShadowCtx { facade, principal };
    ctx.shadow_plan(runs, candidate)
}

struct ShadowCtx<'a> {
    facade: &'a areev_cal::AreevFacade,
    principal: &'a str,
}

impl ShadowCtx<'_> {
    fn shadow_plan(
        &self,
        runs: &[(String, String)],
        candidate: &PlanCandidate,
    ) -> Result<ShadowPlanReport, RunError> {
        let (plan, cand_hash, is_draft, draft_fields) = match candidate {
            PlanCandidate::Hash(h) => (self.load_plan_graph(h)?, *h, false, None),
            PlanCandidate::Body(fields) => {
                let wf = workflow_from_body(fields)?;
                let plan = PlanGraph::build(&wf)?;
                // The address the draft WOULD have — what the report names.
                // It is not in the store, so resolution reads the draft's
                // own fields (`reducers`, `reads`) instead of looking it up.
                let (_, h) = areev_core::format::serialize::serialize_grain(&wf)
                    .map_err(|e| RunError::InvalidPlan { why: e.to_string() })?;
                (plan, h, true, Some(fields))
            }
        };
        let mut report = ShadowPlanReport {
            candidate_plan: cand_hash.to_hex(),
            candidate_is_draft: is_draft,
            runs: Vec::new(),
            totals: ShadowPlanTotals::default(),
            no_worse: false,
            out_of_support_fraction: 0.0,
            effect_dispatches: 0,
            writes: 0,
        };
        for (run_id, ns) in runs {
            let row = self.shadow_one(run_id, ns, &plan, &cand_hash, draft_fields)?;
            let t = &mut report.totals;
            t.runs += 1;
            match row.verdict.as_str() {
                "same" => t.same += 1,
                "better" => t.better += 1,
                "worse" => t.worse += 1,
                _ => t.out_of_support += 1,
            }
            if row.incumbent_outcome == "completed" {
                t.incumbent_completed += 1;
            }
            if row.candidate_outcome == "completed" {
                t.candidate_completed += 1;
            }
            t.incumbent_spent.input_tokens += row.incumbent_spent.input_tokens;
            t.incumbent_spent.output_tokens += row.incumbent_spent.output_tokens;
            t.incumbent_spent.usd_micros += row.incumbent_spent.usd_micros;
            t.candidate_spent.input_tokens += row.candidate_spent.input_tokens;
            t.candidate_spent.output_tokens += row.candidate_spent.output_tokens;
            t.candidate_spent.usd_micros += row.candidate_spent.usd_micros;
            report.runs.push(row);
        }
        let scored = report.totals.runs - report.totals.out_of_support;
        report.no_worse = scored > 0 && report.totals.worse == 0;
        report.out_of_support_fraction = if report.totals.runs == 0 {
            0.0
        } else {
            report.totals.out_of_support as f64 / report.totals.runs as f64
        };
        Ok(report)
    }

    fn load_plan_graph(&self, h: &Hash) -> Result<PlanGraph, RunError> {
        let wf = self
            .facade
            .with_store(|m| m.get(h))
            .map_err(|e| RunError::UnresolvedRef { what: format!("workflow: {e}") })?
            .to_workflow()
            .map_err(|e| RunError::InvalidPlan { why: e.to_string() })?;
        PlanGraph::build(&wf)
    }

    fn shadow_one(
        &self,
        run_id: &str,
        ns: &str,
        plan: &PlanGraph,
        cand_hash: &Hash,
        draft_fields: Option<&Map<String, Value>>,
    ) -> Result<ShadowPlanRun, RunError> {
        let incumbent = self.facade.with_store(|m| RunManifest::load(m, run_id))?;
        let view = self
            .facade
            .with_store(|m| journal::load(m, ns, run_id))
            .map_err(|e| RunError::Storage { detail: e.to_string() })?;
        let identity_plan = incumbent.plan_hash == cand_hash.to_hex();

        // The candidate's manifest, the way `fork --plan` resolves one:
        // every node re-pinned against the store, budgets and limits
        // inherited from the incumbent, the recorded input as the seed.
        // `llm_available` is true unconditionally — nothing is dispatched,
        // and an abstract node's turns are answered from the journal like
        // any other effect.
        let mut manifest = self.facade.with_store(|m| match draft_fields {
            Some(fields) => RunManifest::resolve_with_fields(
                m,
                ns,
                run_id,
                cand_hash,
                Some(fields),
                plan,
                self.principal,
                incumbent.budgets,
                incumbent.ask_ttl_sec,
                incumbent.input.clone(),
                true,
            ),
            None => RunManifest::resolve(
                m,
                ns,
                run_id,
                cand_hash,
                plan,
                self.principal,
                incumbent.budgets,
                incumbent.ask_ttl_sec,
                incumbent.input.clone(),
                true,
            ),
        })?;
        manifest.llm_max_tokens = incumbent.llm_max_tokens;
        manifest.max_effects_per_attempt = incumbent.max_effects_per_attempt;
        manifest.llm_tool_result_chars = incumbent.llm_tool_result_chars;
        manifest.llm_context_tokens = incumbent.llm_context_tokens;
        let executors = manifest.executors();
        let arg_schemas = arg_schemas_for(self.facade, &manifest)?;
        let validate_args = make_validate_args(&arg_schemas);
        let reduce = crate::reducers::make_reduce(&manifest.reducers);
        let env = StepEnv {
            plan,
            executors: &executors,
            budgets: manifest.budgets.to_budgets(),
            reduce: &reduce,
            eval_cond: &builtin_eval,
            ask_ttl_sec: manifest.ask_ttl_sec,
            validate_args: &validate_args,
            llm_reserve_tokens: manifest.llm_reserve_tokens(),
            max_effects_per_attempt: manifest.max_effects_per_attempt(),
            llm_tool_result_chars: manifest.llm_tool_result_chars,
            llm_context_tokens: manifest.llm_context_tokens,
        };

        // The incumbent's side of the ledger.
        let incumbent_outcome = view
            .checkpoints
            .last()
            .and_then(|c| serde_json::from_value::<SchedulerState>(c.scheduler.clone()).ok())
            .map(|s| outcome_label(s.outcome()))
            .unwrap_or_else(|| "open".into());
        let mut incumbent_spent = ShadowSpend::default();
        for e in view.entries.values() {
            if let Some((_, o)) = &e.result {
                incumbent_spent.add(o);
            }
        }
        // The cancel the live run saw, if any: fed at the same superstep.
        let cancel = view.checkpoints.iter().find_map(|c| {
            let a = c.scheduler.get("cancel")?.as_array()?;
            Some((c.superstep, a.first()?.as_str()?.to_string(), a.get(1)?.as_str()?.to_string()))
        });
        // Clock readings follow `verify`'s scheme exactly — the close of
        // the checkpoint these resolutions close, the open of the next —
        // indexed by the candidate's own checkpoint count, so an identity
        // rehearsal is byte-equal to verify. Past the incumbent's last
        // checkpoint the last reading is held: a candidate cannot
        // manufacture time the live run never saw.
        let last_reading = view.checkpoints.last().map(|c| c.decisions.clock_close_ms).unwrap_or(0);
        let close_reading = |idx: usize, floor: u64| -> u64 {
            view.checkpoints.get(idx).map(|c| c.decisions.clock_close_ms).unwrap_or(last_reading).max(floor)
        };
        let open_reading = |idx: usize, floor: u64| -> u64 {
            view.checkpoints.get(idx).map(|c| c.decisions.clock_open_ms).unwrap_or(last_reading).max(floor)
        };

        let mut row = ShadowPlanRun {
            run_id: run_id.to_string(),
            incumbent_outcome,
            candidate_outcome: "open".into(),
            supersteps: 0,
            effects_replayed: 0,
            out_of_support: Vec::new(),
            incumbent_spent,
            candidate_spent: ShadowSpend::default(),
            identity: identity_plan.then_some(ShadowIdentity { checkpoints_compared: 0, consistent: true }),
            verdict: "out_of_support".into(),
            note: None,
        };
        if view.checkpoints.is_empty() {
            row.note = Some("no checkpoints — nothing to rehearse".into());
            return Ok(row);
        }

        let mut st = SchedulerState::new(run_id, plan);
        let first_open = view.checkpoints[0].decisions.clock_open_ms;
        let mut events = vec![
            EventIn::ClockReading { unix_ms: first_open },
            EventIn::Start { input: incumbent.input.clone() },
        ];
        let mut consumed: BTreeSet<JournalKey> = BTreeSet::new();
        let mut ckpt_idx = 0usize;
        let mut guard = 0u32;
        loop {
            guard += 1;
            if guard > 100_000 {
                row.note = Some("replay did not terminate".into());
                break;
            }
            if let Some((at, by, reason)) = &cancel {
                if st.cancel.is_none() && st.superstep >= *at {
                    events.push(EventIn::CancelSeen { principal: by.clone(), reason: reason.clone() });
                }
            }
            let out = step(&env, st, &events);
            st = out.state;
            events = Vec::new();
            let mut resolved: Vec<EventIn> = Vec::new();
            let mut parked = false;
            let mut unanswered: Option<JournalKey> = None;
            for cmd in out.commands {
                match cmd {
                    Command::WriteIntent { .. } | Command::Finish { .. } => {}
                    Command::Dispatch { key, .. } => {
                        match view.entries.get(&key).and_then(|e| e.result.as_ref().map(|(_, o)| o.clone())) {
                            Some(outcome) => {
                                if consumed.insert(key.clone()) {
                                    row.effects_replayed += 1;
                                    row.candidate_spent.add(&outcome);
                                }
                                resolved.push(EventIn::EffectResolved { key, outcome });
                            }
                            None => {
                                unanswered = Some(key);
                                break;
                            }
                        }
                    }
                    Command::EmitEnvelope { .. } => parked = true,
                    Command::WriteCheckpoint { superstep, state_json, .. } => {
                        if let Some(id) = row.identity.as_mut() {
                            match view.checkpoints.get(ckpt_idx) {
                                Some(stored) => {
                                    id.checkpoints_compared += 1;
                                    if stored.superstep != superstep || stored.scheduler != state_json {
                                        id.consistent = false;
                                    }
                                }
                                None => id.consistent = false,
                            }
                        }
                        ckpt_idx += 1;
                    }
                }
            }
            if let Some(key) = unanswered {
                row.out_of_support.push(key_label(&key));
                row.note = Some(format!(
                    "effect {} has no journaled result under the candidate — out of support",
                    key_label(&key)
                ));
                break;
            }
            if st.is_terminal() {
                if let Some(id) = row.identity.as_mut() {
                    if let Some(stored) = view.checkpoints.get(ckpt_idx) {
                        id.checkpoints_compared += 1;
                        let replay_json = serde_json::to_value(&st).unwrap_or(Value::Null);
                        if stored.scheduler != replay_json {
                            id.consistent = false;
                        }
                    }
                }
                break;
            }
            if !resolved.is_empty() {
                let close = close_reading(ckpt_idx, st.clock_ms);
                events.push(EventIn::ClockReading { unix_ms: close });
                events.extend(resolved);
                continue;
            }
            if parked {
                // Settle the asks from the journal, exactly as `verify` does:
                // a forwarded subgraph ask by the presence of its intent, a
                // client ask by its journaled response. One with neither is
                // out of support — the live run never got that answer.
                let close = close_reading(ckpt_idx, st.clock_ms);
                let mut settled_any = false;
                let mut missing: Option<JournalKey> = None;
                for (id, pending) in st.pending_asks.clone() {
                    if matches!(executors.get(pending.node_idx), Some(NodeExecutor::Subgraph { .. })) {
                        if view.entries.contains_key(&pending.key) {
                            events.push(EventIn::AskForwarded { tool_call_id: id });
                            settled_any = true;
                        } else {
                            missing = Some(pending.key.clone());
                        }
                        continue;
                    }
                    match view.entries.get(&pending.key).and_then(|e| e.result.as_ref()) {
                        Some((_, outcome)) => {
                            if !settled_any {
                                events.push(EventIn::ClockReading { unix_ms: close });
                            }
                            if consumed.insert(pending.key.clone()) {
                                row.effects_replayed += 1;
                                row.candidate_spent.add(outcome);
                            }
                            events.push(EventIn::ResponseSettled { tool_call_id: id, outcome: outcome.clone() });
                            settled_any = true;
                        }
                        None => missing = Some(pending.key.clone()),
                    }
                }
                if settled_any {
                    continue;
                }
                if let Some(key) = missing {
                    row.out_of_support.push(key_label(&key));
                    row.note = Some(format!("ask {} was never answered in the journal — out of support", key_label(&key)));
                }
                break;
            }
            // Between supersteps: the next journaled open, or the last one.
            let open = open_reading(ckpt_idx, st.clock_ms);
            events.push(EventIn::ClockReading { unix_ms: open });
        }
        row.supersteps = st.superstep;
        row.candidate_outcome = if row.out_of_support.is_empty() && st.is_terminal() {
            outcome_label(st.outcome())
        } else if row.out_of_support.is_empty() {
            "open".into()
        } else {
            "out_of_support".into()
        };
        let score = |label: &str| if label == "completed" { 1 } else { 0 };
        row.verdict = if !row.out_of_support.is_empty() {
            "out_of_support".into()
        } else {
            match score(&row.candidate_outcome).cmp(&score(&row.incumbent_outcome)) {
                std::cmp::Ordering::Greater => "better".into(),
                std::cmp::Ordering::Less => "worse".into(),
                std::cmp::Ordering::Equal => "same".into(),
            }
        };
        Ok(row)
    }
}

/// The §6.11 argument-validation table for a manifest: tool_name →
/// input_schema for every STRICT pinned Definition. A pure function of the
/// manifest, so live, verify and shadow agree.
pub(crate) fn arg_schemas_for(
    facade: &areev_cal::AreevFacade,
    manifest: &RunManifest,
) -> Result<std::collections::BTreeMap<String, Value>, RunError> {
    let mut schemas = std::collections::BTreeMap::new();
    for p in &manifest.pinned {
        if p.executor != "host" || p.tool_hash.is_empty() {
            continue;
        }
        let h = Hash::from_hex(&p.tool_hash).map_err(|e| RunError::Storage { detail: e.to_string() })?;
        let def = facade
            .with_store(|m| m.get(&h))
            .map_err(|e| RunError::Storage { detail: e.to_string() })?
            .to_tool()
            .map_err(|e| RunError::Storage { detail: e.to_string() })?;
        if def.strict == Some(true) {
            if let Some(schema) = def.input_schema {
                schemas.insert(p.tool_name.clone(), schema);
            }
        }
    }
    Ok(schemas)
}
