//! The egress boundary (docs/anonymization-proposal.md, P1): per-namespace
//! `anon:<ns>` policies, the session pseudonymizer state, and the field
//! transform every model-facing read exit routes through.
//!
//! Policy rows are file-truths (they replicate write-if-absent, like
//! `retention:`); the gate state here is per-handle. Fail-closed contract
//! (D3/D6): an `anon:` row this build cannot parse poisons the gate — reads
//! that would consult it error instead of serving raw, while policy writes
//! (`set`/`clear`) keep working so the row can be repaired.
//!
//! Cost when idle: every read exit pays one [`AnonGate::is_idle`] check —
//! a bool and a map-emptiness test — so the µs-class recall/latest gates
//! (`examples/bench.rs`) are unaffected until a policy is declared.

use std::collections::{BTreeMap, HashMap};

use areev_core::anon::{AnonPolicy, DetectorBackend, SessionAnonymizer};
use areev_core::error::{AreevError, Result};
use areev_core::format::deserialize::DeserializedGrain;

/// `meta` key prefix for per-namespace anonymization policies.
pub(crate) const ANON_PREFIX: &str = "anon:";

/// Bound on each namespace's session pseudonym table (D5: never an unbounded
/// re-identification table on a long-lived handle). Oldest-first eviction.
const MAX_SESSION_ENTRIES: usize = 4096;

/// Grain fields that ARE identities by construction (proposal §5.1): the
/// whole value is category `person`, no detection needed.
const IDENTITY_FIELDS: &[&str] = &["subject", "user_id", "observer"];

/// Fields the transform never touches: `namespace` routes policy lookup and
/// mounts; run/session ids are operational join keys the runtime replays on.
const SKIP_FIELDS: &[&str] = &["namespace", "session_id", "run_id", "node_id", "parent"];

pub(crate) struct AnonGate {
    /// Keys value-derived (`memory`-scope and ingress) pseudonym tokens.
    /// HKDF from the page key; `None` on a plaintext file, where those
    /// features refuse at `set` time (Q5 resolved conservatively:
    /// pseudonymized-at-rest composes with encryption, it does not replace
    /// it).
    memory_key: Option<[u8; 32]>,
    /// ns → parsed policy, rebuilt from `meta` on open and on set/clear.
    policies: HashMap<String, AnonPolicy>,
    /// Set when any `anon:` row failed to parse: reads fail closed with this
    /// message until the row is repaired.
    poisoned: Option<String>,
    /// Host-installed restrictive cap: forces egress on for namespaces with
    /// no declared policy (default policy). Can never switch a declared
    /// policy off.
    floor: bool,
    /// Persistent per-ns pseudonymizers for `session`-scope policies.
    sessions: HashMap<String, SessionAnonymizer>,
    /// Latest `context`-scope pseudonymizer (one per read call), kept so the
    /// in-process caller can still fetch the mapping (D5 custody).
    last_context: Option<(String, SessionAnonymizer)>,
    /// Keys `mapping_id` (D11). HKDF from the page key on encrypted files,
    /// random per handle otherwise — either way never persisted.
    session_key: [u8; 32],
    /// `audit`-mode counters: (ns, category) → detections seen. In-memory,
    /// process-lifetime; never the spans' plaintext.
    audit_counts: BTreeMap<(String, String), u64>,
    /// Host-installed detector backends by kind ("ner"/"llm") — per-process
    /// capabilities, never persisted, embedder-style (proposal §5.2–5.3).
    backends: HashMap<String, Box<dyn DetectorBackend>>,
    /// Keys the sealed vault rows (`HKDF(page_key, areev.vault.v1)`);
    /// `None` on a plaintext file, where vault persistence refuses at set.
    vault_key: Option<[u8; 32]>,
    /// Unsealed vault rows per ns, loaded at policy reload; consumed to
    /// seed the first session so tokens continue across process restarts.
    vault_seed: HashMap<String, Vec<(String, String)>>,
    /// Persistent per-ns pseudonymizers for ingress transforms — always
    /// keyed (value-derived tokens, D8), independent of the egress scope.
    ingress_sessions: HashMap<String, SessionAnonymizer>,
    /// Per-ns identity values accumulated from identity fields (proposal
    /// §5.1 known-identity propagation): a subject one grain declares makes
    /// the bare name detectable in another grain's prose. Bounded; values
    /// only ever feed detection, never leave the process.
    known: HashMap<String, Vec<String>>,
}

/// Bound on the per-ns accumulated identity list.
const MAX_KNOWN_IDENTITIES: usize = 1024;

impl AnonGate {
    pub(crate) fn new(
        session_key: [u8; 32],
        memory_key: Option<[u8; 32]>,
        vault_key: Option<[u8; 32]>,
    ) -> Self {
        AnonGate {
            memory_key,
            backends: HashMap::new(),
            vault_key,
            vault_seed: HashMap::new(),
            policies: HashMap::new(),
            poisoned: None,
            floor: false,
            sessions: HashMap::new(),
            last_context: None,
            session_key,
            audit_counts: BTreeMap::new(),
            ingress_sessions: HashMap::new(),
            known: HashMap::new(),
        }
    }

    pub(crate) fn has_memory_key(&self) -> bool {
        self.memory_key.is_some()
    }

    /// Build a pseudonymizer honoring the policy's scope: `memory` scope is
    /// keyed (value-derived tokens) and therefore needs the memory key.
    fn make_session(&self, policy: AnonPolicy) -> Result<SessionAnonymizer> {
        if policy.scope == "memory" {
            let key = self.memory_key.ok_or_else(|| {
                AreevError::Validation(
                    "anonymization scope \"memory\" needs an encrypted memory (the \
                     value-derived token key is HKDF-derived from the page key)"
                        .into(),
                )
            })?;
            SessionAnonymizer::new_keyed(policy, key)
        } else {
            SessionAnonymizer::new(policy)
        }
    }

    /// The one branch every read exit pays when the feature is unused.
    #[inline]
    pub(crate) fn is_idle(&self) -> bool {
        self.policies.is_empty() && !self.floor && self.poisoned.is_none()
    }

    pub(crate) fn set_floor(&mut self, on: bool) {
        self.floor = on;
    }

    pub(crate) fn floor(&self) -> bool {
        self.floor
    }

    /// Rebuild the cache from the file's `anon:` rows. Unreadable rows
    /// poison the gate rather than silently meaning "no policy" (D3).
    pub(crate) fn reload(&mut self, rows: &[(String, String)]) {
        self.policies.clear();
        self.poisoned = None;
        for (ns, json) in rows {
            match AnonPolicy::from_json(json) {
                Ok(p) => {
                    self.policies.insert(ns.clone(), p);
                }
                Err(e) => {
                    self.poisoned = Some(format!(
                        "anon:{ns} policy row is unreadable ({e}) — egress reads fail closed \
                         until it is repaired with `anonymize set/clear`"
                    ));
                }
            }
        }
    }

    fn check_poisoned(&self) -> Result<()> {
        match &self.poisoned {
            Some(msg) => Err(AreevError::Validation(msg.clone())),
            None => Ok(()),
        }
    }

    pub(crate) fn poison_warning(&self) -> Option<&str> {
        self.poisoned.as_deref()
    }

    /// Effective policy for a namespace: the declared one, or the default
    /// policy under the floor. `None` = no anonymization for this ns.
    fn effective(&self, ns: &str) -> Result<Option<AnonPolicy>> {
        self.check_poisoned()?;
        if let Some(p) = self.policies.get(ns) {
            if p.mode == "off" {
                // The floor may not weaken a declared policy, but a declared
                // "off" is itself a declaration the floor overrides upward.
                if self.floor {
                    let mut forced = p.clone();
                    forced.mode = "egress".into();
                    return Ok(Some(forced));
                }
                return Ok(None);
            }
            return Ok(Some(p.clone()));
        }
        if self.floor {
            return Ok(Some(AnonPolicy::default())); // default mode is egress
        }
        Ok(None)
    }

    /// The effective mode for observability surfaces: `None` when this ns is
    /// untouched, else `off|egress|audit`.
    pub(crate) fn active_mode(&self, ns: &str) -> Result<Option<String>> {
        Ok(self.effective(ns)?.map(|p| p.mode))
    }

    pub(crate) fn declared(&self) -> Vec<(String, String)> {
        let mut v: Vec<(String, String)> =
            self.policies.iter().map(|(ns, p)| (ns.clone(), p.mode.clone())).collect();
        v.sort();
        v
    }

    pub(crate) fn audit_counts(&self) -> &BTreeMap<(String, String), u64> {
        &self.audit_counts
    }

    /// Every live session mapping the in-process caller may rehydrate with:
    /// `(ns, mapping_id, mapping)`, persistent `session`-scope tables first,
    /// then the latest `context`-scope call's table.
    pub(crate) fn mappings(&self) -> Result<Vec<crate::AnonMappingRow>> {
        let mut out = Vec::new();
        let mut nss: Vec<&String> = self.sessions.keys().collect();
        nss.sort();
        for ns in nss {
            let s = &self.sessions[ns];
            if !s.is_empty() {
                out.push((ns.clone(), s.mapping_id(Some(&self.session_key))?, s.mapping().clone()));
            }
        }
        if let Some((ns, s)) = &self.last_context {
            if !s.is_empty() {
                out.push((ns.clone(), s.mapping_id(Some(&self.session_key))?, s.mapping().clone()));
            }
        }
        Ok(out)
    }

    /// `mapping_id` for the namespace's most recent egress state, for the
    /// payload flags surfaces attach (`anonymized: true, mapping_id`).
    pub(crate) fn last_mapping_id(&self, ns: &str) -> Result<Option<String>> {
        if let Some(s) = self.sessions.get(ns) {
            if !s.is_empty() {
                return Ok(Some(s.mapping_id(Some(&self.session_key))?));
            }
        }
        if let Some((cns, s)) = &self.last_context {
            if cns == ns && !s.is_empty() {
                return Ok(Some(s.mapping_id(Some(&self.session_key))?));
            }
        }
        Ok(None)
    }

    /// Transform a batch of grains bound for a caller. `ns_hint` is the
    /// read's namespace parameter; reads without one (`get`,
    /// `grains_derived_from`, `open_forks`) resolve each grain's own
    /// `fields["namespace"]`, defaulting to "shared" like `extract_view`.
    pub(crate) fn egress_grains(
        &mut self,
        ns_hint: Option<&str>,
        grains: &mut [DeserializedGrain],
    ) -> Result<()> {
        if self.is_idle() {
            return Ok(());
        }
        self.check_poisoned()?;
        // Phase 1: resolve each grain's effective ns/policy and accumulate
        // identity values, so prose in one grain can name an identity that
        // only another grain (or an earlier call) declares structurally.
        let mut plan: Vec<Option<(String, AnonPolicy)>> = Vec::with_capacity(grains.len());
        for g in grains.iter() {
            let ns = resolve_ns(ns_hint, g);
            match self.effective(&ns)? {
                Some(policy) => {
                    for id in collect_identities(g) {
                        self.remember_identity(&ns, id);
                    }
                    plan.push(Some((ns, policy)));
                }
                None => plan.push(None),
            }
        }
        // Snapshot the per-ns identity lists once (bounded), so the borrow
        // of `self.sessions` below stays clean.
        let mut known_by_ns: HashMap<String, Vec<String>> = HashMap::new();
        for entry in plan.iter().flatten() {
            if !known_by_ns.contains_key(&entry.0) {
                known_by_ns
                    .insert(entry.0.clone(), self.known.get(&entry.0).cloned().unwrap_or_default());
            }
        }
        // Phase 2: transform. One context-scope session per (call, ns).
        // Backends are collected as direct field refs AFTER the last
        // &mut-self method call, so the borrows stay field-disjoint below.
        let memory_key = self.memory_key;
        let mut per_call: HashMap<String, SessionAnonymizer> = HashMap::new();
        let backends: Vec<&dyn DetectorBackend> =
            self.backends.values().map(|b| b.as_ref()).collect();
        for (g, entry) in grains.iter_mut().zip(plan) {
            let Some((ns, policy)) = entry else { continue };
            let known = known_by_ns.get(&ns).map(Vec::as_slice).unwrap_or(&[]);
            if policy.mode == "audit" {
                let counts = audit_grain(&ns, &policy, g, known, &backends)?;
                for (k, n) in counts {
                    *self.audit_counts.entry(k).or_insert(0) += n;
                }
                continue;
            }
            if policy.scope == "session" || policy.scope == "memory" {
                let session = match self.sessions.entry(ns.clone()) {
                    std::collections::hash_map::Entry::Occupied(e) => e.into_mut(),
                    std::collections::hash_map::Entry::Vacant(e) => {
                        let mut built = if policy.scope == "memory" {
                            let key = memory_key.ok_or_else(|| {
                                AreevError::Validation(
                                    "anonymization scope \"memory\" needs an encrypted memory"
                                        .into(),
                                )
                            })?;
                            SessionAnonymizer::new_keyed(policy.clone(), key)?
                        } else {
                            SessionAnonymizer::new(policy.clone())?
                        };
                        if let Some(seed) = self.vault_seed.remove(&ns) {
                            built.seed(seed);
                        }
                        e.insert(built)
                    }
                };
                transform_grain(session, g, known, &backends)?;
                session.evict_to(MAX_SESSION_ENTRIES);
            } else {
                let session = match per_call.entry(ns.clone()) {
                    std::collections::hash_map::Entry::Occupied(e) => e.into_mut(),
                    std::collections::hash_map::Entry::Vacant(e) => {
                        e.insert(SessionAnonymizer::new(policy.clone())?)
                    }
                };
                transform_grain(session, g, known, &backends)?;
            }
        }
        // Keep the latest context-scope table reachable for rehydration.
        if let Some((ns, session)) = per_call.into_iter().find(|(_, s)| !s.is_empty()) {
            self.last_context = Some((ns, session));
        }
        Ok(())
    }

    fn remember_identity(&mut self, ns: &str, id: String) {
        let list = self.known.entry(ns.to_string()).or_default();
        if !list.contains(&id) && list.len() < MAX_KNOWN_IDENTITIES {
            list.push(id);
        }
    }

    /// Seed the per-ns identity list (from the file's subject terms, at
    /// policy load) so prose-only grains are covered from the first read.
    pub(crate) fn seed_known(&mut self, ns: &str, ids: Vec<String>) {
        for id in ids {
            self.remember_identity(ns, id);
        }
    }

    /// Write-path feed: a subject written under a covered namespace joins
    /// the identity list immediately.
    pub(crate) fn note_written_identity(&mut self, ns: &str, id: &str) {
        if self.policies.contains_key(ns) || self.floor {
            self.remember_identity(ns, id.to_string());
        }
    }

    pub(crate) fn install_backend(&mut self, backend: Box<dyn DetectorBackend>) {
        self.backends.insert(backend.kind().to_string(), backend);
    }

    pub(crate) fn installed_backends(&self) -> Vec<(String, String)> {
        let mut v: Vec<(String, String)> =
            self.backends.values().map(|b| (b.kind().to_string(), b.id().to_string())).collect();
        v.sort();
        v
    }

    pub(crate) fn vault_key(&self) -> Option<[u8; 32]> {
        self.vault_key
    }

    pub(crate) fn set_vault_seed(&mut self, ns: &str, entries: Vec<(String, String)>) {
        self.vault_seed.insert(ns.to_string(), entries);
    }

    /// Namespaces whose policy persists mappings to the vault.
    pub(crate) fn vault_namespaces(&self) -> Vec<(String, AnonPolicy)> {
        let mut v: Vec<(String, AnonPolicy)> = self
            .policies
            .iter()
            .filter(|(_, p)| p.vault)
            .map(|(ns, p)| (ns.clone(), p.clone()))
            .collect();
        v.sort_by(|a, b| a.0.cmp(&b.0));
        v
    }

    /// Drain newly minted (ns, token, value) rows for vault-enabled
    /// namespaces — the write-behind queue the store flushes to meta.
    pub(crate) fn take_vault_pending(&mut self) -> Vec<(String, String, String)> {
        let mut out = Vec::new();
        for (ns, policy) in
            self.policies.iter().filter(|(_, p)| p.vault).map(|(n, p)| (n.clone(), p.clone()))
        {
            let _ = policy;
            if let Some(sess) = self.sessions.get_mut(&ns) {
                for (t, v) in sess.take_pending() {
                    out.push((ns.clone(), t, v));
                }
            }
            if let Some(sess) = self.ingress_sessions.get_mut(&ns) {
                for (t, v) in sess.take_pending() {
                    out.push((ns.clone(), t, v));
                }
            }
        }
        out
    }

    /// In-memory half of REQ-ANON-1: drop every live mapping entry (and
    /// pending row) whose value names an erased identity.
    pub(crate) fn scrub_identities(&mut self, names: &[String]) -> usize {
        let mut n = 0;
        for sess in self.sessions.values_mut() {
            n += sess.scrub_values(names);
        }
        for sess in self.ingress_sessions.values_mut() {
            n += sess.scrub_values(names);
        }
        if let Some((_, sess)) = self.last_context.as_mut() {
            n += sess.scrub_values(names);
        }
        for entries in self.vault_seed.values_mut() {
            entries.retain(|(_, v)| !names.iter().any(|i| i == v));
        }
        n
    }

    /// The declared ingress policy for `ns` (mode `ingress` or `both`).
    /// The floor never forces ingress — it is an egress-only cap.
    fn ingress_policy(&self, ns: &str) -> Result<Option<AnonPolicy>> {
        self.check_poisoned()?;
        Ok(self
            .policies
            .get(ns)
            .filter(|p| matches!(p.mode.as_str(), "ingress" | "both"))
            .cloned())
    }

    #[inline]
    pub(crate) fn ingress_idle(&self) -> bool {
        (self.policies.is_empty() && self.poisoned.is_none())
            || (self.poisoned.is_none()
                && !self
                    .policies
                    .values()
                    .any(|p| matches!(p.mode.as_str(), "ingress" | "both")))
    }

    fn ensure_ingress_session(&mut self, ns: &str, policy: &AnonPolicy) -> Result<()> {
        if !self.ingress_sessions.contains_key(ns) {
            let key = self.memory_key.ok_or_else(|| {
                AreevError::Validation(
                    "anonymization mode \"ingress\" needs an encrypted memory (the \
                     value-derived token key is HKDF-derived from the page key)"
                        .into(),
                )
            })?;
            // Ingress tokens are ALWAYS value-derived (D8), whatever the
            // egress scope says — build keyed regardless.
            let mut p = policy.clone();
            p.scope = "memory".into();
            let mut sess = SessionAnonymizer::new_keyed(p, key)?;
            if let Some(seed) = self.vault_seed.remove(ns) {
                sess.seed(seed);
            }
            self.ingress_sessions.insert(ns.to_string(), sess);
        }
        Ok(())
    }

    /// Whether an ingress policy covers `ns` (poison check included).
    pub(crate) fn ingress_active(&self, ns: &str) -> Result<bool> {
        if self.ingress_idle() {
            return Ok(false);
        }
        Ok(self.ingress_policy(ns)?.is_some())
    }

    /// Ingress text transform: `Ok(None)` when no ingress policy covers the
    /// namespace; `Ok(Some(text))` with the transformed text otherwise.
    pub(crate) fn ingress_text(&mut self, ns: &str, text: &str) -> Result<Option<String>> {
        if self.ingress_idle() {
            return Ok(None);
        }
        let Some(policy) = self.ingress_policy(ns)? else { return Ok(None) };
        let known = self.known.get(ns).cloned().unwrap_or_default();
        self.ensure_ingress_session(ns, &policy)?;
        let backends: Vec<&dyn DetectorBackend> =
            self.backends.values().map(|b| b.as_ref()).collect();
        let session = self.ingress_sessions.get_mut(ns).expect("ensured above");
        let (out, _) = session.transform_text_with(text, &known, &backends)?;
        Ok(Some(out))
    }

    /// Ingress structural-identity transform (a subject is a `person`).
    pub(crate) fn ingress_identity(&mut self, ns: &str, value: &str) -> Result<Option<String>> {
        if self.ingress_idle() {
            return Ok(None);
        }
        let Some(policy) = self.ingress_policy(ns)? else { return Ok(None) };
        self.remember_identity(ns, value.to_string());
        self.ensure_ingress_session(ns, &policy)?;
        let session = self.ingress_sessions.get_mut(ns).expect("ensured above");
        Ok(Some(session.transform_value("person", value)))
    }

    /// Recompute the stored pseudonym for an identity WITHOUT touching
    /// session state — pure derivation (REQ-ANON-7): `FORGET SUBJECT` and
    /// `REPORT SUBJECT` select under both the real name and this alias.
    /// `None` when no ingress policy covers `ns` or the person action is
    /// not `pseudonym` (redacted/masked identities have no stable alias).
    pub(crate) fn ingress_alias(&self, ns: &str, identity: &str) -> Result<Option<String>> {
        if self.ingress_idle() {
            return Ok(None);
        }
        let Some(policy) = self.ingress_policy(ns)? else { return Ok(None) };
        let action = policy.categories.get("person").cloned().unwrap_or_else(|| policy.default_action.clone());
        if action != "pseudonym" {
            return Ok(None);
        }
        let Some(key) = self.memory_key else { return Ok(None) };
        Ok(Some(areev_core::anon::derived_token(
            &policy.placeholder,
            &key,
            "person",
            identity,
        )))
    }

    /// Transform a list of bare strings (graph/history reads). `category`
    /// is `Some("person")` for lists that are identities by construction
    /// (`subjects_with_relation`, fork subjects); `None` runs detection.
    pub(crate) fn egress_strings(
        &mut self,
        ns: &str,
        category: Option<&str>,
        values: &mut [String],
    ) -> Result<()> {
        if self.is_idle() {
            return Ok(());
        }
        self.check_poisoned()?;
        let Some(policy) = self.effective(ns)? else { return Ok(()) };
        let known = self.known.get(ns).cloned().unwrap_or_default();
        let persistent = policy.scope == "session" || policy.scope == "memory";
        if policy.mode == "audit" {
            let backends: Vec<&dyn DetectorBackend> =
                self.backends.values().map(|b| b.as_ref()).collect();
            let mut counts: Vec<(String, String)> = Vec::new();
            for v in values.iter() {
                let outcome = areev_core::anon::scan_with(v, &policy, &known, &backends)?;
                for d in &outcome.detections {
                    counts.push((ns.to_string(), d.category.clone()));
                }
            }
            drop(backends);
            for k in counts {
                *self.audit_counts.entry(k).or_insert(0) += 1;
            }
            return Ok(());
        }
        let mut session = if persistent {
            match self.sessions.remove(ns) {
                Some(s) => s,
                None => self.make_session(policy.clone())?,
            }
        } else {
            SessionAnonymizer::new(policy.clone())?
        };
        if let Some(seed) = self.vault_seed.remove(ns) {
            session.seed(seed);
        }
        {
            let backends: Vec<&dyn DetectorBackend> =
                self.backends.values().map(|b| b.as_ref()).collect();
            for v in values.iter_mut() {
                *v = match category {
                    Some(cat) => session.transform_value(cat, v),
                    None => session.transform_text_with(v, &known, &backends)?.0,
                };
            }
        }
        if persistent {
            session.evict_to(MAX_SESSION_ENTRIES);
            self.sessions.insert(ns.to_string(), session);
        } else if !session.is_empty() {
            self.last_context = Some((ns.to_string(), session));
        }
        Ok(())
    }

}

/// Scan one grain in `audit` mode: counts per (ns, category), no transform,
/// never the spans' plaintext.
fn audit_grain(
    ns: &str,
    policy: &AnonPolicy,
    g: &DeserializedGrain,
    known: &[String],
    backends: &[&dyn DetectorBackend],
) -> Result<Vec<((String, String), u64)>> {
    let mut counts: BTreeMap<(String, String), u64> = BTreeMap::new();
    for (k, v) in &g.fields {
        if SKIP_FIELDS.contains(&k.as_str()) {
            continue;
        }
        for s in string_leaves(v) {
            let outcome = areev_core::anon::scan_with(s, policy, known, backends)?;
            for d in &outcome.detections {
                *counts.entry((ns.to_string(), d.category.clone())).or_insert(0) += 1;
            }
        }
    }
    Ok(counts.into_iter().collect())
}

fn resolve_ns(ns_hint: Option<&str>, g: &DeserializedGrain) -> String {
    ns_hint
        .map(str::to_string)
        .or_else(|| g.fields.get("namespace").and_then(|v| v.as_str()).map(String::from))
        .unwrap_or_else(|| "shared".to_string())
}

fn collect_identities(g: &DeserializedGrain) -> Vec<String> {
    IDENTITY_FIELDS
        .iter()
        .filter_map(|f| g.fields.get(*f).and_then(|v| v.as_str()).map(String::from))
        .collect()
}

fn string_leaves(v: &serde_json::Value) -> Vec<&str> {
    match v {
        serde_json::Value::String(s) => vec![s.as_str()],
        serde_json::Value::Array(a) => a.iter().flat_map(string_leaves).collect(),
        serde_json::Value::Object(o) => o.values().flat_map(string_leaves).collect(),
        _ => Vec::new(),
    }
}

/// Transform one grain's fields in place: identity fields whole-value as
/// `person`, every other string leaf through detection with the accumulated
/// known-identity list (which already includes this grain's own identities —
/// phase 1 of `egress_grains` fed them in).
fn transform_grain(
    session: &mut SessionAnonymizer,
    g: &mut DeserializedGrain,
    identities: &[String],
    backends: &[&dyn DetectorBackend],
) -> Result<()> {
    for f in IDENTITY_FIELDS {
        if let Some(serde_json::Value::String(s)) = g.fields.get_mut(*f) {
            *s = session.transform_value("person", &s.clone());
        }
    }
    let keys: Vec<String> = g
        .fields
        .keys()
        .filter(|k| !SKIP_FIELDS.contains(&k.as_str()) && !IDENTITY_FIELDS.contains(&k.as_str()))
        .cloned()
        .collect();
    for k in keys {
        let Some(v) = g.fields.get_mut(&k) else { continue };
        transform_value_tree(session, v, identities, backends)?;
    }
    Ok(())
}

fn transform_value_tree(
    session: &mut SessionAnonymizer,
    v: &mut serde_json::Value,
    identities: &[String],
    backends: &[&dyn DetectorBackend],
) -> Result<()> {
    match v {
        serde_json::Value::String(s) => {
            let (out, replaced) = session.transform_text_with(s, identities, backends)?;
            if replaced > 0 {
                *s = out;
            }
            Ok(())
        }
        serde_json::Value::Array(a) => {
            for item in a {
                transform_value_tree(session, item, identities, backends)?;
            }
            Ok(())
        }
        serde_json::Value::Object(o) => {
            for (k, item) in o.iter_mut() {
                if !SKIP_FIELDS.contains(&k.as_str()) {
                    transform_value_tree(session, item, identities, backends)?;
                }
            }
            Ok(())
        }
        _ => Ok(()),
    }
}
