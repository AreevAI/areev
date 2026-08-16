//! CalStoreFacade — constrained store interface for CAL query execution.
//!
//! The facade is a **trait** that limits what the executor can access. The
//! executor receives a `&dyn CalStoreFacade`, never a `&Areev` directly. This
//! provides a structural safety guarantee: destructive operations are gated by
//! executor config flags, not exposed unconditionally.
//!
//! # Security (S-2)
//!
//! ADD, SUPERSEDE, and ACCUMULATE are available on this trait and enabled
//! by default (`CalExecutorConfig::tier1_enabled = true`). They can be
//! disabled by setting `tier1_enabled = false`. REVERT remains **absent** —
//! its semantics are not yet defined.
//!
//! FORGET is available on this trait, gated by
//! `CalExecutorConfig::allow_destructive_ops` in the executor (**enabled by
//! default**; set it to `false` for a read-only session). `FORGET <hash>`
//! removes a single grain (a tombstone via `Areev::forget`). The
//! `cal_forget_user`/`cal_forget_scope` methods are unimplemented stubs —
//! there is no user/scope crypto-erasure primitive in the store yet.
//!
//! # Object safety
//!
//! The trait is designed to be object-safe — `&dyn CalStoreFacade` must
//! compile. All methods take `&self` and use concrete types only (no generics
//! or associated types).

use crate::store_types::{ForkGroupInfo, RecallParams, SearchHit};
use crate::store_types::VersionEntry;
use areev_core::error::{Hash, Result};
use areev_core::format::deserialize::DeserializedGrain;

/// Durable evidence for one sampled ASSEMBLE operation. The concrete Areev
/// facade stores it as an Observation in `agent:harness`; mock/foreign stores
/// may ignore it through the trait's default implementation.
#[derive(Debug, Clone, serde::Serialize)]
pub struct AssemblyManifest {
    /// SHA-256 over the canonical JSON of the ASSEMBLE statement, **not** the
    /// statement itself.
    ///
    /// An ASSEMBLE routinely names the subject it is assembling for
    /// (`… WHERE subject = "john"`). This manifest is an immutable Observation
    /// that replicates, and it lives in a reserved namespace the subject
    /// selector does not reach — so retaining the text would re-introduce an
    /// identity that `FORGET SUBJECT` cannot erase and `REPORT SUBJECT` cannot
    /// disclose. The digest proves which statement ran without keeping it.
    /// Same rule, same reason as the corpus manifest's `selector_sha256`.
    pub query_sha256: String,
    pub included_hashes: Vec<String>,
    pub rendered_sha256: String,
    pub budget: serde_json::Value,
    pub dropped_hashes: Vec<String>,
    pub sources: serde_json::Value,
}

// ---------------------------------------------------------------------------
// RerankType — which reranker to use for post-merge ASSEMBLE reranking
// ---------------------------------------------------------------------------

/// Reranker type selector for `CalStoreFacade::rerank_passages()`.
#[derive(Debug, Clone)]
pub enum RerankType {
    /// BERT cross-encoder reranking (feature: `rerank`).
    CrossEncoder,
    /// LLM listwise reranking via external backend (feature: `llm-rerank`).
    Llm,
}

// ---------------------------------------------------------------------------
// CalStoreFacade trait
// ---------------------------------------------------------------------------

/// Constrained store interface for CAL query execution.
///
/// Exposes read operations, Tier 1 write operations (ADD, SUPERSEDE), and
/// Tier 2 destructive operations (FORGET). Destructive operations are gated
/// by `CalExecutorConfig::allow_destructive_ops` — permitted by default, but
/// the operator can set it to `false` to make a session read-only.
pub trait CalStoreFacade: Send + Sync {
    /// Execute a recall query. Maps to `Areev::recall()`.
    fn recall(&self, params: &RecallParams) -> Result<Vec<SearchHit>>;

    /// Check if a grain exists by hash. Maps to `Areev::has()`.
    fn exists(&self, hash: &Hash) -> Result<bool>;

    /// Retrieve a single grain by hash. Maps to `Areev::get()`.
    fn get(&self, hash: &Hash) -> Result<DeserializedGrain>;

    /// Count total grains. Maps to `Areev::count()`.
    fn count(&self) -> Result<usize>;

    /// Get version history for a `(namespace, subject, relation)` triple.
    fn get_history(
        &self,
        namespace: &str,
        subject: &str,
        relation: &str,
    ) -> Result<Vec<VersionEntry>>;

    /// Return the default namespace for this session, if any.
    ///
    /// Set by the auth/capability token layer (capability-scoped namespace
    /// restricts which namespace a client may query). The executor consults
    /// this value when no explicit namespace is provided in the CAL query.
    fn default_namespace(&self) -> Option<&str>;

    /// Return the active user ID for this session, if any.
    ///
    /// Injected from the auth/capability token. When present, the executor
    /// uses it to populate `RecallParams::user_id` for all queries, ensuring
    /// a tenant cannot cross-query another tenant's data.
    fn active_user(&self) -> Option<&str>;

    // ── Tier 1 write operations (ADD, SUPERSEDE) ────────────────────────

    /// Add a grain from a type name and JSON fields. Maps to `Areev::add_from_json()`.
    fn cal_add(
        &self,
        grain_type: &str,
        fields: &serde_json::Map<String, serde_json::Value>,
    ) -> Result<Hash>;

    /// Add a grain with per-call options. Maps to `Areev::add_from_json_with_options()`.
    fn cal_add_with_options(
        &self,
        grain_type: &str,
        fields: &serde_json::Map<String, serde_json::Value>,
        options: crate::store_types::AddOptions,
    ) -> Result<crate::store_types::AddResult> {
        // Default: fall back to basic cal_add (ignoring options).
        let _ = options;
        self.cal_add(grain_type, fields)
            .map(crate::store_types::AddResult::plain)
    }

    /// Supersede a grain from a type name and JSON fields. Maps to `Areev::supersede_from_json()`.
    fn cal_supersede(
        &self,
        old_hash: &Hash,
        grain_type: &str,
        fields: &serde_json::Map<String, serde_json::Value>,
    ) -> Result<Hash>;

    /// Accumulate delta updates into the current tip of a grain's supersession chain.
    ///
    /// Resolves the target grain (by hash or entity_latest), applies ADD deltas
    /// (numeric addition) and SET replacements, creates a new grain, and supersedes
    /// the old one — all under the per-shard supersede lock.
    fn cal_accumulate(
        &self,
        grain_type: &str,
        target: &super::ast::AccumulateTarget,
        add_ops: &[(String, f64)],
        set_ops: &serde_json::Map<String, serde_json::Value>,
        reason: &str,
    ) -> Result<AccumulateResult> {
        let _ = (grain_type, target, add_ops, set_ops, reason);
        Err(areev_core::error::AreevError::Internal(
            "accumulate not available".into(),
        ))
    }

    // ── Phase 2 additions (default implementations) ──────────────────────

    /// Return CAL capabilities and conformance level information.
    fn describe_capabilities(&self) -> CalCapabilities {
        CalCapabilities::default()
    }

    /// Return information about all supported grain types.
    fn describe_grain_types(&self) -> Vec<GrainTypeInfo> {
        Vec::new()
    }

    /// Return field information, optionally scoped to a specific grain type.
    fn describe_fields(&self, _grain_type: Option<areev_core::types::GrainType>) -> Vec<FieldInfo> {
        Vec::new()
    }

    /// Record one assembly-budget sample for the `budget_pressure` analyzer
    /// (telemetry §8): did this ASSEMBLE have to drop grains to fit its token
    /// budget? Default no-op — only a telemetry-backed store overrides it.
    fn note_assembly_budget(&self, _overflow: bool) {}

    /// Persist one sampled assembly manifest. Best-effort by contract: an
    /// observability write must never turn a successful read into an error.
    fn note_assembly_manifest(&self, _manifest: &AssemblyManifest) {}

    // ── Template management (FR-003) ─────────────────────────────────────

    /// Register a custom template (persists to store).
    fn define_template(
        &self,
        _name: &str,
        _source: &str,
        _description: Option<&str>,
        _parent: Option<&str>,
        _grain_types: &[String],
    ) -> Result<()> {
        Err(areev_core::error::AreevError::Internal(
            "template management not available".into(),
        ))
    }

    /// Drop a custom template (removes from store).
    fn drop_template(&self, _name: &str) -> Result<()> {
        Err(areev_core::error::AreevError::Internal(
            "template management not available".into(),
        ))
    }

    // ── Fork inspection (CONTRADICTIONS) ─────────────────────────────────

    /// Every `(namespace, subject, relation)` that currently has more than one
    /// live head. Maps to `Areev::open_forks()`.
    ///
    /// Backs the `CONTRADICTIONS` recall clause. One indexed `GROUP BY … HAVING
    /// COUNT(*) > 1` over `heads` — cheap enough to run per query, which is why
    /// CONTRADICTIONS filters after recall rather than adding a per-hit probe to
    /// the microsecond hot path.
    ///
    /// Default implementation reports no forks, so a facade that does not track
    /// heads degrades to "nothing is contested" rather than erroring.
    fn open_forks(&self) -> Result<Vec<ForkGroupInfo>> {
        Ok(Vec::new())
    }

    // ── Reranking (post-merge ASSEMBLE) ──────────────────────────────────

    /// Rerank text passages against a query using the specified reranker.
    ///
    /// Returns a permutation vector: `result[i]` is the original index of the
    /// passage that should appear at position `i` in the reranked output.
    ///
    /// Default implementation returns identity permutation (no reranking).
    /// The `Areev` implementation delegates to the cross-encoder or LLM
    /// reranker registry, enforcing HIPAA/erasure/audit guards internally.
    fn rerank_passages(
        &self,
        _query: &str,
        _passages: &[&str],
        _rerank_type: RerankType,
        _model: Option<&str>,
        _user_id: Option<&str>,
    ) -> Result<Vec<usize>> {
        Ok((0.._passages.len()).collect())
    }

    // ── Tier 2 destructive operations (DELETE, FORGET) ────────────────────

    /// Delete a single grain by hash. Maps to `Areev::forget()`.
    fn cal_delete(&self, _hash: &Hash, _because: Option<&str>) -> Result<()> {
        Err(areev_core::error::AreevError::Internal(
            "destructive operations not available".into(),
        ))
    }

    /// Crypto-erase all data for a user. Maps to `Areev::forget_user()`.
    fn cal_forget_user(
        &self,
        _user_id: &str,
        _text_mentions: bool,
        _because: &str,
    ) -> Result<crate::store_types::ErasureProof> {
        Err(areev_core::error::AreevError::Internal(
            "destructive operations not available".into(),
        ))
    }

    /// Crypto-erase all data in a scope. Maps to `Areev::forget_scope()`.
    fn cal_forget_scope(&self, _scope: &str) -> Result<crate::store_types::ErasureProof> {
        Err(areev_core::error::AreevError::Internal(
            "destructive operations not available".into(),
        ))
    }

    /// `REPORT SUBJECT` — the read-only DSAR selection: everything
    /// `cal_forget_user` would erase, hydrated instead of erased. Maps to
    /// `Areev::subject_report_with()`. A read (gated by the `read` grant),
    /// not a destructive op.
    fn cal_subject_report(
        &self,
        _subject_id: &str,
        _text_mentions: bool,
    ) -> Result<crate::store_types::SubjectReportResult> {
        Err(areev_core::error::AreevError::Internal(
            "subject report not available on this facade".into(),
        ))
    }

    /// Concrete-type escape hatch for host layers that sit above this
    /// crate (the governance seam): a `GovernanceHost` receives the store
    /// as `&dyn CalStoreFacade` and needs the concrete facade back to
    /// build its substrate view. Default `None` — only the real facade
    /// answers.
    fn as_any(&self) -> Option<&dyn std::any::Any> {
        None
    }

    /// `MERGE` (CAL 1.3 Wave 3): close an open fork with a resolved
    /// value. Returns the merge grain's hash.
    fn cal_merge(
        &self,
        _subject: &str,
        _relation: &str,
        _object: &str,
        _confidence: f64,
        _because: &str,
    ) -> Result<Hash> {
        Err(areev_core::error::AreevError::Internal(
            "merge not available".into(),
        ))
    }

    /// `RELATED` (Wave 3): the bounded k-hop entity walk.
    fn cal_related(
        &self,
        _start: &str,
        _relations: &[&str],
        _direction: &str,
        _depth: usize,
        _limit: usize,
    ) -> Result<Vec<String>> {
        Err(areev_core::error::AreevError::Internal(
            "graph walks not available".into(),
        ))
    }

    /// `NOVELTY` (Wave 3): nearest existing grains to a candidate text.
    /// Rows are `(hash-hex, similarity)`.
    fn cal_novelty(
        &self,
        _text: &str,
        _subject: Option<&str>,
        _relation: Option<&str>,
        _k: usize,
    ) -> Result<Vec<(String, f64)>> {
        Err(areev_core::error::AreevError::Internal(
            "novelty checks not available".into(),
        ))
    }

    /// `ENTITY … AT` (CAL 1.3 Wave 2): the as-of read. `axis` is
    /// `"world"` or `"knowledge"`. Returns the grain as JSON, or None.
    fn cal_entity_at(
        &self,
        _subject: &str,
        _relation: &str,
        _at_ms: i64,
        _axis: &str,
    ) -> Result<Option<serde_json::Value>> {
        Err(areev_core::error::AreevError::Internal(
            "as-of reads not available".into(),
        ))
    }

    /// `RUN TRACE` (Wave 2): what a run recorded and what it produced —
    /// `{ recorded: [...], produced: [...] }`.
    fn cal_run_trace(&self, _run_id: &str, _limit: usize) -> Result<serde_json::Value> {
        Err(areev_core::error::AreevError::Internal(
            "run traces not available".into(),
        ))
    }

    /// `RUNS TOUCHING` (Wave 2): run ids that produced or refined a grain.
    fn cal_runs_touching(&self, _hash: &Hash, _depth: usize) -> Result<Vec<String>> {
        Err(areev_core::error::AreevError::Internal(
            "run joins not available".into(),
        ))
    }

    /// `DERIVED FROM` (Wave 2): reverse provenance, as grain JSON rows.
    fn cal_derived_from(&self, _hash: &Hash) -> Result<Vec<serde_json::Value>> {
        Err(areev_core::error::AreevError::Internal(
            "provenance reads not available".into(),
        ))
    }

    /// `DESCRIBE STATS` (Wave 2): store counters as JSON.
    fn cal_stats(&self) -> Result<serde_json::Value> {
        Err(areev_core::error::AreevError::Internal(
            "stats not available".into(),
        ))
    }

    /// `DESCRIBE INTEGRITY` (Wave 2): integrity + content-address recheck.
    fn cal_verify(&self) -> Result<serde_json::Value> {
        Err(areev_core::error::AreevError::Internal(
            "integrity checks not available".into(),
        ))
    }

    /// `REMEMBER` (CAL 1.3): capture free text as an Event grain with
    /// session/role/run metadata. Returns the Event's hash.
    fn cal_remember(
        &self,
        _content: &str,
        _session_id: Option<&str>,
        _role: Option<&str>,
        _run_id: Option<&str>,
    ) -> Result<Hash> {
        Err(areev_core::error::AreevError::Internal(
            "remember not available".into(),
        ))
    }

    /// Tier-3 DCL (CAL 1.3 §8.15): write a grant grain for `principal`.
    /// Verbs/namespaces are validated and canonicalized by the
    /// implementation; returns the grant grain's hash.
    fn cal_grant(
        &self,
        _principal: &str,
        _verbs: &[String],
        _namespaces: &[String],
        _because: Option<&str>,
    ) -> Result<Hash> {
        Err(areev_core::error::AreevError::Internal(
            "control operations not available".into(),
        ))
    }

    /// Tier-3 DCL: retraction-by-supersession of the covering grant
    /// grain(s). Returns how many grants were touched.
    fn cal_revoke(
        &self,
        _principal: &str,
        _verbs: &[String],
        _namespaces: &[String],
        _because: Option<&str>,
    ) -> Result<usize> {
        Err(areev_core::error::AreevError::Internal(
            "control operations not available".into(),
        ))
    }

    /// The live grant rows; `None` lists every principal.
    fn cal_show_grants(&self, _principal: Option<&str>) -> Result<Vec<GrantRow>> {
        Ok(Vec::new())
    }

    /// The retention sweep. Maps to `Areev::forget_older_than_capped()` —
    /// `batch_limit` is `LIMIT n` and MUST bound the erasure.
    fn cal_purge_stale(
        &self,
        _min_age_days: f64,
        _namespace: Option<&str>,
        _batch_limit: usize,
        _grain_type: Option<&str>,
        _because: &str,
    ) -> Result<usize> {
        Err(areev_core::error::AreevError::Internal(
            "destructive operations not available".into(),
        ))
    }

    /// List all templates (built-in + custom).
    fn list_templates(&self) -> Vec<TemplateInfo> {
        Vec::new()
    }

    /// Get a single template by name.
    fn get_template(&self, _name: &str) -> Option<TemplateInfo> {
        None
    }

    /// Record that a template was used in a FORMAT preset render.
    fn record_template_run(&self, _name: &str) {}

    // ── Saved query management ────────────────────────────────────────────

    /// Register a saved query (persists to store).
    fn define_query(
        &self,
        _name: &str,
        _body: &str,
        _description: Option<&str>,
        _params: &[crate::ast::QueryParam],
    ) -> Result<()> {
        Err(areev_core::error::AreevError::Internal(
            "saved query management not available".into(),
        ))
    }

    /// Drop a saved query (removes from store).
    fn drop_query(&self, _name: &str) -> Result<()> {
        Err(areev_core::error::AreevError::Internal(
            "saved query management not available".into(),
        ))
    }

    /// List all saved queries.
    fn list_queries(&self) -> Vec<crate::queries::QueryListEntry> {
        Vec::new()
    }

    /// Get a single saved query by name.
    fn get_query(&self, _name: &str) -> Option<crate::queries::QueryEntry> {
        None
    }

    /// Record `last_run_at` timestamp for a saved query after successful RUN execution.
    /// Default: no-op (for mock/test stores that don't persist).
    fn update_query_last_run(&self, _name: &str) -> Result<()> {
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// AccumulateResult
// ---------------------------------------------------------------------------

/// Result of an ACCUMULATE operation.
#[derive(Debug, Clone)]
pub struct AccumulateResult {
    /// Hash of the old (now-superseded) grain.
    pub old_hash: Hash,
    /// Hash of the new grain with deltas applied.
    pub new_hash: Hash,
    /// The delta operations that were applied (for audit): (field, old_value, new_value).
    pub applied_deltas: Vec<(String, f64, f64)>,
}

// ---------------------------------------------------------------------------
// Phase 2 facade types
// ---------------------------------------------------------------------------

/// Re-export for convenience.
pub use super::templates::TemplateListEntry as TemplateInfo;

/// One live grant row (`SHOW GRANTS` / `DESCRIBE PRINCIPAL`).
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct GrantRow {
    pub principal: String,
    /// The canonical grant object string (`read,write ON caller`).
    pub object: String,
    /// The grant grain's content address (hex).
    pub hash: String,
}

/// CAL engine capabilities and conformance level.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct CalCapabilities {
    /// CAL specification version (e.g. 1).
    pub cal_version: u8,
    /// Conformance level (1 = Core, 2 = Extended, 3 = Full).
    pub conformance_level: u8,
    /// List of supported statement types.
    pub supported_statements: Vec<String>,
    /// Maximum number of named sources in a multi-source ASSEMBLE.
    pub max_sources: u8,
    /// Maximum number of LET bindings in a single query.
    pub max_let_bindings: u8,
    /// Maximum token budget for ASSEMBLE BUDGET clause.
    pub max_budget_tokens: u32,
}

impl Default for CalCapabilities {
    fn default() -> Self {
        Self {
            cal_version: 1,
            conformance_level: 2,
            supported_statements: vec![
                "RECALL".into(),
                "EXISTS".into(),
                "ASSEMBLE".into(),
                "HISTORY".into(),
                "EXPLAIN".into(),
                "DESCRIBE".into(),
                "BATCH".into(),
                "COALESCE".into(),
                "ADD".into(),
                "SUPERSEDE".into(),
                "ACCUMULATE".into(),
                "REVERT".into(),
                "FORGET".into(),
                "PURGE".into(),
                // The read-only DSAR selection (OMS 1.6 draft).
                "REPORT".into(),
                "DROP".into(),
                // Saved queries and custom templates persist in the file's
                // host metadata, so the engine can honestly claim them.
                "DEFINE".into(),
                // Also RUN LOOP and RUN TRACE — one leading keyword each,
                // as everywhere else in this list.
                "RUN".into(),
                // CAL 1.3 Tier-3 DCL. SHOW covers SHOW GRANTS and SHOW FORKS.
                "GRANT".into(),
                "REVOKE".into(),
                "SHOW".into(),
                // CAL 1.3 evolution: free-text extraction and tip merge.
                "REMEMBER".into(),
                "MERGE".into(),
                // CAL 1.3 Wave-2 reads: as-of, the run↔memory join, reverse
                // provenance, graph neighbourhood, novelty.
                "ENTITY".into(),
                "RUNS".into(),
                "DERIVED".into(),
                "RELATED".into(),
                "NOVELTY".into(),
                // CAL 1.3 governance lifecycle (§8.16).
                "APPROVE".into(),
                "REJECT".into(),
                "APPLY".into(),
                "ROLLBACK".into(),
            ],
            max_sources: 8,
            max_let_bindings: 5,
            max_budget_tokens: 100_000,
        }
    }
}

/// Metadata about a grain type (for DESCRIBE SCHEMA / DESCRIBE grain_types).
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct GrainTypeInfo {
    /// Singular name (e.g. "fact").
    pub name: String,
    /// Plural name (e.g. "facts").
    pub plural: String,
    /// Type-specific fields beyond the common set.
    pub specific_fields: Vec<String>,
}

/// Metadata about a queryable field (for DESCRIBE FIELDS).
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct FieldInfo {
    /// Field name (e.g. "subject", "confidence").
    pub name: String,
    /// Field type description (e.g. "string", "number", "timestamp").
    pub field_type: String,
    /// Whether this field can appear in WHERE clauses.
    pub filterable: bool,
    /// Whether this field can appear in ORDER BY clauses.
    pub sortable: bool,
}

// ---------------------------------------------------------------------------
// Blanket implementation for Areev
// ---------------------------------------------------------------------------

// Blanket implementation of `CalStoreFacade` for the `Areev` engine.
//
// Wraps each Areev method directly. The `default_namespace` and
// `active_user` helpers return values from the engine's own session state.

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use areev_core::error::AreevError;

    // -----------------------------------------------------------------------
    // Minimal mock implementation of CalStoreFacade
    // -----------------------------------------------------------------------

    /// A mock store that holds an in-memory list of (hash, grain) pairs.
    struct MockStore {
        grains: Vec<(Hash, DeserializedGrain)>,
        default_ns: Option<String>,
        active_user: Option<String>,
    }

    impl MockStore {
        fn empty() -> Self {
            Self {
                grains: Vec::new(),
                default_ns: None,
                active_user: None,
            }
        }

        fn with_namespace(ns: &str) -> Self {
            Self {
                grains: Vec::new(),
                default_ns: Some(ns.to_string()),
                active_user: None,
            }
        }

        fn with_user(user: &str) -> Self {
            Self {
                grains: Vec::new(),
                default_ns: None,
                active_user: Some(user.to_string()),
            }
        }
    }

    fn make_grain(subject: &str) -> DeserializedGrain {
        use areev_core::format::header::MgHeader;
        use areev_core::types::GrainType;
        use std::collections::HashMap;

        let mut fields = HashMap::new();
        fields.insert(
            "subject".to_string(),
            serde_json::Value::String(subject.to_string()),
        );
        fields.insert(
            "grain_type".to_string(),
            serde_json::Value::String("fact".to_string()),
        );

        // Construct a minimal hash from the subject bytes.
        let mut hash_bytes = [0u8; 32];
        for (i, b) in subject.as_bytes().iter().enumerate().take(32) {
            hash_bytes[i] = *b;
        }
        let hash = Hash::from_bytes(&hash_bytes);

        DeserializedGrain {
            header: MgHeader {
                version: 1,
                flags: 0,
                grain_type: 0x01, // Fact
                ns_hash: 0,
                created_at_sec: 0,
            },
            grain_type: GrainType::Fact,
            fields,
            hash,
        }
    }

    impl CalStoreFacade for MockStore {
        fn recall(&self, params: &RecallParams) -> Result<Vec<SearchHit>> {
            let mut hits: Vec<SearchHit> = self
                .grains
                .iter()
                .filter(|(_, g)| {
                    if let Some(ref s) = params.subject {
                        if g.get_str("subject") != Some(s.as_str()) {
                            return false;
                        }
                    }
                    true
                })
                .map(|(hash, grain)| SearchHit {
                    grain: grain.clone(),
                    score: 1.0,
                    hash: *hash,
                    score_breakdown: None,
                    explanation: None,
                    scope_depth: None,
                    source_namespace: None,
                    #[cfg(feature = "rerank")]
                    rerank_score: None,
                    #[cfg(feature = "llm-rerank")]
                    llm_rerank_score: None,
                    relative_time: None,
                    conflict_status: None,
                    supersession_status: None,
                    superseded_by_hash: None,
                    recall_source: None,
                })
                .collect();
            if let Some(limit) = params.limit {
                hits.truncate(limit);
            }
            Ok(hits)
        }

        fn exists(&self, hash: &Hash) -> Result<bool> {
            Ok(self.grains.iter().any(|(h, _)| h == hash))
        }

        fn get(&self, hash: &Hash) -> Result<DeserializedGrain> {
            self.grains
                .iter()
                .find(|(h, _)| h == hash)
                .map(|(_, g)| g.clone())
                .ok_or(AreevError::NotFound(*hash))
        }

        fn count(&self) -> Result<usize> {
            Ok(self.grains.len())
        }

        fn get_history(
            &self,
            _namespace: &str,
            _subject: &str,
            _relation: &str,
        ) -> Result<Vec<VersionEntry>> {
            Ok(Vec::new())
        }

        fn cal_add(
            &self,
            _grain_type: &str,
            _fields: &serde_json::Map<String, serde_json::Value>,
        ) -> Result<Hash> {
            Err(AreevError::Validation(
                "mock: cal_add not implemented".into(),
            ))
        }

        fn cal_supersede(
            &self,
            _old_hash: &Hash,
            _grain_type: &str,
            _fields: &serde_json::Map<String, serde_json::Value>,
        ) -> Result<Hash> {
            Err(AreevError::Validation(
                "mock: cal_supersede not implemented".into(),
            ))
        }

        fn default_namespace(&self) -> Option<&str> {
            self.default_ns.as_deref()
        }

        fn active_user(&self) -> Option<&str> {
            self.active_user.as_deref()
        }

        fn list_templates(&self) -> Vec<TemplateInfo> {
            // Return built-in templates from a default registry so DESCRIBE TEMPLATES works.
            let registry = crate::templates::TemplateRegistry::new();
            registry.list()
        }
    }

    // -----------------------------------------------------------------------
    // Object-safety test: &dyn CalStoreFacade must compile
    // -----------------------------------------------------------------------

    #[test]
    fn test_trait_is_object_safe() {
        let store = MockStore::empty();
        // This assignment verifies that CalStoreFacade is object-safe.
        let _facade: &dyn CalStoreFacade = &store;
    }

    #[test]
    fn test_mock_count_empty() {
        let store = MockStore::empty();
        let facade: &dyn CalStoreFacade = &store;
        assert_eq!(facade.count().unwrap(), 0);
    }

    #[test]
    fn test_mock_exists_not_found() {
        let store = MockStore::empty();
        let facade: &dyn CalStoreFacade = &store;
        let hash = Hash::from_bytes(&[0u8; 32]);
        assert!(!facade.exists(&hash).unwrap());
    }

    #[test]
    fn test_mock_get_not_found() {
        let store = MockStore::empty();
        let facade: &dyn CalStoreFacade = &store;
        let hash = Hash::from_bytes(&[0u8; 32]);
        let result = facade.get(&hash);
        assert!(result.is_err());
        match result.unwrap_err() {
            AreevError::NotFound(_) => {}
            other => panic!("expected NotFound, got {:?}", other),
        }
    }

    #[test]
    fn test_mock_recall_empty_store() {
        let store = MockStore::empty();
        let facade: &dyn CalStoreFacade = &store;
        let params = RecallParams::default();
        let hits = facade.recall(&params).unwrap();
        assert!(hits.is_empty());
    }

    #[test]
    fn test_mock_default_namespace_none() {
        let store = MockStore::empty();
        let facade: &dyn CalStoreFacade = &store;
        assert_eq!(facade.default_namespace(), None);
    }

    #[test]
    fn test_mock_default_namespace_some() {
        let store = MockStore::with_namespace("acme");
        let facade: &dyn CalStoreFacade = &store;
        assert_eq!(facade.default_namespace(), Some("acme"));
    }

    #[test]
    fn test_mock_active_user_none() {
        let store = MockStore::empty();
        let facade: &dyn CalStoreFacade = &store;
        assert_eq!(facade.active_user(), None);
    }

    #[test]
    fn test_mock_active_user_some() {
        let store = MockStore::with_user("john");
        let facade: &dyn CalStoreFacade = &store;
        assert_eq!(facade.active_user(), Some("john"));
    }

    #[test]
    fn test_mock_get_history_empty() {
        let store = MockStore::empty();
        let facade: &dyn CalStoreFacade = &store;
        let history = facade.get_history("ns", "john", "likes").unwrap();
        assert!(history.is_empty());
    }

    #[test]
    fn test_facade_can_be_boxed() {
        // Ensures the trait is usable as Box<dyn CalStoreFacade> (Send + Sync).
        let store = MockStore::empty();
        let _boxed: Box<dyn CalStoreFacade> = Box::new(store);
    }

    #[test]
    fn test_facade_recall_with_subject_filter() {
        let grain = make_grain("john");
        let hash = grain.hash;
        let mut store = MockStore::empty();
        store.grains.push((hash, grain));

        let facade: &dyn CalStoreFacade = &store;
        let params = RecallParams {
            subject: Some("john".to_string()),
            ..Default::default()
        };
        let hits = facade.recall(&params).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].hash, hash);
    }

    #[test]
    fn test_facade_recall_subject_filter_no_match() {
        let grain = make_grain("john");
        let hash = grain.hash;
        let mut store = MockStore::empty();
        store.grains.push((hash, grain));

        let facade: &dyn CalStoreFacade = &store;
        let params = RecallParams {
            subject: Some("bob".to_string()),
            ..Default::default()
        };
        let hits = facade.recall(&params).unwrap();
        assert!(hits.is_empty());
    }

    #[test]
    fn test_facade_exists_known_hash() {
        let grain = make_grain("john");
        let hash = grain.hash;
        let mut store = MockStore::empty();
        store.grains.push((hash, grain));

        let facade: &dyn CalStoreFacade = &store;
        assert!(facade.exists(&hash).unwrap());
    }

    #[test]
    fn test_facade_get_known_hash() {
        let grain = make_grain("john");
        let hash = grain.hash;
        let mut store = MockStore::empty();
        store.grains.push((hash, grain));

        let facade: &dyn CalStoreFacade = &store;
        let retrieved = facade.get(&hash).unwrap();
        assert_eq!(retrieved.get_str("subject"), Some("john"));
    }

    // -----------------------------------------------------------------------
    // Phase 2 facade method tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_facade_describe_capabilities_default() {
        let store = MockStore::empty();
        let facade: &dyn CalStoreFacade = &store;
        let caps = facade.describe_capabilities();
        assert_eq!(caps.cal_version, 1);
        assert_eq!(caps.conformance_level, 2);
        assert!(!caps.supported_statements.is_empty());
        assert!(caps.supported_statements.contains(&"RECALL".to_string()));
        assert!(caps.supported_statements.contains(&"ASSEMBLE".to_string()));
        assert!(caps.supported_statements.contains(&"COALESCE".to_string()));
        assert_eq!(caps.max_sources, 8);
        assert_eq!(caps.max_let_bindings, 5);
        assert_eq!(caps.max_budget_tokens, 100_000);
    }

    #[test]
    fn test_facade_describe_grain_types_default_is_empty() {
        // Default trait impl returns empty — real implementations override.
        let store = MockStore::empty();
        let facade: &dyn CalStoreFacade = &store;
        let types = facade.describe_grain_types();
        assert!(types.is_empty(), "default impl should return empty");
    }

    #[test]
    fn test_facade_describe_fields_default_is_empty() {
        // Default trait impl returns empty — real implementations override.
        let store = MockStore::empty();
        let facade: &dyn CalStoreFacade = &store;
        let fields = facade.describe_fields(None);
        assert!(fields.is_empty(), "default impl should return empty");
    }

    #[test]
    fn test_cal_capabilities_default_values() {
        let caps = CalCapabilities::default();
        assert_eq!(caps.cal_version, 1);
        assert_eq!(caps.conformance_level, 2);
        assert_eq!(caps.supported_statements.len(), 32);
        // Every statement a client can reach must be listed: an agent that
        // runs `DESCRIBE CAPABILITIES` as its handshake will never emit a
        // statement missing from here, so an omission hides a whole feature.
        // Assert against the parser's own dispatch rather than a hand count.
        for kw in [
            "GRANT", "REVOKE", "SHOW", "REMEMBER", "MERGE", "ENTITY", "RUNS", "DERIVED",
            "RELATED", "NOVELTY", "APPROVE", "REJECT", "APPLY", "ROLLBACK",
        ] {
            assert!(
                caps.supported_statements.contains(&kw.to_string()),
                "CAL 1.3 statement {kw} parses but is not advertised"
            );
        }
        // The DSAR read joined in the GDPR compliance pack (OMS 1.6 draft).
        assert!(caps.supported_statements.contains(&"REPORT".to_string()));
        assert!(caps.supported_statements.contains(&"RECALL".to_string()));
        // Saved queries and templates persist in the file's host metadata.
        assert!(caps.supported_statements.contains(&"DEFINE".to_string()));
        assert!(caps.supported_statements.contains(&"RUN".to_string()));
        assert!(caps.supported_statements.contains(&"EXISTS".to_string()));
        assert!(caps.supported_statements.contains(&"ASSEMBLE".to_string()));
        assert!(caps.supported_statements.contains(&"HISTORY".to_string()));
        assert!(caps.supported_statements.contains(&"EXPLAIN".to_string()));
        assert!(caps.supported_statements.contains(&"DESCRIBE".to_string()));
        assert!(caps.supported_statements.contains(&"BATCH".to_string()));
        assert!(caps.supported_statements.contains(&"COALESCE".to_string()));
        assert!(caps.supported_statements.contains(&"ADD".to_string()));
        assert!(caps.supported_statements.contains(&"SUPERSEDE".to_string()));
        assert!(caps.supported_statements.contains(&"REVERT".to_string()));
        assert!(caps.supported_statements.contains(&"FORGET".to_string()));
        assert!(caps.supported_statements.contains(&"PURGE".to_string()));
        assert!(caps.supported_statements.contains(&"DROP".to_string()));
        assert_eq!(caps.max_sources, 8);
        assert_eq!(caps.max_let_bindings, 5);
        assert_eq!(caps.max_budget_tokens, 100_000);
    }

    #[test]
    fn test_grain_type_info_struct() {
        let info = GrainTypeInfo {
            name: "fact".to_string(),
            plural: "facts".to_string(),
            specific_fields: vec!["subject".to_string(), "relation".to_string()],
        };
        assert_eq!(info.name, "fact");
        assert_eq!(info.plural, "facts");
        assert_eq!(info.specific_fields.len(), 2);
    }

    #[test]
    fn test_field_info_struct() {
        let info = FieldInfo {
            name: "subject".to_string(),
            field_type: "string".to_string(),
            filterable: true,
            sortable: true,
        };
        assert_eq!(info.name, "subject");
        assert!(info.filterable);
        assert!(info.sortable);
    }
}
