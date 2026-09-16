//! Replay — score a loop configuration against the immutable past.
//!
//! The engine is a pure function of (file, policy, now): `run` never reads
//! the clock, and the golden suite byte-pins queues because of it. So a
//! candidate configuration has a measurable quality on the recorded past:
//! which findings it would have produced at each historical pass, how many
//! of those the humans went on to approve or reject, how many of the
//! approved ones later regressed, and how much queue it would have made.
//! Dream-RSI (arXiv 2609.14858) does this for exploration policies over
//! discovery trees and always includes the incumbent in the candidate set;
//! the same rule holds here — the report is a comparison, never a bare
//! number. `docs/loop-proposal.md` §17 named this rung 1 of the ladder.
//!
//! Three disciplines, all structural:
//!
//! - **Prefix only.** Every step reads through [`PrefixView`], which hides
//!   any grain created after the step's `now` — the paper's prefix rule, no
//!   leakage from the future.
//! - **Zero writes.** The view refuses every mutating method of
//!   [`OmsSubstrate`], and the engine holds `&S`, not `&mut S`.
//! - **Scope honesty.** An LLM is not a pure function of the evidence and
//!   an external command is out of process, so neither is replayed; they
//!   appear in the report as `not_replayed` with the reason. Telemetry
//!   rollups are not time-indexed, so the telemetry-fed analyzers are not
//!   replayed either.
//!
//! No auto-adoption: replay informs, and adopting the configuration remains
//! the policy file or `POST /api/loop/config`, by a human.

use crate::analyzer::OutcomeInput;
use crate::config::{AnalyzerConfig, LoopPersisted};
use crate::engine::{Engine, RunOptions, LOOP_NS};
use crate::error::{Error, Result};
use crate::model::{GrainRecord, Origin};
use crate::policy::Policy;
use crate::recommendation::{RecStatus, Recommendation};
use crate::substrate::{Capabilities, GrainSpec, HeadGroup, OmsSubstrate, ReadOpts, SubstrateRead, TelemetryView};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

/// A candidate loop configuration to score against the past.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReplayCandidate {
    /// Per-analyzer configuration, keyed by full analyzer id — the same
    /// shape `set_analyzer_config` stores (`enabled`, `params`,
    /// `severity_floor`, `namespaces`). Each entry REPLACES the file's entry
    /// for that analyzer; analyzers not named keep the file's config.
    #[serde(default)]
    pub config: BTreeMap<String, AnalyzerConfig>,
    /// An optional host policy to replay under (severity floors, the deny
    /// list, `near_duplicate`, …) instead of the engine's. Auto-apply
    /// grants are irrelevant — a replay applies nothing.
    #[serde(default)]
    pub policy: Option<Policy>,
}

impl ReplayCandidate {
    pub fn from_json(s: &str) -> Result<Self> {
        serde_json::from_str(s).map_err(|e| Error::InvalidProposal(format!("replay candidate: {e}")))
    }
}

/// How `now` steps through the window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplayStep {
    /// One step per recorded pass — the moments the loop actually ran,
    /// reconstructed from the audit trail (every stored finding is an
    /// Observation stamped with the pass's `now`). A pass that stored
    /// nothing left no trace and is not a step.
    PerPass,
    /// A fixed stride from the window's start, in ms.
    Stride(i64),
}

impl ReplayStep {
    /// `per-pass`, or a duration like `1d` / `12h` / `30m` / `86400000`.
    pub fn parse(s: &str) -> Option<Self> {
        let s = s.trim();
        if s.eq_ignore_ascii_case("per-pass") || s.eq_ignore_ascii_case("per_pass") {
            return Some(ReplayStep::PerPass);
        }
        let (num, unit) = match s.char_indices().find(|(_, c)| !c.is_ascii_digit()) {
            Some((i, _)) => (&s[..i], &s[i..]),
            None => (s, "ms"),
        };
        let n: i64 = num.parse().ok()?;
        let mult = match unit {
            "ms" => 1,
            "s" => 1_000,
            "m" => 60_000,
            "h" => 3_600_000,
            "d" => 86_400_000,
            _ => return None,
        };
        (n > 0).then(|| ReplayStep::Stride(n * mult))
    }

    fn label(&self) -> String {
        match self {
            ReplayStep::PerPass => "per_pass".into(),
            ReplayStep::Stride(ms) => format!("stride:{ms}ms"),
        }
    }
}

/// What to replay over.
#[derive(Debug, Clone)]
pub struct ReplayOptions {
    /// The window's start (inclusive). `None` = from the first recorded pass
    /// (per-pass) — a stride needs one.
    pub since_ms: Option<i64>,
    /// The window's end — the caller's `now`.
    pub until_ms: i64,
    pub step: ReplayStep,
    /// The global namespace filter a live run would use (empty = all).
    pub namespaces: Vec<String>,
}

impl ReplayOptions {
    /// The one argument parser every surface shares: a `window` like `90d`
    /// (back from `now`) or an explicit `since_ms`, and a `step` of
    /// `per-pass` (default) or a duration. A window and a since together are
    /// refused rather than silently ranked.
    pub fn from_args(
        window: Option<&str>,
        since_ms: Option<i64>,
        step: Option<&str>,
        namespaces: Vec<String>,
        now_ms: i64,
    ) -> Result<Self> {
        let bad = |what: String| Error::InvalidProposal(format!("replay: {what}"));
        let since = match (window, since_ms) {
            (Some(_), Some(_)) => return Err(bad("give a window or a since, not both".into())),
            (Some(w), None) => match ReplayStep::parse(w) {
                Some(ReplayStep::Stride(ms)) => Some(now_ms - ms),
                _ => return Err(bad(format!("window {w:?} is not a duration like 90d, 12h or 30m"))),
            },
            (None, s) => s,
        };
        let step = match step {
            None => ReplayStep::PerPass,
            Some(s) => ReplayStep::parse(s)
                .ok_or_else(|| bad(format!("step {s:?} is not per-pass or a duration like 1d")))?,
        };
        Ok(ReplayOptions { since_ms: since, until_ms: now_ms, step, namespaces })
    }
}

/// The request every surface accepts (`areev loop replay --config FILE`
/// reads the file into it and adds the flags; `POST /api/loop/replay` and
/// the bindings take it whole): the candidate plus the window.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReplayRequest {
    #[serde(default)]
    pub config: BTreeMap<String, AnalyzerConfig>,
    #[serde(default)]
    pub policy: Option<Policy>,
    /// A duration back from now, e.g. `90d`.
    #[serde(default)]
    pub window: Option<String>,
    #[serde(default)]
    pub since_ms: Option<i64>,
    /// `per-pass` (default) or a duration like `1d`.
    #[serde(default)]
    pub step: Option<String>,
    #[serde(default)]
    pub namespaces: Vec<String>,
}

impl ReplayRequest {
    pub fn from_json(s: &str) -> Result<Self> {
        serde_json::from_str(s).map_err(|e| Error::InvalidProposal(format!("replay request: {e}")))
    }

    pub fn resolve(self, now_ms: i64) -> Result<(ReplayCandidate, ReplayOptions)> {
        let opts = ReplayOptions::from_args(
            self.window.as_deref(),
            self.since_ms,
            self.step.as_deref(),
            self.namespaces,
            now_ms,
        )?;
        Ok((ReplayCandidate { config: self.config, policy: self.policy }, opts))
    }
}

/// One would-be finding at one step.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct ReplayFinding {
    pub step_ms: i64,
    pub analyzer: String,
    pub dedup_key: String,
    pub summary: String,
    pub severity: String,
    pub target_ref: String,
    /// The content address a live pass would have stored it under, when the
    /// substrate can compute one without writing.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub address: Option<String>,
    /// How the recorded history treated a finding with this dedup key:
    /// `approved` (incl. applied / rolled back), `rejected`,
    /// `never_reviewed` (stored, still pending or expired), or
    /// `never_proposed` (the incumbent never produced it).
    pub recorded: String,
    /// The latest Verify-gate verdict on the recorded apply of this key, when
    /// there was one: `held`, `held_costlier`, `regressed`, `drifted`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub outcome: Option<String>,
}

/// Counts for one analyzer, or the total.
#[derive(Debug, Clone, Default, Serialize, PartialEq)]
pub struct ReplayTally {
    pub findings: u64,
    pub approved: u64,
    pub rejected: u64,
    pub never_reviewed: u64,
    pub never_proposed: u64,
    pub regressed: u64,
    pub drifted: u64,
    pub held: u64,
}

impl ReplayTally {
    fn add(&mut self, f: &ReplayFinding) {
        self.findings += 1;
        match f.recorded.as_str() {
            "approved" => self.approved += 1,
            "rejected" => self.rejected += 1,
            "never_reviewed" => self.never_reviewed += 1,
            _ => self.never_proposed += 1,
        }
        match f.outcome.as_deref() {
            Some("regressed") => self.regressed += 1,
            Some("drifted") => self.drifted += 1,
            Some("held") | Some("held_costlier") => self.held += 1,
            _ => {}
        }
    }
}

/// One arm of the comparison: the incumbent or the candidate.
#[derive(Debug, Clone, Default, Serialize, PartialEq)]
pub struct ReplayArm {
    pub total: ReplayTally,
    pub per_analyzer: BTreeMap<String, ReplayTally>,
    /// Findings per step, in step order — the queue volume.
    pub queue_per_step: Vec<u64>,
    pub findings: Vec<ReplayFinding>,
    /// Analyzers skipped at any step and why (disabled, denied, not replayed).
    pub skipped: BTreeMap<String, String>,
}

/// Something the replay did not rehearse, and why.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct NotReplayed {
    pub what: String,
    pub reason: String,
}

/// The report: a comparison, never a bare number.
#[derive(Debug, Clone, Serialize)]
pub struct ReplayReport {
    pub since_ms: i64,
    pub until_ms: i64,
    pub step: String,
    /// The `now` values replayed, in order.
    pub steps: Vec<i64>,
    pub incumbent: ReplayArm,
    pub candidate: ReplayArm,
    pub not_replayed: Vec<NotReplayed>,
    /// Recorded review decisions and outcomes are matched to would-be
    /// findings by dedup key — stated so the overlap columns are read for
    /// what they are.
    pub matching: &'static str,
}

/// A read-only, prefix-bounded view of a substrate. Every grain created
/// after `until_ms` is invisible, and every write is refused — the two
/// properties a rehearsal needs to be exact and harmless, both enforced by
/// the type rather than promised.
pub struct PrefixView<'a, S: OmsSubstrate> {
    inner: &'a S,
    until_ms: i64,
}

impl<'a, S: OmsSubstrate> PrefixView<'a, S> {
    pub fn new(inner: &'a S, until_ms: i64) -> Self {
        PrefixView { inner, until_ms }
    }
}

fn read_only<T>(what: &str) -> Result<T> {
    Err(Error::Substrate(format!("replay is read-only: {what} refused")))
}

impl<S: OmsSubstrate> SubstrateRead for PrefixView<'_, S> {
    fn capabilities(&self) -> Capabilities {
        self.inner.capabilities()
    }
    fn grains_of_type(&self, grain_type: &str, namespace: Option<&str>, opts: ReadOpts) -> Result<Vec<GrainRecord>> {
        Ok(self
            .inner
            .grains_of_type(grain_type, namespace, opts)?
            .into_iter()
            .filter(|g| g.created_at_ms <= self.until_ms)
            .collect())
    }
    fn grain(&self, hash: &str) -> Result<Option<GrainRecord>> {
        Ok(self.inner.grain(hash)?.filter(|g| g.created_at_ms <= self.until_ms))
    }
    fn heads(&self, namespace: Option<&str>) -> Result<Vec<HeadGroup>> {
        self.inner.heads(namespace)
    }
    fn telemetry(&self, namespace: Option<&str>) -> Result<Option<TelemetryView>> {
        self.inner.telemetry(namespace)
    }
    fn validate_plan(&self, workflow: &Value) -> Result<()> {
        self.inner.validate_plan(workflow)
    }
    fn tool_evalset(&self, tool: &str) -> Result<Option<String>> {
        self.inner.tool_evalset(tool)
    }
    fn embed(&self, text: &str) -> Result<Option<Vec<f32>>> {
        self.inner.embed(text)
    }
    fn address_of(&self, spec: &GrainSpec) -> Result<Option<String>> {
        self.inner.address_of(spec)
    }
}

impl<S: OmsSubstrate> OmsSubstrate for PrefixView<'_, S> {
    fn put_grain(&mut self, _spec: &GrainSpec) -> Result<String> {
        read_only("put_grain")
    }
    fn supersede(&mut self, _target_hash: &str, _spec: &GrainSpec, _justification: &str) -> Result<String> {
        read_only("supersede")
    }
    fn retract(&mut self, _hash: &str, _reason: &str) -> Result<()> {
        read_only("retract")
    }
    fn put_blob(&mut self, _bytes: &[u8]) -> Result<String> {
        read_only("put_blob")
    }
    fn execute_cal(&mut self, _cal: &str) -> Result<Vec<Value>> {
        read_only("execute_cal")
    }
    fn validate_cal(&self, cal: &str) -> Result<()> {
        self.inner.validate_cal(cal)
    }
    fn definition_inverse(&self, statement: &str) -> Result<Option<String>> {
        self.inner.definition_inverse(statement)
    }
    fn load_state(&self) -> Result<Value> {
        self.inner.load_state()
    }
    fn store_state(&mut self, _state: &Value) -> Result<()> {
        read_only("store_state")
    }
}

/// One recorded transition on the audit trail.
struct AuditEvent {
    at_ms: i64,
    rec_hash: String,
    to: String,
    from: Option<String>,
}

/// Recorded recommendations by hash, and the dedup-key index over them.
type RecordedIndex = (BTreeMap<String, Recorded>, BTreeMap<String, Vec<String>>);

/// A recorded recommendation, as the replay scores against it.
struct Recorded {
    dedup_key: String,
    status: RecStatus,
    outcome: Option<String>,
    origin: Origin,
    /// For a revert recommendation: the recommendation it retracts.
    revert_of: Option<String>,
}

impl Engine {
    /// Score `candidate` against the recorded past, beside the incumbent
    /// (the file's config under this engine's policy). Writes nothing —
    /// see the module docs for the three structural disciplines.
    pub fn replay<S: OmsSubstrate>(
        &self,
        sub: &S,
        candidate: &ReplayCandidate,
        opts: &ReplayOptions,
    ) -> Result<ReplayReport> {
        let persisted = LoopPersisted::from_value(sub.load_state()?)?;
        let (recorded, by_key) = self.recorded(sub, &persisted)?;
        let events = audit_events(sub)?;

        // The moments the loop ran, reconstructed from the trail: every
        // stored finding's first transition is stamped with its pass's now.
        let mut passes: BTreeSet<i64> = events
            .iter()
            .filter(|e| e.to == "pending" && e.from.is_none())
            .map(|e| e.at_ms)
            .collect();
        if let Some(last) = persisted.state.last_run_ms {
            passes.insert(last);
        }
        let since = match (opts.since_ms, passes.iter().next()) {
            (Some(s), _) => s,
            (None, Some(first)) => *first,
            (None, None) => {
                return Err(Error::InvalidProposal(
                    "no recorded passes to replay through — give --since (or a window) and a --step".into(),
                ))
            }
        };
        let steps: Vec<i64> = match opts.step {
            ReplayStep::PerPass => passes.into_iter().filter(|t| *t >= since && *t <= opts.until_ms).collect(),
            ReplayStep::Stride(ms) => {
                let mut v = Vec::new();
                let mut t = since;
                while t <= opts.until_ms {
                    v.push(t);
                    t += ms;
                }
                v
            }
        };
        if steps.is_empty() {
            return Err(Error::InvalidProposal(
                "the window holds no step — widen it, or use a stride".into(),
            ));
        }

        let mut not_replayed = Vec::new();
        if self.has_llm() {
            not_replayed.push(NotReplayed {
                what: "origin=llm (the attached backend)".into(),
                reason: "a model is not a pure function of the evidence".into(),
            });
        }
        let llm_recorded = recorded.values().filter(|r| matches!(r.origin, Origin::Llm { .. })).count();
        if llm_recorded > 0 {
            not_replayed.push(NotReplayed {
                what: format!("origin=llm ({llm_recorded} recorded finding(s))"),
                reason: "a model is not a pure function of the evidence".into(),
            });
        }
        let cmd_recorded = recorded.values().filter(|r| matches!(r.origin, Origin::Command { .. })).count();
        for a in self.analyzers() {
            let m = a.manifest();
            if m.trust_class == crate::manifest::TrustClass::Command {
                not_replayed.push(NotReplayed {
                    what: format!("origin=command ({})", m.id),
                    reason: "an external command is out of process".into(),
                });
            }
        }
        if cmd_recorded > 0 && !not_replayed.iter().any(|n| n.what.starts_with("origin=command")) {
            not_replayed.push(NotReplayed {
                what: format!("origin=command ({cmd_recorded} recorded finding(s))"),
                reason: "an external command is out of process".into(),
            });
        }

        // The incumbent: the file's config under this engine's policy. The
        // candidate: its overlay on the file's config, under its own policy
        // when it names one.
        let mut candidate_config = persisted.config.clone();
        for (id, cfg) in &candidate.config {
            candidate_config.insert(id.clone(), cfg.clone());
        }
        let candidate_policy = candidate.policy.as_ref().unwrap_or(self.policy());
        let incumbent = self.replay_arm(sub, &persisted, &persisted.config, self.policy(), &steps, opts, &events, &recorded, &by_key)?;
        let candidate_arm = self.replay_arm(sub, &persisted, &candidate_config, candidate_policy, &steps, opts, &events, &recorded, &by_key)?;

        Ok(ReplayReport {
            since_ms: since,
            until_ms: opts.until_ms,
            step: opts.step.label(),
            steps,
            incumbent,
            candidate: candidate_arm,
            not_replayed,
            matching: "recorded review decisions and Verify-gate outcomes are matched to would-be findings by dedup key",
        })
    }

    /// The recorded recommendations, keyed by hash, plus a dedup-key index.
    fn recorded<S: OmsSubstrate>(
        &self,
        sub: &S,
        persisted: &LoopPersisted,
    ) -> Result<RecordedIndex> {
        let grains = sub.grains_of_type(
            crate::model::grain_type::RECOMMENDATION,
            Some(LOOP_NS),
            ReadOpts { live_only: false, since_ms: None },
        )?;
        let mut recorded = BTreeMap::new();
        let mut by_key: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for g in grains {
            let Ok(rec) = Recommendation::from_fields(&g.hash, &g.fields) else { continue };
            let status = persisted.status_index.get(&g.hash).copied().unwrap_or(RecStatus::Pending);
            let outcome = persisted
                .outcomes
                .get(&g.hash)
                .and_then(|v| v.iter().max_by_key(|o| o.measured_at_ms))
                .map(|o| o.verdict.clone());
            let revert_of = match &rec.proposal {
                crate::recommendation::Proposal::Data { data } => {
                    data.get("revert_of").and_then(Value::as_str).map(str::to_string)
                }
                _ => None,
            };
            by_key.entry(rec.dedup_key.clone()).or_default().push(g.hash.clone());
            recorded.insert(
                g.hash.clone(),
                Recorded { dedup_key: rec.dedup_key, status, outcome, origin: rec.origin, revert_of },
            );
        }
        Ok((recorded, by_key))
    }

    /// Walk the steps under one configuration, carrying the state the loop
    /// had at each: the watermark advances per step, a recorded rejection
    /// (or a measured revert) of a key puts it on the same doubling cooldown
    /// a live pass would have, a rollback frees it, and the queue the
    /// rehearsal itself produced is what dedups the next step.
    #[allow(clippy::too_many_arguments)]
    fn replay_arm<S: OmsSubstrate>(
        &self,
        sub: &S,
        persisted: &LoopPersisted,
        config: &BTreeMap<String, AnalyzerConfig>,
        policy: &Policy,
        steps: &[i64],
        opts: &ReplayOptions,
        events: &[AuditEvent],
        recorded: &BTreeMap<String, Recorded>,
        by_key: &BTreeMap<String, Vec<String>>,
    ) -> Result<ReplayArm> {
        let mut scratch = persisted.clone();
        scratch.config = config.clone();
        scratch.cooldowns.clear();
        scratch.cooldown_strikes.clear();
        let mut open: BTreeSet<String> = BTreeSet::new();
        let mut arm = ReplayArm::default();
        let run_opts = RunOptions {
            namespaces: opts.namespaces.clone(),
            ..RunOptions::default()
        };
        let no_outcomes: Vec<OutcomeInput> = Vec::new();
        let mut prev: Option<i64> = None;
        for &t in steps {
            // Decisions recorded since the previous step, in order.
            for e in events.iter().filter(|e| prev.is_none_or(|p| e.at_ms > p) && e.at_ms <= t) {
                let Some(r) = recorded.get(&e.rec_hash) else { continue };
                match e.to.as_str() {
                    "rejected" => {
                        open.remove(&r.dedup_key);
                        crate::engine::strike_cooldown(&mut scratch, r.dedup_key.clone(), e.at_ms);
                    }
                    "rolled_back" => {
                        open.remove(&r.dedup_key);
                    }
                    // Applying a revert is a verdict on the reverted finding:
                    // the live path puts it on cooldown too.
                    "applied" => {
                        if let Some(target) = r.revert_of.as_ref().and_then(|h| recorded.get(h)) {
                            open.remove(&target.dedup_key);
                            crate::engine::strike_cooldown(&mut scratch, target.dedup_key.clone(), e.at_ms);
                        }
                    }
                    _ => {}
                }
            }
            let view = PrefixView::new(sub, t);
            let pass = self.analysis_pass_inner(
                &view,
                &scratch,
                policy,
                &run_opts,
                &BTreeMap::new(),
                prev,
                t,
                &no_outcomes,
                &open,
                Some("replay"),
            )?;
            for sk in &pass.analyzers_skipped {
                arm.skipped.entry(sk.id.clone()).or_insert_with(|| sk.reason.clone());
            }
            arm.queue_per_step.push(pass.survivors.len() as u64);
            for rec in pass.survivors {
                open.insert(rec.dedup_key.clone());
                let (recorded_as, outcome) = match by_key.get(&rec.dedup_key) {
                    None => ("never_proposed", None),
                    Some(hashes) => {
                        let rs: Vec<&Recorded> = hashes.iter().filter_map(|h| recorded.get(h)).collect();
                        let approved = rs.iter().any(|r| {
                            matches!(r.status, RecStatus::Approved | RecStatus::Applied | RecStatus::RolledBack)
                        });
                        let rejected = rs.iter().any(|r| r.status == RecStatus::Rejected);
                        let outcome = rs.iter().filter_map(|r| r.outcome.clone()).next_back();
                        if approved {
                            ("approved", outcome)
                        } else if rejected {
                            ("rejected", None)
                        } else {
                            ("never_reviewed", None)
                        }
                    }
                };
                let address = rec
                    .to_grain_spec(LOOP_NS)
                    .ok()
                    .and_then(|spec| sub.address_of(&spec).ok().flatten());
                let f = ReplayFinding {
                    step_ms: t,
                    analyzer: rec.analyzer.clone(),
                    dedup_key: rec.dedup_key.clone(),
                    summary: rec.summary.render(),
                    severity: rec.severity.as_str().to_string(),
                    target_ref: rec.target_ref.clone(),
                    address,
                    recorded: recorded_as.into(),
                    outcome,
                };
                arm.total.add(&f);
                arm.per_analyzer.entry(rec.analyzer.clone()).or_default().add(&f);
                arm.findings.push(f);
            }
            prev = Some(t);
        }
        Ok(arm)
    }
}

/// Every audit transition on the trail, oldest first.
fn audit_events<S: SubstrateRead>(sub: &S) -> Result<Vec<AuditEvent>> {
    let obs = sub.grains_of_type(
        crate::model::grain_type::OBSERVATION,
        Some(LOOP_NS),
        ReadOpts { live_only: false, since_ms: None },
    )?;
    let mut out: Vec<AuditEvent> = obs
        .iter()
        .filter(|g| g.str_field("observation_kind") == Some("loop_audit"))
        .filter_map(|g| {
            Some(AuditEvent {
                at_ms: g.fields.get("at_ms").and_then(Value::as_i64).unwrap_or(g.created_at_ms),
                rec_hash: g.str_field("rec_hash")?.to_string(),
                to: g.str_field("to_status")?.to_string(),
                from: g.str_field("from_status").map(str::to_string),
            })
        })
        .collect();
    out.sort_by(|a, b| a.at_ms.cmp(&b.at_ms).then(a.rec_hash.cmp(&b.rec_hash)));
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::{Decision, ScopeSet};
    use crate::recommendation::ObserverType;
    use crate::testkit::TestSubstrate;
    use serde_json::json;

    const DAY: i64 = 86_400_000;

    fn seeded() -> TestSubstrate {
        let mut sub = TestSubstrate::new();
        for i in 0..5 {
            sub.add_tool_call_at("stripe_refund", true, "rate limited # retry later", 1_000 + i);
        }
        sub.add_fact_at("agent", "sam", "lives_in", "berlin", 2_000);
        sub.add_fact_at("agent", "sam", "lives_in", "tokyo", 2_001);
        sub
    }

    fn keys(findings: &[ReplayFinding]) -> BTreeSet<(String, String)> {
        findings.iter().map(|f| (f.analyzer.clone(), f.summary.clone())).collect()
    }

    fn per_pass(since: Option<i64>, now: i64) -> ReplayOptions {
        ReplayOptions { since_ms: since, until_ms: now, step: ReplayStep::PerPass, namespaces: vec![] }
    }

    /// Identity: the candidate equal to the current config, stepped through
    /// the recorded passes, reproduces the queue the live passes stored —
    /// same analyzers, same summaries, same count — and the incumbent row is
    /// always present.
    #[test]
    fn replaying_the_incumbent_reproduces_the_recorded_queue() {
        let mut sub = seeded();
        let e = Engine::with_builtins();
        e.run(&mut sub.inner, &RunOptions::default(), 10_000).unwrap();
        let live: BTreeSet<(String, String)> = e
            .recommendations(&sub.inner, None)
            .unwrap()
            .iter()
            .map(|r| (r.analyzer.clone(), r.summary.render()))
            .collect();
        assert!(!live.is_empty());
        let report = e.replay(&sub.inner, &ReplayCandidate::default(), &per_pass(None, 10_000)).unwrap();
        assert_eq!(report.steps, vec![10_000]);
        assert_eq!(keys(&report.incumbent.findings), live);
        assert_eq!(keys(&report.candidate.findings), live, "an empty overlay IS the incumbent");
        assert_eq!(report.incumbent.total.findings, live.len() as u64);
        assert_eq!(report.incumbent.queue_per_step, vec![live.len() as u64]);
        assert!(report.incumbent.findings.iter().all(|f| f.recorded == "never_reviewed"), "stored, still pending");
        assert!(report.not_replayed.is_empty(), "no model, no command: nothing to disclaim");
    }

    /// A parameter change moves the queue as designed, and the decision
    /// overlap moves with it.
    #[test]
    fn a_param_change_moves_the_queue_and_the_decision_overlap() {
        let mut sub = seeded();
        let e = Engine::with_builtins();
        e.run(&mut sub.inner, &RunOptions::default(), 10_000).unwrap();
        // Approve the tool-failure lesson, so the overlap has an approval.
        let rec = e
            .recommendations(&sub.inner, None)
            .unwrap()
            .into_iter()
            .find(|r| r.analyzer.starts_with("loop.tool_failure"))
            .unwrap();
        e.review(&mut sub.inner, &rec.hash, Decision::Approve, "user:a", ObserverType::Human, &ScopeSet::all(), "ok", 10_500)
            .unwrap();
        // The candidate raises the cluster threshold past the seeded five.
        let mut cand = ReplayCandidate::default();
        cand.config.insert(
            "loop.tool_failure/1".into(),
            AnalyzerConfig { params: json!({"min_count": 50}).as_object().unwrap().clone(), ..Default::default() },
        );
        let report = e.replay(&sub.inner, &cand, &per_pass(None, 11_000)).unwrap();
        assert_eq!(report.incumbent.total.approved, 1, "{:?}", report.incumbent.total);
        assert!(report.incumbent.findings.iter().any(|f| f.analyzer.starts_with("loop.tool_failure")));
        assert!(
            !report.candidate.findings.iter().any(|f| f.analyzer.starts_with("loop.tool_failure")),
            "the raised floor drops the cluster: {:?}",
            report.candidate.findings
        );
        assert_eq!(report.candidate.total.approved, 0);
        assert_eq!(report.candidate.total.findings + 1, report.incumbent.total.findings);
    }

    /// Zero writes: the grain count and the loop state are untouched, and the
    /// view refuses every write by type.
    #[test]
    fn replay_writes_nothing() {
        let mut sub = seeded();
        let e = Engine::with_builtins();
        e.run(&mut sub.inner, &RunOptions::default(), 10_000).unwrap();
        let grains_before = sub.inner.grains_of_type("recommendation", None, ReadOpts { live_only: false, since_ms: None }).unwrap().len()
            + sub.inner.grains_of_type("observation", None, ReadOpts { live_only: false, since_ms: None }).unwrap().len()
            + sub.inner.grains_of_type("fact", None, ReadOpts { live_only: false, since_ms: None }).unwrap().len();
        let state_before = sub.inner.load_state().unwrap();
        e.replay(&sub.inner, &ReplayCandidate::default(), &per_pass(None, 10_000)).unwrap();
        let grains_after = sub.inner.grains_of_type("recommendation", None, ReadOpts { live_only: false, since_ms: None }).unwrap().len()
            + sub.inner.grains_of_type("observation", None, ReadOpts { live_only: false, since_ms: None }).unwrap().len()
            + sub.inner.grains_of_type("fact", None, ReadOpts { live_only: false, since_ms: None }).unwrap().len();
        assert_eq!(grains_before, grains_after);
        assert_eq!(sub.inner.load_state().unwrap(), state_before);
        let mut view = PrefixView::new(&sub.inner, 10_000);
        assert!(view.put_grain(&GrainSpec::new("fact", "x")).is_err());
        assert!(view.store_state(&json!({})).is_err());
        assert!(view.execute_cal("ADD fact {}").is_err());
    }

    /// No future leakage: a grain dated after step t cannot contribute to a
    /// finding at t, and does at a later step.
    #[test]
    fn a_grain_after_the_step_is_invisible_at_that_step() {
        let mut sub = TestSubstrate::new();
        // The contradiction needs both values; the second lands after t1.
        sub.add_fact_at("agent", "sam", "lives_in", "berlin", 2_000);
        let e = Engine::with_builtins();
        e.run(&mut sub.inner, &RunOptions::default(), 10_000).unwrap(); // pass 1: nothing
        sub.add_fact_at("agent", "sam", "lives_in", "tokyo", 15_000);
        e.run(&mut sub.inner, &RunOptions::default(), 20_000).unwrap(); // pass 2: the contradiction
        // Per pass: the first pass stored nothing and left no trace, so the
        // only recorded step is the second — and it finds the pair.
        let report = e.replay(&sub.inner, &ReplayCandidate::default(), &per_pass(None, 20_000)).unwrap();
        assert_eq!(report.steps, vec![20_000]);
        assert_eq!(report.incumbent.queue_per_step, vec![1], "{:?}", report.incumbent.findings);
        assert_eq!(report.incumbent.findings[0].step_ms, 20_000);
        // A stride that steps at 12_000 sees berlin alone: still nothing —
        // tokyo (created 15_000) is in the future of that step.
        let opts = ReplayOptions { since_ms: Some(12_000), until_ms: 20_000, step: ReplayStep::Stride(8_000), namespaces: vec![] };
        let report = e.replay(&sub.inner, &ReplayCandidate::default(), &opts).unwrap();
        assert_eq!(report.steps, vec![12_000, 20_000]);
        assert_eq!(report.incumbent.queue_per_step, vec![0, 1]);
    }

    /// State fidelity: a finding rejected at t is on cooldown at t+1, so the
    /// rehearsal does not count it again; the watermark advances per step.
    #[test]
    fn a_recorded_rejection_puts_the_key_on_cooldown_for_the_next_step() {
        let mut sub = seeded();
        let e = Engine::with_builtins();
        e.run(&mut sub.inner, &RunOptions::default(), 10_000).unwrap();
        let rec = e
            .recommendations(&sub.inner, None)
            .unwrap()
            .into_iter()
            .find(|r| r.analyzer.starts_with("loop.contradiction_sweep"))
            .unwrap();
        e.review(&mut sub.inner, &rec.hash, Decision::Reject, "user:a", ObserverType::Human, &ScopeSet::all(), "no", 10_500)
            .unwrap();
        // A second live pass a day later: the rejection cools the finding down.
        e.run(&mut sub.inner, &RunOptions::default(), 10_000 + DAY).unwrap();
        // Replay through both passes: the contradiction is found once, at
        // the first step, and counted `rejected`.
        let opts = ReplayOptions { since_ms: Some(10_000), until_ms: 10_000 + DAY, step: ReplayStep::Stride(DAY), namespaces: vec![] };
        let report = e.replay(&sub.inner, &ReplayCandidate::default(), &opts).unwrap();
        let contradictions: Vec<&ReplayFinding> = report
            .incumbent
            .findings
            .iter()
            .filter(|f| f.analyzer.starts_with("loop.contradiction_sweep"))
            .collect();
        assert_eq!(contradictions.len(), 1, "{contradictions:?}");
        assert_eq!((contradictions[0].step_ms, contradictions[0].recorded.as_str()), (10_000, "rejected"));
        // And every other finding is counted once, not once per step: the
        // replayed queue dedups its own later steps.
        assert_eq!(report.incumbent.queue_per_step[1], 0, "{:?}", report.incumbent.findings);
    }

    struct Cmd;
    impl crate::analyzer::Analyzer for Cmd {
        fn manifest(&self) -> &crate::manifest::AnalyzerManifest {
            use std::sync::OnceLock;
            static M: OnceLock<crate::manifest::AnalyzerManifest> = OnceLock::new();
            M.get_or_init(|| crate::manifest::AnalyzerManifest {
                id: "acme.pii/1".into(),
                title: "PII".into(),
                description: "external".into(),
                tier: crate::manifest::Tier::T0,
                cadence: crate::manifest::CadenceClass::Fast,
                requires: vec![],
                target_classes: vec![crate::manifest::TargetClass::Memory],
                auto_apply: crate::manifest::AutoApplyClass::Never,
                trust_class: crate::manifest::TrustClass::Command,
                params: vec![],
                default_on: true,
            })
        }
        fn analyze(&self, _ctx: &crate::analyzer::AnalyzeCtx) -> Result<Vec<crate::recommendation::RecDraft>> {
            panic!("an out-of-process analyzer must never run in a replay")
        }
    }
    struct Llm;
    impl crate::llm::LlmBackend for Llm {
        fn model(&self) -> &str {
            "mock"
        }
        fn complete(&self, _r: &str) -> Result<String> {
            panic!("a model must never be called in a replay")
        }
    }

    /// Scope honesty: an attached model and a registered external analyzer
    /// are disclaimed, never run, and the deterministic rows are unaffected.
    #[test]
    fn a_model_and_an_external_analyzer_are_reported_not_replayed() {
        let mut sub = seeded();
        let plain = Engine::with_builtins();
        plain.run(&mut sub.inner, &RunOptions::default(), 10_000).unwrap();
        let baseline = plain.replay(&sub.inner, &ReplayCandidate::default(), &per_pass(None, 10_000)).unwrap();
        let mut e = Engine::with_builtins().with_llm(Box::new(Llm));
        e.register(Box::new(Cmd));
        let report = e.replay(&sub.inner, &ReplayCandidate::default(), &per_pass(None, 10_000)).unwrap();
        let whats: Vec<&str> = report.not_replayed.iter().map(|n| n.what.as_str()).collect();
        assert!(whats.iter().any(|w| w.starts_with("origin=llm")), "{whats:?}");
        assert!(whats.iter().any(|w| w.contains("acme.pii/1")), "{whats:?}");
        assert_eq!(report.incumbent.skipped.get("acme.pii/1").map(String::as_str), Some("not replayed: replay"));
        assert_eq!(keys(&report.incumbent.findings), keys(&baseline.incumbent.findings));
    }

    #[test]
    fn steps_and_windows_parse() {
        assert_eq!(ReplayStep::parse("per-pass"), Some(ReplayStep::PerPass));
        assert_eq!(ReplayStep::parse("1d"), Some(ReplayStep::Stride(DAY)));
        assert_eq!(ReplayStep::parse("12h"), Some(ReplayStep::Stride(12 * 3_600_000)));
        assert_eq!(ReplayStep::parse("0d"), None);
        assert_eq!(ReplayStep::parse("soon"), None);
        let o = ReplayOptions::from_args(Some("90d"), None, None, vec![], 100 * DAY).unwrap();
        assert_eq!((o.since_ms, o.step), (Some(10 * DAY), ReplayStep::PerPass));
        assert!(ReplayOptions::from_args(Some("90d"), Some(1), None, vec![], 0).is_err());
        assert!(ReplayOptions::from_args(None, None, Some("weekly"), vec![], 0).is_err());
        let r = ReplayRequest::from_json(r#"{"config": {"loop.staleness/1": {"enabled": false}}, "window": "7d"}"#).unwrap();
        let (c, o) = r.resolve(10 * DAY).unwrap();
        assert_eq!(c.config.get("loop.staleness/1").and_then(|c| c.enabled), Some(false));
        assert_eq!(o.since_ms, Some(3 * DAY));
        assert!(ReplayRequest::from_json(r#"{"analyzers": {}}"#).is_err(), "unknown keys are refused");
    }
}
