//! The driver loop (§5.3): start, resume, respond, cancel, verify. All
//! store writes happen HERE, on the driver thread, in the order the pure
//! core commands them — intents before dispatches, results before their
//! bookkeeping, checkpoints as commanded. Parallelism lives exclusively in
//! the executor pool.

use crate::clock::Clock;
use crate::executor::{DispatchDone, DispatchJob, HostToolExecutor, Pool};
use crate::journal::{self, JournalView};
use crate::manifest::{BudgetsSpec, RunManifest};
use crate::{err_run, RunSession, VerifyReport, VerifyStep};
use areev_cal::AreevFacade;
use areev_core::authz::HARNESS_NS;
use areev_core::error::Hash;
use areev_core::types::{Fact, Grain, Observation};
use areev_run_core::{
    step, Command, DecisionRecord, EffectOutcome, EventIn, FailCause, JournalKey, PlanGraph,
    RunError, RunOutcome, SchedulerState, StepEnv,
};
use serde_json::{json, Value};
use sha2::Digest;
use std::collections::BTreeMap;
use std::sync::Arc;

/// What to do with a dangling intent found on resume (§5.3 step 4): the
/// pinned default re-dispatches under the SAME idempotency key and journals
/// the redelivery; hosts that cannot deduplicate set `Fail`. No third mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OnDangling {
    #[default]
    Redispatch,
    Fail,
}

/// A deterministic crash-injection point — the §5.5 gate-2 mechanism
/// (deterministic injection, never sleeps): the driver aborts at a precise
/// spot in the intent→result protocol, leaving exactly the on-disk state a
/// real crash there would leave. Production callers never set this.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CrashPoint {
    /// Abort immediately after writing the nth intent (1-based), before its
    /// dispatch: the effect never fired; the intent dangles.
    AfterIntent(u32),
    /// Abort after the nth effect COMPLETED but before its result is
    /// journaled: the effect fired in the real world; the intent dangles —
    /// the exact window §5.3's redelivery contract exists for.
    BeforeResult(u32),
}

/// Start/resume options.
#[derive(Default)]
pub struct RunOptions {
    pub budgets: BudgetsSpec,
    pub ask_ttl_sec: Option<i64>,
    pub workers: usize,
    pub on_dangling: OnDangling,
    /// Per-LLM-call `max_tokens` for abstract nodes — frozen into the
    /// manifest at start (§6.7's per-dispatch reservation). None = 1024.
    pub llm_max_tokens: Option<u32>,
    /// Crash-injection for the §5.5 gates. `None` in production.
    pub inject_crash: Option<CrashPoint>,
}

/// The runtime driver: a host over one facade.
pub struct Runner {
    pub facade: Arc<AreevFacade>,
    pub clock: Arc<dyn Clock>,
    pub executor: Arc<dyn HostToolExecutor>,
    /// The tool-calling model for abstract nodes; None = abstract nodes
    /// refuse at load (`RUN-E006`).
    pub llm: Option<Arc<dyn areev_llm::ToolCallLlm>>,
    /// §6.10 streaming subscriber; None = no events. Observational only —
    /// a slow observer delays (and eventually loses) events, never the run.
    pub observer: Option<Arc<dyn crate::stream::RunObserver>>,
    pub ns: String,
    pub principal: String,
}

/// Collect every string in a JSON tree, in a stable depth-first order.
///
/// Walking rather than stringifying keeps structure out of the detector's
/// way: an object key called `subject` is not a person's name, and a
/// pseudonymized key would break the tool's schema.
fn collect_strings(v: &Value, out: &mut Vec<String>) {
    match v {
        Value::String(s) => out.push(s.clone()),
        Value::Array(a) => a.iter().for_each(|x| collect_strings(x, out)),
        Value::Object(o) => o.iter().for_each(|(_, x)| collect_strings(x, out)),
        _ => {}
    }
}

/// Rebuild the tree, substituting strings from `next` in the same order
/// [`collect_strings`] produced them.
fn substitute_strings(v: &Value, next: &mut std::vec::IntoIter<String>) -> Value {
    match v {
        Value::String(_) => next.next().map(Value::String).unwrap_or_else(|| v.clone()),
        Value::Array(a) => {
            Value::Array(a.iter().map(|x| substitute_strings(x, next)).collect())
        }
        Value::Object(o) => Value::Object(
            o.iter().map(|(k, x)| (k.clone(), substitute_strings(x, next))).collect(),
        ),
        other => other.clone(),
    }
}

/// The anonymization boundary between run state and a model.
///
/// ## Why this seam and not the tool seam
///
/// The boundary is the **model**, not the tool. A host tool posting an invoice
/// to a vendor must receive real values — a pseudonymized supplier name writes
/// a corrupt invoice. A model extracting fields from that invoice works just as
/// well on `[ORG_7C1A]`.
///
/// The gate in the store is an egress boundary on *reads*, and an abstract
/// node's prompt is not a read: a trigger hands its payload straight into
/// `run start` in process, so under an `egress` policy the one place a model is
/// actually called was the one place the policy did not reach.
///
/// ## The round trip
///
/// Out, in [`Runner::pseudonymize_for_model`]: every string in the LLM effect's input is
/// pseudonymized. Back, in [`Runner::rehydrate_from_model`]: the tool-call arguments the
/// model produced are rehydrated *before* dispatch, so the tool sees real
/// values again.
///
/// Rehydration **fails closed**. `rehydrate` leaves a token it cannot resolve
/// intact and reports it; dispatching a partially rehydrated call would post a
/// literal `[PERSON_A4F2]` to a vendor, so an unresolved token fails the node
/// instead.
impl Runner {
    /// Is a model-facing anonymization policy live for this run's namespace?
    fn anon_boundary_active(&self) -> Result<bool, RunError> {
        let mode = self
            .facade
            .with_store(|m| m.anon_active_mode(&self.ns))
            .map_err(err_run)?;
        Ok(matches!(mode.as_deref(), Some("egress") | Some("both")))
    }

    /// Pseudonymize everything on its way to the model.
    fn pseudonymize_for_model(&self, input: &Value) -> Result<Value, RunError> {
        if !self.anon_boundary_active()? {
            return Ok(input.clone());
        }
        let mut values = Vec::new();
        collect_strings(input, &mut values);
        if values.is_empty() {
            return Ok(input.clone());
        }
        self.facade
            .with_store(|m| m.anon_egress_text(&self.ns, &mut values))
            .map_err(err_run)?;
        Ok(substitute_strings(input, &mut values.into_iter()))
    }

    /// Rehydrate what came back, or say exactly which token could not be.
    ///
    /// `Err` here is the fail-closed case: the caller turns it into a failed
    /// effect rather than dispatching a call with a placeholder still in it.
    fn rehydrate_from_model(&self, input: &Value) -> Result<Value, String> {
        let active = self.anon_boundary_active().map_err(|e| e.to_string())?;
        if !active {
            return Ok(input.clone());
        }
        let mapping = self
            .facade
            .with_store(|m| m.anon_mapping_for(&self.ns))
            .map_err(|e| e.to_string())?;
        // The policy's OWN template, not the default. Scanning for the default
        // silhouette while the memory mints another shape would report no
        // leftovers and fail open — the exact inversion of what this check is
        // for.
        let template = self
            .facade
            .with_store(|m| m.anon_placeholder(&self.ns))
            .map_err(|e| e.to_string())?
            .unwrap_or_else(|| "[{CATEGORY}_{ID}]".to_string());
        let mut values = Vec::new();
        collect_strings(input, &mut values);
        if values.is_empty() {
            return Ok(input.clone());
        }
        let mut out = Vec::with_capacity(values.len());
        let mut unmatched: Vec<String> = Vec::new();
        for v in values {
            let r = areev_core::anon::rehydrate_with_template(&v, &mapping, &template)
                .map_err(|e| e.to_string())?;
            for u in r.unmatched {
                if !unmatched.contains(&u) {
                    unmatched.push(u);
                }
            }
            out.push(r.text);
        }
        if !unmatched.is_empty() {
            return Err(format!(
                "the model produced placeholder{} this run cannot resolve: {}. \
                 Dispatching would send the placeholder itself to the tool, so \
                 the effect is refused instead",
                if unmatched.len() == 1 { "" } else { "s" },
                unmatched.join(", ")
            ));
        }
        Ok(substitute_strings(input, &mut out.into_iter()))
    }
}

/// Journal-shaped transcript messages → the tool-calling seam's shape.
/// Pure translation, on the driver thread; the pool never parses state.
fn seam_messages(input: &Value) -> Vec<areev_llm::ChatMessage> {
    let Some(msgs) = input.get("messages").and_then(|m| m.as_array()) else {
        return Vec::new();
    };
    msgs.iter()
        .filter_map(|m| {
            let role = m.get("role")?.as_str()?;
            match role {
                "user" => Some(areev_llm::ChatMessage::User(
                    m.get("content")
                        .map(|c| {
                            c.as_str().map(str::to_string).unwrap_or_else(|| c.to_string())
                        })
                        .unwrap_or_default(),
                )),
                "assistant" => Some(areev_llm::ChatMessage::Assistant {
                    text: m.get("text").and_then(|t| t.as_str()).map(str::to_string),
                    tool_calls: m
                        .get("tool_calls")
                        .and_then(|c| c.as_array())
                        .map(|arr| {
                            arr.iter()
                                .map(|c| areev_llm::ToolCallOut {
                                    id: c
                                        .get("id")
                                        .and_then(|v| v.as_str())
                                        .unwrap_or_default()
                                        .to_string(),
                                    name: c
                                        .get("name")
                                        .and_then(|v| v.as_str())
                                        .unwrap_or_default()
                                        .to_string(),
                                    arguments: c
                                        .get("arguments")
                                        .cloned()
                                        .unwrap_or(Value::Null),
                                    arguments_raw: None,
                                })
                                .collect()
                        })
                        .unwrap_or_default(),
                }),
                "tool" => Some(areev_llm::ChatMessage::ToolResult {
                    tool_call_id: m
                        .get("tool_call_id")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .to_string(),
                    content: m
                        .get("content")
                        .map(|c| {
                            c.as_str().map(str::to_string).unwrap_or_else(|| c.to_string())
                        })
                        .unwrap_or_default(),
                    is_error: m.get("is_error").and_then(|v| v.as_bool()).unwrap_or(false),
                }),
                _ => None,
            }
        })
        .collect()
}

/// The pure §6.11 argument validator over the manifest's strict schemas:
/// non-strict (or schema-less) tools pass; strict ones run
/// `validate_instance`, and the violation kind becomes the re-prompt text.
fn make_validate_args(
    schemas: &BTreeMap<String, Value>,
) -> impl Fn(&str, &Value) -> std::result::Result<(), String> + '_ {
    move |tool: &str, args: &Value| match schemas.get(tool) {
        Some(schema) => areev_core::types::validate_instance(args, schema)
            .map_err(|k| format!("schema violation: {}", k.as_str())),
        None => Ok(()),
    }
}

fn builtin_eval(edge: &areev_run_core::PlanEdge, state: &Value) -> bool {
    edge.cond
        .as_ref()
        .map(|c| areev_run_core::cond::eval(c, state))
        .unwrap_or(true)
}

/// Name the top-level fields where two scheduler-state JSONs disagree — the
/// RUN-E009 diagnostic. "Diverges" without saying WHERE would send whoever
/// reads the report straight back to a debugger.
fn diff_fields(stored: &Value, replayed: &Value) -> String {
    let (Some(a), Some(b)) = (stored.as_object(), replayed.as_object()) else {
        return "state shape differs".into();
    };
    let mut diffs: Vec<String> = Vec::new();
    for key in a.keys().chain(b.keys()) {
        if a.get(key) != b.get(key) && !diffs.iter().any(|d| d.starts_with(key.as_str())) {
            diffs.push(format!(
                "{key}: stored={} replayed={}",
                a.get(key).map(|v| v.to_string()).unwrap_or_else(|| "∅".into()),
                b.get(key).map(|v| v.to_string()).unwrap_or_else(|| "∅".into()),
            ));
        }
    }
    diffs.join("; ")
}

fn idempotency_key(key: &JournalKey, input: &Value) -> String {
    let mut h = sha2::Sha256::new();
    h.update(key.idempotency_prefix().as_bytes());
    h.update([0x1f]);
    h.update(input.to_string().as_bytes());
    let out = h.finalize();
    let mut s = String::with_capacity(64);
    for b in out {
        use std::fmt::Write;
        let _ = write!(s, "{b:02x}");
    }
    s
}

impl Runner {
    /// D5 enforcement at the driver boundary. The facade session's rights
    /// decide (owner unless the host bound a principal — the single-user
    /// path never meets authorization, same as every other surface); a
    /// bound session needs the run verb granted in the file.
    fn check_run_verb(&self, verb: areev_core::authz::Verb) -> Result<(), RunError> {
        self.facade
            .authz()
            .check(verb, &self.ns)
            .map_err(|e| RunError::Unauthorized { what: e.to_string() })
    }

    /// `respond` authorizes the RESPONDER, not the session: on a governed
    /// session (non-owner), the responder principal's own grants must cover
    /// `run.respond` — an approval boundary where "whoever holds the
    /// process" can approve is not an approval boundary.
    fn check_responder(&self, responder: &str) -> Result<(), RunError> {
        if self.facade.authz().is_owner() {
            return Ok(());
        }
        let session = self
            .facade
            .principal_session(responder)
            .map_err(err_run)?;
        session
            .authz()
            .check(areev_core::authz::Verb::RunRespond, &self.ns)
            .map_err(|e| RunError::Unauthorized { what: e.to_string() })
    }

    fn load_plan(&self, plan_hash: &Hash) -> Result<PlanGraph, RunError> {
        let wf = self
            .facade
            .with_store(|m| m.get(plan_hash))
            .map_err(|e| RunError::UnresolvedRef { what: format!("workflow: {e}") })?
            .to_workflow()
            .map_err(|e| RunError::InvalidPlan { why: e.to_string() })?;
        PlanGraph::build(&wf)
    }

    fn load_manifest(&self, run_id: &str) -> Result<RunManifest, RunError> {
        self.facade.with_store(|m| RunManifest::load(m, run_id))
    }

    /// The §6.11 argument-validation table: tool_name → input_schema for
    /// every STRICT pinned Definition. Loaded once per drive/verify (the
    /// scheduler's `validate_args` closure must never touch the store), and
    /// a pure function of the manifest — so live and replay agree.
    fn load_arg_schemas(
        &self,
        manifest: &RunManifest,
    ) -> Result<BTreeMap<String, Value>, RunError> {
        let mut schemas = BTreeMap::new();
        for p in &manifest.pinned {
            if p.executor != "host" || p.tool_hash.is_empty() {
                continue;
            }
            let h = Hash::from_hex(&p.tool_hash).map_err(err_run)?;
            let def = self
                .facade
                .with_store(|m| m.get(&h))
                .map_err(err_run)?
                .to_tool()
                .map_err(err_run)?;
            if def.strict == Some(true) {
                if let Some(schema) = def.input_schema {
                    schemas.insert(p.tool_name.clone(), schema);
                }
            }
        }
        Ok(schemas)
    }

    /// Start a new run of the Workflow at `plan_hash` and drive it until it
    /// finishes or parks on a Client ask.
    pub fn start(
        &self,
        plan_hash: &Hash,
        run_id: &str,
        input: Value,
        opts: &RunOptions,
    ) -> Result<RunSession, RunError> {
        self.check_run_verb(areev_core::authz::Verb::RunExecute)?;
        areev_core::types::validate_run_id(run_id).map_err(err_run)?;
        // A run id is an identity: starting twice under one id would
        // interleave two journals (§5.4 forks are the sanctioned reuse).
        if self.load_manifest(run_id).is_ok() {
            return Err(RunError::Tainted {
                why: format!("run '{run_id}' already exists — resume it or fork"),
            });
        }
        let plan = self.load_plan(plan_hash)?;
        let manifest = self.facade.with_store(|m| {
            RunManifest::resolve(
                m,
                &self.ns,
                run_id,
                plan_hash,
                &plan,
                &self.principal,
                opts.budgets,
                opts.ask_ttl_sec,
                input.clone(),
                self.llm.is_some(),
                opts.llm_max_tokens,
            )
        })?;
        // Refuse an unpinned code executor before the run exists: it would
        // otherwise take a lease, write a manifest, and fail on first
        // dispatch, leaving a terminal run to explain. The address is named
        // so pinning it is a copy-paste.
        for p in &manifest.pinned {
            if let Some(uri) = &p.executor_uri {
                if !self.executor.code_allowed(&p.tool_hash, uri) {
                    return Err(RunError::CodeExecRefused {
                        condition: format!(
                            "node '{}' names executor {uri}, which this host has not \
                             pinned — pin it: --allow-executor {} (CLI), \
                             allow_executor (Python/Node), or \
                             $AREEV_RUN_ALLOW_EXECUTOR (areev serve)",
                            p.node,
                            crate::executor::strip_cas(uri)
                        ),
                    });
                }
                // Same fail-closed shape for the declared runtime (#86): a
                // plan asking for the sandbox on a host with none refuses
                // BEFORE the run exists, naming the missing configuration.
                if let Some(rt) = &p.runtime {
                    if !self.executor.runtime_supported(rt) {
                        return Err(RunError::CodeExecRefused {
                            condition: format!(
                                "node '{}' declares runtime {rt:?}, which this host \
                                 cannot dispatch — configure the sandbox: \
                                 --sandbox-cmd areev-sandbox (CLI), sandbox_cmd \
                                 (Python/Node), or $AREEV_RUN_SANDBOX_CMD (areev serve)",
                                p.node
                            ),
                        });
                    }
                }
            }
        }
        // An abstract node crosses the model boundary, and a boundary whose
        // placeholders are not value-derived cannot be replayed. Refuse here
        // rather than let `verify` diverge later, which would look like a
        // journal integrity failure instead of a configuration one.
        if manifest.pinned.iter().any(|p| p.executor == "abstract") && self.anon_boundary_active()?
        {
            let policies = self.facade.with_store(|m| m.anon_policies()).map_err(err_run)?;
            if let Some((_, policy)) = policies.iter().find(|(ns, _)| ns == &self.ns) {
                if policy.scope != "memory" {
                    return Err(RunError::AnonReplayUnsafe {
                        ns: self.ns.clone(),
                        scope: policy.scope.clone(),
                    });
                }
            }
        }
        self.facade
            .with_store(|m| manifest.persist(m))
            .map_err(err_run)?;

        let st = SchedulerState::new(run_id, &plan);
        let events = vec![
            EventIn::ClockReading { unix_ms: self.clock.now_ms() },
            EventIn::Start { input },
        ];
        self.drive(&plan, plan_hash, &manifest, st, JournalView::default(), events, opts)
    }

    /// Resume a run from its latest checkpoint: settle expired asks, adopt
    /// journaled responses, re-deliver dangling intents (§5.3).
    pub fn resume(&self, run_id: &str, opts: &RunOptions) -> Result<RunSession, RunError> {
        self.check_run_verb(areev_core::authz::Verb::RunExecute)?;
        let manifest = self.load_manifest(run_id)?;
        let plan_hash = Hash::from_hex(&manifest.plan_hash)
            .map_err(|_| RunError::ManifestMismatch { why: "bad plan hash".into() })?;
        let plan = self.load_plan(&plan_hash)?;
        let view = self
            .facade
            .with_store(|m| journal::load(m, &self.ns, run_id))
            .map_err(err_run)?;
        let Some(last) = view.checkpoints.last() else {
            // A crash inside the FIRST superstep: the manifest exists but no
            // checkpoint does. The manifest carries the input for exactly
            // this — rebuild from it (dangling intents are adopted by the
            // normal §5.3 path below) rather than bricking the run id.
            let st = SchedulerState::new(run_id, &plan);
            let events = vec![
                EventIn::ClockReading { unix_ms: self.clock.now_ms() },
                EventIn::Start { input: manifest.input.clone() },
            ];
            return self.drive(&plan, &plan_hash, &manifest, st, view, events, opts);
        };
        let st: SchedulerState = serde_json::from_value(last.scheduler.clone())
            .map_err(|e| RunError::ManifestMismatch { why: format!("checkpoint state: {e}") })?;
        if st.run_id != run_id {
            return Err(RunError::ManifestMismatch {
                why: format!("checkpoint belongs to run '{}'", st.run_id),
            });
        }
        if st.is_terminal() {
            return Ok(RunSession::Finished {
                outcome: st.outcome().cloned().expect("terminal"),
                run_id: run_id.to_string(),
            });
        }

        let now = self.clock.now_ms();
        let mut events = vec![EventIn::ClockReading { unix_ms: now }];

        // Settled-while-parked responses (a respond() wrote the result) and
        // expired asks (settled here as Timeout — never re-dispatched,
        // never silently eternal).
        let executors = manifest.executors();
        for (id, pending) in st.pending_asks.clone() {
            if let Some(entry) = view.entries.get(&pending.key) {
                if let Some((_, outcome)) = &entry.result {
                    events.push(EventIn::ResponseSettled {
                        tool_call_id: id.clone(),
                        outcome: outcome.clone(),
                    });
                    continue;
                }
            }
            if pending
                .ask
                .expires_at_sec
                .is_some_and(|exp| (now / 1000) as i64 > exp)
            {
                let detail = "client ask expired".to_string();
                let outcome = EffectOutcome::Failed {
                    cause: FailCause::Timeout,
                    journal_bytes: detail.len() as u64,
                    detail,
                };
                let executor = executors
                    .get(pending.node_idx)
                    .cloned()
                    .ok_or_else(|| RunError::ManifestMismatch {
                        why: "ask node outside manifest".into(),
                    })?;
                let intent = view
                    .entries
                    .get(&pending.key)
                    .map(|e| e.intent)
                    .ok_or_else(|| RunError::Tainted {
                        why: "pending ask with no journaled intent".into(),
                    })?;
                self.facade
                    .with_store(|m| {
                        journal::write_result(
                            m,
                            &self.ns,
                            run_id,
                            &plan_hash,
                            &intent,
                            &pending.key,
                            &executor,
                            &outcome,
                            st.superstep,
                            now,
                            &self.principal,
                        )
                    })
                    .map_err(err_run)?;
                events.push(EventIn::ResponseSettled { tool_call_id: id, outcome });
            }
        }

        self.drive(&plan, &plan_hash, &manifest, st, view, events, opts)
    }

    /// §5.4 time-travel fork: a NEW run descending from one of `base`'s
    /// checkpoints. The fork's manifest records `(base_run,
    /// base_checkpoint)`, an `mg:fork_of` Fact indexes the relationship,
    /// and the SEED checkpoint `derived_from`s the base checkpoint — the
    /// verifiability chain. The base run is never touched.
    ///
    /// - **Same plan** (`new_plan = None`): the base checkpoint's scheduler
    ///   state carries over under the new run id; the fork re-executes
    ///   everything past the checkpoint under its own journal keys.
    ///   A `BudgetExhausted` terminal re-opens (that is how a run continues
    ///   under raised budgets — budgets are the fork's own knobs).
    /// - **New plan** (in-flight migration): full §6.1 validation re-runs;
    ///   the graph restarts at its entry with the base checkpoint's CONTEXT
    ///   as the input.
    ///
    /// v1 refuses forking a checkpoint that parked mid-superstep (pending
    /// asks address journal keys of the parent) — pick a closed superstep.
    ///
    /// Returns the seed checkpoint hash; `resume` continues the fork.
    pub fn fork(
        &self,
        base_run_id: &str,
        at_superstep: Option<u64>,
        new_run_id: &str,
        new_plan: Option<&Hash>,
        opts: &RunOptions,
    ) -> Result<Hash, RunError> {
        self.check_run_verb(areev_core::authz::Verb::RunExecute)?;
        areev_core::types::validate_run_id(new_run_id).map_err(err_run)?;
        if self.load_manifest(new_run_id).is_ok() {
            return Err(RunError::Tainted {
                why: format!("run '{new_run_id}' already exists — fork ids must be fresh"),
            });
        }
        let base_manifest = self.load_manifest(base_run_id)?;
        let view = self
            .facade
            .with_store(|m| journal::load(m, &self.ns, base_run_id))
            .map_err(err_run)?;
        let base_ckpt = match at_superstep {
            Some(ss) => {
                // A terminal checkpoint shares its superstep with that
                // superstep's close — `--at N` means the CLOSED state, so
                // prefer the last Idle checkpoint there (the terminal one
                // still wins when it is all there is, e.g. BudgetExhausted).
                let matching: Vec<_> =
                    view.checkpoints.iter().filter(|c| c.superstep == ss).collect();
                matching
                    .iter()
                    .rev()
                    .find(|c| c.scheduler.get("phase") == Some(&json!("Idle")))
                    .or(matching.last())
                    .copied()
                    .ok_or_else(|| RunError::ManifestMismatch {
                        why: format!(
                            "run '{base_run_id}' has no checkpoint at superstep {ss}"
                        ),
                    })?
            }
            None => view.checkpoints.last().ok_or_else(|| RunError::ManifestMismatch {
                why: format!("run '{base_run_id}' has no checkpoint to fork from"),
            })?,
        };
        let mut st: SchedulerState = serde_json::from_value(base_ckpt.scheduler.clone())
            .map_err(|e| RunError::ManifestMismatch { why: format!("checkpoint state: {e}") })?;
        if !st.pending_asks.is_empty()
            || matches!(st.phase, areev_run_core::Phase::Open { .. })
        {
            return Err(RunError::Tainted {
                why: format!(
                    "checkpoint at superstep {} parked mid-superstep — v1 forks \
                     only from closed supersteps (respond or pick another)",
                    base_ckpt.superstep
                ),
            });
        }
        match st.outcome() {
            Some(RunOutcome::BudgetExhausted { .. }) => {
                // The sanctioned continue-under-raised-budgets path.
                st.phase = areev_run_core::Phase::Idle;
                st.exhausted = None;
            }
            Some(other) => {
                return Err(RunError::Tainted {
                    why: format!(
                        "base checkpoint is terminal ({other:?}) — time-travel \
                         forks pick an earlier superstep (--at N)"
                    ),
                });
            }
            None => {}
        }

        let fork_base = crate::manifest::ForkBase {
            base_run: base_run_id.to_string(),
            base_checkpoint: base_ckpt.hash.to_hex(),
            base_superstep: base_ckpt.superstep,
        };
        let mut manifest = match new_plan {
            Some(h) => {
                // In-flight migration: validate the new plan in full, then
                // restart its graph with the inherited context as input.
                let plan = self.load_plan(h)?;
                let context = st.context.clone();
                let manifest = self.facade.with_store(|m| {
                    RunManifest::resolve(
                        m,
                        &self.ns,
                        new_run_id,
                        h,
                        &plan,
                        &self.principal,
                        opts.budgets,
                        opts.ask_ttl_sec,
                        context.clone(),
                        self.llm.is_some(),
                        opts.llm_max_tokens,
                    )
                })?;
                let mut fresh = SchedulerState::new(new_run_id, &plan);
                // The Start bootstrap, applied here so the seed checkpoint
                // is immediately resumable (§ Start: entry activates).
                fresh.context = context;
                fresh.clock_ms = st.clock_ms;
                fresh.node_state[0] = areev_run_core::NodeState::Ready;
                st = fresh;
                manifest
            }
            None => {
                // Resolution freeze: the fork inherits the base's pins
                // verbatim — never re-resolves. Budgets/TTL are the fork's
                // own knobs.
                st.run_id = new_run_id.to_string();
                RunManifest {
                    run_id: new_run_id.to_string(),
                    // The FORKER triggers this run — separation of duties
                    // must compare against them, not the base run's starter.
                    principal: self.principal.clone(),
                    budgets: opts.budgets,
                    ask_ttl_sec: opts.ask_ttl_sec,
                    ..base_manifest
                }
            }
        };
        manifest.fork_of = Some(fork_base.clone());
        self.facade
            .with_store(|m| manifest.persist(m))
            .map_err(err_run)?;

        // The fork index Fact: `runs_touching`-style joins report forks for
        // inherited-prefix grains through this.
        let now = self.clock.now_ms();
        let mut fact = Fact::new(
            &format!("run:{new_run_id}"),
            "mg:fork_of",
            &format!("{}@{}", fork_base.base_run, fork_base.base_checkpoint),
        )
        .namespace(HARNESS_NS)
        .created_at(now as i64);
        fact.common.author_did = Some(self.principal.clone());
        fact.common.extra_fields.insert("run_id".into(), json!(new_run_id));
        fact.common
            .extra_fields
            .insert("base_run_id".into(), json!(fork_base.base_run));
        self.facade.with_store(|m| m.add(&fact)).map_err(err_run)?;

        // The seed checkpoint, derived_from the BASE checkpoint (§5.4's
        // hash chain) — what `resume` continues from.
        let state_json = serde_json::to_value(&st).unwrap_or(Value::Null);
        let record = DecisionRecord {
            superstep: st.superstep,
            clock_open_ms: st.clock_ms,
            clock_close_ms: st.clock_ms,
            ..Default::default()
        };
        let base_hash = base_ckpt.hash;
        self.facade
            .with_store(|m| {
                journal::write_checkpoint(
                    m,
                    &self.ns,
                    new_run_id,
                    &state_json,
                    &record,
                    st.clock_ms,
                    Some(&base_hash),
                    &self.principal,
                )
            })
            .map_err(err_run)
    }

    /// Answer a Client ask (§6.6's validation order). Responding and
    /// resuming are separate acts: this writes the journaled result and
    /// returns; `resume` spends the compute.
    pub fn respond(
        &self,
        run_id: &str,
        tool_call_id: &str,
        result: Value,
        is_error: bool,
        responder: &str,
    ) -> Result<(), RunError> {
        self.check_responder(responder)?;
        let manifest = self.load_manifest(run_id)?;
        let plan_hash = Hash::from_hex(&manifest.plan_hash)
            .map_err(|_| RunError::ManifestMismatch { why: "bad plan hash".into() })?;
        let view = self
            .facade
            .with_store(|m| journal::load(m, &self.ns, run_id))
            .map_err(err_run)?;
        let Some(last) = view.checkpoints.last() else {
            return Err(RunError::ManifestMismatch { why: "run has no checkpoint".into() });
        };
        let st: SchedulerState = serde_json::from_value(last.scheduler.clone())
            .map_err(|e| RunError::ManifestMismatch { why: format!("checkpoint state: {e}") })?;

        // Fresh clock reading — respond is its own effect boundary (§6.6):
        // there is no scheduler clock while parked. The reading lands on
        // the result grain, so replay sees it.
        let now = self.clock.now_ms();

        let journal_rejection = |why: &str| {
            // The losing/invalid response is journaled BEFORE the caller is
            // told no — "two officers approved conflicting outcomes seconds
            // apart" is audit evidence, not noise (§6.6).
            let mut obs = Observation::new(responder, "human")
                .subject(&format!("run:{run_id}"))
                .object(&format!("rejected response to ask {tool_call_id}: {why}"))
                .namespace(HARNESS_NS)
                .created_at(now as i64);
            obs.common
                .extra_fields
                .insert("run_id".into(), json!(run_id));
            let _ = self.facade.with_store(|m| m.add(&obs));
        };

        let Some(pending) = st.pending_asks.get(tool_call_id) else {
            journal_rejection("no such pending ask (settled, expired, or never asked)");
            return Err(RunError::UnknownAsk { tool_call_id: tool_call_id.to_string() });
        };
        if view
            .entries
            .get(&pending.key)
            .is_some_and(|e| e.result.is_some())
        {
            journal_rejection("already settled — first commit won");
            return Err(RunError::UnknownAsk { tool_call_id: tool_call_id.to_string() });
        }
        if pending
            .ask
            .expires_at_sec
            .is_some_and(|exp| (now / 1000) as i64 > exp)
        {
            journal_rejection("expired");
            return Err(RunError::UnknownAsk { tool_call_id: tool_call_id.to_string() });
        }
        // Approval separation of duties (§6.6, D5): structurally refused,
        // not conventioned — mirrors the loop's self-approval block.
        if pending.ask.approval && responder == manifest.principal {
            return Err(RunError::Unauthorized {
                what: format!(
                    "approval ask {tool_call_id} requires a responder other than \
                     the triggering principal '{}'",
                    manifest.principal
                ),
            });
        }

        let outcome = if is_error {
            let detail = result.to_string();
            EffectOutcome::Failed {
                cause: FailCause::UserAborted,
                journal_bytes: detail.len() as u64,
                detail,
            }
        } else {
            EffectOutcome::Completed {
                journal_bytes: result.to_string().len() as u64,
                result,
                input_tokens: 0,
                output_tokens: 0,
                usd_micros: 0,
            }
        };
        let executor = manifest
            .executors()
            .get(pending.node_idx)
            .cloned()
            .ok_or_else(|| RunError::ManifestMismatch { why: "ask node outside manifest".into() })?;
        let intent = view
            .entries
            .get(&pending.key)
            .map(|e| e.intent)
            .ok_or_else(|| RunError::UnknownAsk { tool_call_id: tool_call_id.to_string() })?;
        self.facade
            .with_store(|m| {
                journal::write_result(
                    m,
                    &self.ns,
                    run_id,
                    &plan_hash,
                    &intent,
                    &pending.key,
                    &executor,
                    &outcome,
                    st.superstep,
                    now,
                    responder,
                )
            })
            .map_err(err_run)?;
        Ok(())
    }

    /// Write the cancel marker (the kill-switch primitive). The drive loop
    /// polls it every iteration; `resume` after cancel drains.
    pub fn cancel(&self, run_id: &str, principal: &str, reason: &str) -> Result<(), RunError> {
        // Deliberately the LOW bar (D5): the brake must never be blocked by
        // missing privilege — RunCancel, not RunExecute.
        self.check_run_verb(areev_core::authz::Verb::RunCancel)?;
        let now = self.clock.now_ms();
        let mut f = Fact::new(&format!("run:{run_id}"), "mg:cancel", reason)
            .namespace(HARNESS_NS)
            .created_at(now as i64);
        f.common.author_did = Some(principal.to_string());
        f.common.extra_fields.insert("run_id".into(), json!(run_id));
        self.facade.with_store(|m| m.add(&f)).map_err(err_run)?;
        Ok(())
    }

    fn cancel_marker(&self, run_id: &str) -> Option<(String, String)> {
        // A cancel on ANY ancestor stops the whole subtree: child run ids
        // are `{parent}~{16-hex}` by construction, so walking suffix-strips
        // covers arbitrarily nested subgraphs (the <5-minute kill-switch
        // clause must not be unbounded under nesting).
        let mut candidate = run_id.to_string();
        loop {
            let hit = self
                .facade
                .with_store(|m| m.latest(HARNESS_NS, &format!("run:{candidate}"), "mg:cancel"))
                .ok()
                .flatten()
                .map(|g| {
                    (
                        g.get_str("author_did").unwrap_or("unknown").to_string(),
                        g.get_str("object").unwrap_or("").to_string(),
                    )
                });
            if hit.is_some() {
                return hit;
            }
            match candidate.rfind('~') {
                Some(pos) if candidate.len() - pos == 17 => candidate.truncate(pos),
                _ => return None,
            }
        }
    }

    /// Execute a subgraph node as a CHILD RUN: own run id (deterministic —
    /// derived from the journal key, so replays and permutations agree),
    /// own journal, linked by content on the parent's effect grains. The
    /// child's final context becomes the node's result, merging into the
    /// parent state through the reducers like any other node result.
    ///
    /// v1 limitation, stated: a child that PARKS on a Client ask fails the
    /// parent node with a clear message — HITL nodes belong in the parent
    /// graph until ask-bubbling ships. A terminally failed child is never
    /// retried (resuming a terminal run returns the same outcome forever).
    fn run_subgraph_effect(
        &self,
        parent_run_id: &str,
        key: &JournalKey,
        workflow_hash: &str,
        input: &Value,
        opts: &RunOptions,
    ) -> EffectOutcome {
        let child_run_id = format!("{parent_run_id}~{}", &key.tool_call_id()[..16]);
        let fail = |cause: FailCause, detail: String| EffectOutcome::Failed {
            journal_bytes: detail.len() as u64,
            cause,
            detail,
        };
        let child_session = if self
            .facade
            .with_store(|m| RunManifest::load(m, &child_run_id))
            .is_ok()
        {
            self.resume(&child_run_id, opts)
        } else {
            let Ok(h) = Hash::from_hex(workflow_hash) else {
                return fail(FailCause::Unknown, "bad subgraph plan hash".into());
            };
            self.start(&h, &child_run_id, input.clone(), opts)
        };
        match child_session {
            Ok(RunSession::Finished { outcome: RunOutcome::Completed, .. }) => {
                // The child's final context is the node's contribution.
                let context = self
                    .facade
                    .with_store(|m| journal::load(m, &self.ns, &child_run_id))
                    .ok()
                    .and_then(|v| v.checkpoints.last().cloned())
                    .and_then(|c| c.scheduler.get("context").cloned())
                    .unwrap_or(Value::Null);
                EffectOutcome::Completed {
                    journal_bytes: context.to_string().len() as u64,
                    result: context,
                    input_tokens: 0,
                    output_tokens: 0,
                    usd_micros: 0,
                }
            }
            Ok(RunSession::Finished { outcome, .. }) => fail(
                FailCause::Unknown,
                format!("subgraph run '{child_run_id}' finished {outcome:?}"),
            ),
            Ok(RunSession::Parked { .. }) => fail(
                FailCause::Unknown,
                format!(
                    "subgraph run '{child_run_id}' parked on a Client ask — v1 \
                     does not bubble asks through subgraphs; move HITL nodes \
                     into the parent graph"
                ),
            ),
            Err(e) => fail(FailCause::Unknown, format!("subgraph: {e}")),
        }
    }

    /// The shared drive loop. One clock reading per iteration; journal hits
    /// replay without executing; everything else goes to the pool.
    #[allow(clippy::too_many_arguments)]
    fn drive(
        &self,
        plan: &PlanGraph,
        plan_hash: &Hash,
        manifest: &RunManifest,
        st: SchedulerState,
        view: JournalView,
        initial_events: Vec<EventIn>,
        opts: &RunOptions,
    ) -> Result<RunSession, RunError> {
        let executors = manifest.executors();
        let arg_schemas = self.load_arg_schemas(manifest)?;
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
            max_effects_per_attempt: 16,
        };
        let run_id = st.run_id.clone();

        // Take the run lease before advancing anything. Two drivers on one run
        // used to last-write-wins in the journal, silently — the `Tainted` doc
        // comment claimed forked tips were detected, but nothing detected them.
        // This prevents the case instead of noticing it afterwards. On the
        // embedded backend one memory is one writer, so this always succeeds
        // and costs a single meta row.
        let holder = format!("{}#{}", self.principal, std::process::id());
        let mut lease = crate::lease::RunLease::acquire(
            &self.facade,
            &run_id,
            &holder,
            self.clock.now_ms() as i64,
            crate::lease::DEFAULT_RUN_LEASE_MS,
        )?;

        // §6.10: the event bus wraps the observer; `emit` never blocks.
        // Arc'd because LLM jobs carry a TokenChunk sink into the pool.
        let bus = self
            .observer
            .as_ref()
            .map(|o| {
                Arc::new(crate::stream::EventBus::new(
                    Arc::clone(o),
                    crate::stream::EVENT_BUFFER,
                ))
            });
        let emit = |ev: crate::stream::RunEvent| {
            if let Some(b) = &bus {
                b.emit(ev);
            }
        };
        emit(if view.checkpoints.is_empty() {
            crate::stream::RunEvent::RunStarted { run_id: run_id.clone() }
        } else {
            crate::stream::RunEvent::RunResumed { run_id: run_id.clone() }
        });
        let pool = Pool::new(
            Arc::clone(&self.executor),
            self.llm.clone(),
            opts.workers.max(1),
        );
        let mut intents: BTreeMap<JournalKey, Hash> =
            view.entries.iter().map(|(k, e)| (k.clone(), e.intent)).collect();
        let mut results: BTreeMap<JournalKey, EffectOutcome> = view
            .entries
            .iter()
            .filter_map(|(k, e)| e.result.as_ref().map(|(_, o)| (k.clone(), o.clone())))
            .collect();
        let mut prev_ckpt: Option<Hash> = view.checkpoints.last().map(|c| c.hash);
        // Distinct refusals already journaled for this drive, so a retry loop
        // against one blocked host records one fact rather than many.
        let mut refusals_seen: std::collections::BTreeSet<(String, String, String)> =
            Default::default();
        let mut in_flight: usize = 0;
        let mut st = st;
        let mut events = initial_events;
        let mut intents_written: u32 = 0;
        let mut results_seen: u32 = 0;

        // Crash-window danglings (§5.3 step 4): fail fast when the host
        // cannot honor idempotency keys; otherwise the re-emitted intents
        // below adopt these and re-dispatch under the same key.
        if opts.on_dangling == OnDangling::Fail {
            if let Some(last) = view.checkpoints.last() {
                if let Some(e) = view
                    .dangling(last.superstep)
                    .into_iter()
                    .find(|e| !st.pending_asks.values().any(|p| p.key == e.key))
                {
                    return Err(RunError::DanglingIntent {
                        key: e.key.tool_call_id(),
                    });
                }
            }
        }

        loop {
            // The kill switch: polled every iteration (a µs point read).
            if st.cancel.is_none() {
                if let Some((by, reason)) = self.cancel_marker(&run_id) {
                    // `step()`'s contract: any call that may CLOSE a
                    // superstep carries a fresh ClockReading (doc comment on
                    // `step()`). A freshly-seen cancel closes the run's
                    // FINAL superstep, but without a reading taken HERE
                    // `st.clock_ms` stays whatever an earlier, already-
                    // completed wave last set it to — which can predate the
                    // cancel Fact's own `created_at`, journaling a
                    // clock_close_ms that appears to close before the
                    // operator asked to cancel. `cancel_marker` only
                    // returns once its read has observed the write `cancel`
                    // performed, so a reading taken now is monotone at or
                    // after it. `verify()`'s replay already assumes this
                    // shape at both its own cancel_peek sites (a
                    // ClockReading immediately preceding CancelSeen in the
                    // same batch) — this is the live counterpart.
                    events.push(EventIn::ClockReading { unix_ms: self.clock.now_ms() });
                    events.push(EventIn::CancelSeen { principal: by, reason });
                }
            }

            let out = step(&env, st, &events);
            st = out.state;
            events = Vec::new();

            let mut parked_envelope: Option<Value> = None;
            let mut finished: Option<RunOutcome> = None;
            // This iteration's dispatch wave, in COMMAND ORDER: every
            // Dispatch lands here — journal-answered replays as
            // Some(outcome), fresh pool submissions as None — and is fed
            // back to step() as one batch at the wave boundary below.
            let mut wave: Vec<(JournalKey, Option<EffectOutcome>)> = Vec::new();

            for cmd in out.commands {
                match cmd {
                    Command::WriteIntent { key, executor, input, superstep, clock_ms } => {
                        if let Some(existing) = intents.get(&key).copied() {
                            // Adopted: a crash re-opened this superstep. A
                            // REDELIVERY Observation is recorded only when
                            // the effect will actually redeliver (no result
                            // journaled) — a journal-answered replay is not
                            // a redelivery and must not fabricate one.
                            if results.contains_key(&key) {
                                continue;
                            }
                            let mut obs = Observation::new(&self.principal, "system")
                                .subject(&format!("run:{run_id}"))
                                .object(&format!(
                                    "redelivery of intent {} for {}",
                                    existing.to_hex(),
                                    key.tool_call_id()
                                ))
                                .namespace(HARNESS_NS)
                                .created_at(clock_ms as i64);
                            obs.common
                                .extra_fields
                                .insert("run_id".into(), json!(run_id));
                            self.facade
                                .with_store(|m| m.add_if_novel(&obs))
                                .map_err(err_run)?;
                            continue;
                        }
                        let h = self
                            .facade
                            .with_store(|m| {
                                journal::write_intent(
                                    m,
                                    &self.ns,
                                    &run_id,
                                    plan_hash,
                                    &key,
                                    &executor,
                                    &input,
                                    superstep,
                                    clock_ms,
                                    &self.principal,
                                )
                            })
                            .map_err(err_run)?;
                        emit(crate::stream::RunEvent::NodeDispatched {
                            superstep,
                            node: key.node.clone(),
                            task_path: key.task_path.clone(),
                            attempt: key.attempt,
                            effect_seq: key.effect_seq,
                        });
                        intents.insert(key, h);
                        intents_written += 1;
                        if opts.inject_crash == Some(CrashPoint::AfterIntent(intents_written)) {
                            return Err(RunError::Storage {
                                detail: "injected crash after intent write".into(),
                            });
                        }
                    }
                    Command::Dispatch { key, executor, input } => {
                        if let Some(outcome) = results.get(&key) {
                            // Replay: the journal answers; nothing executes.
                            // Resolved WITH the wave (not immediately), so a
                            // mixed replay+fresh superstep feeds one batch in
                            // dispatch order — the cadence verify replays.
                            wave.push((key, Some(outcome.clone())));
                            continue;
                        }
                        // Subgraphs run INLINE on the driver thread — the
                        // child needs the store, which pool workers never
                        // touch. Parallel subgraph siblings therefore
                        // serialize (documented v1 bound).
                        if let areev_run_core::NodeExecutor::Subgraph { workflow_hash } =
                            &executor
                        {
                            let outcome = self.run_subgraph_effect(
                                &run_id,
                                &key,
                                workflow_hash,
                                &input,
                                opts,
                            );
                            let intent = intents.get(&key).copied().ok_or_else(|| {
                                RunError::Storage {
                                    detail: "subgraph dispatch without intent".into(),
                                }
                            })?;
                            let now = self.clock.now_ms();
                            self.facade
                                .with_store(|m| {
                                    journal::write_result(
                                        m,
                                        &self.ns,
                                        &run_id,
                                        plan_hash,
                                        &intent,
                                        &key,
                                        &executor,
                                        &outcome,
                                        st.superstep,
                                        now,
                                        &self.principal,
                                    )
                                })
                                .map_err(err_run)?;
                            emit(crate::stream::RunEvent::EffectSettled {
                                superstep: st.superstep,
                                node: key.node.clone(),
                                task_path: key.task_path.clone(),
                                attempt: key.attempt,
                                effect_seq: key.effect_seq,
                                ok: matches!(outcome, EffectOutcome::Completed { .. }),
                            });
                            results.insert(key.clone(), outcome.clone());
                            wave.push((key, Some(outcome)));
                            continue;
                        }
                        // LLM effects: prepare the request ON THIS THREAD
                        // (transcript translation + pinned Definitions from
                        // the store), then hand the pure job to the pool.
                        let llm_prepared = if key.kind == areev_run_core::EffectKind::Llm {
                            let areev_run_core::NodeExecutor::Abstract { tools } = &executor
                            else {
                                return Err(RunError::Storage {
                                    detail: "llm effect on a non-abstract node".into(),
                                });
                            };
                            let mut defs = Vec::with_capacity(tools.len());
                            for t in tools {
                                let h = Hash::from_hex(&t.tool_hash).map_err(err_run)?;
                                let def = self
                                    .facade
                                    .with_store(|m| m.get(&h))
                                    .map_err(err_run)?
                                    .to_tool()
                                    .map_err(err_run)?;
                                defs.push(def);
                            }
                            // §6.10 TokenChunk sink: only when observed —
                            // otherwise the plain call (no stream overhead).
                            let on_token = bus.as_ref().map(|b| {
                                let b = Arc::clone(b);
                                let node = key.node.clone();
                                let task_path = key.task_path.clone();
                                let attempt = key.attempt;
                                Box::new(move |t: &str| {
                                    b.emit(crate::stream::RunEvent::TokenChunk {
                                        node: node.clone(),
                                        task_path: task_path.clone(),
                                        attempt,
                                        text: t.to_string(),
                                    });
                                }) as crate::executor::TokenSink
                            });
                            let to_model = self.pseudonymize_for_model(&input)?;
                            Some(crate::executor::PreparedLlm {
                                messages: seam_messages(&to_model),
                                tools: defs,
                                max_tokens: manifest.llm_max_tokens.unwrap_or(1024),
                                on_token,
                            })
                        } else {
                            None
                        };
                        // A code-carrying tool: read and hash-verify the blob
                        // HERE, on the driver thread, for the same reason the
                        // LLM branch resolves its Definitions here — the pool
                        // touches no store handle. `get_blob` verifies the
                        // digest on every read, so the bytes handed to the
                        // pool always are the address the manifest pinned.
                        let code_prepared = match manifest
                            .pinned
                            .iter()
                            .find(|p| p.node == key.node)
                            .filter(|p| p.executor_uri.is_some())
                        {
                            Some(pin) => {
                                let uri = pin.executor_uri.as_deref().unwrap();
                                let bytes = self
                                    .facade
                                    .with_store(|m| m.get_blob(uri))
                                    .map_err(err_run)?;
                                Some(crate::executor::PreparedCode {
                                    uri: uri.to_string(),
                                    bytes,
                                    // The manifest-frozen runtime rides with
                                    // the bytes, so a mid-run supersession
                                    // cannot re-route sandbox to native.
                                    runtime: pin.runtime.clone(),
                                    limits: pin.runtime_limits.clone(),
                                })
                            }
                            None => None,
                        };
                        let idem = idempotency_key(&key, &input);
                        // Rehydrate for the tool, never for the record: the
                        // intent above journaled the pseudonymized input and
                        // `idem` derives from it, so verify replays identically.
                        let input = match self.rehydrate_from_model(&input) {
                            Ok(v) => v,
                            Err(why) => {
                                // Fail the node, not the run: a plan's retries
                                // and edges still decide what happens next.
                                let outcome = EffectOutcome::Failed {
                                    cause: FailCause::ExecutorError,
                                    detail: why,
                                    journal_bytes: 0,
                                };
                                wave.push((key, Some(outcome)));
                                continue;
                            }
                        };
                        wave.push((key.clone(), None));
                        pool.submit(DispatchJob {
                            key,
                            executor,
                            input,
                            idempotency_key: idem,
                            llm: llm_prepared,
                            code: code_prepared,
                        });
                        in_flight += 1;
                    }
                    Command::EmitEnvelope { asks } => {
                        for ask in &asks {
                            emit(crate::stream::RunEvent::AskRaised {
                                superstep: st.superstep,
                                node: ask.node.clone(),
                                tool_call_id: ask.tool_call_id.clone(),
                            });
                        }
                        parked_envelope = Some(json!({
                            "kind": "requires_action",
                            "run_id": run_id,
                            "checkpoint": prev_ckpt.map(|h| h.to_hex()),
                            "asks": asks,
                        }));
                    }
                    Command::WriteCheckpoint { decision_record, state_json, .. } => {
                        let clock = decision_record
                            .clock_close_ms
                            .max(decision_record.clock_open_ms);
                        let superstep = decision_record.superstep;
                        // Journaled at each superstep boundary rather than at the
                        // terminal, so a crash loses at most one superstep of
                        // evidence. One Observation per DISTINCT refusal. A tool retrying against the same
                        // blocked host is one audit fact, not forty; the count
                        // of attempts stays in the log line, which is the fast
                        // path a human debugging actually reads.
                        for r in self.executor.refusals() {
                            let key = (r.caller.clone(), r.destination.clone(), r.reason.clone());
                            if !refusals_seen.insert(key) {
                                continue;
                            }
                            self.facade
                                .with_store(|m| {
                                    journal::write_egress_refusal(
                                        m,
                                        &run_id,
                                        &r,
                                        clock,
                                        &self.principal,
                                    )
                                })
                                .map_err(err_run)?;
                        }
                        let h = self
                            .facade
                            .with_store(|m| {
                                journal::write_checkpoint(
                                    m,
                                    &self.ns,
                                    &run_id,
                                    &state_json,
                                    &decision_record,
                                    clock,
                                    prev_ckpt.as_ref(),
                                    &self.principal,
                                )
                            })
                            .map_err(err_run)?;
                        emit(crate::stream::RunEvent::CheckpointWritten {
                            superstep,
                            hash: h.to_hex(),
                        });
                        prev_ckpt = Some(h);
                        // Renew on the superstep boundary — the point at which
                        // progress became durable, and therefore the point at
                        // which a driver that has lost the run must stop.
                        lease.renew(&self.facade, &run_id, self.clock.now_ms() as i64)?;
                    }
                    Command::Finish { outcome } => {
                        finished = Some(outcome);
                    }
                }
            }

            if let Some(outcome) = finished {
                // Terminal checkpoint: the Finished state itself.
                let state_json = serde_json::to_value(&st).unwrap_or(Value::Null);
                let record = DecisionRecord {
                    superstep: st.superstep,
                    clock_open_ms: st.clock_ms,
                    clock_close_ms: st.clock_ms,
                    ..Default::default()
                };
                self.facade
                    .with_store(|m| {
                        journal::write_checkpoint(
                            m,
                            &self.ns,
                            &run_id,
                            &state_json,
                            &record,
                            st.clock_ms,
                            prev_ckpt.as_ref(),
                            &self.principal,
                        )
                    })
                    .map_err(err_run)?;
                // The run-outcome record (§8 Wave 4): one compact
                // Observation per terminal run — outcome + the spent
                // figures — the loop's run_outcome analyzer's food and the
                // cost-attribution source. Journaled AFTER the terminal
                // checkpoint; replay never sees it (it is not a journal
                // entry), so verify is unaffected.
                let outcome_label = match &outcome {
                    RunOutcome::Completed => "completed".to_string(),
                    RunOutcome::Stalled { .. } => "stalled".to_string(),
                    RunOutcome::Failed { .. } => "failed".to_string(),
                    RunOutcome::Canceled { .. } => "canceled".to_string(),
                    RunOutcome::BudgetExhausted { axis } => {
                        format!("budget_exhausted:{}", axis.as_str())
                    }
                };
                let mut obs = Observation::new(&self.principal, "system")
                    .subject(&format!("run:{run_id}"))
                    .object(&outcome_label)
                    .namespace(HARNESS_NS)
                    .created_at(st.clock_ms as i64);
                let ex = &mut obs.common.extra_fields;
                ex.insert("run_id".into(), json!(run_id));
                ex.insert("observation_kind".into(), json!("run_outcome"));
                ex.insert("plan_hash".into(), json!(plan_hash.to_hex()));
                ex.insert("spent_input_tokens".into(), json!(st.spent.input_tokens));
                ex.insert("spent_output_tokens".into(), json!(st.spent.output_tokens));
                ex.insert("spent_usd_micros".into(), json!(st.spent.usd_micros));
                ex.insert("spent_wall_ms".into(), json!(st.spent.wall_ms));
                ex.insert("spent_supersteps".into(), json!(st.spent.supersteps));
                if let Some(detail) = match &outcome {
                    RunOutcome::Failed { node, detail } => {
                        Some(format!("{node}: {detail}"))
                    }
                    RunOutcome::Stalled { node } => Some(node.clone()),
                    _ => None,
                } {
                    ex.insert("outcome_detail".into(), json!(detail));
                }
                self.facade.with_store(|m| m.add(&obs)).map_err(err_run)?;
                emit(crate::stream::RunEvent::RunFinished {
                    run_id: run_id.clone(),
                    outcome: format!("{outcome:?}"),
                    dropped_events: bus.as_ref().map(|b| b.dropped()).unwrap_or(0),
                });
                // A terminal run needs no lease: releasing lets `verify`,
                // `fork` or an inspect start at once rather than waiting one
                // lease TTL for a run that is already over.
                lease.release(&self.facade);
                return Ok(RunSession::Finished { outcome, run_id });
            }

            if !wave.is_empty() {
                // The wave boundary: verify replays every superstep as ONE
                // close reading followed by its resolutions in dispatch
                // order, so the live driver must feed exactly that.
                // Trickling completions in arrival batches — each with its
                // own clock reading — makes scheduler state depend on
                // thread timing: an unjournaled decision that diverges
                // replay (and shifts every later scripted-clock value).
                let mut completed: BTreeMap<JournalKey, DispatchDone> = BTreeMap::new();
                while in_flight > 0 {
                    let done = pool
                        .done_rx
                        .recv()
                        .map_err(|_| RunError::Storage { detail: "executor pool died".into() })?;
                    in_flight -= 1;
                    results_seen += 1;
                    if opts.inject_crash == Some(CrashPoint::BeforeResult(results_seen)) {
                        // The effect FIRED (the executor ran); its result
                        // dies with this "process". The §5.3 redelivery
                        // contract owns what happens next.
                        return Err(RunError::Storage {
                            detail: "injected crash before result write".into(),
                        });
                    }
                    completed.insert(done.key.clone(), done);
                }
                let now = self.clock.now_ms();
                events.push(EventIn::ClockReading { unix_ms: now });
                for (key, replayed) in wave {
                    let outcome = match replayed {
                        Some(outcome) => outcome,
                        None => {
                            let done = completed.remove(&key).ok_or_else(|| {
                                RunError::Storage {
                                    detail: "completion missing for a dispatched effect".into(),
                                }
                            })?;
                            let intent = intents.get(&key).copied().ok_or_else(|| {
                                RunError::Storage { detail: "completion without intent".into() }
                            })?;
                            // Result durability precedes bookkeeping. The
                            // executor is the one the job DISPATCHED under
                            // (a flow tool runs as Host inside an Abstract
                            // node) — the result grain re-states what ran,
                            // matching its intent.
                            self.facade
                                .with_store(|m| {
                                    journal::write_result(
                                        m,
                                        &self.ns,
                                        &run_id,
                                        plan_hash,
                                        &intent,
                                        &key,
                                        &done.executor,
                                        &done.outcome,
                                        st.superstep,
                                        now,
                                        &self.principal,
                                    )
                                })
                                .map_err(err_run)?;
                            emit(crate::stream::RunEvent::EffectSettled {
                                superstep: st.superstep,
                                node: key.node.clone(),
                                task_path: key.task_path.clone(),
                                attempt: key.attempt,
                                effect_seq: key.effect_seq,
                                ok: matches!(done.outcome, EffectOutcome::Completed { .. }),
                            });
                            results.insert(key.clone(), done.outcome.clone());
                            done.outcome
                        }
                    };
                    events.push(EventIn::EffectResolved { key, outcome });
                }
                continue;
            }

            if let Some(envelope) = parked_envelope {
                // in_flight is 0 here: every dispatch drains with its wave.
                //
                // Release: a parked run is not a process. It is a journaled
                // checkpoint waiting on a human, so there is no in-memory state
                // to protect and any driver may pick it up — that portability is
                // the property the runtime is built on. Holding the lease across
                // a park would make the next `areev run resume`, in a different
                // process with a different holder id, wait out a full lease TTL
                // for a run nobody is advancing.
                lease.release(&self.facade);
                return Ok(RunSession::Parked { envelope, run_id });
            }

            // Parked with the envelope announced in an earlier session.
            if !st.pending_asks.is_empty() {
                lease.release(&self.facade);
                return Ok(RunSession::Parked {
                    envelope: json!({
                        "kind": "requires_action",
                        "run_id": run_id,
                        "checkpoint": prev_ckpt.map(|h| h.to_hex()),
                        "asks": st
                            .pending_asks
                            .values()
                            .map(|p| p.ask.clone())
                            .collect::<Vec<_>>(),
                    }),
                    run_id,
                });
            }
            return Err(RunError::Storage {
                detail: "driver made no progress with no work in flight".into(),
            });
        }
    }

    /// Shadow evaluation (§8 Wave 4): journal-consistent replay of the given
    /// runs, writing nothing and dispatching nothing — the verify path holds
    /// no executor, no pool, and no model, so "zero effect dispatches" is a
    /// property of the code shape, and the report states it explicitly for
    /// the §7.4 pre-apply gate. A run that fails to load is reported
    /// inconsistent, never skipped silently.
    pub fn shadow_eval(&self, run_ids: &[String]) -> crate::ShadowReport {
        let mut report = crate::ShadowReport::default();
        for run_id in run_ids {
            let row = match self.verify(run_id) {
                Ok(v) => {
                    let effects = self
                        .facade
                        .with_store(|m| journal::load(m, &self.ns, run_id))
                        .map(|view| {
                            view.entries.values().filter(|e| e.result.is_some()).count() as u64
                        })
                        .unwrap_or(0);
                    crate::ShadowRun {
                        run_id: run_id.clone(),
                        consistent: v.verified,
                        effects_replayed: effects,
                        steps: v.steps,
                    }
                }
                Err(e) => crate::ShadowRun {
                    run_id: run_id.clone(),
                    consistent: false,
                    effects_replayed: 0,
                    steps: vec![crate::VerifyStep {
                        superstep: 0,
                        verdict: format!("shadow load failed: {e}"),
                        ok: false,
                    }],
                },
            };
            report.runs.push(row);
        }
        report.all_consistent =
            !report.runs.is_empty() && report.runs.iter().all(|r| r.consistent);
        report.effect_dispatches = 0;
        report
    }

    /// The `run_id`s of the most recent runs in this namespace, newest
    /// first — manifest link Facts in `agent:harness` are the census.
    pub fn recent_runs(&self, limit: usize) -> Result<Vec<String>, RunError> {
        let mut out: Vec<(i64, String)> = self
            .facade
            .with_store(|m| {
                m.recent(
                    HARNESS_NS,
                    Some(areev_core::types::GrainType::Fact),
                    limit.saturating_mul(8).max(64),
                )
            })
            .map_err(err_run)?
            .into_iter()
            .filter(|g| g.get_str("relation") == Some("mg:harness"))
            .filter_map(|g| {
                let id = g.get_str("run_id")?.to_string();
                Some((g.get_i64("created_at").unwrap_or(0), id))
            })
            .collect();
        out.sort_by_key(|(at, _)| std::cmp::Reverse(*at));
        out.dedup_by(|a, b| a.1 == b.1);
        Ok(out.into_iter().map(|(_, id)| id).take(limit).collect())
    }

    /// One run's full picture for an operator UI (issue #34): the frozen
    /// manifest, terminal state, spend, journal size, pending asks, fork
    /// lineage. Read-only — the same report `areev run inspect` prints.
    pub fn inspect(&self, run_id: &str) -> Result<crate::InspectReport, RunError> {
        let manifest = self.load_manifest(run_id)?;
        let view = self
            .facade
            .with_store(|m| journal::load(m, &self.ns, run_id))
            .map_err(err_run)?;
        let last = view.checkpoints.last();
        let scheduler = last.map(|c| c.scheduler.clone()).unwrap_or(Value::Null);
        let asks = scheduler.get("pending_asks").cloned().unwrap_or(json!({}));
        let phase = scheduler.get("phase").and_then(|p| p.as_str().map(String::from)).or_else(
            || {
                scheduler.get("phase").map(|p| {
                    if p.get("Finished").is_some() { "finished".into() } else { "open".into() }
                })
            },
        );
        Ok(crate::InspectReport {
            run_id: run_id.to_string(),
            plan_hash: manifest.plan_hash.clone(),
            principal: manifest.principal.clone(),
            pinned: manifest
                .pinned
                .iter()
                .map(|p| json!({"node": p.node, "tool": p.tool_name, "executor": p.executor}))
                .collect(),
            budgets: serde_json::to_value(manifest.budgets).unwrap_or(Value::Null),
            fork_of: manifest
                .fork_of
                .as_ref()
                .map(|f| json!({"base_run": f.base_run, "base_superstep": f.base_superstep})),
            checkpoints: view.checkpoints.len(),
            journal_entries: view.entries.len(),
            superstep: scheduler.get("superstep").cloned(),
            phase,
            spent: scheduler.get("spent").cloned(),
            pending_asks: asks,
        })
    }

    /// The EU AI Act Article 14 report for `run_id` (or the newest run of
    /// `plan`, or the newest run overall when neither is given), measured
    /// from the journal rather than asserted (issue #34): where a human can
    /// intervene, who is authorized to, what expires when, and how fast the
    /// kill switch actually drained. Read-only — the same report
    /// `areev run oversight-report` prints.
    pub fn oversight_report(
        &self,
        run_id: Option<&str>,
        plan: Option<&Hash>,
    ) -> Result<crate::OversightReport, RunError> {
        let target_run = match (run_id, plan) {
            (Some(id), _) => Some(id.to_string()),
            (None, Some(plan)) => {
                let plan_hex = plan.to_hex();
                self.recent_runs(64)?.into_iter().find(|id| {
                    self.load_manifest(id).map(|mf| mf.plan_hash == plan_hex).unwrap_or(false)
                })
            }
            (None, None) => self.recent_runs(1)?.into_iter().next(),
        };
        let Some(run_id) = target_run else {
            return Err(RunError::UnresolvedRef {
                what: "no runs recorded yet (or none for that plan) — nothing to report on"
                    .into(),
            });
        };
        let manifest = self.load_manifest(&run_id)?;
        let gated: Vec<Value> = manifest
            .pinned
            .iter()
            .filter(|p| p.executor == "client")
            .map(|p| json!({"node": p.node, "tool": p.tool_name}))
            .collect();
        // Authorized responders: principals granted run.respond in the file.
        let responders: Vec<String> = self
            .facade
            .with_store(|m| {
                m.recent("agent:authz", Some(areev_core::types::GrainType::Fact), 4096)
            })
            .map(|rows| {
                rows.into_iter()
                    .filter(|g| {
                        g.get_str("relation") == Some("mg:permits")
                            && g.get_str("object").is_some_and(|o| o.contains("run.respond"))
                    })
                    .filter_map(|g| g.get_str("subject").map(String::from))
                    .collect()
            })
            .unwrap_or_default();
        // Kill-switch time, measured: cancel Fact created_at → terminal
        // checkpoint created_at, over every canceled run we can see.
        let mut cancel_ms: Vec<i64> = Vec::new();
        for id in self.recent_runs(64).unwrap_or_default() {
            let cancel_at = self
                .facade
                .with_store(|m| {
                    m.latest("agent:harness", &format!("run:{id}"), "mg:cancel").ok().flatten()
                })
                .and_then(|g| g.get_i64("created_at"));
            let Some(cancel_at) = cancel_at else { continue };
            if let Ok(view) = self.facade.with_store(|m| journal::load(m, &self.ns, &id)) {
                if let Some(last) = view.checkpoints.last() {
                    let drained_at = last.decisions.clock_close_ms as i64;
                    if drained_at >= cancel_at {
                        cancel_ms.push(drained_at - cancel_at);
                    }
                }
            }
        }
        Ok(crate::OversightReport {
            run_id,
            plan_hash: manifest.plan_hash.clone(),
            human_gates: json!({
                "client_gated_nodes": gated,
                "every_client_ask_is_an_approval": true,
                "separation_of_duties": "responder != triggering principal, refused structurally",
                "ask_ttl_sec": manifest.ask_ttl_sec,
            }),
            authorized_responders: json!({
                "principals_granted_run_respond": responders,
                "note": "empty = owner-only sessions (single-user mode); grants live IN the file as mg:permits Facts",
            }),
            budgets: serde_json::to_value(manifest.budgets).unwrap_or(Value::Null),
            kill_switch: json!({
                "verb": "run.cancel (deliberately the lowest-privilege run verb)",
                "measured_cancel_to_drain_ms": cancel_ms,
                "note": "measured from journaled cancel Facts to the terminal checkpoint's journaled close",
            }),
        })
    }

    /// §5.5 gate 1 / verify tier 1: **journal-consistent replay**. Re-drives
    /// the run from the manifest's input with the clock scripted from
    /// journaled readings and every effect answered from the journal —
    /// writing NOTHING — and byte-compares every checkpoint the replay
    /// commands against the stored chain. A missing journaled result stops
    /// verification at that step (reported, not asserted); a divergent
    /// checkpoint is the `RUN-E009` condition, reported with both hashes.
    pub fn verify(&self, run_id: &str) -> Result<VerifyReport, RunError> {
        let manifest = self.load_manifest(run_id)?;
        let plan_hash = Hash::from_hex(&manifest.plan_hash)
            .map_err(|_| RunError::ManifestMismatch { why: "bad plan hash".into() })?;
        let plan = self.load_plan(&plan_hash)?;
        let view = self
            .facade
            .with_store(|m| journal::load(m, &self.ns, run_id))
            .map_err(err_run)?;
        let executors = manifest.executors();
        let arg_schemas = self.load_arg_schemas(&manifest)?;
        let validate_args = make_validate_args(&arg_schemas);
        let reduce = crate::reducers::make_reduce(&manifest.reducers);
        let env = StepEnv {
            plan: &plan,
            executors: &executors,
            budgets: manifest.budgets.to_budgets(),
            reduce: &reduce,
            eval_cond: &builtin_eval,
            ask_ttl_sec: manifest.ask_ttl_sec,
            validate_args: &validate_args,
            llm_reserve_tokens: manifest.llm_reserve_tokens(),
            max_effects_per_attempt: 16,
        };

        let mut report = VerifyReport::default();
        if view.checkpoints.is_empty() {
            report.steps.push(VerifyStep {
                superstep: 0,
                verdict: "no checkpoints — nothing to verify".into(),
                ok: false,
            });
            return Ok(report);
        }

        // A fork replays its TAIL: the seed checkpoint is trusted through
        // the §5.4 base-checkpoint hash chain (it `derived_from`s the base),
        // and replay verifies everything the fork itself executed.
        let (mut st, mut ckpt_idx, mut events);
        if manifest.fork_of.is_some() {
            st = serde_json::from_value::<SchedulerState>(view.checkpoints[0].scheduler.clone())
                .map_err(|e| RunError::ManifestMismatch {
                    why: format!("fork seed checkpoint: {e}"),
                })?;
            ckpt_idx = 1;
            if view.checkpoints.len() == 1 {
                report.steps.push(VerifyStep {
                    superstep: st.superstep,
                    verdict: format!(
                        "fork seed checkpoint {} chains to base {} — the fork has \
                         not executed yet",
                        view.checkpoints[0].hash.to_hex(),
                        manifest.fork_of.as_ref().map(|f| f.base_checkpoint.as_str()).unwrap_or("?"),
                    ),
                    ok: true,
                });
                report.verified = true;
                return Ok(report);
            }
            events = vec![EventIn::ClockReading {
                unix_ms: view.checkpoints[1].decisions.clock_open_ms,
            }];
        } else {
            st = SchedulerState::new(run_id, &plan);
            ckpt_idx = 0;
            // The first reading is the first superstep's journaled open.
            events = vec![
                EventIn::ClockReading {
                    unix_ms: view.checkpoints[0].decisions.clock_open_ms,
                },
                EventIn::Start { input: manifest.input.clone() },
            ];
        }
        // Cancel is an OUT-OF-BAND marker (drive polls the Fact; the
        // journal has no CancelSeen entry) — replay it from where it IS
        // journaled: the checkpoint state. WHERE inside the superstep the
        // live driver saw it is read off the journal's own evidence: the
        // driver polls at wave boundaries, so a checkpoint that carries
        // both this superstep's resolutions and the cancel means the
        // marker rode the SAME event batch as the resolutions; a step
        // whose dispatches the journal never answered, while the coming
        // checkpoint carries the cancel, means the live driver canceled
        // BEFORE dispatching — that boundary is rewound and re-fed with
        // CancelSeen up front. Feeding it blindly at the superstep's open
        // (the old rule) replayed a phase the live driver can only
        // produce when the marker predates the run.
        let cancel_peek = |ckpt_idx: usize, st: &SchedulerState| -> Option<EventIn> {
            if st.cancel.is_some() {
                return None;
            }
            let c = view.checkpoints.get(ckpt_idx)?.scheduler.get("cancel")?.as_array()?;
            Some(EventIn::CancelSeen {
                principal: c.first()?.as_str()?.to_string(),
                reason: c.get(1)?.as_str()?.to_string(),
            })
        };
        let mut guard = 0;
        'replay: loop {
            guard += 1;
            if guard > 100_000 {
                report.steps.push(VerifyStep {
                    superstep: st.superstep,
                    verdict: "replay did not terminate".into(),
                    ok: false,
                });
                return Ok(report);
            }
            let pre_state = st.clone();
            let pre_events = events.clone();
            let pre_ckpt_idx = ckpt_idx;
            let pre_report_len = report.steps.len();
            let out = step(&env, st, &events);
            st = out.state;
            events = Vec::new();
            let mut resolved: Vec<EventIn> = Vec::new();
            let mut parked = false;
            let mut unanswered: Option<JournalKey> = None;
            for cmd in out.commands {
                match cmd {
                    Command::WriteIntent { .. } => {}
                    Command::Dispatch { key, .. } => {
                        match view.entries.get(&key).and_then(|e| {
                            e.result.as_ref().map(|(_, o)| (o.clone(), e.result_clock_ms))
                        }) {
                            Some((outcome, _clock)) => {
                                resolved.push(EventIn::EffectResolved { key, outcome });
                            }
                            None => {
                                unanswered = Some(key);
                                break;
                            }
                        }
                    }
                    Command::EmitEnvelope { .. } => {
                        parked = true;
                    }
                    Command::WriteCheckpoint { superstep, state_json, .. } => {
                        let Some(stored) = view.checkpoints.get(ckpt_idx) else {
                            report.steps.push(VerifyStep {
                                superstep,
                                verdict: "replay produced a checkpoint the journal lacks"
                                    .into(),
                                ok: false,
                            });
                            break 'replay;
                        };
                        ckpt_idx += 1;
                        let ok = stored.superstep == superstep
                            && stored.scheduler == state_json;
                        report.steps.push(VerifyStep {
                            superstep,
                            verdict: if ok {
                                format!(
                                    "checkpoint {} journal-consistent",
                                    stored.hash.to_hex()
                                )
                            } else {
                                format!(
                                    "checkpoint {} DIVERGES from replay (RUN-E009): {}",
                                    stored.hash.to_hex(),
                                    diff_fields(&stored.scheduler, &state_json)
                                )
                            },
                            ok,
                        });
                        if !ok {
                            break 'replay;
                        }
                    }
                    Command::Finish { .. } => {}
                }
            }
            if let Some(key) = unanswered {
                if let Some(cancel_ev) = cancel_peek(pre_ckpt_idx, &pre_state) {
                    // The live driver canceled at this boundary BEFORE
                    // dispatching (the journal never answered these
                    // effects, and the coming checkpoint carries the
                    // cancel): rewind and re-feed the same boundary with
                    // CancelSeen — the batch the live driver actually saw.
                    st = pre_state;
                    ckpt_idx = pre_ckpt_idx;
                    report.steps.truncate(pre_report_len);
                    events = pre_events;
                    events.push(cancel_ev);
                    continue;
                }
                report.steps.push(VerifyStep {
                    superstep: st.superstep,
                    verdict: format!(
                        "effect {} has no journaled result — not \
                         verifiable past this point",
                        key.tool_call_id()
                    ),
                    ok: false,
                });
                break 'replay;
            }
            if st.is_terminal() {
                // The terminal checkpoint (written by drive on Finish).
                if let Some(stored) = view.checkpoints.get(ckpt_idx) {
                    let replay_json = serde_json::to_value(&st).unwrap_or(Value::Null);
                    let ok = stored.scheduler == replay_json;
                    report.steps.push(VerifyStep {
                        superstep: st.superstep,
                        verdict: if ok {
                            format!(
                                "terminal checkpoint {} journal-consistent",
                                stored.hash.to_hex()
                            )
                        } else {
                            format!(
                                "terminal checkpoint {} DIVERGES from replay (RUN-E009)",
                                stored.hash.to_hex()
                            )
                        },
                        ok,
                    });
                }
                break 'replay;
            }
            if !resolved.is_empty() {
                // The close-time reading: this superstep's journaled close.
                let close = view
                    .checkpoints
                    .get(ckpt_idx)
                    .map(|c| c.decisions.clock_close_ms)
                    .unwrap_or(st.clock_ms);
                events.push(EventIn::ClockReading { unix_ms: close });
                events.extend(resolved);
                // The checkpoint these resolutions close may also carry
                // the cancel: the live driver polls the marker at wave
                // boundaries, so it saw it WITH this batch — CancelSeen
                // rides the same feed, never a boundary of its own.
                if let Some(cancel_ev) = cancel_peek(ckpt_idx, &st) {
                    events.push(cancel_ev);
                }
                continue;
            }
            if parked {
                // Settle parked asks from the journal. The reading fed here
                // is the NEXT stored checkpoint's journaled close — the
                // scheduler accrues park-elapsed at close from that reading
                // (never from response-apply time, which is not journaled),
                // so this is exactly what makes the replayed state byte-equal.
                let mut settled_any = false;
                let close = view
                    .checkpoints
                    .get(ckpt_idx)
                    .map(|c| c.decisions.clock_close_ms)
                    .unwrap_or(st.clock_ms);
                for (id, pending) in st.pending_asks.clone() {
                    if let Some(entry) = view.entries.get(&pending.key) {
                        if let Some((_, outcome)) = &entry.result {
                            if !settled_any {
                                events.push(EventIn::ClockReading { unix_ms: close });
                            }
                            events.push(EventIn::ResponseSettled {
                                tool_call_id: id,
                                outcome: outcome.clone(),
                            });
                            settled_any = true;
                        }
                    }
                }
                if settled_any {
                    continue;
                }
                // A cancel that arrived while parked: the next checkpoint
                // carries it — feed it (with its journaled close) so the
                // drain and terminal checkpoints verify too, instead of
                // breaking here and silently skipping them.
                let cancel_next = view.checkpoints.get(ckpt_idx).and_then(|c| {
                    c.scheduler.get("cancel").and_then(|v| v.as_array()).and_then(|a| {
                        Some((
                            a.first()?.as_str()?.to_string(),
                            a.get(1)?.as_str()?.to_string(),
                        ))
                    })
                });
                if st.cancel.is_none() {
                    if let Some((by, reason)) = cancel_next {
                        events.push(EventIn::ClockReading { unix_ms: close });
                        events.push(EventIn::CancelSeen { principal: by, reason });
                        continue;
                    }
                }
                report.steps.push(VerifyStep {
                    superstep: st.superstep,
                    verdict: if ckpt_idx < view.checkpoints.len() {
                        format!(
                            "run is parked awaiting a response — verified up to                              the park; {} trailing checkpoint(s) NOT verified",
                            view.checkpoints.len() - ckpt_idx
                        )
                    } else {
                        "run is parked awaiting a response — verified up to the park".into()
                    },
                    ok: ckpt_idx >= view.checkpoints.len(),
                });
                break 'replay;
            }
            // No dispatches, not parked, not terminal: feed the next open
            // reading (a between-supersteps boundary).
            if let Some(next) = view.checkpoints.get(ckpt_idx) {
                events.push(EventIn::ClockReading {
                    unix_ms: next.decisions.clock_open_ms,
                });
                continue;
            }
            break 'replay;
        }
        report.verified = !report.steps.is_empty() && report.steps.iter().all(|s| s.ok);
        Ok(report)
    }
}
