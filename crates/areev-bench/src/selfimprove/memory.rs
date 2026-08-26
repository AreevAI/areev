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
use areev_loop::{
    CommandLlm, Decision, Engine, ObserverType, Origin, Proposal, ReadOpts, RecStatus,
    Recommendation, RunOptions, ScopeSet, SubstrateRead,
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
    /// Not executable by design (LLM DISCOVER findings are `Proposal::Data`;
    /// flag-kind analyzer findings likewise) — ledgered, left pending.
    Advisory,
    /// Destructive (FORGET has no inverse) — rejected, never applied.
    Reject,
    /// Executable, non-destructive, rollbackable — approve then apply.
    ApproveApply,
}

/// The scripted review policy. Order matters: an LLM finding is advisory even
/// when its proposal LOOKS executable (apply refuses `origin = llm` data), and
/// a `Proposal::Data` is advisory whatever authored it.
fn review_action(origin: &Origin, proposal: &Proposal, destructive: bool) -> ReviewAction {
    if matches!(origin, Origin::Llm { .. }) || matches!(proposal, Proposal::Data { .. }) {
        ReviewAction::Advisory
    } else if destructive {
        ReviewAction::Reject
    } else {
        ReviewAction::ApproveApply
    }
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
        Ok(())
    }

    /// One governed learn pass at `now_ms`: engine run (deterministic
    /// analyzers, plus DISCOVER→GROUND→VERIFY when `llm_cmd` is given), then
    /// the scripted review over every PENDING recommendation. Returns the
    /// ledger rows this pass added and the hashes it applied.
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
        let mut engine = Engine::with_builtins();
        if let Some(cmd) = llm_cmd {
            let backend =
                CommandLlm::new(cmd, None).map_err(|e| format!("--llm-cmd: {e}"))?;
            engine = engine.with_llm(Box::new(backend));
        }
        if let Some(cmd) = ground_cmd {
            let backend =
                CommandLlm::new(cmd, None).map_err(|e| format!("--ground-cmd: {e}"))?;
            engine = engine.with_ground_llm(Box::new(backend));
        }

        let mut sub = BorrowedSubstrate::new(&self.facade);
        engine
            .run(&mut sub, &RunOptions::default(), now_ms)
            .map_err(|e| format!("loop run: {e}"))?;
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
            match review_action(&r.origin, &r.proposal, r.destructive) {
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

    /// The LESSONS prompt section, rendered from LIVE `fails_with` facts in
    /// the bench namespace. EMPTY STRING when none live — the honesty lever:
    /// rollback tombstones the lesson grains, which must empty this.
    pub fn lessons_markdown(&self) -> Result<String, String> {
        let sub = BorrowedSubstrate::new(&self.facade);
        // ReadOpts::default() is live_only — a tombstoned lesson never renders.
        let grains = sub
            .grains_of_type("fact", Some(&self.ns), ReadOpts::default())
            .map_err(|e| format!("read lessons: {e}"))?;
        let mut lessons: Vec<(String, String)> = grains
            .iter()
            .filter(|g| g.fact_relation() == Some("fails_with"))
            .filter_map(|g| {
                Some((g.fact_subject()?.to_string(), g.fact_object()?.to_string()))
            })
            .collect();
        if lessons.is_empty() {
            return Ok(String::new());
        }
        // Deterministic prompt bytes: sorted by subject then object.
        lessons.sort();
        lessons.dedup();
        let mut out = String::from(
            "## LESSONS (from prior experience)\n\
             Recurring tool failures observed in earlier runs. Account for them\n\
             before and after calling the tool.\n",
        );
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

    /// The scripted review policy, pinned per rec shape: LLM origin trumps an
    /// executable-looking proposal; Data is advisory whoever authored it;
    /// destructive CAL rejects; non-destructive CAL applies.
    #[test]
    fn review_policy_maps_rec_kinds_to_dispositions() {
        let cal = Proposal::Cal { cal: "ADD fact ...".to_string() };
        let data = Proposal::Data { data: Map::new() };
        let llm = Origin::Llm { model: "test-model".to_string() };

        assert_eq!(review_action(&llm, &cal, false), ReviewAction::Advisory);
        assert_eq!(review_action(&llm, &data, false), ReviewAction::Advisory);
        assert_eq!(review_action(&Origin::Builtin, &data, false), ReviewAction::Advisory);
        assert_eq!(review_action(&Origin::Builtin, &cal, true), ReviewAction::Reject);
        assert_eq!(review_action(&Origin::Builtin, &cal, false), ReviewAction::ApproveApply);
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
