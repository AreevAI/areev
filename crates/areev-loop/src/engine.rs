//! The engine: the analyze → DISCOVER → ENRICH → validate/dedup → store
//! pipeline, the run-outcome contract, and the review/apply/rollback lifecycle
//! with the governance gates. The DETERMINISTIC output is a pure function of
//! (store state, params, now); the optional LLM stages (§9) only *add* cited
//! drafts (origin=llm, never auto-apply) and whitelisted guidance — with no
//! backend they are the identity, so the deterministic path is unchanged.
//! Auto-apply execution is gated behind a conservative shape check and stays
//! off by default.

use crate::analyzer::{AnalyzeCtx, Analyzer, OutcomeInput};
use crate::cal;
use crate::config::{AppliedRecord, LoopPersisted};
use crate::error::{Error, Result};
use crate::manifest::{AnalyzerManifest, Capability};
use crate::model::{normalize_ident, ActionKind, GrainRecord, Origin, Severity, TargetRef};
use crate::recommendation::{
    dedup_key, AuditRecord, ObserverType, Proposal, RecStatus, Recommendation, Summary,
    MAX_BECAUSE, MAX_EVIDENCE,
};
use crate::substrate::{Capabilities, OmsSubstrate, ReadOpts, SubstrateRead};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::{BTreeMap, BTreeSet};

/// The namespace the loop's own grains (recommendations, audit) live in.
pub const LOOP_NS: &str = "areev-loop";

/// Host-granted authority, per connection. `admin` implies all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    Read,
    Write,
    Review,
    Apply,
    Admin,
}

/// A set of granted scopes.
#[derive(Debug, Clone, Default)]
pub struct ScopeSet(Vec<Scope>);

impl ScopeSet {
    pub fn of(scopes: &[Scope]) -> Self {
        ScopeSet(scopes.to_vec())
    }
    /// The local root of trust: whoever can run against the file holds all
    /// scopes (the CLI/embedded posture).
    pub fn all() -> Self {
        ScopeSet(vec![Scope::Admin])
    }
    pub fn has(&self, s: Scope) -> bool {
        self.0.contains(&Scope::Admin) || self.0.contains(&s)
    }
}

/// A review decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Approve,
    Reject,
}

/// Gating and scoping options for a run.
#[derive(Debug, Clone, Default)]
pub struct RunOptions {
    pub min_new: Option<u64>,
    pub min_new_errors: Option<u64>,
    pub if_stale_ms: Option<i64>,
    /// Optional global namespace filter (empty = all).
    pub namespaces: Vec<String>,
    /// Re-analyze the WHOLE memory this pass, not just grains since the last-run
    /// watermark (`areev loop reflect`). Dedup/cooldowns still suppress anything
    /// already queued, and the watermark still advances at the end — so a sweep
    /// is safe to run any time and later runs stay incremental. Mainly widens the
    /// watermark-sensitive inputs (tool-failure window, the non-parasitic LLM
    /// evidence bundle) to the full history.
    pub full_sweep: bool,
    /// The principal that invoked this run. Recorded as co-creator on every
    /// non-`Builtin` (LLM / external-command) recommendation the run stores,
    /// so the review gate's self-approval block also fires for whoever
    /// triggered the model that authored the finding — an LLM draft is
    /// authored *via* its trigger, unlike a deterministic finding, which is
    /// computed. `None` (a headless/scheduled run) records no co-creator.
    pub triggering_actor: Option<String>,
}

/// Whether a run executed or was skipped.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RunOutcome {
    Ran,
    Skipped,
}

/// Why a run was a no-op. `LockHeld` is produced by the host adapter (a
/// concurrent writer), surfaced here for a single contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SkipReason {
    MinNewNotMet,
    NotStale,
    LockHeld,
}

/// One analyzer that did not contribute drafts, with why.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AnalyzerSkip {
    pub id: String,
    pub reason: String,
}

/// The run-outcome contract (proposal §13): one shape across CLI/API/MCP/bindings.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunResult {
    pub outcome: RunOutcome,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skip_reason: Option<SkipReason>,
    pub new_grains: u64,
    pub new_error_events: u64,
    pub proposed: u64,
    pub deduped: u64,
    pub stored: u64,
    /// Of the stored recommendations, how many were auto-applied by policy.
    #[serde(default)]
    pub auto_applied: u64,
    #[serde(default)]
    pub analyzers_run: Vec<String>,
    #[serde(default)]
    pub analyzers_skipped: Vec<AnalyzerSkip>,
}

impl RunResult {
    fn skipped(reason: SkipReason, new_grains: u64, new_error_events: u64) -> Self {
        RunResult {
            outcome: RunOutcome::Skipped,
            skip_reason: Some(reason),
            new_grains,
            new_error_events,
            proposed: 0,
            deduped: 0,
            stored: 0,
            auto_applied: 0,
            analyzers_run: vec![],
            analyzers_skipped: vec![],
        }
    }

    pub fn ran(&self) -> bool {
        self.outcome == RunOutcome::Ran
    }
}

/// The engine holds the registered analyzers, the host policy, and an optional
/// LLM enrichment backend (§9).
pub struct Engine {
    analyzers: Vec<Box<dyn Analyzer>>,
    policy: crate::policy::Policy,
    /// Optional LLM backend. `None` → the DISCOVER/ENRICH stages are the
    /// identity, so the pipeline is byte-for-byte the deterministic path.
    llm: Option<Box<dyn crate::llm::LlmBackend>>,
    /// Optional separate backend for the GROUND stage (§5.2, §11). `None` →
    /// grounding rides `llm`. Lets a team point entailment at a cheaper or
    /// specialized model (or take the generative model out of grounding
    /// entirely) without changing the proposer/verifier.
    ground_llm: Option<Box<dyn crate::llm::LlmBackend>>,
}

struct AnalysisPass {
    survivors: Vec<Recommendation>,
    proposed: u64,
    deduped: u64,
    analyzers_run: Vec<String>,
    analyzers_skipped: Vec<AnalyzerSkip>,
}

impl Engine {
    /// An engine with the default built-ins and a default (fully closed)
    /// policy — nothing auto-applies, no LLM.
    pub fn with_builtins() -> Self {
        Engine {
            analyzers: crate::analyzer::builtin_analyzers(),
            policy: crate::policy::Policy::default(),
            llm: None,
            ground_llm: None,
        }
    }

    /// An engine with no analyzers (register your own).
    pub fn empty() -> Self {
        Engine {
            analyzers: vec![],
            policy: crate::policy::Policy::default(),
            llm: None,
            ground_llm: None,
        }
    }

    /// Install a host policy (the only place auto-apply is granted).
    pub fn with_policy(mut self, policy: crate::policy::Policy) -> Self {
        self.policy = policy;
        self
    }

    /// Attach an optional LLM enrichment backend (§9). Only ever *adds* cited
    /// draft recommendations (stamped `origin = llm`, never auto-applied) and
    /// whitelisted guidance notes — it can never gate or rewrite deterministic
    /// output.
    pub fn with_llm(mut self, backend: Box<dyn crate::llm::LlmBackend>) -> Self {
        self.llm = Some(backend);
        self
    }

    /// Attach a separate backend for the GROUND stage (§5.2). Without this,
    /// grounding uses the `with_llm` backend. Independent of the proposer so an
    /// operator can run entailment on a cheaper/specialized model.
    pub fn with_ground_llm(mut self, backend: Box<dyn crate::llm::LlmBackend>) -> Self {
        self.ground_llm = Some(backend);
        self
    }

    pub fn policy(&self) -> &crate::policy::Policy {
        &self.policy
    }

    /// Register an additional analyzer (the linked-Rust seam).
    pub fn register(&mut self, analyzer: Box<dyn Analyzer>) {
        self.analyzers.push(analyzer);
    }

    pub fn analyzers(&self) -> &[Box<dyn Analyzer>] {
        &self.analyzers
    }

    /// Run the exact production analysis/validation path without Phase 0
    /// measurement or Phase 3 persistence. The immutable substrate borrow is
    /// the replay safety boundary: recommendations, audit grains, state,
    /// cooldowns, outcomes, and the op-log cannot be changed here.
    ///
    /// `overrides` is keyed by full analyzer id and overlays the file's stored
    /// parameter map. Unknown keys fail closed through `resolve_params` and
    /// surface as an analyzer skip, exactly as in a production run.
    pub fn analyze_only<S: OmsSubstrate>(
        &self,
        sub: &S,
        opts: &RunOptions,
        overrides: &BTreeMap<String, Map<String, Value>>,
        now_ms: i64,
    ) -> Result<Vec<Recommendation>> {
        let persisted = LoopPersisted::from_value(sub.load_state()?)?;
        let analysis_watermark = if opts.full_sweep {
            None
        } else {
            persisted.state.watermark_ms
        };
        Ok(self
            .analysis_pass(
                sub,
                &persisted,
                opts,
                overrides,
                analysis_watermark,
                now_ms,
                &[],
            )?
            .survivors)
    }

    /// Run one analysis pass. Idempotent under `dedup_key`; the watermark is
    /// advanced at the end, so a crashed run simply re-runs.
    pub fn run<S: OmsSubstrate>(
        &self,
        sub: &mut S,
        opts: &RunOptions,
        now_ms: i64,
    ) -> Result<RunResult> {
        let mut persisted = LoopPersisted::from_value(sub.load_state()?)?;
        let watermark = persisted.state.watermark_ms;
        // A full sweep analyzes the whole memory (watermark ignored for the
        // analysis inputs), while gating, `new` counts, and the end-of-run
        // watermark advance still use the real watermark. Dedup/cooldowns keep
        // it from re-proposing what is already queued.
        let analysis_watermark = if opts.full_sweep { None } else { watermark };

        let (new_grains, new_error_events) = count_new(sub, watermark)?;
        if let Some(reason) = gate(opts, &persisted, new_grains, new_error_events, now_ms) {
            return Ok(RunResult::skipped(reason, new_grains, new_error_events));
        }

        // Phase 0: re-measure applied recommendations due for review (the
        // Verify gate). Records a measured outcome per due recommendation.
        let outcome_inputs = measure_outcomes(sub, &mut persisted, now_ms)?;

        let AnalysisPass {
            survivors,
            proposed,
            deduped,
            analyzers_run,
            analyzers_skipped,
        } = self.analysis_pass(
            &*sub,
            &persisted,
            opts,
            &BTreeMap::new(),
            analysis_watermark,
            now_ms,
            &outcome_inputs,
        )?;

        // Phase 3 (needs &mut): store survivors + propose audit, then
        // auto-apply the ones the host policy grants (all gates in §6.3).
        let mut stored = 0u64;
        let mut auto_applied = 0u64;
        for mut rec in survivors {
            let spec = rec.to_grain_spec(LOOP_NS)?;
            let hash = sub.put_grain(&spec)?;
            rec.hash = hash.clone();
            let actor = format!("engine:{}", rec.analyzer);
            let audit = AuditRecord {
                rec_hash: hash.clone(),
                from: None,
                to: RecStatus::Pending,
                actor: actor.clone(),
                observer_type: ObserverType::System,
                because: "analyzer proposed".into(),
                previous_audit_hash: None,
                gating: None,
                at_ms: now_ms,
            };
            let audit_hash = sub.put_grain(&audit.to_grain_spec(LOOP_NS))?;
            persisted
                .status_index
                .insert(hash.clone(), RecStatus::Pending);
            persisted.creators.insert(hash.clone(), actor);
            // An LLM or external-command finding exists because someone ran
            // it — record that principal too, so review can refuse the
            // trigger approving their own model's output. Builtin analyzers
            // stay engine-only: deterministic output has no human author.
            if !matches!(rec.origin, Origin::Builtin) {
                if let Some(trigger) = &opts.triggering_actor {
                    persisted.co_creators.insert(hash.clone(), trigger.clone());
                }
            }
            persisted.audit_heads.insert(hash.clone(), audit_hash);
            stored += 1;

            if self.can_auto_apply(&*sub, &rec) {
                self.auto_apply(sub, &mut persisted, &rec, now_ms)?;
                auto_applied += 1;
            }
        }

        persisted.state.last_run_ms = Some(now_ms);
        persisted.state.watermark_ms = Some(now_ms);
        sub.store_state(&persisted.to_value()?)?;

        Ok(RunResult {
            outcome: RunOutcome::Ran,
            skip_reason: None,
            new_grains,
            new_error_events,
            proposed,
            deduped,
            stored,
            auto_applied,
            analyzers_run,
            analyzers_skipped,
        })
    }

    /// Shared Phase 1–2 implementation for production and replay. Cooldowns
    /// and the live recommendation queue are honored in both modes; only the
    /// production caller proceeds into the mutating Phase 3 below.
    #[allow(clippy::too_many_arguments)]
    fn analysis_pass<S: OmsSubstrate>(
        &self,
        sub: &S,
        persisted: &LoopPersisted,
        opts: &RunOptions,
        external_overrides: &BTreeMap<String, Map<String, Value>>,
        analysis_watermark: Option<i64>,
        now_ms: i64,
        outcome_inputs: &[OutcomeInput],
    ) -> Result<AnalysisPass> {
        let existing = existing_dedup_keys(sub, persisted)?;
        let mut analyzers_run = Vec::new();
        let mut analyzers_skipped = Vec::new();
        let mut candidates: Vec<Recommendation> = Vec::new();
        let caps = sub.capabilities();

        for analyzer in &self.analyzers {
            let m = analyzer.manifest();
            let cfg = persisted.config.get(&m.id);
            let enabled = cfg.and_then(|c| c.enabled).unwrap_or(m.default_on);
            if !enabled {
                analyzers_skipped.push(AnalyzerSkip {
                    id: m.id.clone(),
                    reason: "disabled".into(),
                });
                continue;
            }
            if self.policy.denies(m.family()) {
                analyzers_skipped.push(AnalyzerSkip {
                    id: m.id.clone(),
                    reason: "denied by host policy".into(),
                });
                continue;
            }
            if let Some(missing) = missing_capability(m, caps) {
                analyzers_skipped.push(AnalyzerSkip {
                    id: m.id.clone(),
                    reason: format!("missing capability: {missing}"),
                });
                continue;
            }
            let mut param_overrides = cfg.map(|c| c.params.clone()).unwrap_or_default();
            if let Some(extra) = external_overrides.get(&m.id) {
                for (key, value) in extra {
                    param_overrides.insert(key.clone(), value.clone());
                }
            }
            let params = match m.resolve_params(&param_overrides) {
                Ok(p) => p,
                Err(e) => {
                    analyzers_skipped.push(AnalyzerSkip {
                        id: m.id.clone(),
                        reason: e.to_string(),
                    });
                    continue;
                }
            };
            let ns_owned = cfg.map(|c| c.namespaces.clone()).unwrap_or_default();
            let ns_slice: &[String] = if ns_owned.is_empty() {
                &opts.namespaces
            } else {
                &ns_owned
            };
            let reader: &dyn SubstrateRead = sub;
            let ctx = AnalyzeCtx::new(
                reader,
                &params,
                ns_slice,
                analysis_watermark,
                now_ms,
                outcome_inputs,
            );
            match analyzer.analyze(&ctx) {
                Ok(drafts) => {
                    analyzers_run.push(m.id.clone());
                    for draft in drafts {
                        match stamp(m, &params, draft, now_ms) {
                            Ok(rec) => candidates.push(rec),
                            Err(e) => analyzers_skipped.push(AnalyzerSkip {
                                id: m.id.clone(),
                                reason: e.to_string(),
                            }),
                        }
                    }
                }
                Err(e) => analyzers_skipped.push(AnalyzerSkip {
                    id: m.id.clone(),
                    reason: e.to_string(),
                }),
            }
        }

        if self.llm.is_some() {
            candidates.extend(self.discover(
                sub,
                &candidates,
                analysis_watermark,
                &opts.namespaces,
                now_ms,
            ));
        }

        let proposed = candidates.len() as u64;
        let mut seen = BTreeSet::new();
        let mut survivors = Vec::new();
        for candidate in candidates {
            let family = crate::manifest::analyzer_family(&candidate.analyzer);
            let floor = [
                severity_floor_for(persisted, &candidate.analyzer),
                self.policy.severity_floor(family),
            ]
            .into_iter()
            .flatten()
            .max();
            if floor.is_some_and(|floor| candidate.severity < floor) {
                continue;
            }
            if !seen.insert(candidate.dedup_key.clone()) {
                continue;
            }
            if existing.contains(&candidate.dedup_key) {
                continue;
            }
            if persisted
                .cooldowns
                .get(&candidate.dedup_key)
                .is_some_and(|until| now_ms < *until)
            {
                continue;
            }
            survivors.push(candidate);
        }
        let deduped = proposed - survivors.len() as u64;
        if self.llm.is_some() {
            self.enrich(&mut survivors);
        }
        Ok(AnalysisPass {
            survivors,
            proposed,
            deduped,
            analyzers_run,
            analyzers_skipped,
        })
    }

    /// DISCOVER (§9): ask the LLM for additional draft recommendations, given
    /// the deterministic findings as *context* and a bounded, provenance-tagged
    /// evidence bundle. Every returned draft must cite evidence present in the
    /// bundle and target a memory/query surface; it is stamped `origin = llm`
    /// (so it can never auto-apply) and enters the ordinary dedup/store path. A
    /// failed or garbled response yields no drafts — never a failed run.
    fn discover<S: OmsSubstrate>(
        &self,
        sub: &S,
        candidates: &[Recommendation],
        watermark: Option<i64>,
        namespaces: &[String],
        now_ms: i64,
    ) -> Vec<Recommendation> {
        let Some(llm) = &self.llm else {
            return Vec::new();
        };
        let findings: Vec<crate::llm::FindingBrief> = candidates
            .iter()
            .take(32)
            .map(|c| crate::llm::FindingBrief {
                analyzer: c.analyzer.clone(),
                summary: c.summary.render(),
                target: c.target_ref.clone(),
                severity: c.severity.as_str().to_string(),
            })
            .collect();
        // Evidence bundle. Seeded first from the grains the deterministic
        // findings cite, THEN — the non-parasitic step (§11) — topped up with
        // RECENT grains (created since the last run) so the LLM gets its own
        // lens and can find issues in grains no analyzer flagged. Without this
        // the LLM could only elaborate near what determinism already caught.
        let mut evidence: Vec<crate::llm::EvidenceItem> = Vec::new();
        let mut bundle: BTreeSet<String> = BTreeSet::new();
        let mut ns_by_hash: std::collections::BTreeMap<String, String> = Default::default();
        'cited: for c in candidates {
            for h in &c.evidence {
                // Every source gets a RESERVED share of the bundle, because a
                // cap that only one source respects is not a budget. A single
                // tool_failure finding may cite up to MAX_EVIDENCE (64)
                // grains — the whole bundle — so without this the "give the
                // LLM its own lens" seeding below could be starved to nothing
                // by the very determinism it is supposed to look past. Found
                // live: with the deterministic findings citing enough, the
                // model never saw a single non-cited grain.
                if evidence.len() >= CITED_SEED_CAP {
                    break 'cited;
                }
                if !bundle.contains(h) {
                    if let Ok(Some(g)) = sub.grain(h) {
                        push_evidence(&mut evidence, &mut bundle, &mut ns_by_hash, &g);
                    }
                }
            }
        }
        let scan_ns: Vec<Option<&str>> = if namespaces.is_empty() {
            vec![None]
        } else {
            namespaces.iter().map(|n| Some(n.as_str())).collect()
        };
        let opts = ReadOpts { live_only: true, since_ms: watermark };
        // Tool grains carry the raw experience of a tool-using agent, and an
        // ERROR is the part a reflection pass can act on. Seeded after the
        // cited grains and inside its own reserved share, so a busy desk
        // cannot crowd out the facts and observations below.
        //
        // Without this the LLM saw tool failures only through the
        // deterministic findings that happened to cite them: it could
        // elaborate on what clustering already caught, but could never find
        // a failure clustering missed — the one thing it is here for. The
        // top-up below called itself "non-parasitic" while omitting the very
        // grain type the flagship analyzer reads.
        let mut tool_seeded = 0usize;
        'tools: for ns in &scan_ns {
            if let Ok(recent) = sub.grains_of_type(crate::model::grain_type::TOOL, *ns, opts) {
                for g in recent {
                    if tool_seeded >= TOOL_SEED_CAP
                        || evidence.len() >= EVIDENCE_CAP - LENS_RESERVE
                    {
                        break 'tools;
                    }
                    if !g.is_error() {
                        continue;
                    }
                    let before = evidence.len();
                    push_evidence(&mut evidence, &mut bundle, &mut ns_by_hash, &g);
                    if evidence.len() > before {
                        tool_seeded += 1;
                    }
                }
            }
        }
        'seed: for gt in [
            crate::model::grain_type::FACT,
            crate::model::grain_type::OBSERVATION,
        ] {
            for ns in &scan_ns {
                if let Ok(recent) = sub.grains_of_type(gt, *ns, opts) {
                    for g in recent {
                        if evidence.len() >= EVIDENCE_CAP {
                            break 'seed;
                        }
                        push_evidence(&mut evidence, &mut bundle, &mut ns_by_hash, &g);
                    }
                }
            }
        }
        if evidence.is_empty() {
            return Vec::new(); // nothing to reflect on
        }
        // PROPOSE (§5.1): the abstention-legitimate objective — "nothing to
        // report" is a first-class, zero-penalty answer. The operator-taste
        // history (recent approve/reject decisions on llm findings) is passed so
        // the model learns what this reviewer accepts.
        let (approved, rejected) = self.llm_history(sub);
        let request = crate::llm::LlmRequest {
            loop_proto: 1,
            op: "discover",
            instructions: DISCOVER_INSTRUCTIONS,
            findings: findings.clone(),
            evidence: evidence.clone(),
            rejected,
            approved,
        };
        let Ok(body) = serde_json::to_string(&request) else {
            return Vec::new();
        };
        let raw = match llm.complete(&body) {
            Ok(r) => r,
            Err(_) => return Vec::new(), // fail-soft
        };
        // Cheap structural validation (cite-check + target class); collect the
        // survivors for the verifier. Storing the normalized target string
        // avoids a TargetRef clone through the pipeline.
        let caps = sub.capabilities();
        let mut validated: Vec<ValidatedDraft> = Vec::new();
        for d in crate::llm::parse_discover(&raw)
            .recommendations
            .into_iter()
            .take(crate::llm::MAX_LLM_DRAFTS)
        {
            let cited: Vec<String> =
                d.evidence.iter().filter(|h| bundle.contains(*h)).cloned().collect();
            if cited.is_empty() {
                continue; // uncited → drop (no fabrication)
            }
            let Ok(target) = TargetRef::parse(&d.target) else {
                continue;
            };
            let tc = target.target_class();
            // The classes the proposal vocabulary can reach: memory
            // (entity/grain), query (query/template) and code (tool). The
            // prompt (`doc:`), `host:`, `evalset:` and `model:` classes stay
            // closed to the model — it may not rewrite the agent's prompt, its
            // host config, or the gate that grades its own code.
            if !matches!(tc, "memory" | "query" | "code") {
                continue;
            }
            // Resolve BEFORE the gates: what GROUND entails and VERIFY
            // stress-tests is exactly what an apply would do.
            let resolved = resolve_proposal(sub, &d, &target, &cited, &ns_by_hash, caps);
            // A `tool:` target has exactly one legal shape (Rule E1: a code
            // target REQUIRES action_kind code_revision), so an unresolved one
            // could not even be stamped advisory — drop it here rather than
            // spend two model calls on something that fails validation after.
            if tc == "code" && resolved.is_none() {
                continue;
            }
            validated.push(ValidatedDraft {
                draft: d,
                target_ref: target.as_string(),
                cited,
                resolved,
            });
        }
        if validated.is_empty() {
            return Vec::new();
        }
        // GROUND → VERIFY → ROUTE (§5.2–5.4): only drafts that survive an
        // independent grounding entailment check *and* an adversarial
        // verification pass (each a separate call — proposer ≠ scorer) reach the
        // queue, stamped with the verifier's calibrated confidence.
        // GROUND may run on a separate backend (§11); VERIFY always uses the
        // main llm (the proposer≠scorer independence is on VERIFY, not GROUND).
        let ground = self.ground_llm.as_deref().unwrap_or(&**llm);
        self.verify_drafts(&**llm, ground, validated, &evidence, now_ms)
    }

    /// GROUND → VERIFY → ROUTE (§5.2–5.4). Two independent model calls, batched
    /// over the drafts: a grounding-entailment gate ("does the cited evidence
    /// support the claim?"), then an adversarial keep/kill with a calibrated
    /// confidence. A draft reaches the queue only if it is grounded **and** kept
    /// **and** clears the confidence floor. Any failed call drops the whole LLM
    /// contribution for the run (safe default), never the run.
    fn verify_drafts(
        &self,
        llm: &dyn crate::llm::LlmBackend,
        ground: &dyn crate::llm::LlmBackend,
        validated: Vec<ValidatedDraft>,
        evidence: &[crate::llm::EvidenceItem],
        now_ms: i64,
    ) -> Vec<Recommendation> {
        use crate::llm::*;
        let ev_by_hash: std::collections::BTreeMap<&str, &EvidenceItem> =
            evidence.iter().map(|e| (e.hash.as_str(), e)).collect();
        let ev_for = |cited: &[String]| -> Vec<EvidenceItem> {
            cited
                .iter()
                .filter_map(|h| ev_by_hash.get(h.as_str()).map(|e| (*e).clone()))
                .collect()
        };

        // GROUND (§5.2): decompose-then-entail per draft, batched into one call.
        // The claim includes any authored lesson — the gate must judge exactly
        // what an apply would record, not only the finding's summary.
        let claims: Vec<GroundItem> = validated
            .iter()
            .enumerate()
            .map(|(i, v)| GroundItem {
                id: i,
                claim: claim_text(&v.draft, v.resolved.as_ref()),
                evidence: ev_for(&v.cited),
            })
            .collect();
        let ground_req = GroundRequest {
            loop_proto: 1,
            op: "ground",
            instructions: GROUND_INSTRUCTIONS,
            claims,
        };
        let grounded: std::collections::BTreeSet<usize> = match serde_json::to_string(&ground_req)
            .ok()
            .and_then(|b| ground.complete(&b).ok())
        {
            Some(raw) => parse_ground(&raw)
                .results
                .into_iter()
                .filter(|r| r.supported)
                .map(|r| r.id)
                .collect(),
            None => return Vec::new(),
        };
        if grounded.is_empty() {
            return Vec::new();
        }

        // VERIFY (§5.3): adversarial keep/kill over the grounded drafts, a
        // separate call from the proposer. Soundness + abstention only — NOT
        // novelty. Novelty is steered at DISCOVER and settled by human review;
        // asking a weak verifier to judge it just makes it hallucinate "already
        // known" and kill genuine findings (§11).
        let items: Vec<VerifyItem> = validated
            .iter()
            .enumerate()
            .filter(|(i, _)| grounded.contains(i))
            .map(|(i, v)| VerifyItem {
                id: i,
                // Same rule as GROUND: the adversarial pass sees the change.
                summary: claim_text(&v.draft, v.resolved.as_ref()),
                target: v.target_ref.clone(),
                evidence: ev_for(&v.cited),
            })
            .collect();
        let verify_req = VerifyRequest {
            loop_proto: 1,
            op: "verify",
            instructions: VERIFY_INSTRUCTIONS,
            findings: items,
        };
        let verdicts: std::collections::BTreeMap<usize, f64> =
            match serde_json::to_string(&verify_req).ok().and_then(|b| llm.complete(&b).ok()) {
                Some(raw) => parse_verify(&raw)
                    .results
                    .into_iter()
                    .filter(|r| r.keep)
                    .map(|r| (r.id, r.confidence.clamp(0.0, 1.0)))
                    .collect(),
                None => return Vec::new(),
            };

        // ROUTE (§5.4): grounded ∧ kept ∧ verifier-confidence ≥ floor. The
        // verifier's confidence (the independent signal) is what we trust and
        // stamp — not the proposer's self-report.
        let mut out = Vec::new();
        for (i, v) in validated.into_iter().enumerate() {
            if let Some(&conf) = verdicts.get(&i) {
                if conf >= MIN_LLM_CONFIDENCE {
                    out.push(stamp_llm(
                        llm.model(),
                        &v.draft,
                        v.target_ref,
                        v.cited,
                        v.resolved,
                        conf,
                        now_ms,
                    ));
                }
            }
        }
        out
    }

    /// Recent operator decisions on `origin = llm` findings — approved (incl.
    /// applied) and rejected summaries, most-recent first and bounded — so
    /// DISCOVER can learn what this reviewer accepts (§9). Best-effort: a read
    /// failure yields empty history, never an error.
    fn llm_history<S: OmsSubstrate>(&self, sub: &S) -> (Vec<String>, Vec<String>) {
        const MAX: usize = 20;
        let Ok(mut recs) = self.recommendations(sub, None) else {
            return (Vec::new(), Vec::new());
        };
        recs.retain(|r| matches!(r.origin, Origin::Llm { .. }));
        recs.sort_by_key(|r| std::cmp::Reverse(r.created_at_ms));
        let mut approved = Vec::new();
        let mut rejected = Vec::new();
        for r in &recs {
            match r.status {
                RecStatus::Approved | RecStatus::Applied | RecStatus::RolledBack
                    if approved.len() < MAX =>
                {
                    approved.push(r.summary.render());
                }
                RecStatus::Rejected if rejected.len() < MAX => rejected.push(r.summary.render()),
                _ => {}
            }
        }
        (approved, rejected)
    }

    /// ENRICH (§9): ask the LLM to add a short guidance note to the surviving
    /// deterministic recommendations. Whitelist-only — only `guidance` is
    /// merged (capped), and only onto recs that don't already have one; the
    /// engine-templated summary is never touched. Fail-soft.
    fn enrich(&self, survivors: &mut [Recommendation]) {
        let Some(llm) = &self.llm else {
            return;
        };
        if survivors.is_empty() {
            return;
        }
        let findings: Vec<crate::llm::FindingBrief> = survivors
            .iter()
            .map(|r| crate::llm::FindingBrief {
                analyzer: r.analyzer.clone(),
                summary: r.summary.render(),
                target: r.target_ref.clone(),
                severity: r.severity.as_str().to_string(),
            })
            .collect();
        let request = crate::llm::LlmRequest {
            loop_proto: 1,
            op: "enrich",
            instructions: ENRICH_INSTRUCTIONS,
            findings,
            evidence: Vec::new(),
            rejected: Vec::new(),
            approved: Vec::new(),
        };
        let Ok(body) = serde_json::to_string(&request) else {
            return;
        };
        let raw = match llm.complete(&body) {
            Ok(r) => r,
            Err(_) => return,
        };
        for note in crate::llm::parse_enrich(&raw).notes {
            if note.guidance.trim().is_empty() {
                continue;
            }
            if let Some(r) = survivors
                .iter_mut()
                .find(|r| r.target_ref == note.target && r.guidance.is_none())
            {
                r.guidance = Some(crate::llm::cap(&note.guidance, crate::llm::MAX_GUIDANCE_LEN));
            }
        }
    }

    /// Evaluate the auto-apply gate (§6.3) — ALL preconditions must hold:
    /// host opt-in + policy grant, builtin origin, memory/query target,
    /// non-destructive, and engine-side shape verification: SUPERSEDE-only
    /// structural curation (never an ADD that introduces evidence-derived
    /// text) whose every replacement is **value-identical** to the grain it
    /// supersedes (the exact-equality check — a near-duplicate consolidation
    /// stays pending). A default (closed) policy never grants, so nothing
    /// auto-applies.
    fn can_auto_apply<S: OmsSubstrate>(&self, sub: &S, rec: &Recommendation) -> bool {
        if !rec.origin.auto_apply_eligible() || rec.destructive {
            return false;
        }
        // The analyzer must declare its curation auto-appliable. An analyzer
        // whose manifest is `Never` (e.g. fork surfacing — a lossy merge) is
        // never auto-applied even if the payload passes the shape check.
        let manifest_ok = self
            .analyzers
            .iter()
            .map(|a| a.manifest())
            .find(|m| m.id == rec.analyzer)
            .is_some_and(|m| m.auto_apply == crate::manifest::AutoApplyClass::StructuralCuration);
        if !manifest_ok {
            return false;
        }
        let Ok(target) = TargetRef::parse(&rec.target_ref) else {
            return false;
        };
        let family = crate::manifest::analyzer_family(&rec.analyzer);
        if !self.policy.grants_auto_apply(family, target.target_class(), rec.severity) {
            return false;
        }
        // Shape verification: only a CAL batch of pure SUPERSEDE statements
        // whose replacements change no value is structural curation. An ADD
        // (introducing content), a FORGET (destructive), or a supersession
        // that alters any field disqualifies.
        match &rec.proposal {
            Proposal::Cal { cal } => cal
                .lines()
                .map(str::trim)
                .filter(|l| !l.is_empty())
                .all(|l| supersede_is_value_identical(sub, l)),
            _ => false,
        }
    }

    /// Apply a recommendation as `policy:auto` (the only `pending → applied`
    /// path). Records the applied inverse + a hash-chained audit grain.
    fn auto_apply<S: OmsSubstrate>(
        &self,
        sub: &mut S,
        p: &mut LoopPersisted,
        rec: &Recommendation,
        now_ms: i64,
    ) -> Result<()> {
        let mut created = Vec::new();
        if let Proposal::Cal { cal } = &rec.proposal {
            // Belt and braces over the policy: `grants_auto_apply` already
            // excludes the `query` class, so a definition rewrite cannot reach
            // this path. If one ever did, it would apply with no recorded
            // inverse and no human BECAUSE — refuse instead.
            if cal.lines().map(str::trim).any(is_definition_statement) {
                return Err(Error::InvalidProposal(
                    "a definition rewrite (DEFINE QUERY / DEFINE TEMPLATE) is never \
                     auto-applied: it changes what every future context contains, so it \
                     requires a human APPROVE + APPLY with BECAUSE"
                        .into(),
                ));
            }
            for r in sub.execute_cal(cal)? {
                if let Some(h) = r.get("hash").and_then(Value::as_str) {
                    created.push(h.to_string());
                }
            }
        }
        let applied = AppliedRecord {
            applied_at_ms: now_ms,
            target_ref: rec.target_ref.clone(),
            rollbackable: rec.rollbackable,
            created_hashes: created,
            inverse_cal: None,
            metric: rec.metric.clone(),
        };
        let prev = p.audit_heads.get(&rec.hash).cloned();
        let audit = AuditRecord {
            rec_hash: rec.hash.clone(),
            from: Some(RecStatus::Pending),
            to: RecStatus::Applied,
            actor: "policy:auto".into(),
            observer_type: ObserverType::Policy,
            because: "auto-applied per host policy".into(),
            previous_audit_hash: prev,
            gating: None,
            at_ms: now_ms,
        };
        let audit_hash = sub.put_grain(&audit.to_grain_spec(LOOP_NS))?;
        p.audit_heads.insert(rec.hash.clone(), audit_hash);
        p.status_index.insert(rec.hash.clone(), RecStatus::Applied);
        p.applied.insert(rec.hash.clone(), applied);
        Ok(())
    }

    /// Approve or reject a pending recommendation. Requires the `review` scope,
    /// a mandatory BECAUSE, and blocks self-approval against the creating actor.
    #[allow(clippy::too_many_arguments)]
    pub fn review<S: OmsSubstrate>(
        &self,
        sub: &mut S,
        rec_hash: &str,
        decision: Decision,
        actor: &str,
        observer: ObserverType,
        scopes: &ScopeSet,
        because: &str,
        now_ms: i64,
    ) -> Result<()> {
        if !scopes.has(Scope::Review) {
            return Err(Error::ScopeDenied("review".into()));
        }
        let because = validate_because(because)?;
        let mut p = LoopPersisted::from_value(sub.load_state()?)?;
        let status = *p
            .status_index
            .get(rec_hash)
            .ok_or_else(|| Error::NotFound(rec_hash.into()))?;
        let to = match decision {
            Decision::Approve => RecStatus::Approved,
            Decision::Reject => RecStatus::Rejected,
        };
        if !status.can_transition_to(to, false) {
            return Err(Error::LifecycleViolation(format!(
                "{} -> {}",
                status.as_str(),
                to.as_str()
            )));
        }
        if to == RecStatus::Approved {
            if let Some(creator) = p.creators.get(rec_hash) {
                if creator == actor {
                    return Err(Error::SelfApproval(format!(
                        "{actor} created this recommendation"
                    )));
                }
            }
            if let Some(trigger) = p.co_creators.get(rec_hash) {
                if trigger == actor {
                    return Err(Error::SelfApproval(format!(
                        "{actor} triggered the run that authored this recommendation"
                    )));
                }
            }
        }
        let prev = p.audit_heads.get(rec_hash).cloned();
        let audit = AuditRecord {
            rec_hash: rec_hash.into(),
            from: Some(status),
            to,
            actor: actor.into(),
            observer_type: observer,
            because,
            previous_audit_hash: prev,
            gating: None,
            at_ms: now_ms,
        };
        let audit_hash = sub.put_grain(&audit.to_grain_spec(LOOP_NS))?;
        p.audit_heads.insert(rec_hash.into(), audit_hash);
        p.status_index.insert(rec_hash.into(), to);
        if to == RecStatus::Rejected {
            if let Ok(rec) = load_rec(sub, rec_hash) {
                // Exponential backoff keyed on dedup_key: 7d, 14d, 28d, … capped
                // at 90d, so a finding a reviewer keeps rejecting stops
                // re-surfacing on a fixed 7d cadence (was a flat 7d despite the
                // "doubling" comment).
                const BASE_MS: i64 = 7 * 86_400_000;
                const CAP_MS: i64 = 90 * 86_400_000;
                let strikes = p.cooldown_strikes.entry(rec.dedup_key.clone()).or_insert(0);
                let interval = BASE_MS.saturating_mul(1_i64 << (*strikes).min(31)).min(CAP_MS);
                *strikes = strikes.saturating_add(1);
                p.cooldowns.insert(rec.dedup_key, now_ms + interval);
            }
        }
        sub.store_state(&p.to_value()?)?;
        Ok(())
    }

    /// Check everything [`apply`](Self::apply) would refuse on, without writing
    /// anything.
    ///
    /// This exists for the fused approve-and-apply callers (the bindings'
    /// `apply_recommendation`). Recording the approval first and *then* hitting
    /// the destructive gate strands the recommendation in `approved`, which has
    /// no exit but `applied` or `expired` — `approved → rejected` is not a
    /// legal transition — so a refused apply left the reviewer unable to
    /// dismiss it. Ask first, then approve.
    ///
    /// Deliberately does not check the lifecycle transition: the caller is
    /// about to make it legal by approving.
    /// `has_gating` is whether the caller will supply a gating run at apply:
    /// a gated revision (code or adapter) without one is refused HERE, before
    /// a fused approve-and-apply records the approval — `approved` has no
    /// exit but `applied` or `expired`, so asking after would strand it.
    pub fn preflight_apply<S: OmsSubstrate>(
        &self,
        sub: &S,
        rec_hash: &str,
        scopes: &ScopeSet,
        allow_destructive: bool,
        has_gating: bool,
    ) -> Result<()> {
        if !scopes.has(Scope::Apply) {
            return Err(Error::ScopeDenied("apply".into()));
        }
        let rec = load_rec(sub, rec_hash)?;
        if rec.destructive && (!scopes.has(Scope::Admin) || !allow_destructive) {
            return Err(Error::DestructiveGated(
                "destructive apply requires admin scope + allow_destructive".into(),
            ));
        }
        ensure_executable(rec.action_kind, &rec.proposal)?;
        if requires_gating(rec.action_kind) && !has_gating {
            return Err(Error::InvalidProposal(GATING_REQUIRED.into()));
        }
        Ok(())
    }

    /// Apply an approved recommendation. Requires `apply`; destructive payloads
    /// additionally require `admin` + `allow_destructive`. Records the applied
    /// info (inverse plan) for rollback.
    #[allow(clippy::too_many_arguments)]
    pub fn apply<S: OmsSubstrate>(
        &self,
        sub: &mut S,
        rec_hash: &str,
        actor: &str,
        observer: ObserverType,
        scopes: &ScopeSet,
        because: &str,
        allow_destructive: bool,
        now_ms: i64,
    ) -> Result<AppliedRecord> {
        self.apply_inner(
            sub, rec_hash, actor, observer, scopes, because, allow_destructive, None, now_ms,
        )
    }

    /// Load the gating evidence for `rec_hash` from the RECORDED
    /// `mg:eval_run` summary named by `run_id` — the one loader every
    /// surface (CLI, bindings, MCP, HTTP) shares, so the stats that admit a
    /// gated revision can never come from a caller: they are read back from
    /// the Fact `areev eval run` journaled, within the recommendation's own
    /// pinned evalset (a run id from a different evalset simply isn't found).
    pub fn gating_evidence<S: OmsSubstrate>(
        &self,
        sub: &S,
        rec_hash: &str,
        run_id: &str,
    ) -> Result<crate::recommendation::GatingEvidence> {
        let rec = self
            .recommendations(sub, None)?
            .into_iter()
            .find(|r| r.hash == rec_hash)
            .ok_or_else(|| {
                Error::InvalidProposal(format!("recommendation {rec_hash} not found"))
            })?;
        let pin = rec.evalset_hash.ok_or_else(|| {
            Error::InvalidProposal(
                "this recommendation pins no evalset — a gating run applies only \
                 to code and adapter revisions"
                    .into(),
            )
        })?;
        // Read through the shared evalset reader, so the run that GATES an
        // apply and the runs that later JUDGE it are parsed by exactly one
        // piece of code — two parsers drifting apart would let a rule be
        // admitted on one reading of a summary and measured on another.
        match crate::eval::eval_run_by_id(sub, &pin, run_id)? {
            Some(run) => Ok(crate::recommendation::GatingEvidence {
                evalset_hash: pin,
                run_id: run.run_id,
                passed: run.passed,
                failed: run.failed,
            }),
            None => Err(Error::InvalidProposal(format!(
                "no recorded gate run '{run_id}' for evalset {pin} — run \
                 `areev eval run --evalset {pin} ...` first"
            ))),
        }
    }

    /// Apply WITH the §7.4 evalset-run edge — the only path that can apply a
    /// gated (code or adapter) revision. The evidence is validated against
    /// the recommendation's pin and recorded on the audit Observation.
    #[allow(clippy::too_many_arguments)]
    pub fn apply_gated<S: OmsSubstrate>(
        &self,
        sub: &mut S,
        rec_hash: &str,
        actor: &str,
        observer: ObserverType,
        scopes: &ScopeSet,
        because: &str,
        allow_destructive: bool,
        gating: &crate::recommendation::GatingEvidence,
        now_ms: i64,
    ) -> Result<AppliedRecord> {
        self.apply_inner(
            sub,
            rec_hash,
            actor,
            observer,
            scopes,
            because,
            allow_destructive,
            Some(gating),
            now_ms,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn apply_inner<S: OmsSubstrate>(
        &self,
        sub: &mut S,
        rec_hash: &str,
        actor: &str,
        observer: ObserverType,
        scopes: &ScopeSet,
        because: &str,
        allow_destructive: bool,
        gating: Option<&crate::recommendation::GatingEvidence>,
        now_ms: i64,
    ) -> Result<AppliedRecord> {
        if !scopes.has(Scope::Apply) {
            return Err(Error::ScopeDenied("apply".into()));
        }
        let because = validate_because(because)?;
        let mut p = LoopPersisted::from_value(sub.load_state()?)?;
        let status = *p
            .status_index
            .get(rec_hash)
            .ok_or_else(|| Error::NotFound(rec_hash.into()))?;
        if !status.can_transition_to(RecStatus::Applied, false) {
            return Err(Error::LifecycleViolation(format!(
                "{} -> applied (approve first)",
                status.as_str()
            )));
        }
        let rec = load_rec(sub, rec_hash)?;
        if rec.destructive && (!scopes.has(Scope::Admin) || !allow_destructive) {
            return Err(Error::DestructiveGated(
                "destructive apply requires admin scope + allow_destructive".into(),
            ));
        }
        // §7.4: a code or adapter revision applies ONLY through the
        // evalset-run edge — the pin must match, the pinned evalset must
        // still be LIVE (a superseded evalset invalidates in-flight
        // recommendations: they re-gate), and a failing gate admits nothing.
        if requires_gating(rec.action_kind) {
            let g = gating
                .ok_or_else(|| Error::InvalidProposal(GATING_REQUIRED.into()))?;
            let pin = rec.evalset_hash.as_deref().unwrap_or_default();
            if g.evalset_hash != pin {
                return Err(Error::InvalidProposal(format!(
                    "gating ran evalset {} but the recommendation is pinned \
                     to {pin} (Rule E1)",
                    g.evalset_hash
                )));
            }
            match sub.grain(pin)? {
                Some(evalset) if evalset.is_live() => {}
                Some(_) => {
                    return Err(Error::InvalidProposal(
                        "the pinned evalset was superseded after gating — \
                         the recommendation must re-gate (Rule E1)"
                            .into(),
                    ))
                }
                None => {
                    return Err(Error::InvalidProposal(format!(
                        "pinned evalset {pin} not found in the substrate"
                    )))
                }
            }
            if g.failed > 0 {
                return Err(Error::InvalidProposal(format!(
                    "the gating run failed {}/{} cases — a failing gate \
                     admits nothing",
                    g.failed,
                    g.passed + g.failed
                )));
            }
        }

        // Execute the proposal.
        let mut created = Vec::new();
        // The inverse of a change that creates no grain — see
        // `AppliedRecord::inverse_cal`. Captured BEFORE execution, because
        // afterwards the previous definition is gone.
        let mut inverse_cal: Option<String> = None;
        match &rec.proposal {
            Proposal::Cal { cal } => {
                for line in cal.lines().map(str::trim).filter(|l| !l.is_empty()) {
                    if !is_definition_statement(line) {
                        continue;
                    }
                    match sub.definition_inverse(line)? {
                        Some(inv) => inverse_cal = Some(inv),
                        None => {
                            return Err(Error::InvalidProposal(format!(
                                "this substrate cannot record a rollback inverse for {line:?}; \
                                 a definition rewrite that ROLLBACK could not undo is refused \
                                 rather than applied"
                            )))
                        }
                    }
                }
                let rows = sub.execute_cal(cal)?;
                for r in rows {
                    if let Some(h) = r.get("hash").and_then(Value::as_str) {
                        created.push(h.to_string());
                    }
                }
            }
            // The engine has no executable Edit primitive. Marking this
            // Applied used to be a lie (and rollback had no inverse).
            Proposal::Edit { .. } => {
                return Err(Error::InvalidProposal(ADVISORY_EDIT.into()));
            }
            // A gated code or adapter revision EXECUTES by writing the
            // promotion grain: an immutable record that this target now
            // resolves to the proposed code (a §7.4 blob address) or adapter
            // (the tuning seam's registry tuple). Hosts read the promotion
            // to re-resolve — `(tool:X, mg:code_promotion)` or
            // `(model:X, mg:adapter_promotion)`; retracting it is the
            // rollback inverse, so the apply is rollbackable end-to-end.
            Proposal::Data { data } if requires_gating(rec.action_kind) => {
                let relation = if rec.action_kind == ActionKind::AdapterRevision {
                    "mg:adapter_promotion"
                } else {
                    "mg:code_promotion"
                };
                // A code revision authored by DISCOVER carries its SOURCE
                // inline, because the discovery pass reads the substrate and
                // cannot write to it. Move it into the CAS here so the
                // promotion names a content ADDRESS: §7.4's rule that code
                // enters the substrate only through the blob seam holds
                // however the revision was authored, and the promotion grain
                // stays a pointer rather than swelling to hold a program.
                let mut promoted = data.clone();
                if let Some(Value::String(src)) = promoted.remove("source") {
                    let address = sub.put_blob(src.as_bytes())?;
                    promoted.insert("code_address".into(), Value::from(address));
                }
                let mut spec = crate::substrate::GrainSpec::new(
                    crate::model::grain_type::FACT,
                    LOOP_NS,
                )
                .with_field("subject", rec.target_ref.clone())
                .with_field("relation", relation)
                .with_field(
                    "object",
                    serde_json::to_string(&promoted)
                        .map_err(|e| Error::Internal(format!("encode promotion: {e}")))?,
                )
                .with_field("rec_hash", rec_hash.to_string());
                if let Some(g) = gating {
                    spec = spec
                        .with_field("gating_evalset", g.evalset_hash.clone())
                        .with_field("gating_run_id", g.run_id.clone());
                }
                created.push(sub.put_grain(&spec)?);
            }
            Proposal::Data { data } => {
                // OutcomeReview is the one executable Data shape: its
                // `revert_of` points at an earlier applied recommendation.
                // Reuse the ordinary rollback path so the created hashes are
                // really retracted and the original lifecycle/audit advances.
                let revert_of = data
                    .get("revert_of")
                    .and_then(Value::as_str)
                    .ok_or_else(|| Error::InvalidProposal(ADVISORY_DATA.into()))?;
                self.rollback(
                    sub,
                    revert_of,
                    actor,
                    observer,
                    scopes,
                    &because,
                    now_ms,
                )?;
                // rollback stored a newer lifecycle state; merge this apply
                // into that state rather than overwriting the rollback.
                p = LoopPersisted::from_value(sub.load_state()?)?;
            }
        }

        let applied = AppliedRecord {
            applied_at_ms: now_ms,
            target_ref: rec.target_ref.clone(),
            rollbackable: rec.rollbackable,
            created_hashes: created,
            inverse_cal,
            metric: rec.metric.clone(),
        };
        let prev = p.audit_heads.get(rec_hash).cloned();
        let audit = AuditRecord {
            rec_hash: rec_hash.into(),
            from: Some(status),
            to: RecStatus::Applied,
            actor: actor.into(),
            observer_type: observer,
            because,
            previous_audit_hash: prev,
            gating: gating.cloned(),
            at_ms: now_ms,
        };
        let audit_hash = sub.put_grain(&audit.to_grain_spec(LOOP_NS))?;
        p.audit_heads.insert(rec_hash.into(), audit_hash);
        p.status_index.insert(rec_hash.into(), RecStatus::Applied);
        p.applied.insert(rec_hash.into(), applied.clone());
        sub.store_state(&p.to_value()?)?;
        Ok(applied)
    }

    /// Roll back an applied recommendation by retracting the grains it created.
    /// Fails for non-rollbackable applies (e.g. FORGET).
    #[allow(clippy::too_many_arguments)]
    pub fn rollback<S: OmsSubstrate>(
        &self,
        sub: &mut S,
        rec_hash: &str,
        actor: &str,
        observer: ObserverType,
        scopes: &ScopeSet,
        because: &str,
        now_ms: i64,
    ) -> Result<()> {
        if !scopes.has(Scope::Apply) {
            return Err(Error::ScopeDenied("apply".into()));
        }
        let because = validate_because(because)?;
        let mut p = LoopPersisted::from_value(sub.load_state()?)?;
        let status = *p
            .status_index
            .get(rec_hash)
            .ok_or_else(|| Error::NotFound(rec_hash.into()))?;
        if !status.can_transition_to(RecStatus::RolledBack, false) {
            return Err(Error::LifecycleViolation(format!(
                "{} -> rolled_back",
                status.as_str()
            )));
        }
        let applied = p
            .applied
            .get(rec_hash)
            .cloned()
            .ok_or_else(|| Error::NotFound(format!("no applied record for {rec_hash}")))?;
        if !applied.rollbackable {
            return Err(Error::LifecycleViolation(
                "recommendation is non-rollbackable (FORGET has no inverse)".into(),
            ));
        }
        for h in &applied.created_hashes {
            sub.retract(h, &format!("rollback of {rec_hash}"))?;
        }
        // A definition rewrite creates no grain, so retracting `created_hashes`
        // undoes nothing. Restoring it means re-running the statement captured
        // at apply time — the previous definition, or a DROP when there was
        // none. Runs BEFORE the audit is written, so a failed restore leaves
        // the recommendation `applied` (still true) rather than recording a
        // rollback that did not happen.
        if let Some(inverse) = &applied.inverse_cal {
            sub.execute_cal(inverse)?;
        }
        let prev = p.audit_heads.get(rec_hash).cloned();
        let audit = AuditRecord {
            rec_hash: rec_hash.into(),
            from: Some(status),
            to: RecStatus::RolledBack,
            actor: actor.into(),
            observer_type: observer,
            because,
            previous_audit_hash: prev,
            gating: None,
            at_ms: now_ms,
        };
        let audit_hash = sub.put_grain(&audit.to_grain_spec(LOOP_NS))?;
        p.audit_heads.insert(rec_hash.into(), audit_hash);
        p.status_index
            .insert(rec_hash.into(), RecStatus::RolledBack);
        sub.store_state(&p.to_value()?)?;
        Ok(())
    }

    /// List stored recommendations, optionally filtered by status. Status comes
    /// from the rebuildable index, not the immutable grain body. Ordered for
    /// review triage — highest severity first, then oldest first — and stable
    /// across runs for identical input.
    pub fn recommendations<S: OmsSubstrate>(
        &self,
        sub: &S,
        status_filter: Option<RecStatus>,
    ) -> Result<Vec<Recommendation>> {
        let p = LoopPersisted::from_value(sub.load_state()?)?;
        let grains = sub.grains_of_type(
            crate::model::grain_type::RECOMMENDATION,
            Some(LOOP_NS),
            ReadOpts {
                live_only: false,
                since_ms: None,
            },
        )?;
        let mut out = Vec::new();
        for g in grains {
            let mut rec = Recommendation::from_fields(&g.hash, &g.fields)?;
            rec.status = p
                .status_index
                .get(&g.hash)
                .copied()
                .unwrap_or(RecStatus::Pending);
            if let Some(f) = status_filter {
                if rec.status != f {
                    continue;
                }
            }
            out.push(rec);
        }
        // Review-queue order: worst first, then oldest first. Hash is only the
        // final tiebreak — sorting by it alone is deterministic per run but
        // meaningless across runs, because a grain's hash covers its timestamp,
        // so an identical queue comes back in a different order every time.
        // `dedup_key` is the last tiebreak that actually decides anything: it is
        // content-derived and stable across runs, whereas findings proposed in
        // the same sweep routinely share a `created_at_ms`. Hash trails it only
        // to make the ordering total.
        out.sort_by(|a, b| {
            b.severity
                .cmp(&a.severity)
                .then(a.created_at_ms.cmp(&b.created_at_ms))
                .then(a.dedup_key.cmp(&b.dedup_key))
                .then(a.hash.cmp(&b.hash))
        });
        Ok(out)
    }

    /// Per-analyzer effective settings for the Setup view: the manifest facts
    /// merged with the file-config (override or manifest default). Read-only.
    pub fn analyzer_settings<S: OmsSubstrate>(
        &self,
        sub: &S,
    ) -> Result<Vec<crate::config::AnalyzerSetting>> {
        let p = LoopPersisted::from_value(sub.load_state()?)?;
        Ok(self
            .analyzers
            .iter()
            .map(|a| {
                let m = a.manifest();
                let cfg = p.config.get(&m.id);
                crate::config::AnalyzerSetting {
                    id: m.id.clone(),
                    title: m.title.clone(),
                    description: m.description.clone(),
                    tier: format!("{:?}", m.tier),
                    trust_class: format!("{:?}", m.trust_class).to_lowercase(),
                    default_on: m.default_on,
                    enabled: cfg.and_then(|c| c.enabled).unwrap_or(m.default_on),
                    severity_floor: cfg
                        .and_then(|c| c.severity_floor)
                        .map(|s| s.as_str().to_string()),
                }
            })
            .collect())
    }

    /// Update one analyzer's file-config (enable/disable, severity floor, param
    /// overrides, namespace scoping). Requires `Admin`. Params are validated
    /// against the analyzer's manifest first (unknown keys rejected, fail-closed),
    /// and the analyzer must exist. Returns the merged config as stored. This is
    /// the only write into `persisted.config` — the config layer, never a grain.
    pub fn set_analyzer_config<S: OmsSubstrate>(
        &self,
        sub: &mut S,
        analyzer_id: &str,
        update: crate::config::AnalyzerConfigUpdate,
        scopes: &ScopeSet,
    ) -> Result<crate::config::AnalyzerConfig> {
        if !scopes.has(Scope::Admin) {
            return Err(Error::ScopeDenied("admin".into()));
        }
        let manifest = self
            .analyzers
            .iter()
            .map(|a| a.manifest())
            .find(|m| m.id == analyzer_id)
            .ok_or_else(|| Error::NotFound(format!("unknown analyzer {analyzer_id:?}")))?;
        // Validate params against the manifest BEFORE touching state.
        if let Some(params) = &update.params {
            manifest.resolve_params(params)?;
        }
        let mut p = LoopPersisted::from_value(sub.load_state()?)?;
        let cfg = p.config.entry(analyzer_id.to_string()).or_default();
        if let Some(enabled) = update.enabled {
            cfg.enabled = Some(enabled);
        }
        if update.clear_floor {
            cfg.severity_floor = None;
        } else if let Some(floor) = update.severity_floor {
            cfg.severity_floor = Some(floor);
        }
        if let Some(params) = update.params {
            cfg.params = params;
        }
        if let Some(ns) = update.namespaces {
            cfg.namespaces = ns;
        }
        let stored = cfg.clone();
        sub.store_state(&p.to_value()?)?;
        Ok(stored)
    }

    /// The measured outcome time series (the Verify gate's history) across all
    /// recommendations, ordered by when each checkpoint was measured.
    pub fn outcomes<S: OmsSubstrate>(&self, sub: &S) -> Result<Vec<crate::recommendation::OutcomeResult>> {
        let p = LoopPersisted::from_value(sub.load_state()?)?;
        let mut out: Vec<_> = p.outcomes.into_values().flatten().collect();
        // `metric` and `rec_hash` break the tie: checkpoints measured in the
        // same sweep share a `measured_at_ms`, and without a tiebreak the order
        // falls through to the map's rec_hash ordering, which shifts every run.
        out.sort_by(|a, b| {
            a.measured_at_ms
                .cmp(&b.measured_at_ms)
                .then(a.horizon_ms.cmp(&b.horizon_ms))
                .then(a.metric.cmp(&b.metric))
                .then(a.rec_hash.cmp(&b.rec_hash))
        });
        Ok(out)
    }

    /// A health snapshot — when the loop last ran, how much is un-analyzed
    /// since, and the queue counts. Lets a host surface "the loop may be stale"
    /// so a forgotten SessionEnd hook / cron doesn't silently kill it.
    pub fn health<S: OmsSubstrate>(&self, sub: &S, now_ms: i64) -> Result<Health> {
        let p = LoopPersisted::from_value(sub.load_state()?)?;
        let (grains_since_run, error_events_since_run) = count_new(sub, p.state.watermark_ms)?;
        let recs = self.recommendations(sub, None)?;
        let mut pending = 0;
        let mut applied = 0;
        for r in &recs {
            match r.status {
                RecStatus::Pending => pending += 1,
                RecStatus::Applied => applied += 1,
                _ => {}
            }
        }
        // Stale if it has never run, or it's been a while / a lot has piled up.
        let stale = match p.state.last_run_ms {
            None => true,
            Some(last) => now_ms - last >= 7 * 86_400_000 || grains_since_run >= 100,
        };
        Ok(Health {
            last_run_ms: p.state.last_run_ms,
            grains_since_run,
            error_events_since_run,
            pending,
            applied,
            total: recs.len() as u64,
            stale,
        })
    }

    /// Approval-rate metric for `origin = llm` recommendations (reflection
    /// design §6b) — the live field-quality signal that accrues off the audit
    /// chain: what fraction of the model's *surfaced* proposals a reviewer
    /// accepts. Complements the offline Effective-Reliability eval.
    pub fn llm_metrics<S: OmsSubstrate>(&self, sub: &S) -> Result<LlmMetrics> {
        let recs = self.recommendations(sub, None)?;
        let mut m = LlmMetrics::default();
        for r in &recs {
            if !matches!(r.origin, Origin::Llm { .. }) {
                continue;
            }
            m.proposed += 1;
            match r.status {
                RecStatus::Pending => m.pending += 1,
                RecStatus::Approved | RecStatus::Applied | RecStatus::RolledBack => m.approved += 1,
                RecStatus::Rejected => m.rejected += 1,
                RecStatus::Expired => {}
            }
        }
        let decided = m.approved + m.rejected;
        m.approval_rate = (decided > 0).then(|| m.approved as f64 / decided as f64);
        Ok(m)
    }
}

/// A health snapshot for the backend's self-improvement loop.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Health {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_run_ms: Option<i64>,
    pub grains_since_run: u64,
    pub error_events_since_run: u64,
    pub pending: u64,
    pub applied: u64,
    pub total: u64,
    /// True when the loop looks stalled (never run, or ≥7d / ≥100 new grains
    /// since the last run) — a nudge that a trigger may be unwired.
    pub stale: bool,
}

/// Approval-rate metric for `origin = llm` recommendations (reflection §6b).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct LlmMetrics {
    /// Total llm-origin recommendations ever stored (those that survived the
    /// verifier and reached the queue).
    pub proposed: u64,
    pub pending: u64,
    /// Approved + Applied + RolledBack (a reviewer said yes at least once).
    pub approved: u64,
    pub rejected: u64,
    /// approved / (approved + rejected); `None` until at least one is decided.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub approval_rate: Option<f64>,
}

/// Re-measure applied recommendations at each **checkpoint** past due, via the
/// engine's typed reads — no CAL-scalar round-trip. A recommendation
/// accumulates one `OutcomeResult` per horizon (measured once each), forming a
/// time series, so a late regression (held at 1d, regressed at 30d) is caught.
/// Only *regressed* checkpoints feed the outcome analyzer (→ a revert).
/// Unknown metric kinds are skipped, never faked.
fn measure_outcomes<S: OmsSubstrate>(
    sub: &S,
    p: &mut LoopPersisted,
    now_ms: i64,
) -> Result<Vec<OutcomeInput>> {
    // Collect all due (recommendation, horizon) checkpoints first.
    let mut due: Vec<(String, crate::config::AppliedRecord, i64)> = Vec::new();
    for (h, a) in &p.applied {
        if p.status_index.get(h) != Some(&RecStatus::Applied) {
            continue;
        }
        let Some(metric) = &a.metric else { continue };
        let done = p.measured.get(h).cloned().unwrap_or_default();
        for horizon in metric.horizons() {
            if now_ms - a.applied_at_ms >= horizon && !done.contains(&horizon) {
                due.push((h.clone(), a.clone(), horizon));
            }
        }
    }

    let mut out = Vec::new();
    for (rec_hash, applied, horizon) in due {
        let metric = applied.metric.as_ref().unwrap();
        let Some(current) = measure_metric(sub, metric, applied.applied_at_ms)? else {
            continue; // metric kind not yet re-measurable
        };
        let regressed = crate::recommendation::is_regression(
            metric.baseline,
            current,
            metric.higher_is_better,
        );
        p.outcomes.entry(rec_hash.clone()).or_default().push(
            crate::recommendation::OutcomeResult {
                rec_hash: rec_hash.clone(),
                metric: metric.metric.clone(),
                baseline: metric.baseline,
                current,
                verdict: if regressed { "regressed" } else { "held" }.into(),
                horizon_ms: horizon,
                measured_at_ms: now_ms,
            },
        );
        p.measured.entry(rec_hash.clone()).or_default().push(horizon);
        if regressed {
            out.push(OutcomeInput {
                rec_hash,
                target_ref: applied.target_ref.clone(),
                metric: metric.metric.clone(),
                baseline: metric.baseline,
                current,
                unit: metric.unit.clone(),
                higher_is_better: metric.higher_is_better,
            });
        }
    }
    Ok(out)
}

/// Typed re-measurement for the fixed set of metric kinds the engine knows.
pub(crate) fn measure_metric<S: SubstrateRead>(
    sub: &S,
    metric: &crate::recommendation::MetricSnapshot,
    since_ms: i64,
) -> Result<Option<f64>> {
    match metric.metric.as_str() {
        // How many times did this tool fail again *with the same signature*
        // after the lesson was applied? Scoped to the signature (metric.relation)
        // so an unrelated later failure of the same tool is not read as a
        // regression of this specific lesson.
        "tool_error_recurrence" => {
            let Some(tool) = &metric.subject else { return Ok(None) };
            let tools = sub.grains_of_type(
                crate::model::grain_type::TOOL,
                None,
                ReadOpts { live_only: true, since_ms: Some(since_ms) },
            )?;
            let n = tools
                .iter()
                .filter(|t| t.tool_name() == Some(tool.as_str()) && t.is_error())
                .filter(|t| {
                    // No stored signature (legacy metric) → fall back to the
                    // whole-tool count so old recommendations still measure.
                    metric.relation.as_deref().is_none_or(|sig| {
                        crate::analyzers::tool_failure::normalize_signature(
                            t.tool_content().unwrap_or(""),
                        ) == sig
                    })
                })
                .count();
            Ok(Some(n as f64))
        }
        // After a resolve-to-latest, does the subject again hold more than one
        // live value under the functional relation? Live-state read (no since
        // filter): the excess beyond one distinct object is the regression.
        "contradiction_recurrence" => {
            let (Some(subject), Some(relation)) = (&metric.subject, &metric.relation) else {
                return Ok(None);
            };
            let facts = scoped_live_facts(sub, metric.namespace.as_deref(), subject)?;
            let distinct: BTreeSet<String> = facts
                .iter()
                .filter(|f| {
                    f.fact_relation()
                        .is_some_and(|r| normalize_ident(r) == *relation)
                })
                .filter_map(|f| f.fact_object().map(normalize_ident))
                .collect();
            Ok(Some(distinct.len().saturating_sub(1) as f64))
        }
        // `evalset:<hash>:<field>` — external correctness, measured the only
        // way that keeps the honesty rule ("Areev Loop improves the agent's
        // memory, not its outputs") intact: an evalset run is an INTERNAL,
        // BOUNDED, ATTRIBUTABLE measurement. The engine never runs the evalset;
        // it only reads summaries a host journaled with `areev eval run`.
        //
        // `since_ms` is the apply time, and it is load-bearing rather than an
        // optimization: a run journaled BEFORE the apply cannot be evidence of
        // what applying did. Comparing the baseline run against itself would
        // report "held" forever — a fabricated receipt, which is worse than no
        // receipt at all. No run since the apply → `None` → not yet
        // measurable, and the checkpoint stays due.
        m if m.starts_with("evalset:") => {
            let Some((evalset, field)) = crate::eval::parse_evalset_metric(m) else {
                return Ok(None);
            };
            let Some(run) = crate::eval::newest_eval_run(sub, evalset, Some(since_ms))? else {
                return Ok(None);
            };
            // `failed`/`passed`/`total` are promoted so a metric can be written
            // against any evalset without the host having to add fields;
            // anything else is read from the summary the host did write.
            Ok(match field {
                "failed" => Some(run.failed as f64),
                "passed" => Some(run.passed as f64),
                "total" => Some(run.total() as f64),
                "error_rate" => match run.total() {
                    0 => None, // no cases ran: undefined, not zero
                    t => Some(run.failed as f64 / t as f64),
                },
                other => run.field(other),
            })
        }
        _ => Ok(None),
    }
}

/// Live facts for one normalized (namespace?, subject) — the shared scope of
/// the fact-shaped recurrence metrics. `namespace: None` spans all namespaces.
fn scoped_live_facts<S: SubstrateRead>(
    sub: &S,
    namespace: Option<&str>,
    subject: &str,
) -> Result<Vec<GrainRecord>> {
    let facts = sub.grains_of_type(
        crate::model::grain_type::FACT,
        None,
        ReadOpts { live_only: true, since_ms: None },
    )?;
    Ok(facts
        .into_iter()
        .filter(|f| namespace.is_none_or(|ns| normalize_ident(&f.namespace) == ns))
        .filter(|f| {
            f.fact_subject()
                .is_some_and(|s| normalize_ident(s) == subject)
        })
        .collect())
}

// --- free helpers ---

/// The §6.3 exact-equality check: a SUPERSEDE is *value-identical* when every
/// replacement field equals the superseded grain's value — strings after
/// case-fold/trim (upstream NFC is an OMS invariant), `namespace` against the
/// grain's own namespace, everything else exactly. This is what makes an
/// auto-applied consolidation provably information-preserving; a
/// near-duplicate (an observation body off by one token) fails it and stays
/// pending for human review. Fails closed: an unrecognized line shape, an
/// empty replacement, a missing grain, or a field the original never had all
/// disqualify.
fn supersede_is_value_identical<S: SubstrateRead>(sub: &S, line: &str) -> bool {
    let Some((target, _gtype, fields)) = crate::cal::parse_own_supersede(line) else {
        return false;
    };
    if fields.is_empty() {
        return false;
    }
    let Ok(Some(grain)) = sub.grain(&target) else {
        return false;
    };
    // An expiry lives OUTSIDE `fields`, so the replacement (built from a fixed
    // field set) can never carry it — consolidating away a grain that has a
    // valid_to would silently drop the expiry, invisibly to the field-by-field
    // check below. Fail closed. (A dup that additionally carries an extra
    // *content* field the replacement omits is a real but subtler info-loss;
    // catching it soundly needs a full canonical-vs-extra comparison rather
    // than this replacement-scoped check, since a real fact's fields also carry
    // OMS metadata like `confidence` that consolidation legitimately keeps —
    // left as a follow-up so this narrow fix can't block valid consolidations.)
    if grain.valid_to_ms.is_some() {
        return false;
    }
    // Forward check: every replacement field equals the grain's value.
    fields.iter().all(|(k, v)| {
        if k == "namespace" {
            return v
                .as_str()
                .is_some_and(|s| normalize_ident(s) == normalize_ident(&grain.namespace));
        }
        match (v, grain.fields.get(k)) {
            (Value::String(a), Some(Value::String(b))) => {
                normalize_ident(a) == normalize_ident(b)
            }
            (a, Some(b)) => a == b,
            (_, None) => false,
        }
    })
}

/// The DISCOVER evidence bundle's total size, and the reserved share each
/// source gets inside it.
///
/// Three sources feed the bundle and they answer different questions, so each
/// is budgeted rather than served first-come: the grains the deterministic
/// findings CITED (what clustering already caught), recent tool ERRORS (what
/// clustering could have caught but did not), and recent facts/observations —
/// the LLM's own lens, and the ONLY source that can carry a problem with no
/// error shape at all. `LENS_RESERVE` is what the last of those is guaranteed.
const EVIDENCE_CAP: usize = 64;
const CITED_SEED_CAP: usize = 24;
const TOOL_SEED_CAP: usize = 16;
const LENS_RESERVE: usize = 24;

/// The confidence floor (§5.4): a verified draft below this is dropped. The
/// verifier's calibrated confidence is the gate, not the proposer's self-report.
const MIN_LLM_CONFIDENCE: f64 = 0.75;

/// The fixed DISCOVER instruction (§5.1). The scoring rule makes "nothing to
/// report" a first-class, zero-penalty answer — the structural antidote to
/// over-generation. Kept in its own request field so it never interleaves with
/// (attacker-influenced) evidence text.
const DISCOVER_INSTRUCTIONS: &str = "You review an agent's memory for quality. \
Given deterministic findings and the evidence they cite, propose ADDITIONAL \
findings the deterministic checks would miss (e.g. a semantic contradiction, a \
stale assumption, a duplicated meaning, a recurring preventable mistake, a \
recurring cost or hand-off the agent's own setup could remove). \
The deterministic findings already cover what the ERROR TEXT says; restating \
one of them earns nothing. The evidence may also contain OUTCOME records — a \
run's observable shape together with whether it was accepted or rejected. A \
problem that raised no error at all is exactly the kind the deterministic \
checks cannot see, so compare the rejected outcomes against the accepted \
ones: a feature they share and the accepted ones lack is a candidate rule. \
Require at least two rejected outcomes before proposing one — a single \
rejection is an anecdote, not a pattern. \
SCORING: propose a finding ONLY if you \
are more than 0.75 confident it is BOTH correct AND materially useful. A correct, \
useful finding earns 1; a wrong or trivial one is penalized 2; returning nothing \
earns 0. When in doubt, propose nothing — an empty list is the correct answer \
when there is nothing worth flagging. The 'approved' and 'rejected' lists, when \
present, show findings this reviewer recently accepted or rejected — prefer the \
kind they accept and avoid the kind they reject. Every proposal MUST cite one \
or more evidence hashes from the bundle, name a 'target', and include your \
confidence 0.0-1.0. Return JSON: {\"recommendations\":[{\"summary\":\"...\",\
\"target\":\"...\",\"guidance\":\"...\",\"evidence\":[\"<hash>\"],\
\"confidence\":0.0,\"proposal\":{...}}]}. \
OMIT 'proposal' for an advisory finding — one worth a human's attention that \
you are not asking to change anything. Include it ONLY when the evidence \
supports a specific change, choosing exactly one kind: \
(1) {\"kind\":\"lesson\",\"lesson\":\"...\"} with target \
\"entity:<ns>/<subject>\" — ONE short imperative rule (max 240 chars) \
preventing a recurring mistake, phrased to apply BEFORE it happens (e.g. \
'Refund a subscription before cancelling it; refunds on cancelled \
subscriptions are refused'). \
(2) {\"kind\":\"fact\",\"relation\":\"...\",\"object\":\"...\"} with the same \
entity target — a durable fact the agent keeps having to be told (an alias, a \
settled default, a preference). 'relation' is a short identifier (letters, \
digits, _ - . :), not a sentence. \
(3) {\"kind\":\"query_revision\",\"body\":\"<CAL>\"} with target \
\"query:<name>\" or \"template:<name>\" — a rewrite of the saved query that \
assembles the agent's context, when the evidence shows it retrieves the wrong \
things. Give the FULL new body; it replaces the old one. \
(4) {\"kind\":\"plan_revision\",\"edits\":[{\"path\":\"...\",\"from\":X,\"to\":Y}]} \
with target \"grain:<workflow hash>\" — at most 8 field-level edits to the \
workflow. Only these paths are editable: 'edges.<i>.cond', \
'edges.<i>.max_cycles', 'retries.<node>'. 'from' MUST equal what the plan \
holds now, or the edit is refused. You cannot add, remove or rewire nodes. \
(5) {\"kind\":\"code_revision\",\"source\":\"...\"} with target \"tool:<name>\" \
— full replacement source for that tool. It is applied only after a recorded \
evaluation run passes, so propose one only when the evidence shows the current \
code is the defect. \
The subject of a fact, the name of a query, the plan hash and the tool name \
all come from 'target' — do not repeat them inside 'proposal'. A proposal \
becomes a change a human reviewer may apply, so it must be fully supported by \
the cited evidence. Propose nothing you cannot ground in the evidence.";

/// The fixed GROUND instruction (§5.2): verify the finding's factual PREMISES
/// are real (anti-fabrication), while allowing an inference. A self-improvement
/// finding may reason BEYOND the evidence (e.g. 'HQ=SF and country=Germany are
/// inconsistent'); grounding checks the premises (HQ=SF, country=Germany) are in
/// the evidence — the soundness of the inference is VERIFY's job, not this one.
const GROUND_INSTRUCTIONS: &str = "You are a grounding checker guarding against \
fabrication. A finding may draw an INFERENCE from facts — your job is to confirm \
the facts it relies on are actually present in the cited evidence, NOT that its \
conclusion is stated verbatim. Decompose the finding into the factual claims it \
depends on. Mark supported=true when those facts are present in the evidence \
(even if the finding reasons beyond them). Mark supported=false ONLY if it relies \
on a fact that is NOT in the evidence (a fabrication) or cites evidence about a \
different subject. Return JSON: {\"results\":[{\"id\":0,\"supported\":true,\"reason\":\"...\"}]}.";

/// The fixed VERIFY instruction (§5.3): adversarial, abstention-biased.
const VERIFY_INSTRUCTIONS: &str = "You are an adversarial reviewer stress-testing \
each finding for SOUNDNESS — reject unsound findings, but only for real reasons, \
never invented ones. For each finding, ask: (1) Reality — is there a genuine, \
SPECIFIC problem, or is it vague/speculative? Reject hedged 'potential' or \
'possible' findings with no concrete defect, and reject any claimed \
inconsistency or contradiction that is not backed by at least two actually \
conflicting facts in the cited evidence. (2) Context — does the finding \
correctly read its cited evidence, or misinterpret what the grains say? Keep a \
finding when it names a genuine, specific problem grounded in its evidence and \
materially useful to a human reviewer; otherwise reject it, and default to \
keep=false when uncertain. Do NOT reject a finding for being 'already known', \
redundant, or a 'common type of error' — duplication is handled elsewhere, and a \
grounded cross-fact inconsistency with two conflicting facts is exactly what to \
KEEP. Give a calibrated confidence 0.0-1.0. Return JSON: \
{\"results\":[{\"id\":0,\"keep\":true,\"confidence\":0.0,\"reason\":\"...\"}]}.";

/// The fixed ENRICH instruction.
const ENRICH_INSTRUCTIONS: &str = "For each finding, optionally add a one-sentence \
guidance note to help a human reviewer decide. Do not restate the finding. Return \
JSON: {\"notes\":[{\"target\":\"<target_ref>\",\"guidance\":\"...\"}]}.";

/// Add a grain to the evidence bundle — deduplicated, bounded at 64, text
/// capped. Shared by the deterministic-citation and recent-grain seeding.
/// `ns_by_hash` records each bundled grain's namespace so an authored lesson
/// can later land in the namespace its evidence lives in (never one the
/// model names).
fn push_evidence(
    evidence: &mut Vec<crate::llm::EvidenceItem>,
    bundle: &mut BTreeSet<String>,
    ns_by_hash: &mut std::collections::BTreeMap<String, String>,
    g: &GrainRecord,
) {
    if evidence.len() < EVIDENCE_CAP && bundle.insert(g.hash.clone()) {
        ns_by_hash.insert(g.hash.clone(), g.namespace.clone());
        evidence.push(crate::llm::EvidenceItem {
            hash: g.hash.clone(),
            grain_type: g.grain_type.clone(),
            text: crate::llm::cap(&grain_brief(g), 400),
        });
    }
}

/// A short human-readable projection of a grain for the evidence bundle.
fn grain_brief(g: &GrainRecord) -> String {
    if let (Some(s), Some(r), Some(o)) = (g.fact_subject(), g.fact_relation(), g.fact_object()) {
        return format!("{s} {r} {o}");
    }
    // Tool grains — the evidence most lesson drafts cite. Rendering them
    // empty starved GROUND of the very facts it exists to check: a correct
    // lesson would be refused as unverifiable (found live — the gate
    // rightly rejected a claim over evidence it could not see).
    if let Some(t) = g.tool_name() {
        let status = if g.is_error() { "error" } else { "ok" };
        let out = g.tool_content().unwrap_or("");
        return format!("tool {t} {status}: {out}");
    }
    for key in ["content", "body", "text", "summary"] {
        if let Some(v) = g.fields.get(key).and_then(|v| v.as_str()) {
            if !v.is_empty() {
                return v.to_string();
            }
        }
    }
    String::new()
}

/// One imperative line, no control characters, capped: the only shape an
/// authored lesson may take. A literal newline could otherwise smuggle a
/// second CAL statement past review (belt: serde_json escapes it anyway) or
/// break the one-line prompt rendering hosts assume.
fn sanitize_lesson(s: &str) -> String {
    sanitize_line(s, crate::llm::MAX_LESSON_LEN)
}

/// One line, no control characters, capped. Every free-text field the model
/// can put into an executable proposal goes through this: a literal newline
/// could otherwise smuggle a second CAL statement past review (belt:
/// serde_json escapes it anyway), split a one-line DEFINE across the batch the
/// apply path iterates, or break the one-line prompt rendering hosts assume.
fn sanitize_line(s: &str, max: usize) -> String {
    let cleaned: String =
        s.chars().map(|c| if c.is_control() { ' ' } else { c }).collect();
    crate::llm::cap(cleaned.trim(), max)
}

/// A relation is an identifier, not prose — it becomes a queryable predicate,
/// and whitespace or quotes in one would make the Fact unfindable by the very
/// recall that should surface it. `None` rejects the draft's `fact` proposal.
fn sanitize_relation(s: &str) -> Option<String> {
    let r = sanitize_line(s, crate::llm::MAX_RELATION_LEN);
    if r.is_empty()
        || !r
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | ':'))
    {
        return None;
    }
    Some(r)
}

/// A model-supplied query body is placed INSIDE a `DEFINE … AS { … }` block,
/// so it is the one place in the vocabulary where model text becomes part of a
/// statement's structure rather than its content. Two belts, because the
/// substrate's parser strength is not something this engine gets to assume:
///
/// - **No braces.** Closing the block early is the injection shape; a saved
///   RECALL/ASSEMBLE body needs no braces of its own, so refusing them costs
///   nothing and fails closed.
/// - **No destructive keyword, anywhere in the body.** `cal::contains_destructive`
///   scans each LINE's leading keyword, which a single-line injection slips
///   past by construction — so scan every token here instead.
///
/// The substrate's own `validate_cal` and the saved-query read-only
/// verification pass still run after this; this is the layer that does not
/// depend on either of them being strict.
fn safe_definition_body(body: &str) -> bool {
    if body.contains('{') || body.contains('}') {
        return false;
    }
    !body
        .split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
        .any(|tok| {
            ["FORGET", "PURGE", "DROP", "DEFINE"]
                .iter()
                .any(|kw| tok.eq_ignore_ascii_case(kw))
        })
}

/// The claim GROUND entails and VERIFY stress-tests. When the draft carries a
/// resolvable proposal the claim names exactly what an apply would do, so what
/// survives the gates is what gets written — never a summary standing in for
/// a change the gates never saw.
fn claim_text(d: &crate::llm::LlmDraft, resolved: Option<&ResolvedProposal>) -> String {
    let summary = crate::llm::cap(&d.summary, crate::llm::MAX_SUMMARY_LEN);
    match resolved {
        Some(r) => format!("{summary} {}", r.rendered),
        None => summary,
    }
}

/// A DISCOVER draft that survived structural validation, carrying the
/// executable form of its proposal. Resolution happens BEFORE GROUND/VERIFY,
/// so both gates judge exactly what an apply would do — the rule the authored
/// lesson already followed, generalized to the whole vocabulary. It also means
/// a malformed proposal costs no model call: it dies here, not at apply.
struct ValidatedDraft {
    draft: crate::llm::LlmDraft,
    target_ref: String,
    cited: Vec<String>,
    resolved: Option<ResolvedProposal>,
}

/// The executable shape of a validated draft. `None` on a [`ValidatedDraft`]
/// means advisory — the model said something a human may want to see, but
/// nothing the engine will ever execute.
struct ResolvedProposal {
    action: ActionKind,
    proposal: Proposal,
    /// One line naming exactly what an apply would do; folded into the claim
    /// both gates judge and shown in the review summary.
    rendered: String,
    summary_key: &'static str,
    summary_args: serde_json::Map<String, Value>,
    rollbackable: bool,
    evalset_hash: Option<String>,
    importance: f64,
    /// Set for the two Fact-writing shapes. Held rather than pre-rendered so
    /// the grain can carry the VERIFIER's confidence, which is not known until
    /// after the gates have run.
    fact_fields: Option<serde_json::Map<String, Value>>,
}

/// The Workflow fields a `plan_revision` may touch. Thresholds and limits —
/// never who calls what.
///
/// The exclusion is structural, not advisory: `nodes`, `edges[].src`,
/// `edges[].dst` and `bindings` are simply not matchable here, so a topology
/// change cannot be expressed by any proposal the model can write. That keeps
/// a plan revision reviewable as a short list of scalar deltas rather than a
/// re-drawn graph, which is the difference between a reviewer checking a
/// number and a reviewer re-deriving a plan.
fn plan_edit_allowed(path: &str) -> bool {
    let seg: Vec<&str> = path.split('.').collect();
    match seg.as_slice() {
        ["edges", i, "cond"] | ["edges", i, "max_cycles"] => i.parse::<usize>().is_ok(),
        ["retries", node] => !node.is_empty(),
        _ => false,
    }
}

/// Read the value at an allowlisted path (absent → `Value::Null`, which is
/// what an edit adding a `retries` entry must declare as its `from`).
fn plan_get(body: &Value, path: &str) -> Value {
    let mut cur = body;
    for seg in path.split('.') {
        cur = match cur {
            Value::Array(a) => match seg.parse::<usize>().ok().and_then(|i| a.get(i)) {
                Some(v) => v,
                None => return Value::Null,
            },
            Value::Object(o) => match o.get(seg) {
                Some(v) => v,
                None => return Value::Null,
            },
            _ => return Value::Null,
        };
    }
    cur.clone()
}

/// Write the value at an allowlisted path. Only creates a missing key in an
/// object (the `retries.<node>` case); never grows an array.
fn plan_set(body: &mut Value, path: &str, to: Value) -> bool {
    let segs: Vec<&str> = path.split('.').collect();
    let Some((last, parents)) = segs.split_last() else {
        return false;
    };
    let mut cur = body;
    for seg in parents {
        cur = match cur {
            Value::Array(a) => match seg.parse::<usize>().ok().and_then(move |i| a.get_mut(i)) {
                Some(v) => v,
                None => return false,
            },
            Value::Object(o) => match o.get_mut(*seg) {
                Some(v) => v,
                None => return false,
            },
            _ => return false,
        };
    }
    match cur {
        Value::Array(a) => match last.parse::<usize>().ok().and_then(move |i| a.get_mut(i)) {
            Some(slot) => {
                *slot = to;
                true
            }
            None => false,
        },
        Value::Object(o) => {
            o.insert((*last).to_string(), to);
            true
        }
        _ => false,
    }
}

/// Type-check one plan edit's new value against the field it targets. Without
/// this a string in `max_cycles` would be dropped by the grain deserializer
/// and the "applied" revision would silently mean *unlimited* — a proposal
/// that reads as a tightening and lands as a removal.
fn plan_value_ok(path: &str, to: &Value) -> bool {
    let seg: Vec<&str> = path.split('.').collect();
    match seg.as_slice() {
        ["edges", _, "cond"] => to.as_str().is_some_and(|c| {
            !c.trim().is_empty() && c.len() <= 200 && !c.chars().any(char::is_control)
        }),
        ["edges", _, "max_cycles"] | ["retries", _] => {
            to.as_u64().is_some_and(|n| n <= 1_000)
        }
        _ => false,
    }
}

/// Resolve a DISCOVER draft's proposal into the executable form an apply would
/// run, or `None` for advisory. Every variant takes its SCOPE from the draft's
/// target and its supporting facts from the substrate — the model names the
/// change, never the subject it lands on, the namespace it lands in, or (for
/// code) the evalset that grades it.
fn resolve_proposal<S: OmsSubstrate>(
    sub: &S,
    d: &crate::llm::LlmDraft,
    target: &TargetRef,
    cited: &[String],
    ns_by_hash: &std::collections::BTreeMap<String, String>,
    caps: Capabilities,
) -> Option<ResolvedProposal> {
    use crate::llm::DraftProposal as P;
    let mut args = serde_json::Map::new();
    match d.parsed_proposal()? {
        // ---- lesson: the pre-vocabulary shape, unchanged ----
        P::Lesson { lesson } => {
            let lesson = sanitize_lesson(&lesson);
            if lesson.is_empty() {
                return None;
            }
            let fields = derived_fact_fields(target, "lesson", &lesson, cited, ns_by_hash)?;
            args.insert("lesson".into(), Value::from(lesson.clone()));
            Some(ResolvedProposal {
                // Same action as the deterministic lesson path — "record a
                // failure-derived lesson" — so dedup groups authored lessons
                // per target and review UIs need no new vocabulary.
                action: ActionKind::ClusterFailure,
                proposal: Proposal::Cal { cal: cal::add("fact", &fields) },
                rendered: format!("Proposed lesson to record: \"{lesson}\""),
                summary_key: "llm.lesson",
                summary_args: args,
                rollbackable: true,
                evalset_hash: None,
                importance: 0.5,
                fact_fields: Some(fields),
            })
        }
        // ---- fact: a durable fact under a model-chosen relation ----
        P::Fact { relation, object } => {
            let relation = sanitize_relation(&relation)?;
            let object = sanitize_line(&object, crate::llm::MAX_OBJECT_LEN);
            if object.is_empty() {
                return None;
            }
            let fields = derived_fact_fields(target, &relation, &object, cited, ns_by_hash)?;
            let subject = fields.get("subject").and_then(Value::as_str).unwrap_or("");
            args.insert("relation".into(), Value::from(relation.clone()));
            args.insert("object".into(), Value::from(object.clone()));
            Some(ResolvedProposal {
                action: ActionKind::Record,
                proposal: Proposal::Cal { cal: cal::add("fact", &fields) },
                rendered: format!("Proposed fact to record: {subject} {relation} \"{object}\""),
                summary_key: "llm.fact",
                summary_args: args,
                rollbackable: true,
                evalset_hash: None,
                importance: 0.5,
                fact_fields: Some(fields),
            })
        }
        // ---- query_revision: how the agent assembles its own context ----
        P::QueryRevision { body } => {
            let name = target.opaque();
            // The name comes from the target, but it still ends up inside a
            // statement — a quote or control character in one would change the
            // statement's shape rather than its content.
            if name.is_empty()
                || name.chars().any(|c| c.is_control() || c == '"' || c == '\\')
            {
                return None;
            }
            let body = sanitize_line(&body, crate::llm::MAX_QUERY_BODY_LEN);
            if body.is_empty() || !safe_definition_body(&body) {
                return None;
            }
            let stmt = match target.scheme() {
                "query" => format!("DEFINE QUERY \"{name}\" AS {{ {body} }}"),
                "template" => format!("DEFINE TEMPLATE {name} AS {{ {body} }}"),
                _ => return None,
            };
            // The substrate owns the grammar: if it will not parse, or will
            // not hand back an inverse, this is not something a reviewer
            // should be offered as applicable. A definition change ROLLBACK
            // could not undo must not be applied at all.
            sub.validate_cal(&stmt).ok()?;
            sub.definition_inverse(&stmt).ok().flatten()?;
            args.insert("name".into(), Value::from(name));
            args.insert("body".into(), Value::from(body.clone()));
            Some(ResolvedProposal {
                action: ActionKind::Revise,
                proposal: Proposal::Cal { cal: stmt },
                rendered: format!("Proposed rewrite of saved {} \"{name}\" to: {body}", target.scheme()),
                summary_key: "llm.query_revision",
                summary_args: args,
                rollbackable: true,
                evalset_hash: None,
                importance: 0.6,
                fact_fields: None,
            })
        }
        // ---- plan_revision: field-level edits to a Workflow grain ----
        P::PlanRevision { edits } => {
            if !caps.plans
                || target.scheme() != "grain"
                || edits.is_empty()
                || edits.len() > crate::llm::MAX_PLAN_EDITS
            {
                return None;
            }
            let hash = target.opaque();
            let g = sub.grain(hash).ok().flatten()?;
            if g.grain_type != "workflow" || !g.is_live() {
                return None;
            }
            let mut body = Value::Object(g.fields.clone());
            let mut deltas = Vec::new();
            let nodes: std::collections::BTreeSet<String> = body
                .get("nodes")
                .and_then(Value::as_array)
                .map(|a| a.iter().filter_map(Value::as_str).map(str::to_string).collect())
                .unwrap_or_default();
            for e in &edits {
                if !plan_edit_allowed(&e.path) || !plan_value_ok(&e.path, &e.to) {
                    return None;
                }
                // A retry count for a node that does not exist is inert, but
                // applying it still mints a new plan hash — and every trigger
                // pointing at the old one must then be walked forward. A
                // no-op is not worth that.
                if let Some(node) = e.path.strip_prefix("retries.") {
                    if !nodes.contains(node) {
                        return None;
                    }
                }
                // `from` is the staleness check: a proposal authored against
                // an older plan does not silently apply to a newer one.
                if plan_get(&body, &e.path) != e.from {
                    return None;
                }
                // A no-op edit is not a revision; it would apply, mint a new
                // plan hash, and orphan every trigger pointing at the old one
                // for nothing.
                if e.from == e.to {
                    return None;
                }
                if !plan_set(&mut body, &e.path, e.to.clone()) {
                    return None;
                }
                deltas.push(format!("{}: {} -> {}", e.path, e.from, e.to));
            }
            // The substrate owns the plan grammar (unique + reachable nodes,
            // conditions parse, every cycle bounded). An edit that would make
            // the plan unrunnable never reaches a reviewer as applicable.
            sub.validate_plan(&body).ok()?;
            let Value::Object(fields) = body else {
                return None;
            };
            let stmt = cal::supersede(hash, "workflow", &fields);
            // Same rule as the definition rewrite: a statement the substrate
            // will not accept is not something to offer a reviewer as
            // applicable. `validate_plan` checked the GRAPH; this checks the
            // statement that carries it.
            sub.validate_cal(&stmt).ok()?;
            args.insert("plan".into(), Value::from(hash));
            args.insert("edits".into(), Value::from(deltas.join("; ")));
            Some(ResolvedProposal {
                action: ActionKind::Revise,
                proposal: Proposal::Cal { cal: stmt },
                rendered: format!("Proposed plan revision ({})", deltas.join("; ")),
                summary_key: "llm.plan_revision",
                summary_args: args,
                rollbackable: true,
                evalset_hash: None,
                importance: 0.7,
                fact_fields: None,
            })
        }
        // ---- code_revision: §7.4, gated by the tool's own evalset ----
        P::CodeRevision { source } => {
            if !caps.code || target.scheme() != "tool" || source.trim().is_empty() {
                return None;
            }
            if source.chars().count() > crate::llm::MAX_CODE_LEN {
                return None;
            }
            // Rule E1's pin, resolved from the substrate. A proposer that
            // could name its own grader is not gated, and a tool that
            // declares no evalset has no gate to pass — advisory either way.
            let evalset = sub.tool_evalset(target.opaque()).ok().flatten()?;
            let mut data = serde_json::Map::new();
            data.insert("tool".into(), Value::from(target.opaque()));
            data.insert("source".into(), Value::from(source.clone()));
            args.insert("tool".into(), Value::from(target.opaque()));
            args.insert("bytes".into(), Value::from(source.len() as u64));
            Some(ResolvedProposal {
                action: ActionKind::CodeRevision,
                proposal: Proposal::Data { data },
                rendered: format!(
                    "Proposed new source for tool {} ({} bytes), gated by evalset {}",
                    target.opaque(),
                    source.len(),
                    evalset
                ),
                summary_key: "llm.code_revision",
                summary_args: args,
                rollbackable: true,
                evalset_hash: Some(evalset),
                importance: 0.8,
                fact_fields: None,
            })
        }
    }
}

/// Stamp a validated DISCOVER draft as an `origin = llm` recommendation.
/// Default shape: an advisory `Flag` carrying `Proposal::Data` (no executable
/// mutation). A draft whose proposal RESOLVED (see [`resolve_proposal`])
/// instead stamps as that executable change, reviewable with the exact line an
/// apply would run. Either way `Origin::Llm` plus the no-manifest analyzer id
/// leave it structurally ineligible for auto-apply — and independently, no
/// class this vocabulary can reach except `memory` is auto-appliable at all
/// (`Policy::grants_auto_apply`). The only path into the agent is a human
/// review with a BECAUSE followed by an explicit apply.
fn stamp_llm(
    model: &str,
    d: &crate::llm::LlmDraft,
    target_ref: String,
    cited: Vec<String>,
    resolved: Option<ResolvedProposal>,
    confidence: f64,
    now_ms: i64,
) -> Recommendation {
    let summary_text = crate::llm::cap(&d.summary, crate::llm::MAX_SUMMARY_LEN);
    let guidance = if d.guidance.trim().is_empty() {
        None
    } else {
        Some(crate::llm::cap(&d.guidance, crate::llm::MAX_GUIDANCE_LEN))
    };
    let (action, proposal, summary, rollbackable, importance, evalset_hash) = match resolved {
        Some(mut r) => {
            // The grain records the VERIFIER's calibrated confidence — the
            // independent signal — never the proposer's self-report.
            if let Some(mut fields) = r.fact_fields.take() {
                fields.insert("confidence".into(), Value::from(confidence.clamp(0.0, 1.0)));
                r.proposal = Proposal::Cal { cal: cal::add("fact", &fields) };
            }
            let mut args = r.summary_args;
            args.insert("text".into(), Value::from(summary_text));
            (
                r.action,
                r.proposal,
                Summary::new(r.summary_key, args),
                r.rollbackable,
                r.importance,
                r.evalset_hash,
            )
        }
        None => {
            let mut args = serde_json::Map::new();
            args.insert("text".into(), Value::from(summary_text));
            let mut data = serde_json::Map::new();
            data.insert("source".into(), Value::from("llm"));
            (
                ActionKind::Flag,
                Proposal::Data { data },
                Summary::new("llm.discover", args),
                false,
                0.3,
                None,
            )
        }
    };
    Recommendation {
        hash: String::new(),
        analyzer: "loop.llm/1".to_string(),
        params_snapshot: serde_json::Map::new(),
        origin: Origin::Llm { model: model.to_string() },
        target_ref: target_ref.clone(),
        action_kind: action,
        dedup_key: dedup_key("llm", &target_ref, action),
        summary,
        severity: Severity::Low,
        proposal,
        destructive: false,
        rollbackable,
        evidence: cited,
        evidence_query: None,
        metric: None,
        // The verifier's calibrated confidence — not a hardcoded default.
        confidence: confidence.clamp(0.0, 1.0),
        importance,
        created_at_ms: now_ms,
        guidance,
        evalset_hash,
        status: RecStatus::Pending,
    }
}

/// The Fact an approved `lesson` or `fact` proposal writes: subject from the
/// entity target, the model's relation (`"lesson"` for the lesson shape —
/// prescriptive prose, distinct from the deterministic `fails_with` signature
/// facts), the sanitized object, and the DOMINANT namespace of the cited
/// evidence (max count, ties to the lexicographically smallest — the
/// tool_failure rule), never a namespace the model names. `None` when the
/// target gives no subject.
fn derived_fact_fields(
    target: &TargetRef,
    relation: &str,
    object: &str,
    cited: &[String],
    ns_by_hash: &std::collections::BTreeMap<String, String>,
) -> Option<serde_json::Map<String, Value>> {
    if target.scheme() != "entity" {
        return None;
    }
    let subject = target
        .opaque()
        .rsplit_once('/')
        .map(|(_, s)| s)
        .unwrap_or(target.opaque());
    if subject.is_empty() {
        return None;
    }
    let mut ns_counts: std::collections::BTreeMap<&str, usize> = Default::default();
    for h in cited {
        if let Some(ns) = ns_by_hash.get(h) {
            if !ns.is_empty() {
                *ns_counts.entry(ns.as_str()).or_default() += 1;
            }
        }
    }
    let lesson_ns = ns_counts
        .iter()
        .max_by(|a, b| a.1.cmp(b.1).then_with(|| b.0.cmp(a.0)))
        .map(|(ns, _)| ns.to_string());
    let mut fields = serde_json::Map::new();
    fields.insert("subject".into(), Value::from(subject));
    fields.insert("relation".into(), Value::from(relation));
    fields.insert("object".into(), Value::from(object));
    // `confidence` is stamped by the caller from the VERIFIER's calibrated
    // score, not the proposer's self-report — so it is deliberately absent
    // here, where only the proposer has spoken.
    if let Some(ns) = lesson_ns {
        fields.insert("namespace".into(), Value::from(ns));
    }
    Some(fields)
}

/// The action kinds that apply ONLY through the evalset-run gating edge
/// (§7.4 for tool code; the tuning seam's adapter promotion inherits the
/// same rule). One predicate so the gate, the rollbackable stamp, and the
/// promotion write can never disagree on membership.
fn requires_gating(kind: ActionKind) -> bool {
    matches!(
        kind,
        ActionKind::CodeRevision | ActionKind::AdapterRevision
    )
}

fn stamp(
    m: &AnalyzerManifest,
    params: &crate::manifest::Params,
    d: crate::recommendation::RecDraft,
    now_ms: i64,
) -> Result<Recommendation> {
    let target = TargetRef::parse(&d.target_ref)?;
    // Rule E1 at the door (§7.4): an unpinned code revision, a pinned
    // non-code target, or a code action on a non-tool target never becomes
    // a recommendation at all.
    crate::recommendation::validate_code_rules(
        d.action_kind,
        target.target_class(),
        d.evalset_hash.as_deref(),
    )?;
    let dedup = dedup_key(m.family(), &d.target_ref, d.action_kind);
    let destructive = match &d.proposal {
        Proposal::Cal { cal } => cal::contains_destructive(cal),
        _ => false,
    };
    let rollbackable = match &d.proposal {
        Proposal::Cal { .. } => !destructive,
        Proposal::Edit { .. } => false,
        // A code or adapter revision applies by WRITING the promotion
        // grain; retracting it is the exact inverse — rollbackable by
        // construction.
        Proposal::Data { .. } => requires_gating(d.action_kind),
    };
    let mut evidence = d.evidence;
    evidence.truncate(MAX_EVIDENCE);
    // Provenance follows the analyzer's trust class: a subprocess
    // (`--analyzer-cmd`) finding is stamped `Command` — structurally
    // auto-apply-ineligible and badged [external] on the recall surface —
    // not `Builtin`.
    let origin = match m.trust_class {
        crate::manifest::TrustClass::Command => Origin::Command { id: m.id.clone() },
        _ => Origin::Builtin,
    };
    Ok(Recommendation {
        hash: String::new(),
        analyzer: m.id.clone(),
        params_snapshot: params.snapshot(),
        origin,
        target_ref: target.as_string(),
        action_kind: d.action_kind,
        dedup_key: dedup,
        summary: d.summary,
        severity: d.severity,
        proposal: d.proposal,
        destructive,
        rollbackable,
        evidence,
        evidence_query: d.evidence_query,
        metric: d.metric,
        confidence: d.confidence,
        importance: d.importance,
        created_at_ms: now_ms,
        guidance: None,
        evalset_hash: d.evalset_hash,
        status: RecStatus::Pending,
    })
}

fn validate_because(because: &str) -> Result<String> {
    let trimmed = because.trim();
    if trimmed.is_empty() {
        return Err(Error::InvalidProposal(
            "a BECAUSE reason is required".into(),
        ));
    }
    if trimmed.chars().count() > MAX_BECAUSE {
        return Err(Error::InvalidProposal(format!(
            "BECAUSE exceeds {MAX_BECAUSE} chars"
        )));
    }
    Ok(trimmed.to_string())
}

fn missing_capability(m: &AnalyzerManifest, caps: Capabilities) -> Option<&'static str> {
    for req in &m.requires {
        match req {
            Capability::Forks if !caps.forks => return Some("forks"),
            Capability::Telemetry if !caps.telemetry => return Some("telemetry"),
            Capability::Embeddings if !caps.embeddings => return Some("embeddings"),
            _ => {}
        }
    }
    None
}

fn severity_floor_for(p: &LoopPersisted, analyzer_id: &str) -> Option<Severity> {
    p.config.get(analyzer_id).and_then(|c| c.severity_floor)
}

fn gate(
    opts: &RunOptions,
    p: &LoopPersisted,
    new_grains: u64,
    new_errors: u64,
    now_ms: i64,
) -> Option<SkipReason> {
    let any = opts.min_new.is_some() || opts.min_new_errors.is_some() || opts.if_stale_ms.is_some();
    if !any {
        return None;
    }
    let min_new_ok = opts.min_new.is_some_and(|m| new_grains >= m);
    let min_err_ok = opts.min_new_errors.is_some_and(|m| new_errors >= m);
    let stale_ok = opts
        .if_stale_ms
        .is_some_and(|d| p.state.last_run_ms.is_none_or(|last| now_ms - last >= d));
    if min_new_ok || min_err_ok || stale_ok {
        return None;
    }
    // Choose the honest reason: staleness-only gate → not_stale, else min_new.
    if opts.if_stale_ms.is_some() && opts.min_new.is_none() && opts.min_new_errors.is_none() {
        Some(SkipReason::NotStale)
    } else {
        Some(SkipReason::MinNewNotMet)
    }
}

fn count_new<S: SubstrateRead>(sub: &S, watermark: Option<i64>) -> Result<(u64, u64)> {
    let opts = ReadOpts {
        live_only: false,
        since_ms: watermark.map(|w| w + 1),
    };
    let mut new_grains = 0u64;
    let mut new_errors = 0u64;
    for t in [
        crate::model::grain_type::FACT,
        crate::model::grain_type::EVENT,
        crate::model::grain_type::TOOL,
        crate::model::grain_type::OBSERVATION,
    ] {
        let g = sub.grains_of_type(t, None, opts)?;
        new_grains += g.len() as u64;
        // The error gate (--min-new-errors) watches captured tool failures.
        if t == crate::model::grain_type::TOOL {
            new_errors += g.iter().filter(|e| e.is_error()).count() as u64;
        }
    }
    Ok((new_grains, new_errors))
}

fn existing_dedup_keys<S: SubstrateRead>(sub: &S, p: &LoopPersisted) -> Result<BTreeSet<String>> {
    let grains = sub.grains_of_type(
        crate::model::grain_type::RECOMMENDATION,
        Some(LOOP_NS),
        ReadOpts {
            live_only: false,
            since_ms: None,
        },
    )?;
    let mut set = BTreeSet::new();
    for g in grains {
        let status = p
            .status_index
            .get(&g.hash)
            .copied()
            .unwrap_or(RecStatus::Pending);
        // Pending/approved (still open) and applied (already handled)
        // recommendations suppress re-proposal of the same finding. Rejected
        // is handled by cooldowns; rolled_back/expired may legitimately
        // re-propose (the situation returned).
        if matches!(
            status,
            RecStatus::Pending | RecStatus::Approved | RecStatus::Applied
        ) {
            if let Some(key) = g.str_field("dedup_key") {
                set.insert(key.to_string());
            }
        }
    }
    Ok(set)
}

fn load_rec<S: SubstrateRead>(sub: &S, rec_hash: &str) -> Result<Recommendation> {
    let g = sub
        .grain(rec_hash)?
        .ok_or_else(|| Error::NotFound(rec_hash.into()))?;
    Recommendation::from_fields(rec_hash, &g.fields)
}

/// Is this line a definition rewrite — a statement that changes a saved
/// `qry:`/`tpl:` registry row rather than writing a grain?
///
/// A keyword test, not a parse: the engine deliberately contains a CAL
/// *writer*, never a parser (parsing is the substrate's job). Both spellings
/// are matched case-insensitively, and `DROP` is intentionally absent — the
/// loop may propose defining a query, never removing one.
pub(crate) fn is_definition_statement(line: &str) -> bool {
    let up = line.trim_start().to_ascii_uppercase();
    up.starts_with("DEFINE QUERY") || up.starts_with("DEFINE TEMPLATE")
}

/// The refusal an advisory `Edit` earns. The engine has no executable edit
/// primitive; the change belongs in the host.
const ADVISORY_EDIT: &str = "This recommendation is advisory: the engine cannot execute this edit. \
     Make the change in the host. Dismiss it with REJECT … BECAUSE while it is still pending, or \
     approve it to acknowledge it and let it expire.";

/// The refusal an advisory `Data` finding earns — every `Data` shape except
/// `outcome_review`'s revert, which carries `revert_of`.
/// Shared by [`Engine::preflight_apply`] and the apply gate so a fused
/// approve-and-apply caller is refused BEFORE the approval lands, not after.
const GATING_REQUIRED: &str = "code and adapter revisions apply only with a recorded gating run \
     (evalset hash + run id + stats) — use apply_gated";

const ADVISORY_DATA: &str = "This finding is advisory: the engine cannot execute it. Act on its \
     guidance. Dismiss it with REJECT … BECAUSE while it is still pending, or approve it to \
     acknowledge it and let it expire.";

/// Whether [`Engine::apply`] can execute this proposal at all.
///
/// One source of truth, shared by [`Engine::preflight_apply`] and
/// [`Engine::apply`] so the two can never disagree.
///
/// Deliberately **not** consulted on approve. An advisory finding is a `Flag`:
/// approving it means "yes, this is real", which is the whole workflow for the
/// LLM path and the telemetry analyzers. What must not happen is a caller being
/// walked into an approval and *then* refused — which is exactly what the fused
/// approve-and-apply path in the bindings did, leaving the recommendation in
/// `approved`, whose only exits are `applied` and `expired`. Preflight asks
/// first, so that path now refuses before it commits anything.
pub(crate) fn ensure_executable(action_kind: ActionKind, proposal: &Proposal) -> Result<()> {
    match proposal {
        Proposal::Cal { .. } => Ok(()),
        Proposal::Edit { .. } => Err(Error::InvalidProposal(ADVISORY_EDIT.into())),
        // Two executable Data shapes: `outcome_review`'s `revert_of` (names
        // an earlier applied recommendation to roll back), and a gated
        // revision (code or adapter), which executes by writing its
        // promotion grain — the gating-run requirement itself is checked at
        // apply, not here.
        Proposal::Data { data } => {
            if requires_gating(action_kind)
                || data.get("revert_of").and_then(Value::as_str).is_some()
            {
                Ok(())
            } else {
                Err(Error::InvalidProposal(ADVISORY_DATA.into()))
            }
        }
    }
}

#[cfg(test)]
mod definition_body_tests {
    use super::safe_definition_body;

    #[test]
    fn ordinary_bodies_pass() {
        assert!(safe_definition_body("RECALL facts WHERE relation = \"lesson\" LIMIT 20"));
        assert!(safe_definition_body("ASSEMBLE context FOR \"desk\" BUDGET 2000"));
    }

    #[test]
    fn a_body_cannot_close_its_own_block_or_carry_destruction() {
        // The injection shape: close the DEFINE block, append a statement.
        // Newlines are already collapsed by `sanitize_line`, so the payload
        // arrives as ONE line — which is exactly what a line-leading-keyword
        // destructive scan cannot see.
        assert!(!safe_definition_body("RECALL facts } FORGET abc {"));
        assert!(!safe_definition_body("RECALL facts } PURGE OLDER THAN 1d {"));
        // …and the keyword alone is refused even without the braces.
        assert!(!safe_definition_body("RECALL facts FORGET abc"));
        assert!(!safe_definition_body("recall facts purge older than 1d"));
        assert!(!safe_definition_body("RECALL facts DROP QUERY \"x\""));
        // A nested DEFINE would redefine something the target does not name.
        assert!(!safe_definition_body("RECALL facts DEFINE QUERY \"other\""));
        // Substring matches are not keywords — this must still pass.
        assert!(safe_definition_body("RECALL facts WHERE subject = \"purged_at\""));
    }
}

#[cfg(test)]
mod plan_edit_tests {
    use super::{plan_edit_allowed, plan_get, plan_set, plan_value_ok};
    use serde_json::json;

    fn plan() -> serde_json::Value {
        json!({
            "nodes": ["fetch", "review", "post"],
            "edges": [
                {"src": "fetch", "dst": "review"},
                {"src": "review", "dst": "fetch", "cond": "confidence < 0.9", "max_cycles": 2}
            ],
            "bindings": {"fetch": "sha256:tool1"},
            "retries": {"fetch": 1}
        })
    }

    #[test]
    fn the_allowlist_admits_thresholds_and_refuses_topology() {
        assert!(plan_edit_allowed("edges.1.cond"));
        assert!(plan_edit_allowed("edges.1.max_cycles"));
        assert!(plan_edit_allowed("retries.fetch"));
        // Topology is not expressible — the structural half of the guarantee
        // that a plan revision stays reviewable as scalar deltas.
        for path in [
            "nodes",
            "nodes.0",
            "edges.0.src",
            "edges.0.dst",
            "edges",
            "bindings.fetch",
            "edges.x.cond",
            "",
        ] {
            assert!(!plan_edit_allowed(path), "{path} must not be editable");
        }
    }

    #[test]
    fn values_are_type_checked_against_the_field() {
        // A string in max_cycles would be DROPPED by the grain deserializer,
        // so an "applied" tightening would silently mean unlimited.
        assert!(!plan_value_ok("edges.1.max_cycles", &json!("2")));
        assert!(plan_value_ok("edges.1.max_cycles", &json!(2)));
        assert!(!plan_value_ok("edges.1.max_cycles", &json!(-1)));
        assert!(!plan_value_ok("retries.fetch", &json!(10_000)));
        assert!(plan_value_ok("retries.fetch", &json!(3)));
        assert!(plan_value_ok("edges.1.cond", &json!("confidence < 0.8")));
        assert!(!plan_value_ok("edges.1.cond", &json!("  ")));
        assert!(!plan_value_ok("edges.1.cond", &json!("a\nb")));
        assert!(!plan_value_ok("edges.0.src", &json!("other")));
    }

    #[test]
    fn get_reads_through_arrays_and_objects_and_absence_is_null() {
        let p = plan();
        assert_eq!(plan_get(&p, "edges.1.max_cycles"), json!(2));
        assert_eq!(plan_get(&p, "retries.fetch"), json!(1));
        // An edit that ADDS a retry declares `from: null` — so absence has to
        // read as Null rather than as an error.
        assert_eq!(plan_get(&p, "retries.review"), json!(null));
        assert_eq!(plan_get(&p, "edges.9.cond"), json!(null));
        assert_eq!(plan_get(&p, "edges.0.cond"), json!(null));
    }

    #[test]
    fn set_writes_scalars_and_adds_a_missing_retry_but_never_grows_an_array() {
        let mut p = plan();
        assert!(plan_set(&mut p, "edges.1.max_cycles", json!(5)));
        assert_eq!(plan_get(&p, "edges.1.max_cycles"), json!(5));
        assert!(plan_set(&mut p, "retries.review", json!(2)));
        assert_eq!(plan_get(&p, "retries.review"), json!(2));
        assert!(!plan_set(&mut p, "edges.7.cond", json!("x")));
        assert_eq!(p["edges"].as_array().unwrap().len(), 2, "no array growth");
    }
}

#[cfg(test)]
mod definition_proposal_tests {
    use super::is_definition_statement;

    #[test]
    fn definition_statements_are_recognized_in_both_spellings() {
        assert!(is_definition_statement(r#"DEFINE QUERY "triage" AS { RECALL facts }"#));
        assert!(is_definition_statement("  define template foo AS { x }"));
        assert!(is_definition_statement("DEFINE TEMPLATE bar AS { y }"));
        // Ordinary proposals are untouched.
        assert!(!is_definition_statement("ADD fact {}"));
        assert!(!is_definition_statement("SUPERSEDE abc WITH fact {}"));
        assert!(!is_definition_statement("FORGET abc"));
        // `DROP` is never proposable, so it is deliberately NOT a definition
        // statement here — a proposal containing one still fails validation
        // rather than being handed an inverse.
        assert!(!is_definition_statement(r#"DROP QUERY "triage""#));
    }
}
