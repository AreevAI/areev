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
//! Zero writes and zero dispatches are structural in the default mode: the
//! path holds no model, reads the journal through the facade, and the only
//! state it builds is the scheduler's, in memory.
//!
//! ## Shadowing a candidate VERSION, not only a candidate plan (#277)
//!
//! Answering every effect from the journal by its key means the patch class
//! most likely to change an answer — a tool's *bytes* — rehearses as `same`
//! by construction: the binding is never consulted, so a candidate that
//! rebinds one node to a new Definition replays the old Definition's
//! journaled result. `ShadowOptions { reexecute: Reexecute::Pure }` is the
//! opt-in that closes it, and the eligibility rule is the whole safety
//! argument:
//!
//! > only a bound host node whose CANDIDATE Definition is a
//! > `wasm32-areev` module — pure Tier C, whose frozen import set is exactly
//! > `areev::emit`: no clock, no filesystem, no sockets — is re-executed,
//! > in the sandbox, under its pinned fuel and pages, on the input the
//! > replayed state built.
//!
//! Everything else — native blobs, `wasm32-areev-io` (which reaches the
//! network through the broker), client, abstract, subgraph, memory reads,
//! and any address this host has not pinned with `--allow-executor` — is
//! still answered from the journal and reported under `not_reexecuted` with
//! the reason. So the run's external effects stay at zero and
//! `effect_dispatches` keeps meaning "no external effect"; the count of
//! modules that ran is reported separately as `sandbox_executions`.
//!
//! What a buyer is shown before an upgrade is the terminal merged-context
//! diff, and it is reported as **key paths only** (`changed_keys`,
//! `added_keys`, `removed_keys` — RFC 6901 pointers), never values, so a
//! host can put the report on a control channel that must not carry content.

use crate::executor::{ExecResult, HostToolExecutor, PreparedCode};
use crate::journal;
use crate::manifest::{PinnedTool, RunManifest};
use crate::runner::{builtin_eval, idempotency_key, make_validate_args, Runner};
use crate::RunError;
use areev_core::error::Hash;
use areev_core::types::{Workflow, WorkflowEdge};
use areev_run_core::{
    step, Command, EffectOutcome, EventIn, FailCause, JournalKey, NodeExecutor, PlanGraph,
    RunOutcome, SchedulerState, StepEnv,
};
use serde_json::{Map, Value};
use std::collections::BTreeSet;
use std::sync::Arc;

/// The plan to rehearse: a stored Workflow grain, or an unstored draft body
/// (validated by `PlanGraph::build` before any run is touched).
#[derive(Debug, Clone)]
pub enum PlanCandidate {
    Hash(Hash),
    Body(Map<String, Value>),
}

/// How a rehearsal answers a bound node's effect (#277).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Reexecute {
    /// Every effect is answered from the journal. The default, and the only
    /// mode whose zero-execution guarantee is structural.
    #[default]
    Off,
    /// A bound host node whose candidate Definition is a pure
    /// `wasm32-areev` module runs in the sandbox on the input the replayed
    /// state built; everything else is still answered from the journal.
    Pure,
}

impl Reexecute {
    /// The surface spelling — `"off"` / `"pure"`, and nothing else. An empty
    /// string is refused rather than read as the default: absent means the
    /// default, and a caller who wrote the key and left it blank is far more
    /// likely to have a bug than an intent. Unknown values are the caller's
    /// to refuse, with the caller's own message.
    pub fn parse(s: &str) -> Option<Reexecute> {
        match s.trim() {
            "off" => Some(Reexecute::Off),
            "pure" => Some(Reexecute::Pure),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Reexecute::Off => "off",
            Reexecute::Pure => "pure",
        }
    }
}

/// Knobs on a plan rehearsal. A struct rather than an argument so a later
/// knob does not re-point every caller — the bindings have no keyword
/// arguments and would each grow a positional parameter.
#[derive(Debug, Clone, Copy, Default)]
pub struct ShadowOptions {
    pub reexecute: Reexecute,
}

impl ShadowOptions {
    pub fn reexecute(mode: Reexecute) -> Self {
        ShadowOptions { reexecute: mode }
    }
}

/// A node the rehearsal did NOT re-execute, and why.
///
/// The reason is the point: "I passed `reexecute: pure` and got
/// `sandbox_executions: 0`" is otherwise undebuggable, and the causes —
/// unpinned address, no sandbox configured, a native or capability runtime,
/// a node that is not a bound host tool at all — need different fixes.
#[derive(Debug, Clone, serde::Serialize)]
pub struct NotReexecuted {
    pub node: String,
    pub why: String,
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
    /// Nodes whose candidate module ran in the sandbox instead of being
    /// answered from the journal. `None` unless `reexecute: "pure"` — every
    /// field below is absent in the default mode, so a report taken without
    /// the option is byte-identical to one taken before it existed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reexecuted: Option<Vec<String>>,
    /// Nodes the mode could not re-execute, each with its reason.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub not_reexecuted: Option<Vec<NotReexecuted>>,
    /// Sandbox invocations this run made. Distinct from `effect_dispatches`,
    /// which stays 0: a pure module reaches nothing outside its own memory.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sandbox_executions: Option<u64>,
    /// The terminal merged-context diff, incumbent → candidate, as RFC 6901
    /// key paths. **Never values** — the report is safe on a control channel
    /// that must not carry content. Present only when the replay reached a
    /// terminal state with nothing out of support; a partial context would
    /// diff as wholesale removal and read as a finding.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub changed_keys: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub added_keys: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub removed_keys: Option<Vec<String>>,
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
    /// Always 0 — stated in the artifact so the claim is explicit. It means
    /// **no external effect**, and it keeps that meaning under
    /// `reexecute: "pure"`: a pure module has no clock, no filesystem and no
    /// sockets, so running one dispatches nothing. What ran is counted as
    /// `sandbox_executions` instead, never folded in here.
    pub effect_dispatches: u64,
    /// Always 0 — the replay path reaches no writer.
    pub writes: u64,
    /// The re-execution mode, when it was not the default. Absent otherwise,
    /// so the default report is byte-identical to the pre-#277 one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reexecute: Option<String>,
    /// Sandbox invocations over every run.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sandbox_executions: Option<u64>,
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

/// The three key-path lists of a context diff.
#[derive(Default)]
struct KeyPaths {
    changed: Vec<String>,
    added: Vec<String>,
    removed: Vec<String>,
}

impl KeyPaths {
    fn sorted(mut self) -> Self {
        for v in [&mut self.changed, &mut self.added, &mut self.removed] {
            v.sort();
            v.dedup();
        }
        self
    }
}

/// RFC 6901 escaping: `~` → `~0`, `/` → `~1`, in that order.
fn escape_pointer(key: &str) -> String {
    key.replace('~', "~0").replace('/', "~1")
}

/// Walk two JSON values and record WHERE they differ, never HOW.
///
/// Objects are descended (that is what makes `/invoice/amount` readable);
/// anything else — arrays, scalars — compares whole at its own path, because
/// reporting an array index as a key path would leak the shape of the data
/// without being any more actionable. A key present on one side only is one
/// entry for the whole subtree, not one per leaf beneath it.
fn diff_key_paths(a: &Value, b: &Value, at: &str, out: &mut KeyPaths) {
    match (a, b) {
        (Value::Object(x), Value::Object(y)) => {
            for (k, av) in x {
                let path = format!("{at}/{}", escape_pointer(k));
                match y.get(k) {
                    Some(bv) => diff_key_paths(av, bv, &path, out),
                    None => out.removed.push(path),
                }
            }
            for k in y.keys() {
                if !x.contains_key(k) {
                    out.added.push(format!("{at}/{}", escape_pointer(k)));
                }
            }
        }
        _ if a != b => out.changed.push(if at.is_empty() { "/".into() } else { at.into() }),
        _ => {}
    }
}

/// Record a node once, first-seen order. A node's eligibility is a property
/// of its pin, so it is constant across attempts and cycles — listing it per
/// dispatch would make a three-iteration loop look like three nodes.
fn note_node(list: Option<&mut Vec<String>>, node: &str) {
    if let Some(list) = list {
        if !list.iter().any(|n| n == node) {
            list.push(node.to_string());
        }
    }
}

fn note_refusal(list: Option<&mut Vec<NotReexecuted>>, node: &str, why: String) {
    if let Some(list) = list {
        if !list.iter().any(|n| n.node == node) {
            list.push(NotReexecuted { node: node.to_string(), why });
        }
    }
}

/// May this node's CANDIDATE binding be re-executed under `Reexecute::Pure`?
/// `Ok(uri)` names the blob to run; `Err(why)` is the sentence that goes in
/// `not_reexecuted`.
///
/// Every arm is deny-by-default and host-side: the declaration replicates,
/// the authorization to execute never does (see [`crate::executor::CodeExecutor`]).
/// A rehearsal is not a weaker place to apply that rule than a run — it is
/// the same act of running someone's code — so the pin and the sandbox
/// configuration are checked here exactly as `Runner::start` checks them.
fn pure_module(pin: Option<&PinnedTool>, executor: Option<&dyn HostToolExecutor>) -> Result<String, String> {
    const PURE: &str = "wasm32-areev";
    let Some(pin) = pin else {
        return Err("the candidate manifest pinned nothing for this node".into());
    };
    if pin.executor != "host" {
        return Err(format!("executes as {}, not as a bound host tool", pin.executor));
    }
    let Some(uri) = pin.executor_uri.as_deref() else {
        return Err("binds no cas:// code blob, so there is nothing to re-execute".into());
    };
    match pin.runtime.as_deref() {
        Some(PURE) => {}
        Some("wasm32-areev-io") => {
            return Err(
                "declares runtime \"wasm32-areev-io\" — a capability module reaches the network \
                 through the broker, so re-running it would be an external effect"
                    .into(),
            )
        }
        other => {
            return Err(format!(
                "declares runtime {:?} — only a pure wasm32-areev module is deterministic by \
                 construction, so nothing else is re-executed",
                other.unwrap_or("native")
            ))
        }
    }
    if pin.capabilities.is_some() {
        return Err("pins a capability declaration, which a pure module does not have".into());
    }
    let Some(executor) = executor else {
        return Err("this rehearsal was given no host executor".into());
    };
    if !executor.code_allowed(&pin.tool_hash, uri) {
        return Err(format!(
            "{uri} is not pinned by this host — pin it with --allow-executor {}",
            crate::executor::strip_cas(uri)
        ));
    }
    if !executor.runtime_supported(PURE) {
        return Err(format!(
            "this host cannot dispatch {PURE:?} — configure the sandbox with --sandbox-cmd"
        ));
    }
    Ok(uri.to_string())
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
        self.shadow_plan_with(run_ids, candidate, &ShadowOptions::default())
    }

    /// [`Runner::shadow_plan`] under [`ShadowOptions`] — `reexecute:
    /// Reexecute::Pure` re-runs the candidate's pure `wasm32-areev` modules
    /// through THIS runner's executor, so the host's `--allow-executor` pins
    /// and `--sandbox-cmd` are what decide whether anything runs at all.
    pub fn shadow_plan_with(
        &self,
        run_ids: &[String],
        candidate: &PlanCandidate,
        opts: &ShadowOptions,
    ) -> Result<ShadowPlanReport, RunError> {
        let runs: Vec<(String, String)> =
            run_ids.iter().map(|id| (id.clone(), self.ns.clone())).collect();
        ShadowCtx {
            facade: &self.facade,
            principal: &self.principal,
            executor: Some(&self.executor),
            opts: *opts,
        }
        .shadow_plan(&runs, candidate)
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
    ShadowCtx { facade, principal, executor: None, opts: ShadowOptions::default() }
        .shadow_plan(runs, candidate)
}

/// [`shadow_plan_scoped`] under options, for a host that holds an executor
/// but no [`Runner`] — `executor` is what `reexecute: "pure"` dispatches
/// through, and `None` makes every node report as `not_reexecuted`.
pub fn shadow_plan_scoped_with(
    facade: &areev_cal::AreevFacade,
    principal: &str,
    runs: &[(String, String)],
    candidate: &PlanCandidate,
    opts: &ShadowOptions,
    executor: Option<&Arc<dyn HostToolExecutor>>,
) -> Result<ShadowPlanReport, RunError> {
    ShadowCtx { facade, principal, executor, opts: *opts }.shadow_plan(runs, candidate)
}

struct ShadowCtx<'a> {
    facade: &'a areev_cal::AreevFacade,
    principal: &'a str,
    /// The host's executor, present only on the paths that hold one. Pure
    /// re-execution dispatches through it; every other mode never touches it.
    executor: Option<&'a Arc<dyn HostToolExecutor>>,
    opts: ShadowOptions,
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
            reexecute: (self.opts.reexecute != Reexecute::Off)
                .then(|| self.opts.reexecute.as_str().to_string()),
            sandbox_executions: (self.opts.reexecute != Reexecute::Off).then_some(0),
        };
        for (run_id, ns) in runs {
            let row = self.shadow_one(run_id, ns, &plan, &cand_hash, draft_fields)?;
            if let (Some(total), Some(n)) = (report.sandbox_executions.as_mut(), row.sandbox_executions) {
                *total += n;
            }
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

    /// Run one pure module in the sandbox and shape its answer exactly as the
    /// live pool does — same `journal_bytes` accounting, same
    /// `catch_unwind` rule, zero tokens and zero USD (a wasm module buys
    /// nothing from a provider). The blob is read and digest-verified here,
    /// through the facade, for the same reason `drive` reads it on the driver
    /// thread: the executor never touches a store handle.
    fn run_pure_module(
        &self,
        pin: &PinnedTool,
        uri: &str,
        key: &JournalKey,
        input: &Value,
    ) -> Result<EffectOutcome, RunError> {
        let bytes = self
            .facade
            .with_store(|m| m.get_blob(uri))
            .map_err(|e| RunError::Storage { detail: e.to_string() })?;
        let code = PreparedCode {
            uri: uri.to_string(),
            bytes,
            runtime: pin.runtime.clone(),
            limits: pin.runtime_limits.clone(),
            // A pure module declares none, and `pure_module` refused the pin
            // if it did — restating it here means no path can hand the
            // sandbox `--allow-fetch` off a rehearsal.
            capabilities: None,
        };
        let idem = idempotency_key(key, input);
        let executor = self.executor.expect("pure_module proves the executor is there");
        let executed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            executor.execute_code(&pin.tool_name, &pin.tool_hash, &code, input, &idem)
        }))
        .unwrap_or_else(|p| {
            let msg = p
                .downcast_ref::<&str>()
                .map(|s| s.to_string())
                .or_else(|| p.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "executor panicked".into());
            ExecResult::Err {
                cause: FailCause::ExecutorError,
                detail: format!("executor panicked: {msg}"),
            }
        });
        Ok(match executed {
            ExecResult::Ok(result) => {
                let journal_bytes = crate::journal::outcome_journal_bytes(&EffectOutcome::Completed {
                    result: result.clone(),
                    journal_bytes: 0,
                    input_tokens: 0,
                    output_tokens: 0,
                    usd_micros: 0,
                });
                EffectOutcome::Completed {
                    result,
                    journal_bytes,
                    input_tokens: 0,
                    output_tokens: 0,
                    usd_micros: 0,
                }
            }
            ExecResult::Err { cause, detail } => {
                let journal_bytes = detail.len() as u64;
                EffectOutcome::Failed { cause, detail, journal_bytes }
            }
        })
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

        // The incumbent's side of the ledger. Deserialized ONCE: the terminal
        // label and the terminal merged context both come off it, and a
        // second parse of the same checkpoint could disagree with the first.
        let incumbent_state = view
            .checkpoints
            .last()
            .and_then(|c| serde_json::from_value::<SchedulerState>(c.scheduler.clone()).ok());
        let incumbent_outcome = incumbent_state
            .as_ref()
            .map(|s| outcome_label(s.outcome()))
            .unwrap_or_else(|| "open".into());
        let incumbent_context =
            incumbent_state.as_ref().map(|s| s.context.clone()).unwrap_or(Value::Null);
        let reexec = self.opts.reexecute == Reexecute::Pure;
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
            reexecuted: reexec.then(Vec::new),
            not_reexecuted: reexec.then(Vec::new),
            sandbox_executions: reexec.then_some(0),
            changed_keys: None,
            added_keys: None,
            removed_keys: None,
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
                    Command::Dispatch { key, input, .. } => {
                        // #277: under `reexecute: "pure"` a bound node whose
                        // CANDIDATE Definition is a pure wasm32-areev module
                        // runs, on the input the replayed state just built,
                        // instead of being answered from the journal — which
                        // is the only way a version whose plan is unchanged
                        // and whose tool bytes are not can rehearse as
                        // anything but `same`. It consults no journal row, so
                        // it is never out of support and never `replayed`.
                        let ran = if reexec {
                            let pin = manifest.pinned.iter().find(|p| p.node == key.node);
                            match pure_module(pin, self.executor.map(|e| e.as_ref())) {
                                Ok(uri) => {
                                    let pin = pin.expect("pure_module proves the pin is there");
                                    let outcome = self.run_pure_module(pin, &uri, &key, &input)?;
                                    note_node(row.reexecuted.as_mut(), &key.node);
                                    if let Some(n) = row.sandbox_executions.as_mut() {
                                        *n += 1;
                                    }
                                    Some(outcome)
                                }
                                Err(why) => {
                                    note_refusal(row.not_reexecuted.as_mut(), &key.node, why);
                                    None
                                }
                            }
                        } else {
                            None
                        };
                        if let Some(outcome) = ran {
                            resolved.push(EventIn::EffectResolved { key, outcome });
                            continue;
                        }
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
        // The answer a buyer is shown before an upgrade: WHICH fields of the
        // terminal state the candidate would have produced differently. Only
        // when the replay actually reached a terminal state — a context
        // abandoned mid-run diffs as wholesale removal, which reads as a
        // finding and is not one.
        if reexec && row.out_of_support.is_empty() && st.is_terminal() {
            let mut paths = KeyPaths::default();
            diff_key_paths(&incumbent_context, &st.context, "", &mut paths);
            let paths = paths.sorted();
            row.changed_keys = Some(paths.changed);
            row.added_keys = Some(paths.added);
            row.removed_keys = Some(paths.removed);
        }
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
            .with_store(|m| m.get_stored(&h))
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
