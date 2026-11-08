//! `AreevSubstrate` — implements `areev_loop::OmsSubstrate` over `AreevFacade`.
//!
//! Two wrapper types share one set of operations (free functions over
//! `&AreevFacade`): [`AreevSubstrate`] **owns** its facade (the CLI path,
//! which moves a store in), while [`BorrowedSubstrate`] holds `&AreevFacade`
//! (the bindings path, where the pyclass/napi object already owns the facade
//! and a second store handle would violate single-writer-per-file). Every
//! operation goes through the facade's `&self` methods (`with_store`,
//! `cal_add`/`cal_supersede`/`cal_delete`), so a shared borrow suffices even
//! for the trait's `&mut self` writers.
//!
//! Two mapping decisions, both interim and documented:
//!
//! 1. **Areev Loop-internal grains ride as Facts.** The recommendation grain
//!    (OMS 0x0C) is not yet realized in areev-core, so recommendation and
//!    audit grains are stored as Facts in the `areev-loop` namespace with the full
//!    loop field-map serialized into the Fact's `object`, tagged by
//!    `relation` (`loop_recommendation` / `loop_audit`). They are real,
//!    content-addressed, syncable grains; only the type byte differs.
//! 2. **Liveness comes from `derived_from`.** `supersede` stamps the new
//!    grain's `derived_from` with the old hash, so a grain is superseded iff
//!    its hash appears in some sibling's `derived_from`.
//!
//! Type reads go through the store's bounded, paged
//! `grains_of_type_since` (#430): a read with `since_ms` touches only grains
//! created at or after it, so a gated Loop pass with nothing new reads nothing
//! old. A read with `since_ms = None` — the first pass, or a `full_sweep` —
//! is the one that loads every grain of the type, and says so.

use areev_cal::classify::{classify, StatementClass};
use areev_cal::{parse, CalExecutor, CalExecutorConfig, CalStoreFacade, AreevFacade};
use areev_core::error::{AreevError, Hash};
use areev_core::format::deserialize::DeserializedGrain;
use areev_core::authz::Verb;
use areev_core::types::{Fact, Grain, GrainType, Workflow, WorkflowEdge};
use areev_store::{Areev, TelemetryMode};
use serde_json::{Map, Value};
use std::collections::BTreeSet;
use areev_loop::error::{Error as WErr, Result as WResult};
use areev_loop::{
    BudgetUsage, Capabilities, GrainAccess, GrainRecord, GrainSpec, HeadGroup, OmsSubstrate,
    QueryUsage, ReadOpts, SubstrateRead, TelemetryView, ToolTally,
};

/// The namespace the loop's own grains live in (mirrors `areev_loop::LOOP_NS`).
const LOOP_NS: &str = "areev-loop";
/// Sentinel subject/relation for the persisted loop state blob.
const STATE_SUBJECT: &str = "__loop_state__";
const STATE_RELATION: &str = "state";
/// Upper bound on grains scanned per `recent` query (the loop-internal reads).
const MAX_SCAN: usize = 1_000_000;
/// Grains fetched per page of a type read (#430). The bound applies per page,
/// so a page's blobs are released before the next is fetched — and a page is
/// the whole resident footprint of a fold (#433), so it is kept small: a Tool
/// body decodes to several times its stored size when its content is
/// structured.
const PAGE: usize = 128;

fn we(e: AreevError) -> WErr {
    WErr::Substrate(e.to_string())
}

/// A substrate error from this adapter's own checks (no underlying
/// `AreevError` to wrap) — plan validation and pin resolution.
fn werr(msg: &str) -> WErr {
    WErr::Substrate(msg.to_string())
}

fn require_read(f: &AreevFacade, ns: &str) -> WResult<()> {
    f.authz().check(Verb::Read, ns).map_err(we)
}

fn may_read(f: &AreevFacade, ns: &str) -> bool {
    f.authz().allows(Verb::Read, ns)
}

/// An Areev-backed substrate that owns its facade (CLI path).
pub struct AreevSubstrate {
    facade: AreevFacade,
}

impl AreevSubstrate {
    /// Wrap an open store, scoping the loop's own writes to `session_ns`
    /// (defaults to the `areev-loop` namespace when `None`).
    pub fn new(store: Areev, session_ns: Option<String>) -> Self {
        let ns = session_ns.unwrap_or_else(|| LOOP_NS.to_string());
        AreevSubstrate {
            facade: AreevFacade::with_session(store, Some(ns), None),
        }
    }

    /// Recover the underlying store (e.g. to reuse it for another verb).
    pub fn into_store(self) -> Areev {
        self.facade.into_inner()
    }

    /// The session's rights, for the coverage filter (#312).
    pub fn authz(&self) -> areev_core::authz::AuthzSet {
        self.facade.authz()
    }
}

/// An Areev-backed substrate that borrows a facade (bindings path). Construct
/// one per areev-loop call over the host object's existing `&AreevFacade`.
pub struct BorrowedSubstrate<'a> {
    facade: &'a AreevFacade,
}

impl<'a> BorrowedSubstrate<'a> {
    pub fn new(facade: &'a AreevFacade) -> Self {
        BorrowedSubstrate { facade }
    }
}

impl AreevSubstrate {
    fn facade_ref(&self) -> &AreevFacade {
        &self.facade
    }

    /// Store-level joins the loop vocabulary doesn't cover (the CLI's §7.4
    /// blast-radius/provenance reports need `runs_touching`). Read-only use.
    pub fn facade(&self) -> &AreevFacade {
        &self.facade
    }
}

impl BorrowedSubstrate<'_> {
    fn facade_ref(&self) -> &AreevFacade {
        self.facade
    }
}

// The two wrappers implement the traits identically by delegating to the free
// functions below (each operation borrows the facade only for its own call).
macro_rules! impl_substrate {
    ($ty:ty) => {
        impl SubstrateRead for $ty {
            fn capabilities(&self) -> Capabilities {
                caps(self.facade_ref())
            }
            fn grains_of_type(
                &self,
                grain_type: &str,
                namespace: Option<&str>,
                opts: ReadOpts,
            ) -> WResult<Vec<GrainRecord>> {
                grains_of_type(self.facade_ref(), grain_type, namespace, opts)
            }
            fn visit_grains_of_type(
                &self,
                grain_type: &str,
                namespace: Option<&str>,
                opts: ReadOpts,
                visit: &mut dyn FnMut(GrainRecord) -> WResult<()>,
            ) -> WResult<BTreeSet<String>> {
                visit_grains_of_type(self.facade_ref(), grain_type, namespace, opts, visit)
            }
            fn count_of_type(
                &self,
                grain_type: &str,
                namespace: Option<&str>,
                since_ms: Option<i64>,
            ) -> WResult<u64> {
                count_of_type(self.facade_ref(), grain_type, namespace, since_ms)
            }
            fn tool_definitions(&self, namespace: Option<&str>) -> WResult<Vec<GrainRecord>> {
                tool_definitions(self.facade_ref(), namespace)
            }
            fn tool_tallies(
                &self,
                namespace: Option<&str>,
                opts: ReadOpts,
            ) -> WResult<Option<std::collections::BTreeMap<String, ToolTally>>> {
                tool_tallies(self.facade_ref(), namespace, opts)
            }
            fn tool_errors(
                &self,
                namespace: Option<&str>,
                opts: ReadOpts,
            ) -> WResult<Option<Vec<GrainRecord>>> {
                tool_errors(self.facade_ref(), namespace, opts)
            }
            fn event_sessions_since(&self, since_ms: Option<i64>) -> WResult<Option<u64>> {
                event_sessions_since(self.facade_ref(), since_ms)
            }
            fn grain(&self, hash: &str) -> WResult<Option<GrainRecord>> {
                grain(self.facade_ref(), hash)
            }
            fn validate_plan(&self, workflow: &Value) -> WResult<()> {
                validate_plan(workflow)
            }
            fn tool_evalset(&self, tool: &str) -> WResult<Option<String>> {
                tool_evalset(self.facade_ref(), tool)
            }
            fn embed(&self, text: &str) -> WResult<Option<Vec<f32>>> {
                self.facade_ref().with_store(|m| m.embed_text(text)).map_err(we)
            }
            fn address_of(&self, spec: &GrainSpec) -> WResult<Option<String>> {
                address_of(spec)
            }
            fn plan_replay(&self, incumbent: &str, candidate: &Value) -> WResult<Option<Value>> {
                plan_replay(self.facade_ref(), incumbent, candidate)
            }
            fn heads(&self, namespace: Option<&str>) -> WResult<Vec<HeadGroup>> {
                heads(self.facade_ref(), namespace)
            }
            fn telemetry(&self, namespace: Option<&str>) -> WResult<Option<TelemetryView>> {
                telemetry(self.facade_ref(), namespace)
            }
        }

        impl OmsSubstrate for $ty {
            fn put_grain(&mut self, spec: &GrainSpec) -> WResult<String> {
                put_grain(self.facade_ref(), spec)
            }
            fn supersede(
                &mut self,
                target_hash: &str,
                spec: &GrainSpec,
                _j: &str,
            ) -> WResult<String> {
                supersede_op(self.facade_ref(), target_hash, spec)
            }
            fn retract(&mut self, hash: &str, _reason: &str) -> WResult<()> {
                retract_op(self.facade_ref(), hash)
            }
            fn execute_cal(&mut self, cal: &str) -> WResult<Vec<Value>> {
                execute_cal(self.facade_ref(), cal)
            }
            fn validate_cal(&self, cal: &str) -> WResult<()> {
                validate_cal(cal)
            }
            fn definition_inverse(&self, statement: &str) -> WResult<Option<String>> {
                definition_inverse(self.facade_ref(), statement)
            }
            fn load_state(&self) -> WResult<Value> {
                load_state(self.facade_ref())
            }
            fn store_state(&mut self, state: &Value) -> WResult<()> {
                store_state(self.facade_ref(), state)
            }
            fn put_blob(&mut self, bytes: &[u8]) -> WResult<String> {
                put_blob_op(self.facade_ref(), bytes)
            }
            fn get_blob(&mut self, address: &str) -> WResult<Vec<u8>> {
                get_blob_op(self.facade_ref(), address)
            }
        }
    };
}

impl_substrate!(AreevSubstrate);
impl_substrate!(BorrowedSubstrate<'_>);

// --- operations (free functions over &AreevFacade) ---

fn recent_ns(f: &AreevFacade, ns: &str, gt: GrainType) -> WResult<Vec<DeserializedGrain>> {
    // Same capability assertion as `read_user_type`'s explicit namespace.
    require_read(f, ns)?;
    f.with_store(|m| m.recent(ns, Some(gt), MAX_SCAN))
        .map_err(we)
}

/// Read user grains of `gt`, paged through the store's bounded
/// `grains_of_type_since` (#430).
///
/// With `since_ms` set, only grains created at or after it are fetched — a
/// gated pass over a large memory with nothing new costs one empty index
/// probe per type, not a read of every grain. With `since_ms = None` this is
/// the whole-type load (the first pass, or a `full_sweep`): every grain of the
/// type is read, a page at a time, and the records kept are what the caller
/// asked for.
///
/// Liveness is decided after paging, as it always was: a grain is superseded
/// when some grain read names it in `derived_from` (mapping decision 2). The
/// store's `superseded_by` column is deliberately NOT used — `forget` of a
/// superseding grain (a Loop rollback) leaves it set on the predecessor, while
/// the Loop treats the predecessor as live again and must re-propose.
fn read_user_type(
    f: &AreevFacade,
    gt: GrainType,
    namespace: Option<&str>,
    opts: ReadOpts,
) -> WResult<Vec<GrainRecord>> {
    let mut out = Vec::new();
    let retired = visit_user_type(f, gt, namespace, opts, &mut |r| {
        out.push(r);
        Ok(())
    })?;
    if !retired.is_empty() {
        out.retain(|r| !retired.contains(&r.hash));
    }
    Ok(out)
}

/// The paged read behind [`read_user_type`], one record at a time (#433).
///
/// Holds a page of blobs and the `derived_from` parents seen, never the
/// records: those go to `visit` as each page is decoded. Liveness is
/// therefore returned rather than applied — with `live_only`, the parents of
/// every grain read are the hashes a live caller must discount, since a grain
/// is retired by one read after it. Without `live_only` the set is empty.
fn visit_user_type(
    f: &AreevFacade,
    gt: GrainType,
    namespace: Option<&str>,
    opts: ReadOpts,
    visit: &mut dyn FnMut(GrainRecord) -> WResult<()>,
) -> WResult<BTreeSet<String>> {
    let explicit = namespace.is_some();
    if let Some(ns) = namespace {
        // An explicitly requested namespace is a capability assertion. Deny it
        // loudly; silently returning an empty set would misreport
        // authorization failure as "the analyzer found no data".
        require_read(f, ns)?;
    }
    let mut parents: BTreeSet<String> = BTreeSet::new();
    let mut cursor = None;
    loop {
        let page = f
            .with_store(|m| {
                m.grains_of_type_since(gt, namespace, opts.since_ms, cursor, PAGE, false)
            })
            .map_err(we)?;
        let next = page.next;
        for g in page.grains {
            // Every grain read counts toward liveness, filtered or not — a
            // supersession recorded in a namespace this scan skips still
            // retires its target.
            if opts.live_only {
                parents.extend(derived_parents(&g));
            }
            if !visible(f, &g, explicit) {
                continue;
            }
            let ns = grain_namespace(&g);
            let created = grain_created_ms(&g);
            // The column and the blob agree on `created_at` for every grain
            // written with one; this keeps the substrate's contract exact for
            // any that do not.
            if opts.since_ms.is_some_and(|s| created < s) {
                continue;
            }
            visit(into_user_record(g, ns, created))?;
        }
        match next {
            Some(n) => cursor = Some(n),
            None => break,
        }
    }
    Ok(parents)
}

/// May an all-namespace scan surface this grain?
///
/// Never loop-internal or governance grains in ALL-NAMESPACE scans.
/// `agent:authz` holds the file's own grant Facts and the Tier-2 audit
/// Observations; an analyzer sweeping everything that sees them as ordinary
/// memory can propose tombstoning them (retention_sweep did), which locks every
/// non-owner principal out of the file and destroys the very Art. 30 records
/// the audit export exists to produce. Governance state is metadata about the
/// memory, not memory. An EXPLICITLY requested namespace is different: the
/// caller named it, `require_read` checked it, and consumers like the
/// run_outcome analyzer (agent:harness) and the gating-evidence loader depend
/// on reading it.
fn visible(f: &AreevFacade, g: &DeserializedGrain, explicit: bool) -> bool {
    explicit || ns_visible(f, &grain_namespace(g))
}

fn ns_visible(f: &AreevFacade, ns: &str) -> bool {
    !(ns == LOOP_NS || ns.starts_with("agent:") || !may_read(f, ns))
}

fn visit_grains_of_type(
    f: &AreevFacade,
    grain_type: &str,
    namespace: Option<&str>,
    opts: ReadOpts,
    visit: &mut dyn FnMut(GrainRecord) -> WResult<()>,
) -> WResult<BTreeSet<String>> {
    match user_grain_type(grain_type, namespace) {
        Some(gt) => visit_user_type(f, gt, namespace, opts, visit),
        // The loop's own ledger reads are small and already one materialised
        // listing; visiting them is the trait's default shape.
        None => {
            for g in grains_of_type(f, grain_type, namespace, opts)? {
                visit(g)?;
            }
            Ok(BTreeSet::new())
        }
    }
}

/// How many grains of a type since `since_ms` an all-namespace read would
/// surface, from the store's index — no blob read (#433). A named namespace
/// (a scope the index count cannot apply the same way) counts the visit.
fn count_of_type(
    f: &AreevFacade,
    grain_type: &str,
    namespace: Option<&str>,
    since_ms: Option<i64>,
) -> WResult<u64> {
    let opts = ReadOpts { live_only: false, since_ms };
    match (user_grain_type(grain_type, namespace), namespace) {
        (Some(gt), None) => {
            let per_ns = f.with_store(|m| m.count_of_type_since(gt, since_ms)).map_err(we)?;
            Ok(per_ns
                .into_iter()
                .filter(|(ns, _)| ns_visible(f, ns))
                .map(|(_, n)| n)
                .sum())
        }
        _ => {
            let mut n = 0u64;
            visit_grains_of_type(f, grain_type, namespace, opts, &mut |_| {
                n += 1;
                Ok(())
            })?;
            Ok(n)
        }
    }
}

/// Live Tool Definitions through the store's definition index (#432) — the
/// call grains are never read. Visibility as for every user read; liveness
/// as `read_user_type` decides it (a Definition retired by a later one's
/// `derived_from`), among the Definitions.
fn tool_definitions(f: &AreevFacade, namespace: Option<&str>) -> WResult<Vec<GrainRecord>> {
    let explicit = namespace.is_some();
    if let Some(ns) = namespace {
        require_read(f, ns)?;
    }
    let defs = f.with_store(|m| m.tool_definitions(namespace)).map_err(we)?;
    let parents: BTreeSet<String> = defs.iter().flat_map(derived_parents).collect();
    Ok(defs
        .into_iter()
        .filter(|g| visible(f, g, explicit) && !parents.contains(&g.hash.to_hex()))
        .map(|g| {
            let ns = grain_namespace(&g);
            let created = grain_created_ms(&g);
            into_user_record(g, ns, created)
        })
        .collect())
}

/// Per-tool counts from the store's Tool index (#433) — no call grain read.
/// `None` when the store cannot answer from the index (not built yet on this
/// handle, or an anonymization policy is live); the engine then folds.
fn tool_tallies(
    f: &AreevFacade,
    namespace: Option<&str>,
    opts: ReadOpts,
) -> WResult<Option<std::collections::BTreeMap<String, ToolTally>>> {
    if let Some(ns) = namespace {
        require_read(f, ns)?;
    }
    let Some(rows) = f
        .with_store(|m| m.tool_counts(namespace, opts.since_ms, opts.live_only))
        .map_err(we)?
    else {
        return Ok(None);
    };
    let mut out: std::collections::BTreeMap<String, ToolTally> = Default::default();
    for r in rows {
        if namespace.is_none() && !ns_visible(f, &r.namespace) {
            continue;
        }
        let t = out.entry(r.tool).or_default();
        t.calls += r.calls;
        t.errors += r.errors;
    }
    Ok(Some(out))
}

/// The error Tool grains from the store's Tool index (#433) — only they are
/// decoded. Visibility as for every user read.
fn tool_errors(f: &AreevFacade, namespace: Option<&str>, opts: ReadOpts) -> WResult<Option<Vec<GrainRecord>>> {
    let explicit = namespace.is_some();
    if let Some(ns) = namespace {
        require_read(f, ns)?;
    }
    let Some(grains) = f
        .with_store(|m| m.tool_errors(namespace, opts.since_ms, opts.live_only))
        .map_err(we)?
    else {
        return Ok(None);
    };
    Ok(Some(
        grains
            .into_iter()
            .filter(|g| visible(f, g, explicit))
            .map(|g| {
                let ns = grain_namespace(&g);
                let created = grain_created_ms(&g);
                into_user_record(g, ns, created)
            })
            .filter(|r| opts.since_ms.is_none_or(|s| r.created_at_ms >= s))
            .collect(),
    ))
}

/// Distinct non-empty Event `session_id`s since `since_ms`, over the
/// namespaces an all-namespace read surfaces — from `thread_idx`, no Event
/// read (#433).
fn event_sessions_since(f: &AreevFacade, since_ms: Option<i64>) -> WResult<Option<u64>> {
    let pairs = f
        .with_store(|m| m.sessions_of_type_since(GrainType::Event, since_ms))
        .map_err(we)?;
    let sessions: BTreeSet<i64> = pairs
        .into_iter()
        .filter(|(ns, _)| ns_visible(f, ns))
        .map(|(_, s)| s)
        .collect();
    Ok(Some(sessions.len() as u64))
}

/// The store grain type a curated user read maps to — `None` for the loop's
/// own ledger reads (`grains_of_type` routes those to `read_loop`).
fn user_grain_type(grain_type: &str, namespace: Option<&str>) -> Option<GrainType> {
    match grain_type {
        "fact" => Some(GrainType::Fact),
        "event" => Some(GrainType::Event),
        "tool" => Some(GrainType::Tool),
        "observation" if namespace == Some(LOOP_NS) => None,
        "observation" => Some(GrainType::Observation),
        "skill" => Some(GrainType::Skill),
        "goal" => Some(GrainType::Goal),
        _ => None,
    }
}

/// Read loop-internal grains (recommendations / audit) stored as Facts.
fn read_loop(
    f: &AreevFacade,
    relation: &str,
    out_type: &str,
    opts: ReadOpts,
) -> WResult<Vec<GrainRecord>> {
    let facts = recent_ns(f, LOOP_NS, GrainType::Fact)?;
    let mut out = Vec::new();
    for fact in facts {
        if fact.get_str("relation") != Some(relation) {
            continue;
        }
        let Some(payload) = fact.get_str("object") else {
            continue;
        };
        let fields: Map<String, Value> = match serde_json::from_str(payload) {
            Ok(Value::Object(m)) => m,
            _ => continue,
        };
        let created = grain_created_ms(&fact);
        if opts.since_ms.is_some_and(|s| created < s) {
            continue;
        }
        out.push(loop_record(
            &fact.hash.to_hex(),
            out_type,
            created,
            fields,
        ));
    }
    Ok(out)
}

fn caps(f: &AreevFacade) -> Capabilities {
    Capabilities {
        forks: true,
        // On iff the host attached a telemetry sidecar (`aggregate`/`full`).
        telemetry: f.with_store(|m| m.telemetry_mode()) != TelemetryMode::Off,
        embeddings: f.with_store(|m| m.declared_embedding().is_some()),
        // Areev models both: workflow plans are grains this adapter can hand
        // to the runtime's own validator, and tool code rides the CAS blob
        // seam implemented above.
        plans: true,
        code: true,
    }
}

/// Structurally validate a proposed Workflow body with the RUNTIME's own
/// validator (`PlanGraph::build`: V1–V6 — unique and reachable nodes,
/// conditions parse, Tarjan SCC with every cycle bounded). The loop therefore
/// never carries a second, drifting opinion about what a runnable plan is.
fn validate_plan(workflow: &Value) -> WResult<()> {
    let fields = workflow
        .as_object()
        .ok_or_else(|| werr("a workflow body must be a JSON object"))?;
    let wf = strict_workflow(fields)?;
    areev_run_core::plan::PlanGraph::build(&wf)
        .map(|_| ())
        .map_err(|e| werr(&format!("proposed plan is not runnable: {e}")))
}

/// Read a `Workflow` out of a grain field map, STRICTLY.
///
/// Deliberately not [`areev_core::format::DeserializedGrain::to_workflow`],
/// which is forward-compatible by contract — it skips a malformed edge rather
/// than failing, which is right when reading a grain someone already wrote and
/// wrong when checking one nobody has written yet. Here a malformed edge is
/// the whole point of asking: silently dropping it would validate a plan that
/// is not the plan under review.
fn strict_workflow(fields: &Map<String, Value>) -> WResult<Workflow> {
    let nodes: Vec<String> = match fields.get("nodes") {
        Some(Value::Array(a)) => a
            .iter()
            .map(|v| {
                v.as_str()
                    .map(str::to_string)
                    .ok_or_else(|| werr("every workflow node must be a string"))
            })
            .collect::<WResult<Vec<String>>>()?,
        _ => return Err(werr("a workflow needs a 'nodes' array")),
    };
    let mut wf = Workflow::new(nodes);
    if let Some(v) = fields.get("edges") {
        let arr = v
            .as_array()
            .ok_or_else(|| werr("workflow 'edges' must be an array"))?;
        for e in arr {
            let (Some(src), Some(dst)) = (
                e.get("src").and_then(Value::as_str),
                e.get("dst").and_then(Value::as_str),
            ) else {
                return Err(werr("every workflow edge needs a 'src' and a 'dst'"));
            };
            let cond = match e.get("cond") {
                None | Some(Value::Null) => None,
                Some(Value::String(c)) => Some(c.clone()),
                Some(_) => return Err(werr("an edge 'cond' must be a string")),
            };
            let max_cycles = match e.get("max_cycles") {
                None | Some(Value::Null) => None,
                Some(v) => Some(
                    v.as_u64()
                        .and_then(|n| u32::try_from(n).ok())
                        .ok_or_else(|| werr("an edge 'max_cycles' must be a small integer"))?,
                ),
            };
            wf.edges.push(WorkflowEdge {
                src: src.to_string(),
                dst: dst.to_string(),
                cond,
                max_cycles,
            });
        }
    }
    if let Some(v) = fields.get("bindings") {
        let obj = v
            .as_object()
            .ok_or_else(|| werr("workflow 'bindings' must be an object"))?;
        for (node, hash) in obj {
            let h = hash
                .as_str()
                .ok_or_else(|| werr("a workflow binding must be a tool hash string"))?;
            wf.bindings.insert(node.clone(), h.to_string());
        }
    }
    if let Some(v) = fields.get("retries") {
        let obj = v
            .as_object()
            .ok_or_else(|| werr("workflow 'retries' must be an object"))?;
        for (node, max) in obj {
            let m = max
                .as_u64()
                .and_then(|n| u32::try_from(n).ok())
                .ok_or_else(|| werr("a workflow retry count must be a small integer"))?;
            wf.retries.insert(node.clone(), m);
        }
    }
    Ok(wf)
}

/// Rule E1's pin for a tool, read from the tool's own live DEFINITION grain
/// (`evalset_hash`) — the same shape `areev tune` embeds in an adapter grain,
/// so code and adapters resolve their gate the same way.
///
/// The tool DECLARES which evalset grades its revisions, ahead of any
/// particular revision existing. That ordering is what the pin is for: a
/// proposer that could choose its own grader is not gated.
fn tool_evalset(f: &AreevFacade, tool: &str) -> WResult<Option<String>> {
    // The Definitions alone, through the store's index (#432) — never the
    // call grains, which are nearly every Tool grain in a busy memory.
    Ok(tool_definitions(f, None)?
        .into_iter()
        .filter(|g| g.tool_name() == Some(tool))
        .filter_map(|g| {
            g.str_field("evalset_hash")
                .filter(|h| !h.trim().is_empty())
                .map(str::to_string)
        })
        .next())
}

/// Read the recall-telemetry sidecar rollups into the loop's substrate-agnostic
/// [`TelemetryView`]. `None` when no sidecar is attached, so telemetry-fed
/// analyzers degrade cleanly.
fn telemetry(f: &AreevFacade, namespace: Option<&str>) -> WResult<Option<TelemetryView>> {
    if f.with_store(|m| m.telemetry_mode()) == TelemetryMode::Off {
        return Ok(None);
    }
    if let Some(ns) = namespace {
        require_read(f, ns)?;
    }
    let access = f.with_store(|m| m.telemetry_access_stats(namespace)).map_err(we)?;
    let queries = f.with_store(|m| m.telemetry_query_stats(namespace)).map_err(we)?;
    // Budget telemetry has no namespace column. It is only safe to expose to
    // a session that may read the whole memory; namespace-scoped sessions
    // still receive their filtered access/query rollups below.
    let budget = if may_read(f, "*") {
        f.with_store(|m| m.telemetry_budget_stats()).map_err(we)?
    } else {
        Default::default()
    };
    Ok(Some(TelemetryView {
        access: access
            .into_iter()
            .filter(|a| may_read(f, &a.ns))
            .map(|a| GrainAccess { hash: a.hash, recall_count: a.recall_count, last_ms: a.last_ms })
            .collect(),
        queries: queries
            .into_iter()
            .filter(|q| may_read(f, &q.ns))
            .map(|q| QueryUsage {
                sample: q.sample,
                run_count: q.run_count,
                empty_count: q.empty_count,
                sum_results: q.sum_results,
                last_ms: q.last_ms,
            })
            .collect(),
        budget: BudgetUsage {
            sample_count: budget.sample_count,
            overflow_count: budget.overflow_count,
        },
    }))
}

fn grains_of_type(
    f: &AreevFacade,
    grain_type: &str,
    namespace: Option<&str>,
    opts: ReadOpts,
) -> WResult<Vec<GrainRecord>> {
    match grain_type {
        "recommendation" => read_loop(f, "loop_recommendation", "recommendation", opts),
        "fact" => read_user_type(f, GrainType::Fact, namespace, opts),
        "event" => read_user_type(f, GrainType::Event, namespace, opts),
        "tool" => read_user_type(f, GrainType::Tool, namespace, opts),
        // Loop audit records ride as Facts (mapping decision 1); asking for
        // observations IN the loop namespace means those — the §7.4
        // forensics chain (approver, BECAUSE, gating fields) reads here.
        "observation" if namespace == Some(LOOP_NS) => {
            read_loop(f, "loop_audit", "observation", opts)
        }
        "observation" => read_user_type(f, GrainType::Observation, namespace, opts),
        "skill" => read_user_type(f, GrainType::Skill, namespace, opts),
        "goal" => read_user_type(f, GrainType::Goal, namespace, opts),
        other => Err(WErr::Substrate(format!("unsupported grain type {other:?}"))),
    }
}

fn grain(f: &AreevFacade, hash: &str) -> WResult<Option<GrainRecord>> {
    let h = Hash::from_hex(hash).map_err(we)?;
    let g = match f.with_store(|m| m.get(&h)) {
        Ok(g) => g,
        Err(AreevError::NotFound(_)) => return Ok(None),
        Err(e) => return Err(we(e)),
    };
    let created = grain_created_ms(&g);
    // Supersession from the STORE'S index — `is_live()` decisions (the
    // apply-gate's evalset-liveness check among them) must see the truth,
    // not a hard-coded None. The review that found this: a superseded
    // evalset read back as live would re-admit code its weakened
    // replacement was supposed to force through re-gating.
    let superseded_by = f
        .with_store(|m| m.supersession_map(&[h]))
        .map_err(we)?
        .get(&h)
        .map(|s| s.to_hex());
    let ns = grain_namespace(&g);
    // Reconstruct loop-internal grains from their JSON payload — but ONLY
    // grains actually living in the loop namespace, and only for readers
    // the loop namespace admits (a user Fact elsewhere that happens to use
    // these relation strings is user data behind its own namespace's gate).
    if ns == LOOP_NS {
        if let Some(rel) = g.get_str("relation") {
            if let Some(out_type) = loop_relation_type(rel) {
                require_read(f, LOOP_NS)?;
                if let Some(Value::Object(fields)) = g
                    .get_str("object")
                    .and_then(|p| serde_json::from_str(p).ok())
                {
                    let mut rec = loop_record(hash, out_type, created, fields);
                    rec.superseded_by = superseded_by;
                    return Ok(Some(rec));
                }
            }
        }
    }
    require_read(f, &ns)?;
    let mut rec = into_user_record(g, ns, created);
    rec.superseded_by = superseded_by;
    Ok(Some(rec))
}

fn heads(f: &AreevFacade, namespace: Option<&str>) -> WResult<Vec<HeadGroup>> {
    let forks = f.with_store(|m| m.open_forks()).map_err(we)?;
    Ok(forks
        .into_iter()
        .filter(|fg| fg.namespace != LOOP_NS)
        .filter(|fg| may_read(f, &fg.namespace))
        .filter(|fg| namespace.is_none_or(|ns| fg.namespace == ns))
        .map(|fg| HeadGroup {
            entity: format!("{}/{}", fg.subject, fg.relation),
            heads: fg.heads.iter().map(|h| h.to_hex()).collect(),
        })
        .collect())
}

/// §7.4 blob seam: candidate code / evalset payloads ride the memory's own
/// CAS (encrypted under the sidecar key when the memory is; addresses are
/// plaintext digests — the documented oracle). Write-gated on the loop
/// namespace: a session that cannot write loop grains cannot carry code.
/// The cap on carried code, not on stored bytes: candidates above it belong
/// in authored adds, not loop payloads.
const MAX_LOOP_BLOB: usize = 4 * 1024 * 1024;

fn put_blob_op(f: &AreevFacade, bytes: &[u8]) -> WResult<String> {
    f.authz().check(Verb::Write, LOOP_NS).map_err(we)?;
    if bytes.len() > MAX_LOOP_BLOB {
        return Err(WErr::Substrate(format!(
            "blob is {} bytes (loop cap {MAX_LOOP_BLOB})",
            bytes.len()
        )));
    }
    f.with_store(|m| m.put_blob(bytes)).map_err(we)
}

fn get_blob_op(f: &AreevFacade, address: &str) -> WResult<Vec<u8>> {
    require_read(f, LOOP_NS)?;
    f.with_store(|m| m.get_blob(address)).map_err(we)
}

fn put_grain(f: &AreevFacade, spec: &GrainSpec) -> WResult<String> {
    // A fact spec that carries its OWN (subject, relation, object) — the
    // engine's §7.4 code-promotion record — is stored as that triple, so
    // hosts can query `(tool:X, mg:code_promotion)` to re-resolve. Wrapping
    // it under a synthetic subject (the generic path below) would disguise
    // the promotion as an unqueryable pseudo-audit row.
    if spec.grain_type == "fact" {
        if let (Some(subject), Some(relation), Some(object)) = (
            spec.fields.get("subject").and_then(Value::as_str),
            spec.fields.get("relation").and_then(Value::as_str),
            spec.fields.get("object").and_then(Value::as_str),
        ) {
            let mut fact = Fact::new(subject, relation, object)
                .namespace(LOOP_NS)
                .confidence(1.0);
            fact.common.created_at = spec_created_ms(spec);
            for (k, v) in &spec.fields {
                if !matches!(k.as_str(), "subject" | "relation" | "object") {
                    fact.common.extra_fields.insert(k.clone(), v.clone());
                }
            }
            return f.with_store(|m| m.add(&fact)).map(|h| h.to_hex()).map_err(we);
        }
    }
    let payload = serde_json::to_string(&spec.fields)
        .map_err(|e| WErr::Substrate(format!("encode grain: {e}")))?;
    let mut fact = Fact::new(
        &unique_subject(&payload),
        loop_relation(&spec.grain_type),
        &payload,
    )
    .namespace(LOOP_NS)
    .confidence(1.0);
    fact.common.created_at = spec_created_ms(spec);
    f.with_store(|m| m.add(&fact))
        .map(|h| h.to_hex())
        .map_err(we)
}

/// The content address `put_grain` would assign — the same Fact it would
/// build, serialized without being stored. What lets `areev loop replay`
/// name the exact grain a live pass would have written, byte for byte.
/// `None` when the spec carries no engine-logical time: the store would
/// stamp the wall clock, and a guessed address would be a wrong one.
fn address_of(spec: &GrainSpec) -> WResult<Option<String>> {
    let Some(created) = spec_created_ms(spec) else {
        return Ok(None);
    };
    let payload = serde_json::to_string(&spec.fields)
        .map_err(|e| WErr::Substrate(format!("encode grain: {e}")))?;
    let mut fact = if spec.grain_type == "fact" {
        match (
            spec.fields.get("subject").and_then(Value::as_str),
            spec.fields.get("relation").and_then(Value::as_str),
            spec.fields.get("object").and_then(Value::as_str),
        ) {
            (Some(subject), Some(relation), Some(object)) => {
                let mut fact = Fact::new(subject, relation, object).namespace(LOOP_NS).confidence(1.0);
                for (k, v) in &spec.fields {
                    if !matches!(k.as_str(), "subject" | "relation" | "object") {
                        fact.common.extra_fields.insert(k.clone(), v.clone());
                    }
                }
                fact
            }
            _ => Fact::new(&unique_subject(&payload), loop_relation(&spec.grain_type), &payload)
                .namespace(LOOP_NS)
                .confidence(1.0),
        }
    } else {
        Fact::new(&unique_subject(&payload), loop_relation(&spec.grain_type), &payload)
            .namespace(LOOP_NS)
            .confidence(1.0)
    };
    fact.common.created_at = Some(created);
    let (_, hash) = areev_core::format::serialize::serialize_grain(&fact).map_err(we)?;
    Ok(Some(hash.to_hex()))
}

/// How many journaled runs of the live plan a revision is rehearsed against.
const PLAN_REPLAY_RUNS: usize = 10;

/// The rehearsal behind `SubstrateRead::plan_replay`: the newest journaled
/// runs of the incumbent plan (any namespace — each rehearsed in its own),
/// re-driven under the candidate body through the runtime's shadow. `None`
/// when the plan has no journaled runs: an unrehearsed revision is not a
/// refused one. A draft the runtime will not build is reported as the
/// substrate error it is — the engine had already validated the graph.
fn plan_replay(f: &AreevFacade, incumbent: &str, candidate: &Value) -> WResult<Option<Value>> {
    let Some(fields) = candidate.as_object() else {
        return Err(werr("a candidate plan must be a JSON object"));
    };
    let runs = areev_run::shadow::runs_of_plan(f, None, incumbent, PLAN_REPLAY_RUNS)
        .map_err(|e| WErr::Substrate(e.to_string()))?;
    if runs.is_empty() {
        return Ok(None);
    }
    let report = areev_run::shadow::shadow_plan_scoped(
        f,
        "loop:replay",
        &runs,
        &areev_run::PlanCandidate::Body(fields.clone()),
    )
    .map_err(|e| WErr::Substrate(e.to_string()))?;
    serde_json::to_value(report)
        .map(Some)
        .map_err(|e| WErr::Substrate(format!("encode replay: {e}")))
}

fn supersede_op(f: &AreevFacade, target_hash: &str, spec: &GrainSpec) -> WResult<String> {
    let payload = serde_json::to_string(&spec.fields)
        .map_err(|e| WErr::Substrate(format!("encode grain: {e}")))?;
    let old = Hash::from_hex(target_hash).map_err(we)?;
    let mut fact = Fact::new(
        &unique_subject(&payload),
        loop_relation(&spec.grain_type),
        &payload,
    )
    .namespace(LOOP_NS);
    fact.common.created_at = spec_created_ms(spec);
    f.with_store(|m| m.supersede(&old, &mut fact))
        .map(|h| h.to_hex())
        .map_err(we)
}

/// The engine-logical creation time already carried in the spec's fields —
/// `created_at_ms` on recommendations, `at_ms` on audit observations. Stamping
/// it onto the stored Fact keeps the grain's own `created_at` aligned with the
/// engine's injected `now` instead of leaking the wall clock, so an areev-loop
/// write is content-addressed purely from (spec, now). `None` (a spec without
/// a time field) falls through to the store's own stamp.
fn spec_created_ms(spec: &GrainSpec) -> Option<i64> {
    spec.fields
        .get("created_at_ms")
        .or_else(|| spec.fields.get("at_ms"))
        .and_then(Value::as_i64)
}

fn retract_op(f: &AreevFacade, hash: &str) -> WResult<()> {
    // No index-only retraction primitive exists; the honest mapping for undoing
    // an engine-created ADD is a tombstone of that grain.
    let h = Hash::from_hex(hash).map_err(we)?;
    f.with_store(|m| m.forget(&h)).map_err(we)
}

fn execute_cal(f: &AreevFacade, cal: &str) -> WResult<Vec<Value>> {
    // Areev Loop proposals are a compact store-op form (ADD/SUPERSEDE/FORGET
    // `<type> {json}`), applied via the facade's typed JSON methods — not the
    // CAL text grammar (which uses SET/BECAUSE). Genuine CAL reads
    // (metric/evidence queries) fall through to the real executor.
    let mut rows = Vec::new();
    for line in cal.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let (keyword, rest) = split_keyword(line);
        match keyword.to_ascii_uppercase().as_str() {
            "FORGET" => {
                let h = Hash::from_hex(rest.trim()).map_err(we)?;
                // A loop-applied FORGET carries its reason on the audit
                // chain already; stamp the Tier-2 audit with the source.
                f.cal_delete(&h, Some("areev-loop apply")).map_err(we)?;
            }
            "ADD" => {
                let (gtype, fields) = parse_type_and_json(rest)?;
                let h = f.cal_add(&gtype, &fields).map_err(we)?;
                rows.push(serde_json::json!({ "hash": h.to_hex() }));
            }
            "SUPERSEDE" => {
                let (target, after) = rest
                    .split_once(" WITH ")
                    .ok_or_else(|| WErr::CalUnsupported(format!("malformed SUPERSEDE: {line}")))?;
                let (gtype, fields) = parse_type_and_json(after)?;
                let old = Hash::from_hex(target.trim()).map_err(we)?;
                let h = f.cal_supersede(&old, &gtype, &fields).map_err(we)?;
                rows.push(serde_json::json!({ "hash": h.to_hex() }));
            }
            _ => {
                // Defense in depth: the engine's destructive gate (admin +
                // allow_destructive) runs before apply, but this executor is
                // process-permissive — never let it run destructive text.
                // Loop-applied destruction goes through the special-cased,
                // Tier-2-audited arms above (FORGET today), one statement
                // shape at a time.
                let parsed = parse(line).map_err(|e| WErr::CalUnsupported(e.to_string()))?;
                if classify(&parsed.statement) == StatementClass::Destructive {
                    return Err(WErr::CalUnsupported(format!(
                        "destructive CAL beyond FORGET <hash> is not executable through the loop substrate: {line}"
                    )));
                }
                let ex = CalExecutor::new(CalExecutorConfig::default());
                let res = ex
                    .execute(line, f)
                    .map_err(|e| WErr::CalUnsupported(e.to_string()))?;
                let payload = serde_json::to_value(&res.result)
                    .map_err(|e| WErr::Substrate(format!("encode CAL result: {e}")))?;
                let mut hs = Vec::new();
                collect_hashes(&payload, &mut hs);
                rows.extend(hs.into_iter().map(|h| serde_json::json!({ "hash": h })));
            }
        }
    }
    Ok(rows)
}

/// The CAL that restores the definition a `DEFINE QUERY` / `DEFINE TEMPLATE`
/// is about to replace — the rollback inverse for a change that writes no
/// grain (issue #28).
///
/// Reconstructed from the registry rather than remembered anywhere: the
/// `qry:`/`tpl:` rows travel with the file, so the inverse is derived from the
/// same state the apply is about to overwrite. When nothing is defined under
/// that name the inverse is a `DROP`, which restores "no such definition" —
/// the correct prior state, not a no-op.
///
/// A BUILT-IN definition has no inverse and returns `None`, which the engine
/// turns into a refusal: built-ins are immutable, so a proposal to redefine
/// one could not be rolled back and must not be applied.
fn definition_inverse(f: &AreevFacade, statement: &str) -> WResult<Option<String>> {
    let up = statement.trim_start().to_ascii_uppercase();
    let (kind, after) = if let Some(rest) = up.strip_prefix("DEFINE QUERY") {
        ("QUERY", &statement.trim_start()[statement.trim_start().len() - rest.len()..])
    } else if let Some(rest) = up.strip_prefix("DEFINE TEMPLATE") {
        ("TEMPLATE", &statement.trim_start()[statement.trim_start().len() - rest.len()..])
    } else {
        return Ok(None);
    };
    // The name is the first token, bare or quoted — both spellings parse.
    let name = after
        .trim_start()
        .trim_start_matches('"')
        .split(['"', '(', ' ', '\t'])
        .next()
        .unwrap_or("")
        .trim()
        .to_string();
    if name.is_empty() {
        return Ok(None);
    }
    if kind == "QUERY" {
        match f.get_query(&name) {
            // Built-ins are immutable; a rewrite of one is unapplicable.
            Some(e) if e.builtin => Ok(None),
            Some(e) => {
                let params = if e.params.is_empty() {
                    String::new()
                } else {
                    let list: Vec<String> =
                        e.params.iter().map(|p| format!("${}", p.name)).collect();
                    format!("({})", list.join(", "))
                };
                Ok(Some(format!(
                    "DEFINE QUERY \"{name}\"{params} DESCRIPTION \"{}\" AS {{ {} }}",
                    e.description.replace('"', "'"),
                    e.body
                )))
            }
            None => Ok(Some(format!("DROP QUERY \"{name}\""))),
        }
    } else {
        match f.get_template(&name) {
            Some(t) if t.builtin => Ok(None),
            Some(t) => Ok(Some(format!(
                "DEFINE TEMPLATE \"{name}\" DESCRIPTION \"{}\" AS {{ {} }}",
                t.description.replace('"', "'"),
                t.source
            ))),
            None => Ok(Some(format!("DROP TEMPLATE \"{name}\""))),
        }
    }
}

fn validate_cal(cal: &str) -> WResult<()> {
    for line in cal.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let (keyword, rest) = split_keyword(line);
        match keyword.to_ascii_uppercase().as_str() {
            "ADD" | "SUPERSEDE" => {}
            // The loop's FORGET arm executes single-grain tombstones only —
            // `FORGET SUBJECT "<id>"` must fail here, not at apply time.
            "FORGET" => {
                Hash::from_hex(rest.trim()).map_err(|_| {
                    WErr::CalUnsupported(format!(
                        "loop FORGET takes a single grain hash: {line}"
                    ))
                })?;
            }
            _ => {
                let parsed = parse(line).map_err(|e| WErr::CalUnsupported(e.to_string()))?;
                // Mirror execute_cal: destructive statements (PURGE, …) have
                // no audited loop execution path, so they are invalid in a
                // proposal — reject at validation, before review effort.
                if classify(&parsed.statement) == StatementClass::Destructive {
                    return Err(WErr::CalUnsupported(format!(
                        "destructive CAL beyond FORGET <hash> is not executable through the loop substrate: {line}"
                    )));
                }
            }
        }
    }
    Ok(())
}

/// The loop's persisted state, read straight off an open store.
///
/// The read-only half of `load_state`, for surfaces that hold a `&mut
/// Areev` rather than a facade — `areev audit export --with-outcomes`
/// (#319), above all. `Value::Null` when the loop has never run.
pub fn loop_state_of(m: &mut areev_store::Areev) -> WResult<Value> {
    let head = m.latest(LOOP_NS, STATE_SUBJECT, STATE_RELATION).map_err(we)?;
    match head.as_ref().and_then(|g| g.get_str("object")) {
        Some(json) => serde_json::from_str(json)
            .map_err(|e| WErr::Substrate(format!("decode loop state: {e}"))),
        None => Ok(Value::Null),
    }
}

fn load_state(f: &AreevFacade) -> WResult<Value> {
    let head = f
        .with_store(|m| m.latest(LOOP_NS, STATE_SUBJECT, STATE_RELATION))
        .map_err(we)?;
    match head.as_ref().and_then(|g| g.get_str("object")) {
        Some(json) => serde_json::from_str(json)
            .map_err(|e| WErr::Substrate(format!("decode loop state: {e}"))),
        None => Ok(Value::Null),
    }
}

/// Persist the loop's state as a superseded Fact grain.
///
/// A grain, not a `meta` row, and the opposite of what `areev trigger` does with
/// its `trg:` state — deliberately. Most of this blob is **governance**:
/// `creators` and `co_creators` are what the self-approval block reads to refuse
/// an approval by the principal who authored the recommendation, `audit_heads`
/// chains the audit records, `status_index` carries the lifecycle. All of it has
/// to travel with the file, or a replica finds no creator recorded and lets
/// someone approve their own recommendation — a silent separation-of-duties
/// bypass.
///
/// A trigger's state is a cursor and must NOT replicate, for the mirror-image
/// reason. Do not harmonise the two; see `docs/loop.md`.
fn store_state(f: &AreevFacade, state: &Value) -> WResult<()> {
    let json = serde_json::to_string(state)
        .map_err(|e| WErr::Substrate(format!("encode loop state: {e}")))?;
    let existing = f
        .with_store(|m| m.latest(LOOP_NS, STATE_SUBJECT, STATE_RELATION))
        .map_err(we)?;
    f.with_store(|m| {
        let mut fact = Fact::new(STATE_SUBJECT, STATE_RELATION, &json).namespace(LOOP_NS);
        match &existing {
            Some(g) => m.supersede(&g.hash, &mut fact).map(|_| ()),
            None => m.add(&fact).map(|_| ()),
        }
    })
    .map_err(we)
}

// --- pure helpers ---

fn loop_relation(grain_type: &str) -> &'static str {
    match grain_type {
        "recommendation" => "loop_recommendation",
        _ => "loop_audit", // the engine only puts recommendation + audit(observation) grains
    }
}

fn loop_relation_type(relation: &str) -> Option<&'static str> {
    match relation {
        "loop_recommendation" => Some("recommendation"),
        "loop_audit" => Some("observation"),
        _ => None,
    }
}

/// Deterministic, collision-safe subject so distinct loop grains never share
/// a `(subject, relation)` head (which would look like a fork).
fn unique_subject(payload: &str) -> String {
    use std::hash::{Hash as _, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    payload.hash(&mut h);
    format!("w{:016x}", h.finish())
}

fn grain_namespace(g: &DeserializedGrain) -> String {
    g.get_str("namespace").unwrap_or("").to_string()
}

fn grain_created_ms(g: &DeserializedGrain) -> i64 {
    g.get_i64("created_at")
        .unwrap_or_else(|| g.header.created_at_sec as i64 * 1000)
}

/// A store grain as the loop's record, moving the decoded fields rather
/// than cloning them — a fold's page holds each body once, not twice.
fn into_user_record(g: DeserializedGrain, namespace: String, created_ms: i64) -> GrainRecord {
    GrainRecord {
        hash: g.hash.to_hex(),
        grain_type: g.grain_type.as_str().to_string(),
        namespace,
        created_at_ms: created_ms,
        valid_to_ms: g.get_i64("valid_to"),
        superseded_by: None,
        fields: g.fields.into_iter().collect(),
    }
}

fn loop_record(
    hash: &str,
    grain_type: &str,
    created_ms: i64,
    fields: Map<String, Value>,
) -> GrainRecord {
    let valid_to_ms = fields.get("valid_to_ms").and_then(Value::as_i64);
    GrainRecord {
        hash: hash.to_string(),
        grain_type: grain_type.to_string(),
        namespace: LOOP_NS.to_string(),
        created_at_ms: created_ms,
        valid_to_ms,
        superseded_by: None,
        fields,
    }
}

fn derived_parents(g: &DeserializedGrain) -> Vec<String> {
    match g.fields.get("derived_from") {
        Some(Value::String(s)) => vec![s.clone()],
        Some(Value::Array(a)) => a
            .iter()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect(),
        _ => Vec::new(),
    }
}

fn split_keyword(line: &str) -> (&str, &str) {
    match line.split_once(char::is_whitespace) {
        Some((k, rest)) => (k, rest.trim_start()),
        None => (line, ""),
    }
}

/// Parse `<type> {json}` → (type, fields).
fn parse_type_and_json(s: &str) -> WResult<(String, Map<String, Value>)> {
    let brace = s
        .find('{')
        .ok_or_else(|| WErr::CalUnsupported(format!("missing JSON object in {s:?}")))?;
    let gtype = s[..brace].trim().to_string();
    if gtype.is_empty() {
        return Err(WErr::CalUnsupported(format!("missing grain type in {s:?}")));
    }
    let value: Value = serde_json::from_str(s[brace..].trim())
        .map_err(|e| WErr::CalUnsupported(format!("bad JSON in {s:?}: {e}")))?;
    match value {
        Value::Object(m) => Ok((gtype, m)),
        _ => Err(WErr::CalUnsupported(format!("JSON not an object in {s:?}"))),
    }
}

fn collect_hashes(v: &Value, out: &mut Vec<String>) {
    match v {
        Value::Object(m) => {
            for (k, val) in m {
                if (k == "hash" || k == "new_hash") && val.is_string() {
                    out.push(val.as_str().unwrap().to_string());
                } else {
                    collect_hashes(val, out);
                }
            }
        }
        Value::Array(a) => a.iter().for_each(|x| collect_hashes(x, out)),
        _ => {}
    }
}
