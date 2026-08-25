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

use std::collections::HashMap;
use std::sync::Arc;

use areev_cal::AreevFacade;
use areev_core::error::{AreevError, Hash};
use areev_core::types::{ContentRef, Event, Grain, GrainType, Observation, Trigger, TriggerKind};
use areev_run::HostToolExecutor;
use areev_store::TriggerState;
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::clock::Clock;
use crate::connector::{
    dedup_value, rewrite_blob_refs, PollItem, PollRequest, PollResponse,
    MAX_BLOB_BYTES_PER_ITEM, MAX_BLOB_BYTES_PER_RESPONSE,
};
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
    /// Decoded-size budget for one item's connector blobs (#93). Overrun
    /// refuses the whole poll (TRG-E011) rather than truncating.
    pub max_blob_bytes_per_item: usize,
    /// Decoded-size budget for one poll response's blobs (#93).
    pub max_blob_bytes_per_response: usize,
}

impl Default for EvalOptions {
    fn default() -> Self {
        EvalOptions {
            only: None,
            lease: std::time::Duration::from_secs(300),
            max_items: 100,
            dry_run: false,
            node: default_node_id(),
            max_blob_bytes_per_item: MAX_BLOB_BYTES_PER_ITEM,
            max_blob_bytes_per_response: MAX_BLOB_BYTES_PER_RESPONSE,
        }
    }
}

/// First `n` characters, safely — `&s[..n]` panics on a short string or a
/// multi-byte boundary, and a message-formatting helper must not be what takes
/// the process down. Declarations can arrive by bundle import from an
/// implementation we did not write.
fn short(s: &str, n: usize) -> &str {
    match s.char_indices().nth(n) {
        Some((idx, _)) => &s[..idx],
        None => s,
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
    /// Items recorded without starting a workflow, because no runtime is wired
    /// in. Counted apart from `runs_started` so the report never claims a run
    /// happened when none did.
    pub ingested: usize,
    /// Items the connector returned that carry no usable identity, so no run
    /// could be minted for them. Reported rather than silently dropped: an item
    /// nobody can name is a connector bug, and it should be visible as one.
    pub unidentifiable: usize,
    /// Declarations that cannot fire as written — a cron that does not parse, a
    /// timezone this build refuses, a composite gate naming a member the
    /// declaration does not carry.
    ///
    /// Counted APART from `skipped_not_due` on purpose. Folded in there, an
    /// unusable trigger is indistinguishable from a healthy one waiting its
    /// turn, which is the failure mode with no symptom: the work simply never
    /// happens and every report looks green. The reason lands in `errors`.
    pub unusable: usize,
    /// Not fatal to the pass: one broken trigger must not stop the others.
    pub errors: Vec<String>,
    /// A connector reported a backlog, so this trigger is due again at once
    /// rather than after its interval.
    pub draining: Vec<String>,
    /// Triggers whose cursor was deliberately left unmoved this pass because
    /// at least one item in the firing failed to start a run (#129) — the
    /// Event add failed, declared context was unavailable, or the run
    /// itself refused. The same page is offered again next tick; whatever
    /// already started comes back as a `duplicates` count rather than
    /// double-processing, which is what makes re-polling safe — see #128's
    /// stable run ids, a prerequisite for this.
    pub cursor_held: Vec<String>,
}

/// One trigger considered, for `--dry-run` and `status`.
#[derive(Debug, Clone, Serialize)]
pub struct TriggerStatus {
    pub trigger: String,
    /// The declaration's `name`, when it carries one.
    ///
    /// `name` is what a human uses to identify a trigger, and it is accepted
    /// on write — but nothing read it back (#73), so identity fell onto the
    /// workflow hash, which is stable only until the plan is re-declared. It
    /// rides in `extra_fields` rather than being a typed field: OMS §A.7 has
    /// no Trigger `name`, and inventing one would be a spec-level decision
    /// for a label.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
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
    /// Will never fire again — a one-shot past its instant.
    pub exhausted: bool,
    /// Never fired and no failure recorded — the state an unnoticed
    /// misconfiguration sits in, so it is reported rather than inferred.
    pub never_fired: bool,
    /// Why this declaration can never fire, if it cannot.
    ///
    /// A trigger can become unusable without ever being written through a
    /// validating path — it can arrive by bundle import from an implementation
    /// that validated differently, or predate the check. So the evaluator
    /// reports it rather than assuming the write path caught everything.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unusable: Option<String>,
    /// The connector's last reported position, for a trigger that has ever
    /// polled. Opaque and connector-defined (#128) — surfaced so an operator
    /// can SEE the state that a re-pointed trigger's evaluation carries
    /// forward, which before this was invisible even though nothing else
    /// could carry it by hand.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
    /// The op-log position, a `memory` trigger's cursor equivalent.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub op_cursor: Option<i64>,
    /// The hash evaluation state is actually keyed under: `trg:<chain_root>`
    /// (#128). Equal to `trigger` for a trigger that has never been
    /// superseded — the common case, unchanged from before this field
    /// existed. Differs only for a trigger re-pointed by `SUPERSEDE`, where
    /// it names the ORIGINAL declaration's hash — the identity the cursor
    /// and dedup fence actually survive under.
    pub chain_root: String,
}

/// The declaration's human label, if it has one.
///
/// Written through `extra_fields` by every surface that accepts `name` on a
/// trigger, and read back here so `list` and `status` can print it (#73).
/// Blank is treated as absent: a name that renders as nothing is worse than
/// none, because it displaces the hash a reader could otherwise have used.
pub fn trigger_name(trigger: &Trigger) -> Option<String> {
    trigger
        .common
        .extra_fields
        .get("name")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
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
    /// Credentials the broker may attach, by name. Values live here and never
    /// in a grain or a connector's environment: a declaration names a
    /// credential, it never carries one.
    /// A SOURCE per name, not a resolved value (#113): the heartbeat is the
    /// path that most needs short-lived credentials, because it is the one
    /// nobody is watching. An env-var source resolves once, as before; a
    /// `cmd:`/`vault:` source is minted per TTL window inside the broker, so a
    /// cadence that outlives a token's hour does not start 401'ing overnight.
    pub credentials: std::collections::BTreeMap<String, areev_run::CredentialSource>,
    pub ns: String,
    pub principal: String,
}

impl Evaluator {
    /// An evaluator that can inspect but not act: no connector, no runtime, no
    /// credentials. What `list`, `show` and `status` want, and the shape that
    /// makes "reading cannot fire anything" true by construction rather than by
    /// remembering to pass `None` four times.
    pub fn read_only(facade: Arc<AreevFacade>, clock: Arc<dyn Clock>, ns: &str) -> Evaluator {
        Evaluator {
            facade,
            clock,
            connector: None,
            starter: None,
            credentials: Default::default(),
            ns: ns.to_string(),
            principal: "user:local".into(),
        }
    }

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
                Ok(mut t) => {
                    // `to_trigger` reconstructs the typed fields; `extra_fields`
                    // is not restored for any grain type, so the declaration's
                    // `name` — an extra field by design, since OMS §A.7 has no
                    // Trigger `name` — has to be carried over here. Doing it in
                    // the shared deserializer would silently change what every
                    // read-modify-write re-serializes, which is the kind of
                    // change that moves content addresses.
                    if let Some(name) = g.get_str("name") {
                        t.common
                            .extra_fields
                            .insert("name".into(), serde_json::json!(name));
                    }
                    out.push((hash, t))
                }
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
        // One cache for this call, same as a `run()` pass — a chain is
        // walked at most once even though `declarations()` above may list
        // the same workflow's triggers repeatedly.
        let mut chains: HashMap<String, Chain> = HashMap::new();
        for (hash, t) in self.declarations()? {
            let chain = self.resolve_chain(&hash, &mut chains)?;
            // Read-only: `adopt = false`. `trigger status`/`show` must not
            // have the side effect of migrating a legacy state row — a real
            // evaluation pass (`run`/`deliver`) does that; this only
            // reports whatever it finds, root row or legacy.
            let (st, _raw) = self.resolve_state(&chain, false)?;
            let unusable = schedule::validate(&t).err().map(|e| e.to_string());
            out.push(TriggerStatus {
                trigger: hash,
                name: trigger_name(&t),
                kind: t.kind.as_str().to_string(),
                workflow: t.workflow.clone(),
                enabled: t.enabled,
                paused: st.paused,
                // An unusable declaration is never due. Saying otherwise would
                // promise a firing that cannot happen.
                due: unusable.is_none()
                    && t.enabled
                    && !st.leased(now)
                    && schedule::is_due(&t, &st, now).unwrap_or(false),
                leased_by: st.leased(now).then(|| st.claimed_by.clone()).flatten(),
                next_due_at: st.next_due_at,
                last_fired_at: st.last_fired_at,
                consecutive_failures: st.consecutive_failures,
                last_error: st.last_error.clone(),
                exhausted: st.exhausted,
                never_fired: st.last_fired_at.is_none() && st.consecutive_failures == 0,
                unusable,
                cursor: st.cursor.clone(),
                op_cursor: st.op_cursor,
                chain_root: chain.root().to_string(),
            });
        }
        Ok(out)
    }

    /// Ingest a payload delivered by the host for a `webhook` or `manual`
    /// trigger.
    ///
    /// Areev never opens a port. The host already terminates TLS and
    /// authenticates the sender — it is far better at both than a memory engine
    /// would be — and hands the payload to a one-shot invocation. That keeps the
    /// useful half of webhooks without a listening daemon.
    ///
    /// Idempotent on the same terms as a poll: the payload's dedup value mints
    /// the run id, so a webhook delivered twice (which every provider does)
    /// produces one run and one recorded skip.
    pub fn deliver(&self, trigger_hash: &str, payload: serde_json::Value) -> Result<EvalReport> {
        let now = self.clock.now_ms();
        let (hash, trigger) = self
            .declarations()?
            .into_iter()
            .find(|(h, _)| h.starts_with(trigger_hash))
            .ok_or_else(|| TriggerError::Malformed {
                what: format!("no trigger matching '{trigger_hash}'"),
            })?;

        if !matches!(trigger.kind, TriggerKind::Webhook | TriggerKind::Manual) {
            return Err(TriggerError::Malformed {
                what: format!(
                    "trigger {} is a {} trigger — only webhook and manual triggers accept a \
                     delivery; a clocked or polling trigger fires from `trigger run`",
                    short(&hash, 12),
                    trigger.kind.as_str()
                ),
            });
        }
        if !trigger.enabled {
            return Err(TriggerError::Malformed {
                what: format!("trigger {} is disabled", short(&hash, 12)),
            });
        }
        // #128 — a single-trigger cache: `deliver` fires exactly one
        // trigger, so this walks its chain at most once regardless of how
        // many places below need the root.
        let mut chains: HashMap<String, Chain> = HashMap::new();
        let chain = self.resolve_chain(&hash, &mut chains)?;
        let (state, _raw) = self.resolve_state(&chain, true)?;
        if state.paused {
            return Err(TriggerError::Malformed {
                what: format!("trigger {} is paused", short(&hash, 12)),
            });
        }

        let mut report = EvalReport::default();
        let item = PollItem { id: String::new(), payload, blobs: Vec::new() };
        report.items = 1;
        let Some(value) = dedup_value(&item, &trigger.dedup_key) else {
            report.unidentifiable = 1;
            // Journal before returning. A delivery naming nothing is precisely
            // the case the record exists for: the sender's payload shape
            // changed under us, and stdout of whatever invoked this is not an
            // audit record -- the replicating Observation is.
            self.journal(
                &hash,
                &trigger,
                &FireOutcome { items: 1, unidentifiable: 1, ..Default::default() },
                Some("delivered"),
                now,
            )?;
            return Ok(report);
        };
        // Keyed on the chain ROOT (#128), so a delivery replayed after the
        // trigger has been re-pointed by `SUPERSEDE` still mints the same id
        // it always would have. `legacy_run_ids` covers a trigger already
        // superseded before this fix — the id it would have minted under
        // its old (then-current) head.
        let root = chain.root();
        let run_id = run_id_for(root, trigger.connector.as_deref(), &value);
        let legacy_run_ids: Vec<String> = chain
            .legacy()
            .iter()
            .map(|h| run_id_for(h, trigger.connector.as_deref(), &value))
            .collect();
        match self.start_run(&trigger, &run_id, &legacy_run_ids, &item, &hash, &[]) {
            StartOutcome::Started => report.runs_started = 1,
            StartOutcome::Ingested => report.ingested = 1,
            StartOutcome::Duplicate => report.duplicates = 1,
            StartOutcome::Failed(why) => report.errors.push(why),
        }

        let outcome = FireOutcome {
            items: 1,
            runs_started: report.runs_started,
            duplicates: report.duplicates,
            ..Default::default()
        };
        self.journal(&hash, &trigger, &outcome, Some("delivered"), now)?;
        Ok(report)
    }

    /// Run one evaluation pass.
    pub fn run(&self, opts: &EvalOptions) -> Result<EvalReport> {
        let mut report = EvalReport::default();
        let now = self.clock.now_ms();
        // #128 — resolved once per trigger per PASS, never once per item:
        // every chain walk below goes through this cache, shared with
        // `record_for_composites` (a composite may be touched by several
        // members firing in the same pass).
        let mut chains: HashMap<String, Chain> = HashMap::new();

        let declarations = self.declarations()?;
        // Composites are settled after their members, so a gate can be
        // satisfied in the same pass that completes it rather than waiting a
        // whole heartbeat. Declaration order is newest-first and says nothing
        // about dependency, so the split is explicit.
        let (composites, members): (Vec<_>, Vec<_>) =
            declarations.into_iter().partition(|(_, t)| t.kind == TriggerKind::Composite);

        for (hash, trigger) in members.into_iter().chain(composites) {
            if opts.only.as_ref().is_some_and(|only| only != &hash) {
                continue;
            }
            if !trigger.enabled {
                report.skipped_not_due += 1;
                continue;
            }
            // Checked BEFORE dueness, so an unusable declaration is reported as
            // what it is rather than disappearing into `not due` — where it
            // looks exactly like a healthy trigger waiting its turn.
            if let Err(why) = schedule::validate(&trigger) {
                report.unusable += 1;
                report.errors.push(format!("{}: {why}", short(&hash, 12)));
                continue;
            }
            // #128 — every read/write of this trigger's evaluation state
            // below is keyed on the chain ROOT, not on `hash` (the
            // declaration's current head): a trigger re-pointed by
            // `SUPERSEDE` mints a new head for the same standing rule, and
            // keying state on the head alone would orphan the cursor and
            // the dedup fence on every applied recommendation. For a
            // never-superseded trigger root == hash, so this resolves to
            // exactly the lookup this crate always did — a byte-identical
            // no-op (pinned by `a_never_superseded_trigger_keys_state_on_its_own_hash`).
            // `adopt = true`: this is a real evaluation pass with write
            // access, so a legacy (pre-#128, head-keyed) row found along the
            // way is migrated under the root.
            let chain = self.resolve_chain(&hash, &mut chains)?;
            let (state, raw) = self.resolve_state(&chain, true)?;

            if state.paused {
                report.skipped_paused += 1;
                continue;
            }
            // An absolute schedule seen for the first time: record when it
            // will be due rather than re-deriving it every pass, so `trigger
            // status` can say when it next fires and the comparison afterwards
            // is a plain timestamp check.
            if state.next_due_at.is_none() && !state.exhausted {
                if let Some(first) = schedule::initial_due(&trigger, now)? {
                    let mut seeded = state.clone();
                    seeded.next_due_at = Some(first);
                    let _ = self
                        .facade
                        .with_store(|m| m.put_trigger_state(chain.root(), raw.as_deref(), &seeded))
                        .map_err(|e| TriggerError::Storage { detail: e.to_string() })?;
                    report.skipped_not_due += 1;
                    continue;
                }
            }
            if !schedule::is_due(&trigger, &state, now)? {
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

            match self.fire(&hash, &chain, &trigger, state, raw.as_deref(), now, opts) {
                // Lost the claim between deciding it was free and taking it.
                // Normal with several nodes on one heartbeat, and it must not
                // be counted as work done.
                Ok(outcome) if outcome.skipped_locked => report.skipped_locked += 1,
                Ok(outcome) => {
                    if !outcome.fired_items.is_empty() {
                        if let Err(e) =
                            self.record_for_composites(&hash, &outcome.fired_items, now, &mut chains)
                        {
                            report.errors.push(e.to_string());
                        }
                    }
                    report.claimed += 1;
                    report.items += outcome.items;
                    report.duplicates += outcome.duplicates;
                    report.runs_started += outcome.runs_started;
                    report.ingested += outcome.ingested;
                    report.unidentifiable += outcome.unidentifiable;
                    for why in &outcome.failures {
                        report.errors.push(format!("{hash}: {why}"));
                    }
                    // #129 — any start failure in this firing means the
                    // cursor was deliberately left unmoved (see `fire`'s
                    // release logic); surfaced apart from `errors` so an
                    // operator sees WHY the same items come back next tick
                    // rather than inferring it.
                    if !outcome.failures.is_empty() {
                        report.cursor_held.push(hash.clone());
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

    // `chain` (#128) is one more independent knob on an already knob-heavy
    // internal call, in the same spirit as the other `too_many_arguments`
    // allows in this codebase (areev-store's tuned-recall internals, the
    // bindings' flat FFI surfaces) — splitting it into a struct would not
    // make any single call site more readable.
    #[allow(clippy::too_many_arguments)]
    fn fire(
        &self,
        hash: &str,
        chain: &Chain,
        trigger: &Trigger,
        state: TriggerState,
        raw: Option<&str>,
        now: i64,
        opts: &EvalOptions,
    ) -> Result<FireOutcome> {
        // #128 — the claim/release CAS below is keyed on the chain ROOT, not
        // `hash`. This is the hazard the fix has to get right: `fire` claims
        // with one key and releases with the same key, so if the two ever
        // disagreed about which key that is, the release's compare-and-swap
        // would find no row, `released_ok` would be false, and every firing
        // of a superseded trigger would permanently report `ClaimLost` — a
        // trigger that could never usefully claim again. `root` is resolved
        // exactly once here and reused for every claim/release call in this
        // function.
        let root = chain.root();

        // Claim. Losing means another evaluator got here first.
        let mut claimed = state.clone();
        claimed.fence = claimed.fence.wrapping_add(1);
        claimed.claimed_by = Some(opts.node.clone());
        claimed.lease_until = Some(now + opts.lease.as_millis() as i64);
        let won = self
            .facade
            .with_store(|m| m.put_trigger_state(root, raw, &claimed))
            .map_err(|e| TriggerError::Storage { detail: e.to_string() })?;
        if !won {
            return Ok(FireOutcome { skipped_locked: true, ..Default::default() });
        }
        // What a later release must present as `expected`.
        let claimed_raw = self
            .facade
            .with_store(|m| m.trigger_state(root))
            .map_err(|e| TriggerError::Storage { detail: e.to_string() })?
            .map(|(_, r)| r);

        let result = self.collect_and_start(hash, chain, trigger, &claimed, now, opts);

        // Release under the fence, whatever happened.
        let mut released = claimed.clone();
        released.claimed_by = None;
        released.lease_until = None;
        match &result {
            // Full success: every item that resolved an identity started (or
            // was ingested, or was recognized as a duplicate) cleanly.
            Ok(outcome) if outcome.failures.is_empty() => {
                released.last_fired_at = Some(now);
                released.consecutive_failures = 0;
                released.last_error = None;
                if let Some(c) = &outcome.cursor {
                    released.cursor = Some(c.clone());
                }
                if let Some(c) = outcome.op_cursor {
                    released.op_cursor = Some(c);
                }
                // A satisfied gate consumes its correlation key. Leaving it
                // would re-fire the same set on every pass; Flink calls this
                // the after-match skip strategy, and the conservative choice —
                // clear only the key that fired — is the one that cannot
                // swallow an unrelated in-flight match.
                for k in &outcome.settled_keys {
                    released.partials.remove(k);
                }
                prune_partials(&mut released, trigger, now);
                released.exhausted = schedule::exhausts_after(trigger, now);
                released.next_due_at = if outcome.draining {
                    // A backlog drains at once rather than waiting out the
                    // interval — a cold start should not take a day.
                    Some(now)
                } else {
                    schedule::advance_after_firing(trigger, now, now)?
                };
            }
            // #129 — at least one item failed to start a run: the Event add
            // failed, declared context was unavailable, or the run itself
            // refused (RUN-E018 and friends). All three mean the item was
            // NOT processed, so this firing must not report itself clean —
            // that is the opposite posture from the Err arm below, which
            // this deliberately mirrors:
            //   - the cursor / op_cursor are left at `claimed`'s values
            //     (never assigned from `outcome`), so the next pass re-asks
            //     the connector for the same page instead of skipping past
            //     unprocessed work;
            //   - `exhausted` is left at `claimed.exhausted` (false, since a
            //     due trigger cannot already be exhausted) rather than
            //     recomputed — a `once` trigger that fails must be allowed
            //     to retry, not be marked as having had its one shot;
            //   - `settled_keys` are NOT removed from `partials` — a
            //     satisfied composite gate whose run refused must stay
            //     satisfied so it retries, not lose its correlation.
            // Re-polling the same page is safe BECAUSE run ids are keyed on
            // the chain root (#128): whatever already started is protected
            // by `start_run`'s dedup fence and comes back as `Duplicate`,
            // never a second run. What DID happen is still real —
            // `runs_started`/`ingested`/`duplicates` in the outcome are not
            // discarded, only the durable cursor advance is withheld.
            Ok(outcome) => {
                released.last_fired_at = Some(now);
                released.consecutive_failures = claimed.consecutive_failures.saturating_add(1);
                released.last_error = Some(summarize_failures(&outcome.failures));
                released.next_due_at =
                    Some(schedule::backoff_until(trigger, released.consecutive_failures, now));
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
            .with_store(|m| m.put_trigger_state(root, claimed_raw.as_deref(), &released))
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
        chain: &Chain,
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
                vec![PollItem {
                    id: now.to_string(),
                    payload: serde_json::json!({ "at_ms": now }),
                    blobs: Vec::new(),
                }]
            }
            TriggerKind::Memory => {
                // Recorded so the journal says "seeded" rather than leaving a
                // firing that produced nothing looking like a quiet memory.
                outcome.seeded = state.op_cursor.is_none();
                let (found, cursor) = self.scan_memory(hash, trigger, state, opts)?;
                outcome.op_cursor = Some(cursor);
                found
            }
            TriggerKind::Composite => {
                let keys = self.satisfied_keys(hash, chain, trigger, now)?;
                outcome.settled_keys = keys.clone();
                keys.into_iter()
                    .map(|k| PollItem {
                        // The correlation value IS the identity: one firing per
                        // correlated set, however many members contributed.
                        id: k.clone(),
                        payload: serde_json::json!({ "correlation": k }),
                        blobs: Vec::new(),
                    })
                    .collect()
            }
            // These do not poll. The host owns the listener and hands the
            // payload to `deliver`, which fires them on the same idempotency
            // terms; an evaluation pass simply has nothing to do for one.
            TriggerKind::Webhook | TriggerKind::Manual => Vec::new(),
        };

        // #93 — store connector blobs in the CAS and rewrite `"blob": "@N"`
        // payload references to their addresses BEFORE identity resolution,
        // so a `--dedup-key` pointer into a rewritten field sees the stable
        // content address rather than a positional index. A contract
        // violation fails the whole poll here (TRG-E011): fire() then leaves
        // the cursor unmoved, so nothing is lost — the connector gets fixed
        // and the same page is re-polled.
        let mut items = items;
        let item_refs = self.ingest_item_blobs(hash, &mut items, opts)?;

        outcome.items = items.len();
        for (item, refs) in items.into_iter().zip(item_refs) {
            let Some(value) = dedup_value(&item, &trigger.dedup_key) else {
                outcome.unidentifiable += 1;
                continue;
            };
            outcome.fired_items.push(item.payload.clone());
            // Keyed on the chain ROOT (#128): the same item seen again after
            // this trigger has been re-pointed by `SUPERSEDE` still mints
            // the id it always would have, so `start_run`'s dedup fence
            // still recognizes it. `legacy_run_ids` is the compatibility
            // path for a trigger already superseded BEFORE this fix — the
            // id(s) it would have minted under an older head — and is empty
            // (so free) for the common never-superseded case.
            let root = chain.root();
            let run_id = run_id_for(root, trigger.connector.as_deref(), &value);
            let legacy_run_ids: Vec<String> = chain
                .legacy()
                .iter()
                .map(|h| run_id_for(h, trigger.connector.as_deref(), &value))
                .collect();
            match self.start_run(trigger, &run_id, &legacy_run_ids, &item, hash, &refs) {
                StartOutcome::Started => outcome.runs_started += 1,
                StartOutcome::Ingested => outcome.ingested += 1,
                StartOutcome::Duplicate => outcome.duplicates += 1,
                StartOutcome::Failed(why) => outcome.failures.push(why),
            }
        }
        Ok(outcome)
    }

    /// Note a member firing against every composite that declares it.
    ///
    /// The correlation value comes from the composite's own `correlate`
    /// pointer applied to the member's item — so two composites may correlate
    /// the same member on different fields, which is why this is resolved per
    /// composite rather than once per member.
    fn record_for_composites(
        &self,
        member: &str,
        items: &[serde_json::Value],
        now: i64,
        chains: &mut HashMap<String, Chain>,
    ) -> Result<()> {
        for (chash, composite) in self.declarations()? {
            let Some(alias) = composite.member_alias(member).map(str::to_string) else {
                continue;
            };
            if composite.kind != TriggerKind::Composite {
                continue;
            }
            // #128 — the composite's OWN chain, not the member's: state
            // lives under the composite's root regardless of which member
            // just fired. Shares the pass-wide cache, so a composite with
            // several members touched this pass walks its chain once.
            let chain = self.resolve_chain(&chash, chains)?;
            let (mut state, raw) = self.resolve_state(&chain, true)?;

            for item in items {
                // No correlate pointer means one global match — every member
                // firing counts toward the same gate.
                let key = match &composite.correlate {
                    Some(ptr) => match item.pointer(ptr) {
                        Some(serde_json::Value::String(s)) => s.clone(),
                        Some(other) => other.to_string(),
                        // An item that cannot be correlated cannot join a
                        // correlated gate. Dropping it is right: attributing it
                        // to an arbitrary key would pair unrelated work.
                        None => continue,
                    },
                    None => String::new(),
                };
                let entry = state.partials.entry(key).or_insert_with(|| {
                    areev_store::PartialMatch { members: Default::default(), started_ms: now }
                });
                // Recorded under the ALIAS, because that is what the gate
                // expression names.
                entry.members.insert(alias.clone(), now);
            }
            prune_partials(&mut state, &composite, now);
            self.facade
                .with_store(|m| m.put_trigger_state(chain.root(), raw.as_deref(), &state))
                .map_err(|e| TriggerError::Storage { detail: e.to_string() })?;
        }
        Ok(())
    }

    /// Which correlation keys currently satisfy this composite's gate.
    fn satisfied_keys(
        &self,
        hash: &str,
        chain: &Chain,
        trigger: &Trigger,
        now: i64,
    ) -> Result<Vec<String>> {
        let predicate = trigger
            .predicate
            .as_ref()
            .ok_or_else(|| TriggerError::Malformed {
                what: format!("composite trigger {hash} has no predicate"),
            })
            .and_then(crate::predicate::from_value)?;

        // Refuse a gate that names a member the declaration does not carry: it
        // could never be satisfied, so the trigger would be silently dead.
        for referenced in crate::predicate::referenced_members(&predicate) {
            if !trigger.members.contains_key(&referenced) {
                return Err(TriggerError::UnknownMember {
                    trigger: hash.to_string(),
                    member: referenced,
                });
            }
        }

        // #128 — this composite's own partials live under its chain root.
        // No adoption dance needed here: by the time `fire` reaches
        // `collect_and_start` for this trigger, `run()`'s loop has already
        // resolved (and, if needed, adopted) this exact root's state row.
        let (state, _) = self
            .facade
            .with_store(|m| m.trigger_state(chain.root()))
            .map_err(|e| TriggerError::Storage { detail: e.to_string() })?
            .unwrap_or_default();

        let mut out = Vec::new();
        for (key, partial) in &state.partials {
            if let Some(w) = trigger.window_ms {
                if now - partial.started_ms > w {
                    continue;
                }
            }
            let fired: Vec<String> = partial.members.keys().cloned().collect();
            if crate::predicate::gate_satisfied(&predicate, &fired) {
                out.push(key.clone());
            }
        }
        Ok(out)
    }

    /// Grains matching the predicate that appeared since the stored op cursor.
    ///
    /// Built on the op-log rather than a scan: `changes_since` is an indexed
    /// primary-key range read with a monotonic exclusive cursor, so cost is
    /// proportional to what changed rather than to how much the memory holds.
    ///
    /// Considered and rejected: `PRAGMA data_version` as a cheap "did anything
    /// change" pre-check. It is documented as unchanged for commits made on the
    /// same connection — and Areev is single-writer with one connection, so our
    /// own writes are exactly what it cannot see.
    fn scan_memory(
        &self,
        hash: &str,
        trigger: &Trigger,
        state: &TriggerState,
        opts: &EvalOptions,
    ) -> Result<(Vec<PollItem>, i64)> {
        let predicate = trigger
            .predicate
            .as_ref()
            .ok_or_else(|| TriggerError::Malformed {
                what: format!("memory trigger {hash} has no predicate"),
            })
            .and_then(crate::predicate::from_value)?;

        // First contact seeds at the head of the log and matches nothing.
        // Otherwise declaring a memory trigger on an established memory fires
        // once for every historical grain that matches — the same reason a
        // polling trigger primes rather than replaying a mailbox.
        let Some(after) = state.op_cursor else {
            let head = self
                .facade
                .with_store(|m| m.head_op_seq())
                .map_err(|e| TriggerError::Storage { detail: e.to_string() })?;
            return Ok((Vec::new(), head));
        };
        let ops = self
            .facade
            .with_store(|m| m.changes_since(after, opts.max_items))
            .map_err(|e| TriggerError::Storage { detail: e.to_string() })?;

        let mut items = Vec::new();
        let mut cursor = after;
        for op in ops {
            cursor = cursor.max(op.op_seq);
            // Additions and supersessions both mean "this is true now".
            // A tombstone means the grain is gone, so there is nothing to match.
            if op.op == areev_store::OP_FORGET {
                continue;
            }
            let Ok(grain) = self.facade.with_store(|m| m.get(&op.hash)) else {
                // A grain that vanished between the op-log read and the fetch
                // (erasure, retention) is not an error — it simply no longer
                // matches anything.
                continue;
            };
            if !crate::predicate::grain_matches(&grain, &predicate) {
                continue;
            }
            let hex = op.hash.to_hex();
            items.push(PollItem {
                // The content address IS the identity: the same grain can never
                // fire twice, and two evaluators seeing it agree without
                // coordinating.
                id: hex.clone(),
                payload: serde_json::json!({
                    "hash": hex,
                    "grain_type": grain.grain_type.as_str(),
                    "op_seq": op.op_seq,
                }),
                blobs: Vec::new(),
            });
        }
        Ok((items, cursor))
    }

    /// Store an item's connector blobs in the CAS and rewrite payload
    /// references (#93). Returns one `ContentRef` list per item, parallel
    /// to `items`. Any contract violation — undecodable base64, a budget
    /// overrun, a dangling `"@N"` — refuses the WHOLE poll with TRG-E011,
    /// so the cursor stays put and nothing is silently dropped. `put_blob`
    /// is idempotent on content, so a re-polled page costs only the
    /// transfer.
    fn ingest_item_blobs(
        &self,
        hash: &str,
        items: &mut [PollItem],
        opts: &EvalOptions,
    ) -> Result<Vec<Vec<ContentRef>>> {
        let violation = |detail: String| TriggerError::BlobContract {
            trigger: short(hash, 12).to_string(),
            detail,
        };
        let mut response_total = 0usize;
        let mut all_refs = Vec::with_capacity(items.len());
        for (idx, item) in items.iter_mut().enumerate() {
            if item.blobs.is_empty() {
                all_refs.push(Vec::new());
                continue;
            }
            let mut item_total = 0usize;
            let mut refs = Vec::with_capacity(item.blobs.len());
            let mut uris = Vec::with_capacity(item.blobs.len());
            for (bi, blob) in item.blobs.iter().enumerate() {
                let name = blob.filename.as_deref().unwrap_or("unnamed");
                let bytes = areev_core::b64::decode(&blob.b64).ok_or_else(|| {
                    violation(format!("item {idx} blob {bi} ({name}) is not valid base64"))
                })?;
                item_total += bytes.len();
                response_total += bytes.len();
                if item_total > opts.max_blob_bytes_per_item {
                    return Err(violation(format!(
                        "item {idx} blobs exceed the per-item budget ({} bytes decoded)",
                        opts.max_blob_bytes_per_item
                    )));
                }
                if response_total > opts.max_blob_bytes_per_response {
                    return Err(violation(format!(
                        "response blobs exceed the per-response budget ({} bytes decoded)",
                        opts.max_blob_bytes_per_response
                    )));
                }
                let uri = self
                    .facade
                    .with_store(|m| m.put_blob(&bytes))
                    .map_err(|e| TriggerError::Storage { detail: e.to_string() })?;
                refs.push(ContentRef {
                    uri: uri.clone(),
                    modality: None,
                    mime_type: blob.mime.clone(),
                    size_bytes: Some(bytes.len() as u64),
                    checksum: Some(uri.trim_start_matches("cas://").to_string()),
                    metadata: blob.filename.as_ref().map(|f| serde_json::json!({ "filename": f })),
                });
                uris.push(uri);
            }
            rewrite_blob_refs(&mut item.payload, &uris)
                .map_err(|d| violation(format!("item {idx}: {d}")))?;
            // The bytes are in the CAS now; drop the base64 so nothing
            // downstream can re-inline it into the Event or the run input.
            item.blobs.clear();
            all_refs.push(refs);
        }
        Ok(all_refs)
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
        // Start a broker for this call, scoped to this trigger's allowlist. The
        // connector is handed its URL — never a token — so a compromised
        // connector has nothing to exfiltrate and nowhere undeclared to send
        // it. The broker stops when this call returns.
        let policy = areev_run::EgressPolicy::from_config(trigger.config.as_ref())
            .map_err(|what| TriggerError::Malformed { what })?;
        // One connector runs per pass, so there is no second caller to tell it
        // apart from: a default grant covers it. Its methods are whatever the
        // connector needs to poll, and the credentials are the ones this host
        // configured — naming one it was not given still fails.
        let grants = areev_run::EgressGrants::new().default_for_all(
            self.credentials.keys().fold(
                areev_run::CallerGrant::new()
                    .method("GET")
                    .method("POST")
                    .method("PUT")
                    .method("PATCH")
                    .method("DELETE"),
                |g, name| g.credential(name),
            ),
        );
        let broker = areev_run::Broker::start(
            policy,
            self.credentials.clone(),
            grants,
            "TRG-E009",
        )
        .map_err(|detail| TriggerError::Storage { detail })?;

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
        // The connector reads AREEV_EGRESS_URL out of its environment. The
        // spawn seam scrubs whatever `--passphrase-env` and `--token-env` name,
        // so this is the only network affordance it has from us.
        //
        // The variable is PROCESS-GLOBAL, and `HostToolExecutor::execute` has no
        // per-call environment, so two evaluators in one process would race:
        // one could hand its broker's URL to the other's connector, pointing it
        // at the wrong allowlist. The lock makes the set→spawn→unset window
        // exclusive. Production runs one evaluator per process, so this
        // serialises nothing that was parallel; it closes a hazard that only
        // exists in-process (and that our own threaded tests could reach).
        static EGRESS_ENV: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let result = {
            let _guard = EGRESS_ENV.lock().unwrap_or_else(|e| e.into_inner());
            std::env::set_var("AREEV_EGRESS_URL", broker.url());
            // The broker authenticates its callers: loopback is not an
            // authorization, and without the token every call would 401.
            if let Some(t) = broker.token_for(connector_name) {
                std::env::set_var("AREEV_EGRESS_TOKEN", t);
            }
            let r = exec.execute(connector_name, hash, &payload, &idem);
            std::env::remove_var("AREEV_EGRESS_URL");
            std::env::remove_var("AREEV_EGRESS_TOKEN");
            r
        };

        // Surface a refused destination as its own error rather than letting it
        // read as a generic connector failure — a connector reaching somewhere
        // undeclared is a different problem from one that crashed.
        let refused = broker.refusals();
        if !refused.is_empty() {
            return Err(TriggerError::EgressRefused {
                trigger: hash.to_string(),
                host: refused
                    .iter()
                    .map(|r| format!("{} ({})", r.destination, r.reason))
                    .collect::<Vec<_>>()
                    .join(", "),
            });
        }
        match result {
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
    ///
    /// `legacy_run_ids` (#128) covers a trigger that was superseded BEFORE
    /// this fix shipped, when the run id was minted from the (then-current)
    /// head rather than the chain root: each entry is the id this same item
    /// would have gotten under an older head. Empty for a never-superseded
    /// trigger, so it costs nothing in the common case.
    fn start_run(
        &self,
        trigger: &Trigger,
        run_id: &str,
        legacy_run_ids: &[String],
        item: &PollItem,
        trigger_hash: &str,
        content_refs: &[ContentRef],
    ) -> StartOutcome {
        // Idempotency has to hold whether or not a runtime is wired in. With a
        // starter, the runtime's duplicate-run-id refusal is the guard; without
        // one, nothing else would stop the same item being re-ingested on every
        // poll forever, because an Event's `created_at` differs per firing and
        // so content addressing does not collapse them.
        let seen = self
            .facade
            .with_store(|m| m.run_grains(&self.ns, run_id, 0, 1))
            .map(|g| !g.is_empty())
            .unwrap_or(false);
        if seen {
            return StartOutcome::Duplicate;
        }
        // #128 compat — an item already processed under a pre-fix
        // (head-keyed) run id must still be recognized, or a trigger
        // superseded before this fix reprocesses its whole backlog the
        // first time it is evaluated after upgrading.
        for legacy_id in legacy_run_ids {
            let seen_legacy = self
                .facade
                .with_store(|m| m.run_grains(&self.ns, legacy_id, 0, 1))
                .map(|g| !g.is_empty())
                .unwrap_or(false);
            if seen_legacy {
                return StartOutcome::Duplicate;
            }
        }

        // The ingest Event is built here but — #129 — WRITTEN only once the
        // outcome is one this dedup check should treat as "seen": Ingested
        // (no starter, so the Event itself IS the completed unit of work),
        // Started, or a starter-reported Duplicate (the runtime already has
        // it covered, possibly from elsewhere). A starter-reported `Failed`
        // writes NOTHING, so a retry's `seen` check above finds nothing and
        // genuinely retries `starter.start()` — before this, the Event was
        // always written first, so a failed start still left a `run_id`-
        // bearing grain behind that made every future retry of the SAME
        // item look like an already-processed duplicate, permanently
        // stranding it despite #129's cursor hold.
        let build_event = || {
            let mut event = Event::new(&item.payload.to_string())
                .namespace(&self.ns)
                .extra_field("trigger", serde_json::json!(trigger_hash))
                .extra_field("run_id", serde_json::json!(run_id));
            if let Some(c) = &trigger.connector {
                event = event.extra_field("connector", serde_json::json!(c));
            }
            // #93 — the item's stored blobs, referenced the same way a
            // host-driven ingest references them: through `content_refs`,
            // which is what keeps them alive through GC, carries them in
            // bundles, and lets erasure's sole-reference reclamation find
            // them.
            for cr in content_refs {
                event = event.content_ref(cr.clone());
            }
            event
        };

        let Some(starter) = &self.starter else {
            // Ingest-only: the item is recorded, nothing is executed. Reported
            // as ingested rather than as a run, because claiming a run started
            // when none did would make the report lie.
            return match self.facade.with_store(|m| m.add(&build_event())) {
                Ok(_) => StartOutcome::Ingested,
                Err(e) => StartOutcome::Failed(format!("ingest: {e}")),
            };
        };
        let mut input = serde_json::json!({
            "trigger": trigger_hash,
            "connector": trigger.connector,
            "scope": trigger.scope,
            "item": item.payload,
        });
        // Declared context (#85): the EVALUATOR assembles it, because it is
        // the one party that already holds the memory — on the embedded
        // backend a tool inside the run cannot open the file its own run
        // locks. Fail closed: a trigger that declared context must not fire
        // blind, so a missing query or a failed read refuses the firing (the
        // evaluator's normal retry/backoff applies) rather than starting a
        // run without the context its declaration promised.
        if let Some(spec) = &trigger.context_query {
            match self.assemble_context(spec, &item.payload) {
                Ok(ctx) => {
                    input["context"] = ctx;
                }
                Err(why) => {
                    return StartOutcome::Failed(format!("context_query \"{spec}\": {why}"));
                }
            }
        }
        // Read through whatever scheme prefix the declaration spelled the
        // reference with (#73): `sha256:<hex>` validated, listed, and reported
        // `waiting` forever, then died here on `FMT-E001: invalid hex hash`.
        match starter.start(trigger.workflow_hash(), run_id, input) {
            // The run is durably underway (or already was, started elsewhere)
            // before we ever try to write the ingest Event, so a failure to
            // write it here does not need to be reported as a failed firing:
            // a retry would find `starter.start()` itself now reporting
            // `Duplicate` (the runtime's own dedup), never a second run. Best
            // effort is enough; the Event is audit/context linkage, not the
            // run's own record of itself.
            StartResult::Started => {
                let _ = self.facade.with_store(|m| m.add(&build_event()));
                StartOutcome::Started
            }
            StartResult::Duplicate => {
                let _ = self.facade.with_store(|m| m.add(&build_event()));
                StartOutcome::Duplicate
            }
            StartResult::Failed(why) => StartOutcome::Failed(why),
        }
    }

    /// Run the trigger's saved query (`RUN "name"($p = v, …)`) against the
    /// facade this evaluator already holds and return the result payload.
    /// The executor re-validates the saved body as read-only at execution,
    /// and the destructive cap is off — a context query can never mutate.
    ///
    /// #92 — the declaration may bind the query's parameters from the
    /// firing item (`name($session = /session)`): each JSON pointer is
    /// resolved against the item's payload with `--dedup-key`'s machinery
    /// and semantics. Fail-closed like the rest of declared context: an
    /// unresolvable pointer, or one landing on a non-scalar, refuses the
    /// firing rather than running the query with a hole in it. Values ride
    /// as a parsed AST (`RunQueryStmt`), never inside CAL text, so an
    /// untrusted payload value cannot inject CAL.
    fn assemble_context(
        &self,
        spec: &str,
        payload: &serde_json::Value,
    ) -> std::result::Result<serde_json::Value, String> {
        let spec = crate::context::parse_context_query(spec)?;
        let mut bindings: Vec<(String, areev_cal::ast::Value)> = Vec::new();
        for (param, pointer) in &spec.bindings {
            let v = payload.pointer(pointer).ok_or_else(|| {
                format!("parameter ${param}: pointer {pointer} does not resolve against the firing item")
            })?;
            let value = match v {
                serde_json::Value::String(s) => {
                    areev_cal::ast::Value::String { value: s.clone() }
                }
                serde_json::Value::Number(n) => areev_cal::ast::Value::Number {
                    value: n.as_f64().ok_or_else(|| {
                        format!("parameter ${param}: {pointer} is not a representable number")
                    })?,
                },
                serde_json::Value::Bool(b) => areev_cal::ast::Value::Boolean { value: *b },
                other => {
                    return Err(format!(
                        "parameter ${param}: pointer {pointer} must land on a scalar, found {}",
                        match other {
                            serde_json::Value::Null => "null",
                            serde_json::Value::Array(_) => "an array",
                            _ => "an object",
                        }
                    ))
                }
            };
            bindings.push((param.clone(), value));
        }
        let ex = areev_cal::CalExecutor::new(areev_cal::CalExecutorConfig {
            allow_destructive_ops: false,
            ..areev_cal::CalExecutorConfig::default()
        });
        let query = areev_cal::ast::CalQuery {
            version: Default::default(),
            statement: areev_cal::ast::CalStatement::RunQuery(areev_cal::ast::RunQueryStmt {
                name: spec.name.clone(),
                bindings,
                span: None,
            }),
            pipeline: Vec::new(),
            with_options: Vec::new(),
            format: None,
            let_bindings: Vec::new(),
            user_vars: Default::default(),
            let_values: Default::default(),
            warnings: Vec::new(),
        };
        let res = ex
            .execute_parsed(query, &format!("RUN \"{}\"", spec.name), self.facade.as_ref())
            .map_err(|e| e.to_string())?;
        serde_json::to_value(&res.result).map_err(|e| e.to_string())
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
        // Without these, a firing where every item lacked the dedup key reads
        // as "items 5, runs_started 0, duplicates 0" -- and that last zero
        // actively misleads, saying the items were not skipped as duplicates
        // without saying why they were skipped at all. Emitted only when
        // non-zero, so an ordinary firing stays as small as it was.
        if outcome.unidentifiable > 0 {
            obs = obs.extra_field("unidentifiable", serde_json::json!(outcome.unidentifiable));
        }
        if outcome.ingested > 0 {
            obs = obs.extra_field("ingested", serde_json::json!(outcome.ingested));
        }
        if !outcome.failures.is_empty() {
            obs = obs.extra_field("failures", serde_json::json!(outcome.failures));
            // #129 — names WHY the cursor did not move this firing, so the
            // audit record does not require the reader to infer it from
            // `duplicates` staying flat on the next firing.
            obs = obs.extra_field("cursor_held", serde_json::json!(true));
        }
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

    /// Resolve (and cache) the supersession chain for one trigger's head
    /// hash within one evaluation pass (#128).
    ///
    /// `cache` lives on the CALLER's stack — a fresh `HashMap` per `run()`,
    /// `deliver()`, or `status()` invocation — so a chain is walked AT MOST
    /// ONCE per trigger per pass no matter how many items it fires or how
    /// many composites settle against it in that pass. Do not add a call
    /// site that walks the chain per item; thread the already-resolved
    /// [`Chain`] through instead, the way `fire`/`collect_and_start` do.
    fn resolve_chain(&self, hash: &str, cache: &mut HashMap<String, Chain>) -> Result<Chain> {
        if let Some(c) = cache.get(hash) {
            return Ok(c.clone());
        }
        let h = Hash::from_hex(hash).map_err(|e| TriggerError::Storage { detail: e.to_string() })?;
        let hashes: Vec<String> = self
            .facade
            .with_store(|m| m.supersession_chain(&h))
            .map_err(|e| TriggerError::Storage { detail: e.to_string() })?
            .into_iter()
            .map(|hh| hh.to_hex())
            .collect();
        let chain = Chain(hashes);
        cache.insert(hash.to_string(), chain.clone());
        Ok(chain)
    }

    /// Read a trigger's evaluation state through its resolved chain, with
    /// compatibility for a trigger superseded BEFORE #128 shipped (its state
    /// was keyed on the head hash of the moment, not the chain root).
    ///
    /// Always tries the ROOT key first. For a trigger that has never been
    /// superseded this is the ONLY path ever taken — `chain.root() ==` the
    /// hash this crate always keyed state on — so the common case is a
    /// byte-identical lookup to before this feature existed.
    ///
    /// If the root row is absent, every other hash in the chain
    /// (`chain.legacy()`) is checked for a row written under an older head.
    /// Several may exist if the trigger was superseded more than once
    /// before upgrading; [`state_is_more_advanced`] picks whichever
    /// represents the most real progress, so the survivor is never the one
    /// that would silently throw away a further-along cursor.
    ///
    /// `adopt = true` persists the found legacy state under the root and
    /// deletes the legacy row — write the new row FIRST, delete the legacy
    /// row SECOND, so a crash in between leaves a harmless duplicate (the
    /// legacy row is simply never consulted again once the root row exists)
    /// rather than losing the cursor. Pass `true` only from a caller that
    /// already owns write access for real evaluation work (`run`,
    /// `deliver`, composite settlement); `status`/`show` pass `false` so a
    /// pure inspection command has no side effect. Adoption is best-effort
    /// against a read-only-opened memory: `STO-E004` is swallowed and the
    /// found state is still returned, just not yet persisted — the next
    /// pass with write access adopts it.
    fn resolve_state(&self, chain: &Chain, adopt: bool) -> Result<(TriggerState, Option<String>)> {
        let root = chain.root();
        if let Some((state, raw)) = self
            .facade
            .with_store(|m| m.trigger_state(root))
            .map_err(|e| TriggerError::Storage { detail: e.to_string() })?
        {
            return Ok((state, Some(raw)));
        }

        let mut best: Option<(&str, TriggerState)> = None;
        for legacy_hash in chain.legacy() {
            if let Some((state, _raw)) = self
                .facade
                .with_store(|m| m.trigger_state(legacy_hash))
                .map_err(|e| TriggerError::Storage { detail: e.to_string() })?
            {
                let better = best
                    .as_ref()
                    .is_none_or(|(_, current)| state_is_more_advanced(&state, current));
                if better {
                    best = Some((legacy_hash.as_str(), state));
                }
            }
        }
        let Some((legacy_hash, state)) = best else {
            // No row anywhere in the chain: a genuinely new trigger.
            return Ok((TriggerState::default(), None));
        };
        if !adopt {
            return Ok((state, None));
        }

        // `expected=None` only claims an ABSENT row (`INSERT OR IGNORE`), so
        // losing a race to another evaluator adopting the same legacy row
        // concurrently is a no-op, never a clobber — either way the legacy
        // row is redundant now.
        match self.facade.with_store(|m| m.put_trigger_state(root, None, &state)) {
            Ok(_) => {}
            Err(AreevError::ReadOnly(_)) => return Ok((state, None)),
            Err(e) => return Err(TriggerError::Storage { detail: e.to_string() }),
        }
        if let Err(e) = self.facade.with_store(|m| m.clear_trigger_state(legacy_hash)) {
            if !matches!(e, AreevError::ReadOnly(_)) {
                return Err(TriggerError::Storage { detail: e.to_string() });
            }
        }
        let (state, raw) = self
            .facade
            .with_store(|m| m.trigger_state(root))
            .map_err(|e| TriggerError::Storage { detail: e.to_string() })?
            .ok_or_else(|| TriggerError::Storage {
                detail: format!(
                    "adopted trigger state for {root} vanished immediately after writing"
                ),
            })?;
        Ok((state, Some(raw)))
    }
}

/// A trigger's supersession chain for the duration of one evaluation pass:
/// every content-address hash in its edit history, HEAD (the declaration's
/// current hash — what the caller resolved from) first, ROOT last (#128).
///
/// Evaluation state (`trg:<hash>`) and derived run ids are keyed on the
/// ROOT, never the head: a Trigger grain re-pointed by `SUPERSEDE` mints a
/// new head hash for the SAME standing rule (`docs/loop.md`'s
/// propose→approve→apply cycle re-points a trigger exactly this way), and
/// keying on the head alone would orphan the cursor and the dedup fence
/// every time a recommendation is applied. For a trigger that has never
/// been superseded, `root() == head`, so every key derived from it is
/// byte-identical to what this crate always computed — the no-op guarantee
/// the fix has to preserve.
#[derive(Debug, Clone)]
struct Chain(Vec<String>);

impl Chain {
    /// The key evaluation state and run ids are actually resolved against.
    fn root(&self) -> &str {
        self.0.last().map(String::as_str).expect("a chain always has at least its head")
    }

    /// Every hash besides the root — the identities a pre-#128 evaluator
    /// might have keyed state or a run id under. Empty for a
    /// never-superseded trigger, so the compatibility paths that walk this
    /// cost nothing in the common case.
    fn legacy(&self) -> &[String] {
        &self.0[..self.0.len() - 1]
    }
}

/// Which of two legacy state rows found along a chain is "further along"
/// and should be adopted, when more than one exists (a trigger superseded
/// more than once before #128 shipped). `last_fired_at` is the most direct
/// evidence of real progress across every trigger kind — the row that
/// fired most recently is the row whose cursor best reflects what has
/// actually been processed. `op_cursor` (a `memory` trigger's position) and
/// then "has this row ever polled at all" break a tie where neither
/// candidate has ever fired.
fn state_is_more_advanced(candidate: &TriggerState, current_best: &TriggerState) -> bool {
    (candidate.last_fired_at, candidate.op_cursor, candidate.cursor.is_some())
        > (current_best.last_fired_at, current_best.op_cursor, current_best.cursor.is_some())
}

/// One line for `released.last_error` (#129): several items in one firing
/// can fail for different reasons, and `last_error` is a single string.
fn summarize_failures(failures: &[String]) -> String {
    match failures {
        [one] => one.clone(),
        many => format!("{} item(s) failed to start: {}", many.len(), many.join("; ")),
    }
}

/// Drop partial matches past their window.
///
/// Without this, a composite that never completes accumulates keys forever and
/// a stale half-match eventually pairs with unrelated work — the exact failure
/// Argo Events' reset cron exists to paper over.
fn prune_partials(state: &mut TriggerState, trigger: &Trigger, now: i64) {
    let Some(window) = trigger.window_ms else {
        return;
    };
    state.partials.retain(|_, p| now - p.started_ms <= window);
}

#[derive(Debug, Default, Clone)]
struct FireOutcome {
    items: usize,
    runs_started: usize,
    ingested: usize,
    duplicates: usize,
    unidentifiable: usize,
    failures: Vec<String>,
    cursor: Option<String>,
    /// The payloads this firing produced, so a composite can pull its
    /// correlation value out of them.
    fired_items: Vec<serde_json::Value>,
    /// Correlation keys a composite consumed, cleared on release.
    settled_keys: Vec<String>,
    op_cursor: Option<i64>,
    draining: bool,
    seeded: bool,
    skipped_locked: bool,
}

enum StartOutcome {
    Started,
    /// Recorded but not executed — no runtime is wired in.
    Ingested,
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
///
/// **`trigger_hash` is the chain ROOT (#128), not necessarily the
/// declaration's current head.** A `Trigger` grain re-pointed by
/// `SUPERSEDE` mints a new head for the same standing rule, and this
/// function has no way to know that on its own — a caller that passed the
/// head would mint a fresh id for an item already processed under the old
/// head, defeating the very dedup fence this function exists to provide.
/// Every call site resolves the root once per pass (`Evaluator::
/// resolve_chain`) and passes that; this function's signature and hashing
/// are otherwise unchanged.
pub fn run_id_for(trigger_hash: &str, connector: Option<&str>, dedup_value: &str) -> String {
    let mut h = Sha256::new();
    h.update(b"areev-trigger:v1");
    h.update([0x1f]);
    h.update(connector.unwrap_or_default().as_bytes());
    h.update([0x1f]);
    h.update(dedup_value.as_bytes());
    let digest = hex(&h.finalize());
    format!("{}-{}", short(trigger_hash, 8), short(&digest, 12))
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
