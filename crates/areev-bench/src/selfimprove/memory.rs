//! memory — the Areev bridge: record → loop → review → apply/rollback →
//! lesson assembly (SELFIMPROVE.md). The one honesty rule this module
//! enforces: the LESSONS prompt section is assembled from LIVE grains on
//! every read, so Areev's own apply/rollback is the only lever that can
//! change the agent's prompt — there is no harness flag.
//!
//! One facade for the whole bench (the embedded backend is single-writer per
//! file); a fresh `BorrowedSubstrate` per engine call.

use super::{Ledger, LedgerEntry, TaskRunRecord};
use areev_cal::AreevFacade;
use areev_core::types::{Fact, Observation};
use areev_loop::{
    CommandLlm, Decision, Engine, LlmBackend, ObserverType, Origin, Proposal, ReadOpts,
    RecStatus, Recommendation, RunOptions, ScopeSet, SubstrateRead,
};
use areev_loop_adapter::BorrowedSubstrate;
use areev_store::Areev;
use serde_json::Value;
use std::path::{Path, PathBuf};

/// BECAUSE recorded on the approve and apply of an executable lesson.
const BECAUSE_APPLY: &str = "bench: recurring failure evidence";
/// BECAUSE recorded on the rejection of a destructive proposal.
const BECAUSE_REJECT: &str = "bench: destructive out of scope";
/// BECAUSE recorded on every A/B/A rollback.
const BECAUSE_ROLLBACK: &str = "bench: A/B/A rollback";

/// What the scripted review does with one pending recommendation. Factored
/// out of [`Memory::learn`] so the policy is unit-testable without an engine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReviewAction {
    /// Not executable, or executable-but-out-of-arm (LLM-authored lessons
    /// with `llm_lessons` off) — ledgered, left pending.
    Advisory,
    /// Destructive (FORGET has no inverse) — rejected, never applied.
    Reject,
    /// Executable, non-destructive, rollbackable — approve then apply.
    ApproveApply,
}

/// Which lesson origins this run is allowed to APPLY. The analyzers always
/// run either way — holding discovery constant is what makes the 2x2 read as
/// one variable per axis — so this governs the review gate only.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LessonArms {
    /// Apply deterministic analyzer lessons (the published default).
    pub analyzer: bool,
    /// Apply LLM-authored lessons that survived GROUND + VERIFY.
    pub llm: bool,
}

impl Default for LessonArms {
    fn default() -> Self {
        LessonArms { analyzer: true, llm: false }
    }
}

/// The scripted review policy. Order matters: a `Proposal::Data` is advisory
/// whatever authored it, and an executable lesson is applied only if this run
/// opted into its ORIGIN. The default (`analyzer` on, `llm` off) is the
/// published governed runs' policy, byte-for-byte.
fn review_action(
    origin: &Origin,
    proposal: &Proposal,
    destructive: bool,
    arms: LessonArms,
) -> ReviewAction {
    let origin_admitted = if matches!(origin, Origin::Llm { .. }) {
        arms.llm
    } else {
        arms.analyzer
    };
    if matches!(proposal, Proposal::Data { .. }) || !origin_admitted {
        ReviewAction::Advisory
    } else if destructive {
        ReviewAction::Reject
    } else {
        ReviewAction::ApproveApply
    }
}

/// The lesson the keyless mock loop-LLM authors. Deliberately free of every
/// env error-code string (`customer_not_found`, `rate_limited`, …): the mock
/// agent keys its behavior off those substrings, so this lesson exercises
/// the authored-lesson PLUMBING (stamp → approve → apply → render → rollback)
/// without moving the mock's success rates.
pub const MOCK_LLM_LESSON: &str = "Before retrying a failing tool, change the \
input that caused the recorded failure instead of repeating it unchanged.";

/// The canned loop-LLM for keyless CI (`--mock-llm`): DISCOVER authors one
/// evidence-cited lesson draft, GROUND supports every claim, VERIFY keeps
/// everything at 0.9, ENRICH abstains. Deterministic — a pure function of the
/// request — so the A/B/A/B shape floor can assert the authored-lesson
/// lifecycle without a key. It answers whatever op it is asked, so it also
/// serves as its own ground backend.
pub struct MockLoopLlm;

impl LlmBackend for MockLoopLlm {
    fn model(&self) -> &str {
        "mock-loop-llm"
    }

    fn complete(&self, request: &str) -> areev_loop::Result<String> {
        let v: Value = serde_json::from_str(request).unwrap_or_default();
        let n_of = |ptr: &str| v.pointer(ptr).and_then(Value::as_array).map_or(0, Vec::len);
        let ok = |val: Value| Ok(val.to_string());
        match v.get("op").and_then(Value::as_str) {
            Some("discover") => {
                // Cite the first bundled evidence grain; target the first
                // deterministic finding's entity (the failure cluster the
                // lesson is about). No evidence → abstain, the honest answer.
                let target = v
                    .pointer("/findings/0/target")
                    .and_then(Value::as_str)
                    .filter(|t| t.starts_with("entity:"))
                    .unwrap_or("entity:lessons/support_desk");
                let Some(h) = v.pointer("/evidence/0/hash").and_then(Value::as_str) else {
                    return ok(serde_json::json!({ "recommendations": [] }));
                };
                let mut recs = vec![serde_json::json!({
                    "summary": "the same tool failure keeps recurring across tasks",
                    "target": target,
                    "guidance": "",
                    "evidence": [h],
                    "confidence": 0.9,
                    "lesson": MOCK_LLM_LESSON,
                })];
                // The SILENT archetypes: correlate rejected EPISODES rather
                // than normalize error bodies, which is the one thing the
                // deterministic clustering structurally cannot do. Canned,
                // like everything else in this backend — it proves the path
                // from an episode correlation to a governed applied lesson,
                // and is never a claim that a model would find these.
                for (subject, probe, lesson) in silent_rule_drafts(&v) {
                    recs.push(serde_json::json!({
                        "summary": format!("rejected episodes share a pattern: {probe}"),
                        // A DISTINCT subject per rule, deliberately. Dedup
                        // identity is (family, target, action), so two llm
                        // lessons on one entity collapse into one — which is
                        // correct for the engine and silently lossy here.
                        "target": format!("entity:lessons/{subject}"),
                        "guidance": "",
                        "evidence": [h],
                        "confidence": 0.9,
                        // The generalized proposal vocabulary, exercised
                        // keylessly end to end.
                        "proposal": { "kind": "lesson", "lesson": lesson },
                    }));
                }
                ok(serde_json::json!({ "recommendations": recs }))
            }
            Some("ground") => ok(serde_json::json!({
                "results": (0..n_of("/claims"))
                    .map(|id| serde_json::json!({"id": id, "supported": true, "reason": "mock"}))
                    .collect::<Vec<_>>()
            })),
            Some("verify") => ok(serde_json::json!({
                "results": (0..n_of("/findings"))
                    .map(|id| {
                        serde_json::json!({"id": id, "keep": true, "confidence": 0.9, "reason": "mock"})
                    })
                    .collect::<Vec<_>>()
            })),
            _ => ok(serde_json::json!({ "notes": [] })),
        }
    }
}

/// The episode objects visible in a DISCOVER evidence bundle. Episode facts
/// render as `"<task> episode {json}"` (the engine's fact-triple projection),
/// so the payload is recoverable from the bundle text without the mock
/// needing any privileged read.
fn bundled_episodes(req: &Value) -> Vec<Value> {
    req.get("evidence")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|e| e.get("text").and_then(Value::as_str))
                .filter_map(|t| t.split_once(" episode ").map(|(_, j)| j))
                .filter_map(|j| serde_json::from_str::<Value>(j).ok())
                .collect()
        })
        .unwrap_or_default()
}

/// Which silent-rule lessons the bundled episodes support, as
/// `(lesson subject, what correlated, lesson text)`.
///
/// Each needs at least two rejected episodes sharing the shape — one is an
/// anecdote, and an analyzer that fires on one data point is the failure mode
/// the loop's own rate gates exist to prevent. The lesson wording carries the
/// word the mock agent keys on (`closure` / `enterprise` / `priority`), the
/// same way a rendered tool-failure lesson carries its error code.
fn silent_rule_drafts(req: &Value) -> Vec<(&'static str, &'static str, &'static str)> {
    let eps = bundled_episodes(req);
    let flag = |v: &Value, k: &str| v.get(k).and_then(Value::as_bool).unwrap_or(false);
    let rejected: Vec<&Value> = eps
        .iter()
        .filter(|e| e.get("outcome").and_then(Value::as_str) == Some("rejected"))
        .collect();
    let count = |f: &dyn Fn(&Value) -> bool| rejected.iter().filter(|e| f(e)).count();
    let mut out = Vec::new();
    if count(&|e| flag(e, "cancelled") && !flag(e, "case_logged")) >= 2 {
        out.push((
            "closure_policy",
            "cancelled with no case logged",
            "When you cancel a subscription, also log a case recording the closure.",
        ));
    }
    if count(&|e| {
        e.get("plan").and_then(Value::as_str) == Some("enterprise")
            && !flag(e, "approval_requested")
    }) >= 2
    {
        out.push((
            "enterprise_refund_policy",
            "enterprise refunds with no approval requested",
            "Refunds for an enterprise plan customer always need a manager approval \
             token, whatever the amount.",
        ));
    }
    if count(&|e| {
        flag(e, "case_logged")
            && e.get("case_priority").and_then(Value::as_str).unwrap_or("").is_empty()
    }) >= 2
    {
        out.push((
            "case_priority_policy",
            "cases logged with no priority set",
            "Log a case about a data export request with priority set to high.",
        ));
    }
    out
}

/// The frozen error code out of a tool-result body (mod.rs: tool errors are
/// `{"error":{"code":C,…}}`). `None` when the body is not that shape — a
/// backend flake is not a hidden-rule failure and must not cluster as one.
fn error_code(body: &str) -> Option<String> {
    serde_json::from_str::<Value>(body)
        .ok()?
        .pointer("/error/code")?
        .as_str()
        .map(str::to_string)
}

/// The Areev bridge for the selfimprove benches.
pub struct Memory {
    facade: AreevFacade,
    ns: String,
    runner: String,
    reviewer: String,
    db_path: PathBuf,
}

impl Memory {
    /// Fresh memory file at `dir/bench.db`, session namespace `bench`.
    /// Refuses a pre-existing file: lessons left over from an earlier run
    /// would silently poison A0 (the "ignorant by construction" state).
    pub fn create(dir: &Path) -> Result<Memory, String> {
        std::fs::create_dir_all(dir)
            .map_err(|e| format!("create workdir {}: {e}", dir.display()))?;
        let db_path = dir.join("bench.db");
        if db_path.exists() {
            return Err(format!(
                "{} already exists — a stale memory would corrupt A0; use a fresh \
                 --workdir or delete it",
                db_path.display()
            ));
        }
        let path_str = db_path
            .to_str()
            .ok_or_else(|| format!("workdir path is not UTF-8: {}", db_path.display()))?;
        let store = Areev::open(path_str).map_err(|e| format!("open {path_str}: {e}"))?;
        let facade = AreevFacade::with_session(store, Some("bench".to_string()), None);
        Ok(Memory {
            facade,
            ns: "bench".to_string(),
            runner: "agent:bench-runner".to_string(),
            reviewer: "user:bench-reviewer".to_string(),
            db_path,
        })
    }

    /// The actor label the experience-capture side runs under.
    pub fn runner_actor(&self) -> &str {
        &self.runner
    }

    /// The distinct reviewer actor (separation of duties on the review gate).
    pub fn reviewer_actor(&self) -> &str {
        &self.reviewer
    }

    /// The memory file every phase reads and writes.
    pub fn db_path(&self) -> &Path {
        &self.db_path
    }

    /// Record every tool call of one experience task (thread = task id, so
    /// the transcript and the memory share a correlation key).
    pub fn record_task(&self, rec: &TaskRunRecord) -> Result<(), String> {
        for c in &rec.calls {
            self.facade
                .record_tool_call(
                    &self.ns,
                    &c.tool,
                    Some(&c.input_json),
                    &c.output_json,
                    c.is_error,
                    Some(&rec.task_id),
                    Some(&c.call_id),
                    None,
                    None,
                    None,
                    None,
                    None,
                    None,
                    None,
                )
                .map_err(|e| format!("record_tool_call {} ({}): {e}", c.tool, c.call_id))?;
        }
        self.record_episode(rec)
    }

    /// Record a note a PERSON wrote, as an Observation.
    ///
    /// This is the third kind of learning signal and the one the bench had
    /// none of. R1-R6 announce themselves with an error code; R7-R9 show up
    /// as a correlation across many outcomes. Both are inferred by the desk
    /// from its own failures. A supervisor's note is neither: it is a
    /// complete rule, stated once, before anything has gone wrong for that
    /// reason — and it is exactly the signal the product's flagship example
    /// leads with ("the correction a person made in week one"), which this
    /// harness could not measure at all.
    ///
    /// Stored as an Observation with a human observer, so it is what it says
    /// it is: a person's statement, not a derived fact.
    pub fn record_supervisor_note(&self, task_id: &str, note: &str) -> Result<(), String> {
        let mut obs = Observation::new("user:billing-lead", "human");
        obs.subject = Some("support_desk".to_string());
        obs.object = Some(note.to_string());
        obs.common.namespace = Some(self.ns.clone());
        // grain_brief renders an Observation from `body`, so the note has to
        // be reachable there or the model is handed an empty line.
        obs.common
            .extra_fields
            .insert("body".into(), Value::String(note.to_string()));
        obs.common
            .extra_fields
            .insert("task_id".into(), Value::String(task_id.to_string()));
        self.facade
            .with_store(|m| m.add(&obs))
            .map(|_| ())
            .map_err(|e| format!("record supervisor note ({task_id}): {e}"))
    }

    /// One EPISODE fact per finished task: the observable shape of what the
    /// desk did, plus whether the outcome was accepted.
    ///
    /// This exists because the silent rules (R7-R9) raise no error, so a
    /// memory holding only tool calls cannot express them at all — a
    /// reflection pass would have nothing to reflect on, and "the LLM found
    /// nothing" would be a fact about the harness rather than about the
    /// model. A real desk knows both halves of this: what it did, and whether
    /// the customer accepted it.
    ///
    /// What it deliberately does NOT carry is the scored `failure_reason` or
    /// the attributed `rule_failures`. Those name the rule, and the rule is
    /// the thing under discovery — writing them here would turn the
    /// experiment into a reading-comprehension test. Every field below is
    /// derived from the agent's OWN calls, so nothing here is knowledge the
    /// agent did not itself generate; the one genuinely new bit is the
    /// accepted/rejected outcome.
    fn record_episode(&self, rec: &TaskRunRecord) -> Result<(), String> {
        let used = |tool: &str| rec.calls.iter().any(|c| c.tool == tool && !c.is_error);
        // The plan is whatever the desk saw when it looked the customer up;
        // "unknown" when it never did (which is itself a real signal).
        let plan = rec
            .calls
            .iter()
            .filter(|c| c.tool == "get_customer" && !c.is_error)
            .find_map(|c| {
                serde_json::from_str::<Value>(&c.output_json)
                    .ok()?
                    .get("subscription")?
                    .get("plan")?
                    .as_str()
                    .map(str::to_string)
            })
            .unwrap_or_else(|| "unknown".to_string());
        let case_priority = rec
            .calls
            .iter()
            .filter(|c| c.tool == "log_case" && !c.is_error)
            .find_map(|c| {
                serde_json::from_str::<Value>(&c.input_json)
                    .ok()?
                    .get("priority")?
                    .as_str()
                    .map(str::to_string)
            })
            .unwrap_or_default();
        let episode = serde_json::json!({
            "outcome": if rec.success { "accepted" } else { "rejected" },
            "plan": plan,
            "approval_requested": used("request_approval"),
            "refunded": used("refund"),
            "cancelled": used("cancel_subscription"),
            "case_logged": used("log_case"),
            "case_priority": case_priority,
            "steps": rec.steps,
        });
        let mut fact = Fact::new(&rec.task_id, "episode", &episode.to_string());
        fact.common.namespace = Some(self.ns.clone());
        self.facade
            .with_store(|m| m.add(&fact))
            .map(|_| ())
            .map_err(|e| format!("record episode {}: {e}", rec.task_id))
    }

    /// One governed learn pass at `now_ms`: engine run (deterministic
    /// analyzers, plus DISCOVER→GROUND→VERIFY when `llm_cmd` is given), then
    /// the scripted review over every PENDING recommendation. Returns the
    /// ledger rows this pass added and the hashes it applied.
    ///
    /// The review policy of the PUBLISHED runs: LLM findings stay advisory
    /// (`llm_lessons = false`). The `loop+LLM` arm opts in via [`learn_with`].
    ///
    /// `triggering_actor` stays `None` deliberately: with it set, LLM-origin
    /// recommendations would carry a co-creator, and this bench's review is
    /// scripted, not a human session.
    pub fn learn(
        &self,
        llm_cmd: Option<&str>,
        ground_cmd: Option<&str>,
        now_ms: i64,
    ) -> Result<(Ledger, Vec<String>), String> {
        let llm: Option<Box<dyn LlmBackend>> = match llm_cmd {
            Some(cmd) => Some(Box::new(
                CommandLlm::new(cmd, None).map_err(|e| format!("--llm-cmd: {e}"))?,
            )),
            None => None,
        };
        let ground: Option<Box<dyn LlmBackend>> = match ground_cmd {
            Some(cmd) => Some(Box::new(
                CommandLlm::new(cmd, None).map_err(|e| format!("--ground-cmd: {e}"))?,
            )),
            None => None,
        };
        self.learn_with(llm, ground, LessonArms::default(), now_ms)
    }

    /// [`learn`] with pre-built loop backends and an explicit [`LessonArms`]
    /// — which lesson ORIGINS this run may apply. The analyzers always run;
    /// only the review gate changes, so the 2x2 (analyzer on/off x llm
    /// on/off) varies one thing per axis. Advisory `Proposal::Data` findings
    /// stay advisory in every arm.
    pub fn learn_with(
        &self,
        llm: Option<Box<dyn LlmBackend>>,
        ground: Option<Box<dyn LlmBackend>>,
        arms: LessonArms,
        now_ms: i64,
    ) -> Result<(Ledger, Vec<String>), String> {
        let mut engine = Engine::with_builtins();
        if let Some(backend) = llm {
            engine = engine.with_llm(backend);
        }
        if let Some(backend) = ground {
            engine = engine.with_ground_llm(backend);
        }

        let mut sub = BorrowedSubstrate::new(&self.facade);
        let run = engine
            .run(&mut sub, &RunOptions::default(), now_ms)
            .map_err(|e| format!("loop run: {e}"))?;
        // Print the attrition, because "the model contributed nothing" has
        // several causes that need opposite fixes and are otherwise
        // indistinguishable from an empty ledger.
        if let Some(f) = &run.llm_funnel {
            eprintln!(
                "  llm funnel: evidence {} -> proposed {} -> cited {} \
                 (uncited {}, bad target {}) -> grounded {}{} -> kept {} -> stored {}",
                f.evidence, f.proposed, f.cited, f.dropped_uncited, f.dropped_target,
                f.grounded,
                if f.ground_call_failed {
                    " [GROUND CALL FAILED]".to_string()
                } else if f.grounded == 0 && f.ground_verdicts == 0 && f.cited > 0 {
                    " [GROUND returned nothing usable]".to_string()
                } else {
                    format!(" of {} verdicts", f.ground_verdicts)
                },
                f.kept, f.stored
            );
        }
        let pending: Vec<Recommendation> = engine
            .recommendations(&sub, Some(RecStatus::Pending))
            .map_err(|e| format!("list recommendations: {e}"))?;

        let scopes = ScopeSet::all();
        let mut ledger = Ledger::default();
        let mut applied = Vec::new();
        for r in &pending {
            let source = if matches!(r.origin, Origin::Llm { .. }) {
                "llm".to_string()
            } else {
                r.analyzer.clone()
            };
            let summary = r.summary.render();
            match review_action(&r.origin, &r.proposal, r.destructive, arms) {
                ReviewAction::Advisory => ledger.entries.push(LedgerEntry {
                    hash: r.hash.clone(),
                    source,
                    summary,
                    disposition: "advisory".to_string(),
                    because: "bench: advisory finding, not executable".to_string(),
                }),
                ReviewAction::Reject => {
                    engine
                        .review(
                            &mut sub,
                            &r.hash,
                            Decision::Reject,
                            &self.reviewer,
                            ObserverType::Human,
                            &scopes,
                            BECAUSE_REJECT,
                            now_ms,
                        )
                        .map_err(|e| format!("reject {}: {e}", r.hash))?;
                    ledger.entries.push(LedgerEntry {
                        hash: r.hash.clone(),
                        source,
                        summary,
                        disposition: "rejected".to_string(),
                        because: BECAUSE_REJECT.to_string(),
                    });
                }
                ReviewAction::ApproveApply => {
                    engine
                        .review(
                            &mut sub,
                            &r.hash,
                            Decision::Approve,
                            &self.reviewer,
                            ObserverType::Human,
                            &scopes,
                            BECAUSE_APPLY,
                            now_ms,
                        )
                        .map_err(|e| format!("approve {}: {e}", r.hash))?;
                    // allow_destructive stays false: a destructive payload that
                    // slipped past classification must fail here, not apply.
                    match engine.apply(
                        &mut sub,
                        &r.hash,
                        &self.reviewer,
                        ObserverType::Human,
                        &scopes,
                        BECAUSE_APPLY,
                        false,
                        now_ms,
                    ) {
                        Ok(_) => {
                            applied.push(r.hash.clone());
                            ledger.entries.push(LedgerEntry {
                                hash: r.hash.clone(),
                                source,
                                summary,
                                disposition: "applied".to_string(),
                                because: BECAUSE_APPLY.to_string(),
                            });
                        }
                        Err(e) => ledger.entries.push(LedgerEntry {
                            hash: r.hash.clone(),
                            source,
                            summary,
                            disposition: "apply_failed".to_string(),
                            because: e.to_string(),
                        }),
                    }
                }
            }
        }
        Ok((ledger, applied))
    }

    /// Roll back every hash in `applied`. RolledBack is terminal for a
    /// recommendation hash — restoration goes through a fresh learn pass.
    pub fn rollback(&self, applied: &[String], now_ms: i64) -> Result<(), String> {
        let engine = Engine::with_builtins();
        let mut sub = BorrowedSubstrate::new(&self.facade);
        for hash in applied {
            engine
                .rollback(
                    &mut sub,
                    hash,
                    &self.reviewer,
                    ObserverType::Human,
                    &ScopeSet::all(),
                    BECAUSE_ROLLBACK,
                    now_ms,
                )
                .map_err(|e| format!("rollback {hash}: {e}"))?;
        }
        Ok(())
    }

    /// Every experience-phase tool call as the passive-memory arms see it, in
    /// stable recording order.
    ///
    /// This is the ONLY thing the M arms read. It is deliberately Tool grains
    /// only: lessons live as Facts, so no governed lesson can leak into an arm
    /// that is supposed to be the loop turned OFF.
    ///
    /// Ordering is `created_at_ms` then `hash` — the only keys `GrainRecord`
    /// exposes (there is no seq). Calls recorded inside one millisecond fall
    /// back to hash order: deterministic, and stable across runs, which is what
    /// the prompt bytes need; "most recent first" then holds at the granularity
    /// that matters (across tasks), not within a single burst.
    pub fn experience_grains(&self) -> Result<Vec<super::context::ExperienceGrain>, String> {
        let sub = BorrowedSubstrate::new(&self.facade);
        // ReadOpts::default() is live_only — same read the lesson renderer uses.
        let mut grains = sub
            .grains_of_type("tool", Some(&self.ns), ReadOpts::default())
            .map_err(|e| format!("read tool grains: {e}"))?;
        grains.sort_by(|a, b| {
            a.created_at_ms
                .cmp(&b.created_at_ms)
                .then_with(|| a.hash.cmp(&b.hash))
        });
        Ok(grains
            .iter()
            .map(|g| {
                let tool = g.tool_name().unwrap_or("unknown").to_string();
                let is_error = g.is_error();
                let output_json = g.tool_content().unwrap_or("").to_string();
                let code = if is_error {
                    error_code(&output_json)
                } else {
                    None
                };
                super::context::ExperienceGrain {
                    // `record_tool_call` writes the thread under `session_id`;
                    // `parent_task_id` is the OMS-native alias other writers use.
                    task_id: g
                        .str_field("session_id")
                        .or_else(|| g.str_field("parent_task_id"))
                        .unwrap_or("")
                        .to_string(),
                    rendered: super::context::render_line(
                        &tool,
                        is_error,
                        code.as_deref(),
                        &output_json,
                    ),
                    tool,
                    // `input` round-trips as parsed JSON, not the string that
                    // was handed in.
                    input_json: g
                        .fields
                        .get("input")
                        .map(Value::to_string)
                        .unwrap_or_default(),
                    output_json,
                    is_error,
                    code,
                }
            })
            .collect())
    }

    /// The LESSONS prompt section, rendered from LIVE lesson facts in the
    /// bench namespace: `fails_with` (deterministic failure signatures) and
    /// `lesson` (LLM-authored rules — exist only when a `loop+LLM` arm
    /// applied them). EMPTY STRING when none live — the honesty lever:
    /// rollback tombstones the lesson grains, which must empty this. A run
    /// with no authored lessons renders byte-identically to the published
    /// governed runs.
    pub fn lessons_markdown(&self) -> Result<String, String> {
        let sub = BorrowedSubstrate::new(&self.facade);
        // ReadOpts::default() is live_only — a tombstoned lesson never renders.
        let grains = sub
            .grains_of_type("fact", Some(&self.ns), ReadOpts::default())
            .map_err(|e| format!("read lessons: {e}"))?;
        let pairs = |relation: &str| -> Vec<(String, String)> {
            let mut v: Vec<(String, String)> = grains
                .iter()
                .filter(|g| g.fact_relation() == Some(relation))
                .filter_map(|g| {
                    Some((g.fact_subject()?.to_string(), g.fact_object()?.to_string()))
                })
                .collect();
            // Deterministic prompt bytes: sorted by subject then object.
            v.sort();
            v.dedup();
            v
        };
        let lessons = pairs("fails_with");
        let rules = pairs("lesson");
        if lessons.is_empty() && rules.is_empty() {
            return Ok(String::new());
        }
        let mut out = String::from("## LESSONS (from prior experience)\n");
        if !lessons.is_empty() {
            out.push_str(
                "Recurring tool failures observed in earlier runs. Account for them\n\
                 before and after calling the tool.\n",
            );
        }
        for (subject, object) in &lessons {
            // Render the stored signature legibly: the error code is the part
            // an operator acts on, the rest is its payload. NEVER add the
            // remedy — the agent inferring it IS the thing under test.
            let parsed: Option<Value> = serde_json::from_str(object).ok();
            let err = parsed.as_ref().and_then(|v| v.get("error"));
            match err.and_then(|e| e.get("code")).and_then(|c| c.as_str()) {
                Some(code) => {
                    let detail = err
                        .map(|e| {
                            let mut d = e.clone();
                            if let Some(o) = d.as_object_mut() {
                                o.remove("code");
                            }
                            d.to_string()
                        })
                        .unwrap_or_default();
                    out.push_str(&format!(
                        "- `{subject}` repeatedly failed with error code `{code}` — {detail}\n"
                    ));
                }
                None => out.push_str(&format!("- `{subject}` repeatedly failed with: {object}\n")),
            }
        }
        if !rules.is_empty() {
            // Authored rules ARE the remedy by design — that is the delta the
            // loop+LLM arm measures against signature-only lessons.
            out.push_str("Rules learned from earlier runs. Follow them:\n");
            for (subject, object) in &rules {
                out.push_str(&format!("- `{subject}`: {object}\n"));
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::selfimprove::{RecordedCall, TaskRunRecord, Usage};
    use areev_core::types::Grain;
    use serde_json::Map;
    use tempfile::TempDir;

    const DAY_MS: i64 = 86_400_000;
    const HOUR_MS: i64 = 3_600_000;
    /// Phase clocks: strictly increasing, total span far under the 1-day
    /// outcome_review horizon so no revert fires mid-test.
    const T1: i64 = 1_700_000_000_000;
    const T2: i64 = T1 + HOUR_MS;
    const T3: i64 = T1 + 2 * HOUR_MS;

    /// An EPISODE fact must land for every recorded task — it is the only
    /// trace a SILENT rule (R7-R9) leaves in memory, so if this write is
    /// missing the whole silent-archetype experiment measures nothing and
    /// says so with a clean "the LLM found nothing".
    #[test]
    fn record_task_writes_an_episode_fact_carrying_no_diagnosis() {
        let dir = TempDir::new().unwrap();
        let mem = Memory::create(dir.path()).unwrap();
        let mut rec = failing_task("refund", "rate_limited", 1);
        rec.task_id = "exp-0007".to_string();
        rec.success = false;
        mem.record_task(&rec).unwrap();

        let sub = BorrowedSubstrate::new(&mem.facade);
        let facts = sub
            .grains_of_type("fact", Some(&mem.ns), ReadOpts::default())
            .unwrap();
        let ep = facts
            .iter()
            .find(|g| g.fact_relation() == Some("episode"))
            .expect("an episode fact per recorded task");
        assert_eq!(ep.fact_subject(), Some("exp-0007"));
        let body: Value = serde_json::from_str(ep.fact_object().unwrap()).unwrap();
        assert_eq!(body["outcome"], "rejected");
        // The diagnosis is the thing under discovery — it must never be here.
        let raw = ep.fact_object().unwrap();
        for leak in ["failure_reason", "no_closure_case", "R7", "R8", "R9"] {
            assert!(!raw.contains(leak), "episode leaked {leak}: {raw}");
        }
    }

    /// The SILENT-rule path end to end, keyless: episodes recorded → the
    /// DISCOVER bundle actually carries them → the mock correlates → the
    /// authored lesson survives the gates → the scripted review applies it →
    /// it renders into the prompt.
    ///
    /// Every one of those steps failed silently at least once while this was
    /// being built (the bundle starvation being the sharp one), and each
    /// failure looked exactly like "the model found nothing".
    #[test]
    fn silent_rule_lesson_travels_from_episodes_to_the_prompt() {
        let dir = TempDir::new().unwrap();
        let mem = Memory::create(dir.path()).unwrap();
        // Four closures that cancelled and filed no case — R7's shape, and
        // NOT one error between them.
        for i in 0..4 {
            let mut rec = failing_task("refund", "rate_limited", 0);
            rec.task_id = format!("exp-{i:04}");
            rec.success = false;
            rec.calls = vec![
                RecordedCall {
                    call_id: format!("c{i}a"),
                    tool: "refund".to_string(),
                    input_json: "{}".to_string(),
                    output_json: r#"{"refund_id":"re_1"}"#.to_string(),
                    is_error: false,
                    rule: None,
                },
                RecordedCall {
                    call_id: format!("c{i}b"),
                    tool: "cancel_subscription".to_string(),
                    input_json: "{}".to_string(),
                    output_json: r#"{"status":"cancelled"}"#.to_string(),
                    is_error: false,
                    rule: None,
                },
            ];
            mem.record_task(&rec).unwrap();
        }
        let arms = LessonArms { analyzer: true, llm: true };
        mem.learn_with(Some(Box::new(MockLoopLlm)), None, arms, T1).unwrap();
        let md = mem.lessons_markdown().unwrap();
        assert!(
            md.contains("closure"),
            "the silent-rule lesson never reached the prompt: {md}"
        );
    }

    #[test]
    fn bundled_episodes_are_recovered_from_the_rendered_fact() {
        // The engine renders a Fact into the bundle as "<s> <r> <o>"; if that
        // projection ever changes, the mock's correlation goes quietly blind.
        let req = serde_json::json!({
            "evidence": [
                {"hash": "h1", "grain_type": "fact",
                 "text": r#"exp-0001 episode {"outcome":"rejected","cancelled":true,"case_logged":false}"#},
                {"hash": "h2", "grain_type": "tool", "text": "tool refund error: boom"},
            ]
        });
        let eps = bundled_episodes(&req);
        assert_eq!(eps.len(), 1, "only the episode fact parses");
        assert_eq!(eps[0]["outcome"], "rejected");
        // One episode is an anecdote — the correlation needs two.
        assert!(silent_rule_drafts(&req).is_empty());
    }

    /// Each silent rule's correlation, driven from synthetic episode
    /// bundles. The end-to-end test above walks R7 through the real engine;
    /// R8 and R9 need an agent that looks a customer up (so the plan is
    /// known) and files a case successfully (so a priority is observable),
    /// which the deterministic mock does neither of — so their correlations
    /// are pinned here rather than left to a live run to discover for us.
    #[test]
    fn each_silent_rule_correlates_from_its_own_episode_shape() {
        let bundle = |eps: Vec<Value>| {
            serde_json::json!({
                "evidence": eps
                    .iter()
                    .enumerate()
                    .map(|(i, e)| serde_json::json!({
                        "hash": format!("h{i}"),
                        "grain_type": "fact",
                        "text": format!("exp-{i:04} episode {e}"),
                    }))
                    .collect::<Vec<_>>()
            })
        };
        let subjects = |req: &Value| -> Vec<&'static str> {
            silent_rule_drafts(req).into_iter().map(|(s, _, _)| s).collect()
        };

        let r7 = serde_json::json!({
            "outcome": "rejected", "cancelled": true, "case_logged": false
        });
        assert_eq!(subjects(&bundle(vec![r7.clone(), r7])), vec!["closure_policy"]);

        let r8 = serde_json::json!({
            "outcome": "rejected", "plan": "enterprise", "approval_requested": false
        });
        assert_eq!(
            subjects(&bundle(vec![r8.clone(), r8])),
            vec!["enterprise_refund_policy"]
        );

        let r9 = serde_json::json!({
            "outcome": "rejected", "case_logged": true, "case_priority": ""
        });
        assert_eq!(subjects(&bundle(vec![r9.clone(), r9])), vec!["case_priority_policy"]);

        // ACCEPTED episodes of the same shape correlate nothing: the signal
        // is the outcome, not the shape.
        let accepted = serde_json::json!({
            "outcome": "accepted", "cancelled": true, "case_logged": false
        });
        assert!(subjects(&bundle(vec![accepted.clone(), accepted])).is_empty());
    }

    fn failing_task(tool: &str, code: &str, failures: usize) -> TaskRunRecord {
        let mut calls: Vec<RecordedCall> = (0..failures)
            .map(|i| RecordedCall {
                call_id: format!("call_{i}"),
                tool: tool.to_string(),
                input_json: r#"{"customer_id":"cus_123","amount":250}"#.to_string(),
                output_json: format!(
                    r#"{{"error":{{"code":"{code}","message":"blocked by hidden rule"}}}}"#
                ),
                is_error: true,
                rule: Some("R3"),
            })
            .collect();
        calls.push(RecordedCall {
            call_id: "call_ok".to_string(),
            tool: tool.to_string(),
            input_json: r#"{"customer_id":"cus_123","amount":50}"#.to_string(),
            output_json: r#"{"refund_id":"rf_1"}"#.to_string(),
            is_error: false,
            rule: None,
        });
        TaskRunRecord {
            task_id: "task-exp-1".to_string(),
            success: false,
            steps: (failures + 1) as u32,
            tool_errors: failures as u32,
            rule_failures: vec![("R3", failures as u32)],
            calls,
            final_answer: "could not complete the refund".to_string(),
            usage: Usage::default(),
            failure_reason: "refund not recorded".to_string(),
        }
    }

    /// The product claim in miniature: recurring failures become a governed
    /// lesson; the lesson is in the prompt ⇔ the lesson is applied; rollback
    /// empties the prompt; a fresh governed pass restores it under a NEW hash.
    #[test]
    fn lesson_apply_rollback_reapply_drives_the_prompt() {
        let dir = TempDir::new().unwrap();
        let mem = Memory::create(dir.path()).unwrap();
        // 6 identical failures + 1 success: over tool_failure's min_count=3
        // and min_rate=0.4 gates.
        mem.record_task(&failing_task("refund", "approval_required", 6))
            .unwrap();

        assert_eq!(
            mem.lessons_markdown().unwrap(),
            "",
            "experience captured but nothing applied ⇒ no lessons (A0 state)"
        );

        let (ledger, applied) = mem.learn(None, None, T1).unwrap();
        assert!(
            !applied.is_empty(),
            "tool_failure lesson must apply; ledger: {:?}",
            ledger.entries
        );
        assert!(ledger.count("applied") >= 1);
        let lessons = mem.lessons_markdown().unwrap();
        assert!(
            lessons.starts_with("## LESSONS (from prior experience)\n"),
            "section header: {lessons:?}"
        );
        assert!(
            lessons.contains("approval_required"),
            "frozen error code must appear verbatim (the mock agent keys on \
             it): {lessons:?}"
        );
        assert!(lessons.contains("`refund`"), "tool name verbatim: {lessons:?}");

        mem.rollback(&applied, T2).unwrap();
        assert_eq!(
            mem.lessons_markdown().unwrap(),
            "",
            "rollback must empty the prompt — the honesty lever"
        );

        // RolledBack is terminal: restoration is a fresh proposal (new hash,
        // created_at_ms differs), approved and applied through the same gates.
        let (ledger2, applied2) = mem.learn(None, None, T3).unwrap();
        assert!(
            !applied2.is_empty(),
            "re-proposal must re-apply; ledger: {:?}",
            ledger2.entries
        );
        assert!(
            applied2.iter().all(|h| !applied.contains(h)),
            "a rolled-back hash never re-applies; restoration mints a new \
             recommendation: {applied:?} vs {applied2:?}"
        );
        let lessons2 = mem.lessons_markdown().unwrap();
        assert!(lessons2.contains("approval_required"), "{lessons2:?}");
    }

    /// Destructive proposals (staleness → FORGET) are rejected in review,
    /// never applied — and the targeted grain stays live.
    #[test]
    fn destructive_recommendations_are_rejected_not_applied() {
        let dir = TempDir::new().unwrap();
        let mem = Memory::create(dir.path()).unwrap();
        // A fact whose declared valid_to elapsed → staleness proposes FORGET.
        // Pinned created_at: determinism rule (never the wall clock when the
        // value decides behavior).
        let mut f = areev_core::types::Fact::new("promo-old", "discount_code", "SAVE10")
            .confidence(1.0)
            .valid_to(T1 - 10 * DAY_MS);
        f.common.namespace = Some("bench".to_string());
        f.common.created_at = Some(T1 - 120 * DAY_MS);
        let stale_hash = mem.facade.with_store(|m| m.add(&f)).unwrap().to_hex();

        let (ledger, applied) = mem.learn(None, None, T1).unwrap();
        assert!(
            applied.is_empty(),
            "nothing executable here; ledger: {:?}",
            ledger.entries
        );
        assert!(
            ledger.count("rejected") >= 1,
            "the FORGET must be ledgered as rejected: {:?}",
            ledger.entries
        );
        let row = ledger
            .entries
            .iter()
            .find(|e| e.disposition == "rejected")
            .unwrap();
        assert_eq!(row.because, BECAUSE_REJECT);
        assert!(
            row.source.contains("staleness"),
            "source names the analyzer: {}",
            row.source
        );
        // Rejected means NOT applied: the grain is still live.
        let sub = BorrowedSubstrate::new(&mem.facade);
        let live = sub
            .grains_of_type("fact", Some("bench"), ReadOpts::default())
            .unwrap();
        assert!(
            live.iter().any(|g| g.hash == stale_hash),
            "stale grain must not be tombstoned by a rejected proposal"
        );
        assert_eq!(
            mem.lessons_markdown().unwrap(),
            "",
            "a rejected FORGET contributes no lesson"
        );
    }

    /// The scripted review policy, pinned per rec shape: Data is advisory
    /// whoever authored it; LLM origin stays advisory by default and its
    /// executable authored lessons apply ONLY under the `loop+LLM` arm
    /// (`llm_lessons`); destructive CAL rejects — llm origin included;
    /// non-destructive CAL applies.
    #[test]
    fn review_policy_maps_rec_kinds_to_dispositions() {
        let cal = Proposal::Cal { cal: "ADD fact ...".to_string() };
        let data = Proposal::Data { data: Map::new() };
        let llm = Origin::Llm { model: "test-model".to_string() };

        let det_only = LessonArms::default();
        let both = LessonArms { analyzer: true, llm: true };
        let llm_only = LessonArms { analyzer: false, llm: true };

        // The published-run policy (analyzer only), byte-for-byte.
        assert_eq!(review_action(&llm, &cal, false, det_only), ReviewAction::Advisory);
        assert_eq!(review_action(&llm, &data, false, det_only), ReviewAction::Advisory);
        assert_eq!(review_action(&Origin::Builtin, &data, false, det_only), ReviewAction::Advisory);
        assert_eq!(review_action(&Origin::Builtin, &cal, true, det_only), ReviewAction::Reject);
        assert_eq!(review_action(&Origin::Builtin, &cal, false, det_only), ReviewAction::ApproveApply);

        // Both arms: authored lessons apply; Data stays advisory; a
        // destructive llm payload (cannot be stamped today) would reject,
        // never apply.
        assert_eq!(review_action(&llm, &cal, false, both), ReviewAction::ApproveApply);
        assert_eq!(review_action(&llm, &data, false, both), ReviewAction::Advisory);
        assert_eq!(review_action(&llm, &cal, true, both), ReviewAction::Reject);
        assert_eq!(review_action(&Origin::Builtin, &cal, false, both), ReviewAction::ApproveApply);

        // LLM-only: the analyzers still RUN (their findings are ledgered and
        // still seed the LLM's evidence), but only authored lessons reach
        // memory — which is what isolates the LLM's contribution.
        assert_eq!(review_action(&llm, &cal, false, llm_only), ReviewAction::ApproveApply);
        assert_eq!(review_action(&Origin::Builtin, &cal, false, llm_only), ReviewAction::Advisory);
    }

    /// The loop+LLM arm end-to-end at bench level: the mock loop-LLM authors
    /// a lesson, the scripted review applies it, it renders into LESSONS,
    /// rollback empties it, and a fresh governed pass restores it. With
    /// `llm_lessons` off the same backend's lesson stays advisory and the
    /// rendered prompt is byte-identical to the deterministic-only path.
    #[test]
    fn mock_llm_lesson_lifecycle_under_the_arm_switch() {
        let dir = TempDir::new().unwrap();
        let mem = Memory::create(dir.path()).unwrap();
        mem.record_task(&failing_task("stripe_refund", "approval_required", 4)).unwrap();

        // Arm OFF: the authored lesson survives the gates but is ledgered
        // advisory — nothing llm-origin applies, no rule line renders.
        let (ledger, applied) = mem
            .learn_with(Some(Box::new(MockLoopLlm)), None, LessonArms::default(), T1)
            .unwrap();
        assert!(
            ledger.entries.iter().any(|e| e.source == "llm" && e.disposition == "advisory"),
            "authored lesson ledgered advisory with the arm off"
        );
        let md = mem.lessons_markdown().unwrap();
        assert!(!md.contains(MOCK_LLM_LESSON), "arm off ⇒ no authored rule in the prompt");
        mem.rollback(&applied, T2).unwrap();

        // Arm ON: approved + applied through the same scripted review, and
        // the rule renders under the LESSONS heading the agent scans.
        let arms = LessonArms { analyzer: true, llm: true };
        let (ledger, applied) = mem
            .learn_with(Some(Box::new(MockLoopLlm)), None, arms, T3)
            .unwrap();
        assert!(
            ledger.entries.iter().any(|e| e.source == "llm" && e.disposition == "applied"),
            "authored lesson applied under the arm: {:?}",
            ledger.entries
        );
        let md = mem.lessons_markdown().unwrap();
        assert!(md.starts_with("## LESSONS (from prior experience)\n"));
        assert!(md.contains(MOCK_LLM_LESSON), "authored rule renders: {md}");

        // Rollback empties the WHOLE section — authored rules included.
        mem.rollback(&applied, T3 + HOUR_MS).unwrap();
        assert_eq!(mem.lessons_markdown().unwrap(), "", "rollback empties authored rules too");

        // Restoration is a fresh governed pass (rolled_back is terminal).
        let (_, applied2) = mem
            .learn_with(Some(Box::new(MockLoopLlm)), None, arms, T3 + 2 * HOUR_MS)
            .unwrap();
        assert!(!applied2.is_empty());
        assert!(mem.lessons_markdown().unwrap().contains(MOCK_LLM_LESSON));
    }

    /// The passive-memory arms' input round-trips through the store: order,
    /// error-code parsing, is_error, the rendered line, and the task join.
    #[test]
    fn experience_grains_round_trip_the_recorded_calls() {
        let dir = TempDir::new().unwrap();
        let mem = Memory::create(dir.path()).unwrap();
        assert!(
            mem.experience_grains().unwrap().is_empty(),
            "no experience yet ⇒ the arms have nothing to offer"
        );

        mem.record_task(&failing_task("refund", "approval_required", 2))
            .unwrap();

        let grains = mem.experience_grains().unwrap();
        assert_eq!(grains.len(), 3, "2 failures + 1 success: {grains:?}");
        assert!(grains.iter().all(|g| g.tool == "refund"));
        assert!(grains.iter().all(|g| g.task_id == "task-exp-1"), "{grains:?}");

        let errors: Vec<_> = grains.iter().filter(|g| g.is_error).collect();
        assert_eq!(errors.len(), 2);
        assert!(
            errors
                .iter()
                .all(|g| g.code.as_deref() == Some("approval_required")),
            "the frozen code must parse out of the body: {errors:?}"
        );
        assert!(
            errors[0].rendered
                == "- `refund` call failed with `approval_required`: \
                    {\"error\":{\"code\":\"approval_required\",\"message\":\"blocked by hidden rule\"}}",
            "rendered: {:?}",
            errors[0].rendered
        );

        let ok: Vec<_> = grains.iter().filter(|g| !g.is_error).collect();
        assert_eq!(ok.len(), 1);
        assert_eq!(ok[0].code, None, "a success carries no code");
        assert_eq!(ok[0].rendered, "- `refund` call succeeded");
        assert!(ok[0].output_json.contains("rf_1"), "{:?}", ok[0].output_json);
        assert!(
            ok[0].input_json.contains("\"amount\":50"),
            "input round-trips as JSON: {:?}",
            ok[0].input_json
        );

        // Stable across reads — the prompt bytes depend on it.
        assert_eq!(grains, mem.experience_grains().unwrap());
    }

    /// Lessons are Facts; the arms read Tool grains. A governed lesson must
    /// never reach an arm that is supposed to be the loop turned OFF.
    #[test]
    fn experience_grains_never_carry_a_lesson() {
        let dir = TempDir::new().unwrap();
        let mem = Memory::create(dir.path()).unwrap();
        mem.record_task(&failing_task("refund", "approval_required", 6))
            .unwrap();
        let before = mem.experience_grains().unwrap();
        let (_, applied) = mem.learn(None, None, T1).unwrap();
        assert!(!applied.is_empty());
        assert!(!mem.lessons_markdown().unwrap().is_empty());

        let after = mem.experience_grains().unwrap();
        assert_eq!(before, after, "applying a lesson must not change the arms' input");
        assert!(
            after.iter().all(|g| g.tool == "refund"),
            "only Tool grains: {after:?}"
        );
    }

    /// A leftover memory file must refuse, not silently resume: lessons from
    /// an earlier run would poison A0.
    #[test]
    fn create_refuses_a_preexisting_memory_file() {
        let dir = TempDir::new().unwrap();
        let first = Memory::create(dir.path()).unwrap();
        drop(first);
        let err = match Memory::create(dir.path()) {
            Ok(_) => panic!("second create over the same workdir must refuse"),
            Err(e) => e,
        };
        assert!(err.contains("already exists"), "{err}");
    }
}
