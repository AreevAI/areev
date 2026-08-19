//! AreevFacade — CalStoreFacade over the embedded areev-store.
//!
//! Session-scoped: carries the capability defaults (namespace, user) that
//! CAL queries inherit when they don't specify one (§7.11 direction).
//!
//! M2 scope: structural recall only. Semantic (`query`) recall returns a
//! clear error until the FTS/vector legs land (M4).

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;

use areev_core::authz::Verb;
use areev_core::error::{Hash, AreevError, Result};
use areev_core::ns::NsScope;
use areev_core::format::deserialize::DeserializedGrain;
use areev_core::format::serialize::serialize_grain;
use areev_core::types::{Fact, Grain, Observation, RelatedTo, State};
use areev_store::Areev;

use crate::ast::QueryParam;
use crate::errors::CalError;
use crate::facade::{AssemblyManifest, CalStoreFacade, TemplateInfo};
use crate::json_build::{build_grain_from_json, GrainSink};
use crate::queries::{PersistedQuery, QueryEntry, QueryListEntry, QueryRegistry};
use crate::store_types::{
    ConflictStatus, DiversityMethod, ForkGroupInfo, RecallParams, SearchHit, SupersessionStatus,
    VersionEntry,
};
use crate::templates::{PersistedTemplate, TemplateRegistry};

/// `meta` key prefixes for CAL host metadata. One row per entry, so recording
/// a last-run timestamp does not rewrite the whole set.
const QRY_PREFIX: &str = "qry:";
const TPL_PREFIX: &str = "tpl:";

/// Recursive ingress transform over a JSON value's string leaves.
fn ingress_value_tree(
    m: &mut Areev,
    ns: &str,
    v: &mut serde_json::Value,
) -> Result<()> {
    match v {
        serde_json::Value::String(s) => {
            if let Some(t) = m.ingress_transform_text(ns, s)? {
                *s = t;
            }
            Ok(())
        }
        serde_json::Value::Array(a) => {
            for item in a {
                ingress_value_tree(m, ns, item)?;
            }
            Ok(())
        }
        serde_json::Value::Object(o) => {
            for (k, item) in o.iter_mut() {
                if !matches!(k.as_str(), "namespace" | "session_id" | "run_id" | "parent") {
                    ingress_value_tree(m, ns, item)?;
                }
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// A synthetic tool-call id for a host that did not supply one, so that two
/// executions of the same tool stay two records.
///
/// The `auto:` prefix is deliberate: it marks the id as minted here rather than
/// carried from a provider, so nothing downstream mistakes it for a real
/// `tool_call_id` it could correlate against an LLM transcript.
///
/// Uniqueness comes from the counter, which is monotonic within a process and
/// therefore holds even where the clock is coarse or steps backwards. The
/// timestamp and pid only separate *different* processes writing the same
/// memory in sequence — a clock tick alone would not, since a short-lived
/// process restarts the counter at zero.
///
/// This does make the record non-reproducible: recording the same call twice
/// writes two grains. That is the intended reading of an occurrence log — it
/// happened twice — and it is why the id is minted per call rather than derived
/// from the call's content, which would put us straight back into collapsing
/// retries.
fn occurrence_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!(
        "auto:{}-{}-{}",
        std::process::id(),
        chrono::Utc::now()
            .timestamp_nanos_opt()
            .unwrap_or_else(|| now_ms().saturating_mul(1_000_000)),
        n
    )
}

/// Membership test for the `WHERE <field> IN (...)` filters. `None` is "no
/// filter"; an empty set never reaches here (the caller short-circuits, because
/// an empty `IN` selects nothing rather than everything).
fn set_contains(set: &Option<Vec<String>>, value: Option<&str>) -> bool {
    match set {
        None => true,
        Some(values) => value.is_some_and(|v| values.iter().any(|c| c == v)),
    }
}

/// "3 hours ago" for `WITH annotate_relative_time`. Coarse on purpose: the
/// point is freshness at a glance, and a model reading "2 weeks ago" needs no
/// more precision than that. Negative ages (a grain stamped in the future,
/// which import and clock skew both produce) read as "just now" rather than
/// wrapping into a nonsense past.
fn relative_time_label(age_ms: i64) -> String {
    const MIN: i64 = 60_000;
    const HOUR: i64 = 60 * MIN;
    const DAY: i64 = 24 * HOUR;
    let plural = |n: i64, unit: &str| {
        if n == 1 {
            format!("1 {unit} ago")
        } else {
            format!("{n} {unit}s ago")
        }
    };
    match age_ms {
        a if a < MIN => "just now".into(),
        a if a < HOUR => plural(a / MIN, "minute"),
        a if a < DAY => plural(a / HOUR, "hour"),
        a if a < 7 * DAY => plural(a / DAY, "day"),
        a if a < 30 * DAY => plural(a / (7 * DAY), "week"),
        a if a < 365 * DAY => plural(a / (30 * DAY), "month"),
        a => plural(a / (365 * DAY), "year"),
    }
}

/// CalStoreFacade implementation over an embedded `Areev` store.
pub struct AreevFacade {
    store: Mutex<Areev>,
    namespace: Option<String>,
    user: Option<String>,
    /// Read-only mounted memories (org/category replicas): alias → store.
    /// A recall with namespace "alias.inner" routes to the mount (§8).
    mounts: std::collections::HashMap<String, Mutex<Areev>>,
    /// Saved queries and custom templates are *host metadata* carried by the
    /// file (`meta` rows), not memories — they travel with the .db so the
    /// CLI, MCP and console all see the same set. Rehydrated on first use.
    queries: Mutex<Option<QueryRegistry>>,
    templates: Mutex<Option<TemplateRegistry>>,
    /// Entries the file carries that this process could not load — a template
    /// that outgrew the §10.8 body limit, a set past the per-file cap, a row
    /// written by a newer version. Reported alongside `Areev::open_warnings`
    /// so a silently smaller set of saved queries is something the operator
    /// sees rather than discovers.
    meta_warnings: Mutex<Vec<String>>,
    /// The session's resolved rights. Defaults to the owner session — a
    /// local open with no principal asserted is the implicit superuser
    /// (`root@localhost`), which is why the single-user path never meets
    /// authorization. `with_principal` replaces it with a fail-closed
    /// restricted set built from the file's own grant grains.
    ///
    /// Behind a lock so a host that serializes requests (the std server
    /// handles one connection at a time) can rebind per request
    /// ([`Self::bind_principal`]). Concurrent hosts bind once at open —
    /// rebinding under concurrent CAL execution would race sessions.
    authz: std::sync::RwLock<areev_core::authz::AuthzSet>,
}

impl AreevFacade {
    pub fn new(store: Areev) -> Self {
        Self::with_session(store, None, None)
    }

    /// Session-scoped facade: `namespace`/`user` become the capability
    /// defaults consulted by the executor.
    pub fn with_session(store: Areev, namespace: Option<String>, user: Option<String>) -> Self {
        AreevFacade {
            store: Mutex::new(store),
            namespace,
            user,
            mounts: std::collections::HashMap::new(),
            queries: Mutex::new(None),
            templates: Mutex::new(None),
            meta_warnings: Mutex::new(Vec::new()),
            authz: std::sync::RwLock::new(areev_core::authz::AuthzSet::owner("user:local")),
        }
    }

    /// Bind this facade to a principal: rights become exactly what the
    /// file's live grant grains cover (fail closed — a file with no grants
    /// for this principal allows nothing). Grants are read once here; a
    /// grant written later needs a rebind (the CAL `GRANT` path will
    /// refresh this itself when it lands).
    pub fn with_principal(self, principal: &str) -> areev_core::error::Result<Self> {
        self.bind_principal(principal)?;
        Ok(self)
    }

    /// Rebind this facade to a principal: rights become exactly what the
    /// file's live grant grains cover right now (fail closed). For hosts
    /// that serialize requests — the std server — this is the per-request
    /// seam: resolve the credential, bind, handle, then [`Self::bind`]
    /// the surface's default back.
    pub fn bind_principal(&self, principal: &str) -> areev_core::error::Result<()> {
        let grants = {
            let mut guard = self.store.lock().unwrap();
            guard.authz_grants(principal)?
        };
        self.bind(areev_core::authz::AuthzSet::restricted(principal, grants));
        Ok(())
    }

    /// Install a pre-built rights set (e.g. the anonymous baseline, or the
    /// owner default when restoring after a request).
    pub fn bind(&self, authz: areev_core::authz::AuthzSet) {
        *self.authz.write().expect("authz lock poisoned") = authz;
    }

    /// The session's resolved rights (owner unless a principal was bound).
    pub fn authz(&self) -> areev_core::authz::AuthzSet {
        self.authz.read().expect("authz lock poisoned").clone()
    }

    /// A per-call, principal-scoped session over this facade — the
    /// rebind-race-free write path (governed-agents §6.8).
    ///
    /// [`Self::bind_principal`] swaps ONE process-wide slot and is documented
    /// as safe only for hosts that serialize requests. A runtime attributing
    /// journal writes to a run's triggering principal — or a second approver
    /// answering a HITL ask while the run's own writes continue — needs
    /// concurrent per-principal rights, which a shared slot cannot express
    /// without racing. A session resolves its fail-closed [`AuthzSet`] ONCE
    /// here (grants written later need a new session, same rule as rebind)
    /// and never touches the shared slot; any number of sessions run
    /// concurrently over the same store handle.
    ///
    /// Writes through a session are **attributed**: grains that do not set
    /// `author_did` get the session principal stamped in, so "who wrote this"
    /// is answerable from the grain itself, not from ambient state.
    pub fn principal_session(
        &self,
        principal: &str,
    ) -> Result<PrincipalSession<'_>> {
        let grants = {
            let mut guard = self.store.lock().unwrap();
            guard.authz_grants(principal)?
        };
        Ok(PrincipalSession {
            facade: self,
            authz: areev_core::authz::AuthzSet::restricted(principal, grants),
        })
    }

    /// The enforcement read used by every gated method.
    fn check_verb(&self, verb: Verb, ns: &str) -> Result<()> {
        self.authz.read().expect("authz lock poisoned").check(verb, ns)
    }

    fn session_is_owner(&self) -> bool {
        self.authz.read().expect("authz lock poisoned").is_owner()
    }

    fn session_principal(&self) -> String {
        self.authz
            .read()
            .expect("authz lock poisoned")
            .principal()
            .to_string()
    }

    /// Write the Tier-2 audit Observation (CAL 1.3 §8.14): every destructive
    /// execution records the session principal, the verb, the target, the
    /// reason, and how many grains it removed. It is a grain — it syncs with
    /// the file and is RECALLable — and it rides the reserved authz
    /// namespace next to the grants. Audit records are *occurrences*, so
    /// each carries a unique frame id: two identical erasures must stay two
    /// records (the #66 lesson).
    fn audit_tier2(
        &self,
        verb: &str,
        target: &str,
        because: Option<&str>,
        count: usize,
        stale_exports: &[areev_store::CorpusExportRegistry],
    ) -> Result<()> {
        // One builder for every surface (areev_core::authz) so the CLI's
        // host-level erasures and CAL's produce identical audit shapes.
        let mut obs = areev_core::authz::audit_observation(
            &self.session_principal(),
            verb,
            target,
            because,
            count,
            now_epoch_ms(),
        );
        if !stale_exports.is_empty() {
            let mut context = obs
                .common
                .context
                .take()
                .and_then(|value| value.as_object().cloned())
                .unwrap_or_default();
            context.insert(
                "stale_corpora".into(),
                serde_json::to_value(stale_exports).unwrap_or_else(|_| serde_json::json!([])),
            );
            obs.common.context = Some(serde_json::Value::Object(context));
        }
        self.store.lock().unwrap().add(&obs).map(|_| ())
    }

    /// The namespace a JSON-built write lands in: the explicit `namespace`
    /// field, else the session default, else `"shared"` (the store default).
    /// This is the resource every write-verb check runs against.
    fn write_ns<'a>(&'a self, fields: &'a serde_json::Map<String, serde_json::Value>) -> &'a str {
        fields
            .get("namespace")
            .and_then(|v| v.as_str())
            .or(self.namespace.as_deref())
            .unwrap_or("shared")
    }

    /// Mount a read-only memory (an org/category replica) under an alias.
    /// CAL reaches it with `WHERE namespace = "<alias>.<inner-ns>"` — which
    /// is what makes single-statement ASSEMBLE span user + org files.
    pub fn mount(&mut self, alias: &str, store: Areev) {
        self.mounts.insert(alias.to_string(), Mutex::new(store));
    }

    pub fn into_inner(self) -> Areev {
        self.store.into_inner().unwrap_or_else(|p| p.into_inner())
    }

    /// Run a closure against the underlying store — the escape hatch for
    /// implementation-level operations CAL structurally excludes (forget,
    /// bundle, stats). Host-surface only; never reachable from CAL text.
    /// The session's capability namespace, if scoped.
    pub fn session_namespace(&self) -> Option<&str> {
        self.namespace.as_deref()
    }

    /// Recall over-fetch multiplier: `recall_hybrid` is asked for
    /// `limit × RECALL_OVERFETCH` candidates before post-filtering.
    pub const RECALL_OVERFETCH: usize = 4;

    /// How many first-pass results seed `WITH multi_hop`. The top few name the
    /// entities worth following; taking every result would fan out over the
    /// long tail of a weak match.
    pub const MULTI_HOP_SEED: usize = 8;

    /// Candidates pulled per entity per hop. Small on purpose — a hop is a
    /// widening heuristic, and the candidate pool is capped anyway.
    pub const MULTI_HOP_FANOUT: usize = 8;

    /// Aliases of read-only mounted stores (ASSEMBLE cross-file sources).
    pub fn mount_aliases(&self) -> Vec<String> {
        let mut a: Vec<String> = self.mounts.keys().cloned().collect();
        a.sort();
        a
    }

    pub fn with_store<R>(&self, f: impl FnOnce(&mut Areev) -> R) -> R {
        let mut guard = self.store.lock().unwrap();
        f(&mut guard)
    }

    /// Ingress boundary for the structured write path (proposal §4.2):
    /// identity fields whole-value as `person`, other string leaves through
    /// detection. `Ok(None)` = no ingress policy covers the write namespace
    /// (the overwhelmingly common case — one store probe). Runtime journal
    /// writes (`record_tool_call`, `record_run_manifest`) are deliberately
    /// exempt, the write-side mirror of the `run_grains` egress exemption.
    fn ingress_fields(
        &self,
        fields: &serde_json::Map<String, serde_json::Value>,
    ) -> Result<Option<serde_json::Map<String, serde_json::Value>>> {
        const IDENTITY: &[&str] = &["subject", "user_id", "observer"];
        const SKIP: &[&str] = &[
            "namespace", "session_id", "run_id", "node_id", "parent", "relation",
            "derived_from", "supersedes", "author_did",
        ];
        let ns = self.write_ns(fields).to_string();
        let mut guard = self.store.lock().unwrap();
        if !guard.ingress_active(&ns)? {
            return Ok(None);
        }
        let mut out = fields.clone();
        for (k, v) in out.iter_mut() {
            if SKIP.contains(&k.as_str()) {
                continue;
            }
            if IDENTITY.contains(&k.as_str()) {
                if let serde_json::Value::String(sv) = v {
                    if let Some(t) = guard.ingress_transform_identity(&ns, sv)? {
                        *sv = t;
                    }
                }
                continue;
            }
            ingress_value_tree(&mut guard, &ns, v)?;
        }
        Ok(Some(out))
    }

    /// Reverse-lookup placeholder tokens (proposal D9, REQ-ANON-3): the
    /// privileged, audited act. Requires the `admin` verb on `ns`; every
    /// execution writes a Tier-2 Observation in `agent:authz` carrying the
    /// revealed values' *fingerprints*, never the identities. Returns JSON
    /// `{"revealed": {token: value | null}}`.
    pub fn reveal_tokens(&self, ns: &str, tokens: &[String]) -> Result<String> {
        self.check_verb(Verb::Admin, ns)?;
        let mut revealed = serde_json::Map::new();
        let mut fingerprints: Vec<String> = Vec::new();
        self.with_store(|m| -> Result<()> {
            for token in tokens {
                match m.anon_reveal(ns, token)? {
                    Some(value) => {
                        fingerprints.push(areev_core::authz::subject_fingerprint(&value));
                        revealed.insert(token.clone(), serde_json::json!(value));
                    }
                    None => {
                        revealed.insert(token.clone(), serde_json::Value::Null);
                    }
                }
            }
            Ok(())
        })?;
        // Tier-2 audit: the reveal IS the record — fingerprints only, so the
        // immutable, replicating audit grain cannot re-identify by itself.
        let mut obs = areev_core::authz::audit_observation(
            &self.session_principal(),
            "reveal",
            ns,
            None,
            fingerprints.len(),
            now_ms(),
        );
        if !fingerprints.is_empty() {
            let ctx = obs.common.context.get_or_insert_with(|| serde_json::json!({}));
            if let Some(o) = ctx.as_object_mut() {
                o.insert("revealed_fingerprints".into(), serde_json::json!(fingerprints));
            }
        }
        self.with_store(|m| m.add(&obs))?;
        serde_json::to_string(&serde_json::json!({"revealed": revealed}))
            .map_err(|e| AreevError::Internal(e.to_string()))
    }

    // ---- text anonymization (docs/anonymization-proposal.md, P0) ----------
    //
    // Text in, JSON out — no store WRITES, but reads the store's
    // known-identity table for the facade's default namespace (issue #32):
    // a subject already interned by an intake step (grain egress) is
    // detectable in free text too, the same propagation table, one
    // matcher. `policy_json` is still supplied explicitly by the caller
    // (P0's contract; the declared `anon:<ns>` policy row is not
    // auto-loaded here) and may carry its own `known` entries for
    // identities the caller holds but never interned as a grain subject.

    /// The facade's default namespace's accumulated known-identity list —
    /// same table `egress_grains` builds for grain reads (issue #32). Empty
    /// when the facade has no default namespace (bare `AreevFacade::new`).
    fn known_identities(&self) -> Vec<String> {
        match &self.namespace {
            Some(ns) => self.with_store(|m| m.anon_known_identities(ns)),
            None => Vec::new(),
        }
    }

    /// Run the Tier-0 detector chain over `text`. Returns JSON
    /// `{"text": <nfc text>, "detections": [{start, end, category,
    /// confidence, detector}]}` — offsets are UTF-8 bytes into the returned
    /// (NFC-normalized) text.
    pub fn scan_text(&self, text: &str, policy_json: Option<&str>) -> Result<String> {
        let policy = match policy_json {
            Some(j) => areev_core::anon::AnonPolicy::from_json(j)?,
            None => areev_core::anon::AnonPolicy::default(),
        };
        let out = areev_core::anon::scan(text, &policy, &self.known_identities())?;
        serde_json::to_string(&out).map_err(|e| AreevError::Internal(e.to_string()))
    }

    /// Pseudonymize `text` under the policy (default policy when `None`).
    /// Returns JSON `{"text", "mapping", "mapping_id", "replaced"}`. Only
    /// `pseudonym` spans enter the mapping — `mask`/`redact` are one-way.
    /// `key_hex` keys the `mapping_id` derivation (D11); without it the id
    /// must not be shipped anywhere the mapping doesn't also travel.
    pub fn anonymize_text(
        &self,
        text: &str,
        policy_json: Option<&str>,
        key_hex: Option<&str>,
    ) -> Result<String> {
        let policy = match policy_json {
            Some(j) => areev_core::anon::AnonPolicy::from_json(j)?,
            None => areev_core::anon::AnonPolicy::default(),
        };
        let key = match key_hex {
            Some(h) => Some(hex::decode(h).map_err(|e| {
                AreevError::Validation(format!("anonymize key must be hex: {e}"))
            })?),
            None => None,
        };
        let out =
            areev_core::anon::anonymize(text, &policy, &self.known_identities(), key.as_deref())?;
        serde_json::to_string(&out).map_err(|e| AreevError::Internal(e.to_string()))
    }

    /// Replace placeholder tokens in `text` with their originals from
    /// `mapping_json` (a JSON object of placeholder → value). Returns JSON
    /// `{"text", "replaced", "unmatched"}`; unmatched tokens are left intact
    /// and reported, never guessed.
    pub fn rehydrate_text(&self, text: &str, mapping_json: &str) -> Result<String> {
        let mapping = areev_core::anon::mapping_from_json(mapping_json)?;
        let out = areev_core::anon::rehydrate(text, &mapping)?;
        serde_json::to_string(&out).map_err(|e| AreevError::Internal(e.to_string()))
    }

    /// Preflight the governed-corpus registry write. Exporting reads the
    /// selected namespaces, but recording its immutable lineage additionally
    /// requires `write ON agent:harness`.
    pub fn authorize_corpus_export(&self) -> Result<()> {
        self.check_verb(Verb::Write, areev_core::authz::HARNESS_NS)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn record_corpus_export(
        &self,
        selector: &str,
        destination: &str,
        recipient: Option<&str>,
        exported_at_ms: i64,
        subject_fingerprints: &[String],
        source_hashes: &[String],
    ) -> Result<Hash> {
        self.authorize_corpus_export()?;
        self.with_store(|store| {
            store.record_corpus_export(
                selector,
                destination,
                recipient,
                exported_at_ms,
                subject_fingerprints,
                source_hashes,
            )
        })
    }

    /// Value-level idempotent add (see [`Areev::add_if_novel`]). Returns the
    /// grain hash and whether a new grain was written (`false` = the value was
    /// already the current head). Bindings expose this as an `idempotent` flag.
    pub fn cal_add_if_novel(
        &self,
        grain_type: &str,
        fields: &serde_json::Map<String, serde_json::Value>,
    ) -> Result<(Hash, bool)> {
        self.check_verb(Verb::Write, self.write_ns(fields))?;
        let ingressed = self.ingress_fields(fields)?;
        let fields = ingressed.as_ref().unwrap_or(fields);
        let mut m = self.store.lock().unwrap();
        build_grain_from_json(grain_type, fields, AddIfNovelSink { m: &mut m })
    }

    /// Record one tool execution. The single implementation behind
    /// `record_tool_call` on every binding, so Python and Node cannot drift.
    ///
    /// Each call is stamped with an identity, which is what makes this record an
    /// *occurrence* rather than a value. Without one they collapsed: grains are
    /// content-addressed over the whole blob and `created_at` has millisecond
    /// resolution, so byte-identical calls recorded inside one millisecond hash
    /// to a single address, and since 1.1.1 a duplicate add is a silent no-op.
    /// An agent retrying a failing tool in a tight loop is precisely that
    /// workload — and "how many times did this fail" is the entire input to
    /// `loop.tool_failure`, so the flagship analyzer lost its signal exactly
    /// where it is meant to fire. The documented five-failure example stored one
    /// grain and produced no recommendation at all (#66).
    ///
    /// `call_id` is the host's invocation id (an LLM `tool_call_id`), stored as
    /// the grain's `tool_call_id` and queryable as such — the correlation key
    /// back to the provider transcript. Absent one, `occurrence_id` mints a
    /// synthetic id.
    ///
    /// It is not a de-duplication key: replaying the same `call_id` later writes
    /// a second grain, since `created_at` is part of the content address too.
    /// Recording is append-only, and a host replaying a tool log owns not
    /// replaying it twice.
    // This signature is mirrored by the CLI, MCP, Python, and Node surfaces;
    // grouping fields would make those scalar-in APIs diverge.
    //
    // The Wave-0 extension (governed-agents proposal §8): the async lifecycle
    // vocabulary (`status`/`failure_cause`/`executor_kind`/`correlation_id`),
    // the run↔plan join (`workflow_hash` + `node_id` → an `mg:step_action`
    // link), and the run-correlation key (`run_id`) were typed Tool fields
    // unreachable from every non-Rust surface — the execution-record READ
    // APIs existed everywhere while only Rust code could produce the records.
    // All values route through `build_grain_from_json`, so enum strings and
    // link shapes get the same strict validation as `add()`.
    #[allow(clippy::too_many_arguments)]
    pub fn record_tool_call(
        &self,
        ns: &str,
        tool_name: &str,
        input: Option<&str>,
        result: &str,
        is_error: bool,
        thread: Option<&str>,
        call_id: Option<&str>,
        run_id: Option<&str>,
        workflow_hash: Option<&str>,
        node_id: Option<&str>,
        status: Option<&str>,
        failure_cause: Option<&str>,
        executor_kind: Option<&str>,
        correlation_id: Option<&str>,
    ) -> Result<Hash> {
        let fields = build_tool_call_fields(
            ns,
            tool_name,
            input,
            result,
            is_error,
            thread,
            call_id,
            run_id,
            workflow_hash,
            node_id,
            status,
            failure_cause,
            executor_kind,
            correlation_id,
        )?;
        self.cal_add("tool", &fields)
    }

    /// Record the immutable configuration for a run and link the run id to it.
    /// The State is timestamped at epoch zero deliberately: it is a value-level
    /// configuration artifact, so identical JSON must have one content address
    /// even when observed by different runs at different wall-clock times.
    pub fn record_run_manifest(&self, run_id: &str, config_json: &str) -> Result<(Hash, Hash)> {
        self.check_verb(Verb::Write, areev_core::authz::HARNESS_NS)?;
        // Same contract as every other run_id write surface (§2 item 3).
        areev_core::types::validate_run_id(run_id)?;
        let config: serde_json::Value = serde_json::from_str(config_json).map_err(|e| {
            AreevError::Validation(format!("run manifest config must be valid JSON: {e}"))
        })?;
        if !config.is_object() {
            return Err(AreevError::Validation(
                "run manifest config must be a JSON object".into(),
            ));
        }

        let state = State::new(config)
            .namespace(areev_core::authz::HARNESS_NS)
            .created_at(0);
        let (_, config_hash) = serialize_grain(&state)?;
        let link = Fact::new(
            &format!("run:{run_id}"),
            "mg:harness",
            &config_hash.to_hex(),
        )
        .namespace(areev_core::authz::HARNESS_NS);
        let mut store = self.store.lock().unwrap();
        let hashes = store.add_batch(&[&state, &link])?;
        Ok((hashes[0], hashes[1]))
    }

    /// Add many grains in one store transaction. Each entry is the same
    /// `(grain_type, fields)` pair [`cal_add`](CalStoreFacade::cal_add) takes,
    /// validated identically — the batching is in the write, not the parsing,
    /// so a malformed entry fails the whole call and writes nothing.
    ///
    /// Worth roughly 1.6x over the same grains added one at a time (244 ->
    /// 148 us/grain, measured at 2k grains), saturating around a batch of 10.
    /// Note this only helps with the BM25 text index **off**: with it on, the
    /// per-row index cost dominates so completely that batch size makes no
    /// difference at all (~17ms/grain at every size) — see
    /// `defer_text_index` and tursodatabase/turso#8170.
    pub fn cal_add_batch(
        &self,
        entries: &[(String, serde_json::Map<String, serde_json::Value>)],
    ) -> Result<Vec<Hash>> {
        // Every entry's namespace is checked before anything is written —
        // a batch is all-or-nothing for authorization like it is for
        // validation.
        for (_, fields) in entries {
            self.check_verb(Verb::Write, self.write_ns(fields))?;
        }
        // Build every grain first so a bad entry is rejected before anything
        // is written, then hand the whole set to the store as one batch.
        let mut built: Vec<Box<dyn areev_store::AddableDyn>> = Vec::with_capacity(entries.len());
        for (grain_type, fields) in entries {
            let ingressed = self.ingress_fields(fields)?;
            let fields = ingressed.as_ref().unwrap_or(fields);
            build_grain_from_json(grain_type, fields, CollectSink { out: &mut built })?;
        }
        let refs: Vec<&dyn areev_store::AddableDyn> = built.iter().map(|b| b.as_ref()).collect();
        let mut m = self.store.lock().unwrap();
        m.add_batch(&refs)
    }

    fn hit(grain: DeserializedGrain) -> SearchHit {
        let hash = grain.hash;
        SearchHit {
            grain,
            score: 1.0,
            hash,
            score_breakdown: None,
            explanation: None,
            scope_depth: None,
            source_namespace: None,
            relative_time: None,
            conflict_status: None,
            supersession_status: None,
            superseded_by_hash: None,
            recall_source: None,
        }
    }
}

/// Sink that keeps the built grain instead of writing it, so a whole batch
/// can be validated up front and then written in one transaction.
struct CollectSink<'a> {
    out: &'a mut Vec<Box<dyn areev_store::AddableDyn>>,
}
impl GrainSink for CollectSink<'_> {
    type Out = ();
    fn consume<G: Grain + Clone + 'static>(self, grain: &G) -> Result<()> {
        self.out.push(Box::new(grain.clone()));
        Ok(())
    }
}

/// The one field-builder behind `record_tool_call` on the facade AND on
/// [`PrincipalSession`] — two entry points, one validation path, so the
/// attributed and unattributed surfaces cannot drift.
#[allow(clippy::too_many_arguments)]
fn build_tool_call_fields(
    ns: &str,
    tool_name: &str,
    input: Option<&str>,
    result: &str,
    is_error: bool,
    thread: Option<&str>,
    call_id: Option<&str>,
    run_id: Option<&str>,
    workflow_hash: Option<&str>,
    node_id: Option<&str>,
    status: Option<&str>,
    failure_cause: Option<&str>,
    executor_kind: Option<&str>,
    correlation_id: Option<&str>,
) -> Result<serde_json::Map<String, serde_json::Value>> {
    let mut fields = serde_json::Map::new();
    fields.insert("tool_name".into(), serde_json::json!(tool_name));
    if let Some(input) = input {
        let input = serde_json::from_str(input).map_err(|e| {
            AreevError::Validation(format!("record_tool_call input must be valid JSON: {e}"))
        })?;
        fields.insert("input".into(), input);
    }
    fields.insert("content".into(), serde_json::json!(result));
    fields.insert("is_error".into(), serde_json::json!(is_error));
    fields.insert("namespace".into(), serde_json::json!(ns));
    if let Some(t) = thread {
        fields.insert("session_id".into(), serde_json::json!(t));
    }
    fields.insert(
        "tool_call_id".into(),
        serde_json::json!(match call_id {
            Some(id) => id.to_string(),
            None => occurrence_id(),
        }),
    );
    if let Some(r) = run_id {
        fields.insert("run_id".into(), serde_json::json!(r));
    }
    // The step_action link is a pair: a hash with no node names no execution
    // record, a node with no plan links nothing. Both or neither — half a
    // join is refused, not guessed at.
    match (workflow_hash, node_id) {
        (Some(wf), Some(node)) => {
            fields.insert(
                "related_to".into(),
                serde_json::json!([{
                    "hash": wf,
                    "relation_type": areev_core::types::step_action_relation(node),
                }]),
            );
        }
        (None, None) => {}
        _ => {
            return Err(AreevError::Validation(
                "record_tool_call takes workflow_hash and node_id together \
                 (both or neither) — half an mg:step_action link is meaningless"
                    .into(),
            ))
        }
    }
    if let Some(s) = status {
        fields.insert("status".into(), serde_json::json!(s));
    }
    if let Some(fc) = failure_cause {
        fields.insert("failure_cause".into(), serde_json::json!(fc));
    }
    if let Some(ek) = executor_kind {
        fields.insert("executor_kind".into(), serde_json::json!(ek));
    }
    if let Some(cid) = correlation_id {
        fields.insert("correlation_id".into(), serde_json::json!(cid));
    }
    Ok(fields)
}

/// A principal-scoped, rebind-race-free write session — see
/// [`AreevFacade::principal_session`]. Holds its own fail-closed rights;
/// never reads or writes the facade's shared authz slot. Reads still go
/// through the facade's normal surface — this type exists for **attributed
/// writes** (a run journaling as its triggering principal, a responder
/// answering a HITL ask), where "who" must be per-call, not ambient.
pub struct PrincipalSession<'f> {
    facade: &'f AreevFacade,
    authz: areev_core::authz::AuthzSet,
}

impl PrincipalSession<'_> {
    pub fn principal(&self) -> &str {
        self.authz.principal()
    }

    pub fn authz(&self) -> &areev_core::authz::AuthzSet {
        &self.authz
    }

    /// The namespace a write lands in (mirrors the facade's default rule).
    fn write_ns<'a>(&'a self, fields: &'a serde_json::Map<String, serde_json::Value>) -> &'a str {
        fields
            .get("namespace")
            .and_then(|v| v.as_str())
            .or(self.facade.namespace.as_deref())
            .unwrap_or("default")
    }

    /// Add a grain, checked against THIS session's rights and attributed to
    /// its principal: grains that do not set `author_did` get it stamped.
    pub fn cal_add(
        &self,
        grain_type: &str,
        fields: &serde_json::Map<String, serde_json::Value>,
    ) -> Result<Hash> {
        self.authz
            .check(areev_core::authz::Verb::Write, self.write_ns(fields))?;
        let mut attributed;
        let fields = if fields.contains_key("author_did") {
            fields
        } else {
            attributed = fields.clone();
            attributed.insert(
                "author_did".into(),
                serde_json::json!(self.authz.principal()),
            );
            &attributed
        };
        let ingressed = self.facade.ingress_fields(fields)?;
        let fields = ingressed.as_ref().unwrap_or(fields);
        let mut m = self.facade.store.lock().unwrap();
        build_grain_from_json(grain_type, fields, AddSink { m: &mut m })
    }

    /// The attributed twin of [`AreevFacade::record_tool_call`] — one
    /// field-builder ([`build_tool_call_fields`]), two authorization roots.
    #[allow(clippy::too_many_arguments)]
    pub fn record_tool_call(
        &self,
        ns: &str,
        tool_name: &str,
        input: Option<&str>,
        result: &str,
        is_error: bool,
        thread: Option<&str>,
        call_id: Option<&str>,
        run_id: Option<&str>,
        workflow_hash: Option<&str>,
        node_id: Option<&str>,
        status: Option<&str>,
        failure_cause: Option<&str>,
        executor_kind: Option<&str>,
        correlation_id: Option<&str>,
    ) -> Result<Hash> {
        let fields = build_tool_call_fields(
            ns,
            tool_name,
            input,
            result,
            is_error,
            thread,
            call_id,
            run_id,
            workflow_hash,
            node_id,
            status,
            failure_cause,
            executor_kind,
            correlation_id,
        )?;
        self.cal_add("tool", &fields)
    }
}

struct AddSink<'a> {
    m: &'a mut Areev,
}
impl GrainSink for AddSink<'_> {
    type Out = Hash;
    fn consume<G: Grain + Clone + 'static>(self, grain: &G) -> Result<Hash> {
        self.m.add(grain)
    }
}

struct AddIfNovelSink<'a> {
    m: &'a mut Areev,
}
impl GrainSink for AddIfNovelSink<'_> {
    type Out = (Hash, bool);
    fn consume<G: Grain + Clone + 'static>(self, grain: &G) -> Result<(Hash, bool)> {
        self.m.add_if_novel(grain)
    }
}

struct SupersedeSink<'a> {
    m: &'a mut Areev,
    old: Hash,
}
impl GrainSink for SupersedeSink<'_> {
    type Out = Hash;
    fn consume<G: Grain + Clone + 'static>(self, grain: &G) -> Result<Hash> {
        let mut g = grain.clone();
        self.m.supersede(&self.old, &mut g)
    }
}

impl AreevFacade {
    /// Note an entry the file carries that this process could not load.
    ///
    /// Skipping is deliberate — one unloadable row must not make the whole
    /// memory unusable — but skipping *silently* is not: a saved query that
    /// vanishes without a word looks like it was never written.
    fn note_meta_warning(&self, kind: &str, name: &str, why: impl std::fmt::Display) {
        self.meta_warnings
            .lock()
            .expect("meta warnings poisoned")
            .push(format!(
                "{kind} \"{name}\" is in the file but could not be loaded ({why}); \
                 it is not available in this process and will be lost if you overwrite it"
            ));
    }

    /// Saved queries and templates the file carries that this process could not
    /// load. Empty when everything in the file is usable here.
    pub fn meta_warnings(&self) -> Vec<String> {
        self.meta_warnings
            .lock()
            .expect("meta warnings poisoned")
            .clone()
    }

    /// Run `f` against the saved-query registry, rehydrating it from the
    /// file's `meta` rows on first use. A row that fails to parse or register
    /// is skipped rather than failing the whole open — one bad entry must not
    /// make the memory unusable — but it is recorded in [`Self::meta_warnings`].
    fn with_queries<R>(&self, f: impl FnOnce(&mut QueryRegistry) -> R) -> R {
        let mut guard = self.queries.lock().expect("query registry poisoned");
        if guard.is_none() {
            let mut reg = QueryRegistry::new();
            match self.with_store(|m| m.meta_scan(QRY_PREFIX)) {
                Ok(rows) => {
                    for (name, json) in rows {
                        match serde_json::from_str::<PersistedQuery>(&json) {
                            Ok(p) => {
                                if let Err(e) = reg.register_full(
                                    &name,
                                    &p.body,
                                    &p.description,
                                    &p.params,
                                    p.last_run_at,
                                    p.updated_at,
                                ) {
                                    self.note_meta_warning("saved query", &name, e);
                                }
                            }
                            Err(e) => self.note_meta_warning("saved query", &name, e),
                        }
                    }
                }
                Err(e) => self.note_meta_warning("saved queries", "*", e),
            }
            *guard = Some(reg);
        }
        f(guard.as_mut().expect("initialised directly above"))
    }

    /// Same for custom templates.
    fn with_templates<R>(&self, f: impl FnOnce(&mut TemplateRegistry) -> R) -> R {
        let mut guard = self.templates.lock().expect("template registry poisoned");
        if guard.is_none() {
            let mut reg = TemplateRegistry::new();
            match self.with_store(|m| m.meta_scan(TPL_PREFIX)) {
                Ok(rows) => {
                    for (name, json) in rows {
                        match serde_json::from_str::<PersistedTemplate>(&json) {
                            Ok(p) => {
                                match reg.register(
                                    &name,
                                    &p.source,
                                    &p.description,
                                    p.parent.as_deref(),
                                ) {
                                    Ok(()) => {
                                        reg.restore_timestamps(&name, p.last_run_at, p.updated_at);
                                        // The FOR clause lives on the statement,
                                        // not in the body, so it only survives a
                                        // reload if it is put back explicitly.
                                        reg.set_grain_types(&name, &p.grain_types);
                                    }
                                    Err(e) => self.note_meta_warning("template", &name, e),
                                }
                            }
                            Err(e) => self.note_meta_warning("template", &name, e),
                        }
                    }
                }
                Err(e) => self.note_meta_warning("templates", "*", e),
            }
            *guard = Some(reg);
        }
        f(guard.as_mut().expect("initialised directly above"))
    }

    fn persist_query(&self, name: &str, p: &PersistedQuery) -> Result<()> {
        let json = serde_json::to_string(p)
            .map_err(|e| AreevError::Validation(format!("saved query \"{name}\": {e}")))?;
        self.with_store(|m| m.meta_put(&format!("{QRY_PREFIX}{name}"), &json))
    }

    fn snapshot_query(&self, name: &str) -> Option<PersistedQuery> {
        self.with_queries(|reg| {
            reg.get(name).map(|e| PersistedQuery {
                body: e.body.clone(),
                description: e.description.clone(),
                params: e.params.clone(),
                last_run_at: e.last_run_at,
                updated_at: e.updated_at,
            })
        })
    }

    fn snapshot_template(&self, name: &str) -> Option<PersistedTemplate> {
        self.with_templates(|reg| {
            reg.get(name).filter(|e| !e.builtin).map(|e| PersistedTemplate {
                source: e.template.source().to_string(),
                description: e.description.clone(),
                parent: e.parent.clone(),
                grain_types: e.grain_types.clone(),
                last_run_at: e.last_run_at,
                updated_at: e.updated_at,
            })
        })
    }
}

impl CalStoreFacade for AreevFacade {
    fn as_any(&self) -> Option<&dyn std::any::Any> {
        Some(self)
    }

    /// Record one assembly-budget sample into the telemetry sidecar (feeds the
    /// `budget_pressure` analyzer). Best-effort: telemetry never fails a query.
    fn note_assembly_budget(&self, overflow: bool) {
        let _ = self.with_store(|m| m.telemetry_note_budget(overflow));
    }

    /// The egress payload flag (proposal §4.1): active when any namespace
    /// declares an egress policy or the host installed the floor. Carries
    /// mapping *ids* only — the mapping itself stays in process (D5).
    fn anon_egress_report(&self) -> Option<serde_json::Value> {
        self.with_store(|m| {
            let declared = m.anon_declared();
            let floor = m.anonymize_egress_floor();
            let egress_ns: Vec<String> = declared
                .iter()
                .filter(|(_, mode)| mode == "egress")
                .map(|(ns, _)| ns.clone())
                .collect();
            if egress_ns.is_empty() && !floor {
                return None;
            }
            let mappings: Vec<serde_json::Value> = m
                .anon_mappings()
                .unwrap_or_default()
                .into_iter()
                .map(|(ns, id, _)| serde_json::json!({"ns": ns, "mapping_id": id}))
                .collect();
            Some(serde_json::json!({
                "namespaces": egress_ns,
                "floor": floor,
                "mappings": mappings,
            }))
        })
    }

    fn note_assembly_manifest(&self, manifest: &AssemblyManifest) {
        // A read-only principal must not acquire a write through telemetry.
        if !self
            .authz()
            .allows(Verb::Write, areev_core::authz::HARNESS_NS)
        {
            return;
        }
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let now = now_ms();
        let mut obs = Observation::new("areev:assemble", "system")
            .namespace(areev_core::authz::HARNESS_NS)
            .subject(&manifest.rendered_sha256)
            .object("assembly_manifest")
            .created_at(now);
        obs.frame_id = Some(format!(
            "assembly:{now}:{}",
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        obs.common.extra_fields.insert(
            "manifest".into(),
            serde_json::to_value(manifest).unwrap_or(serde_json::Value::Null),
        );
        obs.common.related_to = manifest
            .included_hashes
            .iter()
            .map(|hash| RelatedTo {
                hash: hash.clone(),
                relation_type: "mg:assembly_input".into(),
                weight: None,
            })
            .collect();
        // Off the recall measurement and best-effort: a manifest failure does
        // not change the successful ASSEMBLE result.
        let _ = self.with_store(|m| m.add(&obs));
    }

    // ── CAL host metadata: saved queries and custom templates ───────────
    //
    // The registry owns the rules (name shape, per-namespace cap, body size,
    // parameter count), so every write validates in memory first and only
    // then touches the file. If the file write fails the in-memory entry is
    // rolled back, so the registry never runs ahead of what is persisted.

    fn define_query(
        &self,
        name: &str,
        body: &str,
        description: Option<&str>,
        params: &[QueryParam],
    ) -> Result<()> {
        // Saved queries and templates are the host-config surface: admin.
        self.check_verb(Verb::Admin, "*")?;
        let existing = self.snapshot_query(name);
        self.with_queries(|reg| reg.register(name, body, description.unwrap_or(""), params))
            .map_err(AreevError::Validation)?;
        let Some(p) = self.snapshot_query(name) else {
            return Err(AreevError::Internal(format!(
                "saved query \"{name}\" vanished after registration"
            )));
        };
        if let Err(e) = self.persist_query(name, &p) {
            // Roll the registry back to whatever was there before.
            self.with_queries(|reg| {
                let _ = reg.delete(name);
                if let Some(prev) = &existing {
                    let _ = reg.register_full(
                        name,
                        &prev.body,
                        &prev.description,
                        &prev.params,
                        prev.last_run_at,
                        prev.updated_at,
                    );
                }
            });
            return Err(e);
        }
        Ok(())
    }

    fn drop_query(&self, name: &str) -> Result<()> {
        self.check_verb(Verb::Admin, "*")?;
        // Snapshot before deleting: if the file write fails the registry has to
        // go back, or the entry is gone here and still on disk — it would
        // reappear on the next open, which reads as the drop never happening.
        let existing = self.snapshot_query(name);
        self.with_queries(|reg| reg.delete(name))
            .map_err(AreevError::Validation)?;
        if let Err(e) = self.with_store(|m| m.meta_delete(&format!("{QRY_PREFIX}{name}"))) {
            if let Some(prev) = &existing {
                self.with_queries(|reg| {
                    let _ = reg.register_full(
                        name,
                        &prev.body,
                        &prev.description,
                        &prev.params,
                        prev.last_run_at,
                        prev.updated_at,
                    );
                });
            }
            return Err(e);
        }
        Ok(())
    }

    fn list_queries(&self) -> Vec<QueryListEntry> {
        self.with_queries(|reg| reg.list())
    }

    fn get_query(&self, name: &str) -> Option<QueryEntry> {
        self.with_queries(|reg| reg.get(name).cloned())
    }

    fn update_query_last_run(&self, name: &str) -> Result<()> {
        let now = now_secs();
        let updated = self.with_queries(|reg| {
            let e = reg.get(name)?;
            // Built-ins are not persisted, and a re-run within the same second
            // cannot change the one-second-resolution timestamp — so it would
            // be a write transaction that rewrites the row with what it
            // already holds.
            if e.builtin || e.last_run_at == Some(now) {
                return None;
            }
            let (body, description, params, updated_at) = (
                e.body.clone(),
                e.description.clone(),
                e.params.clone(),
                e.updated_at,
            );
            reg.register_full(
                name,
                &body,
                &description,
                &params,
                Some(now),
                updated_at,
            )
            .ok()?;
            Some(PersistedQuery {
                body,
                description,
                params,
                last_run_at: Some(now),
                updated_at,
            })
        });
        match updated {
            Some(p) => self.persist_query(name, &p),
            None => Ok(()),
        }
    }

    fn define_template(
        &self,
        name: &str,
        source: &str,
        description: Option<&str>,
        parent: Option<&str>,
        grain_types: &[String],
    ) -> Result<()> {
        self.check_verb(Verb::Admin, "*")?;
        let existing = self.snapshot_template(name);
        self.with_templates(|reg| {
            reg.register(name, source, description.unwrap_or(""), parent)?;
            // The FOR clause rides the statement, not the body — set it on the
            // entry too, so the in-memory and persisted views agree.
            reg.set_grain_types(name, grain_types);
            Ok(())
        })
        .map_err(|e: CalError| AreevError::Validation(e.to_string()))?;
        let p = PersistedTemplate {
            source: source.to_string(),
            description: description.unwrap_or("").to_string(),
            parent: parent.map(str::to_string),
            grain_types: grain_types.to_vec(),
            last_run_at: None,
            updated_at: Some(now_secs()),
        };
        let json = serde_json::to_string(&p)
            .map_err(|e| AreevError::Validation(format!("template \"{name}\": {e}")))?;
        if let Err(e) = self.with_store(|m| m.meta_put(&format!("{TPL_PREFIX}{name}"), &json)) {
            // Roll the registry back to whatever was there before, so it never
            // runs ahead of what is persisted.
            self.with_templates(|reg| {
                let _ = reg.delete(name);
                if let Some(prev) = &existing {
                    if reg
                        .register(name, &prev.source, &prev.description, prev.parent.as_deref())
                        .is_ok()
                    {
                        reg.restore_timestamps(name, prev.last_run_at, prev.updated_at);
                        reg.set_grain_types(name, &prev.grain_types);
                    }
                }
            });
            return Err(e);
        }
        Ok(())
    }

    fn drop_template(&self, name: &str) -> Result<()> {
        self.check_verb(Verb::Admin, "*")?;
        let existing = self.snapshot_template(name);
        self.with_templates(|reg| reg.delete(name))
            .map_err(|e| AreevError::Validation(e.to_string()))?;
        if let Err(e) = self.with_store(|m| m.meta_delete(&format!("{TPL_PREFIX}{name}"))) {
            if let Some(prev) = &existing {
                self.with_templates(|reg| {
                    if reg
                        .register(name, &prev.source, &prev.description, prev.parent.as_deref())
                        .is_ok()
                    {
                        reg.restore_timestamps(name, prev.last_run_at, prev.updated_at);
                        reg.set_grain_types(name, &prev.grain_types);
                    }
                });
            }
            return Err(e);
        }
        Ok(())
    }

    fn list_templates(&self) -> Vec<TemplateInfo> {
        self.with_templates(|reg| reg.list())
    }

    fn get_template(&self, name: &str) -> Option<TemplateInfo> {
        // Direct lookup, not `list().find()`: this runs on the FORMAT render
        // path, and building the whole list (every source string cloned) to
        // throw all but one away is work proportional to the registry on every
        // rendered query.
        self.with_templates(|reg| {
            reg.get(name).map(|e| TemplateInfo {
                name: name.to_string(),
                description: e.description.clone(),
                builtin: e.builtin,
                parent: e.parent.clone(),
                grain_types: e.grain_types.clone(),
                source: e.template.source().to_string(),
                last_run_at: e.last_run_at,
                updated_at: e.updated_at,
            })
        })
    }

    fn record_template_run(&self, name: &str) {
        let persisted = self.with_templates(|reg| {
            let before = reg.get(name).and_then(|e| e.last_run_at);
            reg.record_run(name);
            let entry = reg.get(name)?;
            // `last_run_at` has one-second resolution, so a second render in
            // the same second cannot change what is on disk. Skipping that
            // write matters: this runs on the FORMAT path, and a rendered
            // recall should not cost a write transaction per call.
            if entry.builtin || entry.last_run_at == before {
                return None;
            }
            Some(PersistedTemplate {
                source: entry.template.source().to_string(),
                description: entry.description.clone(),
                parent: entry.parent.clone(),
                grain_types: entry.grain_types.clone(),
                last_run_at: entry.last_run_at,
                updated_at: entry.updated_at,
            })
        });
        if let Some(p) = persisted {
            if let Ok(json) = serde_json::to_string(&p) {
                let _ = self.with_store(|m| m.meta_put(&format!("{TPL_PREFIX}{name}"), &json));
            }
        }
    }

    fn recall(&self, params: &RecallParams) -> Result<Vec<SearchHit>> {
        // `WHERE <field> IN (...)` with nothing in it selects nothing. This is
        // the sharp edge of the whole `IN` family: `LET $friends = …` binding to
        // the empty set is the *natural* "this user has no friends yet"
        // outcome, and treating an empty filter as no filter answered with the
        // entire table. Fail closed, before any leg runs.
        for empty in [&params.subject_in, &params.relation_in, &params.object_in] {
            if empty.as_ref().is_some_and(|v| v.is_empty()) {
                return Ok(Vec::new());
            }
        }

        // ---- namespace scope resolution --------------------------------
        // The requested scope terms: an explicit `namespace IN (…)` set wins,
        // else the single namespace filter, else the session default. Each
        // term may be an exact name, a `"org.*"` prefix pattern (parent +
        // descendants), or mount-routed `alias.inner` (the inner part may
        // itself be a pattern).
        let requested_terms: Vec<String> = match &params.namespaces {
            Some(set) => set.clone(),
            None => match params.namespace.as_deref().or(self.namespace.as_deref()) {
                Some(one) => vec![one.to_string()],
                None => Vec::new(),
            },
        };
        // A session with no namespace scope keeps the historical contract:
        // the read gate checks `*`, the recall runs in the store default.
        if requested_terms.is_empty() {
            self.check_verb(Verb::Read, "*")?;
        }
        // Mount-route every term; one RECALL reads one store, so a set that
        // spans mounts (or mixes a mount with the session store) refuses with
        // a pointer at ASSEMBLE, which exists for exactly that.
        let mut mount_alias: Option<String> = None;
        let mut inner_scopes: Vec<(String, NsScope)> = Vec::with_capacity(requested_terms.len());
        for (i, full) in requested_terms.iter().enumerate() {
            let (alias, inner) = match full.split_once('.') {
                Some((a, rest)) if self.mounts.contains_key(a) => {
                    (Some(a.to_string()), rest.to_string())
                }
                _ => (None, full.clone()),
            };
            if i == 0 {
                mount_alias = alias;
            } else if mount_alias != alias {
                return Err(AreevError::Validation(
                    "a namespace set cannot span mounted memories in one RECALL — query each \
                     store separately, or ASSEMBLE with one source per store"
                        .into(),
                ));
            }
            inner_scopes.push((inner.clone(), NsScope::parse(&inner)?));
        }
        // Exact terms pass the read gate as themselves — the caller named
        // them, so the refusal may too. (Unknown names stay in the scope and
        // recall as empty; existence is not checked here.)
        for (term, scope) in &inner_scopes {
            if matches!(scope, NsScope::Exact(_)) {
                self.check_verb(Verb::Read, term)?;
            }
        }
        let mut m = match &mount_alias {
            Some(a) => self.mounts.get(a).unwrap().lock().unwrap(),
            None => self.store.lock().unwrap(),
        };
        // Resolve prefix scopes against the target store's namespace
        // registry, gating each DISCOVERED namespace on the session's read
        // grants — fail closed, and without naming what the caller could not
        // already know: in a multi-tenant file, a refusal that names a
        // sibling tenant's namespace would itself be a disclosure, so it
        // names the pattern the caller typed instead.
        let owner = self.session_is_owner();
        let mut ns_set: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
        for (term, scope) in &inner_scopes {
            match scope {
                NsScope::Exact(e) => {
                    ns_set.insert(e.clone());
                }
                NsScope::Prefix { .. } => {
                    for cand in m.namespaces_in_scope(scope)? {
                        if !owner && self.check_verb(Verb::Read, &cand).is_err() {
                            return Err(AreevError::AuthzDenied(format!(
                                "principal {} lacks read on part of scope {term:?} — the \
                                 prefix covers a namespace outside this session's grants",
                                self.session_principal()
                            )));
                        }
                        ns_set.insert(cand);
                    }
                }
            }
        }
        // A scope of nothing selects nothing: all-pattern terms that matched
        // no namespace answer empty, honestly — but only when the caller
        // actually named a scope (the no-scope session default is below).
        if ns_set.is_empty() && !requested_terms.is_empty() {
            return Ok(Vec::new());
        }
        if ns_set.is_empty() {
            ns_set.insert("shared".to_string()); // the no-scope store default
        }
        let ns_list: Vec<String> = ns_set.into_iter().collect();
        // `scoped` = the result set can span namespaces; the single exact
        // namespace — the common case and the voice path — keeps every
        // single-namespace fast path below.
        let scoped = ns_list.len() > 1;
        let ns = ns_list[0].as_str();
        let k = params.limit.unwrap_or(16).min(1000);

        // M4: hybrid recall — structural leg + BM25 leg fused with RRF.
        // A query alone, a subject alone, or both are all valid.
        //
        // With neither a subject nor a free-text query there is no leg to hang
        // ranking on, so fall back to a bounded recent-by-type scan (newest
        // first). This is the "reflect over recent experience" path — e.g.
        // `RECALL events RECENT 20`, `RECALL observations WHERE session_id = X`
        // — whose WHERE conditions (session_id, observer_id, object, …) are
        // applied as post-filters below and by the executor. Bare `RECALL *`
        // (no grain type) with no anchor is still rejected as too broad.
        // Which leg ran matters below: `recent` takes no structural predicates
        // and does not know about supersession, so this path has to reapply
        // both itself.
        //
        // `WHERE subject IN (a, b, c)` anchors just as well as `subject = a` —
        // it is a union of anchored legs, one per value. Treating it as
        // unanchored dropped the query into the recent-by-type scan, where the
        // filter was never applied at all (it lives in `RecallParams` and
        // nothing downstream read it), so the documented
        // `LET $friends = SUBJECTS OF (…)` pattern answered with *everybody's*
        // preferences instead of the friends'. Anchoring per value also means
        // the scan is bounded by the subjects asked for rather than by
        // `default_limit` — a post-filter over the newest 50 would miss a
        // named subject whose grains are older than that.
        let anchors: Vec<String> = match (&params.subject, &params.subject_in) {
            (Some(s), _) => vec![s.clone()],
            (None, Some(values)) => values.clone(),
            (None, None) => Vec::new(),
        };
        let unanchored = anchors.is_empty() && params.query.is_none();
        // `WITH superseded` — the caller is asking about the past, so every leg
        // widens from the heads to the whole supersession chain.
        let include_superseded = params.exclude_superseded == Some(false);
        // `WHERE session_id = "…"` with nothing else to anchor on: read the
        // thread index instead of a page of the namespace. Without this, the
        // `unanchored` arm below scans `recent`/`recent_live` newest-first
        // across the WHOLE namespace and the session filter is applied
        // afterwards, so on a busy namespace the tail of one conversation can
        // sit entirely outside the scanned page and the query answers
        // "nothing" — the single most common read a chat/voice agent makes,
        // silently wrong. `idx_thread(ns, session, seq)` bounds the scan by the
        // session, so k here means k turns of THIS conversation.
        //
        // A session IS an anchor, so this branch also lifts the "RECALL needs a
        // subject filter, a free-text query, or a specific grain type" refusal
        // for the typed-less form: `RECALL WHERE session_id = "…"` is bounded.
        //
        // Only on the unanchored path: with a subject or a free-text query the
        // hybrid leg is already bounded by that anchor (a far tighter bound
        // than a namespace page), and the post-filter finishes the job. Kept as
        // its own branch rather than nested inside the unanchored arm so the
        // recent-by-type match below stays exactly as it was.
        let raw = if let Some(session) = params.session_id.as_deref().filter(|_| unanchored) {
            let n = k.saturating_mul(Self::RECALL_OVERFETCH);
            if scoped {
                m.recent_in_session_scoped(&ns_list, session, params.grain_type, n, !include_superseded)?
            } else {
                m.recent_in_session(ns, session, params.grain_type, n, !include_superseded)?
            }
        } else if let Some(order) = params.order_by.as_ref().and_then(|k| {
            // `created_at` is the one sort key the `grains` table carries as a
            // column, so it is the one ORDER BY that can be served from the
            // scan instead of re-sorting a page afterwards. The executor only
            // sets `order_by` for it; anything else it ranks itself over a
            // widened scan (see `RecallParams::order_by`).
            // `params.grain_type.is_some()` matters: ORDER BY is not an
            // anchor, so without a type this must still fall through to the
            // "RECALL needs a subject filter, a free-text query, or a specific
            // grain type" refusal below rather than scanning the namespace.
            (unanchored && params.grain_type.is_some() && k.field == "created_at").then(|| {
                if k.descending {
                    areev_store::RecentOrder::CreatedAtDesc
                } else {
                    areev_store::RecentOrder::CreatedAtAsc
                }
            })
        }) {
            let n = k.min(1000);
            if scoped {
                m.recent_ordered_scoped(&ns_list, params.grain_type, n, !include_superseded, order)?
            } else {
                m.recent_ordered(ns, params.grain_type, n, !include_superseded, order)?
            }
        } else if unanchored {
            match params.grain_type {
                // Heads only, unless `WITH superseded` asked otherwise. The
                // anchored leg already serves heads; this one read the grains
                // table straight through, so a superseded value came back
                // alongside the head that replaced it and recall reported both
                // as current. Supersession is index-layer state, so the
                // distinction has to be made in the query, not after it.
                Some(_) if !include_superseded => {
                    if scoped {
                        m.recent_live_scoped(
                            &ns_list,
                            params.grain_type,
                            k.saturating_mul(Self::RECALL_OVERFETCH),
                        )?
                    } else {
                        m.recent_live(ns, params.grain_type, k.saturating_mul(Self::RECALL_OVERFETCH))?
                    }
                }
                Some(_) => {
                    if scoped {
                        m.recent_scoped(
                            &ns_list,
                            params.grain_type,
                            k.saturating_mul(Self::RECALL_OVERFETCH),
                        )?
                    } else {
                        m.recent(ns, params.grain_type, k.saturating_mul(Self::RECALL_OVERFETCH))?
                    }
                }
                None => {
                    return Err(AreevError::Validation(
                        "RECALL needs a subject filter, a free-text (LIKE) query, \
                         or a specific grain type with RECENT/LIMIT"
                            .into(),
                    ))
                }
            }
        } else {
            // Translate the recall flags the executor set from `WITH` options
            // (diversity / rerank / query_expansion) into engine tuning. MMR is
            // the only diversity method wired in-engine; the threshold variant
            // is not reachable from CAL's `WITH diversity`.
            let tuning = areev_store::RecallTuning {
                query_expansion: params.query_expansion == Some(true),
                rerank: params.rerank.is_some(),
                diversity_lambda: params.diversity.as_ref().and_then(|d| match d.method {
                    DiversityMethod::Mmr { lambda } => Some(lambda),
                    DiversityMethod::Threshold(_) => None,
                }),
                include_superseded,
            };
            let budget = k.saturating_mul(Self::RECALL_OVERFETCH);
            match anchors.len() {
                // The common case, and the voice hot path: one anchored leg.
                0 | 1 if !scoped => m.recall_hybrid_tuned(
                    ns,
                    anchors.first().map(String::as_str),
                    params.relation.as_deref(),
                    params.query.as_deref(),
                    budget,
                    None,
                    tuning,
                )?,
                // Same leg over a resolved namespace set (`IN` / a prefix
                // scope that expanded to more than one namespace).
                0 | 1 => m.recall_hybrid_scoped(
                    &ns_list,
                    anchors.first().map(String::as_str),
                    params.relation.as_deref(),
                    params.query.as_deref(),
                    budget,
                    None,
                    tuning,
                )?,
                // `subject IN (a, b, …)`: one anchored leg per value, unioned.
                // Each leg gets the full budget so a subject with many grains
                // cannot starve the others out of the result set; the union is
                // deduped by content address and the caller's LIMIT is applied
                // below, as it is on every other path.
                _ => {
                    let mut seen: HashSet<Hash> = HashSet::new();
                    let mut union = Vec::new();
                    for anchor in &anchors {
                        let leg = if scoped {
                            m.recall_hybrid_scoped(
                                &ns_list,
                                Some(anchor.as_str()),
                                params.relation.as_deref(),
                                params.query.as_deref(),
                                budget,
                                None,
                                tuning,
                            )?
                        } else {
                            m.recall_hybrid_tuned(
                                ns,
                                Some(anchor.as_str()),
                                params.relation.as_deref(),
                                params.query.as_deref(),
                                budget,
                                None,
                                tuning,
                            )?
                        };
                        for g in leg {
                            if seen.insert(g.hash) {
                                union.push(g);
                            }
                        }
                    }
                    // Newest first, matching what a single anchored leg returns.
                    union.sort_by(|a, b| {
                        b.get_i64("created_at")
                            .unwrap_or(0)
                            .cmp(&a.get_i64("created_at").unwrap_or(0))
                    });
                    union
                }
            }
        };

        // `WITH multi_hop(n)` — entity-graph expansion.
        //
        // Take the entities the first pass surfaced (its results' subject and
        // object terms), anchor a fresh recall on each, and add what comes back
        // to the candidate pool. Repeat `n` times, following the graph outward.
        //
        // Both directions: an entity is followed as a subject (what does X point
        // at) *and* as an object (what points at X). Forward-only expansion made
        // "who else works here" — the archetypal one-hop question — return
        // nothing, since entities are harvested from the `object` field but were
        // only ever re-anchored as subjects. The reverse leg reads the OSP index,
        // so it sees the relations the file declares as entity relations; that is
        // the same rule every other reverse traversal in the engine follows.
        //
        // Expansion happens here, before the post-filters and the `LIMIT` below,
        // so hops *compete* for the k slots the caller asked for rather than
        // extending past them. Appended after the first pass, so a direct match
        // always outranks something reached by association.
        //
        // Fail-open, like every other recall refinement: a hop that errors is
        // skipped rather than failing the query.
        let mut raw = raw;
        if let Some(hops) = params.multi_hop.filter(|h| *h > 0) {
            let mut seen: HashSet<Hash> = raw.iter().map(|g| g.hash).collect();
            let mut visited: HashSet<String> = HashSet::new();
            let mut frontier: Vec<String> = Vec::new();
            let push_entities = |g: &DeserializedGrain,
                                     visited: &mut HashSet<String>,
                                     out: &mut Vec<String>| {
                for field in ["subject", "object"] {
                    if let Some(v) = g.get_str(field) {
                        if !v.is_empty() && visited.insert(v.to_string()) {
                            out.push(v.to_string());
                        }
                    }
                }
            };
            for g in raw.iter().take(Self::MULTI_HOP_SEED) {
                push_entities(g, &mut visited, &mut frontier);
            }

            let budget = k.saturating_mul(Self::RECALL_OVERFETCH);
            'hops: for _ in 0..hops {
                let mut next: Vec<String> = Vec::new();
                for entity in std::mem::take(&mut frontier) {
                    if raw.len() >= budget {
                        break 'hops;
                    }
                    // Hops stay inside the resolved scope: a graph walk must
                    // not escape the namespaces the query (and its authz
                    // sweep) selected.
                    let forward = if scoped {
                        m.recall_hybrid_scoped(
                            &ns_list,
                            Some(&entity),
                            None,
                            params.query.as_deref(),
                            Self::MULTI_HOP_FANOUT,
                            None,
                            areev_store::RecallTuning::default(),
                        )
                        .unwrap_or_default()
                    } else {
                        m.recall_hybrid(
                            ns,
                            Some(&entity),
                            None,
                            params.query.as_deref(),
                            Self::MULTI_HOP_FANOUT,
                            None,
                        )
                        .unwrap_or_default()
                    };
                    let reverse: Vec<DeserializedGrain> = ns_list
                        .iter()
                        .flat_map(|hop_ns| {
                            m.grains_by_object(hop_ns, &entity, Self::MULTI_HOP_FANOUT)
                                .unwrap_or_default()
                        })
                        .collect();
                    for g in forward.into_iter().chain(reverse) {
                        if seen.insert(g.hash) {
                            push_entities(&g, &mut visited, &mut next);
                            raw.push(g);
                        }
                    }
                }
                if next.is_empty() {
                    break;
                }
                frontier = next;
            }
        }
        drop(m);

        // Defense in depth for scoped recalls: every leg is already
        // namespace-bounded in SQL, but a future leg that forgets the bound
        // would silently WIDEN a result set — the worst failure shape here —
        // so membership is re-asserted per grain when the scope is a set.
        let ns_filter: HashSet<&str> = ns_list.iter().map(String::as_str).collect();
        let mut hits: Vec<SearchHit> = raw
            .into_iter()
            .filter(|g| {
                !scoped || ns_filter.contains(g.get_str("namespace").unwrap_or("shared"))
            })
            // `relation` reaches the anchored leg as a store-side predicate,
            // but `recent` takes no predicates at all — so on that path the
            // filter was simply dropped and `RECALL facts WHERE relation = "x"`
            // answered with every grain of that type. Silently returning more
            // than was asked for is worse than returning nothing.
            .filter(|g| {
                !unanchored
                    || match &params.relation {
                        Some(r) => g.get_str("relation") == Some(r.as_str()),
                        None => true,
                    }
            })
            .filter(|g| match &params.object {
                Some(o) => g.get_str("object") == Some(o.as_str()),
                None => true,
            })
            // The `IN` family. `subject_in` already anchored the legs above, but
            // an anchored leg on "jane" can still surface grains where jane is
            // the *object*, so all three are re-applied here as membership
            // tests. Before this, nothing in the engine read these three fields
            // at all: the executor set them and the query returned every row the
            // rest of the WHERE matched, with no error and no warning.
            .filter(|g| set_contains(&params.subject_in, g.get_str("subject")))
            .filter(|g| set_contains(&params.relation_in, g.get_str("relation")))
            .filter(|g| set_contains(&params.object_in, g.get_str("object")))
            .filter(|g| match params.grain_type {
                Some(gt) => g.grain_type == gt,
                None => true,
            })
            .filter(|g| {
                let ca = g.get_i64("created_at").unwrap_or(0);
                params.time_start.is_none_or(|t| ca >= t)
                    && params.time_end.is_none_or(|t| ca <= t)
            })
            .filter(|g| match params.confidence_threshold {
                Some(c) => g.get_f64("confidence").unwrap_or(0.0) >= c,
                None => true,
            })
            // `WITH recency_weight(w)` re-ranks the CANDIDATES, so it has to
            // see them before this bound — ranking a set already cut to k is
            // the same truncated-window mistake ORDER BY made. Without the
            // option the take stays exactly where it was, so the hot path is
            // unchanged.
            .take(if params.recency_weight.is_some() {
                usize::MAX
            } else {
                k
            })
            .map(Self::hit)
            .collect();

        // ── `WITH recency_weight(w)` ─────────────────────────────────────
        //
        // Parsed since 1.0, stored on RecallParams, and read by NOTHING — ten
        // of the built-in saved queries in `queries.rs` pass it, so ten shipped
        // queries were quietly not doing what they said. Implemented to the
        // formula the field has always documented:
        //
        //     final = (1 - w) * relevance + w * freshness
        //     freshness = 1 / (1 + age_hours)
        //
        // `relevance` comes from FUSION ORDER, not `hit.score`: every hit
        // leaves `Self::hit` with score 1.0, so the ranking a recall carries is
        // positional (RRF/structural order), not numeric. Rank i of n therefore
        // scores `1 - i/n` — order-preserving, and identical to the current
        // order when w = 0.
        //
        // State facts are exempt, as documented: "lives in Berlin" does not
        // become less true with age, so decaying it would push a current fact
        // below a stale event.
        if let Some(w) = params.recency_weight.filter(|w| *w > 0.0) {
            let w = w.clamp(0.0, 1.0);
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as i64)
                .unwrap_or(0);
            let n = hits.len().max(1) as f64;
            for (i, h) in hits.iter_mut().enumerate() {
                let relevance = 1.0 - (i as f64) / n;
                let is_state = h.grain.get_str("temporal_type") == Some("state");
                h.score = if is_state {
                    relevance
                } else {
                    let age_hours =
                        ((now - h.grain.get_i64("created_at").unwrap_or(now)).max(0) as f64)
                            / 3_600_000.0;
                    let freshness = 1.0 / (1.0 + age_hours);
                    (1.0 - w) * relevance + w * freshness
                };
            }
            // Stable, so equal scores keep fusion order.
            hits.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
            hits.truncate(k);
        }

        // Label the history. A widened recall returns stale versions next to the
        // heads that replaced them, and nothing in an immutable blob says which
        // is which — unlabeled, `WITH superseded` would quietly feed a model
        // outdated values that read as current. Renderers and the RF-2
        // context rule key off these two fields, so stamping them is what makes
        // the widened result honest rather than merely bigger.
        if include_superseded && !hits.is_empty() {
            let hashes: Vec<Hash> = hits.iter().map(|h| h.hash).collect();
            let mut m = match &mount_alias {
                Some(a) => self.mounts.get(a).unwrap().lock().unwrap(),
                None => self.store.lock().unwrap(),
            };
            // Fail-open, like every other recall refinement: an unlabeled result
            // is worse than a labeled one but far better than a failed query.
            if let Ok(map) = m.supersession_map(&hashes) {
                drop(m);
                for hit in &mut hits {
                    hit.supersession_status = Some(match map.get(&hit.hash) {
                        Some(_) => SupersessionStatus::Superseded,
                        None => SupersessionStatus::Current,
                    });
                    hit.superseded_by_hash = map.get(&hit.hash).copied();
                }
            }
        }

        // `WITH conflict_resolution` — keep only the newest grain per
        // (subject, relation). Documented in §5 and advertised by DESCRIBE, but
        // the implementation lived on the ASSEMBLE post-merge path only, which
        // a RECALL payload never reaches — so a caller specifically trying to
        // avoid feeding two contradictory values into a model's context got
        // both, with no signal. Ties break on the later hash so the choice is
        // deterministic across nodes rather than dependent on scan order.
        if params.conflict_resolution == Some(true) {
            let mut newest: HashMap<(String, String), (i64, Hash)> = HashMap::new();
            for h in &hits {
                let (Some(s), Some(r)) = (h.grain.get_str("subject"), h.grain.get_str("relation"))
                else {
                    continue;
                };
                let created = h.grain.get_i64("created_at").unwrap_or(0);
                let key = (s.to_string(), r.to_string());
                let better = match newest.get(&key) {
                    None => true,
                    Some((best, best_hash)) => {
                        (created, h.hash.as_bytes()) > (*best, best_hash.as_bytes())
                    }
                };
                if better {
                    newest.insert(key, (created, h.hash));
                }
            }
            for h in &mut hits {
                let keep = match (h.grain.get_str("subject"), h.grain.get_str("relation")) {
                    // A grain with no (subject, relation) key has nothing to
                    // conflict with, so it is never the loser of a comparison.
                    (Some(s), Some(r)) => newest
                        .get(&(s.to_string(), r.to_string()))
                        .is_none_or(|(_, hash)| *hash == h.hash),
                    _ => true,
                };
                h.conflict_status = Some(if keep {
                    ConflictStatus::Current
                } else {
                    ConflictStatus::Outdated
                });
            }
            hits.retain(|h| h.conflict_status != Some(ConflictStatus::Outdated));
        }

        // `WITH annotate_relative_time` — "3 hours ago" beside the epoch
        // milliseconds, so a model reading the context does not have to do
        // date arithmetic to know whether a memory is fresh.
        if params.annotate_relative_time == Some(true) {
            let now = now_ms();
            for h in &mut hits {
                h.relative_time = h
                    .grain
                    .get_i64("created_at")
                    .map(|created| relative_time_label(now - created));
            }
        }

        // `WITH explanation` — why this grain came back. Template-based and
        // derived entirely from the predicates that ran (the field's contract:
        // no LLM, suitable for EU AI Act Art. 86 disclosure).
        if params.explanation == Some(true) {
            for h in &mut hits {
                let mut why: Vec<String> = Vec::new();
                if !anchors.is_empty() {
                    if let Some(s) = h.grain.get_str("subject") {
                        if anchors.iter().any(|a| a == s) {
                            why.push(format!("anchored on subject \"{s}\""));
                        }
                    }
                }
                if let Some(q) = &params.query {
                    why.push(format!("matched the free-text query \"{q}\""));
                }
                if let Some(r) = params.relation.as_deref().or_else(|| {
                    params.relation_in.as_ref().and_then(|_| h.grain.get_str("relation"))
                }) {
                    why.push(format!("relation is \"{r}\""));
                }
                if why.is_empty() {
                    why.push(match params.grain_type {
                        Some(gt) => format!("recent {} in this namespace", gt.as_str()),
                        None => "recent grain in this namespace".into(),
                    });
                }
                if h.supersession_status == Some(SupersessionStatus::Superseded) {
                    why.push("included as history by WITH superseded".into());
                }
                h.explanation = Some(why.join("; "));
            }
        }

        Ok(hits)
    }

    fn exists(&self, hash: &Hash) -> Result<bool> {
        if self.session_is_owner() {
            return self.store.lock().unwrap().has(hash);
        }
        // An existence bit is a read, and a hash names no namespace — so a
        // restricted session resolves the grain first and answers only for
        // namespaces its grants can read.
        let fetched = self.store.lock().unwrap().get(hash);
        match fetched {
            Ok(g) => {
                self.check_verb(Verb::Read, g.get_str("namespace").unwrap_or("shared"))?;
                Ok(true)
            }
            Err(AreevError::NotFound(_)) => Ok(false),
            Err(e) => Err(e),
        }
    }

    fn get(&self, hash: &Hash) -> Result<DeserializedGrain> {
        let g = self.store.lock().unwrap().get(hash)?;
        self.check_verb(Verb::Read, g.get_str("namespace").unwrap_or("shared"))?;
        Ok(g)
    }

    fn count(&self) -> Result<usize> {
        // A store-wide count spans every namespace.
        self.check_verb(Verb::Read, "*")?;
        self.store.lock().unwrap().count()
    }

    fn get_history(&self, namespace: &str, subject: &str, relation: &str) -> Result<Vec<VersionEntry>> {
        self.check_verb(Verb::Read, namespace)?;
        let entries = self.store.lock().unwrap().history(namespace, subject, relation)?;
        Ok(entries
            .into_iter()
            .map(|e| VersionEntry {
                hash: e.hash,
                object: e.object,
                created_at: e.created_at,
                confidence: e.confidence,
                superseded_by: e.superseded_by,
            })
            .collect())
    }

    fn open_forks(&self) -> Result<Vec<ForkGroupInfo>> {
        // The fork listing spans every namespace.
        self.check_verb(Verb::Read, "*")?;
        let groups = self.with_store(|m| m.open_forks())?;
        Ok(groups
            .into_iter()
            .map(|f| ForkGroupInfo {
                namespace: f.namespace,
                subject: f.subject,
                relation: f.relation,
                // `Areev::heads` orders `created_at DESC, hash DESC` — the same
                // tuple the provisional-head election uses — so tip 0 is the
                // value recall serves. Preserve that order; CONTRADICTIONS
                // reports every other tip as a peer of it.
                heads: f.heads.iter().map(|h| h.to_hex()).collect(),
            })
            .collect())
    }

    fn default_namespace(&self) -> Option<&str> {
        self.namespace.as_deref()
    }

    fn active_user(&self) -> Option<&str> {
        self.user.as_deref()
    }

    fn cal_add(
        &self,
        grain_type: &str,
        fields: &serde_json::Map<String, serde_json::Value>,
    ) -> Result<Hash> {
        self.check_verb(Verb::Write, self.write_ns(fields))?;
        let ingressed = self.ingress_fields(fields)?;
        let fields = ingressed.as_ref().unwrap_or(fields);
        let mut m = self.store.lock().unwrap();
        build_grain_from_json(grain_type, fields, AddSink { m: &mut m })
    }

    fn cal_supersede(
        &self,
        old_hash: &Hash,
        grain_type: &str,
        fields: &serde_json::Map<String, serde_json::Value>,
    ) -> Result<Hash> {
        if !self.session_is_owner() {
            // Authorize against the TARGET's namespace, the way `cal_delete`
            // does — that is the grain this statement actually rewrites out
            // of live recall. `SET namespace` names where the *replacement*
            // lands, so checking only that lets a caller supersede a grain in
            // a namespace it holds no supersede grant on simply by naming one
            // it does.
            let old_ns = {
                let g = self.store.lock().unwrap().get(old_hash)?;
                g.get_str("namespace").unwrap_or("shared").to_string()
            };
            self.check_verb(Verb::Supersede, &old_ns)?;
            // A supersession that moves the value to another namespace also
            // writes there, so that namespace needs the grant too.
            let new_ns = self.write_ns(fields);
            if new_ns != old_ns {
                self.check_verb(Verb::Supersede, new_ns)?;
            }
        }
        let ingressed = self.ingress_fields(fields)?;
        let fields = ingressed.as_ref().unwrap_or(fields);
        let mut m = self.store.lock().unwrap();
        build_grain_from_json(
            grain_type,
            fields,
            SupersedeSink {
                m: &mut m,
                old: *old_hash,
            },
        )
    }

    /// `FORGET <hash>` — tombstone a single grain by content address. Only
    /// ever hits the session store; mounts are read-only by construction.
    /// Capped upstream by `CalExecutorConfig::allow_destructive_ops`; the
    /// session's `delete` grant on the grain's own namespace is the
    /// authorization.
    fn cal_delete(&self, hash: &Hash, because: Option<&str>) -> Result<()> {
        if !self.session_is_owner() {
            let ns = {
                let g = self.store.lock().unwrap().get(hash)?;
                g.get_str("namespace").unwrap_or("shared").to_string()
            };
            self.check_verb(Verb::Delete, &ns)?;
        }
        self.store.lock().unwrap().forget(hash)?;
        self.audit_tier2("delete", &format!("hash:{}", hash.to_hex()), because, 1, &[])
    }

    /// `MERGE` — the resolved value supersedes every open tip; the merge
    /// grain records all parents. `supersede` is the authorization (it is
    /// a head rewrite, however principled), and the reason rides the
    /// grain's supersession justification.
    fn cal_merge(
        &self,
        subject: &str,
        relation: &str,
        object: &str,
        confidence: f64,
        because: &str,
    ) -> Result<Hash> {
        let ns = self.namespace.as_deref().unwrap_or("shared").to_string();
        self.check_verb(Verb::Supersede, &ns)?;
        let mut merged = areev_core::types::Fact::new(subject, relation, object)
            .namespace(&ns)
            .confidence(confidence)
            .created_at(now_epoch_ms());
        merged.common.supersession_justification = Some(because.to_string());
        self.store.lock().unwrap().merge_heads(&ns, subject, relation, &mut merged)
    }

    /// `RELATED` — the bounded k-hop walk in the session namespace.
    fn cal_related(
        &self,
        start: &str,
        relations: &[&str],
        direction: &str,
        depth: usize,
        limit: usize,
    ) -> Result<Vec<String>> {
        let ns = self.namespace.as_deref().unwrap_or("shared").to_string();
        self.check_verb(Verb::Read, &ns)?;
        let dir = match direction {
            "in" => areev_store::Direction::In,
            "both" => areev_store::Direction::Both,
            _ => areev_store::Direction::Out,
        };
        self.store
            .lock()
            .unwrap()
            .related(&ns, start, relations, dir, depth.clamp(1, 4), limit.min(256))
    }

    /// `NOVELTY` — nearest existing grains; requires a host embedder.
    fn cal_novelty(
        &self,
        text: &str,
        subject: Option<&str>,
        relation: Option<&str>,
        k: usize,
    ) -> Result<Vec<(String, f64)>> {
        let ns = self.namespace.as_deref().unwrap_or("shared").to_string();
        self.check_verb(Verb::Read, &ns)?;
        let rows = self
            .store
            .lock()
            .unwrap()
            .nearest_semantic(&ns, subject, relation, text, k.clamp(1, 64))?;
        Ok(rows.into_iter().map(|(h, sim)| (h.to_hex(), sim as f64)).collect())
    }

    fn cal_entity_at(
        &self,
        subject: &str,
        relation: &str,
        at_ms: i64,
        axis: &str,
    ) -> Result<Option<serde_json::Value>> {
        let ns = self.namespace.as_deref().unwrap_or("shared").to_string();
        self.check_verb(Verb::Read, &ns)?;
        let axis = match axis {
            "knowledge" => areev_store::Axis::Knowledge,
            _ => areev_store::Axis::World,
        };
        let g = self
            .store
            .lock()
            .unwrap()
            .entity_at(&ns, subject, relation, at_ms, axis)?;
        Ok(g.as_ref().map(grain_json))
    }

    fn cal_run_trace(&self, run_id: &str, limit: usize) -> Result<serde_json::Value> {
        let ns = self.namespace.as_deref().unwrap_or("shared").to_string();
        self.check_verb(Verb::Read, &ns)?;
        let mut m = self.store.lock().unwrap();
        let recorded: Vec<_> = m.run_trace(&ns, run_id, limit)?.iter().map(grain_json).collect();
        let produced: Vec<_> = m.run_yield(&ns, run_id, limit)?.iter().map(grain_json).collect();
        Ok(serde_json::json!({ "recorded": recorded, "produced": produced }))
    }

    fn cal_runs_touching(&self, hash: &Hash, depth: usize) -> Result<Vec<String>> {
        let ns = self.namespace.as_deref().unwrap_or("shared").to_string();
        self.check_verb(Verb::Read, &ns)?;
        self.store.lock().unwrap().runs_touching(&ns, hash, depth)
    }

    fn cal_derived_from(&self, hash: &Hash) -> Result<Vec<serde_json::Value>> {
        // Provenance spans namespaces — the read needs the wide grant.
        self.check_verb(Verb::Read, "*")?;
        let grains = self.store.lock().unwrap().grains_derived_from(hash)?;
        Ok(grains.iter().map(grain_json).collect())
    }

    fn cal_stats(&self) -> Result<serde_json::Value> {
        self.check_verb(Verb::Read, "*")?;
        let s = self.store.lock().unwrap().stats()?;
        Ok(serde_json::json!({
            "grains": s.grains,
            "current": s.current,
            "triples": s.triples,
            "terms": s.terms,
            "ops": s.ops,
            "events_indexed": s.events_indexed,
        }))
    }

    fn cal_verify(&self) -> Result<serde_json::Value> {
        self.check_verb(Verb::Read, "*")?;
        let r = self.store.lock().unwrap().verify()?;
        Ok(serde_json::json!({
            "integrity": r.integrity,
            "fts_notes": r.fts_notes,
            "grains": r.grains,
            "hash_mismatches": r.hash_mismatches,
            "undecodable": r.undecodable,
        }))
    }

    /// `REMEMBER` (CAL 1.3) — capture into the session namespace via the
    /// same store path as `areev remember` and the bindings' `capture`. The
    /// observer is the bound principal; a `write` grant is the
    /// authorization.
    fn cal_remember(
        &self,
        content: &str,
        session_id: Option<&str>,
        role: Option<&str>,
        run_id: Option<&str>,
    ) -> Result<Hash> {
        let ns = self.namespace.as_deref().unwrap_or("shared").to_string();
        self.check_verb(Verb::Write, &ns)?;
        let observer = self.session_principal();
        self.store.lock().unwrap().capture(
            &ns,
            content,
            &areev_store::Capture {
                observer: Some(&observer),
                session_id,
                role,
                run_id,
            },
        )
    }

    /// `GRANT` (CAL 1.3 §8.15) — writes the grant grain: a Fact in the
    /// reserved `agent:authz` namespace, subject = grantee, relation =
    /// `mg:permits`, object = the canonical grant string, grantor + reason
    /// in `context`. Authorized by `admin`. A session's own rights are
    /// fixed at bind — a grant to the current principal applies to
    /// sessions bound after it, like a new database connection.
    fn cal_grant(
        &self,
        principal: &str,
        verbs: &[String],
        namespaces: &[String],
        because: Option<&str>,
    ) -> Result<Hash> {
        self.check_verb(Verb::Admin, "*")?;
        let grant = parse_grant_parts(principal, verbs, namespaces)?;
        let mut fact = areev_core::types::Fact::new(
            principal,
            areev_core::authz::REL_PERMITS,
            &grant.to_object_string(),
        )
        .namespace(areev_core::authz::AUTHZ_NS)
        .created_at(now_epoch_ms());
        fact.common.context = Some(serde_json::json!({
            "grantor": self.session_principal(),
            "because": because.unwrap_or(""),
        }));
        self.store.lock().unwrap().add(&fact)
    }

    /// `REVOKE` (CAL 1.3 §8.15) — retraction by supersession. Each live
    /// grant whose namespace scope is covered by the revoke loses the
    /// revoked verbs: reduced grants supersede in place; a grant with
    /// nothing left is superseded by a `revoked` retraction record. A grant
    /// with a WIDER scope than the revoke is refused by name — no silent
    /// splitting; revoke at the grant's own scope.
    fn cal_revoke(
        &self,
        principal: &str,
        verbs: &[String],
        namespaces: &[String],
        because: Option<&str>,
    ) -> Result<usize> {
        self.check_verb(Verb::Admin, "*")?;
        let revoke = parse_grant_parts(principal, verbs, namespaces)?;
        let revoke_all_ns = revoke.namespaces.iter().any(|n| n == "*");

        let mut m = self.store.lock().unwrap();
        let grains = m.recall(
            areev_core::authz::AUTHZ_NS,
            principal,
            Some(areev_core::authz::REL_PERMITS),
            256,
        )?;
        let mut touched = 0usize;
        for g in &grains {
            let Some(obj) = g.get_str("object") else { continue };
            let Ok(existing) = areev_core::authz::Grant::from_object_string(obj) else {
                continue; // retraction records and junk never carry rights
            };
            if !existing.verbs.iter().any(|v| revoke.verbs.contains(v)) {
                continue;
            }
            let covered = revoke_all_ns
                || existing
                    .namespaces
                    .iter()
                    .all(|n| revoke.namespaces.contains(n));
            if !covered {
                return Err(AreevError::Validation(format!(
                    "grant {obj:?} for {principal} spans more namespaces than the \
                     revoke — revoke at the grant's own scope"
                )));
            }
            let remaining: Vec<_> = existing
                .verbs
                .iter()
                .copied()
                .filter(|v| !revoke.verbs.contains(v))
                .collect();
            let object = if remaining.is_empty() {
                // Nothing left: a retraction record. `authz_grants` skips it
                // (it is not a parseable grant), so it conveys no rights —
                // and the history stays append-only.
                "revoked".to_string()
            } else {
                areev_core::authz::Grant {
                    verbs: remaining,
                    namespaces: existing.namespaces.clone(),
                }
                .to_object_string()
            };
            let mut replacement = areev_core::types::Fact::new(
                principal,
                areev_core::authz::REL_PERMITS,
                &object,
            )
            .namespace(areev_core::authz::AUTHZ_NS)
            .created_at(now_epoch_ms());
            replacement.common.context = Some(serde_json::json!({
                "grantor": self.session_principal(),
                "because": because.unwrap_or(""),
                "revoked": revoke.to_object_string(),
            }));
            m.supersede(&g.hash, &mut replacement)?;
            touched += 1;
        }
        Ok(touched)
    }

    /// `SHOW GRANTS` — the live grant rows. Reading the authz namespace
    /// requires `read` on it (or the owner session).
    fn cal_show_grants(&self, principal: Option<&str>) -> Result<Vec<crate::facade::GrantRow>> {
        self.check_verb(Verb::Read, areev_core::authz::AUTHZ_NS)?;
        let mut m = self.store.lock().unwrap();
        let grains = match principal {
            Some(p) => m.recall(
                areev_core::authz::AUTHZ_NS,
                p,
                Some(areev_core::authz::REL_PERMITS),
                256,
            )?,
            None => m.recent(
                areev_core::authz::AUTHZ_NS,
                Some(areev_core::types::GrainType::Fact),
                1000,
            )?,
        };
        let mut rows = Vec::new();
        for g in &grains {
            if g.get_str("relation") != Some(areev_core::authz::REL_PERMITS) {
                continue;
            }
            let (Some(subject), Some(obj)) = (g.get_str("subject"), g.get_str("object")) else {
                continue;
            };
            // Only parseable grants convey rights; retraction records and
            // junk are history, not ACL.
            if areev_core::authz::Grant::from_object_string(obj).is_err() {
                continue;
            }
            rows.push(crate::facade::GrantRow {
                principal: subject.to_string(),
                object: obj.to_string(),
                hash: g.hash.to_hex(),
            });
        }
        Ok(rows)
    }

    /// `FORGET SUBJECT` — identity-scoped erasure in the session namespace
    /// (CAL 1.3 §8.14): every grain referencing the identity, its partition
    /// keys, history, and dictionary entries; `text_mentions` extends it to
    /// indexed-text mentions. Authorized by `erase` on that namespace.
    fn cal_forget_user(
        &self,
        user_id: &str,
        text_mentions: bool,
        because: &str,
    ) -> Result<crate::store_types::ErasureProof> {
        let ns = self.namespace.as_deref().unwrap_or("shared").to_string();
        self.check_verb(Verb::Erase, &ns)?;
        let report = self.store.lock().unwrap().forget_subject_with(
            &ns,
            user_id,
            areev_store::ErasureOptions { text_mentions },
        )?;
        let stale_exports = self
            .store
            .lock()
            .unwrap()
            .corpus_exports_touching_subject(user_id)
            .unwrap_or_default();
        // The audit target carries a FINGERPRINT, never the identity: an
        // immutable, replicating audit grain naming the erased subject
        // would put the reference straight back into the file (and every
        // bundle and archive made from it). See
        // `areev_core::authz::subject_fingerprint`.
        self.audit_tier2(
            "erase",
            &format!(
                "subject:{} ns:{ns}",
                areev_core::authz::subject_fingerprint(user_id)
            ),
            Some(because),
            report.grains_erased,
            &stale_exports,
        )?;
        Ok(crate::store_types::ErasureProof {
            user_id: user_id.to_string(),
            count: report.grains_erased as u64,
            // Tombstone + index erasure, not key destruction — there is no
            // key fingerprint to report on this path.
            key_fingerprint: String::new(),
            timestamp: now_epoch_ms(),
            user_record_deleted: report.grains_erased > 0,
        })
    }

    /// `REPORT SUBJECT` — the read-only DSAR selection (OMS 1.6 draft):
    /// the SAME selector `cal_forget_user` erases with, hydrated instead.
    /// Session-namespace-scoped like the erasure form; authorized by
    /// `read` — it is a pure read and writes no audit grain (the audit
    /// obligation is on destruction, not access).
    fn cal_subject_report(
        &self,
        subject_id: &str,
        text_mentions: bool,
    ) -> Result<crate::store_types::SubjectReportResult> {
        let ns = self.namespace.as_deref().unwrap_or("shared").to_string();
        self.check_verb(Verb::Read, &ns)?;
        let report = self.store.lock().unwrap().subject_report_with(
            &ns,
            subject_id,
            areev_store::ErasureOptions { text_mentions },
        )?;
        Ok(crate::store_types::SubjectReportResult {
            identity_names: report.identity_names,
            grains: report.grains.iter().map(grain_json).collect(),
        })
    }

    /// `PURGE OLDER THAN` — the retention sweep (CAL 1.3 §8.14), scoped to
    /// one namespace (the statement's `IN`, else the session's, else
    /// "shared" — never an implicit all-namespace sweep from CAL).
    /// Authorized by `erase` on that namespace.
    fn cal_purge_stale(
        &self,
        min_age_days: f64,
        namespace: Option<&str>,
        batch_limit: usize,
        grain_type: Option<&str>,
        because: &str,
    ) -> Result<usize> {
        let ns = namespace
            .or(self.namespace.as_deref())
            .unwrap_or("shared")
            .to_string();
        self.check_verb(Verb::Erase, &ns)?;
        let gtype = match grain_type {
            None => None,
            Some(t) => Some(areev_core::types::GrainType::from_str(t).ok_or_else(|| {
                AreevError::Validation(format!("unknown grain type for PURGE: {t:?}"))
            })?),
        };
        let cutoff_ms = now_epoch_ms() - (min_age_days * 86_400_000.0) as i64;
        // `LIMIT n` bounds the sweep to the oldest n matches — the operator
        // asked for a reviewable batch, not the namespace.
        let exports = self.store.lock().unwrap().corpus_exports().unwrap_or_default();
        let report = self.store.lock().unwrap().forget_older_than_capped(
            Some(&ns),
            cutoff_ms,
            gtype,
            Some(batch_limit),
        )?;
        let stale_exports =
            areev_store::exports_touching_hashes(&exports, &report.erased_hashes);
        self.audit_tier2(
            "erase",
            &format!(
                "older_than:{min_age_days}d ns:{ns}{}",
                grain_type.map(|t| format!(" type:{t}")).unwrap_or_default()
            ),
            Some(because),
            report.grains_erased,
            &stale_exports,
        )?;
        Ok(report.grains_erased)
    }
}

/// Validate DCL parts into a canonical [`areev_core::authz::Grant`]: every
/// verb must be in the verb registry, the principal non-empty, namespaces
/// non-empty strings. Fail-closed — one bad part refuses the whole
/// statement.
fn parse_grant_parts(
    principal: &str,
    verbs: &[String],
    namespaces: &[String],
) -> Result<areev_core::authz::Grant> {
    if principal.trim().is_empty() {
        return Err(AreevError::Validation("empty principal".into()));
    }
    if verbs.is_empty() {
        return Err(AreevError::Validation("no verbs named".into()));
    }
    let mut parsed = Vec::new();
    for v in verbs {
        let v = areev_core::authz::Verb::parse(v)?;
        if !parsed.contains(&v) {
            parsed.push(v);
        }
    }
    let namespaces: Vec<String> = if namespaces.is_empty() {
        vec!["*".to_string()]
    } else {
        namespaces.to_vec()
    };
    for n in &namespaces {
        if n.trim().is_empty() {
            return Err(AreevError::Validation("empty namespace".into()));
        }
        // Grants match namespaces EXACTLY (or `*` for all) — a prefix
        // pattern here would silently grant nothing today and a family
        // tomorrow, so `"org.*"` refuses with the rule spelled out rather
        // than becoming a grant on a namespace literally named "org.*".
        if n != "*" && NsScope::is_pattern(n) {
            return Err(AreevError::Validation(format!(
                "grants take exact namespaces or `*` (got {n:?}): prefix patterns are a \
                 recall scope, not a grant scope — grant each namespace, or `*`"
            )));
        }
    }
    Ok(areev_core::authz::Grant { verbs: parsed, namespaces })
}

/// The grain-as-JSON shape shared by the Wave-2 reads:
/// `{hash, type, fields}` — the same shape the MCP tools speak.
fn grain_json(g: &DeserializedGrain) -> serde_json::Value {
    serde_json::json!({
        "hash": g.hash.to_hex(),
        "type": g.grain_type.as_str(),
        "fields": g.fields.clone().into_iter().collect::<serde_json::Map<_, _>>(),
    })
}

/// Wall-clock epoch ms for audit stamps and PURGE cutoffs — host-side time,
/// deliberately outside the executor (which stays clock-free).
fn now_epoch_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}
