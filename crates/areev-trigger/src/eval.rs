//! The evaluation cycle: find due, claim, invoke, ingest, start, release.
//!
//! One invocation performs the whole cycle and exits. Nothing stays resident.
//! Losing a claim race is a no-op exit, not an error — that is the steady state
//! when several nodes share a heartbeat.
//!
//! ## Why correctness does not rest on the lease
//!
//! A lease deduplicates *"who is working right now"*. But the thing that
//! actually needs deduplicating — a firing of trigger T for item I — has a
//! natural identity, so the run id is derived from it. `areev run start`
//! already refuses a duplicate run id, so two evaluators racing produce one run
//! and one recorded skip, with no lease consulted at all: no duration to guess,
//! no fencing token to compare, no clock-skew window.
//!
//! The lease then does one narrower job: stopping two nodes from both making
//! the same expensive, side-effecting connector call. Losing it can cost a
//! wasted API call; it can never cause a missed or duplicated firing. That is
//! the risk profile a lease should carry.

use std::sync::Arc;

use areev_cal::AreevFacade;
use areev_core::types::{Event, Grain, GrainType, Observation, Trigger, TriggerKind};
use areev_run::HostToolExecutor;
use areev_store::TriggerState;
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::clock::Clock;
use crate::connector::{dedup_value, PollItem, PollRequest, PollResponse};
use crate::error::{Result, TriggerError};
use crate::schedule;

/// Namespace for trigger firing records. Alongside the runtime's own
/// `agent:harness`, so operational journalling stays out of user namespaces.
pub const TRIGGER_NS: &str = "agent:triggers";

/// How many triggers one invocation will look at. A backstop, not a policy.
const MAX_TRIGGERS_SCANNED: usize = 4096;

/// Knobs for one evaluation pass.
#[derive(Debug, Clone)]
pub struct EvalOptions {
    /// Evaluate only this trigger (its content address).
    pub only: Option<String>,
    /// How long a claim is held before another evaluator may take over.
    /// Deliberately generous: a lease shorter than the worst-case connector
    /// runtime causes takeovers mid-poll, which costs duplicate API calls.
    pub lease: std::time::Duration,
    /// Cap on items requested per firing.
    pub max_items: usize,
    /// Report what would happen and touch nothing. The safe first command on a
    /// new deployment.
    pub dry_run: bool,
    /// This evaluator's identity, recorded in the claim.
    pub node: String,
}

impl Default for EvalOptions {
    fn default() -> Self {
        EvalOptions {
            only: None,
            lease: std::time::Duration::from_secs(300),
            max_items: 100,
            dry_run: false,
            node: default_node_id(),
        }
    }
}

/// A stable-ish identity for this evaluator. Host name plus pid: enough to tell
/// two nodes apart in a claim, and to recognise our own stale claim after a
/// restart.
fn default_node_id() -> String {
    let host = std::env::var("HOSTNAME")
        .or_else(|_| std::env::var("COMPUTERNAME"))
        .unwrap_or_else(|_| "node".into());
    format!("{host}/{}", std::process::id())
}

/// What one evaluation pass did. Shaped like `loop run`'s report for symmetry.
#[derive(Debug, Clone, Default, Serialize)]
pub struct EvalReport {
    pub claimed: usize,
    pub skipped_not_due: usize,
    pub skipped_locked: usize,
    pub skipped_paused: usize,
    pub items: usize,
    pub duplicates: usize,
    pub runs_started: usize,
    /// Items the connector returned that carry no usable identity, so no run
    /// could be minted for them. Reported rather than silently dropped: an item
    /// nobody can name is a connector bug, and it should be visible as one.
    pub unidentifiable: usize,
    /// Not fatal to the pass: one broken trigger must not stop the others.
    pub errors: Vec<String>,
    /// A connector reported a backlog, so this trigger is due again at once
    /// rather than after its interval.
    pub draining: Vec<String>,
}

/// One trigger considered, for `--dry-run` and `status`.
#[derive(Debug, Clone, Serialize)]
pub struct TriggerStatus {
    pub trigger: String,
    pub kind: String,
    pub workflow: String,
    pub enabled: bool,
    pub paused: bool,
    pub due: bool,
    pub leased_by: Option<String>,
    pub next_due_at: Option<i64>,
    pub last_fired_at: Option<i64>,
    pub consecutive_failures: u32,
    pub last_error: Option<String>,
    /// Never fired and no failure recorded — the state an unnoticed
    /// misconfiguration sits in, so it is reported rather than inferred.
    pub never_fired: bool,
}

/// Starts the workflow a trigger is bound to.
///
/// A seam rather than a direct `areev_run::Runner` call, for the same reason
/// the CAL executor takes a governance host: the runtime sits above this crate
/// in the dependency order for hosts that want it, and a test needs to observe
/// what *would* be started without spending a real run. It is also where the
/// duplicate rule lives, which is the whole idempotency story.
pub trait RunStarter: Send + Sync {
    fn start(&self, workflow: &str, run_id: &str, input: serde_json::Value) -> StartResult;
}

/// What starting a run did.
#[derive(Debug, Clone, PartialEq)]
pub enum StartResult {
    Started,
    /// A run with this id already exists. **Not an error** — it is the
    /// idempotency guarantee working: this item was already handled, by an
    /// earlier firing or by another node that won the race.
    Duplicate,
    Failed(String),
}

/// Evaluates triggers against one memory.
pub struct Evaluator {
    pub facade: Arc<AreevFacade>,
    pub clock: Arc<dyn Clock>,
    /// Executes connector commands. `None` means a due polling trigger fails
    /// loudly with `TRG-E003` rather than quietly doing nothing — a poll that
    /// silently returns no items is indistinguishable from a healthy source.
    pub connector: Option<Arc<dyn HostToolExecutor>>,
    /// Starts the bound workflow. `None` ingests the item and records the
    /// firing without starting anything — what `--no-start` is for, and what
    /// the ingest-only tests use.
    pub starter: Option<Arc<dyn RunStarter>>,
    pub ns: String,
    pub principal: String,
}

impl Evaluator {
    /// Every trigger declaration in the memory, newest first.
    pub fn declarations(&self) -> Result<Vec<(String, Trigger)>> {
        let ns = self.ns.clone();
        let grains = self
            .facade
            .with_store(|m| {
                m.recent_live_scoped(&[ns], Some(GrainType::Trigger), MAX_TRIGGERS_SCANNED)
            })
            .map_err(|e| TriggerError::Storage { detail: e.to_string() })?;

        let mut out = Vec::new();
        for g in grains {
            let hash = g.hash.to_hex();
            match g.to_trigger() {
                Ok(t) => out.push((hash, t)),
                // A declaration this build cannot read is reported, never
                // skipped silently: silence is the symptom of every trigger
                // failure, so it must never also be the symptom of a bug.
                Err(e) => {
                    return Err(TriggerError::Malformed {
                        what: format!("declaration {hash} is unreadable: {e}"),
                    })
                }
            }
        }
        Ok(out)
    }

    /// What every trigger's state looks like right now.
    pub fn status(&self) -> Result<Vec<TriggerStatus>> {
        let now = self.clock.now_ms();
        let mut out = Vec::new();
        for (hash, t) in self.declarations()? {
            let st = self
                .facade
                .with_store(|m| m.trigger_state(&hash))
                .map_err(|e| TriggerError::Storage { detail: e.to_string() })?
                .map(|(s, _)| s)
                .unwrap_or_default();
            out.push(TriggerStatus {
                trigger: hash,
                kind: t.kind.as_str().to_string(),
                workflow: t.workflow.clone(),
                enabled: t.enabled,
                paused: st.paused,
                due: t.enabled && st.due(now) && !st.leased(now),
                leased_by: st.leased(now).then(|| st.claimed_by.clone()).flatten(),
                next_due_at: st.next_due_at,
                last_fired_at: st.last_fired_at,
                consecutive_failures: st.consecutive_failures,
                last_error: st.last_error.clone(),
                never_fired: st.last_fired_at.is_none() && st.consecutive_failures == 0,
            });
        }
        Ok(out)
    }

    /// Run one evaluation pass.
    pub fn run(&self, opts: &EvalOptions) -> Result<EvalReport> {
        let mut report = EvalReport::default();
        let now = self.clock.now_ms();

        for (hash, trigger) in self.declarations()? {
            if opts.only.as_ref().is_some_and(|only| only != &hash) {
                continue;
            }
            if !trigger.enabled {
                report.skipped_not_due += 1;
                continue;
            }
            let stored = self
                .facade
                .with_store(|m| m.trigger_state(&hash))
                .map_err(|e| TriggerError::Storage { detail: e.to_string() })?;
            let (state, raw) = match stored {
                Some((s, r)) => (s, Some(r)),
                None => (TriggerState::default(), None),
            };

            if state.paused {
                report.skipped_paused += 1;
                continue;
            }
            if !state.due(now) {
                report.skipped_not_due += 1;
                continue;
            }
            if state.leased(now) {
                // The normal steady state with several nodes on one heartbeat,
                // not a problem.
                report.skipped_locked += 1;
                continue;
            }
            if opts.dry_run {
                report.claimed += 1;
                continue;
            }

            match self.fire(&hash, &trigger, state, raw.as_deref(), now, opts) {
                // Lost the claim between deciding it was free and taking it.
                // Normal with several nodes on one heartbeat, and it must not
                // be counted as work done.
                Ok(outcome) if outcome.skipped_locked => report.skipped_locked += 1,
                Ok(outcome) => {
                    report.claimed += 1;
                    report.items += outcome.items;
                    report.duplicates += outcome.duplicates;
                    report.runs_started += outcome.runs_started;
                    report.unidentifiable += outcome.unidentifiable;
                    for why in &outcome.failures {
                        report.errors.push(format!("{hash}: {why}"));
                    }
                    if outcome.draining {
                        report.draining.push(hash.clone());
                    }
                }
                Err(e) => {
                    // One broken trigger must not stop the others.
                    report.errors.push(e.to_string());
                }
            }
        }
        Ok(report)
    }

    fn fire(
        &self,
        hash: &str,
        trigger: &Trigger,
        state: TriggerState,
        raw: Option<&str>,
        now: i64,
        opts: &EvalOptions,
    ) -> Result<FireOutcome> {
        // Claim. Losing means another evaluator got here first.
        let mut claimed = state.clone();
        claimed.fence = claimed.fence.wrapping_add(1);
        claimed.claimed_by = Some(opts.node.clone());
        claimed.lease_until = Some(now + opts.lease.as_millis() as i64);
        let won = self
            .facade
            .with_store(|m| m.put_trigger_state(hash, raw, &claimed))
            .map_err(|e| TriggerError::Storage { detail: e.to_string() })?;
        if !won {
            return Ok(FireOutcome { skipped_locked: true, ..Default::default() });
        }
        // What a later release must present as `expected`.
        let claimed_raw = self
            .facade
            .with_store(|m| m.trigger_state(hash))
            .map_err(|e| TriggerError::Storage { detail: e.to_string() })?
            .map(|(_, r)| r);

        let result = self.collect_and_start(hash, trigger, &claimed, now, opts);

        // Release under the fence, whatever happened.
        let mut released = claimed.clone();
        released.claimed_by = None;
        released.lease_until = None;
        match &result {
            Ok(outcome) => {
                released.last_fired_at = Some(now);
                released.consecutive_failures = 0;
                released.last_error = None;
                if let Some(c) = &outcome.cursor {
                    released.cursor = Some(c.clone());
                }
                released.next_due_at = if outcome.draining {
                    // A backlog drains at once rather than waiting out the
                    // interval — a cold start should not take a day.
                    Some(now)
                } else {
                    schedule::advance_after_firing(trigger, now, now)?
                };
            }
            Err(e) => {
                released.consecutive_failures = claimed.consecutive_failures.saturating_add(1);
                released.last_error = Some(e.to_string());
                released.next_due_at =
                    Some(schedule::backoff_until(trigger, released.consecutive_failures, now));
            }
        }
        let released_ok = self
            .facade
            .with_store(|m| m.put_trigger_state(hash, claimed_raw.as_deref(), &released))
            .map_err(|e| TriggerError::Storage { detail: e.to_string() })?;
        if !released_ok {
            // Our lease expired mid-firing and someone else took over. Their
            // state stands; ours is refused. The work we did is not lost — the
            // runs we started are journaled and idempotent — but we must not
            // write back over the new owner.
            self.journal(hash, trigger, &FireOutcome::default(), Some("claim lost"), now)?;
            return Err(TriggerError::ClaimLost { trigger: hash.to_string() });
        }

        let outcome = result?;
        self.journal(hash, trigger, &outcome, None, now)?;
        Ok(outcome)
    }

    /// Get the items for this firing and start a run per item.
    fn collect_and_start(
        &self,
        hash: &str,
        trigger: &Trigger,
        state: &TriggerState,
        now: i64,
        opts: &EvalOptions,
    ) -> Result<FireOutcome> {
        let mut outcome = FireOutcome::default();

        let items: Vec<PollItem> = match trigger.kind {
            TriggerKind::Polling => {
                let response = self.poll(hash, trigger, state, opts)?;
                outcome.cursor = response.cursor.clone();
                outcome.draining = response.more;
                // First contact seeds the cursor and fires nothing. Otherwise
                // declaring a mailbox trigger starts a run for every message in
                // history — Zapier's priming poll, and for the same reason.
                if state.cursor.is_none() {
                    outcome.seeded = true;
                    return Ok(outcome);
                }
                response.items
            }
            // A clocked trigger with no external source fires once per
            // occurrence, carrying the occurrence itself as the item.
            TriggerKind::Interval | TriggerKind::Schedule | TriggerKind::Once => {
                vec![PollItem { id: now.to_string(), payload: serde_json::json!({ "at_ms": now }) }]
            }
            // Land in later phases; a declaration is accepted now so the
            // vocabulary is stable, but nothing fires it yet.
            TriggerKind::Memory | TriggerKind::Webhook | TriggerKind::Manual
            | TriggerKind::Composite => Vec::new(),
        };

        outcome.items = items.len();
        for item in items {
            let Some(value) = dedup_value(&item, &trigger.dedup_key) else {
                outcome.unidentifiable += 1;
                continue;
            };
            let run_id = run_id_for(hash, trigger.connector.as_deref(), &value);
            match self.start_run(trigger, &run_id, &item, hash) {
                StartOutcome::Started => outcome.runs_started += 1,
                StartOutcome::Duplicate => outcome.duplicates += 1,
                StartOutcome::Failed(why) => outcome.failures.push(why),
            }
        }
        Ok(outcome)
    }

    fn poll(
        &self,
        hash: &str,
        trigger: &Trigger,
        state: &TriggerState,
        opts: &EvalOptions,
    ) -> Result<PollResponse> {
        let connector_name = trigger.connector.as_deref().unwrap_or_default();
        let Some(exec) = &self.connector else {
            return Err(TriggerError::NoConnector {
                trigger: hash.to_string(),
                connector: connector_name.to_string(),
            });
        };
        let request = PollRequest {
            trigger: hash,
            connector: connector_name,
            scope: trigger.scope.as_deref(),
            cursor: state.cursor.as_deref(),
            max_items: opts.max_items,
            config: trigger.config.as_ref(),
        };
        let payload = serde_json::to_value(&request).map_err(|e| TriggerError::ConnectorFailed {
            trigger: hash.to_string(),
            detail: format!("serialize request: {e}"),
        })?;
        // The idempotency key ties this call to this occurrence, so a connector
        // that deduplicates on it sees a retry as a retry.
        let idem = format!("{hash}:{}", state.fence);
        match exec.execute(connector_name, hash, &payload, &idem) {
            areev_run::ExecResult::Ok(v) => {
                serde_json::from_value(v).map_err(|e| TriggerError::ConnectorFailed {
                    trigger: hash.to_string(),
                    detail: format!("response is not a poll response: {e}"),
                })
            }
            areev_run::ExecResult::Err { detail, .. } => {
                Err(TriggerError::ConnectorFailed { trigger: hash.to_string(), detail })
            }
        }
    }

    /// Record the item as an Event and start the bound workflow.
    fn start_run(
        &self,
        trigger: &Trigger,
        run_id: &str,
        item: &PollItem,
        trigger_hash: &str,
    ) -> StartOutcome {
        let mut event = Event::new(&item.payload.to_string())
            .namespace(&self.ns)
            .extra_field("trigger", serde_json::json!(trigger_hash))
            .extra_field("run_id", serde_json::json!(run_id));
        if let Some(c) = &trigger.connector {
            event = event.extra_field("connector", serde_json::json!(c));
        }
        if let Err(e) = self.facade.with_store(|m| m.add(&event)) {
            return StartOutcome::Failed(format!("ingest: {e}"));
        }
        let Some(starter) = &self.starter else {
            // Ingest-only: the item is recorded, nothing is executed.
            return StartOutcome::Started;
        };
        let input = serde_json::json!({
            "trigger": trigger_hash,
            "connector": trigger.connector,
            "scope": trigger.scope,
            "item": item.payload,
        });
        match starter.start(&trigger.workflow, run_id, input) {
            StartResult::Started => StartOutcome::Started,
            StartResult::Duplicate => StartOutcome::Duplicate,
            StartResult::Failed(why) => StartOutcome::Failed(why),
        }
    }

    /// One Observation per firing: what fired, what it produced, what it
    /// skipped. An unfired trigger is invisible otherwise.
    fn journal(
        &self,
        hash: &str,
        trigger: &Trigger,
        outcome: &FireOutcome,
        note: Option<&str>,
        now: i64,
    ) -> Result<()> {
        let mut obs = Observation::new("areev:trigger", "trigger:firing")
            .namespace(TRIGGER_NS)
            .created_at(now)
            .extra_field("trigger", serde_json::json!(hash))
            .extra_field("kind", serde_json::json!(trigger.kind.as_str()))
            .extra_field("workflow", serde_json::json!(trigger.workflow))
            .extra_field("items", serde_json::json!(outcome.items))
            .extra_field("runs_started", serde_json::json!(outcome.runs_started))
            .extra_field("duplicates", serde_json::json!(outcome.duplicates));
        if outcome.seeded {
            obs = obs.extra_field("seeded", serde_json::json!(true));
        }
        if let Some(n) = note {
            obs = obs.extra_field("note", serde_json::json!(n));
        }
        self.facade
            .with_store(|m| m.add_if_novel(&obs))
            .map(|_| ())
            .map_err(|e| TriggerError::Storage { detail: e.to_string() })
    }
}

#[derive(Debug, Default, Clone)]
struct FireOutcome {
    items: usize,
    runs_started: usize,
    duplicates: usize,
    unidentifiable: usize,
    failures: Vec<String>,
    cursor: Option<String>,
    draining: bool,
    seeded: bool,
    skipped_locked: bool,
}

enum StartOutcome {
    Started,
    Duplicate,
    Failed(String),
}

/// The run id for one firing of one item.
///
/// Derived from `(trigger, connector, dedup value)`, so the same item seen
/// twice — connector replay, overlapping cursors, two nodes racing a lease
/// boundary — produces the same id. `areev run start` refuses a duplicate, so
/// the second attempt is a recorded skip rather than a second run. At-most-once
/// firing with no lease, no fencing token, and no clock assumption.
///
/// The connector is part of the identity because CloudEvents made that
/// normative for the same reason: `source` + `id` is unique, `id` alone is only
/// unique within a producer. Two connectors sharing an id space must not
/// collide.
pub fn run_id_for(trigger_hash: &str, connector: Option<&str>, dedup_value: &str) -> String {
    let mut h = Sha256::new();
    h.update(b"areev-trigger:v1");
    h.update([0x1f]);
    h.update(connector.unwrap_or_default().as_bytes());
    h.update([0x1f]);
    h.update(dedup_value.as_bytes());
    let digest = hex(&h.finalize());
    format!("{}-{}", &trigger_hash[..trigger_hash.len().min(8)], &digest[..12])
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_run_id_is_stable_for_the_same_item() {
        let a = run_id_for("abcdef1234", Some("gmail"), "m1");
        let b = run_id_for("abcdef1234", Some("gmail"), "m1");
        assert_eq!(a, b, "the same item must always mint the same id");
    }

    #[test]
    fn different_items_get_different_ids() {
        let a = run_id_for("abcdef1234", Some("gmail"), "m1");
        let b = run_id_for("abcdef1234", Some("gmail"), "m2");
        assert_ne!(a, b);
    }

    #[test]
    fn the_connector_is_part_of_the_identity() {
        // CloudEvents' rule: `id` alone is unique only within a producer, so two
        // connectors with a shared id space must not collide.
        let a = run_id_for("abcdef1234", Some("gmail"), "1");
        let b = run_id_for("abcdef1234", Some("stripe"), "1");
        assert_ne!(a, b);
    }

    #[test]
    fn the_id_names_its_trigger_for_a_human_reading_a_run_list() {
        let id = run_id_for("abcdef1234567890", Some("gmail"), "m1");
        assert!(id.starts_with("abcdef12"), "{id}");
        assert!(id.len() < 32, "short enough to read: {id}");
    }

    #[test]
    fn a_short_trigger_hash_does_not_panic() {
        let id = run_id_for("ab", None, "x");
        assert!(id.starts_with("ab-"));
    }
}
