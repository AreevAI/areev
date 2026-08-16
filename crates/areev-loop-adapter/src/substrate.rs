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
//!    its hash appears in some sibling's `derived_from`. All-namespace scans
//!    (no ns filter) enumerate via the op-log (`changes_since`), since `recent`
//!    requires a namespace.

use areev_cal::classify::{classify, StatementClass};
use areev_cal::{parse, CalExecutor, CalExecutorConfig, CalStoreFacade, AreevFacade};
use areev_core::error::{AreevError, Hash};
use areev_core::format::deserialize::DeserializedGrain;
use areev_core::authz::Verb;
use areev_core::types::{Fact, Grain, GrainType};
use areev_store::{Areev, TelemetryMode, OP_FORGET};
use serde_json::{Map, Value};
use std::collections::BTreeSet;
use areev_loop::error::{Error as WErr, Result as WResult};
use areev_loop::{
    BudgetUsage, Capabilities, GrainAccess, GrainRecord, GrainSpec, HeadGroup, OmsSubstrate,
    QueryUsage, ReadOpts, SubstrateRead, TelemetryView,
};

/// The namespace the loop's own grains live in (mirrors `areev_loop::LOOP_NS`).
const LOOP_NS: &str = "areev-loop";
/// Sentinel subject/relation for the persisted loop state blob.
const STATE_SUBJECT: &str = "__loop_state__";
const STATE_RELATION: &str = "state";
/// Upper bound on grains scanned per query (interim; real substrate paginates).
const MAX_SCAN: usize = 1_000_000;

fn we(e: AreevError) -> WErr {
    WErr::Substrate(e.to_string())
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
            fn grain(&self, hash: &str) -> WResult<Option<GrainRecord>> {
                grain(self.facade_ref(), hash)
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
    // An explicitly requested namespace is a capability assertion. Deny it
    // loudly; silently returning an empty set would misreport authorization
    // failure as "the analyzer found no data".
    require_read(f, ns)?;
    f.with_store(|m| m.recent(ns, Some(gt), MAX_SCAN))
        .map_err(we)
}

/// Enumerate every grain of `gt` across all namespaces via the op-log.
fn all_grains(f: &AreevFacade, gt: GrainType) -> WResult<Vec<DeserializedGrain>> {
    let ops = f.with_store(|m| m.changes_since(0, MAX_SCAN)).map_err(we)?;
    let mut seen = BTreeSet::new();
    let mut out = Vec::new();
    for op in ops {
        if op.op == OP_FORGET || !seen.insert(op.hash.to_hex()) {
            continue;
        }
        if let Ok(g) = f.with_store(|m| m.get(&op.hash)) {
            if g.grain_type == gt && may_read(f, &grain_namespace(&g)) {
                out.push(g);
            }
        }
    }
    Ok(out)
}

fn read_user_type(
    f: &AreevFacade,
    gt: GrainType,
    namespace: Option<&str>,
    opts: ReadOpts,
) -> WResult<Vec<GrainRecord>> {
    let explicit = namespace.is_some();
    let raw = match namespace {
        Some(ns) => recent_ns(f, ns, gt)?,
        None => all_grains(f, gt)?,
    };
    let superseded = superseded_set(&raw);
    let mut out = Vec::new();
    for g in raw {
        let ns = grain_namespace(&g);
        // Never surface loop-internal or governance grains in ALL-NAMESPACE
        // scans. `agent:authz` holds the file's own grant Facts and the
        // Tier-2 audit Observations; an analyzer sweeping everything that
        // sees them as ordinary memory can propose tombstoning them
        // (retention_sweep did), which locks every non-owner principal out
        // of the file and destroys the very Art. 30 records the audit
        // export exists to produce. Governance state is metadata about the
        // memory, not memory. An EXPLICITLY requested namespace is
        // different: the caller named it, `recent_ns` authz-checked it, and
        // consumers like the run_outcome analyzer (agent:harness) and the
        // gating-evidence loader depend on reading it.
        if !explicit && (ns == LOOP_NS || ns.starts_with("agent:")) {
            continue;
        }
        if opts.live_only && superseded.contains(&g.hash.to_hex()) {
            continue;
        }
        let created = grain_created_ms(&g);
        if opts.since_ms.is_some_and(|s| created < s) {
            continue;
        }
        out.push(map_user_grain(&g, ns, created));
    }
    Ok(out)
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
    }
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
    let mut rec = map_user_grain(&g, ns, created);
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

fn map_user_grain(g: &DeserializedGrain, namespace: String, created_ms: i64) -> GrainRecord {
    GrainRecord {
        hash: g.hash.to_hex(),
        grain_type: g.grain_type.as_str().to_string(),
        namespace,
        created_at_ms: created_ms,
        valid_to_ms: g.get_i64("valid_to"),
        superseded_by: None,
        fields: g.fields.clone().into_iter().collect(),
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

/// Hashes that some sibling grain supersedes (via `derived_from`).
fn superseded_set(grains: &[DeserializedGrain]) -> BTreeSet<String> {
    let mut set = BTreeSet::new();
    for g in grains {
        for parent in derived_parents(g) {
            set.insert(parent);
        }
    }
    set
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
