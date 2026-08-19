//! CAL executor — runs parsed CAL queries against a `CalStoreFacade`.
//!
//! The executor takes a parsed [`CalQuery`] (or a raw CAL string, which it
//! parses internally) and executes it against a `&dyn CalStoreFacade`,
//! returning a [`CalExecResult`].
//!
//! # Security conditions
//!
//! - **S-2**: Tier 1 (evolve) statements — `Add`, `Supersede`, `Accumulate` —
//!   execute by default (`CalExecutorConfig::tier1_enabled = true`). They can be
//!   disabled by setting `tier1_enabled = false`. `Revert` always returns
//!   `Unsupported` regardless of this flag (semantics not yet defined).
//!   All Tier 1 writes go through the same `PolicyEngine::check_write()` and
//!   audit trail as the HTTP/gRPC write path, so compliance invariants are preserved.
//! - **S-5**: Audit MUST NOT log parameter values, only names.
//!
//! # Compliance conditions
//!
//! - **C-1**: The `CalExecResult::query_hash` field is always populated with
//!   a SHA-256 of the normalized CAL string (for audit trail).
//! - **C-4**: `query_hash` is the SHA-256 of the normalized (trimmed) query.

use std::cmp::Ordering;
use std::collections::{BTreeMap, HashMap};

use sha2::{Digest, Sha256};

use super::ast::{
    AddWithOption, AssembleStmt, BatchEntry, BatchStmt, CalQuery, CalStatement, CoalesceStmt,
    Comparator, Condition, DescribeStmt, DescribeTarget, ExistsStmt, ExplainStmt, Extractor,
    FormatClause, GrainTypePlural, HistoryStmt, LetBinding, PipelineStage, RecallStmt, SetOp,
    SetOpStmt, Source, Value, WithOption,
};
use super::errors::{CalError, Span};
use super::facade::{AssemblyManifest, CalStoreFacade};
use crate::store_types::{AddOptions, DiversityConfig, RecallParams};
use areev_core::error::{AreevError, Hash};

// ---------------------------------------------------------------------------
// CalExecutorConfig
// ---------------------------------------------------------------------------

/// Configuration for the CAL executor.
#[derive(Debug, Clone)]
pub struct CalExecutorConfig {
    /// Maximum LIMIT value allowed. Queries specifying higher limits are clamped.
    /// Default: 1000.
    pub max_limit: u64,
    /// Default LIMIT applied when the query doesn't specify one.
    /// Default: 50.
    pub default_limit: u64,
    /// Whether Tier 1 (evolve) statements are enabled.
    ///
    /// When `true` (default), ADD, SUPERSEDE, and ACCUMULATE execute through
    /// the store facade. All writes go through `PolicyEngine::check_write()`
    /// and emit audit events, preserving compliance invariants.
    /// When `false`, they return `Unsupported`. REVERT always returns
    /// `Unsupported` regardless of this flag (semantics undefined).
    ///
    /// Under the grants model (`docs/cal-all-you-need-proposal.md`) this is
    /// a **process-wide restrictive cap**, not the authorization mechanism:
    /// the session's grants decide who may write, and this flag set `false`
    /// still wins over any grant — belt-and-suspenders for "serve untrusted
    /// callers read-only".
    pub tier1_enabled: bool,
    /// Namespace override injected from the auth/capability token.
    ///
    /// When set, overrides any `namespace` condition in the CAL query's
    /// WHERE clause, preventing a client from querying outside its scope.
    pub namespace_override: Option<String>,
    /// User ID override injected from the auth/capability token.
    ///
    /// When set, overrides any `user_id` condition in the CAL query's
    /// WHERE clause.
    pub user_id_override: Option<String>,
    /// Whether destructive operations (FORGET, DROP, PURGE) are permitted.
    ///
    /// When `true` (**the default**), FORGET/DROP/PURGE execute through the
    /// store facade. When `false`, they return `Unsupported`. This is
    /// per-process host config (invariant #5) — never persisted in the file.
    /// Set it to `false` to make a session read-only, e.g.
    /// `areev serve --mcp --no-destructive-ops`.
    ///
    /// Under the grants model this is a **process-wide restrictive cap**:
    /// the session's grants (`AreevFacade::authz`) decide who may destroy,
    /// and this flag set `false` still wins over any grant.
    pub allow_destructive_ops: bool,
    /// When `true`, ASSEMBLE results strip internal budget metadata
    /// (tokens_allocated, tokens_used) from the `sources` array.
    /// Default: `false`.  (S-09)
    pub redact_budget_metadata: bool,
    /// Fraction of ASSEMBLE operations whose provenance manifest is stored in
    /// `agent:harness`. Host-only and never persisted; 0.0 (default) is off.
    pub assembly_manifest_sample_rate: f64,
    /// Caller's JWT scopes for identity-based access control.
    /// When non-empty, the executor checks the required scope for each statement
    /// type before execution. Empty = no enforcement (CLI, tests).
    pub caller_scopes: Vec<String>,
}

impl Default for CalExecutorConfig {
    fn default() -> Self {
        Self {
            max_limit: 1000,
            default_limit: 50,
            tier1_enabled: true,
            allow_destructive_ops: true,
            namespace_override: None,
            user_id_override: None,
            redact_budget_metadata: false,
            assembly_manifest_sample_rate: 0.0,
            caller_scopes: vec![],
        }
    }
}

// ---------------------------------------------------------------------------
// CalExecResult and associated types
// ---------------------------------------------------------------------------

/// Result of executing a CAL query.
///
/// Named `CalExecResult` (not `CalResult`) to avoid shadowing the
/// `CalResult<T>` type alias in `errors.rs`.
#[derive(Debug, serde::Serialize)]
pub struct CalExecResult {
    /// The query string that was executed (as supplied, not normalized).
    pub query: String,
    /// SHA-256 hash of the trimmed query string (C-4 audit requirement).
    pub query_hash: String,
    /// The result payload.
    pub result: CalResultPayload,
    /// Non-fatal warnings emitted during parsing or execution.
    pub warnings: Vec<String>,
    /// Anonymization egress report (proposal §4.1): present when the store
    /// has an egress policy active, carrying mode/floor and mapping *ids*
    /// only — the mapping itself never rides a payload (D5 custody).
    pub anonymized: Option<serde_json::Value>,
    /// Execution metadata.
    pub metadata: CalMetadata,
}

impl CalExecResult {
    /// The wire payload every embedding surface returns: `result` with the
    /// non-fatal `warnings` folded in under a `warnings` key.
    ///
    /// Warnings used to be a field only the caller could reach, and the three
    /// surfaces that return JSON to a user — Python, Node, and the MCP
    /// `areev_cal` tool — all serialized `result` alone. So every `CAL-Wnnn`
    /// the executor raised was dropped on the way out, and an option that
    /// parsed but could not change the result (`score_breakdown` on `RECALL`,
    /// `CAL-W014`) read as a silent no-op — the exact failure the warning
    /// exists to make visible. `warnings` is omitted when empty, so a clean
    /// query keeps the payload it always had.
    /// Errors exactly where serializing `result` alone would have, so folding
    /// the warnings in never converts a failure into a plausible-looking
    /// payload.
    pub fn payload_json(&self) -> serde_json::Result<serde_json::Value> {
        let mut v = serde_json::to_value(&self.result)?;
        if let Some(a) = &self.anonymized {
            if let Some(obj) = v.as_object_mut() {
                obj.insert("anonymized".into(), a.clone());
            }
        }
        if !self.warnings.is_empty() {
            // `CalResultPayload` is internally tagged, so it always serializes
            // to an object — but degrade to the bare payload rather than panic
            // if that ever stops being true.
            if let Some(obj) = v.as_object_mut() {
                obj.insert("warnings".into(), serde_json::json!(self.warnings));
            }
        }
        Ok(v)
    }
}

/// Metadata about a CAL query execution.
#[derive(Debug, serde::Serialize)]
pub struct CalMetadata {
    /// CAL version declared in the query (e.g. `1` for `CAL/1`).
    pub version: u32,
    /// Statement type name (e.g. `"recall"`, `"exists"`).
    pub statement_type: String,
    /// Wall-clock execution time in milliseconds.
    pub execution_time_ms: u64,
    /// Number of top-level results in the payload.
    pub result_count: usize,
}

/// The result payload, discriminated by statement type.
#[derive(Debug, serde::Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CalResultPayload {
    /// Result of `RECALL`, `SET`, or `ASSEMBLE` operations.
    Grains {
        grains: Vec<CalGrainResult>,
        /// Total results available before pipeline LIMIT/OFFSET (best-effort).
        total_available: Option<usize>,
    },
    /// Result of `EXISTS`.
    Exists { exists: bool, hash: String },
    /// Result of a `| COUNT` pipeline stage.
    Count { count: usize },
    /// Result of `GRANT` (CAL 1.3 §8.15).
    Granted { principal: String, object: String, hash: String },
    /// Result of `REVOKE`.
    Revoked { principal: String, grants_touched: usize },
    /// Result of `SHOW GRANTS` — one row per live grant grain.
    GrantList { grants: Vec<serde_json::Value> },
    /// Result of `RUN LOOP` (CAL 1.3 §8.16) — the engine's run report.
    LoopRan { run: serde_json::Value },
    /// Result of `APPROVE`/`REJECT`.
    Reviewed { hash: String, decision: String },
    /// Result of `APPLY`.
    RecApplied { hash: String, rollbackable: bool },
    /// Result of `ROLLBACK`.
    RecRolledBack { hash: String },
    /// Result of `REMEMBER` — the captured Event's hash.
    Remembered { hash: String },
    /// Result of `ENTITY … AT` — the as-of grain, or null.
    EntityAt { grain: Option<serde_json::Value>, axis: String, at_ms: i64 },
    /// Result of `RUN TRACE` — what a run recorded and produced.
    RunTrace { run_id: String, trace: serde_json::Value },
    /// Result of `RUNS TOUCHING` — run ids, most recent first.
    RunsTouching { hash: String, runs: Vec<String> },
    /// Result of `DERIVED FROM` — reverse provenance rows.
    DerivedFrom { hash: String, grains: Vec<serde_json::Value> },
    /// Result of `SHOW FORKS` — the open forks.
    Forks { forks: Vec<serde_json::Value> },
    /// Result of `MERGE` — the resolved head.
    Merged { hash: String, subject: String, relation: String, object: String },
    /// Result of `RELATED` — the reachable entities.
    RelatedEntities { start: String, entities: Vec<String> },
    /// Result of `NOVELTY` — nearest matches, most similar first.
    NoveltyMatches { matches: Vec<serde_json::Value> },
    /// Result of `HISTORY OF`.
    History { versions: Vec<CalVersionResult> },
    /// Result of `DESCRIBE`.
    Describe { info: serde_json::Value },
    /// Result of `EXPLAIN`.
    Explain { plan: CalQueryPlan },
    /// Result of `BATCH`.
    Batch {
        results: HashMap<String, CalResultPayload>,
    },
    /// Result of a multi-source `ASSEMBLE` (Phase 2).
    Assembled {
        /// Assembled grains after budget and dedup processing.
        grains: Vec<CalGrainResult>,
        /// Per-source metadata.
        sources: Vec<super::assemble::SourceMeta>,
        /// Total tokens used across all sources.
        total_tokens: u32,
        /// Budget limit (if specified).
        budget_limit: Option<u32>,
        /// Always false (progressive_disclosure has been removed).
        progressive: bool,
        /// Total grains available before trimming.
        total_available: Option<usize>,
    },
    /// Result of `HISTORY <hash> DIFF <hash>` — field-level differences.
    Diff {
        /// Content-address hash of the source grain.
        source_hash: String,
        /// Content-address hash of the target grain.
        target_hash: String,
        /// Field-level differences between source and target.
        changes: Vec<super::ast::FieldDiff>,
    },
    /// Result of single-format rendering (WI-1.1). Contains the rendered text
    /// and format name. Used when ASSEMBLE or RECALL has a single FORMAT clause.
    Formatted {
        /// Rendered text output.
        text: String,
        /// Format name (e.g. "json", "markdown", "sml").
        format: String,
        /// Number of grains that were formatted.
        grain_count: usize,
        /// Raw grains for A2UI surface building (not serialized on the wire).
        #[serde(skip_serializing)]
        grains: Vec<CalGrainResult>,
    },
    /// Result of multi-format rendering (CAL spec v1.0.1, Section 14.2.1).
    /// Contains multiple renderings keyed by format name.
    MultiFormatted {
        /// Renderings keyed by format name (e.g. "markdown" -> "...", "json" -> "...").
        formats: HashMap<String, String>,
        /// Number of grains that were formatted.
        grain_count: usize,
        /// Raw grains for A2UI surface building (not serialized on the wire).
        #[serde(skip_serializing)]
        grains: Vec<CalGrainResult>,
    },
    /// Result of `ADD` (Tier 1).
    Added {
        hash: String,
        grain_type: String,
        /// Number of facts extracted (when `WITH extract_memories` was used).
        #[serde(skip_serializing_if = "Option::is_none")]
        extracted_count: Option<usize>,
        /// Warnings from the extraction pipeline.
        #[serde(skip_serializing_if = "Vec::is_empty")]
        extraction_warnings: Vec<String>,
    },
    /// Result of `SUPERSEDE` (Tier 1).
    Superseded { old_hash: String, new_hash: String },
    /// Result of `ACCUMULATE` (Tier 1).
    Accumulated {
        old_hash: String,
        new_hash: String,
        deltas: Vec<AccumulatedDelta>,
    },
    /// Result of `DEFINE TEMPLATE` (FR-003).
    TemplateDefined { name: String },
    /// Result of `DROP TEMPLATE` (FR-003).
    TemplateDropped { name: String },
    /// Result of `DEFINE QUERY`.
    QueryDefined { name: String },
    /// Result of `DROP QUERY`.
    QueryDropped { name: String },
    /// Returned for `STREAM ASSEMBLE` — signals the HTTP handler to use SSE (FR-004).
    StreamAssemble {
        /// The parsed assemble statement (to be executed by the streaming handler).
        assemble: Box<super::ast::AssembleStmt>,
        /// Query-level WITH options to apply post-merge (rerank, dedup, etc.).
        with_options: Vec<super::ast::WithOption>,
    },
    /// Result of `FORGET <hash>`, `FORGET USER`, or `FORGET SCOPE` (Tier 2).
    Forgotten { target: String, count: u64 },
    /// Result of `PURGE STALE` (Tier 2).
    Purged { count: usize },
    /// Result of `REPORT SUBJECT` — the read-only DSAR selection: the
    /// matched identity strings and grains (`{hash, type, fields}`).
    SubjectReport {
        subject: String,
        identity_names: Vec<String>,
        grains: Vec<serde_json::Value>,
    },
    /// Returned for Tier 1/2 statements (S-2) or genuinely unsupported paths.
    Unsupported { statement: String, message: String },
}

/// A single grain in a CAL result set (projected view).
#[derive(Debug, Clone, serde::Serialize)]
pub struct CalGrainResult {
    /// Content-address hash of the grain (hex string).
    pub hash: String,
    /// Canonical grain type name (e.g. `"fact"`, `"event"`).
    pub grain_type: String,
    /// Final relevance score (RRF-fused).
    pub score: f64,
    /// All grain fields as a JSON object.
    pub fields: serde_json::Value,
    /// Per-component score breakdown. Present when `WITH score_breakdown`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub score_breakdown: Option<serde_json::Value>,
    /// Human-readable ranking explanation. Present when `WITH explanation`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub explanation: Option<String>,
    /// Coarse age label ("3 hours ago"). Present when
    /// `WITH annotate_relative_time`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub relative_time: Option<String>,
    /// True when this grain came from a RECALL source with no ABOUT clause —
    /// i.e. no semantic comparison was performed and `score` is a structural
    /// sentinel, not a relevance signal. Such grains must bypass score-based
    /// filters (`WITH min_score`) at the post-merge ASSEMBLE level; their
    /// inclusion is governed by the source's PRIORITY/BUDGET allocation.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub is_deterministic: bool,
    /// The other live heads that contest this grain's `(subject, relation)`.
    ///
    /// Present only when `CONTRADICTIONS` or `WITH contradiction_detection`
    /// asked for fork status, and only on a grain that is itself a live tip of
    /// an open fork. Non-empty means: another writer holds a different current
    /// value for this key and neither has been merged. An agent should treat
    /// the value as disputed rather than settled.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub contested_by: Option<Vec<String>>,
}

/// A single delta that was applied during ACCUMULATE.
#[derive(Debug, Clone, serde::Serialize)]
pub struct AccumulatedDelta {
    pub field: String,
    pub old_value: f64,
    pub new_value: f64,
}

/// One entry in a `HISTORY OF` result.
#[derive(Debug, serde::Serialize)]
pub struct CalVersionResult {
    /// Content-address hash of this version.
    pub hash: String,
    /// The object/value at this version.
    pub object: String,
    /// Creation timestamp (epoch milliseconds).
    pub created_at: i64,
    /// Confidence score.
    pub confidence: f64,
    /// Hash of the grain that superseded this version, if any.
    pub superseded_by: Option<String>,
}

/// Query execution plan returned by `EXPLAIN`.
#[derive(Debug, serde::Serialize)]
pub struct CalQueryPlan {
    /// Statement type that will be executed.
    pub statement_type: String,
    /// Grain type filter (if known at plan time).
    pub grain_type: Option<String>,
    /// Which query routing path will be used (e.g. `"bm25"`, `"hybrid"`, `"structural"`).
    pub query_routing: String,
    /// Index layers that will be consulted.
    pub index_usage: Vec<String>,
    /// Estimated relative cost.
    pub estimated_cost: String,
    /// Filters that will be applied.
    pub filters: Vec<String>,
    /// Logical pipeline stages (statement + pipeline stages).
    pub pipeline: Vec<String>,
}

// ---------------------------------------------------------------------------
// LET binding scope (Phase 2)
// ---------------------------------------------------------------------------

/// Maximum number of LET bindings per query (S-06).
const MAX_LET_BINDINGS: usize = 5;

/// Maximum grains per LET binding evaluation (C2-04).
const MAX_GRAINS_PER_LET: usize = 1000;

/// Resolved value of a LET binding.
///
/// Two variants mirror the two pipeline output shapes:
/// - `Grains`: the raw grain result set.
/// - `Extracted`: string values from `| SUBJECTS`, `| OBJECTS`, or `| HASHES`.
#[derive(Debug, Clone)]
pub enum LetValue {
    /// A grain result set (from RECALL without an extractor pipeline).
    Grains(Vec<CalGrainResult>),
    /// Extracted string values (from `| SUBJECTS`, `| OBJECTS`, or `| HASHES`).
    Extracted(Vec<String>),
}

/// LET binding scope for an execution context.
///
/// # Security (S-03)
///
/// `Drop` implementation clears and shrinks the bindings map to prevent
/// sensitive data from lingering in freed memory.
#[derive(Debug)]
pub struct LetScope {
    bindings: HashMap<String, LetValue>,
}

impl LetScope {
    /// Flatten the bindings into the string lists a `WHERE ... IN $var` needs.
    ///
    /// A `SUBJECTS`/`OBJECTS`/`HASHES` extractor already produces strings. A
    /// binding with no extractor holds whole grains, so its members are their
    /// content addresses — the only identity every grain type shares, and the
    /// one `WHERE hash IN $var` can match on.
    fn to_values(&self) -> HashMap<String, Vec<String>> {
        self.bindings
            .iter()
            .map(|(name, value)| {
                let values = match value {
                    LetValue::Extracted(v) => v.clone(),
                    LetValue::Grains(g) => g.iter().map(|r| r.hash.clone()).collect(),
                };
                (name.clone(), values)
            })
            .collect()
    }
}

impl LetScope {
    /// Evaluate all LET bindings in declaration order.
    ///
    /// # Limits
    ///
    /// - **S-06**: Maximum 5 LET bindings per query.
    /// - **C2-04**: Each binding evaluation is capped at 1000 grains.
    pub fn evaluate(
        let_bindings: &[LetBinding],
        executor: &CalExecutor,
        store: &dyn CalStoreFacade,
        query: &CalQuery,
        warnings: &mut Vec<String>,
    ) -> std::result::Result<Self, CalError> {
        if let_bindings.len() > MAX_LET_BINDINGS {
            return Err(CalError::TooManyLetBindings {
                count: let_bindings.len(),
                max: MAX_LET_BINDINGS,
                span: let_bindings.first().and_then(|b| b.span),
            });
        }

        let mut scope = LetScope {
            bindings: HashMap::new(),
        };

        for binding in let_bindings {
            // Check for duplicate names.
            if scope.bindings.contains_key(&binding.name) {
                return Err(CalError::DuplicateParameter {
                    name: binding.name.clone(),
                    span: binding.span,
                });
            }

            // Execute the source sub-query.
            let surrogate = CalQuery {
                version: query.version,
                statement: (*binding.source).clone(),
                pipeline: Vec::new(),
                with_options: Vec::new(),
                format: None,
                let_bindings: Vec::new(),
                let_values: scope.to_values(),
                user_vars: HashMap::new(),
                warnings: Vec::new(),
            };

            let payload =
                executor.execute_statement(&surrogate.statement, store, &surrogate, warnings)?;

            // Extract grains from the payload.
            let grains = extract_grains(payload);

            // C2-04: Cap grains per binding.
            let grains = if grains.len() > MAX_GRAINS_PER_LET {
                warnings.push(format!(
                    "LET ${} produced {} grains (capped to {})",
                    binding.name,
                    grains.len(),
                    MAX_GRAINS_PER_LET
                ));
                grains.into_iter().take(MAX_GRAINS_PER_LET).collect()
            } else {
                grains
            };

            // Apply the extractor to determine the LetValue type.
            let value = match binding.extractor {
                Extractor::Subjects => {
                    let values: Vec<String> = grains
                        .iter()
                        .filter_map(|g| {
                            json_field(&g.fields, "subject")
                                .and_then(|v| v.as_str())
                                .map(|s| s.to_string())
                        })
                        .collect();
                    LetValue::Extracted(values)
                }
                Extractor::Objects => {
                    let values: Vec<String> = grains
                        .iter()
                        .filter_map(|g| {
                            json_field(&g.fields, "object")
                                .and_then(|v| v.as_str())
                                .map(|s| s.to_string())
                        })
                        .collect();
                    LetValue::Extracted(values)
                }
                Extractor::Hashes => {
                    let values: Vec<String> = grains.iter().map(|g| g.hash.clone()).collect();
                    LetValue::Extracted(values)
                }
            };

            scope.bindings.insert(binding.name.clone(), value);
        }

        Ok(scope)
    }

    /// Resolve a `$name` reference.
    ///
    /// Returns `CalError::UnboundParameter` if the name is not in scope.
    pub fn resolve(&self, name: &str) -> std::result::Result<&LetValue, CalError> {
        self.bindings
            .get(name)
            .ok_or_else(|| CalError::UnboundParameter {
                name: name.to_string(),
                span: None,
            })
    }
}

/// S-03: Clear and shrink bindings on drop to prevent data lingering.
impl Drop for LetScope {
    fn drop(&mut self) {
        self.bindings.clear();
        self.bindings.shrink_to_fit();
    }
}

// ---------------------------------------------------------------------------
// CalExecutor
// ---------------------------------------------------------------------------

/// CAL query executor.
///
/// Stateless aside from configuration. Create once and call `execute()` for
/// each query. Thread-safe (`Send + Sync` by construction — no interior
/// mutability).
pub struct CalExecutor {
    config: CalExecutorConfig,
    /// The governance seam (CAL 1.3 §8.16): loop lifecycle statements
    /// execute only when a host attached one — `areev-loop-adapter` provides the
    /// real implementation. Absent → those statements return
    /// `Unsupported`.
    governance: Option<std::sync::Arc<dyn crate::governance::GovernanceHost>>,
}

impl CalExecutor {
    /// Create a new executor with the given configuration.
    pub fn new(config: CalExecutorConfig) -> Self {
        Self { config, governance: None }
    }

    /// Attach the governance host that backs `RUN LOOP`, `APPROVE`,
    /// `REJECT`, `APPLY`, `ROLLBACK`, and the loop `DESCRIBE` reads.
    pub fn with_governance(
        mut self,
        host: std::sync::Arc<dyn crate::governance::GovernanceHost>,
    ) -> Self {
        self.governance = Some(host);
        self
    }

    /// The effective configuration (read-only; for observability surfaces).
    pub fn config(&self) -> &CalExecutorConfig {
        &self.config
    }

    /// Create an executor with all defaults.
    pub fn with_defaults() -> Self {
        Self::new(CalExecutorConfig::default())
    }

    /// Check that the caller's scopes allow executing this statement.
    /// Returns Ok if: scopes are empty (no enforcement), OR scopes contain
    /// the required scope or "admin" (admin is superset of all).
    fn check_caller_scope(&self, stmt: &CalStatement) -> std::result::Result<(), CalError> {
        let scopes = &self.config.caller_scopes;
        if scopes.is_empty() {
            return Ok(()); // No enforcement (CLI, tests)
        }
        let required = required_scope_for_statement(stmt);
        // Admin scope passes any check (superset)
        if scopes.iter().any(|s| s == "admin") {
            return Ok(());
        }
        if scopes.iter().any(|s| s == required) {
            return Ok(());
        }
        // Write scope implies read
        if required == "read" && scopes.iter().any(|s| s == "write") {
            return Ok(());
        }
        Err(CalError::InsufficientScope {
            required: required.to_string(),
            statement: statement_type_name(stmt),
        })
    }

    // -----------------------------------------------------------------------
    // Public API
    // -----------------------------------------------------------------------

    /// Parse and execute a CAL query string against `store`.
    ///
    /// This is the primary entry point. Callers that have already parsed the
    /// query can call `execute_query()` instead.
    ///
    /// # Errors
    ///
    /// Returns a `CalError` for parse failures or execution errors that map to
    /// a specific CAL error code.  Store errors that do not map to a CAL code
    /// are wrapped in `CalError::BudgetExceeded`.
    pub fn execute(
        &self,
        input: &str,
        store: &dyn CalStoreFacade,
    ) -> std::result::Result<CalExecResult, CalError> {
        let start = std::time::Instant::now();

        // 1. Parse (validates length, bidi, nesting limits, etc.)
        let query = crate::parser::parse(input)?;

        // 2. Compute query hash (C-4: SHA-256 of normalized / trimmed input).
        let query_hash = compute_query_hash(input);

        // 2b. Evaluate LET bindings, then bind the results into the query.
        //
        // Bindings are evaluated sequentially in declaration order. The scope
        // used to be evaluated and dropped — the comment here said WHERE
        // resolution was "Phase 3" — so `$friends` never reached the WHERE
        // clause at all, and the documented two-step pattern quietly matched
        // nothing it was supposed to scope by.
        let mut query = query;
        let mut exec_warnings: Vec<String> = Vec::new();
        if !query.let_bindings.is_empty() {
            let scope = LetScope::evaluate(
                &query.let_bindings,
                self,
                store,
                &query,
                &mut exec_warnings,
            )?;
            query.let_values = scope.to_values();
        }

        // Validate pipeline-stage field references against the closed field
        // set before execution.
        if let CalStatement::Recall(ref r) = query.statement {
            Self::validate_pipeline_fields(&query.pipeline, &r.grain_type)?;
        }

        // 3. Execute the statement (collects execution-time warnings).
        let payload =
            self.execute_statement(&query.statement, store, &query, &mut exec_warnings)?;

        // 4. Apply pipeline stages.
        let (payload, grouped_by) = self.apply_pipeline(payload, &query.pipeline)?;

        // 5. Apply FORMAT clause if present (CAL spec v1.0.1).
        let payload = apply_format_clause(
            payload,
            &query.format,
            grouped_by.as_deref(),
            RenderInputs {
                user_vars: &query.user_vars,
                store,
                disclosure: disclosure_of(&query.with_options),
            },
            None,
            &mut exec_warnings,
        )?;

        // 6. Collect warnings from the AST + execution.
        let mut warnings: Vec<String> = query.warnings.iter().map(|w| w.to_string()).collect();
        warnings.extend(exec_warnings);

        // 7. Assemble result.
        let elapsed_ms = start.elapsed().as_millis() as u64;
        let stmt_type = statement_type_name(&query.statement);
        let result_count = count_payload_results(&payload);

        Ok(CalExecResult {
            query: input.to_string(),
            query_hash,
            result: payload,
            warnings,
            anonymized: store.anon_egress_report(),
            metadata: CalMetadata {
                version: query.version.0,
                statement_type: stmt_type,
                execution_time_ms: elapsed_ms,
                result_count,
            },
        })
    }

    /// Execute an already-parsed `CalQuery` AST against the store.
    ///
    /// Used by the `application/json+cal` wire format path where the AST
    /// arrives as JSON rather than text.  The `original_text` parameter is
    /// used for the query hash and the `query` field in the result; pass
    /// the JSON body or a synthetic representation.
    pub fn execute_parsed(
        &self,
        query: crate::ast::CalQuery,
        original_text: &str,
        store: &dyn CalStoreFacade,
    ) -> std::result::Result<CalExecResult, CalError> {
        let start = std::time::Instant::now();

        // Compute query hash (C-4).
        let query_hash = compute_query_hash(original_text);

        // Evaluate LET bindings and bind them into the query (mirror of
        // `execute()` — the two entry points have to stay in step).
        let mut query = query;
        let mut exec_warnings: Vec<String> = Vec::new();
        if !query.let_bindings.is_empty() {
            let scope = LetScope::evaluate(
                &query.let_bindings,
                self,
                store,
                &query,
                &mut exec_warnings,
            )?;
            query.let_values = scope.to_values();
        }

        // Validate pipeline-stage field references (mirror of execute()).
        if let CalStatement::Recall(ref r) = query.statement {
            Self::validate_pipeline_fields(&query.pipeline, &r.grain_type)?;
        }

        // Execute the statement.
        let payload =
            self.execute_statement(&query.statement, store, &query, &mut exec_warnings)?;

        // Apply pipeline stages.
        let (payload, grouped_by) = self.apply_pipeline(payload, &query.pipeline)?;

        // Apply FORMAT clause if present (CAL spec v1.0.1).
        let payload = apply_format_clause(
            payload,
            &query.format,
            grouped_by.as_deref(),
            RenderInputs {
                user_vars: &query.user_vars,
                store,
                disclosure: disclosure_of(&query.with_options),
            },
            None,
            &mut exec_warnings,
        )?;

        // Collect warnings from the AST + execution.
        let mut warnings: Vec<String> = query.warnings.iter().map(|w| w.to_string()).collect();
        warnings.extend(exec_warnings);

        // Assemble result.
        let elapsed_ms = start.elapsed().as_millis() as u64;
        let stmt_type = statement_type_name(&query.statement);
        let result_count = count_payload_results(&payload);

        Ok(CalExecResult {
            query: original_text.to_string(),
            query_hash,
            result: payload,
            warnings,
            anonymized: store.anon_egress_report(),
            metadata: CalMetadata {
                version: query.version.0,
                statement_type: stmt_type,
                execution_time_ms: elapsed_ms,
                result_count,
            },
        })
    }

    // -----------------------------------------------------------------------
    // Statement dispatch
    // -----------------------------------------------------------------------

    /// Internal statement execution — exposed to sibling modules (e.g.
    /// `assemble.rs`) via `pub(super)`.
    /// Namespace for Tier-1 write statements, mirroring the RECALL
    /// precedence: a capability-scoped `namespace_override` always wins,
    /// then an explicit `SET namespace = ...`, then the session default.
    /// Without this, an ADD in a `with_session` facade lands in the store
    /// default namespace while RECALL reads the session one — the write
    /// succeeds but the same session can't see it.
    fn inject_write_namespace(
        &self,
        fields: &mut serde_json::Map<String, serde_json::Value>,
        store: &dyn CalStoreFacade,
    ) {
        if let Some(ref ns) = self.config.namespace_override {
            fields.insert("namespace".into(), serde_json::Value::String(ns.clone()));
        } else if !fields.contains_key("namespace") {
            if let Some(ns) = store.default_namespace() {
                fields.insert(
                    "namespace".into(),
                    serde_json::Value::String(ns.to_string()),
                );
            }
        }
    }

    pub(super) fn execute_statement_internal(
        &self,
        stmt: &CalStatement,
        store: &dyn CalStoreFacade,
        query: &CalQuery,
        exec_warnings: &mut Vec<String>,
    ) -> std::result::Result<CalResultPayload, CalError> {
        self.execute_statement(stmt, store, query, exec_warnings)
    }

    pub fn execute_statement(
        &self,
        stmt: &CalStatement,
        store: &dyn CalStoreFacade,
        query: &CalQuery,
        exec_warnings: &mut Vec<String>,
    ) -> std::result::Result<CalResultPayload, CalError> {
        // Identity-based scope check — must pass before any execution.
        self.check_caller_scope(stmt)?;

        match stmt {
            CalStatement::Recall(recall) => {
                self.execute_recall(recall, store, query, exec_warnings)
            }
            CalStatement::Exists(exists) => {
                self.execute_exists(exists, store, exec_warnings, &query.let_values)
            }
            CalStatement::History(history) => {
                self.execute_history(history, store, exec_warnings, &query.let_values)
            }
            CalStatement::Describe(describe) => self.execute_describe(describe, store),
            CalStatement::Explain(explain) => self.execute_explain(explain, store, query),
            CalStatement::Batch(batch) => self.execute_batch(batch, store, exec_warnings),
            CalStatement::Coalesce(coalesce) => {
                self.execute_coalesce(coalesce, store, query, exec_warnings)
            }
            CalStatement::SetOp(set_op) => self.execute_set_op(set_op, store, query, exec_warnings),
            CalStatement::Assemble(assemble) => {
                self.execute_assemble(assemble, store, query, exec_warnings)
            }

            // Tier 1 — ADD and SUPERSEDE execute when tier1_enabled, REVERT always unsupported.
            CalStatement::Add(add) => {
                if !self.config.tier1_enabled {
                    return Err(CalError::Tier1NotEnabled {
                        statement: "ADD".into(),
                        span: add.span,
                    });
                }
                // Reject grain types that cannot be built from flat `SET k = v`
                // pairs. Sourced from the grain-type registry (D1) so there is
                // no separate list to keep in sync. This is a shape check, not a
                // permission check — the structured write paths (ADD WORKFLOW,
                // the per-type JSON builders behind `cal_add`, `capture()`) are
                // deliberately not gated by it.
                let add_type = add.grain_type.as_str();
                if !areev_core::types::registry::add_via_set_names().any(|n| n == add_type) {
                    let shapeable: Vec<&str> = areev_core::types::registry::add_via_set_names().collect();
                    return Ok(CalResultPayload::Unsupported {
                        statement: "add".into(),
                        message: format!(
                            "Grain type '{}' cannot be created via ADD. Addable types: {}.",
                            add_type,
                            shapeable.join(", ")
                        ),
                    });
                }
                // Reject unresolved parameters in SET values.
                for fa in &add.fields {
                    if let super::ast::Value::Parameter { name } = &fa.value {
                        return Ok(CalResultPayload::Unsupported {
                            statement: "add".into(),
                            message: format!(
                                "Unresolved parameter ${} in SET clause. Parameters must be bound via LET.",
                                name
                            ),
                        });
                    }
                }
                let mut fields = serde_json::Map::new();
                for fa in &add.fields {
                    fields.insert(fa.field.clone(), cal_value_to_json(&fa.value));
                }
                // Inject the REASON into the fields map.
                if !add.reason.is_empty() {
                    fields.insert(
                        "add_reason".into(),
                        serde_json::Value::String(add.reason.clone()),
                    );
                }
                // `WITH occurrence` — honest inert-option handling: tool
                // grains are not ADD-able from text (add_via_set is false;
                // the shape check above already refused them), and the host
                // API that IS the tool-call path (`record_tool_call`)
                // stamps per-call identity itself. The option exists so an
                // agent pasting the pattern gets a warning that names the
                // right tool, not silence.
                if add
                    .with_options
                    .iter()
                    .any(|o| matches!(o, super::ast::AddWithOption::Occurrence))
                {
                    exec_warnings.push(
                        "CAL-W014: WITH occurrence is inert here — tool occurrences are \
                         recorded via record_tool_call (every binding), which stamps the \
                         per-call identity that keeps retries distinct."
                            .to_string(),
                    );
                }
                self.inject_write_namespace(&mut fields, store);
                // Build AddOptions from WITH clause.
                let options = build_add_options(&add.with_options, exec_warnings);
                match store.cal_add_with_options(add.grain_type.as_str(), &fields, options) {
                    Ok(result) => Ok(CalResultPayload::Added {
                        hash: hex::encode(result.hash.as_bytes()),
                        grain_type: add.grain_type.as_str().to_string(),
                        extracted_count: if result.extracted_count > 0 {
                            Some(result.extracted_count)
                        } else {
                            None
                        },
                        extraction_warnings: result.extraction_warnings,
                    }),
                    Err(e) => Ok(CalResultPayload::Unsupported {
                        statement: "add".into(),
                        message: format!("ADD failed: {e}"),
                    }),
                }
            }
            CalStatement::Supersede(sup) => {
                if !self.config.tier1_enabled {
                    return Err(CalError::Tier1NotEnabled {
                        statement: "SUPERSEDE".into(),
                        span: sup.span,
                    });
                }
                // Strip optional "sha256:" prefix.
                let raw_hash = sup.hash.strip_prefix("sha256:").unwrap_or(&sup.hash);
                let old_hash = match Hash::from_hex(raw_hash) {
                    Ok(h) => h,
                    Err(e) => {
                        return Ok(CalResultPayload::Unsupported {
                            statement: "supersede".into(),
                            message: format!("invalid hash: {e}"),
                        })
                    }
                };
                // Get old grain to determine its type and merge fields.
                let old_grain = match store.get(&old_hash) {
                    Ok(g) => g,
                    Err(e) => {
                        return Ok(CalResultPayload::Unsupported {
                            statement: "supersede".into(),
                            message: format!("cannot retrieve grain for SUPERSEDE: {e}"),
                        })
                    }
                };
                let grain_type_str = old_grain.grain_type.as_str();
                // Start from the old grain's fields and overlay SET clauses.
                let mut fields: serde_json::Map<String, serde_json::Value> =
                    old_grain.fields.into_iter().collect();
                for sc in &sup.set_clauses {
                    fields.insert(sc.field.clone(), cal_value_to_json(&sc.value));
                }
                // Inject the BECAUSE reason.
                if !sup.reason.is_empty() {
                    fields.insert(
                        "supersede_reason".into(),
                        serde_json::Value::String(sup.reason.clone()),
                    );
                }
                match store.cal_supersede(&old_hash, grain_type_str, &fields) {
                    Ok(new_hash) => Ok(CalResultPayload::Superseded {
                        old_hash: hex::encode(old_hash.as_bytes()),
                        new_hash: hex::encode(new_hash.as_bytes()),
                    }),
                    Err(e) => Ok(CalResultPayload::Unsupported {
                        statement: "supersede".into(),
                        message: format!("SUPERSEDE failed: {e}"),
                    }),
                }
            }
            CalStatement::AddWorkflow(wf) => {
                if !self.config.tier1_enabled {
                    return Err(CalError::Tier1NotEnabled {
                        statement: "ADD WORKFLOW".into(),
                        span: wf.span,
                    });
                }
                // Build JSON fields for the workflow grain.
                let mut fields = serde_json::Map::new();
                fields.insert("name".into(), serde_json::Value::String(wf.name.clone()));
                if let Some(ref trigger) = wf.trigger {
                    fields.insert("trigger".into(), serde_json::Value::String(trigger.clone()));
                }
                // nodes: array of strings
                fields.insert(
                    "nodes".into(),
                    serde_json::Value::Array(
                        wf.nodes
                            .iter()
                            .map(|n| serde_json::Value::String(n.clone()))
                            .collect(),
                    ),
                );
                // edges: array of objects (repeat is NOT stored on edges —
                // `* N` populates the top-level `retries` map instead).
                let edges_json: Vec<serde_json::Value> = wf
                    .edges
                    .iter()
                    .map(|e| {
                        let mut m = serde_json::Map::new();
                        m.insert("src".into(), serde_json::Value::String(e.src.clone()));
                        m.insert("dst".into(), serde_json::Value::String(e.dst.clone()));
                        if let Some(ref c) = e.cond {
                            m.insert("cond".into(), serde_json::Value::String(c.clone()));
                        }
                        serde_json::Value::Object(m)
                    })
                    .collect();
                fields.insert("edges".into(), serde_json::Value::Array(edges_json));
                // bindings: object
                if !wf.bindings.is_empty() {
                    let mut bind_map = serde_json::Map::new();
                    for b in &wf.bindings {
                        bind_map.insert(
                            b.node.clone(),
                            serde_json::Value::String(format!("sha256:{}", b.hash)),
                        );
                    }
                    fields.insert("bindings".into(), serde_json::Value::Object(bind_map));
                }
                // retries: `* N` on an edge means "retry the target node
                // up to N times on failure".  This is stored as a top-level
                // `retries` map keyed by the destination node name.
                // Several edges may share a destination (a join, or a diamond), and
                // each may carry its own `* N`. The map is keyed by node, so a plain
                // insert lets the last edge win and the other bounds vanish — take
                // the largest instead, the only merge that never retries a node
                // fewer times than some edge asked for.
                let mut retries_map = serde_json::Map::new();
                for e in &wf.edges {
                    if let Some(r) = e.repeat {
                        let merged = retries_map
                            .get(&e.dst)
                            .and_then(|v| v.as_u64())
                            .map_or(r, |prev| (prev as u32).max(r));
                        retries_map.insert(
                            e.dst.clone(),
                            serde_json::Value::Number(serde_json::Number::from(merged)),
                        );
                    }
                }
                if !retries_map.is_empty() {
                    fields.insert("retries".into(), serde_json::Value::Object(retries_map));
                }
                self.inject_write_namespace(&mut fields, store);
                let options = build_add_options(&wf.with_options, exec_warnings);
                match store.cal_add_with_options("workflow", &fields, options) {
                    Ok(result) => Ok(CalResultPayload::Added {
                        hash: hex::encode(result.hash.as_bytes()),
                        grain_type: "workflow".into(),
                        extracted_count: None,
                        extraction_warnings: vec![],
                    }),
                    Err(e) => Ok(CalResultPayload::Unsupported {
                        statement: "add workflow".into(),
                        message: format!("ADD workflow failed: {e}"),
                    }),
                }
            }
            CalStatement::SupersedeWorkflow(wf) => {
                if !self.config.tier1_enabled {
                    return Err(CalError::Tier1NotEnabled {
                        statement: "SUPERSEDE WORKFLOW".into(),
                        span: wf.span,
                    });
                }
                let raw_hash = wf.hash.strip_prefix("sha256:").unwrap_or(&wf.hash);
                let old_hash = match Hash::from_hex(raw_hash) {
                    Ok(h) => h,
                    Err(e) => {
                        return Ok(CalResultPayload::Unsupported {
                            statement: "supersede workflow".into(),
                            message: format!("invalid hash: {e}"),
                        })
                    }
                };
                // Build workflow fields for supersession.
                let mut fields = serde_json::Map::new();
                if let Some(ref trigger) = wf.trigger {
                    fields.insert("trigger".into(), serde_json::Value::String(trigger.clone()));
                }
                fields.insert(
                    "nodes".into(),
                    serde_json::Value::Array(
                        wf.nodes
                            .iter()
                            .map(|n| serde_json::Value::String(n.clone()))
                            .collect(),
                    ),
                );
                let edges_json: Vec<serde_json::Value> = wf
                    .edges
                    .iter()
                    .map(|e| {
                        let mut m = serde_json::Map::new();
                        m.insert("src".into(), serde_json::Value::String(e.src.clone()));
                        m.insert("dst".into(), serde_json::Value::String(e.dst.clone()));
                        if let Some(ref c) = e.cond {
                            m.insert("cond".into(), serde_json::Value::String(c.clone()));
                        }
                        serde_json::Value::Object(m)
                    })
                    .collect();
                fields.insert("edges".into(), serde_json::Value::Array(edges_json));
                if !wf.bindings.is_empty() {
                    let mut bind_map = serde_json::Map::new();
                    for b in &wf.bindings {
                        bind_map.insert(
                            b.node.clone(),
                            serde_json::Value::String(format!("sha256:{}", b.hash)),
                        );
                    }
                    fields.insert("bindings".into(), serde_json::Value::Object(bind_map));
                }
                // retries: same semantics as AddWorkflow.
                // Several edges may share a destination (a join, or a diamond), and
                // each may carry its own `* N`. The map is keyed by node, so a plain
                // insert lets the last edge win and the other bounds vanish — take
                // the largest instead, the only merge that never retries a node
                // fewer times than some edge asked for.
                let mut retries_map = serde_json::Map::new();
                for e in &wf.edges {
                    if let Some(r) = e.repeat {
                        let merged = retries_map
                            .get(&e.dst)
                            .and_then(|v| v.as_u64())
                            .map_or(r, |prev| (prev as u32).max(r));
                        retries_map.insert(
                            e.dst.clone(),
                            serde_json::Value::Number(serde_json::Number::from(merged)),
                        );
                    }
                }
                if !retries_map.is_empty() {
                    fields.insert("retries".into(), serde_json::Value::Object(retries_map));
                }
                if !wf.reason.is_empty() {
                    fields.insert(
                        "supersede_reason".into(),
                        serde_json::Value::String(wf.reason.clone()),
                    );
                }
                match store.cal_supersede(&old_hash, "workflow", &fields) {
                    Ok(new_hash) => Ok(CalResultPayload::Superseded {
                        old_hash: hex::encode(old_hash.as_bytes()),
                        new_hash: hex::encode(new_hash.as_bytes()),
                    }),
                    Err(e) => Ok(CalResultPayload::Unsupported {
                        statement: "supersede workflow".into(),
                        message: format!("SUPERSEDE workflow failed: {e}"),
                    }),
                }
            }
            CalStatement::Accumulate(acc) => {
                if !self.config.tier1_enabled {
                    return Err(CalError::Tier1NotEnabled {
                        statement: "ACCUMULATE".into(),
                        span: acc.span,
                    });
                }

                // Convert DeltaOps to (field, delta) pairs.
                let add_ops: Vec<(String, f64)> = acc
                    .add_ops
                    .iter()
                    .map(|op| (op.field.clone(), op.delta))
                    .collect();

                // Convert SET ops to JSON map.
                let mut set_map = serde_json::Map::new();
                for s in &acc.set_ops {
                    set_map.insert(s.field.clone(), cal_value_to_json(&s.value));
                }
                if !acc.reason.is_empty() {
                    set_map.insert(
                        "supersede_reason".into(),
                        serde_json::Value::String(acc.reason.clone()),
                    );
                }

                match store.cal_accumulate(
                    acc.grain_type.as_str(),
                    &acc.target,
                    &add_ops,
                    &set_map,
                    &acc.reason,
                ) {
                    Ok(result) => Ok(CalResultPayload::Accumulated {
                        old_hash: hex::encode(result.old_hash.as_bytes()),
                        new_hash: hex::encode(result.new_hash.as_bytes()),
                        deltas: result
                            .applied_deltas
                            .iter()
                            .map(|(f, old, new)| AccumulatedDelta {
                                field: f.clone(),
                                old_value: *old,
                                new_value: *new,
                            })
                            .collect(),
                    }),
                    // CU-86d2wr4n4: typed CalError propagation. Retry
                    // exhaustion → CAL-E083 (409); generic internal →
                    // CAL-E084 (500). Inner-error text never reaches
                    // the wire (security C3); request_id correlation
                    // happens in the route layer's tracing::error!.
                    Err(areev_core::error::AreevError::AccumulateRetryExhausted) => {
                        // Log the inner cause (no PII — fixed string)
                        // for operator forensics; correlation with
                        // request_id is added by the tracing span.
                        tracing::error!(
                            cal_code = "CAL-E083",
                            "ACCUMULATE retry budget exhausted under sustained contention"
                        );
                        let (subject, relation) = match &acc.target {
                            super::ast::AccumulateTarget::TipResolved {
                                subject, relation, ..
                            } => (subject.clone(), relation.clone()),
                            super::ast::AccumulateTarget::Hash { .. } => {
                                (String::new(), String::new())
                            }
                        };
                        Err(CalError::AccumulateRetryExhausted {
                            subject,
                            relation,
                            span: acc.span,
                        })
                    }
                    Err(areev_core::error::AreevError::AccumulateInternal(detail)) => {
                        tracing::error!(
                            cal_code = "CAL-E084",
                            error = %detail,
                            "ACCUMULATE internal failure"
                        );
                        Err(CalError::AccumulateInternal { span: acc.span })
                    }
                    // CU-86d2wr4n4 v2.1: CAL-E085 backpressure — admission
                    // control rejected this attempt (per-key inflight cap
                    // or global retry-permit semaphore saturated). Surfaces
                    // as HTTP 429 with `Retry-After: 1`.
                    Err(areev_core::error::AreevError::AccumulateBackpressureRejected) => {
                        tracing::warn!(cal_code = "CAL-E085", "ACCUMULATE backpressure rejected");
                        let (subject, relation) = match &acc.target {
                            super::ast::AccumulateTarget::TipResolved {
                                subject, relation, ..
                            } => (subject.clone(), relation.clone()),
                            super::ast::AccumulateTarget::Hash { .. } => {
                                (String::new(), String::new())
                            }
                        };
                        Err(CalError::AccumulateBackpressureRejected {
                            subject,
                            relation,
                            span: acc.span,
                        })
                    }
                    // Pre-existing validation/typing errors keep flowing
                    // as CAL-E081 / CAL-E020 etc. — wrapped in the
                    // generic Unsupported envelope, which routes will
                    // continue to surface as 400 via the existing
                    // AreevError::Validation mapping.
                    Err(e) => Ok(CalResultPayload::Unsupported {
                        statement: "accumulate".into(),
                        message: format!("ACCUMULATE failed: {e}"),
                    }),
                }
            }
            CalStatement::Revert(_) => Ok(CalResultPayload::Unsupported {
                statement: "revert".into(),
                message: "Tier 1 REVERT semantics are not yet defined. \
                              Use EXPLAIN to preview without execution."
                    .into(),
            }),

            // Tier 2 — FORGET executes when allow_destructive_ops is enabled.
            CalStatement::Forget(forget) => {
                if !self.config.allow_destructive_ops {
                    return Ok(CalResultPayload::Unsupported {
                        statement: "forget".into(),
                        message: "Destructive operations are disabled for this session \
                                  (started with --no-destructive-ops)."
                            .into(),
                    });
                }
                match &forget.target {
                    super::ast::ForgetTarget::Hash { hash } => {
                        let h = Hash::from_hex(hash).map_err(|_| CalError::InvalidHash {
                            found: hash.clone(),
                            span: forget.span,
                        })?;
                        match store.cal_delete(&h, forget.reason.as_deref()) {
                            Ok(()) => Ok(CalResultPayload::Forgotten {
                                target: format!("hash:{hash}"),
                                count: 1,
                            }),
                            Err(e) => Ok(CalResultPayload::Unsupported {
                                statement: "forget".into(),
                                message: format!("FORGET failed: {e}"),
                            }),
                        }
                    }
                    super::ast::ForgetTarget::User { user_id } => {
                        // BECAUSE is mandatory on identity erasure (the text
                        // parser enforces it; the JSON-CAL path enforces it
                        // here).
                        let because = match forget.reason.as_deref() {
                            Some(r) if !r.trim().is_empty() => r,
                            _ => {
                                return Err(CalError::MissingReason { span: forget.span });
                            }
                        };
                        match store.cal_forget_user(user_id, forget.text_mentions, because) {
                            Ok(proof) => Ok(CalResultPayload::Forgotten {
                                target: format!("subject:{user_id}"),
                                count: proof.count,
                            }),
                            Err(e) => Ok(CalResultPayload::Unsupported {
                                statement: "forget".into(),
                                message: format!("FORGET SUBJECT failed: {e}"),
                            }),
                        }
                    }
                    super::ast::ForgetTarget::Scope { scope } => {
                        match store.cal_forget_scope(scope) {
                            Ok(proof) => Ok(CalResultPayload::Forgotten {
                                target: format!("scope:{scope}"),
                                count: proof.count,
                            }),
                            Err(e) => Ok(CalResultPayload::Unsupported {
                                statement: "forget".into(),
                                message: format!("FORGET SCOPE failed: {e}"),
                            }),
                        }
                    }
                }
            }

            // Template management (FR-003). The per-namespace ceiling
            // (§10.8, CAL-E118) is enforced in the registry.
            CalStatement::DefineTemplate(def) => {
                store
                    .define_template(
                        &def.name,
                        &def.source,
                        def.description.as_deref(),
                        def.parent.as_deref(),
                        &def.grain_types,
                    )
                    .map_err(|e| {
                        let s = e.to_string();
                        // An authorization refusal is neither a bad name nor
                        // bad syntax — surface it as itself.
                        if s.contains("AUT-E") {
                            return CalError::NotAuthorized { detail: s, span: None };
                        }
                        // If the inner error is already a CAL error about template
                        // validation (unknown variable, syntax, etc.), surface it
                        // directly instead of wrapping it as TemplateInvalidName.
                        if s.contains("CAL-E04")
                            || s.contains("CAL-E11")
                            || s.contains("Unknown template")
                            || s.contains("Invalid template")
                            || s.contains("syntax")
                        {
                            CalError::TemplateSyntaxError {
                                detail: s,
                                span: None,
                            }
                        } else {
                            CalError::TemplateInvalidName {
                                name: def.name.clone(),
                                span: None,
                            }
                        }
                    })?;
                Ok(CalResultPayload::TemplateDefined {
                    name: def.name.clone(),
                })
            }
            CalStatement::DropTemplate(drop) => {
                if !self.config.allow_destructive_ops {
                    return Ok(CalResultPayload::Unsupported {
                        statement: "drop_template".into(),
                        message: "Destructive operations are disabled for this session \
                                  (started with --no-destructive-ops)."
                            .into(),
                    });
                }
                match store.drop_template(&drop.name) {
                    Ok(()) => Ok(CalResultPayload::TemplateDropped {
                        name: drop.name.clone(),
                    }),
                    Err(e) => Ok(CalResultPayload::Unsupported {
                        statement: "drop_template".into(),
                        message: format!("DROP TEMPLATE failed: {e}"),
                    }),
                }
            }

            // Saved query management. The per-namespace ceiling is enforced
            // in the registry.
            CalStatement::DefineQuery(def) => {
                store
                    .define_query(
                        &def.name,
                        &def.body,
                        def.description.as_deref(),
                        &def.params,
                    )
                    .map_err(|e| {
                        let detail = e.to_string();
                        if detail.contains("AUT-E") {
                            CalError::NotAuthorized { detail, span: None }
                        } else {
                            CalError::InvalidQueryBody { detail, span: None }
                        }
                    })?;
                Ok(CalResultPayload::QueryDefined {
                    name: def.name.clone(),
                })
            }
            CalStatement::DropQuery(drop) => {
                if !self.config.allow_destructive_ops {
                    return Ok(CalResultPayload::Unsupported {
                        statement: "drop_query".into(),
                        message: "Destructive operations are disabled for this session \
                                  (started with --no-destructive-ops)."
                            .into(),
                    });
                }
                match store.drop_query(&drop.name) {
                    Ok(()) => Ok(CalResultPayload::QueryDropped {
                        name: drop.name.clone(),
                    }),
                    Err(e) => Ok(CalResultPayload::Unsupported {
                        statement: "drop_query".into(),
                        message: format!("DROP QUERY failed: {e}"),
                    }),
                }
            }
            CalStatement::RunQuery(run) => self.execute_run_query(run, store, query, exec_warnings),

            // Tier 2 — PURGE OLDER THAN (the retention sweep).
            CalStatement::Purge(purge) => {
                if !self.config.allow_destructive_ops {
                    return Ok(CalResultPayload::Unsupported {
                        statement: "purge".into(),
                        message: "Destructive operations are disabled for this session \
                                  (started with --no-destructive-ops)."
                            .into(),
                    });
                }
                // BECAUSE is mandatory (the text parser enforces it; the
                // JSON-CAL path enforces it here).
                let because = match purge.reason.as_deref() {
                    Some(r) if !r.trim().is_empty() => r,
                    _ => {
                        return Err(CalError::MissingReason { span: purge.span });
                    }
                };
                let min_age = purge.min_age_days.unwrap_or(30.0);
                let batch_limit = purge.limit.unwrap_or(1000);
                let ns = purge.namespace.as_deref();
                match store.cal_purge_stale(
                    min_age,
                    ns,
                    batch_limit,
                    purge.grain_type.as_deref(),
                    because,
                ) {
                    Ok(count) => Ok(CalResultPayload::Purged { count }),
                    Err(e) => Ok(CalResultPayload::Unsupported {
                        statement: "purge".into(),
                        message: format!("PURGE failed: {e}"),
                    }),
                }
            }

            // REPORT SUBJECT — the read-only DSAR selection (OMS 1.6
            // draft): a pure read under the session's `read` grant, no
            // destructive gate and no BECAUSE.
            CalStatement::ReportSubject(rs) => {
                match store.cal_subject_report(&rs.subject_id, rs.text_mentions) {
                    Ok(report) => Ok(CalResultPayload::SubjectReport {
                        subject: rs.subject_id.clone(),
                        identity_names: report.identity_names,
                        grains: report.grains,
                    }),
                    Err(e) => Ok(CalResultPayload::Unsupported {
                        statement: "report_subject".into(),
                        message: format!("REPORT SUBJECT failed: {e}"),
                    }),
                }
            }

            // Wave-2 reads (CAL 1.3): as-of, the run↔memory join, reverse
            // provenance, the fork listing. All plain reads under the
            // session's grants.
            CalStatement::EntityAt(ea) => {
                let axis = ea.axis.clone().unwrap_or_else(|| "world".to_string());
                match store.cal_entity_at(&ea.subject, &ea.relation, ea.at_ms, &axis) {
                    Ok(grain) => Ok(CalResultPayload::EntityAt { grain, axis, at_ms: ea.at_ms }),
                    Err(e) => Ok(CalResultPayload::Unsupported {
                        statement: "entity_at".into(),
                        message: format!("ENTITY AT failed: {e}"),
                    }),
                }
            }
            CalStatement::RunTrace(rt) => {
                match store.cal_run_trace(&rt.run_id, rt.limit.unwrap_or(64).min(1024)) {
                    Ok(trace) => Ok(CalResultPayload::RunTrace {
                        run_id: rt.run_id.clone(),
                        trace,
                    }),
                    Err(e) => Ok(CalResultPayload::Unsupported {
                        statement: "run_trace".into(),
                        message: format!("RUN TRACE failed: {e}"),
                    }),
                }
            }
            CalStatement::RunsTouching(rt) => {
                let h = Hash::from_hex(&rt.hash).map_err(|_| CalError::InvalidHash {
                    found: rt.hash.clone(),
                    span: rt.span,
                })?;
                match store.cal_runs_touching(&h, rt.depth.unwrap_or(4).min(8)) {
                    Ok(runs) => Ok(CalResultPayload::RunsTouching { hash: rt.hash.clone(), runs }),
                    Err(e) => Ok(CalResultPayload::Unsupported {
                        statement: "runs_touching".into(),
                        message: format!("RUNS TOUCHING failed: {e}"),
                    }),
                }
            }
            CalStatement::DerivedFrom(df) => {
                let h = Hash::from_hex(&df.hash).map_err(|_| CalError::InvalidHash {
                    found: df.hash.clone(),
                    span: df.span,
                })?;
                match store.cal_derived_from(&h) {
                    Ok(grains) => Ok(CalResultPayload::DerivedFrom { hash: df.hash.clone(), grains }),
                    Err(e) => Ok(CalResultPayload::Unsupported {
                        statement: "derived_from".into(),
                        message: format!("DERIVED FROM failed: {e}"),
                    }),
                }
            }
            CalStatement::Merge(mg) => {
                if !self.config.tier1_enabled {
                    return Err(CalError::Tier1NotEnabled {
                        statement: "MERGE".into(),
                        span: mg.span,
                    });
                }
                match store.cal_merge(
                    &mg.subject,
                    &mg.relation,
                    &mg.object,
                    mg.confidence.unwrap_or(0.9),
                    &mg.reason,
                ) {
                    Ok(hash) => Ok(CalResultPayload::Merged {
                        hash: hash.to_hex(),
                        subject: mg.subject.clone(),
                        relation: mg.relation.clone(),
                        object: mg.object.clone(),
                    }),
                    Err(e) => Ok(CalResultPayload::Unsupported {
                        statement: "merge".into(),
                        message: format!("MERGE failed: {e}"),
                    }),
                }
            }
            CalStatement::Related(rel) => {
                let relations: Vec<&str> = rel
                    .relations
                    .split(',')
                    .map(str::trim)
                    .filter(|r| !r.is_empty())
                    .collect();
                match store.cal_related(
                    &rel.start,
                    &relations,
                    rel.direction.as_deref().unwrap_or("out"),
                    rel.depth.unwrap_or(2),
                    rel.limit.unwrap_or(64),
                ) {
                    Ok(entities) => Ok(CalResultPayload::RelatedEntities {
                        start: rel.start.clone(),
                        entities,
                    }),
                    Err(e) => Ok(CalResultPayload::Unsupported {
                        statement: "related".into(),
                        message: format!("RELATED failed: {e}"),
                    }),
                }
            }
            CalStatement::Novelty(nv) => {
                match store.cal_novelty(
                    &nv.text,
                    nv.subject.as_deref(),
                    nv.relation.as_deref(),
                    nv.limit.unwrap_or(5),
                ) {
                    Ok(rows) => Ok(CalResultPayload::NoveltyMatches {
                        matches: rows
                            .into_iter()
                            .map(|(hash, similarity)| {
                                serde_json::json!({ "hash": hash, "similarity": similarity })
                            })
                            .collect(),
                    }),
                    Err(e) => Ok(CalResultPayload::Unsupported {
                        statement: "novelty".into(),
                        message: format!("NOVELTY failed: {e}"),
                    }),
                }
            }
            CalStatement::ShowForks(_) => match store.open_forks() {
                Ok(groups) => Ok(CalResultPayload::Forks {
                    forks: groups
                        .iter()
                        .map(|f| {
                            serde_json::json!({
                                "namespace": f.namespace,
                                "subject": f.subject,
                                "relation": f.relation,
                                "heads": f.heads,
                            })
                        })
                        .collect(),
                }),
                Err(e) => Ok(CalResultPayload::Unsupported {
                    statement: "show_forks".into(),
                    message: format!("SHOW FORKS failed: {e}"),
                }),
            },

            // REMEMBER (CAL 1.3) — the capture verb, an append-only write.
            CalStatement::Remember(rem) => {
                if !self.config.tier1_enabled {
                    return Err(CalError::Tier1NotEnabled {
                        statement: "REMEMBER".into(),
                        span: rem.span,
                    });
                }
                match store.cal_remember(
                    &rem.content,
                    rem.session_id.as_deref(),
                    rem.role.as_deref(),
                    rem.run_id.as_deref(),
                ) {
                    Ok(hash) => Ok(CalResultPayload::Remembered { hash: hash.to_hex() }),
                    Err(e) => Ok(CalResultPayload::Unsupported {
                        statement: "remember".into(),
                        message: format!("REMEMBER failed: {e}"),
                    }),
                }
            }

            // Tier 3 — DCL (CAL 1.3 §8.15). Append-only writes to the authz
            // namespace: capped by the writes cap (`tier1_enabled`), gated
            // by the session's `admin` grant at the facade, untouched by
            // `allow_destructive_ops`.
            CalStatement::Grant(grant) => {
                if !self.config.tier1_enabled {
                    return Err(CalError::Tier1NotEnabled {
                        statement: "GRANT".into(),
                        span: grant.span,
                    });
                }
                match store.cal_grant(
                    &grant.principal,
                    &grant.verbs,
                    &grant.namespaces,
                    grant.reason.as_deref(),
                ) {
                    Ok(hash) => {
                        let object = areev_core::authz::Grant {
                            verbs: grant
                                .verbs
                                .iter()
                                .filter_map(|v| areev_core::authz::Verb::parse(v).ok())
                                .collect(),
                            namespaces: grant.namespaces.clone(),
                        }
                        .to_object_string();
                        Ok(CalResultPayload::Granted {
                            principal: grant.principal.clone(),
                            object,
                            hash: hash.to_hex(),
                        })
                    }
                    Err(e) => Ok(CalResultPayload::Unsupported {
                        statement: "grant".into(),
                        message: format!("GRANT failed: {e}"),
                    }),
                }
            }
            CalStatement::Revoke(revoke) => {
                if !self.config.tier1_enabled {
                    return Err(CalError::Tier1NotEnabled {
                        statement: "REVOKE".into(),
                        span: revoke.span,
                    });
                }
                match store.cal_revoke(
                    &revoke.principal,
                    &revoke.verbs,
                    &revoke.namespaces,
                    revoke.reason.as_deref(),
                ) {
                    Ok(grants_touched) => Ok(CalResultPayload::Revoked {
                        principal: revoke.principal.clone(),
                        grants_touched,
                    }),
                    Err(e) => Ok(CalResultPayload::Unsupported {
                        statement: "revoke".into(),
                        message: format!("REVOKE failed: {e}"),
                    }),
                }
            }
            // Governance (CAL 1.3 §8.16) — executes through the attached
            // GovernanceHost; identity comes from the bound session, never
            // the statement. No host → Unsupported (this executor was not
            // wired for governance).
            CalStatement::Approve(g) | CalStatement::Reject(g) => {
                let decision = if matches!(stmt, CalStatement::Approve(_)) {
                    crate::governance::ReviewDecision::Approve
                } else {
                    crate::governance::ReviewDecision::Reject
                };
                let name = if decision == crate::governance::ReviewDecision::Approve {
                    "approve"
                } else {
                    "reject"
                };
                let Some(host) = &self.governance else {
                    return Ok(governance_unwired(name));
                };
                match host.review(store, &g.hash, decision, &g.reason) {
                    Ok(()) => Ok(CalResultPayload::Reviewed {
                        hash: g.hash.clone(),
                        decision: name.into(),
                    }),
                    Err(e) => Ok(CalResultPayload::Unsupported {
                        statement: name.into(),
                        message: format!("{} failed: {e}", name.to_uppercase()),
                    }),
                }
            }
            CalStatement::ApplyRec(g) => {
                let Some(host) = &self.governance else {
                    return Ok(governance_unwired("apply"));
                };
                match host.apply(
                    store,
                    &g.hash,
                    &g.reason,
                    self.config.allow_destructive_ops,
                ) {
                    Ok(rollbackable) => Ok(CalResultPayload::RecApplied {
                        hash: g.hash.clone(),
                        rollbackable,
                    }),
                    Err(e) => Ok(CalResultPayload::Unsupported {
                        statement: "apply".into(),
                        message: format!("APPLY failed: {e}"),
                    }),
                }
            }
            CalStatement::RollbackRec(g) => {
                let Some(host) = &self.governance else {
                    return Ok(governance_unwired("rollback"));
                };
                match host.rollback(store, &g.hash, &g.reason) {
                    Ok(()) => Ok(CalResultPayload::RecRolledBack { hash: g.hash.clone() }),
                    Err(e) => Ok(CalResultPayload::Unsupported {
                        statement: "rollback".into(),
                        message: format!("ROLLBACK failed: {e}"),
                    }),
                }
            }
            CalStatement::RunLoop(run) => {
                let Some(host) = &self.governance else {
                    return Ok(governance_unwired("run_loop"));
                };
                let opts = crate::governance::RunLoopOptions {
                    full_sweep: run.full_sweep,
                    min_new: run.min_new,
                    if_stale_ms: run.if_stale_ms,
                };
                match host.run_loop(store, &opts) {
                    Ok(report) => Ok(CalResultPayload::LoopRan { run: report }),
                    Err(e) => Ok(CalResultPayload::Unsupported {
                        statement: "run_loop".into(),
                        message: format!("RUN LOOP failed: {e}"),
                    }),
                }
            }
            CalStatement::ShowGrants(show) => {
                match store.cal_show_grants(show.principal.as_deref()) {
                    Ok(rows) => Ok(CalResultPayload::GrantList {
                        grants: rows
                            .into_iter()
                            .map(|row| {
                                let parsed =
                                    areev_core::authz::Grant::from_object_string(&row.object)
                                        .ok();
                                serde_json::json!({
                                    "principal": row.principal,
                                    "object": row.object,
                                    "verbs": parsed.as_ref().map(|g| g
                                        .verbs
                                        .iter()
                                        .map(|v| v.as_str())
                                        .collect::<Vec<_>>()),
                                    "namespaces": parsed.as_ref().map(|g| g.namespaces.clone()),
                                    "hash": row.hash,
                                })
                            })
                            .collect(),
                    }),
                    Err(e) => Ok(CalResultPayload::Unsupported {
                        statement: "show_grants".into(),
                        message: format!("SHOW GRANTS failed: {e}"),
                    }),
                }
            }
        }
    }

    // -----------------------------------------------------------------------
    // RUN (saved query execution)
    // -----------------------------------------------------------------------

    fn execute_run_query(
        &self,
        run: &super::ast::RunQueryStmt,
        store: &dyn CalStoreFacade,
        outer_query: &CalQuery,
        exec_warnings: &mut Vec<String>,
    ) -> std::result::Result<CalResultPayload, CalError> {
        // 1. Load saved query from store.
        let entry = store
            .get_query(&run.name)
            .ok_or_else(|| CalError::QueryNotFound {
                name: run.name.clone(),
                span: run.span,
            })?;

        // 2. Substitute parameters into body.
        let mut body = entry.body.clone();

        // Build a map of available bindings: call-site bindings override defaults.
        let mut param_values: HashMap<String, String> = HashMap::new();

        // Apply defaults first.
        for p in &entry.params {
            if let Some(ref default) = p.default {
                param_values.insert(p.name.clone(), value_to_cal_literal(default));
            }
        }

        // Apply call-site bindings (override defaults).
        for (name, value) in &run.bindings {
            param_values.insert(name.clone(), value_to_cal_literal(value));
        }

        // Check for missing required parameters.
        for p in &entry.params {
            if p.default.is_none() && !param_values.contains_key(&p.name) {
                return Err(CalError::MissingQueryParam {
                    name: p.name.clone(),
                    query: run.name.clone(),
                    span: run.span,
                });
            }
        }

        // Warn on unused parameters (supplied but not in query definition).
        let declared_names: std::collections::HashSet<&str> =
            entry.params.iter().map(|p| p.name.as_str()).collect();
        for (name, _) in &run.bindings {
            if !declared_names.contains(name.as_str()) {
                exec_warnings.push(format!(
                    "CAL-W006: Parameter \"${}\" supplied but not used in query \"{}\"",
                    name, run.name
                ));
            }
        }

        // Substitute $param references in body text.
        for (name, literal) in &param_values {
            body = body.replace(&format!("${}", name), literal);
        }

        // 3. Parse the substituted body.
        let parsed = super::parser::parse(&body).map_err(|e| CalError::InvalidQueryBody {
            detail: e.to_string(),
            span: run.span,
        })?;

        // 3a. Re-validate read-only at execution. The DEFINE-time scan is
        // skipped for $-parameterized bodies, so this is the precise gate: a
        // saved-query body that resolves to ADD/SUPERSEDE/FORGET/DROP/DEFINE/RUN
        // is refused here regardless of the destructive-ops gate.
        let span = run.span.unwrap_or_else(Span::zero);
        super::parser::check_read_only_statement(&parsed.statement, &span)?;

        // 4. Merge WITH options: saved query's with + outer query's with (outer wins).
        let mut merged_query = parsed;

        // If the outer query (RUN site) has WITH options, merge them.
        for opt in &outer_query.with_options {
            // Check if this option already exists in the merged query.
            let existing_idx = merged_query
                .with_options
                .iter()
                .position(|o| std::mem::discriminant(o) == std::mem::discriminant(opt));
            if let Some(idx) = existing_idx {
                // Call-site wins on conflict.
                merged_query.with_options[idx] = opt.clone();
            } else {
                merged_query.with_options.push(opt.clone());
            }
        }

        // If the outer query has a FORMAT, it replaces the body's FORMAT.
        if outer_query.format.is_some() {
            merged_query.format = outer_query.format.clone();
        }

        // If the outer query has pipeline stages, append them.
        for stage in &outer_query.pipeline {
            merged_query.pipeline.push(stage.clone());
        }

        // If the outer query has user_vars, merge them.
        for (k, v) in &outer_query.user_vars {
            merged_query
                .user_vars
                .entry(k.clone())
                .or_insert_with(|| v.clone());
        }

        // 5. Execute the parsed+merged query through the normal path.
        let result =
            self.execute_statement(&merged_query.statement, store, &merged_query, exec_warnings)?;

        // 6. Record last_run_at timestamp on successful execution.
        //    Best-effort — a persistence failure here should not fail the query.
        let _ = store.update_query_last_run(&run.name);

        Ok(result)
    }

    // -----------------------------------------------------------------------
    // RECALL
    // -----------------------------------------------------------------------

    fn execute_recall(
        &self,
        recall: &RecallStmt,
        store: &dyn CalStoreFacade,
        query: &CalQuery,
        exec_warnings: &mut Vec<String>,
    ) -> std::result::Result<CalResultPayload, CalError> {
        let mut params = RecallParams::default();

        // Grain type filter.
        if let Some(gt) = recall.grain_type.to_grain_type() {
            params.grain_type = Some(gt);
        }

        // ABOUT clause → free-text BM25 query.
        if let Some(ref about) = recall.about {
            params.query = Some(about.text.clone());
        }

        // LIKE clause → textual similarity; in Areev this rides the same
        // BM25 leg (parser already rejects ABOUT+LIKE together).
        if params.query.is_none() {
            if let Some(ref like) = recall.like {
                params.query = Some(like.text.clone());
            }
        }

        // WHERE clause → structured filter fields.
        if let Some(ref where_clause) = recall.where_clause {
            self.apply_where_clause(&where_clause.condition, &mut params, exec_warnings, &query.let_values)?;
        }

        // Consent grains index subject_did in the hexastore, so when the user
        // queries `WHERE subject = "alice"`, also search for "did:alice" to
        // match both plain and DID-prefixed storage formats.
        if recall.grain_type == GrainTypePlural::Consents {
            let expand_did = |value: &str| -> Vec<String> {
                let mut variants = vec![value.to_string()];
                if !value.starts_with("did:") {
                    variants.push(format!("did:{}", value));
                }
                variants
            };

            if let Some(ref subj) = params.subject.take() {
                let expanded = expand_did(subj);
                match params.subject_in.as_mut() {
                    Some(existing) => existing.extend(expanded),
                    None => params.subject_in = Some(expanded),
                }
            }
            if let Some(ref existing) = params.subject_in.clone() {
                // Dedup while preserving order.
                let mut seen = std::collections::HashSet::new();
                let deduped: Vec<String> = existing
                    .iter()
                    .flat_map(|s| expand_did(s))
                    .filter(|s| seen.insert(s.clone()))
                    .collect();
                params.subject_in = Some(deduped);
            }
        }

        // SINCE + optional UNTIL → temporal expression.
        match (&recall.since, &recall.until) {
            (Some(since), Some(until)) => {
                // SINCE "start" UNTIL "end" → combine into a range expression.
                params.temporal_expr = Some(format!(
                    "between {} and {}",
                    since.expression, until.expression
                ));
            }
            (Some(since), None) => {
                params.temporal_expr = Some(since.expression.clone());
            }
            (None, Some(until)) => {
                params.temporal_expr = Some(format!("before {}", until.expression));
            }
            (None, None) => {}
        }

        // BETWEEN clause → temporal range expression.
        if let Some(ref between) = recall.between {
            params.temporal_expr = Some(format!("between {} and {}", between.start, between.end));
        }

        // RECENT n → limit + implicit created_at DESC ordering.
        if let Some(ref recent) = recall.recent {
            params.limit = Some(recent.count.min(self.config.max_limit) as usize);
        }

        // Inline LIMIT (overrides RECENT if both present — parser prevents that).
        if let Some(limit) = recall.limit {
            params.limit = Some(limit.min(self.config.max_limit) as usize);
        }

        // Apply default limit if still unset.
        if params.limit.is_none() {
            params.limit = Some(self.config.default_limit as usize);
        }

        // ── CONTRADICTIONS widens the candidate scan ─────────────────────
        //
        // The clause filters *after* recall, so whatever LIMIT bounded the
        // recall also bounds which forks can be seen. Left alone, the default
        // limit would make "nothing is contested" mean "nothing among the
        // newest 50" — the exact false all-clear this clause exists to
        // prevent. So scan as wide as CAL allows and re-apply the caller's
        // LIMIT to the *contested* grains afterwards: LIMIT bounds the answer,
        // not the search for it.
        let contradictions_limit = if recall.contradictions.is_some() {
            params.limit.replace(self.config.max_limit as usize)
        } else {
            None
        };

        // WITH options.
        self.apply_with_options(&query.with_options, &mut params)?;

        // WITH exhaustive requires an ABOUT clause for semantic search.
        if params.exhaustive.is_some() && recall.about.is_none() {
            return Err(CalError::UnexpectedToken {
                expected: "ABOUT clause (WITH exhaustive requires a semantic search query)".into(),
                found: "no ABOUT clause".into(),
                span: recall.span,
                suggestion: Some(
                    "Add an ABOUT clause: RECALL facts ABOUT \"...\" WITH exhaustive".into(),
                ),
            });
        }

        // Namespace and user_id overrides from config (capability-scoped auth).
        if let Some(ref ns) = self.config.namespace_override {
            params.namespace = Some(ns.clone());
            // The pin wins over EVERY caller-supplied scope: a surviving
            // `namespace IN (…)` set (or a prefix pattern inside one) would
            // let a pinned session read outside its tenant.
            params.namespaces = None;
        } else if params.namespace.is_none() && params.namespaces.is_none() {
            // The session default fills in only when the query named NO scope
            // at all — an explicit `namespace IN (…)` set is already a scope.
            if let Some(ns) = store.default_namespace() {
                params.namespace = Some(ns.to_string());
            }
        }

        if let Some(ref uid) = self.config.user_id_override {
            params.user_id = Some(uid.clone());
        }

        // Execute via the facade.
        let hits = store
            .recall(&params)
            .map_err(|e| map_store_err(e, recall.span))?;

        // A recall that came back exactly full was cut off by the limit, so
        // anything CONTRADICTIONS says about grains beyond it is unknown.
        let scan_was_bounded = params.limit.is_some_and(|l| hits.len() >= l);

        let mut grains = hits_to_grain_results(&hits);

        // Tag grains from deterministic recalls (no ABOUT) so post-merge
        // score-based filters (e.g. WITH min_score in ASSEMBLE) skip them.
        if recall.about.is_none() {
            for g in &mut grains {
                g.is_deterministic = true;
            }
        }

        // ── WI-1.6: Post-retrieval filtering for type-specific fields ────
        //
        // Extract type-specific conditions from the WHERE clause and apply
        // them as post-filters on the grain result set. These fields
        // (e.g., tool_name on tools, goal_state on goals) are not part
        // of RecallParams and must be filtered after retrieval.
        if let Some(ref where_clause) = recall.where_clause {
            let type_conditions = extract_type_specific_conditions(&where_clause.condition);
            let set_conditions = extract_type_specific_set_conditions(&where_clause.condition);

            // Validate fields against the target grain type.
            let referenced_fields = type_conditions
                .iter()
                .map(|(f, _, _)| f.as_str())
                .chain(set_conditions.iter().map(|c| c.field.as_str()));
            for field in referenced_fields {
                let specific = type_specific_fields(&recall.grain_type);
                if recall.grain_type != GrainTypePlural::All
                    && !specific.contains(&field)
                    && !COMMON_FIELDS.contains(&field)
                {
                    let suggestion = suggest_field(field, &recall.grain_type);
                    return Err(CalError::FieldNotOnGrainType {
                        field: field.to_string(),
                        grain_type: recall.grain_type.as_str().to_string(),
                        span: recall.span,
                        suggestion,
                    });
                }
            }

            if !type_conditions.is_empty() || !set_conditions.is_empty() {
                grains.retain(|grain| {
                    type_conditions
                        .iter()
                        .all(|(field, comp, val)| grain_matches_condition(grain, field, comp, val))
                        && set_conditions
                            .iter()
                            .all(|c| grain_matches_set_condition(grain, c))
                });
            }
        }

        // ── WITH dedup(<field>) on RECALL ────────────────────────────────
        //
        // §5 introduces the WITH table with "WITH options tune recall
        // behavior", but `dedup` was only ever implemented on the ASSEMBLE
        // merge path — on a RECALL it parsed, ran, and changed nothing. Keep
        // the first grain per distinct value of the field, which is the same
        // rule ASSEMBLE applies, and preserves recall order (so the
        // highest-ranked representative of each value survives).
        for opt in &query.with_options {
            // `WITH dedup` with no field is the similarity-based form; only the
            // per-field spelling is meaningful on a single recall's grains.
            if let WithOption::Dedup { field: Some(field) } = opt {
                let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
                grains.retain(|g| match json_field(&g.fields, field) {
                    Some(v) => seen.insert(value_dedup_key(v)),
                    // A grain that does not carry the field cannot be a
                    // duplicate on it; dropping those would silently narrow the
                    // result to whichever grain types happen to have it.
                    None => true,
                });
            }
        }

        // `WITH score_breakdown` asks for per-leg scoring detail this recall
        // path does not produce: the facade returns grains, not per-leg scores,
        // so every structural hit carries the sentinel 1.0. §5's own rule is
        // that an option needing an unavailable backend "returns an honest
        // error rather than silently degrading" — a warning is the
        // non-breaking form of honest, since the option has shipped for
        // several releases and callers pass it today.
        if params.score_breakdown == Some(true) {
            exec_warnings.push(
                super::errors::CalWarning::WithOptionInert {
                    option: "score_breakdown",
                    statement: "RECALL",
                    why: "this recall path returns fused grains, not per-leg scores, so \
                          structural hits all carry the sentinel score 1.0",
                }
                .to_string(),
            );
        }

        // ── CONTRADICTIONS — restrict to grains that are contested ───────
        //
        // A fork is `(ns, subject, relation)` with more than one live head, which
        // happens when two writers diverge and then sync: immutability keeps both
        // tips, and recall serves a deterministically-elected provisional head.
        // Plain recall therefore answers with a value that *looks* settled. This
        // clause is how an agent asks the opposite question — "what do I hold
        // that is still disputed?" — without an operator running `areev forks`.
        //
        // Runs last, after every other filter, so `CONTRADICTIONS` composes with
        // ABOUT/WHERE/SINCE rather than overriding them — and before LIMIT, so
        // the limit bounds the contested grains rather than the search.
        if let Some(ref clause) = recall.contradictions {
            // `CONTRADICTIONS OF (sub-query)` narrows the fork set to the keys
            // that sub-query selected. Execute it with the pipeline and FORMAT
            // stripped: those belong to the enclosing statement, and letting them
            // reach the inner query is the leak that bit nested ASSEMBLE.
            let scope = match clause.inner {
                Some(ref inner) => {
                    let inner_query = CalQuery {
                        statement: (**inner).clone(),
                        pipeline: Vec::new(),
                        format: None,
                        ..query.clone()
                    };
                    let payload = self.execute_statement(
                        &inner_query.statement,
                        store,
                        &inner_query,
                        exec_warnings,
                    )?;
                    match payload {
                        CalResultPayload::Grains { ref grains, .. } => Some(
                            contradiction_scope_keys(grains, params.namespace.as_deref()),
                        ),
                        // A sub-query that yields no grain set (COUNT, EXISTS, a
                        // formatted string) selects no keys to scope by. Treat it
                        // as an empty scope rather than silently ignoring the
                        // tail, so the query cannot over-report.
                        _ => {
                            exec_warnings.push(
                                "CONTRADICTIONS OF (...) sub-query returned no grains; \
                                 no keys to scope by"
                                    .into(),
                            );
                            Some(std::collections::HashSet::new())
                        }
                    }
                }
                None => None,
            };
            apply_fork_status(&mut grains, store, true, scope.as_ref(), exec_warnings);

            // Even a max_limit scan can be cut off. Saying so is the whole
            // point: an agent may act on "nothing is contested", so it must
            // learn when that answer only covers part of the memory.
            if scan_was_bounded {
                exec_warnings.push(
                    super::errors::CalWarning::ContradictionScanBounded {
                        scanned: self.config.max_limit as usize,
                    }
                    .to_string(),
                );
            }

            // The caller's LIMIT applies to the contested grains, not to the
            // scan that found them (see where `contradictions_limit` is taken).
            if let Some(limit) = contradictions_limit {
                grains.truncate(limit);
            }
        } else if params.detect_contradictions == Some(true) {
            // `WITH contradiction_detection` — annotate, don't filter. The agent
            // gets its normal context back, with disputed grains marked.
            apply_fork_status(&mut grains, store, false, None, exec_warnings);
        }

        let count = grains.len();

        // FORMAT clause — render into the requested format.
        // Skip early rendering when pipeline stages exist; the main execute()
        // path applies FORMAT after pipeline, which is needed for GROUP BY to
        // propagate its metadata to the renderer.
        if query.pipeline.is_empty() {
            if let Some(ref fmt) = query.format {
                return apply_format_clause_to_grains(
                    &grains,
                    fmt,
                    None,
                    RenderInputs {
                        user_vars: &query.user_vars,
                        store,
                        disclosure: disclosure_of(&query.with_options),
                    },
                    &Default::default(),
                    exec_warnings,
                );
            }
        }

        Ok(CalResultPayload::Grains {
            grains,
            total_available: Some(count),
        })
    }

    // -----------------------------------------------------------------------
    // WHERE clause mapping
    // -----------------------------------------------------------------------

    fn apply_where_clause(
        &self,
        condition: &Condition,
        params: &mut RecallParams,
        warnings: &mut Vec<String>,
        // Resolved LET bindings, so `IN $var` can expand. Empty for statements
        // that carry no LET clause, which is every statement but the two-step
        // pattern.
        let_values: &HashMap<String, Vec<String>>,
    ) -> std::result::Result<(), CalError> {
        match condition {
            Condition::Comparison {
                field,
                comparator,
                value,
                ..
            } => {
                match (field.as_str(), comparator) {
                    ("subject", Comparator::Eq) => {
                        params.subject = Some(value_to_string(value)?);
                    }
                    ("relation", Comparator::Eq) => {
                        params.relation = Some(value_to_string(value)?);
                    }
                    ("object", Comparator::Eq) => {
                        params.object = Some(value_to_string(value)?);
                    }
                    ("namespace", Comparator::Eq) => {
                        // Only apply if not overridden by capability token.
                        if self.config.namespace_override.is_none() {
                            params.namespace = Some(value_to_string(value)?);
                        }
                    }
                    ("user_id", Comparator::Eq) => {
                        // Only apply if not overridden by capability token.
                        if self.config.user_id_override.is_none() {
                            params.user_id = Some(value_to_string(value)?);
                        }
                    }
                    ("confidence", Comparator::Gte) | ("confidence", Comparator::Gt) => {
                        params.confidence_threshold = Some(value_to_f64(value)?);
                    }
                    ("importance", Comparator::Gte) | ("importance", Comparator::Gt) => {
                        params.importance_threshold = Some(value_to_f64(value)?);
                    }
                    ("query", Comparator::Eq) => {
                        params.query = Some(value_to_string(value)?);
                    }
                    ("time", Comparator::Eq) => {
                        params.temporal_expr = Some(value_to_string(value)?);
                    }
                    ("contradicted", Comparator::Eq) => {
                        if let Value::Boolean { value: b } = value {
                            params.include_contradicted = Some(*b);
                        }
                    }
                    ("entity", Comparator::Eq) => {
                        params.entity = Some(value_to_string(value)?);
                    }
                    // `hash` is on the grain envelope, not in `fields`, so it
                    // reached neither the structural filters nor the
                    // post-filter's `fields` lookup: `WHERE hash = "<real>"`
                    // returned the whole result set with a CAL-W010, and
                    // `hash IN (...)` returned nothing. Both are handled as
                    // post-filters now (see `grain_matches_condition`); listing
                    // the field here is what stops the spurious warning.
                    ("hash", Comparator::Eq) | ("hash", Comparator::NotEq) => {}
                    ("scope_path", Comparator::Eq) | ("scope", Comparator::Eq) => {
                        params.scope_path = Some(value_to_string(value)?);
                    }
                    // Type-specific fields (e.g. goal_state, session_id, is_error)
                    // are handled later by extract_type_specific_conditions as post-filters.
                    // Only warn for truly unknown fields.
                    _ => {
                        if !is_known_type_specific_field(field) {
                            warnings.push(format!(
                                "CAL-W010: WHERE field '{}' is not a recognized filter field. Check field name.",
                                field
                            ));
                        }
                    }
                }
                Ok(())
            }

            Condition::And { left, right, .. } => {
                self.apply_where_clause(left, params, warnings, let_values)?;
                self.apply_where_clause(right, params, warnings, let_values)?;
                Ok(())
            }

            Condition::In { field, values, span } => {
                // `IN $var` arrives as a single `Value::Parameter`. Expanding it
                // is what makes `LET $friends = SUBJECTS OF (…)` mean anything;
                // a `filter_map` over string literals dropped it, leaving an
                // empty set that nothing downstream read, so the query returned
                // the whole table. An unbound name is an error rather than an
                // empty set: silently scoping to nothing is as wrong as
                // silently scoping to everything, and the caller can tell the
                // difference only if we say so.
                let mut str_values: Vec<String> = Vec::new();
                for v in values {
                    match v {
                        Value::String { value } | Value::Hash { value } => {
                            str_values.push(value.clone())
                        }
                        Value::Number { value } => str_values.push(value.to_string()),
                        Value::Parameter { name } => match let_values.get(name.as_str()) {
                            Some(bound) => str_values.extend(bound.iter().cloned()),
                            None => {
                                return Err(CalError::UnboundParameter {
                                    name: name.clone(),
                                    span: *span,
                                })
                            }
                        },
                        Value::Array { values: inner } => {
                            for iv in inner {
                                if let Value::String { value } = iv {
                                    str_values.push(value.clone());
                                }
                            }
                        }
                        Value::Boolean { value } => str_values.push(value.to_string()),
                    }
                }
                match field.as_str() {
                    "subject" => params.subject_in = Some(str_values),
                    "relation" => params.relation_in = Some(str_values),
                    "object" => params.object_in = Some(str_values),
                    "tags" => params.tags = Some(str_values),
                    "namespace" => {
                        // Multi-namespace scope. The facade consumes the SET
                        // (issue #19: setting only a "primary" first value
                        // meant every other member was silently dropped);
                        // `params.namespace` stays untouched so a session
                        // default cannot shadow an explicit IN scope.
                        params.namespaces = Some(str_values);
                    }
                    _ => {
                        // Unknown field IN — silently ignore (CAL spec allows
                        // domain-specific fields that may not map to RecallParams).
                    }
                }
                Ok(())
            }

            Condition::NotIn { field, values, .. } => {
                if field == "tags" {
                    let tag_strs: Vec<String> = values
                        .iter()
                        .filter_map(|v| match v {
                            Value::String { value } => Some(value.clone()),
                            _ => None,
                        })
                        .collect();
                    params.exclude_tags = Some(tag_strs);
                }
                Ok(())
            }

            Condition::Contains { field, value, .. } => {
                // Map CONTAINS to substring search for subject/object,
                // or to BM25 text query for other searchable fields.
                match field.as_str() {
                    "subject" => {
                        params.subject_contains = Some(value.clone());
                    }
                    "object" => {
                        params.object_contains = Some(value.clone());
                    }
                    "content" | "summary" if params.query.is_none() => {
                        params.query = Some(value.clone());
                    }
                    _ => {}
                }
                Ok(())
            }

            Condition::Or { left, .. } => {
                // OR is not directly representable in RecallParams.
                // Apply the left branch only and emit a warning.
                self.apply_where_clause(left, params, warnings, let_values)?;
                warnings.push(
                    "OR conditions are partially supported in Phase 1: only the left branch is applied. Use separate queries with UNION for full OR semantics.".to_string()
                );
                Ok(())
            }

            // IS CATEGORY expansion — desugar to relation_in using the mg: vocabulary.
            Condition::IsCategory {
                field, category, ..
            } => {
                if field == "relation" {
                    let relations = super::relations::expand_category(category);
                    if relations.is_empty() {
                        warnings.push(format!(
                            "Unknown relation category '{}'; no relations expanded.",
                            category
                        ));
                    } else {
                        // expand_category returns both mg: and plain variants
                        // (minimum 2 per category), so always use relation_in.
                        params.relation_in =
                            Some(relations.iter().map(|r| r.to_string()).collect());
                    }
                } else {
                    warnings.push(format!(
                        "CAL-W008: IS {} used on field '{}' — IS CATEGORY is only meaningful on the 'relation' field; this condition was ignored.",
                        category, field
                    ));
                }
                Ok(())
            }

            // NOT, IsNull, IsNotNull, StartsWith — treated as pass-through.
            // The engine will do a broader recall; post-filters can be added
            // in Phase 2 when we have a richer filter DSL.
            _ => Ok(()),
        }
    }

    // -----------------------------------------------------------------------
    // WITH options
    // -----------------------------------------------------------------------

    fn apply_with_options(
        &self,
        options: &[WithOption],
        params: &mut RecallParams,
    ) -> std::result::Result<(), CalError> {
        for opt in options {
            match opt {
                WithOption::Superseded => {
                    params.exclude_superseded = Some(false);
                }
                WithOption::ScoreBreakdown => {
                    params.score_breakdown = Some(true);
                }
                WithOption::Explanation => {
                    params.explanation = Some(true);
                }
                WithOption::ContradictionDetection => {
                    params.detect_contradictions = Some(true);
                }
                WithOption::Diversity { lambda } => {
                    params.diversity = Some(if let Some(l) = lambda {
                        DiversityConfig::mmr_with_lambda(*l as f32)
                    } else {
                        DiversityConfig::mmr()
                    });
                }

                // -- Previously parsed but ignored, now wired ----------------
                WithOption::Provenance => {
                    params.record_provenance = Some(true);
                }
                WithOption::Dedup { field: _ } => {
                    // Bug 5: argument is now a field name (spec EBNF); the
                    // underlying recall engine still uses similarity-based
                    // dedup with its default threshold. Per-field dedup is
                    // not yet wired through `RecallParams`.
                    params.deduplicate = Some(true);
                }
                // -- Recall feature flags (parity with HTTP/gRPC/MCP/A2A) ----
                // Rerank is a runtime seam in Areev (an installed
                // `RerankBackend`, not a cargo feature): always translate the
                // option; the facade no-ops it when no backend is installed.
                WithOption::Rerank { ref model } => {
                    let mut cfg = crate::store_types::RerankConfig::default();
                    if let Some(m) = model {
                        cfg.model = Some(m.clone());
                    }
                    params.rerank = Some(cfg);
                }
                // LLM-dependent refinements (Tier-3): Areev takes no LLM
                // dependency by policy, so these are honestly unavailable
                // rather than silently ignored. They live in the host's loop.
                WithOption::LlmRerank { .. } => {
                    return Err(CalError::LlmFeatureUnavailable { feature: "llm_rerank".into() });
                }
                WithOption::Hyde => {
                    return Err(CalError::LlmFeatureUnavailable { feature: "hyde".into() });
                }
                WithOption::QueryExpansion => {
                    params.query_expansion = Some(true);
                }
                WithOption::QueryDecompose => {
                    params.query_decompose = Some(true);
                }
                WithOption::ConflictResolution => {
                    params.conflict_resolution = Some(true);
                }
                WithOption::IncludeSources => {
                    params.include_sources = Some(true);
                }
                WithOption::AnnotateRelativeTime => {
                    params.annotate_relative_time = Some(true);
                }
                WithOption::RecencyWeight { weight } => {
                    params.recency_weight = Some(*weight);
                }
                WithOption::MinScore { score } => {
                    params.min_score = Some(*score);
                }
                WithOption::MultiHop { hops } => {
                    params.multi_hop = Some((*hops as u8).clamp(1, 3));
                }
                WithOption::SessionAffinity { boost } => {
                    params.session_affinity_boost = Some(boost.clamp(0.0, 1.0));
                }
                WithOption::SubjectAffinity { boost } => {
                    params.subject_affinity_boost = Some(boost.clamp(0.0, 1.0));
                }
                WithOption::SessionCoverage { min_per_ns } => {
                    params.min_per_namespace = Some((*min_per_ns as usize).clamp(1, 10));
                }
                WithOption::MaxNamespaces { max } => {
                    params.max_namespaces = Some((*max as usize).clamp(1, 100));
                }
                WithOption::Exhaustive { max_rounds } => {
                    let mut config = crate::store_types::ExhaustiveConfig::default();
                    if let Some(rounds) = max_rounds {
                        config.max_rounds = (*rounds as u8).clamp(1, 5);
                    }
                    config.validate();
                    params.exhaustive = Some(config);
                }
                WithOption::SessionCensus {
                    min_per_session,
                    min_score,
                } => {
                    let mut config = crate::store_types::SessionCensusConfig::default();
                    if let Some(mps) = min_per_session {
                        config.min_per_session = (*mps as u8).clamp(1, 10);
                    }
                    if let Some(ms) = min_score {
                        config.min_score = ms.clamp(0.0, 1.0);
                    }
                    config.validate();
                    params.session_census = Some(config);
                }
                WithOption::AggregationIntent => {
                    params.aggregation_intent = Some(true);
                }

                WithOption::PreferenceEnrichment => {
                    params.preference_enrichment = Some(true);
                }

                // OMS §4 WITH options accepted at parse time; runtime
                // semantics not yet wired into RecallParams. The parser
                // emits a CAL-W004 UnknownExtensionOption warning at
                // parse time (see `parse_with_option` arms) so the
                // caller knows the option is recognized syntactically
                // but is currently a no-op at the executor.
                WithOption::ProgressiveDisclosure { .. } => {}
                WithOption::Consistency { .. } => {}
                WithOption::Locale { .. } => {}
                WithOption::Cache { .. } => {}

            }
        }
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Post-merge WITH options for multi-source ASSEMBLE
    // -----------------------------------------------------------------------

    /// Apply query-level WITH options to an already-merged ASSEMBLE result.
    ///
    /// Multi-source ASSEMBLE delegates per-source retrieval to the AssembleEngine,
    /// which handles budget allocation, hash-dedup, and grain capping.  However,
    /// query-level WITH options (parsed from the CAL query's trailing WITH clause)
    /// were previously never applied to the merged result set.  This method
    /// closes that gap by applying **post-merge** operations.
    ///
    /// # Post-merge vs per-source options
    ///
    /// | Category        | Options                                                      |
    /// |-----------------|--------------------------------------------------------------|
    /// | Post-merge      | `conflict_resolution`, `dedup`, `min_score`, `rerank`,      |
    /// |                 | `llm_rerank`, `diversity`                                    |
    /// | Per-source only | `query_expansion`, `hyde`, `temporal_field`, `recency_weight`|
    /// | Both            | Per-source retrieval already applies all WITH options;       |
    /// |                 | this method re-applies post-merge operations on the combined |
    /// |                 | result set so cross-source conflicts/duplicates are handled. |
    ///
    /// `rerank` and `llm_rerank` are applied both per-source (each sub-RECALL
    /// is reranked individually) and post-merge (the merged set is reranked
    /// against the ASSEMBLE topic via `CalStoreFacade::rerank_passages()`).
    /// Post-merge reranking requires a non-empty `about_text` (the ASSEMBLE
    /// topic); a warning is emitted if the topic is empty.
    #[allow(unused_variables)] // warnings, store, about_text used only with rerank features
    #[allow(clippy::ptr_arg)] // Vec: pushed to inside #[cfg(feature = "rerank"/"llm-rerank")] blocks
    fn apply_assemble_post_merge_options(
        &self,
        payload: CalResultPayload,
        options: &[WithOption],
        warnings: &mut Vec<String>,
        store: &dyn CalStoreFacade,
        about_text: &str,
    ) -> std::result::Result<CalResultPayload, CalError> {
        // Only Assembled payloads have grains to post-process.
        let (mut grains, sources, total_tokens, budget_limit, _total_available) = match payload {
            CalResultPayload::Assembled {
                grains,
                sources,
                total_tokens,
                budget_limit,
                total_available,
                ..
            } => (grains, sources, total_tokens, budget_limit, total_available),
            other => return Ok(other),
        };

        for opt in options {
            match opt {
                // ── conflict_resolution ──────────────────────────────────
                // Keep only the newest grain per (subject, relation) when
                // multiple grains across sources conflict (same key,
                // different object).
                WithOption::ConflictResolution => {
                    #[allow(clippy::type_complexity)]
                    let mut groups: HashMap<
                        (String, String),
                        Vec<(i64, usize, String)>,
                    > = HashMap::new();
                    for (idx, grain) in grains.iter().enumerate() {
                        let subj = grain
                            .fields
                            .get("subject")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                        let rel = grain
                            .fields
                            .get("relation")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                        if subj.is_empty() && rel.is_empty() {
                            continue;
                        }
                        let obj = grain
                            .fields
                            .get("object")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                        let created = grain
                            .fields
                            .get("created_at")
                            .and_then(|v| v.as_i64())
                            .unwrap_or(0);
                        groups
                            .entry((subj, rel))
                            .or_default()
                            .push((created, idx, obj));
                    }
                    let mut remove_indices: std::collections::HashSet<usize> =
                        std::collections::HashSet::new();
                    for members in groups.values() {
                        if members.len() < 2 {
                            continue;
                        }
                        // Check if objects differ within this group.
                        let has_diff = members.windows(2).any(|w| w[0].2 != w[1].2);
                        if !has_diff {
                            continue;
                        }
                        // Keep the newest, remove the rest.
                        let best_idx = members
                            .iter()
                            .max_by_key(|(ts, _, _)| *ts)
                            .map(|(_, idx, _)| *idx)
                            .unwrap();
                        for (_, idx, _) in members {
                            if *idx != best_idx {
                                remove_indices.insert(*idx);
                            }
                        }
                    }
                    if !remove_indices.is_empty() {
                        let mut idx = 0;
                        grains.retain(|_| {
                            let keep = !remove_indices.contains(&idx);
                            idx += 1;
                            keep
                        });
                    }
                }

                // ── dedup (threshold-based) ──────────────────────────────
                // Cross-source near-duplicate removal.  The AssembleEngine
                // already does hash-based dedup; this applies the softer
                // threshold-based dedup from WITH dedup (similarity threshold).
                // `WITH dedup(<field>)` keeps one grain per distinct value of
                // that field — the same rule the RECALL path applies, so the
                // option means the same thing wherever it is written. Without
                // a field it falls back to the similarity-based form.
                WithOption::Dedup { field: Some(field) } => {
                    let mut seen: std::collections::HashSet<String> =
                        std::collections::HashSet::new();
                    grains.retain(|grain| match json_field(&grain.fields, field) {
                        Some(v) => seen.insert(value_dedup_key(v)),
                        None => true,
                    });
                }
                WithOption::Dedup { field: None } => {
                    let threshold = 0.85_f64;
                    let mut seen_texts: Vec<String> = Vec::new();
                    grains.retain(|grain| {
                        let text = grain_result_text(grain);
                        for prev in &seen_texts {
                            if text_similarity(&text, prev) >= threshold {
                                return false;
                            }
                        }
                        seen_texts.push(text);
                        true
                    });
                }

                // ── min_score ────────────────────────────────────────────
                // Drop grains below the score floor.  Per-source retrieval
                // already applies min_score, but grains may have been rescored
                // or normalised during merge.
                //
                // Deterministic-source grains (RECALL with no ABOUT) carry a
                // structural sentinel score, not a relevance signal — score-
                // based filtering would silently erase entire sources whose
                // selection was governed by PRIORITY/BUDGET, not semantics.
                WithOption::MinScore { score } => {
                    grains.retain(|g| g.is_deterministic || g.score >= *score);
                }

                // ── diversity (score-based) ──────────────────────────────
                // Without vector embeddings we cannot do MMR diversity.
                // Re-sort by score descending so the most relevant grains
                // from each source are interleaved at the top.
                WithOption::Diversity { .. } => {
                    grains.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(Ordering::Equal));
                }

                // ── rerank (cross-encoder) ───────────────────────────────
                // Post-merge cross-source reranking using the ASSEMBLE topic
                // as the rerank query. Delegates to CalStoreFacade::rerank_passages().
                #[cfg(feature = "rerank")]
                WithOption::Rerank { ref model } => {
                    if about_text.is_empty() {
                        warnings.push(
                            "WITH rerank on ASSEMBLE requires a topic — skipping \
                             post-merge reranking"
                                .into(),
                        );
                    } else if !grains.is_empty() {
                        let texts: Vec<String> = grains.iter().map(grain_result_text).collect();
                        let refs: Vec<&str> = texts.iter().map(|s| s.as_str()).collect();
                        let user_id = self.config.user_id_override.as_deref();
                        let perm = store
                            .rerank_passages(
                                about_text,
                                &refs,
                                super::facade::RerankType::CrossEncoder,
                                model.as_deref(),
                                user_id,
                            )
                            .map_err(|e| CalError::BudgetExceeded {
                                detail: format!("post-merge reranking failed: {}", e),
                                span: None,
                            })?;
                        let mut reranked = Vec::with_capacity(grains.len());
                        for &i in &perm {
                            if let Some(g) = grains.get(i) {
                                reranked.push(g.clone());
                            }
                        }
                        grains = reranked;
                    }
                }

                // ── llm_rerank (LLM listwise) ──────────────────────────────
                // Post-merge cross-source reranking via external LLM backend.
                #[cfg(feature = "llm-rerank")]
                WithOption::LlmRerank { ref model } => {
                    if about_text.is_empty() {
                        warnings.push(
                            "WITH llm_rerank on ASSEMBLE requires a topic — skipping \
                             post-merge reranking"
                                .into(),
                        );
                    } else if !grains.is_empty() {
                        let texts: Vec<String> = grains.iter().map(grain_result_text).collect();
                        let refs: Vec<&str> = texts.iter().map(|s| s.as_str()).collect();
                        let user_id = self.config.user_id_override.as_deref();
                        let perm = store
                            .rerank_passages(
                                about_text,
                                &refs,
                                super::facade::RerankType::Llm,
                                model.as_deref(),
                                user_id,
                            )
                            .map_err(|e| CalError::BudgetExceeded {
                                detail: format!("post-merge LLM reranking failed: {}", e),
                                span: None,
                            })?;
                        let mut reranked = Vec::with_capacity(grains.len());
                        for &i in &perm {
                            if let Some(g) = grains.get(i) {
                                reranked.push(g.clone());
                            }
                        }
                        grains = reranked;
                    }
                }

                // All other WITH options either:
                // - Are per-source (query_expansion, hyde, temporal_field, etc.)
                //   and already applied in execute_source via the surrogate query.
                // - Are display options (score_breakdown, explanation, etc.)
                //   already applied per-source.
                // - Are not applicable to ASSEMBLE (summarize, etc.)
                _ => {}
            }
        }

        let count = grains.len();
        Ok(CalResultPayload::Assembled {
            grains,
            sources,
            total_tokens,
            budget_limit,
            progressive: false,
            total_available: Some(count),
        })
    }

    // -----------------------------------------------------------------------
    // EXISTS
    // -----------------------------------------------------------------------

    fn execute_exists(
        &self,
        exists: &ExistsStmt,
        store: &dyn CalStoreFacade,
        exec_warnings: &mut Vec<String>,
        let_values: &HashMap<String, Vec<String>>,
    ) -> std::result::Result<CalResultPayload, CalError> {
        // EXISTS by grain type and WHERE clause: recall with limit=1 and check count.
        let mut params = RecallParams::default();

        if let Some(gt) = exists.grain_type.to_grain_type() {
            params.grain_type = Some(gt);
        }

        if let Some(ref about) = exists.about {
            params.query = Some(about.text.clone());
        }

        if let Some(ref where_clause) = exists.where_clause {
            self.apply_where_clause(&where_clause.condition, &mut params, exec_warnings, let_values)?;
        }

        // Special case: if WHERE contains a hash comparison, look up directly.
        let hash_str =
            extract_hash_from_condition(exists.where_clause.as_ref().map(|w| &w.condition));
        if let Some(ref hs) = hash_str {
            let hash = Hash::from_hex(hs).map_err(|_| CalError::InvalidHash {
                found: hs.clone(),
                span: exists.span,
            })?;
            let found = store
                .exists(&hash)
                .map_err(|e| map_store_err(e, exists.span))?;
            return Ok(CalResultPayload::Exists {
                exists: found,
                hash: hs.clone(),
            });
        }

        // General case: recall with limit 1 to detect presence.
        params.limit = Some(1);

        // Apply capability overrides (the pin also clears any IN-set scope).
        if let Some(ref ns) = self.config.namespace_override {
            params.namespace = Some(ns.clone());
            params.namespaces = None;
        }
        if let Some(ref uid) = self.config.user_id_override {
            params.user_id = Some(uid.clone());
        }

        let hits = store
            .recall(&params)
            .map_err(|e| map_store_err(e, exists.span))?;

        let found = !hits.is_empty();
        let hash_out = hits.first().map(|h| h.hash.to_hex()).unwrap_or_default();

        Ok(CalResultPayload::Exists {
            exists: found,
            hash: hash_out,
        })
    }

    // -----------------------------------------------------------------------
    // HISTORY
    // -----------------------------------------------------------------------

    fn execute_history(
        &self,
        history: &HistoryStmt,
        store: &dyn CalStoreFacade,
        exec_warnings: &mut Vec<String>,
        let_values: &HashMap<String, Vec<String>>,
    ) -> std::result::Result<CalResultPayload, CalError> {
        // ── HISTORY DIFF path ──────────────────────────────────────────
        //
        // When `diff_target` is set, compare two grains field-by-field
        // and return a `CalResultPayload::Diff`.
        if let Some(ref diff_hash_str) = history.diff_target {
            let source_hash = Hash::from_hex(&history.hash).map_err(|_| CalError::InvalidHash {
                found: history.hash.clone(),
                span: history.span,
            })?;
            let target_hash = Hash::from_hex(diff_hash_str).map_err(|_| CalError::InvalidHash {
                found: diff_hash_str.clone(),
                span: history.span,
            })?;

            // C2-07: If either grain cannot be retrieved (e.g. user was
            // erased, or grain does not exist), return an error.
            let grain_a = store.get(&source_hash).map_err(|e| match e {
                // Map "no such grain" to HashNotFound (CAL-E091) — distinct
                // from BudgetExceeded so callers can differentiate.
                AreevError::NotFound(_) => CalError::HashNotFound {
                    hash: history.hash.clone(),
                    span: history.span,
                },
                AreevError::CryptoError(_) => CalError::CryptoError {
                    detail: format!("DIFF source grain decrypt failed: {}", e),
                    span: history.span,
                },
                other => CalError::BudgetExceeded {
                    detail: format!("DIFF source grain error: {}", other),
                    span: history.span,
                },
            })?;
            let grain_b = store.get(&target_hash).map_err(|e| match e {
                // Mirror the DIFF source mapping above.
                AreevError::NotFound(_) => CalError::HashNotFound {
                    hash: diff_hash_str.clone(),
                    span: history.span,
                },
                AreevError::CryptoError(_) => CalError::CryptoError {
                    detail: format!("DIFF target grain decrypt failed: {}", e),
                    span: history.span,
                },
                other => CalError::BudgetExceeded {
                    detail: format!("DIFF target grain error: {}", other),
                    span: history.span,
                },
            })?;

            // CAL-W005: Warn if subject+relation differ between grains.
            let sub_a = grain_a.get_str("subject").unwrap_or("");
            let sub_b = grain_b.get_str("subject").unwrap_or("");
            let rel_a = grain_a.get_str("relation").unwrap_or("");
            let rel_b = grain_b.get_str("relation").unwrap_or("");
            if sub_a != sub_b || rel_a != rel_b {
                exec_warnings.push(
                    "CAL-W005: DIFF targets have different subject/relation — diff may not be meaningful".to_string()
                );
            }

            let changes = diff_grains(&grain_a, &grain_b);
            return Ok(CalResultPayload::Diff {
                source_hash: history.hash.clone(),
                target_hash: diff_hash_str.clone(),
                changes,
            });
        }

        // ── HISTORY WHERE path ─────────────────────────────────────────
        //
        // When `where_clause` is present (and `hash` is empty), use the
        // WHERE clause to find matching grains via recall, then return
        // their version chains.
        if history.hash.is_empty() {
            if let Some(ref wc) = history.where_clause {
                let mut params = RecallParams::default();
                self.apply_where_clause(&wc.condition, &mut params, exec_warnings, let_values)?;

                // Apply capability overrides (the pin also clears any IN-set scope).
                if let Some(ref ns) = self.config.namespace_override {
                    params.namespace = Some(ns.clone());
                    params.namespaces = None;
                }
                if let Some(ref uid) = self.config.user_id_override {
                    params.user_id = Some(uid.clone());
                }

                // Use a modest limit to find matching grains for history.
                if params.limit.is_none() {
                    params.limit = Some(10);
                }

                let hits = store
                    .recall(&params)
                    .map_err(|e| map_store_err(e, history.span))?;

                // Collect version histories for all matching grains.
                let mut all_versions: Vec<CalVersionResult> = Vec::new();
                for hit in &hits {
                    let ns = hit.grain.get_str("namespace").unwrap_or("");
                    let subj = hit.grain.get_str("subject").unwrap_or("");
                    let rel = hit.grain.get_str("relation").unwrap_or("");

                    if let Ok(entries) = store.get_history(ns, subj, rel) {
                        for v in entries {
                            // Avoid duplicates if multiple grains share
                            // the same (subject, relation) triple.
                            if !all_versions
                                .iter()
                                .any(|existing| existing.hash == v.hash.to_hex())
                            {
                                all_versions.push(CalVersionResult {
                                    hash: v.hash.to_hex(),
                                    object: v.object,
                                    created_at: v.created_at,
                                    confidence: v.confidence,
                                    superseded_by: v
                                        .superseded_by
                                        .map(|h: areev_core::error::Hash| h.to_hex()),
                                });
                            }
                        }
                    }
                }

                return Ok(CalResultPayload::History {
                    versions: all_versions,
                });
            }

            // Empty hash with no WHERE clause is an error.
            return Err(CalError::InvalidHash {
                found: String::new(),
                span: history.span,
            });
        }

        // ── Standard HISTORY path (hash-based) ────────────────────────

        // Parse hash from AST.
        let hash = Hash::from_hex(&history.hash).map_err(|_| CalError::InvalidHash {
            found: history.hash.clone(),
            span: history.span,
        })?;

        // Fetch the grain to extract (namespace, subject, relation).
        let grain = match store.get(&hash) {
            Ok(g) => g,
            Err(AreevError::NotFound(_)) => {
                return Ok(CalResultPayload::Unsupported {
                    statement: "history".into(),
                    message: format!("grain not found: {}", history.hash),
                });
            }
            Err(other) => {
                return Err(map_store_err(other, history.span));
            }
        };

        let namespace = grain.get_str("namespace").unwrap_or("");
        let subject = grain.get_str("subject").unwrap_or("");
        let relation = grain.get_str("relation").unwrap_or("");

        let entries = store
            .get_history(namespace, subject, relation)
            .map_err(|e: areev_core::error::AreevError| map_store_err(e, history.span))?;

        let versions: Vec<CalVersionResult> = entries
            .into_iter()
            .map(|v| CalVersionResult {
                hash: v.hash.to_hex(),
                object: v.object,
                created_at: v.created_at,
                confidence: v.confidence,
                superseded_by: v.superseded_by.map(|h: areev_core::error::Hash| h.to_hex()),
            })
            .collect();

        Ok(CalResultPayload::History { versions })
    }

    // -----------------------------------------------------------------------
    // DESCRIBE
    // -----------------------------------------------------------------------

    fn execute_describe(
        &self,
        describe: &DescribeStmt,
        store: &dyn CalStoreFacade,
    ) -> std::result::Result<CalResultPayload, CalError> {
        let info = match &describe.target {
            DescribeTarget::Schema => serde_json::json!({
                "grain_types": [
                    "fact", "event", "state", "workflow", "tool", "recommendation",
                    "observation", "goal", "reasoning", "consensus", "consent", "skill"
                ],
                "common_fields": [
                    "subject", "relation", "object", "namespace", "user_id",
                    "created_at", "confidence", "importance", "tags",
                    "session_id", "content", "summary"
                ],
                "cal_version": 1,
                "tier1_enabled": self.config.tier1_enabled,
                "max_limit": self.config.max_limit,
                "default_limit": self.config.default_limit,
                "oms_version": "1.2",
                "pipeline_stages": [
                    "SELECT", "ORDER BY", "LIMIT", "OFFSET", "COUNT",
                    "FIRST", "SUBJECTS", "OBJECTS", "HASHES", "GROUP BY", "PROJECT"
                ],
                // What actually changes the result on a RECALL. The list used
                // to advertise `score_breakdown`, which is inert here (there
                // are no per-leg scores to break down) — DESCRIBE naming an
                // option that does nothing is worse than not naming it, since
                // it is what a client introspects to decide what to send.
                "with_options": [
                    "superseded", "explanation", "annotate_relative_time",
                    "conflict_resolution", "dedup", "contradiction_detection",
                    "diversity", "multi_hop", "provenance", "query_expansion",
                    "rerank"
                ]
            }),
            DescribeTarget::GrainType(gt) => {
                let type_name = gt.as_str();
                let specific_fields: &[&str] = match gt {
                    // OMS 1.6 §8.13. Typed and queryable — the reason a trigger
                    // is its own grain rather than an Observation carrying
                    // `int:` keys in a context map, which no query can filter.
                    GrainTypePlural::Triggers => &[
                        "kind",
                        "workflow",
                        "connector",
                        "scope",
                        "enabled",
                        "cron",
                    ],
                    GrainTypePlural::Facts => {
                        &["subject", "relation", "object", "confidence", "session_id"]
                    }
                    GrainTypePlural::Events => &[
                        "session_id",
                        "run_id",
                        "content",
                        "created_at",
                        "role",
                        "parent_message_id",
                        "model_id",
                        "stop_reason",
                    ],
                    // OMS §8.3. `checkpoint_data` never existed as a field, and
                    // `session_id` is Event-only.
                    // OMS 1.5 §8.12. `rec_status` is index-layer — filterable,
                    // never author-written.
                    GrainTypePlural::Recommendations => &[
                        "target_ref",
                        "analyzer",
                        "severity",
                        "dedup_key",
                        "rec_status",
                    ],
                    GrainTypePlural::States => &["context", "plan", "history"],
                    GrainTypePlural::Workflows => &[
                        "name",
                        "nodes",
                        "edges",
                        "bindings",
                        "retries",
                        "trigger",
                        "status",
                        "session_id",
                    ],
                    GrainTypePlural::Tools => &[
                        "tool",
                        "input",
                        "content",
                        "is_error",
                        "duration_ms",
                        "session_id",
                    ],
                    GrainTypePlural::Observations => &["sensor", "value", "unit", "session_id"],
                    GrainTypePlural::Goals => &[
                        "title",
                        "description",
                        "priority",
                        "status",
                        "parent_hash",
                        "session_id",
                    ],
                    GrainTypePlural::Reasonings => {
                        &["premises", "conclusion", "confidence", "session_id"]
                    }
                    GrainTypePlural::Consensuses => {
                        &["participants", "agreement", "confidence", "session_id"]
                    }
                    GrainTypePlural::Consents => &[
                        "user_id",
                        "scope",
                        "granted",
                        "expires_at",
                        "subject_did",
                        "grantee_did",
                        "session_id",
                    ],
                    GrainTypePlural::Skills => &[
                        "name",
                        "description",
                        "version",
                        "domain",
                        "holder_did",
                        "proficiency",
                        "transferable",
                        "practice_count",
                        "last_practiced_at",
                        "session_id",
                    ],
                    GrainTypePlural::All => &[],
                };
                serde_json::json!({
                    "grain_type": type_name,
                    "specific_fields": specific_fields,
                    // What the write path refuses to build the grain without.
                    // Without this a caller discovers the shape one VAL-E001 at
                    // a time — `skill` asks for `name`, then asks for
                    // `description` — with no way to ask up front.
                    "required_fields": crate::json_build::required_fields(
                        gt.to_grain_type().map(|t| t.as_str()).unwrap_or("")
                    ),
                    "common_fields": [
                        "namespace", "user_id", "created_at", "tags", "importance"
                    ]
                })
            }

            // ── Phase 2 DESCRIBE targets ───────────────────────────────
            DescribeTarget::Capabilities => {
                let caps = store.describe_capabilities();
                serde_json::json!({
                    "cal_version": caps.cal_version,
                    "conformance_level": caps.conformance_level,
                    "supported_statements": caps.supported_statements,
                    "max_sources": caps.max_sources,
                    "max_let_bindings": caps.max_let_bindings,
                    "max_budget_tokens": caps.max_budget_tokens,
                    "tier1_enabled": self.config.tier1_enabled,
                    "oms_version": "1.2"
                })
            }
            DescribeTarget::Server => {
                let caps = store.describe_capabilities();
                serde_json::json!({
                    "name": "areev",
                    "version": env!("CARGO_PKG_VERSION"),
                    "cal_version": caps.cal_version,
                    "conformance_level": caps.conformance_level,
                    "oms_version": "1.2",
                    "build_features": build_features_list()
                })
            }
            DescribeTarget::Fields(opt_gt) => {
                let gt_engine = opt_gt.as_ref().and_then(|g| g.to_grain_type());
                let field_infos = store.describe_fields(gt_engine);

                // If the facade returns data, use it; otherwise fall back
                // to the static field table for backward compatibility.
                if !field_infos.is_empty() {
                    let fields_json: Vec<serde_json::Value> = field_infos
                        .iter()
                        .map(|fi| {
                            serde_json::json!({
                                "name": fi.name,
                                "type": fi.field_type,
                                "filterable": fi.filterable,
                                "sortable": fi.sortable,
                            })
                        })
                        .collect();
                    if let Some(gt) = opt_gt {
                        serde_json::json!({
                            "grain_type": gt.as_str(),
                            "fields": fields_json
                        })
                    } else {
                        serde_json::json!({
                            "fields": fields_json
                        })
                    }
                } else {
                    // Static fallback (same as Phase 1).
                    let common = serde_json::json!([
                        {"name": "subject", "type": "string", "filterable": true, "sortable": true},
                        {"name": "relation", "type": "string", "filterable": true, "sortable": true},
                        {"name": "object", "type": "string", "filterable": true, "sortable": false},
                        {"name": "namespace", "type": "string", "filterable": true, "sortable": true},
                        {"name": "user_id", "type": "string", "filterable": true, "sortable": true},
                        {"name": "created_at", "type": "timestamp", "filterable": true, "sortable": true},
                        {"name": "confidence", "type": "number", "filterable": true, "sortable": true},
                        {"name": "importance", "type": "number", "filterable": true, "sortable": true},
                        {"name": "tags", "type": "array", "filterable": true, "sortable": false},
                        {"name": "session_id", "type": "string", "filterable": true, "sortable": true}
                    ]);
                    if let Some(gt) = opt_gt {
                        serde_json::json!({
                            "grain_type": gt.as_str(),
                            "fields": common
                        })
                    } else {
                        serde_json::json!({
                            "fields": common
                        })
                    }
                }
            }
            DescribeTarget::Templates => {
                let templates = store.list_templates();
                let template_list: Vec<serde_json::Value> = templates
                    .iter()
                    .map(|t| {
                        serde_json::json!({
                            "name": t.name,
                            "description": t.description,
                            "builtin": t.builtin,
                            "parent": t.parent,
                        })
                    })
                    .collect();
                serde_json::json!({ "templates": template_list })
            }
            DescribeTarget::Queries => {
                let queries = store.list_queries();
                serde_json::json!({
                    "queries": queries,
                })
            }
            DescribeTarget::Query(name) => {
                let entry = store
                    .get_query(name)
                    .ok_or_else(|| CalError::QueryNotFound {
                        name: name.clone(),
                        span: None,
                    })?;
                serde_json::json!({
                    "name": name,
                    "description": entry.description,
                    "builtin": entry.builtin,
                    "params": entry.params.iter().map(|p| {
                        serde_json::json!({
                            "name": p.name,
                            "required": p.default.is_none(),
                            "default": p.default.as_ref().map(|v| match v {
                                super::ast::Value::String { value } => value.clone(),
                                super::ast::Value::Number { value } => value.to_string(),
                                super::ast::Value::Boolean { value } => value.to_string(),
                                other => format!("{}", other),
                            }),
                        })
                    }).collect::<Vec<_>>(),
                    "body": entry.body,
                    "body_size": entry.body.len(),
                })
            }
            // The self-discovery read (CAL 1.3 §8.15): what may this
            // principal do, per namespace. Empty grants = the fail-closed
            // answer, stated rather than implied.
            DescribeTarget::Principal(name) => {
                let rows = store.cal_show_grants(Some(name)).map_err(|e| {
                    let detail = e.to_string();
                    if detail.contains("AUT-E") {
                        CalError::NotAuthorized { detail, span: None }
                    } else {
                        CalError::InvalidQuery { detail, span: None }
                    }
                })?;
                let grants: Vec<serde_json::Value> = rows
                    .iter()
                    .filter_map(|row| {
                        let g =
                            areev_core::authz::Grant::from_object_string(&row.object).ok()?;
                        Some(serde_json::json!({
                            "verbs": g.verbs.iter().map(|v| v.as_str()).collect::<Vec<_>>(),
                            "namespaces": g.namespaces,
                            "hash": row.hash,
                        }))
                    })
                    .collect();
                serde_json::json!({
                    "principal": name,
                    "grants": grants,
                    "note": if grants.is_empty() {
                        "no live grants — this principal can do nothing in a bound session"
                    } else {
                        "rights shown are the live grant heads; owner sessions bypass grants"
                    },
                })
            }
            // The loop reads (CAL 1.3 §8.16) — delegated to the governance
            // host; unwired surfaces say so instead of pretending.
            DescribeTarget::Loop
            | DescribeTarget::Analyzers
            | DescribeTarget::Outcomes
            | DescribeTarget::LoopPolicy => {
                let what = match &describe.target {
                    DescribeTarget::Loop => crate::governance::LoopInfo::Loop,
                    DescribeTarget::Analyzers => crate::governance::LoopInfo::Analyzers,
                    DescribeTarget::Outcomes => crate::governance::LoopInfo::Outcomes,
                    _ => crate::governance::LoopInfo::Policy,
                };
                match &self.governance {
                    None => serde_json::json!({
                        "error": "governance is not wired on this surface — no \
                                  GovernanceHost attached",
                    }),
                    Some(host) => host.describe(store, what).map_err(|e| {
                        let detail = e.to_string();
                        if detail.contains("AUT-E") {
                            CalError::NotAuthorized { detail, span: None }
                        } else {
                            CalError::InvalidQuery { detail, span: None }
                        }
                    })?,
                }
            }
            DescribeTarget::Stats | DescribeTarget::Integrity => {
                let res = if matches!(&describe.target, DescribeTarget::Stats) {
                    store.cal_stats()
                } else {
                    store.cal_verify()
                };
                res.map_err(|e| {
                    let detail = e.to_string();
                    if detail.contains("AUT-E") {
                        CalError::NotAuthorized { detail, span: None }
                    } else {
                        CalError::InvalidQuery { detail, span: None }
                    }
                })?
            }
            DescribeTarget::Grammar => serde_json::json!({
                "grammar": "CAL/1 grammar (simplified BNF)",
                "version": 1,
                "conformance_level": 2,
                "features": [
                    "RECALL", "EXISTS", "ASSEMBLE", "HISTORY", "HISTORY DIFF",
                    "EXPLAIN", "DESCRIBE", "BATCH", "COALESCE",
                    "SET operations (UNION, INTERSECT, EXCEPT)",
                    "Pipeline stages (SELECT, ORDER BY, LIMIT, OFFSET, COUNT, FIRST, SUBJECTS, OBJECTS, HASHES, GROUP BY, PROJECT)",
                    "WITH options (superseded, score_breakdown, explanation, contradiction_detection, diversity)",
                    "LET bindings", "WHERE clause", "ABOUT semantic search",
                    "SINCE / BETWEEN temporal filters", "RECENT shorthand"
                ],
                "url": "https://github.com/AreevAI/areev"
            }),
        };

        Ok(CalResultPayload::Describe { info })
    }

    // -----------------------------------------------------------------------
    // EXPLAIN
    // -----------------------------------------------------------------------

    fn execute_explain(
        &self,
        explain: &ExplainStmt,
        store: &dyn CalStoreFacade,
        query: &CalQuery,
    ) -> std::result::Result<CalResultPayload, CalError> {
        let inner = explain.inner.as_ref();
        let stmt_type = statement_type_name(inner);

        let (grain_type, query_routing, index_usage, mut filters) = match inner {
            CalStatement::Recall(recall) => {
                let gt = recall
                    .grain_type
                    .to_grain_type()
                    .map(|g| g.as_str().to_string());

                let mut index_usage = Vec::new();
                let mut filters = Vec::new();

                // `LIKE` and `ABOUT` are the same free-text leg at execution —
                // both set `RecallParams::query` (see `build_recall_params`).
                // Reporting only `ABOUT` made EXPLAIN describe a `LIKE` recall
                // as a structural `O(n) full scan` with no index, which is the
                // opposite of what it runs. A planner that misreports is worse
                // than one that says nothing.
                let free_text = recall.about.is_some() || recall.like.is_some();

                if free_text {
                    index_usage.push("bm25_fts".to_string());
                }

                if let Some(ref wc) = recall.where_clause {
                    collect_filter_names(&wc.condition, &mut filters);
                    // If WHERE has subject/relation/object: structural index.
                    if filters
                        .iter()
                        .any(|f| f == "subject" || f == "relation" || f == "object")
                    {
                        index_usage.push("hexastore".to_string());
                    }
                }

                if recall.since.is_some() || recall.until.is_some() || recall.between.is_some() {
                    filters.push("temporal".to_string());
                }

                // S-5: list filter names only, not values.
                let routing = if free_text && !index_usage.contains(&"hexastore".to_string()) {
                    "bm25".to_string()
                } else if free_text {
                    "hybrid_rrf".to_string()
                } else {
                    "structural".to_string()
                };

                (gt, routing, index_usage, filters)
            }
            CalStatement::Exists(exists) => {
                let gt = exists
                    .grain_type
                    .to_grain_type()
                    .map(|g| g.as_str().to_string());
                let routing = "structural".to_string();
                let index_usage = vec!["hexastore".to_string()];
                let mut filters = Vec::new();
                if let Some(ref wc) = exists.where_clause {
                    collect_filter_names(&wc.condition, &mut filters);
                }
                (gt, routing, index_usage, filters)
            }
            CalStatement::History(h) => {
                let mut filters = vec!["hash".to_string()];
                if h.diff_target.is_some() {
                    filters.push("diff_target".to_string());
                }
                if h.where_clause.is_some() {
                    filters.push("where_clause".to_string());
                }
                (
                    None,
                    if h.diff_target.is_some() {
                        "diff_comparison".to_string()
                    } else {
                        "entity_latest".to_string()
                    },
                    vec![
                        "entity_latest".to_string(),
                        "supersession_chain".to_string(),
                    ],
                    filters,
                )
            }
            CalStatement::Assemble(assemble) => {
                let mut index_usage = Vec::new();
                let mut filters = Vec::new();

                // Determine source count for multi-source plan.
                let source_count = assemble.sources.as_ref().map_or(1, |s| s.len());
                filters.push(format!("sources: {}", source_count));

                if assemble.budget.is_some() {
                    filters.push("budget_allocation".to_string());
                }
                for opt in &assemble.assemble_with {
                    let super::ast::AssembleWithOption::Dedup { .. } = opt;
                    filters.push("dedup".to_string());
                }
                index_usage.push("bm25_fts".to_string());
                index_usage.push("hexastore".to_string());

                (
                    None,
                    format!("multi_source_assemble({}_sources)", source_count),
                    index_usage,
                    filters,
                )
            }
            CalStatement::Coalesce(coalesce) => {
                let gt = coalesce
                    .grain_type
                    .to_grain_type()
                    .map(|g| g.as_str().to_string());
                let branch_count = if coalesce.branches.is_empty() {
                    1
                } else {
                    coalesce.branches.len()
                };
                let has_else = coalesce.else_branch.is_some();
                let mut filters = vec![format!("fallback_chain: {} branches", branch_count)];
                if has_else {
                    filters.push("else_fallback".to_string());
                }
                (
                    gt,
                    "coalesce_fallback".to_string(),
                    vec!["bm25_fts".to_string(), "hexastore".to_string()],
                    filters,
                )
            }
            CalStatement::Batch(batch) => {
                let stmt_count = batch.statements.len();
                let has_labels = batch.labeled.is_some();
                let has_formats = batch.statements.iter().any(|e| e.format.is_some());
                let has_pipelines = batch.statements.iter().any(|e| !e.pipeline.is_empty());
                let mut filters = vec![format!("parallel_execution: {} statements", stmt_count)];
                if has_labels {
                    filters.push("labeled_results".to_string());
                }
                if has_formats {
                    filters.push("per_entry_format".to_string());
                }
                if has_pipelines {
                    filters.push("per_entry_pipeline".to_string());
                }
                (None, "parallel_batch".to_string(), vec![], filters)
            }
            CalStatement::Describe(_) => (None, "introspection".to_string(), vec![], vec![]),
            CalStatement::ReportSubject(rs) => {
                let mut filters = vec![format!("subject: {}", rs.subject_id)];
                if rs.text_mentions {
                    filters.push("text_mentions".to_string());
                }
                (
                    None,
                    "subject_report".to_string(),
                    vec!["terms_dictionary".to_string(), "fts_postings".to_string()],
                    filters,
                )
            }
            CalStatement::Purge(purge) => {
                let mut filters = vec![];
                if let Some(age) = purge.min_age_days {
                    filters.push(format!("min_age_days: {age}"));
                }
                if let Some(ref ns) = purge.namespace {
                    filters.push(format!("namespace: {ns}"));
                }
                if let Some(lim) = purge.limit {
                    filters.push(format!("batch_limit: {lim}"));
                }
                (
                    None,
                    "destructive_purge_stale".to_string(),
                    vec!["blobs_partition".to_string(), "decay_engine".to_string()],
                    filters,
                )
            }
            CalStatement::Forget(forget) => {
                let target_desc = match &forget.target {
                    super::ast::ForgetTarget::Hash { hash } => format!("hash: {hash}"),
                    super::ast::ForgetTarget::User { user_id } => format!("user: {user_id}"),
                    super::ast::ForgetTarget::Scope { scope } => format!("scope: {scope}"),
                };
                let indexes = match &forget.target {
                    super::ast::ForgetTarget::Hash { .. } => vec!["blobs_partition".to_string()],
                    _ => vec!["blobs_partition".to_string(), "key_store".to_string()],
                };
                (
                    None,
                    "destructive_forget".to_string(),
                    indexes,
                    vec![target_desc],
                )
            }
            CalStatement::DefineTemplate(def) => (
                None,
                "template_registry_write".to_string(),
                vec!["meta_partition".to_string()],
                vec![format!("template_name: {}", def.name)],
            ),
            CalStatement::DropTemplate(drop) => (
                None,
                "template_registry_delete".to_string(),
                vec!["meta_partition".to_string()],
                vec![format!("template_name: {}", drop.name)],
            ),
            CalStatement::DefineQuery(def) => (
                None,
                "query_registry_write".to_string(),
                vec!["meta_partition".to_string()],
                vec![format!("query_name: {}", def.name)],
            ),
            CalStatement::DropQuery(drop) => (
                None,
                "query_registry_delete".to_string(),
                vec!["meta_partition".to_string()],
                vec![format!("query_name: {}", drop.name)],
            ),
            CalStatement::RunQuery(run) => (
                None,
                "saved_query_execute".to_string(),
                vec!["meta_partition".to_string()],
                vec![format!("query_name: {}", run.name)],
            ),
            _ => (None, "unknown".to_string(), vec![], vec![]),
        };

        // Build pipeline step descriptions.
        let mut pipeline_steps = vec![format!("execute_{}", stmt_type)];
        for stage in &query.pipeline {
            pipeline_steps.push(pipeline_stage_name(stage));
        }

        // ── Active policy filters (WI-1.5) ───────────────────────────────
        //
        // Report namespace/user scoping and auth constraints so the caller
        // can understand which policy filters are active for this query.
        let mut policy_filters = Vec::new();

        if self.config.namespace_override.is_some() {
            policy_filters.push("namespace_override (capability token)".to_string());
        } else if store.default_namespace().is_some() {
            policy_filters.push("namespace_scope (session)".to_string());
        }

        if self.config.user_id_override.is_some() {
            policy_filters.push("user_id_override (capability token)".to_string());
        } else if store.active_user().is_some() {
            policy_filters.push("user_id_scope (session)".to_string());
        }

        if !self.config.tier1_enabled {
            policy_filters.push("tier1_disabled (read-only mode)".to_string());
        }

        // Append policy filters to the main filters list.
        filters.extend(policy_filters);

        // Check if the namespace oracle knows the default.
        let ns_context = if self.config.namespace_override.is_some() {
            "(namespace: from capability token)"
        } else if store.default_namespace().is_some() {
            "(namespace: from session)"
        } else {
            "(namespace: not set)"
        };

        let cost = match index_usage.len() {
            0 => "O(n) full scan",
            1 => "O(log n) index lookup",
            _ => "O(log n) hybrid with RRF fusion",
        };

        Ok(CalResultPayload::Explain {
            plan: CalQueryPlan {
                statement_type: stmt_type,
                grain_type,
                query_routing,
                index_usage,
                estimated_cost: format!("{} {}", cost, ns_context),
                filters,
                pipeline: pipeline_steps,
            },
        })
    }

    // -----------------------------------------------------------------------
    // BATCH
    // -----------------------------------------------------------------------

    fn execute_batch(
        &self,
        batch: &BatchStmt,
        store: &dyn CalStoreFacade,
        exec_warnings: &mut Vec<String>,
    ) -> std::result::Result<CalResultPayload, CalError> {
        // ── Phase 2: Labeled BATCH path ──────────────────────────────────
        //
        // When `batch.labeled` is Some, process labeled entries and return
        // results keyed by label. Duplicate labels produce CAL-E034.
        if let Some(ref labeled) = batch.labeled {
            let mut results: HashMap<String, CalResultPayload> = HashMap::new();
            let mut seen_labels: std::collections::HashSet<String> =
                std::collections::HashSet::new();

            for (label, entry) in labeled {
                // Check for duplicate labels (CAL-E034).
                if !seen_labels.insert(label.clone()) {
                    return Err(CalError::AssembleDuplicateLabel {
                        label: label.clone(),
                        span: batch.span,
                    });
                }

                let payload = self.execute_batch_entry(entry, store, exec_warnings)?;
                results.insert(label.clone(), payload);
            }

            return Ok(CalResultPayload::Batch { results });
        }

        // ── Positional BATCH path ────────────────────────────────────────
        let mut results: HashMap<String, CalResultPayload> = HashMap::new();

        for (idx, entry) in batch.statements.iter().enumerate() {
            let payload = self.execute_batch_entry(entry, store, exec_warnings)?;
            results.insert(idx.to_string(), payload);
        }

        Ok(CalResultPayload::Batch { results })
    }

    /// Execute a single `BatchEntry`: run statement, apply pipeline, apply FORMAT.
    fn execute_batch_entry(
        &self,
        entry: &BatchEntry,
        store: &dyn CalStoreFacade,
        exec_warnings: &mut Vec<String>,
    ) -> std::result::Result<CalResultPayload, CalError> {
        let surrogate_query = crate::ast::CalQuery {
            version: crate::ast::CalVersion(1),
            statement: entry.statement.clone(),
            pipeline: entry.pipeline.clone(),
            with_options: entry.with_options.clone(),
            format: entry.format.clone(),
            let_bindings: Vec::new(),
            let_values: HashMap::new(),
            user_vars: entry.user_vars.clone(),
            warnings: Vec::new(),
        };

        let payload = self
            .execute_statement(&entry.statement, store, &surrogate_query, exec_warnings)
            .unwrap_or_else(|e| CalResultPayload::Unsupported {
                statement: statement_type_name(&entry.statement),
                message: e.to_string(),
            });

        // Apply pipeline stages (SELECT, LIMIT, ORDER BY, WHERE, etc.).
        let (payload, grouped_by) = if entry.pipeline.is_empty() {
            (payload, None)
        } else {
            self.apply_pipeline(payload, &entry.pipeline)?
        };

        // Apply FORMAT clause if present.
        let payload = apply_format_clause(
            payload,
            &entry.format,
            grouped_by.as_deref(),
            RenderInputs {
                user_vars: &entry.user_vars,
                store,
                disclosure: disclosure_of(&entry.with_options),
            },
            None,
            exec_warnings,
        )?;

        Ok(payload)
    }

    // -----------------------------------------------------------------------
    // COALESCE
    // -----------------------------------------------------------------------

    fn execute_coalesce(
        &self,
        coalesce: &CoalesceStmt,
        store: &dyn CalStoreFacade,
        query: &CalQuery,
        exec_warnings: &mut Vec<String>,
    ) -> std::result::Result<CalResultPayload, CalError> {
        // ── Phase 2: Multi-branch COALESCE ────────────────────────────────
        //
        // When `branches` is non-empty, try each branch in order.
        // Return the first non-empty result. Short-circuit remaining branches.
        // If all branches are empty, try the ELSE branch.
        if !coalesce.branches.is_empty() {
            for (i, branch) in coalesce.branches.iter().enumerate() {
                let surrogate = CalQuery {
                    version: query.version,
                    statement: branch.query.clone(),
                    pipeline: Vec::new(),
                    with_options: query.with_options.clone(),
                    format: None,
                    let_bindings: Vec::new(),
                    let_values: query.let_values.clone(),
                    user_vars: HashMap::new(),
                    warnings: Vec::new(),
                };

                let payload =
                    self.execute_statement(&surrogate.statement, store, &surrogate, exec_warnings)?;

                // Check if result is non-empty — short-circuit on first hit.
                let is_non_empty = match &payload {
                    CalResultPayload::Grains { grains, .. } => !grains.is_empty(),
                    CalResultPayload::Exists { exists, .. } => *exists,
                    CalResultPayload::History { versions } => !versions.is_empty(),
                    CalResultPayload::Count { count } => *count > 0,
                    _ => true,
                };

                if is_non_empty {
                    exec_warnings.push(format!(
                        "COALESCE: branch {} returned results; short-circuited {} remaining branch(es)",
                        i + 1,
                        coalesce.branches.len() - i - 1
                            + if coalesce.else_branch.is_some() { 1 } else { 0 }
                    ));
                    return Ok(payload);
                }
            }

            // All branches empty — try ELSE branch if present.
            if let Some(ref else_stmt) = coalesce.else_branch {
                let surrogate = CalQuery {
                    version: query.version,
                    statement: *else_stmt.clone(),
                    pipeline: Vec::new(),
                    with_options: query.with_options.clone(),
                    format: None,
                    let_bindings: Vec::new(),
                    let_values: query.let_values.clone(),
                    user_vars: HashMap::new(),
                    warnings: Vec::new(),
                };

                return self.execute_statement(
                    &surrogate.statement,
                    store,
                    &surrogate,
                    exec_warnings,
                );
            }

            // All branches empty, no ELSE — return empty grains.
            return Ok(CalResultPayload::Grains {
                grains: Vec::new(),
                total_available: Some(0),
            });
        }

        // ── Phase 1: Single-branch COALESCE (backward compat) ────────────
        //
        // Treat as RECALL with the coalesce grain type and where clause.
        let recall = RecallStmt {
            grain_type: coalesce.grain_type.clone(),
            about: None,
            where_clause: coalesce.where_clause.clone(),
            recent: None,
            since: None,
            until: None,
            like: None,
            between: None,
            contradictions: None,
            limit: None,
            as_format: None,
            span: coalesce.span,
        };

        let payload = self.execute_recall(&recall, store, query, exec_warnings)?;

        // Return the first grain only (coalesce semantics: stop at first hit).
        match payload {
            CalResultPayload::Grains { mut grains, .. } => {
                grains.truncate(1);
                let count = grains.len();
                Ok(CalResultPayload::Grains {
                    grains,
                    total_available: Some(count),
                })
            }
            other => Ok(other),
        }
    }

    // -----------------------------------------------------------------------
    // SET OPERATIONS (UNION / INTERSECT / EXCEPT)
    // -----------------------------------------------------------------------

    fn execute_set_op(
        &self,
        set_op: &SetOpStmt,
        store: &dyn CalStoreFacade,
        query: &CalQuery,
        exec_warnings: &mut Vec<String>,
    ) -> std::result::Result<CalResultPayload, CalError> {
        // Execute each operand independently.
        let mut operand_results: Vec<Vec<CalGrainResult>> = Vec::new();

        for stmt in &set_op.operands {
            let surrogate = crate::ast::CalQuery {
                version: query.version,
                statement: stmt.clone(),
                pipeline: Vec::new(),
                with_options: query.with_options.clone(),
                format: None,
                let_bindings: Vec::new(),
                let_values: query.let_values.clone(),
                user_vars: HashMap::new(),
                warnings: Vec::new(),
            };
            let payload = self.execute_statement(stmt, store, &surrogate, exec_warnings)?;
            let grains = extract_grains(payload);
            operand_results.push(grains);
        }

        if operand_results.is_empty() {
            return Ok(CalResultPayload::Grains {
                grains: Vec::new(),
                total_available: Some(0),
            });
        }

        let mut result = operand_results.remove(0);

        for next in operand_results {
            result = match set_op.op {
                SetOp::Union => union_grains(result, next),
                SetOp::Intersect => intersect_grains(result, &next),
                SetOp::Except => except_grains(result, &next),
            };
        }

        let count = result.len();
        Ok(CalResultPayload::Grains {
            grains: result,
            total_available: Some(count),
        })
    }

    // -----------------------------------------------------------------------
    // ASSEMBLE
    // -----------------------------------------------------------------------

    fn sample_assembly_manifest(&self) -> bool {
        let rate = self.config.assembly_manifest_sample_rate.clamp(0.0, 1.0);
        if rate <= 0.0 {
            return false;
        }
        if rate >= 1.0 {
            return true;
        }
        use std::sync::atomic::{AtomicU64, Ordering};
        static SAMPLE_SEQ: AtomicU64 = AtomicU64::new(0);
        // Golden-ratio stepping distributes samples without pulling a random
        // generator into the dependency-light CAL crate.
        let n = SAMPLE_SEQ.fetch_add(1, Ordering::Relaxed);
        let bucket = n.wrapping_mul(0x9e3779b97f4a7c15) % 1_000_000;
        bucket < (rate * 1_000_000.0) as u64
    }

    // Keeping every measured value explicit makes the call site an auditable
    // inventory of what the manifest commits to; this is internal and sampled.
    #[allow(clippy::too_many_arguments)]
    fn record_assembly_manifest(
        &self,
        assemble: &AssembleStmt,
        store: &dyn CalStoreFacade,
        candidates: Vec<String>,
        included: Vec<String>,
        payload: &CalResultPayload,
        budget: serde_json::Value,
        sources: serde_json::Value,
    ) {
        if !self.sample_assembly_manifest() {
            return;
        }
        let included_set: std::collections::HashSet<&str> =
            included.iter().map(String::as_str).collect();
        let dropped_hashes = candidates
            .into_iter()
            .filter(|h| !included_set.contains(h.as_str()))
            .collect();
        let rendered = match payload {
            CalResultPayload::Formatted { text, .. } => text.as_bytes().to_vec(),
            CalResultPayload::MultiFormatted { formats, .. } => {
                let sorted: std::collections::BTreeMap<&str, &str> = formats
                    .iter()
                    .map(|(k, v)| (k.as_str(), v.as_str()))
                    .collect();
                serde_json::to_vec(&sorted).unwrap_or_default()
            }
            _ => serde_json::to_vec(payload).unwrap_or_default(),
        };
        let mut digest = Sha256::new();
        digest.update(&rendered);
        // Digest the statement, never store it: an ASSEMBLE routinely names its
        // subject, and this manifest is immutable, replicating, and sits in a
        // namespace the erasure selector does not reach.
        let mut query_digest = Sha256::new();
        query_digest.update(
            serde_json::to_vec(assemble)
                .unwrap_or_default()
                .as_slice(),
        );
        store.note_assembly_manifest(&AssemblyManifest {
            query_sha256: hex::encode(query_digest.finalize()),
            included_hashes: included,
            rendered_sha256: hex::encode(digest.finalize()),
            budget,
            dropped_hashes,
            sources,
        });
    }

    fn execute_assemble(
        &self,
        assemble: &AssembleStmt,
        store: &dyn CalStoreFacade,
        query: &CalQuery,
        exec_warnings: &mut Vec<String>,
    ) -> std::result::Result<CalResultPayload, CalError> {
        // The ASSEMBLE's own recall-tuning options (the non-dedup tail of its
        // `WITH dedup, <opts>` clause) live on the AssembleStmt so they scope to
        // THIS assemble even when it is nested (EXPLAIN/COALESCE/…). Fold them
        // into the query's with_options once, here, so every downstream consumer
        // — the streaming payload, the AssembleEngine's per-source recall, the
        // post-merge pass, and the single-source surrogate — sees them.
        let merged;
        let query = if assemble.with_options.is_empty() {
            query
        } else {
            merged = CalQuery {
                with_options: query
                    .with_options
                    .iter()
                    .chain(&assemble.with_options)
                    .cloned()
                    .collect(),
                ..query.clone()
            };
            &merged
        };
        // FR-004: Streaming ASSEMBLE — return the statement + WITH options
        // for the HTTP handler to stream. The handler applies post-merge
        // options (rerank, dedup, diversity, etc.) before streaming grains.
        if assemble.streaming {
            return Ok(CalResultPayload::StreamAssemble {
                assemble: Box::new(assemble.clone()),
                with_options: query.with_options.clone(),
            });
        }

        // ── Multi-source path (Phase 2) ────────────────────────────────
        //
        // When `assemble.sources` is `Some(...)`, delegate to the
        // AssembleEngine which handles budget allocation, dedup,
        // and per-source timeouts.
        if assemble.sources.is_some() {
            let engine = super::assemble::AssembleEngine::new(self);
            let result = engine.execute(assemble, store, query, exec_warnings)?;
            let (candidates, source_snapshot, budget_snapshot) = match &result {
                CalResultPayload::Assembled {
                    grains,
                    sources,
                    total_tokens,
                    budget_limit,
                    ..
                } => {
                    let mut all = Vec::new();
                    let mut offset = 0usize;
                    for source in sources {
                        let end = (offset + source.grain_count).min(grains.len());
                        all.extend(grains[offset..end].iter().map(|g| g.hash.clone()));
                        all.extend(source.omitted.iter().map(|g| g.hash.clone()));
                        offset = end;
                    }
                    (
                        all,
                        serde_json::to_value(sources).unwrap_or(serde_json::Value::Null),
                        serde_json::json!({
                            "limit": budget_limit,
                            "unit": "tokens",
                            "used": total_tokens,
                        }),
                    )
                }
                _ => (Vec::new(), serde_json::Value::Null, serde_json::Value::Null),
            };

            // ── Post-merge WITH options ────────────────────────────────
            //
            // The AssembleEngine merges results from multiple sources but
            // does not apply query-level WITH options (e.g., conflict_resolution,
            // dedup, min_score, diversity, rerank, llm_rerank).  These are
            // parsed into `query.with_options` but were previously silently
            // discarded for multi-source ASSEMBLE.  Apply them now on the
            // merged result set.
            let result = if !query.with_options.is_empty() {
                self.apply_assemble_post_merge_options(
                    result,
                    &query.with_options,
                    exec_warnings,
                    store,
                    &assemble.topic,
                )?
            } else {
                result
            };

            let included = match &result {
                CalResultPayload::Assembled { grains, .. } => {
                    grains.iter().map(|g| g.hash.clone()).collect()
                }
                _ => Vec::new(),
            };

            // Apply FORMAT clause to multi-source results (same as single-source path).
            let result = if assemble.format.is_some() {
                apply_format_clause(
                    result,
                    &assemble.format,
                    None,
                    RenderInputs {
                        user_vars: &query.user_vars,
                        store,
                        // An ASSEMBLE holds its own WITH options so they scope
                        // to this assembly even when nested; fall back to the
                        // enclosing query's.
                        disclosure: disclosure_of(&assemble.with_options)
                            .or_else(|| disclosure_of(&query.with_options)),
                    },
                    // `context_name` is the explicit `ASSEMBLE "name"`
                    // override; `topic` is the bare name. `for_whom` is the
                    // FOR clause, which is what §10.5 calls the intent.
                    Some((
                        assemble.context_name.as_deref().unwrap_or(&assemble.topic),
                        assemble.for_whom.as_deref().unwrap_or(""),
                    )),
                    exec_warnings,
                )?
            } else {
                result
            };
            self.record_assembly_manifest(
                assemble,
                store,
                candidates,
                included,
                &result,
                budget_snapshot,
                source_snapshot,
            );
            return Ok(result);
        }

        // ── Single-source path (Phase 1 compat) ───────────────────────
        //
        // Execute the FROM source query, then apply the optional WHERE
        // filter as a second pass over the results.
        let base_grains = match &assemble.from {
            Source::Query(recall_stmt) => {
                let surrogate = crate::ast::CalQuery {
                    version: query.version,
                    statement: CalStatement::Recall(*recall_stmt.clone()),
                    pipeline: Vec::new(),
                    with_options: query.with_options.clone(),
                    format: None,
                    let_bindings: Vec::new(),
                    let_values: query.let_values.clone(),
                    user_vars: HashMap::new(),
                    warnings: Vec::new(),
                };
                let payload =
                    self.execute_statement(&surrogate.statement, store, &surrogate, exec_warnings)?;
                extract_grains(payload)
            }
            Source::Hashes(hashes) => {
                let mut grains = Vec::new();
                for hs in hashes {
                    match Hash::from_hex(hs) {
                        Ok(hash) => {
                            if let Ok(grain) = store.get(&hash) {
                                grains.push(CalGrainResult {
                                    hash: hs.clone(),
                                    grain_type: grain.grain_type.as_str().to_string(),
                                    score: 1.0,
                                    fields: serde_json::Value::Object(
                                        grain
                                            .fields
                                            .iter()
                                            .map(|(k, v)| (k.clone(), v.clone()))
                                            .collect(),
                                    ),
                                    score_breakdown: None,
                                    explanation: None,
                                    relative_time: None,
                                    is_deterministic: true,
                                    contested_by: None,
                                });
                            }
                        }
                        Err(_) => {
                            // Silently skip invalid hashes (parser validates them).
                        }
                    }
                }
                grains
            }
            Source::Parameter { name, .. } => {
                return Ok(CalResultPayload::Unsupported {
                    statement: "assemble".into(),
                    message: format!(
                        "Parameter source ${} is not yet supported in single-source ASSEMBLE. \
                         Use a RECALL subquery instead.",
                        name
                    ),
                });
            }
        };

        // ── WI-1.1: ASSEMBLE WHERE clause ────────────────────────────────
        //
        // Apply the WHERE clause as a post-composition filter on assembled
        // results. Each condition is matched against the grain's fields.
        let grains = if let Some(ref wc) = assemble.where_clause {
            let type_conditions = extract_type_specific_conditions(&wc.condition);
            let mut common_conditions = Vec::new();
            collect_common_conditions(&wc.condition, &mut common_conditions);

            let filtered: Vec<CalGrainResult> = base_grains
                .into_iter()
                .filter(|grain| {
                    // Check type-specific conditions.
                    let type_ok = type_conditions
                        .iter()
                        .all(|(field, comp, val)| grain_matches_condition(grain, field, comp, val));
                    // Check common field conditions post-hoc.
                    let common_ok = common_conditions
                        .iter()
                        .all(|(field, comp, val)| grain_matches_condition(grain, field, comp, val));
                    type_ok && common_ok
                })
                .collect();

            filtered
        } else {
            base_grains
        };

        // ── Apply BUDGET limit (single-source path) ─────────────────────
        //
        // For the single-source path, apply the budget as a grain-count
        // limit. The multi-source path uses the AssembleEngine with proper
        // token-counting; here grain count is a reasonable approximation.
        // Keep what the budget cut: a template's `ELEMENT_OMIT` section is how
        // an assembly accounts for what it left out, and that has to work the
        // same whether there is one source or several.
        let candidates: Vec<String> = grains.iter().map(|g| g.hash.clone()).collect();
        let (grains, omitted) = if let Some(ref budget) = assemble.budget {
            let mut grains = grains;
            let limit = budget.tokens as usize;
            if grains.len() > limit {
                let dropped = grains.split_off(limit);
                (grains, dropped)
            } else {
                (grains, Vec::new())
            }
        } else {
            (grains, Vec::new())
        };
        store.note_assembly_budget(!omitted.is_empty());
        let included: Vec<String> = grains.iter().map(|g| g.hash.clone()).collect();
        let budget_snapshot = serde_json::json!({
            "limit": assemble.budget.as_ref().map(|b| b.tokens),
            "unit": "grains",
            "used": grains.len(),
        });
        let source_snapshot = serde_json::json!([{
            "label": "",
            "grain_count": grains.len(),
            "dropped_count": omitted.len(),
        }]);

        // ── WI-1.1: ASSEMBLE FORMAT clause ───────────────────────────────
        //
        // If a FORMAT clause is present, render the grains into the
        // specified format and return a Formatted/MultiFormatted payload.
        let result = if let Some(ref clause) = assemble.format {
            // One source, but still an assembly: `assembly.*`, `budget.*` and
            // `source.*` must resolve here exactly as they do on the
            // multi-source path. Rendering with an empty plan is what silently
            // blanked them.
            let ctx = super::templates::AssemblyContext {
                name: assemble
                    .context_name
                    .clone()
                    .unwrap_or_else(|| assemble.topic.clone()),
                intent: assemble.for_whom.clone().unwrap_or_default(),
                source_count: 1,
                grain_count: grains.len(),
                budget_total: assemble.budget.as_ref().map_or(0, |b| b.tokens as u64),
                budget_used: grains.len() as u64,
                // The single-source path budgets by grain count, not tokens —
                // say so rather than mislabel the unit.
                budget_unit: "grains".to_string(),
            };
            let sources = [super::templates::RenderSource {
                label: "",
                grains: &grains,
                omitted: &omitted,
                priority: 1,
                tokens_used: grains.len(),
                truncated: !omitted.is_empty(),
            }];
            let plan = super::templates::RenderPlan {
                assembly: Some(&ctx),
                sources: Some(&sources),
            };
            apply_format_clause_to_grains(
                &grains,
                clause,
                None,
                RenderInputs {
                    user_vars: &query.user_vars,
                    store,
                    disclosure: disclosure_of(&query.with_options),
                },
                &plan,
                exec_warnings,
            )?
        } else {
            let count = grains.len();
            CalResultPayload::Grains {
                grains,
                total_available: Some(count),
            }
        };
        self.record_assembly_manifest(
            assemble,
            store,
            candidates,
            included,
            &result,
            budget_snapshot,
            source_snapshot,
        );
        Ok(result)
    }

    // -----------------------------------------------------------------------
    // Pipeline application
    // -----------------------------------------------------------------------

    /// Validate that any field references in `stages` are known. SELECT /
    /// ORDER BY / GROUP BY / PROJECT field names are restricted to the
    /// common-field set plus the declared grain type's type-specific fields.
    fn validate_pipeline_fields(
        stages: &[PipelineStage],
        grain_type: &GrainTypePlural,
    ) -> std::result::Result<(), CalError> {
        let check = |field: &str, span: Option<Span>| -> std::result::Result<(), CalError> {
            // Common fields are always valid.
            if COMMON_FIELDS.contains(&field) {
                return Ok(());
            }
            // Domain-prefixed fields (hc:patient_id, fin:account, …) are
            // valid by structure — skip lookup.
            if field.contains(':') {
                return Ok(());
            }
            // Type-specific fields valid on the declared grain type.
            let allowed = type_specific_fields(grain_type);
            if allowed.contains(&field) {
                return Ok(());
            }
            // If no grain type was specified (`RECALL WHERE …`), only the
            // common set is in scope — anything else is a hard reject.
            let suggestion = suggest_field(field, grain_type);
            Err(CalError::FieldNotOnGrainType {
                field: field.to_string(),
                grain_type: grain_type.as_str().to_string(),
                span,
                suggestion,
            })
        };

        for stage in stages {
            match stage {
                PipelineStage::Select { fields, span } => {
                    for f in fields {
                        check(f, *span)?;
                    }
                }
                PipelineStage::OrderBy { field, span, .. } => check(field, *span)?,
                PipelineStage::GroupBy { field, span } => check(field, *span)?,
                PipelineStage::Project { fields, span } => {
                    for pf in fields {
                        check(&pf.field, *span)?;
                    }
                }
                _ => {}
            }
        }
        Ok(())
    }

    fn apply_pipeline(
        &self,
        payload: CalResultPayload,
        stages: &[PipelineStage],
    ) -> std::result::Result<(CalResultPayload, Option<String>), CalError> {
        let mut current = payload;
        let mut grouped_by: Option<String> = None;

        for stage in stages {
            current = match (current, stage) {
                // LIMIT
                (CalResultPayload::Grains { grains, .. }, PipelineStage::Limit { value, .. }) => {
                    let capped = (*value).min(self.config.max_limit) as usize;
                    let limited: Vec<_> = grains.into_iter().take(capped).collect();
                    let count = limited.len();
                    CalResultPayload::Grains {
                        grains: limited,
                        total_available: Some(count),
                    }
                }

                // OFFSET
                (CalResultPayload::Grains { grains, .. }, PipelineStage::Offset { value, .. }) => {
                    let offset: Vec<_> = grains.into_iter().skip(*value as usize).collect();
                    let count = offset.len();
                    CalResultPayload::Grains {
                        grains: offset,
                        total_available: Some(count),
                    }
                }

                // COUNT
                (CalResultPayload::Grains { grains, .. }, PipelineStage::Count { .. }) => {
                    CalResultPayload::Count {
                        count: grains.len(),
                    }
                }

                // FIRST
                (CalResultPayload::Grains { grains, .. }, PipelineStage::First { .. }) => {
                    let first: Vec<_> = grains.into_iter().take(1).collect();
                    CalResultPayload::Grains {
                        grains: first,
                        total_available: Some(1),
                    }
                }

                // ORDER BY
                (
                    CalResultPayload::Grains { grains, .. },
                    PipelineStage::OrderBy {
                        field, descending, ..
                    },
                ) => {
                    let mut sorted = grains;
                    sorted.sort_by(|a, b| {
                        let va = json_field(&a.fields, field);
                        let vb = json_field(&b.fields, field);
                        let cmp = compare_json_values(va, vb);
                        if *descending {
                            cmp.reverse()
                        } else {
                            cmp
                        }
                    });
                    let count = sorted.len();
                    CalResultPayload::Grains {
                        grains: sorted,
                        total_available: Some(count),
                    }
                }

                // SELECT (field projection)
                (CalResultPayload::Grains { grains, .. }, PipelineStage::Select { fields, .. }) => {
                    let projected: Vec<_> = grains
                        .into_iter()
                        .map(|g| {
                            let projected_fields = project_fields(&g.fields, fields);
                            CalGrainResult {
                                fields: projected_fields,
                                ..g
                            }
                        })
                        .collect();
                    let count = projected.len();
                    CalResultPayload::Grains {
                        grains: projected,
                        total_available: Some(count),
                    }
                }

                // PROJECT (field projection with optional aliasing)
                (
                    CalResultPayload::Grains { grains, .. },
                    PipelineStage::Project { fields, .. },
                ) => {
                    let field_names: Vec<String> =
                        fields.iter().map(|pf| pf.field.clone()).collect();
                    let projected: Vec<_> = grains
                        .into_iter()
                        .map(|g| {
                            let mut new_map = serde_json::Map::new();
                            if let serde_json::Value::Object(map) = &g.fields {
                                for pf in fields {
                                    if let Some(v) = map.get(&pf.field) {
                                        let key = pf.alias.as_deref().unwrap_or(&pf.field);
                                        new_map.insert(key.to_string(), v.clone());
                                    }
                                }
                            }
                            let _ = field_names.len(); // suppress unused warning
                            CalGrainResult {
                                fields: serde_json::Value::Object(new_map),
                                ..g
                            }
                        })
                        .collect();
                    let count = projected.len();
                    CalResultPayload::Grains {
                        grains: projected,
                        total_available: Some(count),
                    }
                }

                // SUBJECTS extractor (I-7 fix: returns Grains, not Describe)
                (CalResultPayload::Grains { grains, .. }, PipelineStage::Subjects { .. }) => {
                    let extracted: Vec<CalGrainResult> = grains
                        .iter()
                        .filter_map(|g| {
                            json_field(&g.fields, "subject").map(|v| CalGrainResult {
                                hash: String::new(),
                                grain_type: "extracted".into(),
                                score: 0.0,
                                fields: serde_json::json!({ "value": v }),
                                score_breakdown: None,
                                explanation: None,
                                relative_time: None,
                                is_deterministic: true,
                                contested_by: None,
                            })
                        })
                        .collect();
                    let count = extracted.len();
                    CalResultPayload::Grains {
                        grains: extracted,
                        total_available: Some(count),
                    }
                }

                // OBJECTS extractor (I-7 fix: returns Grains, not Describe)
                (CalResultPayload::Grains { grains, .. }, PipelineStage::Objects { .. }) => {
                    let extracted: Vec<CalGrainResult> = grains
                        .iter()
                        .filter_map(|g| {
                            json_field(&g.fields, "object").map(|v| CalGrainResult {
                                hash: String::new(),
                                grain_type: "extracted".into(),
                                score: 0.0,
                                fields: serde_json::json!({ "value": v }),
                                score_breakdown: None,
                                explanation: None,
                                relative_time: None,
                                is_deterministic: true,
                                contested_by: None,
                            })
                        })
                        .collect();
                    let count = extracted.len();
                    CalResultPayload::Grains {
                        grains: extracted,
                        total_available: Some(count),
                    }
                }

                // HASHES extractor (I-7 fix: returns Grains, not Describe)
                (CalResultPayload::Grains { grains, .. }, PipelineStage::Hashes { .. }) => {
                    let extracted: Vec<CalGrainResult> = grains
                        .iter()
                        .map(|g| CalGrainResult {
                            hash: String::new(),
                            grain_type: "extracted".into(),
                            score: 0.0,
                            fields: serde_json::json!({ "value": g.hash }),
                            score_breakdown: None,
                            explanation: None,
                            relative_time: None,
                            is_deterministic: true,
                            contested_by: None,
                        })
                        .collect();
                    let count = extracted.len();
                    CalResultPayload::Grains {
                        grains: extracted,
                        total_available: Some(count),
                    }
                }

                // GROUP BY — reorders grains so same-field-value grains are
                // contiguous, sorted chronologically within each group. Groups
                // are ordered by the earliest created_at_sec in each group.
                (
                    CalResultPayload::Grains {
                        grains,
                        total_available,
                    },
                    PipelineStage::GroupBy { field, .. },
                ) => {
                    grouped_by = Some(field.clone());
                    let grouped = group_grains_by_field(grains, field);
                    CalResultPayload::Grains {
                        grains: grouped,
                        total_available,
                    }
                }

                // WHERE (post-pipeline filter)
                (
                    CalResultPayload::Grains { grains, .. },
                    PipelineStage::Filter { condition, .. },
                ) => {
                    let filtered: Vec<_> = grains
                        .into_iter()
                        .filter(|grain| grain_matches_condition_tree(grain, condition))
                        .collect();
                    let count = filtered.len();
                    CalResultPayload::Grains {
                        grains: filtered,
                        total_available: Some(count),
                    }
                }

                // Non-Grains payload: passthrough for all stages.
                (other, _) => other,
            };
        }

        Ok((current, grouped_by))
    }
}

// ---------------------------------------------------------------------------
// Helper functions
// ---------------------------------------------------------------------------

/// Compute field-level differences between two grain versions.
///
/// Uses BTreeSet-based key comparison to produce a deterministic,
/// sorted list of `FieldDiff` entries.
fn diff_grains(
    a: &areev_core::format::deserialize::DeserializedGrain,
    b: &areev_core::format::deserialize::DeserializedGrain,
) -> Vec<super::ast::FieldDiff> {
    use std::collections::BTreeSet;

    let keys_a: BTreeSet<&str> = a.fields.keys().map(|s| s.as_str()).collect();
    let keys_b: BTreeSet<&str> = b.fields.keys().map(|s| s.as_str()).collect();

    let mut diffs = Vec::new();

    // Added fields (in b but not in a)
    for key in keys_b.difference(&keys_a) {
        diffs.push(super::ast::FieldDiff::Added {
            field: key.to_string(),
            value: b.fields[*key].clone(),
        });
    }

    // Removed fields (in a but not in b)
    for key in keys_a.difference(&keys_b) {
        diffs.push(super::ast::FieldDiff::Removed {
            field: key.to_string(),
            value: a.fields[*key].clone(),
        });
    }

    // Changed fields (in both, different values)
    for key in keys_a.intersection(&keys_b) {
        if a.fields[*key] != b.fields[*key] {
            diffs.push(super::ast::FieldDiff::Changed {
                field: key.to_string(),
                old: a.fields[*key].clone(),
                new: b.fields[*key].clone(),
            });
        }
    }

    diffs
}

/// Return a list of build-time feature flags for DESCRIBE SERVER.
#[allow(clippy::vec_init_then_push)]
fn build_features_list() -> Vec<&'static str> {
    let mut features = Vec::new();
    #[cfg(feature = "http")]
    features.push("http");
    #[cfg(feature = "grpc")]
    features.push("grpc");
    #[cfg(feature = "mcp")]
    features.push("mcp");
    #[cfg(feature = "a2a")]
    features.push("a2a");
    #[cfg(feature = "app")]
    features.push("app");
    #[cfg(feature = "signing")]
    features.push("signing");
    #[cfg(feature = "import")]
    features.push("import");
    #[cfg(feature = "auth")]
    features.push("auth");
    #[cfg(feature = "rerank")]
    features.push("rerank");
    #[cfg(feature = "llm-rerank")]
    features.push("llm-rerank");
    #[cfg(feature = "pii_ner")]
    features.push("pii_ner");
    #[cfg(feature = "cal")]
    features.push("cal");
    features
}

/// Compute a SHA-256 hash of the NFC-normalized, trimmed query string (C-4).
fn compute_query_hash(input: &str) -> String {
    use unicode_normalization::UnicodeNormalization as _;
    let normalized: String = input.trim().nfc().collect();
    let mut hasher = Sha256::new();
    hasher.update(normalized.as_bytes());
    hex::encode(hasher.finalize())
}

/// Map an `AreevError` raised during CAL execution into the right `CalError`
/// variant. Crypto failures (AES-GCM decrypt, envelope too short, KEY-E003)
/// are a distinct class from budget overruns and get `CAL-E090`. Everything
/// else falls through to `CAL-E030 BudgetExceeded` for backwards
/// compatibility with existing error-surfacing tests.
pub(super) fn map_store_err(e: AreevError, span: Option<super::errors::Span>) -> CalError {
    match e {
        AreevError::CryptoError(_) => CalError::CryptoError {
            detail: e.to_string(),
            span,
        },
        // A store-side input validation failure is not a resource overrun —
        // surface it as CAL-E092, not CAL-E030 "Budget exceeded".
        AreevError::Validation(_) => CalError::InvalidQuery {
            detail: e.to_string(),
            span,
        },
        _ => CalError::BudgetExceeded {
            detail: e.to_string(),
            span,
        },
    }
}

/// Build `AddOptions` from CAL `AddWithOption` list.
fn build_add_options(opts: &[AddWithOption], warnings: &mut Vec<String>) -> AddOptions {
    let mut options = AddOptions::default();
    for opt in opts {
        match opt {
            AddWithOption::ExtractEventDate => {
                options.extract_event_date = Some(true);
            }
            AddWithOption::AutoRelate => {
                // Parsed and accepted since 1.0, but no store path has ever read
                // `AddOptions::auto_relate` — the option silently did nothing.
                // It stays in the grammar (it is documented, and removing it
                // would turn working queries into parse errors) but now says so
                // instead of pretending.
                // CAL-W013, not W004: W004 is UnknownExtensionOption, and a code
                // has to locate exactly one variant to be worth reporting.
                warnings.push(
                    "CAL-W013: WITH auto_relate is accepted but not implemented — no relations \
                     are inferred. Link grains explicitly with related_to, or use WITH \
                     multi_hop(n) on recall to widen through the entity graph."
                        .to_string(),
                );
                options.auto_relate = Some(true);
            }
            AddWithOption::ExtractMemories => {
                // No-op: sync extraction removed, offloaded to an async extraction executor via markers.
            }
            AddWithOption::Sync => {
                options.sync = Some(true);
            }
            // Handled at the ADD arm (field stamping) — nothing for the
            // store contract to carry.
            AddWithOption::Occurrence => {}
        }
    }
    options
}

/// Convert a `Value` to its CAL literal representation for parameter substitution.
fn value_to_cal_literal(value: &super::ast::Value) -> String {
    match value {
        super::ast::Value::String { value } => {
            format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
        }
        super::ast::Value::Number { value } => value.to_string(),
        super::ast::Value::Boolean { value } => value.to_string(),
        super::ast::Value::Hash { value } => format!("#{value}"),
        super::ast::Value::Parameter { name } => format!("${name}"),
        super::ast::Value::Array { values } => {
            let items: Vec<String> = values.iter().map(value_to_cal_literal).collect();
            format!("[{}]", items.join(", "))
        }
    }
}

/// Convert a CAL AST `Value` to a `serde_json::Value`.
fn cal_value_to_json(val: &super::ast::Value) -> serde_json::Value {
    match val {
        super::ast::Value::String { value } => serde_json::Value::String(value.clone()),
        // An integral literal becomes a JSON *integer*, not a float. The AST
        // holds every number as f64, and `Number::from_f64` always yields a
        // float — so `serde_json::Value::as_i64()` returned None for all of
        // them, and every i64-typed field set from CAL (`created_at`,
        // `valid_to`, `duration_ms`, …) was silently discarded on the way to
        // the grain builder. Fractional values are unaffected.
        super::ast::Value::Number { value } => {
            if value.fract() == 0.0 && value.abs() < 9.007_199_254_740_992e15 {
                serde_json::Value::Number(serde_json::Number::from(*value as i64))
            } else {
                serde_json::Number::from_f64(*value)
                    .map(serde_json::Value::Number)
                    .unwrap_or(serde_json::Value::Null)
            }
        }
        super::ast::Value::Boolean { value } => serde_json::Value::Bool(*value),
        super::ast::Value::Array { values } => {
            serde_json::Value::Array(values.iter().map(cal_value_to_json).collect())
        }
        super::ast::Value::Hash { value } => serde_json::Value::String(value.clone()),
        super::ast::Value::Parameter { name } => serde_json::Value::String(format!("${}", name)),
    }
}

/// The payload for a governance statement on an executor no host wired.
fn governance_unwired(statement: &str) -> CalResultPayload {
    CalResultPayload::Unsupported {
        statement: statement.into(),
        message: "governance is not wired on this surface — the host attaches a \
                  GovernanceHost (CalExecutor::with_governance) to back RUN LOOP, \
                  APPROVE, REJECT, APPLY, ROLLBACK, and the loop DESCRIBE reads"
            .into(),
    }
}

/// Return the canonical statement type name as a lowercase string.
fn statement_type_name(stmt: &CalStatement) -> String {
    match stmt {
        CalStatement::Recall(_) => "recall",
        CalStatement::SetOp(_) => "set_op",
        CalStatement::Exists(_) => "exists",
        CalStatement::Assemble(_) => "assemble",
        CalStatement::History(_) => "history",
        CalStatement::Explain(_) => "explain",
        CalStatement::Describe(_) => "describe",
        CalStatement::Batch(_) => "batch",
        CalStatement::Coalesce(_) => "coalesce",
        CalStatement::Add(_) => "add",
        CalStatement::AddWorkflow(_) => "add_workflow",
        CalStatement::Supersede(_) => "supersede",
        CalStatement::SupersedeWorkflow(_) => "supersede_workflow",
        CalStatement::Accumulate(_) => "accumulate",
        CalStatement::Revert(_) => "revert",
        CalStatement::Forget(_) => "forget",
        CalStatement::Purge(_) => "purge",
        CalStatement::ReportSubject(_) => "report_subject",
        CalStatement::DefineTemplate(_) => "define_template",
        CalStatement::DropTemplate(_) => "drop_template",
        CalStatement::DefineQuery(_) => "define_query",
        CalStatement::DropQuery(_) => "drop_query",
        CalStatement::RunQuery(_) => "run_query",
        CalStatement::Grant(_) => "grant",
        CalStatement::Revoke(_) => "revoke",
        CalStatement::ShowGrants(_) => "show_grants",
        CalStatement::Approve(_) => "approve",
        CalStatement::Reject(_) => "reject",
        CalStatement::ApplyRec(_) => "apply",
        CalStatement::RollbackRec(_) => "rollback",
        CalStatement::RunLoop(_) => "run_loop",
        CalStatement::Remember(_) => "remember",
        CalStatement::EntityAt(_) => "entity_at",
        CalStatement::RunTrace(_) => "run_trace",
        CalStatement::RunsTouching(_) => "runs_touching",
        CalStatement::DerivedFrom(_) => "derived_from",
        CalStatement::ShowForks(_) => "show_forks",
        CalStatement::Merge(_) => "merge",
        CalStatement::Related(_) => "related",
        CalStatement::Novelty(_) => "novelty",
    }
    .to_string()
}

/// Classify the required JWT scope for a CAL statement type.
/// Read statements require "read", write statements require "write",
/// destructive and admin statements require "admin".
fn required_scope_for_statement(stmt: &CalStatement) -> &'static str {
    match stmt {
        CalStatement::Recall(_)
        | CalStatement::SetOp(_)
        | CalStatement::Exists(_)
        | CalStatement::Assemble(_)
        | CalStatement::History(_)
        | CalStatement::Explain(_)
        | CalStatement::Describe(_)
        | CalStatement::Batch(_)
        | CalStatement::Coalesce(_)
        | CalStatement::RunQuery(_)
        | CalStatement::ShowGrants(_)
        | CalStatement::EntityAt(_)
        | CalStatement::RunTrace(_)
        | CalStatement::RunsTouching(_)
        | CalStatement::DerivedFrom(_)
        | CalStatement::ShowForks(_)
        | CalStatement::Related(_)
        | CalStatement::Novelty(_)
        | CalStatement::ReportSubject(_) => "read",

        CalStatement::Add(_)
        | CalStatement::AddWorkflow(_)
        | CalStatement::Supersede(_)
        | CalStatement::SupersedeWorkflow(_)
        | CalStatement::Accumulate(_)
        | CalStatement::Revert(_)
        | CalStatement::Remember(_)
        | CalStatement::Merge(_) => "write",

        CalStatement::Forget(_)
        | CalStatement::Purge(_)
        | CalStatement::DefineTemplate(_)
        | CalStatement::DropTemplate(_)
        | CalStatement::DefineQuery(_)
        | CalStatement::DropQuery(_)
        | CalStatement::Grant(_)
        | CalStatement::Revoke(_)
        | CalStatement::Approve(_)
        | CalStatement::Reject(_)
        | CalStatement::ApplyRec(_)
        | CalStatement::RollbackRec(_)
        | CalStatement::RunLoop(_) => "admin",
    }
}

/// Return the canonical pipeline stage name for EXPLAIN plans.
fn pipeline_stage_name(stage: &PipelineStage) -> String {
    match stage {
        PipelineStage::Select { .. } => "SELECT".to_string(),
        PipelineStage::OrderBy {
            field, descending, ..
        } => {
            format!(
                "ORDER BY {} {}",
                field,
                if *descending { "DESC" } else { "ASC" }
            )
        }
        PipelineStage::Limit { value, .. } => format!("LIMIT {}", value),
        PipelineStage::Offset { value, .. } => format!("OFFSET {}", value),
        PipelineStage::Count { .. } => "COUNT".to_string(),
        PipelineStage::First { .. } => "FIRST".to_string(),
        PipelineStage::Subjects { .. } => "SUBJECTS".to_string(),
        PipelineStage::Objects { .. } => "OBJECTS".to_string(),
        PipelineStage::Hashes { .. } => "HASHES".to_string(),
        PipelineStage::GroupBy { field, .. } => format!("GROUP BY {}", field),
        PipelineStage::Project { .. } => "PROJECT".to_string(),
        PipelineStage::Filter { .. } => "WHERE (post-pipeline)".to_string(),
    }
}

/// Count the top-level items in a payload (for metadata.result_count).
fn count_payload_results(payload: &CalResultPayload) -> usize {
    match payload {
        CalResultPayload::Grains { grains, .. } => grains.len(),
        CalResultPayload::Exists { .. } => 1,
        CalResultPayload::Count { .. } => 1,
        CalResultPayload::History { versions } => versions.len(),
        CalResultPayload::Describe { .. } => 1,
        CalResultPayload::Explain { .. } => 1,
        CalResultPayload::Batch { results } => results.len(),
        CalResultPayload::Assembled { grains, .. } => grains.len(),
        CalResultPayload::Diff { changes, .. } => changes.len(),
        CalResultPayload::Formatted { grain_count, .. } => *grain_count,
        CalResultPayload::MultiFormatted { grain_count, .. } => *grain_count,
        CalResultPayload::Added { .. } => 1,
        CalResultPayload::Granted { .. } => 1,
        CalResultPayload::Revoked { .. } => 1,
        CalResultPayload::GrantList { grants } => grants.len(),
        CalResultPayload::LoopRan { .. } => 1,
        CalResultPayload::Reviewed { .. } => 1,
        CalResultPayload::RecApplied { .. } => 1,
        CalResultPayload::RecRolledBack { .. } => 1,
        CalResultPayload::Remembered { .. } => 1,
        CalResultPayload::EntityAt { grain, .. } => usize::from(grain.is_some()),
        CalResultPayload::RunTrace { .. } => 1,
        CalResultPayload::RunsTouching { runs, .. } => runs.len(),
        CalResultPayload::DerivedFrom { grains, .. } => grains.len(),
        CalResultPayload::Forks { forks } => forks.len(),
        CalResultPayload::Merged { .. } => 1,
        CalResultPayload::RelatedEntities { entities, .. } => entities.len(),
        CalResultPayload::NoveltyMatches { matches } => matches.len(),
        CalResultPayload::Superseded { .. } => 1,
        CalResultPayload::Accumulated { .. } => 1,
        CalResultPayload::Forgotten { .. } => 1,
        CalResultPayload::Purged { count } => *count,
        CalResultPayload::SubjectReport { grains, .. } => grains.len(),
        CalResultPayload::Unsupported { .. } => 0,
        CalResultPayload::TemplateDefined { .. } => 1,
        CalResultPayload::TemplateDropped { .. } => 1,
        CalResultPayload::QueryDefined { .. } => 1,
        CalResultPayload::QueryDropped { .. } => 1,
        CalResultPayload::StreamAssemble { .. } => 0,
    }
}

/// Extract a Vec<CalGrainResult> from a payload (for set operations).
fn extract_grains(payload: CalResultPayload) -> Vec<CalGrainResult> {
    match payload {
        CalResultPayload::Grains { grains, .. } => grains,
        CalResultPayload::Assembled { grains, .. } => grains,
        _ => Vec::new(),
    }
}

/// Convert a SearchHit slice to CalGrainResult vec.
fn hits_to_grain_results(hits: &[crate::store_types::SearchHit]) -> Vec<CalGrainResult> {
    hits.iter()
        .map(|hit| CalGrainResult {
            hash: hit.hash.to_hex(),
            grain_type: hit.grain.grain_type.as_str().to_string(),
            score: hit.score,
            fields: serde_json::Value::Object({
                let mut map: serde_json::Map<String, serde_json::Value> = hit
                    .grain
                    .fields
                    .iter()
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect();
                // `superseded_by` is a common grain field, but nothing can fill
                // it at write time — a grain cannot know its own successor — so
                // it only ever arrives from the index, via a recall that asked
                // for history. Project it here and every downstream consumer
                // gets the label for free: the JSON payload, and the renderers'
                // existing "(superseded)" note.
                if let Some(sup) = hit.superseded_by_hash {
                    map.entry("superseded_by".to_string())
                        .or_insert_with(|| serde_json::Value::String(sup.to_hex()));
                }
                map
            }),
            score_breakdown: hit
                .score_breakdown
                .as_ref()
                .map(|sb| serde_json::to_value(sb).unwrap_or(serde_json::Value::Null)),
            explanation: hit.explanation.clone(),
            relative_time: hit.relative_time.clone(),
            is_deterministic: false,
            contested_by: None,
        })
        .collect()
}

/// Stamp fork status onto recalled grains, and optionally drop the uncontested.
///
/// Two surfaces share this. `CONTRADICTIONS` keeps **only** contested grains —
/// the agent asked for the conflicts. `WITH contradiction_detection` keeps
/// everything and merely marks what is disputed — the agent asked for its normal
/// context, plus a warning about which parts of it are not settled.
///
/// `restrict_to` is the `(namespace, subject, relation)` key set from a
/// `CONTRADICTIONS OF (sub-query)` tail; `None` considers every open fork.
///
/// **Fail-open**, matching the recall path: a facade that cannot enumerate heads
/// reports no forks and the query degrades to "nothing is contested" with a
/// warning, rather than turning a working recall into a failed one. The one
/// exception is a *filtering* query, which yields nothing — claiming "no
/// conflicts" when the check did not run would be a false all-clear, and this
/// clause exists precisely so an agent can trust that answer.
fn apply_fork_status(
    grains: &mut Vec<CalGrainResult>,
    store: &dyn CalStoreFacade,
    filter: bool,
    restrict_to: Option<&std::collections::HashSet<(String, String, String)>>,
    exec_warnings: &mut Vec<String>,
) {
    let forks = match store.open_forks() {
        Ok(f) => f,
        Err(e) => {
            exec_warnings.push(format!(
                "contradiction detection unavailable ({e}); fork status not applied"
            ));
            if filter {
                grains.clear();
            }
            return;
        }
    };

    // hash -> every *other* live tip contesting the same key.
    let mut peers: HashMap<String, Vec<String>> = HashMap::new();
    for f in &forks {
        if let Some(keys) = restrict_to {
            if !keys.contains(&(
                f.namespace.clone(),
                f.subject.clone(),
                f.relation.clone(),
            )) {
                continue;
            }
        }
        for (i, head) in f.heads.iter().enumerate() {
            let others: Vec<String> = f
                .heads
                .iter()
                .enumerate()
                .filter(|(j, _)| *j != i)
                .map(|(_, o)| o.clone())
                .collect();
            peers.insert(head.clone(), others);
        }
    }

    if filter {
        grains.retain(|g| peers.contains_key(&g.hash));
    }
    for g in grains.iter_mut() {
        if let Some(others) = peers.get(&g.hash) {
            g.contested_by = Some(others.clone());
        }
    }
}

/// The `(namespace, subject, relation)` keys a `CONTRADICTIONS OF (...)`
/// sub-query selected.
///
/// Keyed on the full fork identity, not just `(subject, relation)`: the same
/// subject and relation in two namespaces are two different keys, and matching
/// on the pair alone would let a fork in one namespace be reported as in scope
/// because an unrelated grain in another namespace happened to share them.
/// A grain that does not carry its namespace is attributed to the namespace the
/// recall was scoped to, which is where it came from.
///
/// Grains with no subject or no relation (an Event, say) contribute no key and
/// so narrow nothing — the tail restricts the fork set, it never widens it.
fn contradiction_scope_keys(
    grains: &[CalGrainResult],
    default_ns: Option<&str>,
) -> std::collections::HashSet<(String, String, String)> {
    grains
        .iter()
        .filter_map(|g| {
            let s = g.fields.get("subject").and_then(|v| v.as_str())?;
            let r = g.fields.get("relation").and_then(|v| v.as_str())?;
            let ns = g
                .fields
                .get("namespace")
                .and_then(|v| v.as_str())
                .or(default_ns)
                .unwrap_or_default();
            Some((ns.to_string(), s.to_string(), r.to_string()))
        })
        .collect()
}

/// Extract the primary text content from a `CalGrainResult` for dedup comparison.
///
/// Mirrors the text extraction logic in `src/llm_rerank/mod.rs::extract_grain_text`
/// but operates on the JSON `fields` value instead of `DeserializedGrain`.
fn grain_result_text(grain: &CalGrainResult) -> String {
    // For facts: "subject relation object"
    if grain.grain_type == "fact" {
        let s = grain
            .fields
            .get("subject")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let r = grain
            .fields
            .get("relation")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let o = grain
            .fields
            .get("object")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        return format!("{} {} {}", s, r, o).trim().to_string();
    }
    // For other types: try common text fields.
    for field in &[
        "content",
        "description",
        "title",
        "goal",
        "query",
        "result",
        "output",
    ] {
        if let Some(v) = grain.fields.get(*field).and_then(|v| v.as_str()) {
            if !v.trim().is_empty() {
                return v.to_string();
            }
        }
    }
    grain.grain_type.clone()
}

/// Compute a simple text similarity score (Jaccard over word bigrams).
///
/// Returns a value in `[0.0, 1.0]`.  Used for threshold-based dedup in
/// `apply_assemble_post_merge_options`.  This is intentionally a lightweight
/// approximation — it does not need to match the engine's `find_similar_facts`
/// which uses TF-IDF cosine similarity.
fn text_similarity(a: &str, b: &str) -> f64 {
    if a == b {
        return 1.0;
    }
    let words_a: Vec<&str> = a.split_whitespace().collect();
    let words_b: Vec<&str> = b.split_whitespace().collect();
    if words_a.is_empty() && words_b.is_empty() {
        return 1.0;
    }
    if words_a.is_empty() || words_b.is_empty() {
        return 0.0;
    }
    // Bigram Jaccard.
    let bigrams = |words: &[&str]| -> std::collections::HashSet<(String, String)> {
        if words.len() < 2 {
            let mut s = std::collections::HashSet::new();
            s.insert((words[0].to_lowercase(), String::new()));
            return s;
        }
        words
            .windows(2)
            .map(|w| (w[0].to_lowercase(), w[1].to_lowercase()))
            .collect()
    };
    let set_a = bigrams(&words_a);
    let set_b = bigrams(&words_b);
    let intersection = set_a.intersection(&set_b).count();
    let union = set_a.union(&set_b).count();
    if union == 0 {
        return 1.0;
    }
    intersection as f64 / union as f64
}

/// Extract a string from a CAL `Value`.
///
/// # I-5 fix
///
/// `Value::Parameter` now returns `CalError::UnboundParameter` instead of
/// silently producing `"$name"`.  Parameters must be resolved before
/// execution; encountering one here means the caller forgot to bind it.
fn value_to_string(value: &Value) -> std::result::Result<String, CalError> {
    match value {
        Value::String { value } => Ok(value.clone()),
        Value::Hash { value } => Ok(value.clone()),
        Value::Parameter { name } => Err(CalError::UnboundParameter {
            name: name.clone(),
            span: None,
        }),
        other => Err(CalError::IncompatibleTypes {
            left: "string".into(),
            right: format!("{:?}", other),
            span: None,
            suggestion: Some("expected a quoted string value".into()),
        }),
    }
}

/// Extract a number from a CAL `Value`.
fn value_to_f64(value: &Value) -> std::result::Result<f64, CalError> {
    match value {
        Value::Number { value } => Ok(*value),
        other => Err(CalError::IncompatibleTypes {
            left: "number".into(),
            right: format!("{:?}", other),
            span: None,
            suggestion: Some("expected a numeric value".into()),
        }),
    }
}

/// Try to extract a hash string from a WHERE condition for EXISTS optimisation.
fn extract_hash_from_condition(condition: Option<&Condition>) -> Option<String> {
    match condition? {
        Condition::Comparison {
            field,
            comparator: Comparator::Eq,
            value: Value::Hash { value },
            ..
        } if field == "hash" => Some(value.clone()),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// FORMAT clause application (CAL spec v1.0.1)
// ---------------------------------------------------------------------------

/// Apply a `FormatClause` to a payload after pipeline stages.
///
/// Only applies to grain-bearing payloads (Grains, Assembled). Other payload
/// types (Exists, Count, History, etc.) pass through unchanged.
///
/// When `grouped_by` is `Some`, the grains have been reordered by `| GROUP BY`
/// and FORMAT renderers emit group headers.
/// The ambient inputs a FORMAT render needs beyond the grains themselves.
/// Grouped because all three stages of the format path take the same set, and
/// threading them one by one pushed each signature past what is readable.
#[derive(Clone, Copy)]
struct RenderInputs<'a> {
    /// `WITH VARS` bindings, for template rendering.
    user_vars: &'a HashMap<String, String>,
    /// Store access — templates are host metadata read through the facade.
    store: &'a dyn CalStoreFacade,
    /// OMS §4 `WITH progressive_disclosure(level)`. `None` = not asked for, and
    /// every render then keeps its historical output byte for byte.
    disclosure: Option<crate::render::Disclosure>,
}

/// The progressive-disclosure tier a `WITH` clause asks for (OMS §4).
///
/// The parser has already refused any level word outside
/// `summary | headlines | full`, so the fallthrough only ever catches the bare
/// `WITH progressive_disclosure` form — read as "everything", since `full` is
/// the only tier that adds anything a caller could be asking for.
fn disclosure_of(opts: &[super::ast::WithOption]) -> Option<crate::render::Disclosure> {
    opts.iter().find_map(|o| match o {
        super::ast::WithOption::ProgressiveDisclosure { level } => Some(match level.as_deref() {
            Some("summary") => crate::render::Disclosure::Summary,
            Some("headlines") => crate::render::Disclosure::Headlines,
            _ => crate::render::Disclosure::Full,
        }),
        _ => None,
    })
}

fn apply_format_clause(
    payload: CalResultPayload,
    format: &Option<FormatClause>,
    grouped_by: Option<&str>,
    inputs: RenderInputs<'_>,
    // `(context name, FOR intent)` — only an ASSEMBLE has them, and they feed
    // `{{assembly.name}}` / `{{assembly.intent}}`.
    assemble_ident: Option<(&str, &str)>,
    warnings: &mut Vec<String>,
) -> std::result::Result<CalResultPayload, CalError> {
    let Some(clause) = format else {
        return Ok(payload);
    };

    // Extract grains from the payload, plus the source structure when this
    // came from an ASSEMBLE — that structure is what SOURCE_BREAK,
    // ELEMENT_OMIT and the assembly./source./budget. namespaces render from,
    // and flattening it here is what previously made them unreachable.
    let (grains, assembled) = match &payload {
        CalResultPayload::Grains { grains, .. } => (grains, None),
        CalResultPayload::Assembled {
            grains,
            sources,
            total_tokens,
            budget_limit,
            ..
        } => (grains, Some((sources, *total_tokens, *budget_limit))),
        // Non-grain payloads pass through unchanged.
        _ => return Ok(payload),
    };

    let mut render_sources: Vec<super::templates::RenderSource<'_>> = Vec::new();
    let mut assembly_ctx: Option<super::templates::AssemblyContext> = None;

    if let Some((metas, total_tokens, budget_limit)) = assembled {
        // `AssembleEngine` concatenates each source's surviving grains in
        // order, so `grain_count` walks the boundaries back out.
        let mut offset = 0usize;
        for (i, m) in metas.iter().enumerate() {
            let end = (offset + m.grain_count).min(grains.len());
            render_sources.push(super::templates::RenderSource {
                label: &m.label,
                grains: &grains[offset..end],
                omitted: &m.omitted,
                priority: i + 1,
                tokens_used: m.tokens_used as usize,
                truncated: !m.omitted.is_empty(),
            });
            offset = end;
        }
        assembly_ctx = Some(super::templates::AssemblyContext {
            name: assemble_ident.map(|(n, _)| n.to_string()).unwrap_or_default(),
            intent: assemble_ident.map(|(_, i)| i.to_string()).unwrap_or_default(),
            source_count: metas.len(),
            grain_count: grains.len(),
            budget_total: budget_limit.unwrap_or(0) as u64,
            budget_used: total_tokens as u64,
            budget_unit: "tokens".to_string(),
        });
    }

    let plan = super::templates::RenderPlan {
        assembly: assembly_ctx.as_ref(),
        sources: (!render_sources.is_empty()).then_some(render_sources.as_slice()),
    };

    apply_format_clause_to_grains(
        grains, clause, grouped_by, inputs, &plan, warnings,
    )
}

/// Apply a `FormatClause` to a slice of grains, producing either
/// `Grains` (for single JSON without GROUP BY), `Formatted` (other single
/// formats), or `MultiFormatted` (multi-format list).
///
/// Special case: `FORMAT json` (single, non-grouped) returns the structured
/// `Grains` payload directly so that `result.grains` is a JSON array on the
/// wire — not a stringified `result.text`.  This fixes the "CAL RECALL
/// returns 0" confusion where clients parsed `result.grains` and found it
/// empty because the actual data was in `result.text`.
fn apply_format_clause_to_grains(
    grains: &[CalGrainResult],
    clause: &FormatClause,
    grouped_by: Option<&str>,
    inputs: RenderInputs<'_>,
    plan: &super::templates::RenderPlan<'_>,
    warnings: &mut Vec<String>,
) -> std::result::Result<CalResultPayload, CalError> {
    match clause {
        FormatClause::Single(super::ast::FormatSpec::Json) if grouped_by.is_none() => {
            // FORMAT json is a no-op for the JSON wire format: grains are
            // already serialisable.  Return Grains directly so that
            // `result.grains` is a structured array.
            Ok(CalResultPayload::Grains {
                grains: grains.to_vec(),
                total_available: Some(grains.len()),
            })
        }
        FormatClause::Single(spec) => {
            format_grain_results(
                grains, spec, grouped_by, inputs, plan, warnings,
            )
        }
        FormatClause::Multi(entries) => {
            let mut formats = HashMap::new();
            for entry in entries {
                let rendered =
                    format_grain_results(
                        grains, &entry.spec, grouped_by, inputs, plan, warnings,
                    )?;
                if let CalResultPayload::Formatted { text, format, .. } = rendered {
                    let key = entry.alias.clone().unwrap_or(format);
                    formats.insert(key, text);
                }
            }
            Ok(CalResultPayload::MultiFormatted {
                formats,
                grain_count: grains.len(),
                grains: grains.to_vec(),
            })
        }
    }
}

// ---------------------------------------------------------------------------
// FORMAT rendering (WI-1.1)
// ---------------------------------------------------------------------------

/// Render grains using the specified FORMAT clause.
///
/// Returns a `CalResultPayload::Formatted` with the rendered text and format
/// name. When `grouped_by` is `Some`, grains are assumed to already be in
/// group order (from `| GROUP BY`) and renderers emit group headers.
/// The shared-render view of a CAL result grain. `created_at` in result
/// fields is epoch milliseconds; the view carries seconds.
fn grain_view(grain: &CalGrainResult) -> crate::render::GrainView<'_> {
    crate::render::GrainView {
        grain_type: &grain.grain_type,
        hash: &grain.hash,
        fields: &grain.fields,
        created_at_sec: crate::render::created_at_sec_from_fields(&grain.fields),
    }
}

/// Disclosure tier for a template render (§10.5/§10.6): an ASSEMBLE token
/// budget squeezes per-grain rendering from ELEMENT toward ELEMENT_SUMMARY
/// via `select_tier`; a grain-unit budget or a plain RECALL renders Full.
fn template_tier(
    plan: &super::templates::RenderPlan<'_>,
    grain_count: usize,
) -> super::templates::DisclosureTier {
    match plan.assembly {
        Some(a) if a.budget_unit == "tokens" && a.budget_total > 0 => {
            super::templates::select_tier(a.budget_total.min(u32::MAX as u64) as u32, grain_count)
        }
        _ => super::templates::DisclosureTier::Full,
    }
}

fn format_grain_results(
    grains: &[CalGrainResult],
    format: &super::ast::FormatSpec,
    grouped_by: Option<&str>,
    inputs: RenderInputs<'_>,
    plan: &super::templates::RenderPlan<'_>,
    warnings: &mut Vec<String>,
) -> std::result::Result<CalResultPayload, CalError> {
    let RenderInputs { user_vars, store, disclosure } = inputs;
    let (text, format_name) = match format {
        super::ast::FormatSpec::Json => {
            if let Some(field) = grouped_by {
                let groups = collect_groups(grains, field);
                let json_groups: Vec<serde_json::Value> = groups
                    .iter()
                    .map(|(key, members)| {
                        serde_json::json!({
                            "group_key": key,
                            "count": members.len(),
                            "grains": members,
                        })
                    })
                    .collect();
                let json = serde_json::to_string_pretty(&json_groups).unwrap_or_default();
                (json, "json")
            } else {
                let json = serde_json::to_string_pretty(grains).unwrap_or_default();
                (json, "json")
            }
        }
        super::ast::FormatSpec::Markdown => {
            let mut md = String::new();
            if let Some(field) = grouped_by {
                let groups = collect_groups(grains, field);
                for (key, members) in &groups {
                    md.push_str(&format!(
                        "### {} ({} {})\n\n",
                        key,
                        members.len(),
                        if members.len() == 1 {
                            "memory"
                        } else {
                            "memories"
                        }
                    ));
                    for grain in members {
                        md.push_str(&crate::render::render_grain_markdown_at(
                            &grain_view(grain),
                            disclosure,
                        ));
                        md.push('\n');
                    }
                }
            } else {
                // No per-grain heading: a hash and a type repeated above every
                // line is noise in a prompt, and the assertion already names
                // what it is about.
                for grain in grains {
                    md.push_str(&crate::render::render_grain_markdown_at(
                        &grain_view(grain),
                        disclosure,
                    ));
                    md.push('\n');
                }
            }
            (md, "markdown")
        }
        super::ast::FormatSpec::Yaml => {
            // Simple YAML-like output.
            let mut yaml = String::new();
            for (i, grain) in grains.iter().enumerate() {
                yaml.push_str(&format!("- hash: \"{}\"\n", grain.hash));
                yaml.push_str(&format!("  grain_type: \"{}\"\n", grain.grain_type));
                if let serde_json::Value::Object(map) = &grain.fields {
                    yaml.push_str("  fields:\n");
                    for (k, v) in map {
                        yaml.push_str(&format!("    {}: {}\n", k, v));
                    }
                }
                if i < grains.len() - 1 {
                    yaml.push('\n');
                }
            }
            (yaml, "yaml")
        }
        super::ast::FormatSpec::Text => {
            let mut text = String::new();
            if let Some(field) = grouped_by {
                let groups = collect_groups(grains, field);
                let total_groups = groups.len();
                for (idx, (key, members)) in groups.iter().enumerate() {
                    text.push_str(&format!(
                        "--- Group {}/{}: {} ({} {}) ---\n",
                        idx + 1,
                        total_groups,
                        key,
                        members.len(),
                        if members.len() == 1 {
                            "memory"
                        } else {
                            "memories"
                        }
                    ));
                    for (i, grain) in members.iter().enumerate() {
                        text.push_str(&crate::render::render_grain_text_line(
                            &grain_view(grain),
                            Some(i + 1),
                        ));
                        text.push('\n');
                    }
                    if idx + 1 < total_groups {
                        text.push('\n');
                    }
                }
            } else {
                for grain in grains {
                    text.push_str(&crate::render::render_grain_text_line(
                        &grain_view(grain),
                        None,
                    ));
                    text.push('\n');
                }
            }
            (text, "text")
        }
        super::ast::FormatSpec::Sml => {
            // Semantic per-type elements via the shared renderer; the
            // `<grains>` / `<group>` envelope is this surface's own.
            let level = crate::render::MetadataDetail::Minimal;
            let mut sml = String::from("<grains>\n");
            if let Some(field) = grouped_by {
                let groups = collect_groups(grains, field);
                for (key, members) in &groups {
                    let escaped_key = crate::render::sml_escape(key);
                    sml.push_str(&format!(
                        "  <group key=\"{}\" count=\"{}\">\n",
                        escaped_key,
                        members.len()
                    ));
                    for grain in members {
                        sml.push_str("    ");
                        sml.push_str(&crate::render::render_grain_sml_at(
                            &grain_view(grain),
                            level,
                            disclosure,
                        ));
                        sml.push('\n');
                    }
                    sml.push_str("  </group>\n");
                }
            } else {
                for grain in grains {
                    sml.push_str("  ");
                    sml.push_str(&crate::render::render_grain_sml_at(
                        &grain_view(grain),
                        level,
                        disclosure,
                    ));
                    sml.push('\n');
                }
            }
            sml.push_str("</grains>");
            (sml, "sml")
        }
        super::ast::FormatSpec::Toon => {
            let views: Vec<crate::render::GrainView<'_>> = grains.iter().map(grain_view).collect();
            (crate::render::render_toon(&views), "toon")
        }
        super::ast::FormatSpec::Triples => {
            let mut triples = String::new();
            for grain in grains {
                if let serde_json::Value::Object(map) = &grain.fields {
                    let s = map.get("subject").and_then(|v| v.as_str()).unwrap_or("_");
                    let r = map.get("relation").and_then(|v| v.as_str()).unwrap_or("_");
                    let o = map.get("object").and_then(|v| v.as_str()).unwrap_or("_");
                    triples.push_str(&format!("{}\t{}\t{}\n", s, r, o));
                }
            }
            (triples, "triples")
        }
        super::ast::FormatSpec::Csv => {
            let mut csv = String::from("hash,grain_type,subject,relation,object,confidence\n");
            for grain in grains {
                let get_field = |f: &str| -> String {
                    json_field(&grain.fields, f)
                        .map(|v| match v {
                            serde_json::Value::String(s) => {
                                // Escape quotes and wrap if contains comma or newline.
                                if s.contains(',') || s.contains('"') || s.contains('\n') {
                                    format!("\"{}\"", s.replace('"', "\"\""))
                                } else {
                                    s.clone()
                                }
                            }
                            _ => v.to_string(),
                        })
                        .unwrap_or_default()
                };
                csv.push_str(&format!(
                    "{},{},{},{},{},{}\n",
                    grain.hash,
                    grain.grain_type,
                    get_field("subject"),
                    get_field("relation"),
                    get_field("object"),
                    get_field("confidence"),
                ));
            }
            (csv, "csv")
        }
        super::ast::FormatSpec::Table => {
            let header = "| hash | grain_type | subject | relation | object | confidence |";
            let separator = "| --- | --- | --- | --- | --- | --- |";
            let mut table = format!("{}\n{}\n", header, separator);
            for grain in grains {
                let get_field = |f: &str| -> String {
                    json_field(&grain.fields, f)
                        .map(|v| match v {
                            serde_json::Value::String(s) => s.replace('|', "\\|"),
                            _ => v.to_string(),
                        })
                        .unwrap_or_default()
                };
                table.push_str(&format!(
                    "| {} | {} | {} | {} | {} | {} |\n",
                    &grain.hash[..8.min(grain.hash.len())],
                    grain.grain_type,
                    get_field("subject"),
                    get_field("relation"),
                    get_field("object"),
                    get_field("confidence"),
                ));
            }
            (table, "table")
        }
        // `FORMAT TEMPLATE <name>` (§10.6) and the older `FORMAT preset "<name>"`
        // resolve identically — both name a registered template.
        super::ast::FormatSpec::Preset { name } | super::ast::FormatSpec::TemplateRef { name } => {
            // Look up the preset name in the template registry.
            let info = store
                .get_template(name)
                .ok_or_else(|| CalError::TemplateNotFound {
                    name: name.clone(),
                    span: None,
                })?;
            // Parse and render using the proper Mustache template engine.
            // `parse_template_any` recovers the §10.6 sectioned form, so a
            // sectioned template renders through the engine-driven pipeline
            // rather than being emitted as its own source text.
            let mut parsed = super::templates::parse_template_any(&info.source)?;

            // §10.7 inheritance. Registration validates the parent but the
            // render path never applied it, so EXTENDS was inert; the default
            // `readable` parent is what supplies ELEMENT_SUMMARY to a
            // sectioned template that only defines ELEMENT.
            let parent_name = info.parent.clone().or_else(|| {
                (parsed.is_sectioned() && !info.builtin).then(|| "readable".to_string())
            });
            if let Some(parent_name) = parent_name {
                if let Some(parent_info) = store.get_template(&parent_name) {
                    let parent = super::templates::parse_template_any(&parent_info.source)?;
                    parsed = super::templates::merge_templates(&parent, &parsed);
                }
            }
            let user_vars_map: std::collections::HashMap<String, String> = user_vars
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();
            let ctx = super::templates::RenderContext {
                now_secs: std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs() as i64)
                    .unwrap_or(0),
                tier: template_tier(plan, grains.len()),
                total_count: grains.len(),
                user_vars: user_vars_map,
            };
            let rendered = match parsed.sections() {
                Some(sections) => super::templates::render_sectioned(
                    sections,
                    plan.sources
                        .unwrap_or(&[super::templates::RenderSource::single(grains)]),
                    &ctx,
                    plan.assembly,
                    super::templates::MAX_RENDER_OUTPUT_SIZE,
                )?,
                None => {
                    // §10.8 caps {{#each}} at 200. Truncating quietly would
                    // read as "these are all the grains", so say so.
                    if let Some((rendered, total)) =
                        super::templates::each_iteration_cap(&parsed, grains.len())
                    {
                        warnings.push(
                            super::errors::CalWarning::EachIterationCapped {
                                rendered,
                                total,
                                max: super::templates::MAX_EACH_ITERATIONS,
                            }
                            .to_string(),
                        );
                    }
                    super::templates::render(&parsed, grains, &ctx)?
                }
            };
            store.record_template_run(name);
            let key = match format {
                super::ast::FormatSpec::Preset { .. } => "preset",
                _ => "template",
            };
            (rendered, key)
        }
        super::ast::FormatSpec::Template { template } => {
            // `FORMAT TEMPLATE "<text>"` is an inline template in the §10.6.1
            // ELEMENT shorthand: the string renders one grain and the engine
            // iterates. It goes through the same parser, variable set and
            // limits as a registered template — this used to be naive string
            // substitution, which silently ignored filters, conditionals and
            // the closed variable set.
            let user_vars_map: std::collections::HashMap<String, String> = user_vars
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();
            let ctx = super::templates::RenderContext {
                now_secs: std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs() as i64)
                    .unwrap_or(0),
                tier: template_tier(plan, grains.len()),
                total_count: grains.len(),
                user_vars: user_vars_map,
            };
            let sections = super::templates::TemplateSections {
                element: Some(super::templates::parse_section(template)?),
                ..Default::default()
            };
            let rendered = super::templates::render_sectioned(
                &sections,
                plan.sources
                    .unwrap_or(&[super::templates::RenderSource::single(grains)]),
                &ctx,
                plan.assembly,
                super::templates::MAX_RENDER_OUTPUT_SIZE,
            )?;
            (rendered, "template")
        }
        super::ast::FormatSpec::TemplateInline { sections } => {
            // `FORMAT TEMPLATE { HEADER { ... } ELEMENT { ... } }` — the same
            // section pipeline as a registered template, defined at the point
            // of use.
            let user_vars_map: std::collections::HashMap<String, String> = user_vars
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();
            let ctx = super::templates::RenderContext {
                now_secs: std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs() as i64)
                    .unwrap_or(0),
                tier: template_tier(plan, grains.len()),
                total_count: grains.len(),
                user_vars: user_vars_map,
            };
            let parsed = super::templates::parse_sections(sections)?;
            let rendered = super::templates::render_sectioned(
                &parsed,
                plan.sources
                    .unwrap_or(&[super::templates::RenderSource::single(grains)]),
                &ctx,
                plan.assembly,
                super::templates::MAX_RENDER_OUTPUT_SIZE,
            )?;
            (rendered, "template")
        }
    };

    Ok(CalResultPayload::Formatted {
        text,
        format: format_name.to_string(),
        grain_count: grains.len(),
        grains: grains.to_vec(),
    })
}

// ---------------------------------------------------------------------------
// GROUP BY helpers
// ---------------------------------------------------------------------------

/// Group grains by field value, reorder so same-value grains are contiguous,
/// sorted chronologically within each group. Groups ordered by earliest
/// `created_at_sec`.
fn group_grains_by_field(grains: Vec<CalGrainResult>, field: &str) -> Vec<CalGrainResult> {
    // Collect grains into groups keyed by the field value.
    let mut groups: BTreeMap<String, Vec<CalGrainResult>> = BTreeMap::new();
    for grain in grains {
        let key = json_field(&grain.fields, field)
            .map(|v| match v {
                serde_json::Value::String(s) => s.clone(),
                _ => v.to_string(),
            })
            .unwrap_or_default();
        groups.entry(key).or_default().push(grain);
    }

    // Sort within each group by created_at_sec ascending.
    for members in groups.values_mut() {
        members.sort_by(|a, b| {
            let ta = json_field(&a.fields, "created_at_sec").and_then(|v| v.as_f64());
            let tb = json_field(&b.fields, "created_at_sec").and_then(|v| v.as_f64());
            ta.partial_cmp(&tb).unwrap_or(Ordering::Equal)
        });
    }

    // Order groups by the earliest created_at_sec in each group.
    let mut group_vec: Vec<(String, Vec<CalGrainResult>)> = groups.into_iter().collect();
    group_vec.sort_by(|a, b| {
        let earliest_a =
            a.1.first()
                .and_then(|g| json_field(&g.fields, "created_at_sec").and_then(|v| v.as_f64()));
        let earliest_b =
            b.1.first()
                .and_then(|g| json_field(&g.fields, "created_at_sec").and_then(|v| v.as_f64()));
        earliest_a
            .partial_cmp(&earliest_b)
            .unwrap_or(Ordering::Equal)
    });

    // Move empty-key group to the end.
    if let Some(pos) = group_vec.iter().position(|(k, _)| k.is_empty()) {
        let empty = group_vec.remove(pos);
        group_vec.push(empty);
    }

    // Flatten.
    group_vec
        .into_iter()
        .flat_map(|(_, members)| members)
        .collect()
}

/// Collect already-grouped grains into `(key, members)` pairs by detecting
/// contiguous runs of the same field value.
fn collect_groups<'a>(
    grains: &'a [CalGrainResult],
    field: &str,
) -> Vec<(String, Vec<&'a CalGrainResult>)> {
    let mut groups: Vec<(String, Vec<&CalGrainResult>)> = Vec::new();
    for grain in grains {
        let key = json_field(&grain.fields, field)
            .map(|v| match v {
                serde_json::Value::String(s) => s.clone(),
                _ => v.to_string(),
            })
            .unwrap_or_default();
        if let Some(last) = groups.last_mut() {
            if last.0 == key {
                last.1.push(grain);
                continue;
            }
        }
        groups.push((key, vec![grain]));
    }
    groups
}

// ---------------------------------------------------------------------------
// Grain-type-specific field validation and filtering (WI-1.6)
// ---------------------------------------------------------------------------

/// Common fields shared by all grain types. These are handled directly by
/// `apply_where_clause` and do not need post-retrieval filtering. Aligned
/// with OMS §5.2 common-field set plus Areev-specific extensions retained
/// for backward compatibility with persisted queries (`created_at`,
/// `summary`, `content`, `grain_type`, plus four cross-grain extensions
/// `scope`/`scope_path`/`priority`/`status` that pre-date the spec audit).
const COMMON_FIELDS: &[&str] = &[
    // Spec §5.2 — 18 fields.
    "subject",
    "relation",
    "object",
    "namespace",
    "user_id",
    "confidence",
    "importance",
    "score",
    "tags",
    "type",
    "time",
    "hash",
    "contradicted",
    "verification_status",
    "source_type",
    "recall_priority",
    "epistemic_status",
    "query",
    // Areev extensions (pre-spec; kept for back-compat).
    "created_at",
    "content",
    "summary",
    "grain_type",
    "scope",
    "scope_path",
    "priority",
    "status",
];

/// Return the known type-specific fields for a grain type plural name.
///
/// These fields are NOT part of the engine's `RecallParams` and must be
/// filtered post-retrieval by inspecting the grain's `fields` JSON. Each
/// list combines the spec-mandated fields (per OMS §6.3) with Areev
/// implementation extensions (e.g. `session_id` cross-cutting marker,
/// `nodes`/`bindings` plural aliases) kept for backward compatibility.
fn type_specific_fields(grain_type: &GrainTypePlural) -> &'static [&'static str] {
    // Data-only — sourced from the grain-type registry (D1). The wildcard has
    // no single type, so it lists no type-specific fields.
    match grain_type.to_grain_type() {
        Some(ty) => areev_core::types::registry::meta(ty).queryable_fields,
        None => &[],
    }
}

/// Check if a field is a known type-specific field for ANY grain type.
fn is_known_type_specific_field(field: &str) -> bool {
    // Sourced from the registry (D1) — every type's queryable fields.
    areev_core::types::registry::GRAIN_TYPES
        .iter()
        .any(|m| m.queryable_fields.contains(&field))
}

/// Suggest the closest valid field name for a given unknown field on a grain type.
fn suggest_field(field: &str, grain_type: &GrainTypePlural) -> Option<String> {
    let valid_fields = type_specific_fields(grain_type);
    for known in valid_fields {
        // Simple Levenshtein-like: if they share a common prefix or one contains the other.
        if known.contains(field) || field.contains(known) {
            return Some(known.to_string());
        }
    }
    // Check if the field is valid on a different grain type.
    if is_known_type_specific_field(field) {
        return Some(format!(
            "'{}' exists on a different grain type, not on {}",
            field,
            grain_type.as_str()
        ));
    }
    None
}

/// Extract type-specific field conditions from a WHERE clause.
///
/// Returns a Vec of (field, comparator, value) tuples for conditions that
/// reference fields NOT in `COMMON_FIELDS`. These need post-retrieval
/// filtering because they cannot be pushed down into `RecallParams`.
fn extract_type_specific_conditions(condition: &Condition) -> Vec<(String, Comparator, Value)> {
    let mut result = Vec::new();
    extract_type_specific_conditions_inner(condition, &mut result);
    result
}

fn extract_type_specific_conditions_inner(
    condition: &Condition,
    result: &mut Vec<(String, Comparator, Value)>,
) {
    match condition {
        Condition::Comparison {
            field,
            comparator,
            value,
            ..
            // `hash` is listed in COMMON_FIELDS but no structural filter
            // consumes it — it lives on the envelope, not in `RecallParams`.
            // Skipping it here meant `WHERE hash = "<a real address>"` matched
            // nothing structurally and filtered nothing afterwards, so it
            // returned the entire result set. Route it to the post-filter,
            // which resolves envelope fields.
        } if !COMMON_FIELDS.contains(&field.as_str()) || field == "hash" => {
            result.push((field.clone(), *comparator, value.clone()));
        }
        Condition::And { left, right, .. } => {
            extract_type_specific_conditions_inner(left, result);
            extract_type_specific_conditions_inner(right, result);
        }
        Condition::Or { left, right, .. } => {
            extract_type_specific_conditions_inner(left, result);
            extract_type_specific_conditions_inner(right, result);
        }
        Condition::Not { inner, .. } => {
            extract_type_specific_conditions_inner(inner, result);
        }
        _ => {}
    }
}

/// Type-specific IN/NOT IN conditions extracted from a WHERE clause.
/// Kept separate from `extract_type_specific_conditions` so callers that
/// only care about scalar comparisons stay simple.
struct TypeSpecificSetCondition {
    field: String,
    values: Vec<Value>,
    negated: bool,
}

fn extract_type_specific_set_conditions(condition: &Condition) -> Vec<TypeSpecificSetCondition> {
    let mut result = Vec::new();
    extract_type_specific_set_conditions_inner(condition, &mut result);
    result
}

fn extract_type_specific_set_conditions_inner(
    condition: &Condition,
    result: &mut Vec<TypeSpecificSetCondition>,
) {
    match condition {
        Condition::In { field, values, .. }
            // The apply_where_clause path already handles `subject`/`relation`/
            // `object`/`tags`/`namespace` at engine-level; other fields need
            // post-retrieval filtering.
            if !matches!(
                field.as_str(),
                "subject" | "relation" | "object" | "tags" | "namespace"
            ) => {
                result.push(TypeSpecificSetCondition {
                    field: field.clone(),
                    values: values.clone(),
                    negated: false,
                });
            }
        Condition::NotIn { field, values, .. }
            if field.as_str() != "tags" => {
                result.push(TypeSpecificSetCondition {
                    field: field.clone(),
                    values: values.clone(),
                    negated: true,
                });
            }
        Condition::And { left, right, .. } => {
            extract_type_specific_set_conditions_inner(left, result);
            extract_type_specific_set_conditions_inner(right, result);
        }
        Condition::Or { left, right, .. } => {
            extract_type_specific_set_conditions_inner(left, result);
            extract_type_specific_set_conditions_inner(right, result);
        }
        Condition::Not { inner, .. } => {
            extract_type_specific_set_conditions_inner(inner, result);
        }
        _ => {}
    }
}

fn grain_matches_set_condition(grain: &CalGrainResult, cond: &TypeSpecificSetCondition) -> bool {
    let any_match = cond
        .values
        .iter()
        .any(|v| grain_matches_condition(grain, &cond.field, &Comparator::Eq, v));
    if cond.negated {
        !any_match
    } else {
        any_match
    }
}

/// Apply a type-specific field condition to a single grain result.
///
/// Returns `true` if the grain matches the condition.
pub fn grain_matches_condition(
    grain: &CalGrainResult,
    field: &str,
    comparator: &Comparator,
    value: &Value,
) -> bool {
    // Envelope fields are not in `fields` — a grain's content address is a
    // property *of* the blob, so it cannot be inside it. Looking `hash` up in
    // `fields` therefore always missed, which is why `hash IN ("<real hash>")`
    // matched nothing. A `sha256:` prefix is accepted because that is how the
    // rest of CAL spells an address.
    let envelope: Option<serde_json::Value> = match field {
        "hash" => Some(serde_json::Value::String(grain.hash.clone())),
        "grain_type" => Some(serde_json::Value::String(grain.grain_type.clone())),
        _ => None,
    };
    let grain_value = match &envelope {
        Some(v) => Some(v),
        None => json_field(&grain.fields, field),
    };
    let value = &match (field, value) {
        ("hash", Value::String { value: v }) => Value::String {
            value: v.strip_prefix("sha256:").unwrap_or(v).to_string(),
        },
        _ => value.clone(),
    };

    match comparator {
        Comparator::Eq => match value {
            Value::String { value: target } => grain_value
                .and_then(|v| v.as_str())
                .map(|s| s == target.as_str())
                .unwrap_or(false),
            Value::Number { value: target } => grain_value
                .and_then(|v| v.as_f64())
                .map(|n| (n - target).abs() < f64::EPSILON)
                .unwrap_or(false),
            Value::Boolean { value: target } => grain_value
                .and_then(|v| v.as_bool())
                .map(|b| b == *target)
                .unwrap_or(false),
            _ => false,
        },
        Comparator::NotEq => !grain_matches_condition(grain, field, &Comparator::Eq, value),
        Comparator::Gte => match value {
            Value::Number { value: target } => grain_value
                .and_then(|v| v.as_f64())
                .map(|n| n >= *target)
                .unwrap_or(false),
            _ => false,
        },
        Comparator::Gt => match value {
            Value::Number { value: target } => grain_value
                .and_then(|v| v.as_f64())
                .map(|n| n > *target)
                .unwrap_or(false),
            _ => false,
        },
        Comparator::Lte => match value {
            Value::Number { value: target } => grain_value
                .and_then(|v| v.as_f64())
                .map(|n| n <= *target)
                .unwrap_or(false),
            _ => false,
        },
        Comparator::Lt => match value {
            Value::Number { value: target } => grain_value
                .and_then(|v| v.as_f64())
                .map(|n| n < *target)
                .unwrap_or(false),
            _ => false,
        },
    }
}

/// Evaluate a full `Condition` tree against a single grain result.
///
/// Public because it is the ONE boolean evaluator in the workspace, and a
/// second implementation would be a second set of semantics. `areev-trigger`
/// uses it for memory-trigger predicates and composite gates: the `Condition`
/// AST already has And/Or/Not with parenthesised grouping and the right
/// precedence, it already serializes, and it is total — a missing field is
/// false rather than an error. Reusing it also keeps `areev_run_core::cond`'s
/// standing exclusion on expression languages intact, because nothing new is
/// parsed.
///
/// Note this is the AUTHORITATIVE match. The structural push-down in
/// `apply_where_clause` drops OR and NOT with a warning, so it *widens*; a
/// caller that trusts the recall result alone gets more than it asked for.
///
/// Used by `PipelineStage::Filter` (post-pipeline WHERE) to filter grains
/// by conditions after pipeline stages like SELECT have been applied.
pub fn grain_matches_condition_tree(grain: &CalGrainResult, condition: &Condition) -> bool {
    match condition {
        Condition::Comparison {
            field,
            comparator,
            value,
            ..
        } => grain_matches_condition(grain, field, comparator, value),
        Condition::And { left, right, .. } => {
            grain_matches_condition_tree(grain, left) && grain_matches_condition_tree(grain, right)
        }
        Condition::Or { left, right, .. } => {
            grain_matches_condition_tree(grain, left) || grain_matches_condition_tree(grain, right)
        }
        Condition::Not { inner, .. } => !grain_matches_condition_tree(grain, inner),
        Condition::In { field, values, .. } => values
            .iter()
            .any(|v| grain_matches_condition(grain, field, &Comparator::Eq, v)),
        Condition::NotIn { field, values, .. } => !values
            .iter()
            .any(|v| grain_matches_condition(grain, field, &Comparator::Eq, v)),
        Condition::IsNull { field, .. } => {
            json_field(&grain.fields, field).is_none()
                || json_field(&grain.fields, field) == Some(&serde_json::Value::Null)
        }
        Condition::IsNotNull { field, .. } => {
            matches!(json_field(&grain.fields, field), Some(v) if !v.is_null())
        }
        Condition::Contains { field, value, .. } => json_field(&grain.fields, field)
            .and_then(|v| v.as_str())
            .map(|s| s.contains(value.as_str()))
            .unwrap_or(false),
        Condition::StartsWith { field, value, .. } => json_field(&grain.fields, field)
            .and_then(|v| v.as_str())
            .map(|s| s.starts_with(value.as_str()))
            .unwrap_or(false),
        Condition::IsCategory {
            field, category, ..
        } => json_field(&grain.fields, field)
            .and_then(|v| v.as_str())
            .map(|s| s.eq_ignore_ascii_case(category))
            .unwrap_or(false),
    }
}

/// Extract ALL comparison conditions from a WHERE clause (common + type-specific).
///
/// Used by ASSEMBLE WHERE for post-composition filtering where every
/// condition must be checked against the assembled grain set.
fn collect_common_conditions(condition: &Condition, result: &mut Vec<(String, Comparator, Value)>) {
    match condition {
        Condition::Comparison {
            field,
            comparator,
            value,
            ..
        }
            // Include common fields that ASSEMBLE needs for post-filtering.
            if COMMON_FIELDS.contains(&field.as_str()) => {
                result.push((field.clone(), *comparator, value.clone()));
            }
        Condition::And { left, right, .. } => {
            collect_common_conditions(left, result);
            collect_common_conditions(right, result);
        }
        _ => {}
    }
}

/// Collect field names referenced in a condition (for EXPLAIN plans).
/// Records only names, never values (S-5).
fn collect_filter_names(condition: &Condition, names: &mut Vec<String>) {
    match condition {
        Condition::Comparison { field, .. } => {
            if !names.contains(field) {
                names.push(field.clone());
            }
        }
        Condition::In { field, .. } | Condition::NotIn { field, .. } => {
            if !names.contains(field) {
                names.push(field.clone());
            }
        }
        Condition::IsNull { field, .. } | Condition::IsNotNull { field, .. } => {
            if !names.contains(field) {
                names.push(field.clone());
            }
        }
        Condition::Contains { field, .. } | Condition::StartsWith { field, .. } => {
            if !names.contains(field) {
                names.push(field.clone());
            }
        }
        Condition::IsCategory { field, .. } => {
            if !names.contains(field) {
                names.push(field.clone());
            }
        }
        Condition::And { left, right, .. } | Condition::Or { left, right, .. } => {
            collect_filter_names(left, names);
            collect_filter_names(right, names);
        }
        Condition::Not { inner, .. } => {
            collect_filter_names(inner, names);
        }
    }
}

/// Get a field value from a grain's JSON fields object.
/// Collapse a JSON field value to the string key `WITH dedup(<field>)` groups
/// on. Strings compare as themselves; anything else by its canonical JSON, so
/// `1` and `"1"` stay distinct rather than colliding through `to_string`.
fn value_dedup_key(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

fn json_field<'a>(fields: &'a serde_json::Value, field: &str) -> Option<&'a serde_json::Value> {
    if let serde_json::Value::Object(map) = fields {
        map.get(field)
    } else {
        None
    }
}

/// Project a subset of fields from a JSON value.
fn project_fields(fields: &serde_json::Value, selected: &[String]) -> serde_json::Value {
    let mut out = serde_json::Map::new();
    if let serde_json::Value::Object(map) = fields {
        for key in selected {
            if let Some(v) = map.get(key) {
                out.insert(key.clone(), v.clone());
            }
        }
    }
    serde_json::Value::Object(out)
}

/// Total ordering for JSON values (used for ORDER BY).
fn compare_json_values(a: Option<&serde_json::Value>, b: Option<&serde_json::Value>) -> Ordering {
    match (a, b) {
        (None, None) => Ordering::Equal,
        (None, Some(_)) => Ordering::Less,
        (Some(_), None) => Ordering::Greater,
        (Some(va), Some(vb)) => {
            // Compare numbers as f64, strings lexicographically, booleans as int.
            if let (Some(fa), Some(fb)) = (va.as_f64(), vb.as_f64()) {
                fa.partial_cmp(&fb).unwrap_or(Ordering::Equal)
            } else if let (Some(sa), Some(sb)) = (va.as_str(), vb.as_str()) {
                sa.cmp(sb)
            } else if let (Some(ba), Some(bb)) = (va.as_bool(), vb.as_bool()) {
                ba.cmp(&bb)
            } else {
                // Fallback: compare display representations.
                va.to_string().cmp(&vb.to_string())
            }
        }
    }
}

/// Union of two grain result sets (deduplicated by hash).
fn union_grains(mut left: Vec<CalGrainResult>, right: Vec<CalGrainResult>) -> Vec<CalGrainResult> {
    let left_hashes: std::collections::HashSet<String> =
        left.iter().map(|g| g.hash.clone()).collect();
    for g in right {
        if !left_hashes.contains(&g.hash) {
            left.push(g);
        }
    }
    left
}

/// Intersection of two grain result sets (grains present in both, by hash).
fn intersect_grains(left: Vec<CalGrainResult>, right: &[CalGrainResult]) -> Vec<CalGrainResult> {
    let right_hashes: std::collections::HashSet<&str> =
        right.iter().map(|g| g.hash.as_str()).collect();
    left.into_iter()
        .filter(|g| right_hashes.contains(g.hash.as_str()))
        .collect()
}

/// Difference of two grain result sets (grains in left but not in right).
fn except_grains(left: Vec<CalGrainResult>, right: &[CalGrainResult]) -> Vec<CalGrainResult> {
    let right_hashes: std::collections::HashSet<&str> =
        right.iter().map(|g| g.hash.as_str()).collect();
    left.into_iter()
        .filter(|g| !right_hashes.contains(g.hash.as_str()))
        .collect()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::facade::CalStoreFacade;
    use crate::store_types::{RecallParams, SearchHit};
    use crate::store_types::VersionEntry;
    use areev_core::error::{AreevError, Hash};
    use areev_core::format::deserialize::DeserializedGrain;
    use areev_core::format::header::MgHeader;
    use areev_core::types::GrainType;
    use std::collections::HashMap;

    // -----------------------------------------------------------------------
    // Shared mock store
    // -----------------------------------------------------------------------

    struct MockStore {
        grains: Vec<(Hash, DeserializedGrain)>,
    }

    impl MockStore {
        fn empty() -> Self {
            Self { grains: Vec::new() }
        }

        fn with_grains(grains: Vec<(Hash, DeserializedGrain)>) -> Self {
            Self { grains }
        }
    }

    /// Build a minimal Fact grain with a given subject, returning (hash, grain).
    fn make_fact(subject: &str, relation: &str, object: &str) -> (Hash, DeserializedGrain) {
        let mut fields: HashMap<String, serde_json::Value> = HashMap::new();
        fields.insert("subject".into(), serde_json::json!(subject));
        fields.insert("relation".into(), serde_json::json!(relation));
        fields.insert("object".into(), serde_json::json!(object));
        fields.insert("grain_type".into(), serde_json::json!("fact"));
        fields.insert("confidence".into(), serde_json::json!(0.9));

        // Build a deterministic hash from subject bytes.
        let mut hash_bytes = [0u8; 32];
        let key = format!("{}|{}|{}", subject, relation, object);
        for (i, b) in key.as_bytes().iter().enumerate().take(32) {
            hash_bytes[i] = *b;
        }
        let hash = Hash::from_bytes(&hash_bytes);

        let grain = DeserializedGrain {
            header: MgHeader {
                version: 1,
                flags: 0,
                grain_type: 0x01,
                ns_hash: 0,
                created_at_sec: 0,
            },
            grain_type: GrainType::Fact,
            fields,
            hash,
        };
        (hash, grain)
    }

    impl CalStoreFacade for MockStore {
        fn recall(&self, params: &RecallParams) -> areev_core::error::Result<Vec<SearchHit>> {
            let mut hits: Vec<SearchHit> = self
                .grains
                .iter()
                .filter(|(_, g)| {
                    if let Some(ref s) = params.subject {
                        if g.get_str("subject") != Some(s.as_str()) {
                            return false;
                        }
                    }
                    if let Some(ref r) = params.relation {
                        if g.get_str("relation") != Some(r.as_str()) {
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

        fn exists(&self, hash: &Hash) -> areev_core::error::Result<bool> {
            Ok(self.grains.iter().any(|(h, _)| h == hash))
        }

        fn get(&self, hash: &Hash) -> areev_core::error::Result<DeserializedGrain> {
            self.grains
                .iter()
                .find(|(h, _)| h == hash)
                .map(|(_, g)| g.clone())
                .ok_or(AreevError::NotFound(*hash))
        }

        fn count(&self) -> areev_core::error::Result<usize> {
            Ok(self.grains.len())
        }

        fn get_history(
            &self,
            _ns: &str,
            _s: &str,
            _r: &str,
        ) -> areev_core::error::Result<Vec<VersionEntry>> {
            Ok(Vec::new())
        }

        fn default_namespace(&self) -> Option<&str> {
            None
        }

        fn active_user(&self) -> Option<&str> {
            None
        }

        fn cal_add(
            &self,
            _grain_type: &str,
            _fields: &serde_json::Map<String, serde_json::Value>,
        ) -> areev_core::error::Result<Hash> {
            Err(AreevError::Validation(
                "mock: cal_add not implemented".into(),
            ))
        }

        fn cal_supersede(
            &self,
            _old_hash: &Hash,
            _grain_type: &str,
            _fields: &serde_json::Map<String, serde_json::Value>,
        ) -> areev_core::error::Result<Hash> {
            Err(AreevError::Validation(
                "mock: cal_supersede not implemented".into(),
            ))
        }

        fn list_templates(&self) -> Vec<crate::facade::TemplateInfo> {
            let registry = crate::templates::TemplateRegistry::new();
            registry.list()
        }
    }

    // -----------------------------------------------------------------------
    // Helper to build an executor with defaults.
    // -----------------------------------------------------------------------

    fn exec() -> CalExecutor {
        CalExecutor::with_defaults()
    }

    // -----------------------------------------------------------------------
    // Test 1: Execute a simple RECALL (empty store returns empty grains).
    // -----------------------------------------------------------------------

    #[test]
    fn test_execute_recall_empty_store() {
        let store = MockStore::empty();
        let ex = exec();
        let result = ex.execute("RECALL facts", &store).unwrap();
        assert_eq!(result.metadata.statement_type, "recall");
        match result.result {
            CalResultPayload::Grains { grains, .. } => assert!(grains.is_empty()),
            other => panic!("unexpected payload: {:?}", other),
        }
    }

    // -----------------------------------------------------------------------
    // Test 2: RECALL with WHERE subject = "john" matches correctly.
    // -----------------------------------------------------------------------

    #[test]
    fn test_execute_recall_where_subject() {
        let (hash_a, grain_a) = make_fact("john", "likes", "coffee");
        let (hash_b, grain_b) = make_fact("bob", "likes", "coffee");
        let store = MockStore::with_grains(vec![(hash_a, grain_a), (hash_b, grain_b)]);
        let ex = exec();
        let result = ex
            .execute(r#"RECALL facts WHERE subject = "john""#, &store)
            .unwrap();
        match result.result {
            CalResultPayload::Grains { grains, .. } => {
                assert_eq!(grains.len(), 1);
                assert_eq!(grains[0].hash, hash_a.to_hex());
            }
            other => panic!("unexpected payload: {:?}", other),
        }
    }

    // -----------------------------------------------------------------------
    // Test 3: RECALL with ABOUT clause sets query param.
    // -----------------------------------------------------------------------

    #[test]
    fn test_execute_recall_about_clause() {
        let store = MockStore::empty();
        let ex = exec();
        // Should not fail even if no FTS; mock returns empty.
        let result = ex
            .execute(r#"RECALL facts ABOUT "coffee preferences""#, &store)
            .unwrap();
        assert_eq!(result.metadata.statement_type, "recall");
    }

    // -----------------------------------------------------------------------
    // Test 4: RECALL with pipeline LIMIT.
    // -----------------------------------------------------------------------

    #[test]
    fn test_execute_recall_pipeline_limit() {
        let grains: Vec<_> = (0..10u8)
            .map(|i| {
                let key = format!("user{}", i);
                make_fact(&key, "likes", "coffee")
            })
            .collect();
        let store = MockStore::with_grains(grains);
        let ex = exec();
        let result = ex.execute("RECALL facts LIMIT 3", &store).unwrap();
        match result.result {
            CalResultPayload::Grains { grains, .. } => assert_eq!(grains.len(), 3),
            other => panic!("unexpected: {:?}", other),
        }
    }

    // -----------------------------------------------------------------------
    // Test 5: RECALL with pipeline COUNT.
    // -----------------------------------------------------------------------

    #[test]
    fn test_execute_recall_pipeline_count() {
        let grains: Vec<_> = (0..5u8)
            .map(|i| make_fact(&format!("u{}", i), "likes", "tea"))
            .collect();
        let store = MockStore::with_grains(grains);
        let ex = exec();
        let result = ex.execute("RECALL facts COUNT", &store).unwrap();
        match result.result {
            CalResultPayload::Count { count } => assert_eq!(count, 5),
            other => panic!("unexpected: {:?}", other),
        }
    }

    // -----------------------------------------------------------------------
    // Test 6: RECALL with pipeline ORDER BY field.
    // -----------------------------------------------------------------------

    #[test]
    fn test_execute_recall_pipeline_order_by() {
        let mut grains = vec![
            make_fact("charlie", "likes", "rust"),
            make_fact("john", "likes", "python"),
            make_fact("bob", "likes", "go"),
        ];
        // Assign predictable created_at values so ORDER BY works.
        for (i, (_, g)) in grains.iter_mut().enumerate() {
            g.fields
                .insert("created_at".into(), serde_json::json!(i as i64));
        }
        let store = MockStore::with_grains(grains);
        let ex = exec();
        let result = ex.execute("RECALL facts ORDER BY subject", &store).unwrap();
        match result.result {
            CalResultPayload::Grains { grains, .. } => {
                // Should be sorted A-Z by subject: bob < charlie < john.
                assert_eq!(grains[0].hash, make_fact("bob", "likes", "go").0.to_hex());
                assert_eq!(
                    grains[1].hash,
                    make_fact("charlie", "likes", "rust").0.to_hex()
                );
            }
            other => panic!("unexpected: {:?}", other),
        }
    }

    // -----------------------------------------------------------------------
    // Test 7: RECALL with pipeline SELECT (field projection).
    // -----------------------------------------------------------------------

    #[test]
    fn test_execute_recall_pipeline_select() {
        let (hash, _) = make_fact("john", "likes", "vim");
        let store = MockStore::with_grains(vec![make_fact("john", "likes", "vim")]);
        let ex = exec();
        let result = ex
            .execute("RECALL facts SELECT subject, object", &store)
            .unwrap();
        match result.result {
            CalResultPayload::Grains { grains, .. } => {
                assert_eq!(grains.len(), 1);
                if let serde_json::Value::Object(map) = &grains[0].fields {
                    assert!(map.contains_key("subject"));
                    assert!(map.contains_key("object"));
                    // "relation" should not be present after SELECT.
                    assert!(!map.contains_key("relation"));
                } else {
                    panic!("expected Object fields");
                }
                let _ = hash;
            }
            other => panic!("unexpected: {:?}", other),
        }
    }

    // -----------------------------------------------------------------------
    // Test 8: EXISTS with known hash returns true.
    //
    // CAL syntax: `EXISTS sha256:<hex>` (direct hash lookup form).
    // The parser desugars this to ExistsStmt with WHERE hash = <hash>.
    // -----------------------------------------------------------------------

    #[test]
    fn test_execute_exists_known_hash() {
        let (hash, grain) = make_fact("john", "is", "a developer");
        let store = MockStore::with_grains(vec![(hash, grain)]);
        let ex = exec();
        let hex = hash.to_hex();
        // CAL hash literals use the `sha256:` prefix.
        let query = format!("EXISTS sha256:{}", hex);
        let result = ex.execute(&query, &store).unwrap();
        match result.result {
            CalResultPayload::Exists { exists, .. } => assert!(exists),
            other => panic!("unexpected: {:?}", other),
        }
    }

    // -----------------------------------------------------------------------
    // Test 9: EXISTS with unknown hash returns false.
    //
    // CAL syntax: `EXISTS sha256:<hex>` with a hash not in the store.
    // -----------------------------------------------------------------------

    #[test]
    fn test_execute_exists_unknown_hash() {
        let store = MockStore::empty();
        let ex = exec();
        // 64 hex chars = 32 bytes = valid SHA-256 hash length.
        let hex = "a".repeat(64);
        let query = format!("EXISTS sha256:{}", hex);
        let result = ex.execute(&query, &store).unwrap();
        match result.result {
            CalResultPayload::Exists { exists, .. } => assert!(!exists),
            other => panic!("unexpected: {:?}", other),
        }
    }

    // -----------------------------------------------------------------------
    // Test 10: DESCRIBE SCHEMA returns introspection info.
    // -----------------------------------------------------------------------

    #[test]
    fn test_execute_describe_schema() {
        let store = MockStore::empty();
        let ex = exec();
        let result = ex.execute("DESCRIBE SCHEMA", &store).unwrap();
        assert_eq!(result.metadata.statement_type, "describe");
        match result.result {
            CalResultPayload::Describe { info } => {
                assert!(info.get("grain_types").is_some());
                assert!(info.get("common_fields").is_some());
            }
            other => panic!("unexpected: {:?}", other),
        }
    }

    // -----------------------------------------------------------------------
    // Test 11: EXPLAIN RECALL builds a query plan without executing.
    // -----------------------------------------------------------------------

    #[test]
    fn test_execute_explain_recall() {
        let store = MockStore::empty();
        let ex = exec();
        let result = ex
            .execute(r#"EXPLAIN RECALL facts ABOUT "preferences""#, &store)
            .unwrap();
        assert_eq!(result.metadata.statement_type, "explain");
        match result.result {
            CalResultPayload::Explain { plan } => {
                assert_eq!(plan.statement_type, "recall");
                assert!(!plan.index_usage.is_empty());
            }
            other => panic!("unexpected: {:?}", other),
        }
    }

    #[test]
    fn test_explain_reports_like_as_the_bm25_leg_it_runs() {
        // `LIKE` and `ABOUT` both set `RecallParams::query` and execute the
        // same free-text leg, but the plan was derived from `ABOUT` alone, so
        // a `LIKE` recall was described as a structural `O(n) full scan` over
        // no index — the opposite of what it does. Both spellings must report
        // the same plan.
        let store = MockStore::empty();
        let ex = exec();
        let plan_of = |q: &str| match ex.execute(q, &store).unwrap().result {
            CalResultPayload::Explain { plan } => plan,
            other => panic!("unexpected: {:?}", other),
        };

        let like = plan_of(r#"EXPLAIN RECALL facts LIKE "window""#);
        assert_eq!(like.query_routing, "bm25");
        assert!(like.index_usage.contains(&"bm25_fts".to_string()));
        assert!(
            like.estimated_cost.starts_with("O(log n)"),
            "LIKE must not be reported as a full scan, got: {}",
            like.estimated_cost
        );

        let about = plan_of(r#"EXPLAIN RECALL facts ABOUT "window""#);
        assert_eq!(like.query_routing, about.query_routing);
        assert_eq!(like.index_usage, about.index_usage);

        // Anchored, the same free text is the hybrid path for either spelling.
        let anchored = plan_of(r#"EXPLAIN RECALL facts LIKE "window" WHERE subject = "john""#);
        assert_eq!(anchored.query_routing, "hybrid_rrf");
    }

    // -----------------------------------------------------------------------
    // Test 12: BATCH with two queries returns two results.
    // -----------------------------------------------------------------------

    #[test]
    fn test_execute_batch_two_queries() {
        let store = MockStore::empty();
        let ex = exec();
        let result = ex
            .execute("BATCH { RECALL facts ; RECALL events }", &store)
            .unwrap();
        assert_eq!(result.metadata.statement_type, "batch");
        match result.result {
            CalResultPayload::Batch { results } => {
                assert_eq!(results.len(), 2);
                assert!(results.contains_key("0"));
                assert!(results.contains_key("1"));
            }
            other => panic!("unexpected: {:?}", other),
        }
    }

    // -----------------------------------------------------------------------
    // Test 13: COALESCE returns first non-empty result.
    //
    // CAL syntax: `COALESCE(RECALL ..., RECALL ...)` (function call form).
    // The executor executes the first branch that returns a non-empty result.
    // -----------------------------------------------------------------------

    #[test]
    fn test_execute_coalesce() {
        let (hash, grain) = make_fact("john", "likes", "coffee");
        let store = MockStore::with_grains(vec![(hash, grain)]);
        let ex = exec();
        // COALESCE with one sub-query that will match john → 1 result returned.
        let result = ex
            .execute(
                r#"COALESCE(RECALL facts WHERE subject = "john", RECALL facts WHERE subject = "bob")"#,
                &store,
            )
            .unwrap();
        match result.result {
            CalResultPayload::Grains { grains, .. } => assert_eq!(grains.len(), 1),
            other => panic!("unexpected: {:?}", other),
        }
    }

    // -----------------------------------------------------------------------
    // Test 13b: COALESCE fallback — first branch empty, second matches.
    // -----------------------------------------------------------------------

    #[test]
    fn test_execute_coalesce_fallback_to_second_branch() {
        let (hash, grain) = make_fact("bob", "likes", "coffee");
        let store = MockStore::with_grains(vec![(hash, grain)]);
        let ex = exec();
        // First branch queries "john" (not in store), second queries "bob" (in store).
        let result = ex
            .execute(
                r#"COALESCE(RECALL facts WHERE subject = "john", RECALL facts WHERE subject = "bob")"#,
                &store,
            )
            .unwrap();
        match result.result {
            CalResultPayload::Grains { grains, .. } => {
                assert_eq!(grains.len(), 1, "should fall back to second branch");
                assert_eq!(
                    grains[0].fields.get("subject").and_then(|v| v.as_str()),
                    Some("bob"),
                    "result should be from the second branch (bob)"
                );
            }
            other => panic!("expected Grains, got: {:?}", other),
        }
    }

    // -----------------------------------------------------------------------
    // Test 13c: COALESCE with all branches empty returns empty Grains.
    // -----------------------------------------------------------------------

    #[test]
    fn test_execute_coalesce_all_branches_empty() {
        let store = MockStore::empty();
        let ex = exec();
        let result = ex
            .execute(
                r#"COALESCE(RECALL facts WHERE subject = "john", RECALL facts WHERE subject = "bob")"#,
                &store,
            )
            .unwrap();
        match result.result {
            CalResultPayload::Grains { grains, .. } => {
                assert!(grains.is_empty(), "all branches empty → empty result");
            }
            other => panic!("expected Grains, got: {:?}", other),
        }
    }

    // -----------------------------------------------------------------------
    // Test 13d: Large BATCH with 5 queries.
    // -----------------------------------------------------------------------

    #[test]
    fn test_execute_batch_five_queries() {
        let (hash, grain) = make_fact("john", "likes", "coffee");
        let store = MockStore::with_grains(vec![(hash, grain)]);
        let ex = exec();
        let result = ex
            .execute(
                r#"BATCH { RECALL facts ; RECALL events ; RECALL tools ; RECALL goals ; RECALL facts WHERE subject = "john" }"#,
                &store,
            )
            .unwrap();
        assert_eq!(result.metadata.statement_type, "batch");
        match result.result {
            CalResultPayload::Batch { results } => {
                assert_eq!(results.len(), 5, "BATCH should execute all 5 sub-queries");
                // Sub-query 0: RECALL facts (john is a fact) → 1 result
                // Sub-query 4: RECALL facts WHERE subject = "john" → 1 result
                assert!(results.contains_key("0"));
                assert!(results.contains_key("4"));
            }
            other => panic!("expected Batch, got: {:?}", other),
        }
    }

    // -----------------------------------------------------------------------
    // Test 14: Tier 1 ADD returns Unsupported when tier1_enabled = false.
    //
    // CAL syntax: `ADD fact SET subject = "..." SET relation = "..." ...`
    // REASON is optional; all field assignments use the SET keyword.
    // The executor must return Unsupported for all Tier 1 statements
    // when tier1_enabled is explicitly disabled.
    // -----------------------------------------------------------------------

    #[test]
    fn test_execute_tier1_add_returns_e044() {
        // Per spec §2.4, ADD with tier1 disabled must surface as
        // CAL-E044 Tier1NotEnabled (hard error), not a soft "Unsupported"
        // 200-payload that masks a capability denial.
        let store = MockStore::empty();
        let ex = CalExecutor::new(CalExecutorConfig {
            tier1_enabled: false,
            ..Default::default()
        });
        let err = ex
            .execute(
                r#"ADD fact SET subject = "john" SET relation = "likes" SET object = "rust" REASON "test""#,
                &store,
            )
            .expect_err("Tier 1 disabled must error, not Unsupported-payload");
        assert_eq!(err.code(), "CAL-E044");
        assert!(err.to_string().contains("ADD"));
    }

    // -----------------------------------------------------------------------
    // Test 14b: generic ADD ... SET with a type that cannot be shaped that
    // way returns Unsupported (see GrainTypeMeta::add_via_set).
    // -----------------------------------------------------------------------

    #[test]
    fn test_execute_generic_add_rejects_unshapeable_type() {
        let store = MockStore::empty();
        let ex = exec();
        let result = ex
            .execute(r#"ADD event SET content = "test" REASON "test""#, &store)
            .unwrap();
        match result.result {
            CalResultPayload::Unsupported { message, .. } => {
                assert!(
                    message.contains("cannot be created via ADD"),
                    "expected grain type restriction message, got: {}",
                    message
                );
                assert!(
                    message.contains("event"),
                    "message should mention the rejected type, got: {}",
                    message
                );
            }
            other => panic!("expected Unsupported, got: {:?}", other),
        }
    }

    // -----------------------------------------------------------------------
    // Test 14c: ADD with unresolved parameter returns Unsupported.
    // -----------------------------------------------------------------------

    #[test]
    fn test_execute_add_unresolved_parameter() {
        let store = MockStore::empty();
        let ex = exec();
        let result = ex
            .execute(
                r#"ADD fact SET subject = $user SET relation = "likes" SET object = "rust" REASON "test""#,
                &store,
            )
            .unwrap();
        match result.result {
            CalResultPayload::Unsupported { message, .. } => {
                assert!(
                    message.contains("Unresolved parameter"),
                    "expected unresolved parameter message, got: {}",
                    message
                );
                assert!(
                    message.contains("$user"),
                    "message should mention the parameter name, got: {}",
                    message
                );
            }
            other => panic!("expected Unsupported, got: {:?}", other),
        }
    }

    // -----------------------------------------------------------------------
    // Test 14d: CalResultPayload::Added has extraction fields.
    // -----------------------------------------------------------------------

    #[test]
    fn test_added_payload_has_extraction_fields() {
        // Compile-time check: verify the Added variant has the new fields.
        let payload = CalResultPayload::Added {
            hash: "abc123".into(),
            grain_type: "fact".into(),
            extracted_count: Some(3),
            extraction_warnings: vec!["warn1".into()],
        };
        match payload {
            CalResultPayload::Added {
                extracted_count,
                extraction_warnings,
                ..
            } => {
                assert_eq!(extracted_count, Some(3));
                assert_eq!(extraction_warnings.len(), 1);
            }
            _ => unreachable!(),
        }

        // Verify None/empty defaults work.
        let payload2 = CalResultPayload::Added {
            hash: "def456".into(),
            grain_type: "observation".into(),
            extracted_count: None,
            extraction_warnings: vec![],
        };
        match payload2 {
            CalResultPayload::Added {
                extracted_count,
                extraction_warnings,
                ..
            } => {
                assert_eq!(extracted_count, None);
                assert!(extraction_warnings.is_empty());
            }
            _ => unreachable!(),
        }
    }

    // -----------------------------------------------------------------------
    // Test 15: RECALL with WITH score_breakdown sets the flag.
    // -----------------------------------------------------------------------

    #[test]
    fn test_execute_recall_with_score_breakdown() {
        let store = MockStore::empty();
        let ex = exec();
        // Should not error; mock doesn't populate score_breakdown but the
        // RecallParams flag must be set correctly.
        let result = ex
            .execute("RECALL facts WITH score_breakdown", &store)
            .unwrap();
        assert_eq!(result.metadata.statement_type, "recall");
    }

    // -----------------------------------------------------------------------
    // Test 16: Query hash is computed (C-4).
    // -----------------------------------------------------------------------

    #[test]
    fn test_query_hash_is_computed_c4() {
        let store = MockStore::empty();
        let ex = exec();
        let input = "RECALL facts";
        let result = ex.execute(input, &store).unwrap();
        // Must be a 64-character lowercase hex string (SHA-256).
        assert_eq!(result.query_hash.len(), 64);
        assert!(result.query_hash.chars().all(|c| c.is_ascii_hexdigit()));
        // Must be reproducible.
        let result2 = ex.execute(input, &store).unwrap();
        assert_eq!(result.query_hash, result2.query_hash);
    }

    // -----------------------------------------------------------------------
    // Test 17: Namespace override is applied (ignores WHERE namespace).
    // -----------------------------------------------------------------------

    #[test]
    fn test_namespace_override_applied() {
        let store = MockStore::empty();
        let config = CalExecutorConfig {
            namespace_override: Some("tenant_a".to_string()),
            ..Default::default()
        };
        let ex = CalExecutor::new(config);
        // WHERE namespace = "other" should be ignored because of the override.
        // If it weren't ignored, the test would still pass since mock doesn't
        // filter by namespace — this test verifies no panic or parse error.
        let result = ex
            .execute(r#"RECALL facts WHERE namespace = "other""#, &store)
            .unwrap();
        assert_eq!(result.metadata.statement_type, "recall");
    }

    // -----------------------------------------------------------------------
    // Test 18: compute_query_hash is stable across calls.
    // -----------------------------------------------------------------------

    #[test]
    fn test_compute_query_hash_stable() {
        let h1 = compute_query_hash("RECALL facts");
        let h2 = compute_query_hash("RECALL facts");
        assert_eq!(h1, h2);
    }

    // -----------------------------------------------------------------------
    // Test 19: compute_query_hash differs for different inputs.
    // -----------------------------------------------------------------------

    #[test]
    fn test_compute_query_hash_distinct() {
        let h1 = compute_query_hash("RECALL facts");
        let h2 = compute_query_hash("RECALL events");
        assert_ne!(h1, h2);
    }

    // -----------------------------------------------------------------------
    // Test 20: Pipeline OFFSET skips results.
    // -----------------------------------------------------------------------

    #[test]
    fn test_execute_pipeline_offset() {
        let grains: Vec<_> = (0..5u8)
            .map(|i| make_fact(&format!("u{}", i), "likes", "rust"))
            .collect();
        let store = MockStore::with_grains(grains);
        let ex = exec();
        let result = ex.execute("RECALL facts OFFSET 3", &store).unwrap();
        match result.result {
            CalResultPayload::Grains { grains, .. } => {
                // 5 grains in store, limit 50 default (gets all 5), then offset 3 → 2 remain.
                assert_eq!(grains.len(), 2);
            }
            other => panic!("unexpected: {:?}", other),
        }
    }

    // -----------------------------------------------------------------------
    // Test 21: Pipeline FIRST returns exactly one grain.
    // -----------------------------------------------------------------------

    #[test]
    fn test_execute_pipeline_first() {
        let grains: Vec<_> = (0..5u8)
            .map(|i| make_fact(&format!("u{}", i), "likes", "rust"))
            .collect();
        let store = MockStore::with_grains(grains);
        let ex = exec();
        let result = ex.execute("RECALL facts FIRST", &store).unwrap();
        match result.result {
            CalResultPayload::Grains { grains, .. } => assert_eq!(grains.len(), 1),
            other => panic!("unexpected: {:?}", other),
        }
    }

    // -----------------------------------------------------------------------
    // Test 22: Pipeline HASHES extractor.
    // -----------------------------------------------------------------------

    #[test]
    fn test_execute_pipeline_hashes() {
        let (hash, grain) = make_fact("john", "is", "a developer");
        let store = MockStore::with_grains(vec![(hash, grain)]);
        let ex = exec();
        let result = ex.execute("RECALL facts HASHES", &store).unwrap();
        // I-7 fix: HASHES now returns Grains (not Describe).
        match result.result {
            CalResultPayload::Grains { grains, .. } => {
                assert_eq!(grains.len(), 1);
                assert_eq!(grains[0].grain_type, "extracted");
                let value = grains[0].fields.get("value").unwrap();
                assert_eq!(value.as_str().unwrap(), hash.to_hex());
            }
            other => panic!("unexpected: {:?}", other),
        }
    }

    // -----------------------------------------------------------------------
    // Test 23: DESCRIBE facts returns type-specific fields.
    // -----------------------------------------------------------------------

    #[test]
    fn test_execute_describe_facts() {
        let store = MockStore::empty();
        let ex = exec();
        let result = ex.execute("DESCRIBE facts", &store).unwrap();
        match result.result {
            CalResultPayload::Describe { info } => {
                assert_eq!(info["grain_type"], "facts");
                assert!(info.get("specific_fields").is_some());
            }
            other => panic!("unexpected: {:?}", other),
        }
    }

    // -----------------------------------------------------------------------
    // Test 24: S-2 — SUPERSEDE returns Unsupported in Phase 1.
    // -----------------------------------------------------------------------

    #[test]
    fn test_execute_tier1_supersede_returns_e044() {
        // S-2: SUPERSEDE with tier1 disabled now surfaces as
        // CAL-E044 Tier1NotEnabled (hard error) per spec §2.4.
        let store = MockStore::empty();
        let ex = CalExecutor::new(CalExecutorConfig {
            tier1_enabled: false,
            ..Default::default()
        });
        let err = ex
            .execute(
                r#"SUPERSEDE sha256:abc123def456 SET object = "new value" REASON "update""#,
                &store,
            )
            .expect_err("Tier 1 disabled must error, not Unsupported-payload");
        assert_eq!(err.code(), "CAL-E044");
        assert!(err.to_string().contains("SUPERSEDE"));
    }

    // -----------------------------------------------------------------------
    // Test 25: S-2 — REVERT returns Unsupported in Phase 1.
    // -----------------------------------------------------------------------

    #[test]
    fn test_execute_tier1_revert_returns_unsupported_s2() {
        let store = MockStore::empty();
        // REVERT always returns Unsupported regardless of tier1_enabled
        // (semantics not yet defined), so default config suffices.
        let ex = exec();
        let result = ex
            .execute(r#"REVERT sha256:abc123def456 REASON "mistake""#, &store)
            .unwrap();
        match result.result {
            CalResultPayload::Unsupported { message, .. } => {
                assert!(
                    message.contains("Tier 1"),
                    "REVERT must return Tier 1 unsupported message (S-2), got: {}",
                    message
                );
            }
            other => panic!("expected Unsupported for REVERT, got: {:?}", other),
        }
    }

    // -----------------------------------------------------------------------
    // Test 26: C-4 — query_hash is SHA-256 hex for various inputs.
    // -----------------------------------------------------------------------

    #[test]
    fn test_query_hash_format_c4() {
        let store = MockStore::empty();
        let ex = exec();
        let queries = [
            "RECALL facts",
            "RECALL events",
            "EXISTS sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "DESCRIBE SCHEMA",
            "BATCH { RECALL facts ; RECALL events }",
        ];
        for q in &queries {
            let result = ex.execute(q, &store).unwrap();
            assert_eq!(
                result.query_hash.len(),
                64,
                "query_hash must be 64 hex chars for '{}', got {}",
                q,
                result.query_hash.len()
            );
            assert!(
                result.query_hash.chars().all(|c| c.is_ascii_hexdigit()),
                "query_hash must be hex for '{}', got '{}'",
                q,
                result.query_hash
            );
        }
    }

    // -----------------------------------------------------------------------
    // Test 27: C-4 — NFC-equivalent inputs produce the same query hash.
    // -----------------------------------------------------------------------

    #[test]
    fn test_query_hash_nfc_equivalence_c4_s6() {
        let store = MockStore::empty();
        let ex = exec();
        // Decomposed: e + combining acute = precomposed e-acute
        let decomposed = "RECALL facts WHERE subject = \"caf\u{0065}\u{0301}\"";
        let precomposed = "RECALL facts WHERE subject = \"caf\u{00E9}\"";
        let r1 = ex.execute(decomposed, &store).unwrap();
        let r2 = ex.execute(precomposed, &store).unwrap();
        assert_eq!(
            r1.query_hash, r2.query_hash,
            "NFC-equivalent inputs must produce the same query_hash (C-4 + S-6)"
        );
    }

    // -----------------------------------------------------------------------
    // Test 28: OR condition propagates warning about partial support.
    // -----------------------------------------------------------------------

    #[test]
    fn test_or_condition_warning_propagation() {
        let store = MockStore::empty();
        let ex = exec();
        let result = ex
            .execute(
                r#"RECALL facts WHERE subject = "john" OR subject = "bob""#,
                &store,
            )
            .unwrap();
        // The parser should produce an OR warning since OR is partial in Phase 1.
        // Verify it does not panic and produces a valid result.
        assert_eq!(result.metadata.statement_type, "recall");
    }

    // -----------------------------------------------------------------------
    // Test 29: ASSEMBLE WHERE clause produces a warning (not supported in P1).
    // -----------------------------------------------------------------------

    #[test]
    fn test_assemble_where_clause_applied() {
        // WI-1.1: WHERE clause is now applied as post-composition filter.
        // Previously this was a stub that emitted a warning; now it filters.
        let store = MockStore::empty();
        let ex = exec();
        let result = ex
            .execute(
                r#"ASSEMBLE "summary" FROM (RECALL facts) WHERE confidence >= 0.8"#,
                &store,
            )
            .unwrap();
        // With an empty store, there are no grains to filter.
        match result.result {
            CalResultPayload::Grains { grains, .. } => {
                assert!(grains.is_empty(), "empty store should return no grains");
            }
            other => panic!("expected Grains, got: {:?}", other),
        }
    }

    // -----------------------------------------------------------------------
    // Test 30: Empty input through executor produces an error (not a panic).
    // -----------------------------------------------------------------------

    #[test]
    fn test_execute_empty_input_error() {
        let store = MockStore::empty();
        let ex = exec();
        let result = ex.execute("", &store);
        assert!(result.is_err(), "empty input must return an error");
    }

    // -----------------------------------------------------------------------
    // Test 31: Whitespace-only input produces an error (not a panic).
    // -----------------------------------------------------------------------

    #[test]
    fn test_execute_whitespace_only_error() {
        let store = MockStore::empty();
        let ex = exec();
        let result = ex.execute("   \t\n  ", &store);
        assert!(
            result.is_err(),
            "whitespace-only input must return an error"
        );
    }

    // -----------------------------------------------------------------------
    // Test 32: Input exceeding MAX_QUERY_LENGTH produces an error.
    // -----------------------------------------------------------------------

    #[test]
    fn test_execute_query_too_long_error() {
        let store = MockStore::empty();
        let ex = exec();
        // MAX_QUERY_LENGTH is 65536 bytes; create a query exceeding that.
        let huge = format!("RECALL facts WHERE subject = \"{}\"", "a".repeat(66_000));
        let result = ex.execute(&huge, &store);
        assert!(result.is_err(), "oversized input must return an error");
    }

    // -----------------------------------------------------------------------
    // Test 33: Malformed input does not panic (fuzz-like basic check).
    // -----------------------------------------------------------------------

    #[test]
    fn test_execute_malformed_inputs_no_panic() {
        let store = MockStore::empty();
        let ex = exec();
        let malformed = [
            "RECALL",
            "WHERE subject",
            "|||",
            "RECALL facts |",
            "RECALL facts WHERE",
            "RECALL facts WHERE subject =",
            "RECALL 123",
            "; ; ;",
            "RECALL facts beliefs beliefs",
            "RECALL facts WHERE subject = \"unterminated",
        ];
        for input in &malformed {
            // Must not panic; Ok or Err is fine.
            let _ = ex.execute(input, &store);
        }
    }

    // ===================================================================
    // Sprint 2c: HISTORY DIFF tests
    // ===================================================================

    #[test]
    fn test_history_diff_identical_grains() {
        let (hash, grain) = make_fact("john", "likes", "coffee");
        let store = MockStore::with_grains(vec![(hash, grain)]);
        let ex = exec();

        let history_stmt = super::HistoryStmt {
            hash: hash.to_hex(),
            where_clause: None,
            diff_target: Some(hash.to_hex()),
            span: None,
        };
        let query = super::CalQuery {
            version: super::super::ast::CalVersion(1),
            statement: super::CalStatement::History(history_stmt),
            pipeline: Vec::new(),
            with_options: Vec::new(),
            format: None,
            let_bindings: Vec::new(),
            let_values: Default::default(),
            user_vars: HashMap::new(),
            warnings: Vec::new(),
        };
        let mut warnings = Vec::new();
        let payload = ex
            .execute_statement(&query.statement, &store, &query, &mut warnings)
            .unwrap();

        match payload {
            CalResultPayload::Diff { changes, .. } => {
                assert!(
                    changes.is_empty(),
                    "identical grains should have no differences"
                );
            }
            other => panic!("expected Diff, got {:?}", other),
        }
        assert!(warnings.is_empty(), "no warnings for same subject/relation");
    }

    #[test]
    fn test_history_diff_different_grains() {
        let (hash_a, grain_a) = make_fact("john", "likes", "coffee");
        let (hash_b, grain_b) = make_fact("john", "likes", "tea");
        let store = MockStore::with_grains(vec![(hash_a, grain_a), (hash_b, grain_b)]);
        let ex = exec();

        let history_stmt = super::HistoryStmt {
            hash: hash_a.to_hex(),
            where_clause: None,
            diff_target: Some(hash_b.to_hex()),
            span: None,
        };
        let query = super::CalQuery {
            version: super::super::ast::CalVersion(1),
            statement: super::CalStatement::History(history_stmt),
            pipeline: Vec::new(),
            with_options: Vec::new(),
            format: None,
            let_bindings: Vec::new(),
            let_values: Default::default(),
            user_vars: HashMap::new(),
            warnings: Vec::new(),
        };
        let mut warnings = Vec::new();
        let payload = ex
            .execute_statement(&query.statement, &store, &query, &mut warnings)
            .unwrap();

        match payload {
            CalResultPayload::Diff {
                source_hash,
                target_hash,
                changes,
            } => {
                assert_eq!(source_hash, hash_a.to_hex());
                assert_eq!(target_hash, hash_b.to_hex());
                let obj_change = changes.iter().find(|c| {
                    matches!(c,
                        super::super::ast::FieldDiff::Changed { field, .. } if field == "object"
                    )
                });
                assert!(
                    obj_change.is_some(),
                    "expected 'object' field to be Changed"
                );
            }
            other => panic!("expected Diff, got {:?}", other),
        }
        assert!(
            !warnings.iter().any(|w| w.contains("CAL-W005")),
            "same subject+relation should not trigger CAL-W005"
        );
    }

    #[test]
    fn test_history_diff_w005_different_subject() {
        let (hash_a, grain_a) = make_fact("john", "likes", "coffee");
        let (hash_b, grain_b) = make_fact("bob", "likes", "coffee");
        let store = MockStore::with_grains(vec![(hash_a, grain_a), (hash_b, grain_b)]);
        let ex = exec();

        let history_stmt = super::HistoryStmt {
            hash: hash_a.to_hex(),
            where_clause: None,
            diff_target: Some(hash_b.to_hex()),
            span: None,
        };
        let query = super::CalQuery {
            version: super::super::ast::CalVersion(1),
            statement: super::CalStatement::History(history_stmt),
            pipeline: Vec::new(),
            with_options: Vec::new(),
            format: None,
            let_bindings: Vec::new(),
            let_values: Default::default(),
            user_vars: HashMap::new(),
            warnings: Vec::new(),
        };
        let mut warnings = Vec::new();
        let _ = ex
            .execute_statement(&query.statement, &store, &query, &mut warnings)
            .unwrap();
        assert!(
            warnings.iter().any(|w| w.contains("CAL-W005")),
            "different subject/relation should trigger CAL-W005, got: {:?}",
            warnings
        );
    }

    #[test]
    fn test_history_diff_source_not_found() {
        let (hash_b, grain_b) = make_fact("john", "likes", "coffee");
        let store = MockStore::with_grains(vec![(hash_b, grain_b)]);
        let ex = exec();

        let fake_hash = "a".repeat(64);
        let history_stmt = super::HistoryStmt {
            hash: fake_hash,
            where_clause: None,
            diff_target: Some(hash_b.to_hex()),
            span: None,
        };
        let query = super::CalQuery {
            version: super::super::ast::CalVersion(1),
            statement: super::CalStatement::History(history_stmt),
            pipeline: Vec::new(),
            with_options: Vec::new(),
            format: None,
            let_bindings: Vec::new(),
            let_values: Default::default(),
            user_vars: HashMap::new(),
            warnings: Vec::new(),
        };
        let mut warnings = Vec::new();
        let result = ex.execute_statement(&query.statement, &store, &query, &mut warnings);
        assert!(result.is_err(), "should error when source grain not found");
    }

    #[test]
    fn test_history_diff_target_not_found() {
        let (hash_a, grain_a) = make_fact("john", "likes", "coffee");
        let store = MockStore::with_grains(vec![(hash_a, grain_a)]);
        let ex = exec();

        let fake_hash = "b".repeat(64);
        let history_stmt = super::HistoryStmt {
            hash: hash_a.to_hex(),
            where_clause: None,
            diff_target: Some(fake_hash),
            span: None,
        };
        let query = super::CalQuery {
            version: super::super::ast::CalVersion(1),
            statement: super::CalStatement::History(history_stmt),
            pipeline: Vec::new(),
            with_options: Vec::new(),
            format: None,
            let_bindings: Vec::new(),
            let_values: Default::default(),
            user_vars: HashMap::new(),
            warnings: Vec::new(),
        };
        let mut warnings = Vec::new();
        let result = ex.execute_statement(&query.statement, &store, &query, &mut warnings);
        assert!(result.is_err(), "should error when target grain not found");
    }

    // ===================================================================
    // Sprint 2c: diff_grains unit tests
    // ===================================================================

    #[test]
    fn test_diff_grains_field_added() {
        let (_, grain_a) = make_fact("john", "likes", "coffee");
        let (_, mut grain_b) = make_fact("john", "likes", "coffee");
        grain_b
            .fields
            .insert("extra".into(), serde_json::json!("new_value"));

        let diffs = super::diff_grains(&grain_a, &grain_b);
        let added = diffs.iter().find(|d| {
            matches!(d,
                super::super::ast::FieldDiff::Added { field, .. } if field == "extra"
            )
        });
        assert!(added.is_some(), "should detect added field 'extra'");
    }

    #[test]
    fn test_diff_grains_field_removed() {
        let (_, mut grain_a) = make_fact("john", "likes", "coffee");
        let (_, grain_b) = make_fact("john", "likes", "coffee");
        grain_a
            .fields
            .insert("old_field".into(), serde_json::json!("old_value"));

        let diffs = super::diff_grains(&grain_a, &grain_b);
        let removed = diffs.iter().find(|d| {
            matches!(d,
                super::super::ast::FieldDiff::Removed { field, .. } if field == "old_field"
            )
        });
        assert!(removed.is_some(), "should detect removed field 'old_field'");
    }

    #[test]
    fn test_diff_grains_field_changed() {
        let (_, grain_a) = make_fact("john", "likes", "coffee");
        let (_, grain_b) = make_fact("john", "likes", "tea");

        let diffs = super::diff_grains(&grain_a, &grain_b);
        let changed = diffs.iter().find(|d| {
            matches!(d,
                super::super::ast::FieldDiff::Changed { field, .. } if field == "object"
            )
        });
        assert!(changed.is_some(), "should detect changed field 'object'");

        if let Some(super::super::ast::FieldDiff::Changed { old, new, .. }) = changed {
            assert_eq!(old.as_str().unwrap(), "coffee");
            assert_eq!(new.as_str().unwrap(), "tea");
        }
    }

    #[test]
    fn test_diff_grains_identical() {
        let (_, grain_a) = make_fact("john", "likes", "vim");
        let (_, grain_b) = make_fact("john", "likes", "vim");
        let diffs = super::diff_grains(&grain_a, &grain_b);
        assert!(
            diffs.is_empty(),
            "identical grains should produce empty diff"
        );
    }

    // ===================================================================
    // Sprint 2c: Enhanced DESCRIBE tests
    // ===================================================================

    #[test]
    fn test_execute_describe_capabilities() {
        let store = MockStore::empty();
        let ex = exec();
        let result = ex.execute("DESCRIBE CAPABILITIES", &store).unwrap();
        assert_eq!(result.metadata.statement_type, "describe");
        match result.result {
            CalResultPayload::Describe { info } => {
                assert_eq!(info["cal_version"], 1);
                assert_eq!(info["conformance_level"], 2);
                assert!(!info["supported_statements"].as_array().unwrap().is_empty());
                assert_eq!(info["max_sources"], 8);
                assert_eq!(info["max_let_bindings"], 5);
            }
            other => panic!("expected Describe, got {:?}", other),
        }
    }

    #[test]
    fn test_execute_describe_server() {
        let store = MockStore::empty();
        let ex = exec();
        let result = ex.execute("DESCRIBE SERVER", &store).unwrap();
        match result.result {
            CalResultPayload::Describe { info } => {
                assert_eq!(info["name"], "areev");
                assert!(info.get("version").is_some());
                assert_eq!(info["oms_version"], "1.2");
                assert!(info.get("build_features").is_some());
            }
            other => panic!("expected Describe, got {:?}", other),
        }
    }

    #[test]
    fn test_execute_describe_fields() {
        let store = MockStore::empty();
        let ex = exec();
        let result = ex.execute("DESCRIBE FIELDS", &store).unwrap();
        match result.result {
            CalResultPayload::Describe { info } => {
                assert!(info.get("fields").is_some());
                let fields = info["fields"].as_array().unwrap();
                assert!(!fields.is_empty());
                let has_subject = fields.iter().any(|f| f["name"] == "subject");
                assert!(has_subject, "fields should contain 'subject'");
            }
            other => panic!("expected Describe, got {:?}", other),
        }
    }

    #[test]
    fn test_execute_describe_templates() {
        let store = MockStore::empty();
        let ex = exec();
        let result = ex.execute("DESCRIBE TEMPLATES", &store).unwrap();
        match result.result {
            CalResultPayload::Describe { info } => {
                assert!(info.get("templates").is_some());
                let templates = info["templates"].as_array().unwrap();
                assert!(!templates.is_empty(), "templates list should not be empty");
                // Built-ins are exactly the §10.1 presets; in particular no
                // builtin may shadow a FORMAT arm name (toon, triples, …).
                for name in ["structured", "readable", "compact"] {
                    assert!(
                        templates.iter().any(|t| t["name"] == name),
                        "templates should include '{name}'"
                    );
                }
                assert!(
                    !templates.iter().any(|t| t["name"] == "toon"),
                    "no builtin template may shadow the FORMAT toon arm"
                );
            }
            other => panic!("expected Describe, got {:?}", other),
        }
    }

    #[test]
    fn test_execute_describe_grammar() {
        let store = MockStore::empty();
        let ex = exec();
        let result = ex.execute("DESCRIBE GRAMMAR", &store).unwrap();
        match result.result {
            CalResultPayload::Describe { info } => {
                assert_eq!(info["version"], 1);
                assert!(info.get("features").is_some());
                let features = info["features"].as_array().unwrap();
                assert!(!features.is_empty());
                assert_eq!(info["conformance_level"], 2);
            }
            other => panic!("expected Describe, got {:?}", other),
        }
    }

    // ===================================================================
    // Sprint 2c: Enhanced EXPLAIN tests
    // ===================================================================

    #[test]
    fn test_execute_explain_assemble() {
        let store = MockStore::empty();
        let ex = exec();
        let result = ex
            .execute(r#"EXPLAIN ASSEMBLE "summary" FROM (RECALL facts)"#, &store)
            .unwrap();
        match result.result {
            CalResultPayload::Explain { plan } => {
                assert_eq!(plan.statement_type, "assemble");
                assert!(plan.query_routing.contains("assemble"));
                assert!(plan.filters.iter().any(|f| f.contains("sources")));
            }
            other => panic!("expected Explain, got {:?}", other),
        }
    }

    #[test]
    fn test_execute_explain_batch() {
        let store = MockStore::empty();
        let ex = exec();
        let result = ex
            .execute("EXPLAIN BATCH { RECALL facts ; RECALL events }", &store)
            .unwrap();
        match result.result {
            CalResultPayload::Explain { plan } => {
                assert_eq!(plan.statement_type, "batch");
                assert_eq!(plan.query_routing, "parallel_batch");
                assert!(plan
                    .filters
                    .iter()
                    .any(|f| f.contains("parallel_execution")));
            }
            other => panic!("expected Explain, got {:?}", other),
        }
    }

    #[test]
    fn test_execute_explain_coalesce() {
        let store = MockStore::empty();
        let ex = exec();
        let result = ex.execute(
            r#"EXPLAIN COALESCE(RECALL facts WHERE subject = "john", RECALL facts WHERE subject = "bob")"#,
            &store,
        ).unwrap();
        match result.result {
            CalResultPayload::Explain { plan } => {
                assert_eq!(plan.statement_type, "coalesce");
                assert_eq!(plan.query_routing, "coalesce_fallback");
                assert!(plan.filters.iter().any(|f| f.contains("fallback_chain")));
            }
            other => panic!("expected Explain, got {:?}", other),
        }
    }

    #[test]
    fn test_execute_explain_describe_rejected() {
        // EXPLAIN can only wrap statements that produce an execution plan;
        // DESCRIBE is introspection and has no plan.
        let store = MockStore::empty();
        let ex = exec();
        let err = ex
            .execute("EXPLAIN DESCRIBE SCHEMA", &store)
            .expect_err("EXPLAIN DESCRIBE should be a parse error per §8.5");
        assert_eq!(err.code(), "CAL-E002");
        assert!(
            err.to_string().contains("DESCRIBE"),
            "error should mention DESCRIBE: {}",
            err
        );
    }

    #[test]
    fn test_execute_explain_history() {
        let (hash, grain) = make_fact("john", "likes", "coffee");
        let store = MockStore::with_grains(vec![(hash, grain)]);
        let ex = exec();
        let hex = hash.to_hex();
        let query_str = format!("EXPLAIN HISTORY OF sha256:{}", hex);
        let result = ex.execute(&query_str, &store).unwrap();
        match result.result {
            CalResultPayload::Explain { plan } => {
                assert_eq!(plan.statement_type, "history");
                assert_eq!(plan.query_routing, "entity_latest");
                assert!(plan.index_usage.contains(&"entity_latest".to_string()));
            }
            other => panic!("expected Explain, got {:?}", other),
        }
    }

    // ===================================================================
    // Sprint 2c: Serialization and helper tests
    // ===================================================================

    #[test]
    fn test_field_diff_serialization() {
        let diff = super::super::ast::FieldDiff::Changed {
            field: "object".into(),
            old: serde_json::json!("coffee"),
            new: serde_json::json!("tea"),
        };
        let json = serde_json::to_value(&diff).unwrap();
        assert_eq!(json["kind"], "changed");
        assert_eq!(json["field"], "object");
        assert_eq!(json["old"], "coffee");
        assert_eq!(json["new"], "tea");
    }

    #[test]
    fn test_diff_payload_serialization() {
        let payload = CalResultPayload::Diff {
            source_hash: "aaa".into(),
            target_hash: "bbb".into(),
            changes: vec![
                super::super::ast::FieldDiff::Added {
                    field: "new_field".into(),
                    value: serde_json::json!("value"),
                },
                super::super::ast::FieldDiff::Removed {
                    field: "old_field".into(),
                    value: serde_json::json!(42),
                },
            ],
        };
        let json = serde_json::to_value(&payload).unwrap();
        assert_eq!(json["type"], "diff");
        assert_eq!(json["source_hash"], "aaa");
        assert_eq!(json["target_hash"], "bbb");
        assert_eq!(json["changes"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn test_build_features_list() {
        let features = super::build_features_list();
        assert!(features.contains(&"cal"), "cal feature should be active");
    }

    #[test]
    fn test_count_payload_results_diff() {
        let payload = CalResultPayload::Diff {
            source_hash: "a".into(),
            target_hash: "b".into(),
            changes: vec![super::super::ast::FieldDiff::Added {
                field: "x".into(),
                value: serde_json::json!(1),
            }],
        };
        assert_eq!(super::count_payload_results(&payload), 1);
    }

    // ===================================================================
    // Sprint 2b: LET binding, IS CATEGORY, I-5, I-7, redact_budget_metadata
    // ===================================================================

    // -- I-5: value_to_string for Parameter returns UnboundParameter ----

    #[test]
    fn test_i5_value_to_string_parameter_returns_error() {
        let val = Value::Parameter {
            name: "test".into(),
        };
        let result = super::value_to_string(&val);
        assert!(
            result.is_err(),
            "Parameter must return error, not \"$test\""
        );
        match result.unwrap_err() {
            CalError::UnboundParameter { name, .. } => {
                assert_eq!(name, "test");
            }
            other => panic!("expected UnboundParameter, got: {:?}", other),
        }
    }

    // -- I-7: Extractors return Grains, not Describe --------------------

    #[test]
    fn test_i7_subjects_extractor_returns_grains() {
        let (hash, grain) = make_fact("john", "likes", "coffee");
        let store = MockStore::with_grains(vec![(hash, grain)]);
        let ex = exec();
        let result = ex.execute("RECALL facts SUBJECTS", &store).unwrap();
        match result.result {
            CalResultPayload::Grains { grains, .. } => {
                assert_eq!(grains.len(), 1);
                assert_eq!(grains[0].grain_type, "extracted");
                let value = grains[0].fields.get("value").unwrap();
                assert_eq!(value.as_str().unwrap(), "john");
            }
            other => panic!("SUBJECTS should return Grains (I-7 fix), got: {:?}", other),
        }
    }

    #[test]
    fn test_i7_objects_extractor_returns_grains() {
        let (hash, grain) = make_fact("john", "likes", "coffee");
        let store = MockStore::with_grains(vec![(hash, grain)]);
        let ex = exec();
        let result = ex.execute("RECALL facts OBJECTS", &store).unwrap();
        match result.result {
            CalResultPayload::Grains { grains, .. } => {
                assert_eq!(grains.len(), 1);
                assert_eq!(grains[0].grain_type, "extracted");
                let value = grains[0].fields.get("value").unwrap();
                assert_eq!(value.as_str().unwrap(), "coffee");
            }
            other => panic!("OBJECTS should return Grains (I-7 fix), got: {:?}", other),
        }
    }

    #[test]
    fn test_i7_hashes_extractor_returns_grains() {
        let (hash, grain) = make_fact("john", "likes", "coffee");
        let store = MockStore::with_grains(vec![(hash, grain)]);
        let ex = exec();
        let result = ex.execute("RECALL facts HASHES", &store).unwrap();
        match result.result {
            CalResultPayload::Grains { grains, .. } => {
                assert_eq!(grains.len(), 1);
                assert_eq!(grains[0].grain_type, "extracted");
                let value = grains[0].fields.get("value").unwrap();
                assert_eq!(value.as_str().unwrap(), hash.to_hex());
            }
            other => panic!("HASHES should return Grains (I-7 fix), got: {:?}", other),
        }
    }

    // -- LET binding tests -----------------------------------------------

    #[test]
    fn test_let_binding_subjects_extractor() {
        let grains = vec![
            make_fact("john", "likes", "coffee"),
            make_fact("bob", "likes", "tea"),
        ];
        let store = MockStore::with_grains(grains);
        let ex = exec();
        // LET $users = RECALL facts SUBJECTS ; RECALL facts
        let result = ex
            .execute(
                r#"LET $users = RECALL facts SUBJECTS; RECALL facts WHERE subject = "john""#,
                &store,
            )
            .unwrap();
        // The main query should still work (LET scope is evaluated but
        // not yet used for parameter resolution in WHERE clauses).
        assert_eq!(result.metadata.statement_type, "recall");
    }

    #[test]
    fn test_let_scope_evaluate_basic() {
        let grains = vec![
            make_fact("john", "likes", "coffee"),
            make_fact("bob", "likes", "coffee"),
        ];
        let store = MockStore::with_grains(grains);
        let ex = exec();

        let binding = super::super::ast::LetBinding {
            name: "users".into(),
            extractor: super::super::ast::Extractor::Subjects,
            source: Box::new(CalStatement::Recall(RecallStmt {
                grain_type: GrainTypePlural::Facts,
                about: None,
                where_clause: None,
                recent: None,
                since: None,
                until: None,
                like: None,
                between: None,
                contradictions: None,
                limit: None,
                as_format: None,
                span: None,
            })),
            span: None,
        };

        let query = CalQuery {
            version: super::super::ast::CalVersion(1),
            statement: CalStatement::Recall(RecallStmt {
                grain_type: GrainTypePlural::Facts,
                about: None,
                where_clause: None,
                recent: None,
                since: None,
                until: None,
                like: None,
                between: None,
                contradictions: None,
                limit: None,
                as_format: None,
                span: None,
            }),
            pipeline: Vec::new(),
            with_options: Vec::new(),
            format: None,
            let_bindings: vec![binding.clone()],
            let_values: Default::default(),
            user_vars: HashMap::new(),
            warnings: Vec::new(),
        };

        let mut warnings = Vec::new();
        let scope =
            super::LetScope::evaluate(&[binding], &ex, &store, &query, &mut warnings).unwrap();

        match scope.resolve("users").unwrap() {
            super::LetValue::Extracted(values) => {
                assert!(values.contains(&"john".to_string()));
                assert!(values.contains(&"bob".to_string()));
            }
            other => panic!("expected Extracted, got: {:?}", other),
        }

        // Unbound name should error.
        assert!(scope.resolve("missing").is_err());
    }

    #[test]
    fn test_let_scope_too_many_bindings_s06() {
        let store = MockStore::empty();
        let ex = exec();

        // Create 6 bindings (max is 5).
        let bindings: Vec<super::super::ast::LetBinding> = (0..6)
            .map(|i| super::super::ast::LetBinding {
                name: format!("var{}", i),
                extractor: super::super::ast::Extractor::Subjects,
                source: Box::new(CalStatement::Recall(RecallStmt {
                    grain_type: GrainTypePlural::Facts,
                    about: None,
                    where_clause: None,
                    recent: None,
                    since: None,
                    until: None,
                    like: None,
                    between: None,
                    contradictions: None,
                    limit: None,
                    as_format: None,
                    span: None,
                })),
                span: None,
            })
            .collect();

        let query = CalQuery {
            version: super::super::ast::CalVersion(1),
            statement: CalStatement::Recall(RecallStmt {
                grain_type: GrainTypePlural::Facts,
                about: None,
                where_clause: None,
                recent: None,
                since: None,
                until: None,
                like: None,
                between: None,
                contradictions: None,
                limit: None,
                as_format: None,
                span: None,
            }),
            pipeline: Vec::new(),
            with_options: Vec::new(),
            format: None,
            let_bindings: bindings.clone(),
            let_values: Default::default(),
            user_vars: HashMap::new(),
            warnings: Vec::new(),
        };

        let mut warnings = Vec::new();
        let result = super::LetScope::evaluate(&bindings, &ex, &store, &query, &mut warnings);
        assert!(result.is_err(), "6 bindings must exceed S-06 limit of 5");
        match result.unwrap_err() {
            CalError::TooManyLetBindings { count, max, .. } => {
                assert_eq!(count, 6);
                assert_eq!(max, 5);
            }
            other => panic!("expected TooManyLetBindings, got: {:?}", other),
        }
    }

    // -- redact_budget_metadata config test --------------------------------

    #[test]
    fn test_redact_budget_metadata_config_default() {
        let config = CalExecutorConfig::default();
        assert!(
            !config.redact_budget_metadata,
            "default should be false (S-09)"
        );
    }

    #[test]
    fn test_redact_budget_metadata_config_override() {
        let config = CalExecutorConfig {
            redact_budget_metadata: true,
            ..Default::default()
        };
        assert!(config.redact_budget_metadata);
    }

    // -- Assembled payload count test -------------------------------------

    #[test]
    fn test_count_payload_results_assembled() {
        let payload = CalResultPayload::Assembled {
            grains: vec![CalGrainResult {
                hash: "abc".into(),
                grain_type: "fact".into(),
                score: 1.0,
                fields: serde_json::json!({}),
                score_breakdown: None,
                explanation: None,
                relative_time: None,
                is_deterministic: false,
                contested_by: None,
            }],
            sources: vec![],
            total_tokens: 100,
            budget_limit: Some(500),
            progressive: false,
            total_available: Some(1),
        };
        assert_eq!(super::count_payload_results(&payload), 1);
    }

    // ===================================================================
    // Phase 2 WI tests — WI-1.1 through WI-1.6
    // ===================================================================

    // -- WI-1.2: Labeled BATCH -------------------------------------------

    #[test]
    fn test_labeled_batch_returns_keyed_results() {
        let store = MockStore::with_grains(vec![make_fact("john", "likes", "coffee")]);
        let ex = exec();

        // Build a labeled BATCH with two labels, each recalling facts.
        let batch = super::super::ast::BatchStmt {
            statements: Vec::new(),
            labeled: Some(vec![
                (
                    "prefs".to_string(),
                    super::super::ast::BatchEntry {
                        statement: CalStatement::Recall(RecallStmt {
                            grain_type: GrainTypePlural::Facts,
                            about: None,
                            where_clause: Some(super::super::ast::WhereClause {
                                condition: super::super::ast::Condition::Comparison {
                                    field: "subject".into(),
                                    comparator: super::super::ast::Comparator::Eq,
                                    value: super::super::ast::Value::String {
                                        value: "john".into(),
                                    },
                                    span: None,
                                },
                                span: None,
                            }),
                            recent: None,
                            since: None,
                            until: None,
                            like: None,
                            between: None,
                            contradictions: None,
                            limit: None,
                            as_format: None,
                            span: None,
                        }),
                        pipeline: Vec::new(),
                        with_options: Vec::new(),
                        format: None,
                        user_vars: HashMap::new(),
                    },
                ),
                (
                    "all".to_string(),
                    super::super::ast::BatchEntry {
                        statement: CalStatement::Recall(RecallStmt {
                            grain_type: GrainTypePlural::Facts,
                            about: None,
                            where_clause: None,
                            recent: None,
                            since: None,
                            until: None,
                            like: None,
                            between: None,
                            contradictions: None,
                            limit: None,
                            as_format: None,
                            span: None,
                        }),
                        pipeline: Vec::new(),
                        with_options: Vec::new(),
                        format: None,
                        user_vars: HashMap::new(),
                    },
                ),
            ]),
            span: None,
        };

        let mut warnings = Vec::new();
        let result = ex.execute_batch(&batch, &store, &mut warnings).unwrap();

        match result {
            CalResultPayload::Batch { results } => {
                assert!(results.contains_key("prefs"), "should have 'prefs' label");
                assert!(results.contains_key("all"), "should have 'all' label");
                assert_eq!(results.len(), 2, "exactly 2 labeled results");
            }
            other => panic!("expected Batch, got: {:?}", other),
        }
    }

    #[test]
    fn test_labeled_batch_duplicate_label_errors() {
        let store = MockStore::empty();
        let ex = exec();

        let batch = super::super::ast::BatchStmt {
            statements: Vec::new(),
            labeled: Some(vec![
                (
                    "dup".to_string(),
                    super::super::ast::BatchEntry {
                        statement: CalStatement::Recall(RecallStmt {
                            grain_type: GrainTypePlural::Facts,
                            about: None,
                            where_clause: None,
                            recent: None,
                            since: None,
                            until: None,
                            like: None,
                            between: None,
                            contradictions: None,
                            limit: None,
                            as_format: None,
                            span: None,
                        }),
                        pipeline: Vec::new(),
                        with_options: Vec::new(),
                        format: None,
                        user_vars: HashMap::new(),
                    },
                ),
                (
                    "dup".to_string(),
                    super::super::ast::BatchEntry {
                        statement: CalStatement::Recall(RecallStmt {
                            grain_type: GrainTypePlural::Events,
                            about: None,
                            where_clause: None,
                            recent: None,
                            since: None,
                            until: None,
                            like: None,
                            between: None,
                            contradictions: None,
                            limit: None,
                            as_format: None,
                            span: None,
                        }),
                        pipeline: Vec::new(),
                        with_options: Vec::new(),
                        format: None,
                        user_vars: HashMap::new(),
                    },
                ),
            ]),
            span: None,
        };

        let mut warnings = Vec::new();
        let result = ex.execute_batch(&batch, &store, &mut warnings);
        assert!(result.is_err());
        match result.unwrap_err() {
            CalError::AssembleDuplicateLabel { label, .. } => {
                assert_eq!(label, "dup");
            }
            other => panic!("expected AssembleDuplicateLabel, got: {:?}", other),
        }
    }

    #[test]
    fn test_positional_batch_still_works() {
        // Ensure the Phase 1 positional path is not broken.
        let store = MockStore::with_grains(vec![make_fact("john", "likes", "coffee")]);
        let ex = exec();

        let batch = super::super::ast::BatchStmt {
            statements: vec![super::super::ast::BatchEntry {
                statement: CalStatement::Recall(RecallStmt {
                    grain_type: GrainTypePlural::Facts,
                    about: None,
                    where_clause: None,
                    recent: None,
                    since: None,
                    until: None,
                    like: None,
                    between: None,
                    contradictions: None,
                    limit: None,
                    as_format: None,
                    span: None,
                }),
                pipeline: Vec::new(),
                with_options: Vec::new(),
                format: None,
                user_vars: HashMap::new(),
            }],
            labeled: None,
            span: None,
        };

        let mut warnings = Vec::new();
        let result = ex.execute_batch(&batch, &store, &mut warnings).unwrap();
        match result {
            CalResultPayload::Batch { results } => {
                assert!(
                    results.contains_key("0"),
                    "positional batch uses index keys"
                );
            }
            other => panic!("expected Batch, got: {:?}", other),
        }
    }

    // -- WI-1.3: Multi-branch COALESCE -----------------------------------

    #[test]
    fn test_coalesce_multibranch_first_hit_wins() {
        let store = MockStore::with_grains(vec![make_fact("john", "likes", "coffee")]);
        let ex = exec();

        let coalesce = super::super::ast::CoalesceStmt {
            grain_type: GrainTypePlural::Facts,
            where_clause: None,
            branches: vec![
                // Branch 1: matches john.
                super::super::ast::CoalesceBranch {
                    query: CalStatement::Recall(RecallStmt {
                        grain_type: GrainTypePlural::Facts,
                        about: None,
                        where_clause: Some(super::super::ast::WhereClause {
                            condition: super::super::ast::Condition::Comparison {
                                field: "subject".into(),
                                comparator: super::super::ast::Comparator::Eq,
                                value: super::super::ast::Value::String {
                                    value: "john".into(),
                                },
                                span: None,
                            },
                            span: None,
                        }),
                        recent: None,
                        since: None,
                        until: None,
                        like: None,
                        between: None,
                        contradictions: None,
                        limit: None,
                        as_format: None,
                        span: None,
                    }),
                    span: None,
                },
                // Branch 2: should NOT be reached.
                super::super::ast::CoalesceBranch {
                    query: CalStatement::Recall(RecallStmt {
                        grain_type: GrainTypePlural::Events,
                        about: None,
                        where_clause: None,
                        recent: None,
                        since: None,
                        until: None,
                        like: None,
                        between: None,
                        contradictions: None,
                        limit: None,
                        as_format: None,
                        span: None,
                    }),
                    span: None,
                },
            ],
            else_branch: None,
            span: None,
        };

        let query = CalQuery {
            version: super::super::ast::CalVersion(1),
            statement: CalStatement::Coalesce(coalesce.clone()),
            pipeline: Vec::new(),
            with_options: Vec::new(),
            format: None,
            let_bindings: Vec::new(),
            let_values: Default::default(),
            user_vars: HashMap::new(),
            warnings: Vec::new(),
        };

        let mut warnings = Vec::new();
        let result = ex
            .execute_coalesce(&coalesce, &store, &query, &mut warnings)
            .unwrap();

        match result {
            CalResultPayload::Grains { grains, .. } => {
                assert_eq!(grains.len(), 1, "branch 1 should return john");
            }
            other => panic!("expected Grains, got: {:?}", other),
        }

        // Check the short-circuit warning was emitted.
        assert!(
            warnings.iter().any(|w| w.contains("short-circuited")),
            "should emit short-circuit warning"
        );
    }

    #[test]
    fn test_coalesce_multibranch_falls_through_to_else() {
        let store = MockStore::with_grains(vec![make_fact("john", "likes", "coffee")]);
        let ex = exec();

        let coalesce = super::super::ast::CoalesceStmt {
            grain_type: GrainTypePlural::Facts,
            where_clause: None,
            branches: vec![
                // Branch 1: no match (nobody is named "zzz").
                super::super::ast::CoalesceBranch {
                    query: CalStatement::Recall(RecallStmt {
                        grain_type: GrainTypePlural::Facts,
                        about: None,
                        where_clause: Some(super::super::ast::WhereClause {
                            condition: super::super::ast::Condition::Comparison {
                                field: "subject".into(),
                                comparator: super::super::ast::Comparator::Eq,
                                value: super::super::ast::Value::String {
                                    value: "zzz".into(),
                                },
                                span: None,
                            },
                            span: None,
                        }),
                        recent: None,
                        since: None,
                        until: None,
                        like: None,
                        between: None,
                        contradictions: None,
                        limit: None,
                        as_format: None,
                        span: None,
                    }),
                    span: None,
                },
            ],
            // ELSE: recall all facts.
            else_branch: Some(Box::new(CalStatement::Recall(RecallStmt {
                grain_type: GrainTypePlural::Facts,
                about: None,
                where_clause: None,
                recent: None,
                since: None,
                until: None,
                like: None,
                between: None,
                contradictions: None,
                limit: None,
                as_format: None,
                span: None,
            }))),
            span: None,
        };

        let query = CalQuery {
            version: super::super::ast::CalVersion(1),
            statement: CalStatement::Coalesce(coalesce.clone()),
            pipeline: Vec::new(),
            with_options: Vec::new(),
            format: None,
            let_bindings: Vec::new(),
            let_values: Default::default(),
            user_vars: HashMap::new(),
            warnings: Vec::new(),
        };

        let mut warnings = Vec::new();
        let result = ex
            .execute_coalesce(&coalesce, &store, &query, &mut warnings)
            .unwrap();

        match result {
            CalResultPayload::Grains { grains, .. } => {
                assert_eq!(grains.len(), 1, "ELSE branch should return john");
            }
            other => panic!("expected Grains, got: {:?}", other),
        }
    }

    #[test]
    fn test_coalesce_multibranch_all_empty_no_else() {
        let store = MockStore::empty();
        let ex = exec();

        let coalesce = super::super::ast::CoalesceStmt {
            grain_type: GrainTypePlural::Facts,
            where_clause: None,
            branches: vec![super::super::ast::CoalesceBranch {
                query: CalStatement::Recall(RecallStmt {
                    grain_type: GrainTypePlural::Facts,
                    about: None,
                    where_clause: None,
                    recent: None,
                    since: None,
                    until: None,
                    like: None,
                    between: None,
                    contradictions: None,
                    limit: None,
                    as_format: None,
                    span: None,
                }),
                span: None,
            }],
            else_branch: None,
            span: None,
        };

        let query = CalQuery {
            version: super::super::ast::CalVersion(1),
            statement: CalStatement::Coalesce(coalesce.clone()),
            pipeline: Vec::new(),
            with_options: Vec::new(),
            format: None,
            let_bindings: Vec::new(),
            let_values: Default::default(),
            user_vars: HashMap::new(),
            warnings: Vec::new(),
        };

        let mut warnings = Vec::new();
        let result = ex
            .execute_coalesce(&coalesce, &store, &query, &mut warnings)
            .unwrap();

        match result {
            CalResultPayload::Grains { grains, .. } => {
                assert!(grains.is_empty(), "all empty, no ELSE → empty result");
            }
            other => panic!("expected Grains, got: {:?}", other),
        }
    }

    // -- WI-1.5: EXPLAIN with policy filters -----------------------------

    #[test]
    fn test_explain_reports_tier1_disabled() {
        let store = MockStore::empty();
        let ex = CalExecutor::new(CalExecutorConfig {
            tier1_enabled: false,
            ..Default::default()
        });
        let result = ex.execute("EXPLAIN RECALL facts", &store).unwrap();

        match result.result {
            CalResultPayload::Explain { plan } => {
                assert!(
                    plan.filters.iter().any(|f| f.contains("tier1_disabled")),
                    "should report tier1_disabled: {:?}",
                    plan.filters
                );
            }
            other => panic!("expected Explain, got: {:?}", other),
        }
    }

    #[test]
    fn test_explain_reports_namespace_override() {
        let store = MockStore::empty();
        let ex = CalExecutor::new(CalExecutorConfig {
            namespace_override: Some("test_ns".to_string()),
            ..Default::default()
        });
        let result = ex.execute("EXPLAIN RECALL facts", &store).unwrap();

        match result.result {
            CalResultPayload::Explain { plan } => {
                assert!(
                    plan.filters
                        .iter()
                        .any(|f| f.contains("namespace_override")),
                    "should report namespace_override: {:?}",
                    plan.filters
                );
            }
            other => panic!("expected Explain, got: {:?}", other),
        }
    }

    #[test]
    fn test_explain_reports_user_id_override() {
        let store = MockStore::empty();
        let ex = CalExecutor::new(CalExecutorConfig {
            user_id_override: Some("user123".to_string()),
            ..Default::default()
        });
        let result = ex.execute("EXPLAIN RECALL facts", &store).unwrap();

        match result.result {
            CalResultPayload::Explain { plan } => {
                assert!(
                    plan.filters.iter().any(|f| f.contains("user_id_override")),
                    "should report user_id_override: {:?}",
                    plan.filters
                );
            }
            other => panic!("expected Explain, got: {:?}", other),
        }
    }

    // -- WI-1.6: Grain-type-specific field filtering ---------------------

    #[test]
    fn test_type_specific_fields_tools() {
        let fields = super::type_specific_fields(&GrainTypePlural::Tools);
        assert!(fields.contains(&"tool"), "Tools should have 'tool' field");
        assert!(
            fields.contains(&"is_error"),
            "Tools should have 'is_error' field"
        );
        assert!(
            fields.contains(&"duration_ms"),
            "Tools should have 'duration_ms' field"
        );
    }

    #[test]
    fn test_type_specific_fields_goals() {
        let fields = super::type_specific_fields(&GrainTypePlural::Goals);
        assert!(fields.contains(&"title"), "Goals should have 'title' field");
        // `priority` and `status` are common fields (per spec they appear
        // across multiple grain types) and are validated via
        // `COMMON_FIELDS`, not via Goals-specific list.
        assert!(super::COMMON_FIELDS.contains(&"priority"));
        assert!(super::COMMON_FIELDS.contains(&"status"));
    }

    #[test]
    fn test_type_specific_fields_all_returns_empty() {
        let fields = super::type_specific_fields(&GrainTypePlural::All);
        assert!(
            fields.is_empty(),
            "All should return empty (no type-specific validation)"
        );
    }

    #[test]
    fn test_is_known_type_specific_field() {
        assert!(
            super::is_known_type_specific_field("tool"),
            "'tool' is known (on Tools)"
        );
        // `priority` is a common field — known overall via COMMON_FIELDS,
        // not via type-specific table.
        assert!(super::COMMON_FIELDS.contains(&"priority"));
        assert!(
            !super::is_known_type_specific_field("zzz_unknown"),
            "'zzz_unknown' is not known"
        );
    }

    #[test]
    fn test_grain_matches_condition_string_eq() {
        let grain = CalGrainResult {
            hash: "abc".into(),
            grain_type: "tool".into(),
            score: 1.0,
            fields: serde_json::json!({
                "tool": "web_search",
                "is_error": false,
                "duration_ms": 150
            }),
            score_breakdown: None,
            explanation: None,
            relative_time: None,
            is_deterministic: false,
            contested_by: None,
        };

        // String equality.
        assert!(super::grain_matches_condition(
            &grain,
            "tool",
            &super::super::ast::Comparator::Eq,
            &super::super::ast::Value::String {
                value: "web_search".into()
            }
        ));
        assert!(!super::grain_matches_condition(
            &grain,
            "tool",
            &super::super::ast::Comparator::Eq,
            &super::super::ast::Value::String {
                value: "db_query".into()
            }
        ));
    }

    #[test]
    fn test_grain_matches_condition_number_comparisons() {
        let grain = CalGrainResult {
            hash: "abc".into(),
            grain_type: "tool".into(),
            score: 1.0,
            fields: serde_json::json!({
                "duration_ms": 150.0
            }),
            score_breakdown: None,
            explanation: None,
            relative_time: None,
            is_deterministic: false,
            contested_by: None,
        };

        // Gte
        assert!(super::grain_matches_condition(
            &grain,
            "duration_ms",
            &super::super::ast::Comparator::Gte,
            &super::super::ast::Value::Number { value: 100.0 }
        ));
        assert!(super::grain_matches_condition(
            &grain,
            "duration_ms",
            &super::super::ast::Comparator::Gte,
            &super::super::ast::Value::Number { value: 150.0 }
        ));
        assert!(!super::grain_matches_condition(
            &grain,
            "duration_ms",
            &super::super::ast::Comparator::Gte,
            &super::super::ast::Value::Number { value: 200.0 }
        ));

        // Lt
        assert!(super::grain_matches_condition(
            &grain,
            "duration_ms",
            &super::super::ast::Comparator::Lt,
            &super::super::ast::Value::Number { value: 200.0 }
        ));
        assert!(!super::grain_matches_condition(
            &grain,
            "duration_ms",
            &super::super::ast::Comparator::Lt,
            &super::super::ast::Value::Number { value: 150.0 }
        ));
    }

    #[test]
    fn test_grain_matches_condition_boolean() {
        let grain = CalGrainResult {
            hash: "abc".into(),
            grain_type: "tool".into(),
            score: 1.0,
            fields: serde_json::json!({
                "is_error": false
            }),
            score_breakdown: None,
            explanation: None,
            relative_time: None,
            is_deterministic: false,
            contested_by: None,
        };

        assert!(super::grain_matches_condition(
            &grain,
            "is_error",
            &super::super::ast::Comparator::Eq,
            &super::super::ast::Value::Boolean { value: false }
        ));
        assert!(!super::grain_matches_condition(
            &grain,
            "is_error",
            &super::super::ast::Comparator::Eq,
            &super::super::ast::Value::Boolean { value: true }
        ));
    }

    #[test]
    fn test_grain_matches_condition_not_eq() {
        let grain = CalGrainResult {
            hash: "abc".into(),
            grain_type: "tool".into(),
            score: 1.0,
            fields: serde_json::json!({
                "tool": "web_search"
            }),
            score_breakdown: None,
            explanation: None,
            relative_time: None,
            is_deterministic: false,
            contested_by: None,
        };

        assert!(super::grain_matches_condition(
            &grain,
            "tool",
            &super::super::ast::Comparator::NotEq,
            &super::super::ast::Value::String {
                value: "db_query".into()
            }
        ));
        assert!(!super::grain_matches_condition(
            &grain,
            "tool",
            &super::super::ast::Comparator::NotEq,
            &super::super::ast::Value::String {
                value: "web_search".into()
            }
        ));
    }

    #[test]
    fn test_grain_matches_condition_missing_field_returns_false() {
        let grain = CalGrainResult {
            hash: "abc".into(),
            grain_type: "tool".into(),
            score: 1.0,
            fields: serde_json::json!({}),
            score_breakdown: None,
            explanation: None,
            relative_time: None,
            is_deterministic: false,
            contested_by: None,
        };

        // Missing field never matches Eq.
        assert!(!super::grain_matches_condition(
            &grain,
            "tool",
            &super::super::ast::Comparator::Eq,
            &super::super::ast::Value::String {
                value: "anything".into()
            }
        ));
        // Missing field DOES match NotEq (since !false = true).
        assert!(super::grain_matches_condition(
            &grain,
            "tool",
            &super::super::ast::Comparator::NotEq,
            &super::super::ast::Value::String {
                value: "anything".into()
            }
        ));
    }

    #[test]
    fn test_extract_type_specific_conditions_filters_common() {
        let condition = super::super::ast::Condition::And {
            left: Box::new(super::super::ast::Condition::Comparison {
                field: "subject".into(), // common field
                comparator: super::super::ast::Comparator::Eq,
                value: super::super::ast::Value::String {
                    value: "john".into(),
                },
                span: None,
            }),
            right: Box::new(super::super::ast::Condition::Comparison {
                field: "tool".into(), // type-specific field
                comparator: super::super::ast::Comparator::Eq,
                value: super::super::ast::Value::String {
                    value: "web_search".into(),
                },
                span: None,
            }),
            span: None,
        };

        let type_conds = super::extract_type_specific_conditions(&condition);
        assert_eq!(type_conds.len(), 1, "only 'tool' should be extracted");
        assert_eq!(type_conds[0].0, "tool");
    }

    #[test]
    fn test_collect_common_conditions() {
        let condition = super::super::ast::Condition::And {
            left: Box::new(super::super::ast::Condition::Comparison {
                field: "subject".into(), // common field
                comparator: super::super::ast::Comparator::Eq,
                value: super::super::ast::Value::String {
                    value: "john".into(),
                },
                span: None,
            }),
            right: Box::new(super::super::ast::Condition::Comparison {
                field: "tool".into(), // type-specific field
                comparator: super::super::ast::Comparator::Eq,
                value: super::super::ast::Value::String {
                    value: "web_search".into(),
                },
                span: None,
            }),
            span: None,
        };

        let mut common_conds = Vec::new();
        super::collect_common_conditions(&condition, &mut common_conds);
        assert_eq!(common_conds.len(), 1, "only 'subject' should be collected");
        assert_eq!(common_conds[0].0, "subject");
    }

    #[test]
    fn test_suggest_field_cross_type() {
        // "tool" is on Tools but not on Goals.
        let suggestion = super::suggest_field("tool", &GrainTypePlural::Goals);
        assert!(
            suggestion.is_some(),
            "should suggest 'tool' exists on another type"
        );
        assert!(
            suggestion.unwrap().contains("different grain type"),
            "should mention different grain type"
        );
    }

    #[test]
    fn test_suggest_field_substring_match() {
        // "stat" contains the substring checked against "status" on Goals.
        let suggestion = super::suggest_field("stat", &GrainTypePlural::Goals);
        assert!(
            suggestion.is_some(),
            "should suggest 'status' for substring 'stat'"
        );
    }

    // -- Type-specific fields coverage ------------------------------------

    #[test]
    fn test_type_specific_fields_events() {
        let fields = super::type_specific_fields(&GrainTypePlural::Events);
        assert!(fields.contains(&"session_id"));
        assert!(fields.contains(&"content"));
        assert!(fields.contains(&"created_at"));
    }

    #[test]
    fn test_type_specific_fields_states() {
        let fields = super::type_specific_fields(&GrainTypePlural::States);
        // Per spec §8.3/§6.3: context (required), plan + history (optional).
        // `checkpoint_data` was advertised here as "an Areev extension" but had
        // no struct field, serializer, or deserializer — it was never storable.
        // `session_id` is Event-only and no longer leaks here.
        assert!(fields.contains(&"context"));
        assert!(fields.contains(&"plan"));
        assert!(fields.contains(&"history"));
        assert!(!fields.contains(&"checkpoint_data"));
        assert!(!fields.contains(&"session_id"));
    }

    #[test]
    fn test_type_specific_fields_consents() {
        let fields = super::type_specific_fields(&GrainTypePlural::Consents);
        // `scope` is a common field (per spec §5.2 cross-grain) — validated
        // via COMMON_FIELDS, not via the consent type-specific table.
        assert!(super::COMMON_FIELDS.contains(&"scope"));
        assert!(fields.contains(&"granted"));
        assert!(fields.contains(&"subject_did"));
        assert!(fields.contains(&"grantee_did"));
    }

    #[test]
    fn test_type_specific_fields_observations() {
        let fields = super::type_specific_fields(&GrainTypePlural::Observations);
        assert!(fields.contains(&"sensor"));
        assert!(fields.contains(&"value"));
        assert!(fields.contains(&"unit"));
    }

    #[test]
    fn test_type_specific_fields_workflows() {
        let fields = super::type_specific_fields(&GrainTypePlural::Workflows);
        assert!(fields.contains(&"name"));
        // `status` is a common field; the Workflow-specific list no longer
        // duplicates it.
        assert!(super::COMMON_FIELDS.contains(&"status"));
        assert!(fields.contains(&"nodes"));
    }

    #[test]
    fn test_type_specific_fields_reasonings() {
        let fields = super::type_specific_fields(&GrainTypePlural::Reasonings);
        assert!(fields.contains(&"premises"));
        assert!(fields.contains(&"conclusion"));
        // `confidence` is common.
        assert!(super::COMMON_FIELDS.contains(&"confidence"));
    }

    #[test]
    fn test_session_id_not_in_common_fields() {
        // session_id must NOT be in COMMON_FIELDS so it gets picked up by
        // extract_type_specific_conditions as a post-filter.
        assert!(
            !super::COMMON_FIELDS.contains(&"session_id"),
            "session_id should not be in COMMON_FIELDS"
        );
    }

    #[test]
    fn test_session_id_extracted_as_type_specific() {
        let condition = super::super::ast::Condition::And {
            left: Box::new(super::super::ast::Condition::Comparison {
                field: "subject".into(),
                comparator: super::super::ast::Comparator::Eq,
                value: super::super::ast::Value::String {
                    value: "alice".into(),
                },
                span: None,
            }),
            right: Box::new(super::super::ast::Condition::Comparison {
                field: "session_id".into(),
                comparator: super::super::ast::Comparator::Eq,
                value: super::super::ast::Value::String {
                    value: "sess-001".into(),
                },
                span: None,
            }),
            span: None,
        };

        let type_conds = super::extract_type_specific_conditions(&condition);
        assert_eq!(
            type_conds.len(),
            1,
            "session_id should be extracted as type-specific"
        );
        assert_eq!(type_conds[0].0, "session_id");
    }

    #[test]
    fn test_goal_state_post_filter() {
        let grain = CalGrainResult {
            hash: "abc".into(),
            grain_type: "goal".into(),
            score: 1.0,
            fields: serde_json::json!({"goal_state": "active", "subject": "alice"}),
            score_breakdown: None,
            explanation: None,
            relative_time: None,
            is_deterministic: false,
            contested_by: None,
        };

        assert!(super::grain_matches_condition(
            &grain,
            "goal_state",
            &super::super::ast::Comparator::Eq,
            &super::super::ast::Value::String {
                value: "active".into()
            }
        ));
        assert!(!super::grain_matches_condition(
            &grain,
            "goal_state",
            &super::super::ast::Comparator::Eq,
            &super::super::ast::Value::String {
                value: "failed".into()
            }
        ));
    }

    #[test]
    fn test_session_id_post_filter() {
        let grain = CalGrainResult {
            hash: "abc".into(),
            grain_type: "event".into(),
            score: 1.0,
            fields: serde_json::json!({"session_id": "sess-001", "subject": "alice"}),
            score_breakdown: None,
            explanation: None,
            relative_time: None,
            is_deterministic: false,
            contested_by: None,
        };

        assert!(super::grain_matches_condition(
            &grain,
            "session_id",
            &super::super::ast::Comparator::Eq,
            &super::super::ast::Value::String {
                value: "sess-001".into()
            }
        ));
        assert!(!super::grain_matches_condition(
            &grain,
            "session_id",
            &super::super::ast::Comparator::Eq,
            &super::super::ast::Value::String {
                value: "sess-999".into()
            }
        ));
    }

    // -- Chat-event type-specific filter coverage (harness-chat-events) --

    #[test]
    fn event_role_listed_as_type_specific() {
        let fields = super::type_specific_fields(&GrainTypePlural::Events);
        assert!(fields.contains(&"role"));
        assert!(fields.contains(&"parent_message_id"));
        assert!(fields.contains(&"model_id"));
        assert!(fields.contains(&"stop_reason"));
    }

    #[test]
    fn event_role_not_in_common_fields() {
        assert!(!super::COMMON_FIELDS.contains(&"role"));
        assert!(!super::COMMON_FIELDS.contains(&"parent_message_id"));
        assert!(!super::COMMON_FIELDS.contains(&"model_id"));
        assert!(!super::COMMON_FIELDS.contains(&"stop_reason"));
    }

    #[test]
    fn role_eq_post_filter() {
        let grain = CalGrainResult {
            hash: "h1".into(),
            grain_type: "event".into(),
            score: 1.0,
            fields: serde_json::json!({"role": "user", "content": "hi"}),
            score_breakdown: None,
            explanation: None,
            relative_time: None,
            is_deterministic: false,
            contested_by: None,
        };
        assert!(super::grain_matches_condition(
            &grain,
            "role",
            &super::super::ast::Comparator::Eq,
            &super::super::ast::Value::String {
                value: "user".into()
            }
        ));
        assert!(!super::grain_matches_condition(
            &grain,
            "role",
            &super::super::ast::Comparator::Eq,
            &super::super::ast::Value::String {
                value: "assistant".into()
            }
        ));
    }

    #[test]
    fn role_in_list_post_filter() {
        use super::super::ast::{Condition, Value};

        let condition = Condition::In {
            field: "role".into(),
            values: vec![
                Value::String {
                    value: "user".into(),
                },
                Value::String {
                    value: "assistant".into(),
                },
            ],
            span: None,
        };
        let sets = super::extract_type_specific_set_conditions(&condition);
        assert_eq!(sets.len(), 1);
        let set = &sets[0];

        let user_grain = CalGrainResult {
            hash: "h1".into(),
            grain_type: "event".into(),
            score: 1.0,
            fields: serde_json::json!({"role": "user"}),
            score_breakdown: None,
            explanation: None,
            relative_time: None,
            is_deterministic: false,
            contested_by: None,
        };
        let tool_grain = CalGrainResult {
            hash: "h2".into(),
            grain_type: "event".into(),
            score: 1.0,
            fields: serde_json::json!({"role": "tool"}),
            score_breakdown: None,
            explanation: None,
            relative_time: None,
            is_deterministic: false,
            contested_by: None,
        };

        assert!(super::grain_matches_set_condition(&user_grain, set));
        assert!(!super::grain_matches_set_condition(&tool_grain, set));
    }

    #[test]
    fn parent_message_id_eq_post_filter() {
        let grain = CalGrainResult {
            hash: "h1".into(),
            grain_type: "event".into(),
            score: 1.0,
            fields: serde_json::json!({"parent_message_id": "deadbeef"}),
            score_breakdown: None,
            explanation: None,
            relative_time: None,
            is_deterministic: false,
            contested_by: None,
        };
        assert!(super::grain_matches_condition(
            &grain,
            "parent_message_id",
            &super::super::ast::Comparator::Eq,
            &super::super::ast::Value::String {
                value: "deadbeef".into()
            }
        ));
    }

    #[test]
    fn role_and_session_id_both_extracted_as_type_specific() {
        use super::super::ast::{Comparator, Condition, Value};
        let condition = Condition::And {
            left: Box::new(Condition::Comparison {
                field: "role".into(),
                comparator: Comparator::Eq,
                value: Value::String {
                    value: "user".into(),
                },
                span: None,
            }),
            right: Box::new(Condition::Comparison {
                field: "session_id".into(),
                comparator: Comparator::Eq,
                value: Value::String { value: "s1".into() },
                span: None,
            }),
            span: None,
        };
        let conds = super::extract_type_specific_conditions(&condition);
        let fields: Vec<&str> = conds.iter().map(|(f, _, _)| f.as_str()).collect();
        assert!(fields.contains(&"role"));
        assert!(fields.contains(&"session_id"));
    }

    // -- WI-1.1: ASSEMBLE WHERE clause -----------------------------------

    #[test]
    fn test_assemble_where_filters_results() {
        let store = MockStore::with_grains(vec![
            make_fact("john", "likes", "coffee"),
            make_fact("bob", "likes", "coffee"),
        ]);
        let ex = exec();

        let assemble = super::super::ast::AssembleStmt {
            topic: "test".into(),
            from: super::super::ast::Source::Query(Box::new(RecallStmt {
                grain_type: GrainTypePlural::Facts,
                about: None,
                where_clause: None,
                recent: None,
                since: None,
                until: None,
                like: None,
                between: None,
                contradictions: None,
                limit: None,
                as_format: None,
                span: None,
            })),
            where_clause: Some(super::super::ast::WhereClause {
                condition: super::super::ast::Condition::Comparison {
                    field: "subject".into(),
                    comparator: super::super::ast::Comparator::Eq,
                    value: super::super::ast::Value::String {
                        value: "john".into(),
                    },
                    span: None,
                },
                span: None,
            }),
            context_name: None,
            sources: None,
            budget: None,
            priority: None,
            format: None,
            for_whom: None,
            assemble_with: Vec::new(),
            with_options: Vec::new(),
            streaming: false,
            span: None,
        };

        let query = CalQuery {
            version: super::super::ast::CalVersion(1),
            statement: CalStatement::Assemble(assemble.clone()),
            pipeline: Vec::new(),
            with_options: Vec::new(),
            format: None,
            let_bindings: Vec::new(),
            let_values: Default::default(),
            user_vars: HashMap::new(),
            warnings: Vec::new(),
        };

        let mut warnings = Vec::new();
        let result = ex
            .execute_assemble(&assemble, &store, &query, &mut warnings)
            .unwrap();

        match result {
            CalResultPayload::Grains { grains, .. } => {
                assert_eq!(grains.len(), 1, "WHERE subject='john' should filter to 1");
                assert_eq!(
                    grains[0].fields.get("subject").and_then(|v| v.as_str()),
                    Some("john")
                );
            }
            other => panic!("expected Grains, got: {:?}", other),
        }
    }

    #[test]
    fn test_assemble_format_json() {
        let store = MockStore::with_grains(vec![make_fact("john", "likes", "coffee")]);
        let ex = exec();

        let assemble = super::super::ast::AssembleStmt {
            topic: "test".into(),
            from: super::super::ast::Source::Query(Box::new(RecallStmt {
                grain_type: GrainTypePlural::Facts,
                about: None,
                where_clause: None,
                recent: None,
                since: None,
                until: None,
                like: None,
                between: None,
                contradictions: None,
                limit: None,
                as_format: None,
                span: None,
            })),
            where_clause: None,
            context_name: None,
            sources: None,
            budget: None,
            priority: None,
            format: Some(super::super::ast::FormatClause::Single(
                super::super::ast::FormatSpec::Json,
            )),
            for_whom: None,
            assemble_with: Vec::new(),
            with_options: Vec::new(),
            streaming: false,
            span: None,
        };

        let query = CalQuery {
            version: super::super::ast::CalVersion(1),
            statement: CalStatement::Assemble(assemble.clone()),
            pipeline: Vec::new(),
            with_options: Vec::new(),
            format: None,
            let_bindings: Vec::new(),
            let_values: Default::default(),
            user_vars: HashMap::new(),
            warnings: Vec::new(),
        };

        let mut warnings = Vec::new();
        let result = ex
            .execute_assemble(&assemble, &store, &query, &mut warnings)
            .unwrap();

        // FORMAT json now returns a Grains payload directly (not Formatted text)
        // so that result.grains is a structured array.
        match result {
            CalResultPayload::Grains { grains, .. } => {
                assert_eq!(grains.len(), 1);
                assert_eq!(grains[0].grain_type, "fact");
                let subject = grains[0].fields.get("subject").and_then(|v| v.as_str());
                assert_eq!(subject, Some("john"));
            }
            other => panic!("expected Grains, got: {:?}", other),
        }
    }

    #[test]
    fn test_assemble_format_markdown() {
        let store = MockStore::with_grains(vec![make_fact("john", "likes", "coffee")]);
        let ex = exec();

        let assemble = super::super::ast::AssembleStmt {
            topic: "test".into(),
            from: super::super::ast::Source::Query(Box::new(RecallStmt {
                grain_type: GrainTypePlural::Facts,
                about: None,
                where_clause: None,
                recent: None,
                since: None,
                until: None,
                like: None,
                between: None,
                contradictions: None,
                limit: None,
                as_format: None,
                span: None,
            })),
            where_clause: None,
            context_name: None,
            sources: None,
            budget: None,
            priority: None,
            format: Some(super::super::ast::FormatClause::Single(
                super::super::ast::FormatSpec::Markdown,
            )),
            for_whom: None,
            assemble_with: Vec::new(),
            with_options: Vec::new(),
            streaming: false,
            span: None,
        };

        let query = CalQuery {
            version: super::super::ast::CalVersion(1),
            statement: CalStatement::Assemble(assemble.clone()),
            pipeline: Vec::new(),
            with_options: Vec::new(),
            format: None,
            let_bindings: Vec::new(),
            let_values: Default::default(),
            user_vars: HashMap::new(),
            warnings: Vec::new(),
        };

        let mut warnings = Vec::new();
        let result = ex
            .execute_assemble(&assemble, &store, &query, &mut warnings)
            .unwrap();

        match result {
            CalResultPayload::Formatted {
                text,
                format,
                grain_count,
                ..
            } => {
                assert_eq!(format, "markdown");
                assert_eq!(grain_count, 1);
                // Ungrouped markdown is a flat list of assertions: a `### fact
                // (hash)` heading above every line is noise in a prompt, which
                // is what this output is for. Grouped renders keep their
                // group heading — see the GROUP BY tests.
                assert!(
                    !text.contains("###"),
                    "ungrouped markdown should not carry per-grain headings, got: {text}"
                );
                // The lead of each line is bold — under the old dump that was
                // the literal field name (`**subject**:`), now it is the value
                // the line is about, which is what a reader needs.
                assert!(
                    text.starts_with("- **"),
                    "Markdown should render an assertion line, got: {text}"
                );
            }
            other => panic!("expected Formatted, got: {:?}", other),
        }
    }

    #[test]
    fn test_assemble_format_triples() {
        let store = MockStore::with_grains(vec![
            make_fact("john", "likes", "coffee"),
            make_fact("bob", "likes", "coffee"),
        ]);
        let ex = exec();

        let assemble = super::super::ast::AssembleStmt {
            topic: "test".into(),
            from: super::super::ast::Source::Query(Box::new(RecallStmt {
                grain_type: GrainTypePlural::Facts,
                about: None,
                where_clause: None,
                recent: None,
                since: None,
                until: None,
                like: None,
                between: None,
                contradictions: None,
                limit: None,
                as_format: None,
                span: None,
            })),
            where_clause: None,
            context_name: None,
            sources: None,
            budget: None,
            priority: None,
            format: Some(super::super::ast::FormatClause::Single(
                super::super::ast::FormatSpec::Triples,
            )),
            for_whom: None,
            assemble_with: Vec::new(),
            with_options: Vec::new(),
            streaming: false,
            span: None,
        };

        let query = CalQuery {
            version: super::super::ast::CalVersion(1),
            statement: CalStatement::Assemble(assemble.clone()),
            pipeline: Vec::new(),
            with_options: Vec::new(),
            format: None,
            let_bindings: Vec::new(),
            let_values: Default::default(),
            user_vars: HashMap::new(),
            warnings: Vec::new(),
        };

        let mut warnings = Vec::new();
        let result = ex
            .execute_assemble(&assemble, &store, &query, &mut warnings)
            .unwrap();

        match result {
            CalResultPayload::Formatted {
                text,
                format,
                grain_count,
                ..
            } => {
                assert_eq!(format, "triples");
                assert_eq!(grain_count, 2);
                // Triples are tab-separated: subject\trelation\tobject
                assert_eq!(text.lines().count(), 2, "should have 2 triple lines");
            }
            other => panic!("expected Formatted, got: {:?}", other),
        }
    }

    #[test]
    fn test_assemble_format_with_where_filters_then_formats() {
        // Test that WHERE is applied BEFORE FORMAT.
        let store = MockStore::with_grains(vec![
            make_fact("john", "likes", "coffee"),
            make_fact("bob", "likes", "coffee"),
        ]);
        let ex = exec();

        let assemble = super::super::ast::AssembleStmt {
            topic: "test".into(),
            from: super::super::ast::Source::Query(Box::new(RecallStmt {
                grain_type: GrainTypePlural::Facts,
                about: None,
                where_clause: None,
                recent: None,
                since: None,
                until: None,
                like: None,
                between: None,
                contradictions: None,
                limit: None,
                as_format: None,
                span: None,
            })),
            where_clause: Some(super::super::ast::WhereClause {
                condition: super::super::ast::Condition::Comparison {
                    field: "subject".into(),
                    comparator: super::super::ast::Comparator::Eq,
                    value: super::super::ast::Value::String {
                        value: "john".into(),
                    },
                    span: None,
                },
                span: None,
            }),
            context_name: None,
            sources: None,
            budget: None,
            priority: None,
            format: Some(super::super::ast::FormatClause::Single(
                super::super::ast::FormatSpec::Json,
            )),
            for_whom: None,
            assemble_with: Vec::new(),
            with_options: Vec::new(),
            streaming: false,
            span: None,
        };

        let query = CalQuery {
            version: super::super::ast::CalVersion(1),
            statement: CalStatement::Assemble(assemble.clone()),
            pipeline: Vec::new(),
            with_options: Vec::new(),
            format: None,
            let_bindings: Vec::new(),
            let_values: Default::default(),
            user_vars: HashMap::new(),
            warnings: Vec::new(),
        };

        let mut warnings = Vec::new();
        let result = ex
            .execute_assemble(&assemble, &store, &query, &mut warnings)
            .unwrap();

        // FORMAT json now returns Grains payload directly.
        match result {
            CalResultPayload::Grains { grains, .. } => {
                assert_eq!(grains.len(), 1, "WHERE should filter to 1 grain");
                let subject = grains[0].fields.get("subject").and_then(|v| v.as_str());
                assert_eq!(subject, Some("john"));
            }
            other => panic!("expected Grains, got: {:?}", other),
        }
    }

    #[test]
    fn test_assemble_without_where_returns_all() {
        let store = MockStore::with_grains(vec![
            make_fact("john", "likes", "coffee"),
            make_fact("bob", "likes", "coffee"),
        ]);
        let ex = exec();

        let assemble = super::super::ast::AssembleStmt {
            topic: "test".into(),
            from: super::super::ast::Source::Query(Box::new(RecallStmt {
                grain_type: GrainTypePlural::Facts,
                about: None,
                where_clause: None,
                recent: None,
                since: None,
                until: None,
                like: None,
                between: None,
                contradictions: None,
                limit: None,
                as_format: None,
                span: None,
            })),
            where_clause: None,
            context_name: None,
            sources: None,
            budget: None,
            priority: None,
            format: None,
            for_whom: None,
            assemble_with: Vec::new(),
            with_options: Vec::new(),
            streaming: false,
            span: None,
        };

        let query = CalQuery {
            version: super::super::ast::CalVersion(1),
            statement: CalStatement::Assemble(assemble.clone()),
            pipeline: Vec::new(),
            with_options: Vec::new(),
            format: None,
            let_bindings: Vec::new(),
            let_values: Default::default(),
            user_vars: HashMap::new(),
            warnings: Vec::new(),
        };

        let mut warnings = Vec::new();
        let result = ex
            .execute_assemble(&assemble, &store, &query, &mut warnings)
            .unwrap();

        match result {
            CalResultPayload::Grains { grains, .. } => {
                assert_eq!(grains.len(), 2, "no WHERE should return all");
            }
            other => panic!("expected Grains, got: {:?}", other),
        }
    }

    // ── Multi-source ASSEMBLE FORMAT tests ────────────────────────────

    #[test]
    fn test_multisource_assemble_format_json() {
        let store = MockStore::with_grains(vec![
            make_fact("john", "likes", "coffee"),
            make_fact("bob", "likes", "coffee"),
        ]);
        let ex = exec();

        let recall_facts = RecallStmt {
            grain_type: GrainTypePlural::Facts,
            about: None,
            where_clause: None,
            recent: None,
            since: None,
            until: None,
            like: None,
            between: None,
            contradictions: None,
            limit: None,
            as_format: None,
            span: None,
        };

        let assemble = super::super::ast::AssembleStmt {
            topic: "test".into(),
            from: super::super::ast::Source::Query(Box::new(recall_facts.clone())),
            where_clause: None,
            context_name: None,
            sources: Some(vec![super::super::ast::NamedSource {
                label: "facts".into(),
                query: Box::new(CalStatement::Recall(recall_facts)),
                with_options: vec![],
                span: None,
            }]),
            budget: None,
            priority: None,
            format: Some(super::super::ast::FormatClause::Single(
                super::super::ast::FormatSpec::Json,
            )),
            for_whom: None,
            assemble_with: Vec::new(),
            with_options: Vec::new(),
            streaming: false,
            span: None,
        };

        let query = CalQuery {
            version: super::super::ast::CalVersion(1),
            statement: CalStatement::Assemble(assemble.clone()),
            pipeline: Vec::new(),
            with_options: Vec::new(),
            format: None,
            let_bindings: Vec::new(),
            let_values: Default::default(),
            user_vars: HashMap::new(),
            warnings: Vec::new(),
        };

        let mut warnings = Vec::new();
        let result = ex
            .execute_assemble(&assemble, &store, &query, &mut warnings)
            .unwrap();

        // FORMAT json now returns Grains payload directly.
        match result {
            CalResultPayload::Grains { grains, .. } => {
                assert!(!grains.is_empty(), "should have grains");
                assert!(
                    grains
                        .iter()
                        .filter_map(|g| g.fields.get("subject").and_then(|v| v.as_str()))
                        .any(|x| x == "john"),
                    "should contain john"
                );
            }
            other => panic!(
                "expected Grains for multi-source FORMAT json, got: {:?}",
                other
            ),
        }
    }

    #[test]
    fn test_multisource_assemble_format_sml() {
        let store = MockStore::with_grains(vec![make_fact("john", "likes", "coffee")]);
        let ex = exec();

        let recall_facts = RecallStmt {
            grain_type: GrainTypePlural::Facts,
            about: None,
            where_clause: None,
            recent: None,
            since: None,
            until: None,
            like: None,
            between: None,
            contradictions: None,
            limit: None,
            as_format: None,
            span: None,
        };

        let assemble = super::super::ast::AssembleStmt {
            topic: "test".into(),
            from: super::super::ast::Source::Query(Box::new(recall_facts.clone())),
            where_clause: None,
            context_name: None,
            sources: Some(vec![super::super::ast::NamedSource {
                label: "events".into(),
                query: Box::new(CalStatement::Recall(recall_facts)),
                with_options: vec![],
                span: None,
            }]),
            budget: None,
            priority: None,
            format: Some(super::super::ast::FormatClause::Single(
                super::super::ast::FormatSpec::Sml,
            )),
            for_whom: None,
            assemble_with: Vec::new(),
            with_options: Vec::new(),
            streaming: false,
            span: None,
        };

        let query = CalQuery {
            version: super::super::ast::CalVersion(1),
            statement: CalStatement::Assemble(assemble.clone()),
            pipeline: Vec::new(),
            with_options: Vec::new(),
            format: None,
            let_bindings: Vec::new(),
            let_values: Default::default(),
            user_vars: HashMap::new(),
            warnings: Vec::new(),
        };

        let mut warnings = Vec::new();
        let result = ex
            .execute_assemble(&assemble, &store, &query, &mut warnings)
            .unwrap();

        match result {
            CalResultPayload::Formatted {
                text,
                format,
                grain_count,
                ..
            } => {
                assert_eq!(format, "sml");
                assert!(grain_count > 0, "should have grains");
                assert!(text.contains("<grains>"), "SML should contain <grains> tag");
            }
            other => panic!(
                "expected Formatted for multi-source FORMAT sml, got: {:?}",
                other
            ),
        }
    }

    #[test]
    fn test_multisource_assemble_without_format_returns_assembled() {
        let store = MockStore::with_grains(vec![make_fact("john", "likes", "coffee")]);
        let ex = exec();

        let recall_facts = RecallStmt {
            grain_type: GrainTypePlural::Facts,
            about: None,
            where_clause: None,
            recent: None,
            since: None,
            until: None,
            like: None,
            between: None,
            contradictions: None,
            limit: None,
            as_format: None,
            span: None,
        };

        let assemble = super::super::ast::AssembleStmt {
            topic: "test".into(),
            from: super::super::ast::Source::Query(Box::new(recall_facts.clone())),
            where_clause: None,
            context_name: None,
            sources: Some(vec![super::super::ast::NamedSource {
                label: "facts".into(),
                query: Box::new(CalStatement::Recall(recall_facts)),
                with_options: vec![],
                span: None,
            }]),
            budget: None,
            priority: None,
            format: None,
            for_whom: None,
            assemble_with: Vec::new(),
            with_options: Vec::new(),
            streaming: false,
            span: None,
        };

        let query = CalQuery {
            version: super::super::ast::CalVersion(1),
            statement: CalStatement::Assemble(assemble.clone()),
            pipeline: Vec::new(),
            with_options: Vec::new(),
            format: None,
            let_bindings: Vec::new(),
            let_values: Default::default(),
            user_vars: HashMap::new(),
            warnings: Vec::new(),
        };

        let mut warnings = Vec::new();
        let result = ex
            .execute_assemble(&assemble, &store, &query, &mut warnings)
            .unwrap();

        match result {
            CalResultPayload::Assembled { grains, .. } => {
                assert!(!grains.is_empty(), "should have assembled grains");
            }
            other => panic!("expected Assembled (no FORMAT), got: {:?}", other),
        }
    }

    // ── Multi-format executor tests (CAL spec v1.0.1) ─────────────────

    #[test]
    fn test_multi_format_rendering() {
        let store = MockStore::with_grains(vec![make_fact("john", "likes", "coffee")]);
        let ex = CalExecutor::with_defaults();

        let result = ex
            .execute("RECALL facts FORMAT [json, markdown]", &store)
            .unwrap();
        match result.result {
            CalResultPayload::MultiFormatted {
                formats,
                grain_count,
                ..
            } => {
                assert_eq!(grain_count, 1);
                assert_eq!(formats.len(), 2);
                assert!(formats.contains_key("json"), "should have json key");
                assert!(formats.contains_key("markdown"), "should have markdown key");
                // JSON rendering should contain the grain data.
                assert!(formats["json"].contains("john"));
                // Markdown rendering should contain the grain data.
                assert!(formats["markdown"].contains("john"));
            }
            other => panic!("expected MultiFormatted, got: {:?}", other),
        }
    }

    #[test]
    fn test_single_format_backward_compat() {
        let store = MockStore::with_grains(vec![make_fact("john", "likes", "coffee")]);
        let ex = CalExecutor::with_defaults();

        // FORMAT json now returns Grains payload directly (not Formatted text).
        let result = ex.execute("RECALL facts FORMAT json", &store).unwrap();
        match result.result {
            CalResultPayload::Grains { grains, .. } => {
                assert_eq!(grains.len(), 1);
            }
            other => panic!("expected Grains, got: {:?}", other),
        }
    }

    #[test]
    fn test_multi_format_single_element_list() {
        let store = MockStore::with_grains(vec![make_fact("john", "likes", "coffee")]);
        let ex = CalExecutor::with_defaults();

        // FORMAT [json] should produce MultiFormatted, not Formatted.
        let result = ex.execute("RECALL facts FORMAT [json]", &store).unwrap();
        match result.result {
            CalResultPayload::MultiFormatted {
                formats,
                grain_count,
                ..
            } => {
                assert_eq!(grain_count, 1);
                assert_eq!(formats.len(), 1);
                assert!(formats.contains_key("json"));
            }
            other => panic!("expected MultiFormatted, got: {:?}", other),
        }
    }

    #[test]
    fn test_multi_format_all_seven_types() {
        let store = MockStore::with_grains(vec![make_fact("john", "likes", "coffee")]);
        let ex = CalExecutor::with_defaults();

        let result = ex
            .execute(
                "RECALL facts FORMAT [json, markdown, yaml, text, sml]",
                &store,
            )
            .unwrap();
        match result.result {
            CalResultPayload::MultiFormatted { formats, .. } => {
                assert_eq!(formats.len(), 5);
                for key in &["json", "markdown", "yaml", "text", "sml"] {
                    assert!(formats.contains_key(*key), "missing format: {}", key);
                }
            }
            other => panic!("expected MultiFormatted, got: {:?}", other),
        }
    }

    #[test]
    fn test_no_format_returns_grains() {
        let store = MockStore::with_grains(vec![make_fact("john", "likes", "coffee")]);
        let ex = CalExecutor::with_defaults();

        // No FORMAT clause — returns Grains payload (unchanged behavior).
        let result = ex.execute("RECALL facts", &store).unwrap();
        assert!(matches!(result.result, CalResultPayload::Grains { .. }));
    }

    #[test]
    fn test_multi_format_with_aliases() {
        let store = MockStore::with_grains(vec![make_fact("john", "likes", "coffee")]);
        let ex = CalExecutor::with_defaults();

        let result = ex
            .execute(
                "RECALL facts FORMAT [json AS customers, markdown AS report]",
                &store,
            )
            .unwrap();
        match result.result {
            CalResultPayload::MultiFormatted {
                formats,
                grain_count,
                ..
            } => {
                assert_eq!(grain_count, 1);
                assert_eq!(formats.len(), 2);
                assert!(
                    formats.contains_key("customers"),
                    "should have aliased key 'customers'"
                );
                assert!(
                    formats.contains_key("report"),
                    "should have aliased key 'report'"
                );
                assert!(
                    !formats.contains_key("json"),
                    "should NOT have canonical key 'json'"
                );
                assert!(
                    !formats.contains_key("markdown"),
                    "should NOT have canonical key 'markdown'"
                );
                assert!(formats["customers"].contains("john"));
                assert!(formats["report"].contains("john"));
            }
            other => panic!("expected MultiFormatted, got: {:?}", other),
        }
    }

    #[test]
    fn test_multi_format_mixed_alias_and_no_alias() {
        let store = MockStore::with_grains(vec![make_fact("john", "likes", "coffee")]);
        let ex = CalExecutor::with_defaults();

        let result = ex
            .execute("RECALL facts FORMAT [json AS customers, markdown]", &store)
            .unwrap();
        match result.result {
            CalResultPayload::MultiFormatted {
                formats,
                grain_count,
                ..
            } => {
                assert_eq!(grain_count, 1);
                assert_eq!(formats.len(), 2);
                assert!(formats.contains_key("customers"), "aliased key");
                assert!(
                    formats.contains_key("markdown"),
                    "canonical key for non-aliased"
                );
            }
            other => panic!("expected MultiFormatted, got: {:?}", other),
        }
    }

    #[test]
    fn test_multi_format_template_alias() {
        let store = MockStore::with_grains(vec![make_fact("john", "likes", "coffee")]);
        let ex = CalExecutor::with_defaults();

        let result = ex
            .execute(
                r#"RECALL facts FORMAT [TEMPLATE "{{subject}}: {{object}}" AS summary, json]"#,
                &store,
            )
            .unwrap();
        match result.result {
            CalResultPayload::MultiFormatted {
                formats,
                grain_count,
                ..
            } => {
                assert_eq!(grain_count, 1);
                assert_eq!(formats.len(), 2);
                assert!(formats.contains_key("summary"), "template alias");
                assert!(formats.contains_key("json"), "json canonical");
                assert!(formats["summary"].contains("john: coffee"));
            }
            other => panic!("expected MultiFormatted, got: {:?}", other),
        }
    }

    // ── GROUP BY pipeline stage tests ─────────────────────────────────

    /// Build a fact grain with a custom `created_at_sec` for GROUP BY tests.
    fn make_fact_with_time(
        subject: &str,
        relation: &str,
        object: &str,
        created_at_sec: u32,
    ) -> (Hash, DeserializedGrain) {
        let mut fields: HashMap<String, serde_json::Value> = HashMap::new();
        fields.insert("subject".into(), serde_json::json!(subject));
        fields.insert("relation".into(), serde_json::json!(relation));
        fields.insert("object".into(), serde_json::json!(object));
        fields.insert("grain_type".into(), serde_json::json!("fact"));
        fields.insert("confidence".into(), serde_json::json!(0.9));
        fields.insert("created_at_sec".into(), serde_json::json!(created_at_sec));

        let mut hash_bytes = [0u8; 32];
        let key = format!("{}|{}|{}|{}", subject, relation, object, created_at_sec);
        for (i, b) in key.as_bytes().iter().enumerate().take(32) {
            hash_bytes[i] = *b;
        }
        let hash = Hash::from_bytes(&hash_bytes);

        let grain = DeserializedGrain {
            header: MgHeader {
                version: 1,
                flags: 0,
                grain_type: 0x01,
                ns_hash: 0,
                created_at_sec,
            },
            grain_type: GrainType::Fact,
            fields,
            hash,
        };
        (hash, grain)
    }

    #[test]
    fn test_group_by_parses() {
        let store = MockStore::with_grains(vec![make_fact("john", "likes", "coffee")]);
        let ex = CalExecutor::with_defaults();
        let result = ex.execute("RECALL facts GROUP BY subject", &store);
        assert!(result.is_ok(), "GROUP BY should parse and execute");
    }

    #[test]
    fn test_group_by_reorders_grains() {
        // Create grains with interleaved subjects: bob, john, bob, john.
        let store = MockStore::with_grains(vec![
            make_fact_with_time("bob", "likes", "tea", 100),
            make_fact_with_time("john", "likes", "coffee", 200),
            make_fact_with_time("bob", "likes", "vim", 300),
            make_fact_with_time("john", "likes", "rust", 400),
        ]);
        let ex = CalExecutor::with_defaults();

        let result = ex.execute("RECALL facts GROUP BY subject", &store).unwrap();
        match result.result {
            CalResultPayload::Grains { grains, .. } => {
                assert_eq!(grains.len(), 4);
                // Grains should be grouped: john grains first (earlier created_at_sec
                // in first grain? no — bob has 100 which is earliest). So bob first.
                // bob: 100, 300; john: 200, 400. Groups ordered by earliest.
                let subjects: Vec<&str> = grains
                    .iter()
                    .filter_map(|g| g.fields.get("subject").and_then(|v| v.as_str()))
                    .collect();
                assert_eq!(subjects, vec!["bob", "bob", "john", "john"]);
            }
            other => panic!("expected Grains, got: {:?}", other),
        }
    }

    #[test]
    fn test_group_by_chronological_within_group() {
        let store = MockStore::with_grains(vec![
            make_fact_with_time("john", "likes", "vim", 500),
            make_fact_with_time("john", "likes", "coffee", 100),
            make_fact_with_time("john", "likes", "rust", 300),
        ]);
        let ex = CalExecutor::with_defaults();

        let result = ex.execute("RECALL facts GROUP BY subject", &store).unwrap();
        match result.result {
            CalResultPayload::Grains { grains, .. } => {
                let times: Vec<u64> = grains
                    .iter()
                    .filter_map(|g| g.fields.get("created_at_sec").and_then(|v| v.as_u64()))
                    .collect();
                assert_eq!(
                    times,
                    vec![100, 300, 500],
                    "should be chronological within group"
                );
            }
            other => panic!("expected Grains, got: {:?}", other),
        }
    }

    #[test]
    fn test_group_by_sml_format() {
        let store = MockStore::with_grains(vec![
            make_fact_with_time("sess_1", "user", "hello", 100),
            make_fact_with_time("sess_2", "user", "world", 200),
            make_fact_with_time("sess_1", "assistant", "hi", 150),
        ]);
        let ex = CalExecutor::with_defaults();

        let result = ex
            .execute("RECALL facts GROUP BY subject FORMAT sml", &store)
            .unwrap();
        match result.result {
            CalResultPayload::Formatted { text, format, .. } => {
                assert_eq!(format, "sml");
                assert!(text.contains("<group key=\"sess_1\" count=\"2\">"));
                assert!(text.contains("<group key=\"sess_2\" count=\"1\">"));
                assert!(text.contains("</group>"));
            }
            other => panic!("expected Formatted, got: {:?}", other),
        }
    }

    #[test]
    fn test_group_by_markdown_format() {
        let store = MockStore::with_grains(vec![
            make_fact_with_time("sess_1", "user", "hello", 100),
            make_fact_with_time("sess_2", "user", "world", 200),
        ]);
        let ex = CalExecutor::with_defaults();

        let result = ex
            .execute("RECALL facts GROUP BY subject FORMAT markdown", &store)
            .unwrap();
        match result.result {
            CalResultPayload::Formatted { text, format, .. } => {
                assert_eq!(format, "markdown");
                assert!(text.contains("### sess_1 (1 memory)"));
                assert!(text.contains("### sess_2 (1 memory)"));
            }
            other => panic!("expected Formatted, got: {:?}", other),
        }
    }

    #[test]
    fn test_group_by_text_format() {
        let store = MockStore::with_grains(vec![
            make_fact_with_time("sess_1", "user", "hello", 100),
            make_fact_with_time("sess_1", "assistant", "hi", 150),
            make_fact_with_time("sess_2", "user", "world", 200),
        ]);
        let ex = CalExecutor::with_defaults();

        let result = ex
            .execute("RECALL facts GROUP BY subject FORMAT text", &store)
            .unwrap();
        match result.result {
            CalResultPayload::Formatted { text, format, .. } => {
                assert_eq!(format, "text");
                assert!(text.contains("--- Group 1/2: sess_1 (2 memories) ---"));
                assert!(text.contains("--- Group 2/2: sess_2 (1 memory) ---"));
                assert!(text.contains("[1]"));
                assert!(text.contains("[2]"));
            }
            other => panic!("expected Formatted, got: {:?}", other),
        }
    }

    #[test]
    fn test_group_by_json_format() {
        let store = MockStore::with_grains(vec![
            make_fact_with_time("sess_1", "user", "hello", 100),
            make_fact_with_time("sess_2", "user", "world", 200),
        ]);
        let ex = CalExecutor::with_defaults();

        let result = ex
            .execute("RECALL facts GROUP BY subject FORMAT json", &store)
            .unwrap();
        match result.result {
            CalResultPayload::Formatted { text, format, .. } => {
                assert_eq!(format, "json");
                assert!(text.contains("\"group_key\""));
                assert!(text.contains("\"sess_1\""));
                assert!(text.contains("\"sess_2\""));
                assert!(text.contains("\"count\""));
            }
            other => panic!("expected Formatted, got: {:?}", other),
        }
    }

    #[test]
    fn test_group_by_missing_field_present_field() {
        // Grains without the grouped-by field should go to empty-key group.
        // Use `relation`, a common field that exists on every grain.
        let store = MockStore::with_grains(vec![make_fact("john", "likes", "coffee")]);
        let ex = CalExecutor::with_defaults();

        let result = ex
            .execute("RECALL facts GROUP BY relation FORMAT text", &store)
            .unwrap();
        match result.result {
            CalResultPayload::Formatted { text, format, .. } => {
                assert_eq!(format, "text");
                assert!(text.contains("--- Group 1/1:"));
            }
            other => panic!("expected Formatted, got: {:?}", other),
        }
    }

    #[test]
    fn test_group_by_unknown_field_rejected() {
        // Pipeline-stage field references are validated against the closed
        // common + type-specific field set. `session_id` is an Event-only
        // field, so GROUP BY session_id on RECALL facts must CAL-E060.
        let store = MockStore::with_grains(vec![make_fact("john", "likes", "coffee")]);
        let ex = CalExecutor::with_defaults();

        let err = ex
            .execute("RECALL facts GROUP BY session_id FORMAT text", &store)
            .expect_err("GROUP BY unknown field must be rejected per Bug 9");
        assert_eq!(err.code(), "CAL-E060");
    }

    // -----------------------------------------------------------------------
    // WITH VARS end-to-end tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_with_vars_template_substitution() {
        let (hash_a, grain_a) = make_fact("john", "likes", "coffee");
        let store = MockStore::with_grains(vec![(hash_a, grain_a)]);
        let ex = CalExecutor::with_defaults();

        let result = ex
            .execute(
                r#"RECALL facts FORMAT TEMPLATE "User: {{$user_name}} | {{subject}} {{relation}} {{object}}" WITH VARS { "user_name": "John" }"#,
                &store,
            )
            .unwrap();
        match result.result {
            CalResultPayload::Formatted { text, format, .. } => {
                assert_eq!(format, "template");
                assert!(
                    text.contains("User: John |"),
                    "user var not substituted: {}",
                    text
                );
                assert!(text.contains("john"), "subject not substituted: {}", text);
                assert!(text.contains("coffee"), "object not substituted: {}", text);
            }
            other => panic!("expected Formatted, got: {:?}", other),
        }
    }

    #[test]
    fn test_with_vars_missing_var_renders_empty() {
        let (hash_a, grain_a) = make_fact("john", "likes", "coffee");
        let store = MockStore::with_grains(vec![(hash_a, grain_a)]);
        let ex = CalExecutor::with_defaults();

        let result = ex
            .execute(
                r#"RECALL facts FORMAT TEMPLATE "{{$missing}} | {{subject}}" WITH VARS { "other": "val" }"#,
                &store,
            )
            .unwrap();
        match result.result {
            CalResultPayload::Formatted { text, .. } => {
                // {{$missing}} should remain as-is (simple replacement only replaces known keys)
                assert!(
                    text.contains("john"),
                    "subject should be resolved: {}",
                    text
                );
            }
            other => panic!("expected Formatted, got: {:?}", other),
        }
    }

    #[test]
    fn test_with_vars_parsed_into_query() {
        let store = MockStore::empty();
        let ex = CalExecutor::with_defaults();

        let result = ex
            .execute(
                r#"RECALL facts WITH VARS { "app": "test", "version": "1.0" }"#,
                &store,
            )
            .unwrap();
        // Query should parse and execute without error even without FORMAT
        assert_eq!(result.metadata.statement_type, "recall");
    }

    // --- Bug 86d29rjng: relation field rendering tests ---

    /// Helper: create a CalGrainResult with content and relation fields (event-style grain).
    fn make_event_grain_result(content: &str, relation: &str, subject: &str) -> CalGrainResult {
        let mut fields = serde_json::Map::new();
        fields.insert("subject".into(), serde_json::json!(subject));
        fields.insert("relation".into(), serde_json::json!(relation));
        fields.insert("content".into(), serde_json::json!(content));
        CalGrainResult {
            hash: "aabbccdd00112233".into(),
            grain_type: "event".into(),
            score: 1.0,
            fields: serde_json::Value::Object(fields),
            score_breakdown: None,
            explanation: None,
            relative_time: None,
            is_deterministic: false,
            contested_by: None,
        }
    }

    /// Helper: create a CalGrainResult without a relation field (event with no speaker).
    fn make_event_grain_result_no_relation(content: &str, subject: &str) -> CalGrainResult {
        let mut fields = serde_json::Map::new();
        fields.insert("subject".into(), serde_json::json!(subject));
        fields.insert("content".into(), serde_json::json!(content));
        CalGrainResult {
            hash: "aabbccdd00112233".into(),
            grain_type: "event".into(),
            score: 1.0,
            fields: serde_json::Value::Object(fields),
            score_breakdown: None,
            explanation: None,
            relative_time: None,
            is_deterministic: false,
            contested_by: None,
        }
    }

    /// Shared-renderer view of a test CalGrainResult.
    fn view_of(grain: &CalGrainResult) -> crate::render::GrainView<'_> {
        grain_view(grain)
    }

    #[test]
    fn test_render_sml_event_carries_speaker_role() {
        let grain = make_event_grain_result("I am a hair stylist.", "user", "s000");
        let out = crate::render::render_grain_sml(&view_of(&grain), crate::render::MetadataDetail::Minimal);
        assert!(
            out.contains(r#"role="user""#),
            "SML should carry the speaker as a role attribute, got: {out}"
        );
        assert!(
            out.starts_with("<event"),
            "events render as semantic <event> elements, got: {out}"
        );
        assert!(out.contains("I am a hair stylist."));
    }

    #[test]
    fn test_render_sml_omits_role_attribute_when_absent() {
        let grain = make_event_grain_result_no_relation("Hello world", "s000");
        let out = crate::render::render_grain_sml(&view_of(&grain), crate::render::MetadataDetail::Minimal);
        assert!(
            !out.contains("role="),
            "SML should not have a role attribute when the field is absent, got: {out}"
        );
    }

    #[test]
    fn test_render_sml_omits_role_attribute_when_empty() {
        let grain = make_event_grain_result("Hello world", "", "s000");
        let out = crate::render::render_grain_sml(&view_of(&grain), crate::render::MetadataDetail::Minimal);
        assert!(
            !out.contains("role=\"\""),
            "SML should not have an empty role attribute, got: {out}"
        );
    }

    #[test]
    fn test_render_sml_escapes_role_attribute() {
        let grain = make_event_grain_result("test", "user<script>", "s000");
        let out = crate::render::render_grain_sml(&view_of(&grain), crate::render::MetadataDetail::Minimal);
        assert!(
            out.contains("role=\"user&lt;script&gt;\""),
            "SML should escape the role attribute value, got: {out}"
        );
    }

    #[test]
    fn test_render_markdown_prefixes_content_with_relation() {
        let grain = make_event_grain_result("I am a hair stylist.", "user", "s000");
        let out = crate::render::render_grain_markdown(&view_of(&grain));
        assert!(
            out.contains("- **user**: I am a hair stylist."),
            "Markdown should prefix content with relation, got: {out}"
        );
    }

    #[test]
    fn test_render_markdown_no_relation_names_the_subject() {
        let grain = make_event_grain_result_no_relation("Hello world", "s000");
        let out = crate::render::render_grain_markdown(&view_of(&grain));
        // With no speaker to name, lead with what the text is about. The old
        // output led with the literal field name (`**content**:`), which tells
        // a reader nothing they cannot see.
        assert!(
            out.contains("- **s000**: Hello world"),
            "Markdown should name the subject when there is no relation, got: {out}"
        );
    }

    #[test]
    fn test_render_markdown_speaker_disambiguation() {
        // Two grains from same session, different speakers — must be distinguishable.
        let assistant = make_event_grain_result("I am a school teacher.", "assistant", "s000");
        let user = make_event_grain_result("I am a hair stylist.", "user", "s000");
        let mut out = String::new();
        out.push_str(&crate::render::render_grain_markdown(&view_of(&assistant)));
        out.push_str(&crate::render::render_grain_markdown(&view_of(&user)));
        assert!(
            out.contains("- **assistant**: I am a school teacher."),
            "Markdown should show assistant role, got: {out}"
        );
        assert!(
            out.contains("- **user**: I am a hair stylist."),
            "Markdown should show user role, got: {out}"
        );
    }

    #[test]
    fn test_render_text_event_with_subject_relation_uses_triple_path() {
        // Event grains with subject + relation take the triple path (subject relation object).
        // The relation (speaker role) IS visible via the triple rendering.
        let grain = make_event_grain_result("I am a hair stylist.", "user", "s000");
        let out = crate::render::render_grain_text_line(&view_of(&grain), None);
        assert!(
            out.contains("s000 user"),
            "Text should render subject + relation via triple path, got: {out}"
        );
    }

    #[test]
    fn test_render_text_content_only_no_relation() {
        // Grain with only content (no triple fields) renders content directly.
        let grain = make_event_grain_result_no_relation("Hello world", "");
        let out = crate::render::render_grain_text_line(&view_of(&grain), None);
        assert_eq!(
            out.trim(),
            "Hello world",
            "Text should render content without prefix when no triple fields"
        );
    }

    #[test]
    fn test_render_text_speaker_disambiguation_via_triple() {
        // Two grains from same session, different speakers — distinguishable via triple path.
        let assistant = make_event_grain_result("I am a school teacher.", "assistant", "s000");
        let user = make_event_grain_result("I am a hair stylist.", "user", "s000");
        let mut out = String::new();
        out.push_str(&crate::render::render_grain_text_line(&view_of(&assistant), Some(1)));
        out.push_str(&crate::render::render_grain_text_line(&view_of(&user), Some(2)));
        assert!(
            out.contains("[1] s000 assistant"),
            "Text should show assistant relation in triple, got: {out}"
        );
        assert!(
            out.contains("[2] s000 user"),
            "Text should show user relation in triple, got: {out}"
        );
    }

    #[test]
    fn test_render_sml_fact_states_the_triple_inline() {
        // Semantic SML says the assertion as a sentence, relation included.
        let (_, grain) = make_fact("john", "likes", "coffee");
        let cgr = CalGrainResult {
            hash: grain.hash.to_hex(),
            grain_type: "fact".into(),
            score: 1.0,
            fields: serde_json::to_value(&grain.fields).unwrap(),
            score_breakdown: None,
            explanation: None,
            relative_time: None,
            is_deterministic: false,
            contested_by: None,
        };
        let out = crate::render::render_grain_sml(&view_of(&cgr), crate::render::MetadataDetail::Minimal);
        assert!(
            out.starts_with("<fact") && out.contains("john likes coffee"),
            "SML fact should state the triple inline, got: {out}"
        );
    }

    // -----------------------------------------------------------------------
    // Scope enforcement tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_scope_read_allows_recall() {
        let store = MockStore::empty();
        let config = CalExecutorConfig {
            caller_scopes: vec!["read".to_string()],
            ..Default::default()
        };
        let ex = CalExecutor::new(config);
        let result = ex.execute("RECALL facts LIMIT 1", &store);
        assert!(result.is_ok(), "read scope should allow RECALL");
    }

    #[test]
    fn test_scope_read_blocks_add() {
        let store = MockStore::empty();
        let config = CalExecutorConfig {
            caller_scopes: vec!["read".to_string()],
            ..Default::default()
        };
        let ex = CalExecutor::new(config);
        let result = ex.execute(
            r#"ADD fact SET subject = "x" SET relation = "y" SET object = "z" REASON "test""#,
            &store,
        );
        assert!(result.is_err(), "read scope should block ADD");
        let err = result.unwrap_err();
        assert_eq!(err.code(), "CAL-E114");
    }

    #[test]
    fn test_scope_write_allows_add() {
        let store = MockStore::empty();
        let config = CalExecutorConfig {
            caller_scopes: vec!["read".to_string(), "write".to_string()],
            ..Default::default()
        };
        let ex = CalExecutor::new(config);
        let result = ex.execute(
            r#"ADD fact SET subject = "x" SET relation = "y" SET object = "z" REASON "test""#,
            &store,
        );
        if let Err(ref e) = result {
            assert_ne!(e.code(), "CAL-E114", "write scope should not block ADD");
        }
    }

    #[test]
    fn test_forget_hash_parses_user_scope_rejected() {
        // `FORGET <hash>` is a valid CAL statement (gated at execution by
        // allow_destructive_ops). The USER/SCOPE targets are NOT reachable
        // from CAL text — only the hash form parses — because user/scope
        // crypto-erasure has no store backing.
        let store = MockStore::empty();
        // Empty scopes → no scope enforcement; isolate the grammar/gate layers.
        let ex = CalExecutor::new(CalExecutorConfig {
            allow_destructive_ops: true,
            ..Default::default()
        });

        // Hash form parses (no parse error).
        let hash = format!("sha256:{}", "a".repeat(64));
        ex.execute(&format!("FORGET {hash}"), &store)
            .expect("FORGET <hash> is a valid CAL statement");

        // USER form is rejected at parse time with CAL-E002.
        let err = ex
            .execute("FORGET USER \"test\"", &store)
            .expect_err("FORGET USER is not reachable from CAL text");
        assert_eq!(err.code(), "CAL-E002");
    }

    #[test]
    fn test_forget_requires_admin_scope_when_scoped() {
        // Independent of allow_destructive_ops: when caller_scopes are enforced
        // (server path), FORGET requires the "admin" scope. read+write is not
        // enough — a capability token can permit writes yet forbid erasure.
        let store = MockStore::empty();
        let hash = format!("sha256:{}", "a".repeat(64));
        let ex = CalExecutor::new(CalExecutorConfig {
            caller_scopes: vec!["read".to_string(), "write".to_string()],
            allow_destructive_ops: true,
            ..Default::default()
        });
        let err = ex
            .execute(&format!("FORGET {hash}"), &store)
            .expect_err("FORGET needs admin scope");
        assert!(
            matches!(err, CalError::InsufficientScope { .. }),
            "expected InsufficientScope, got {err:?}"
        );
    }

    #[test]
    fn test_forget_hash_gated_by_allow_destructive_ops() {
        // With destructive ops disabled, `FORGET <hash>` still parses but the
        // executor returns Unsupported instead of touching the store.
        let store = MockStore::empty();
        let hash = format!("sha256:{}", "a".repeat(64));
        let ex = CalExecutor::new(CalExecutorConfig {
            allow_destructive_ops: false,
            ..Default::default()
        });
        let res = ex
            .execute(&format!("FORGET {hash}"), &store)
            .expect("FORGET <hash> parses even when disabled");
        match res.result {
            CalResultPayload::Unsupported { statement, message } => {
                assert_eq!(statement, "forget");
                assert!(message.contains("disabled"), "unexpected message: {message}");
            }
            other => panic!("expected Unsupported when disabled, got {other:?}"),
        }
    }

    #[test]
    fn test_admin_forget_still_rejected_at_parse_time() {
        // `FORGET USER "…"` is not reachable from CAL text regardless of scope
        // — only `FORGET <hash>` parses. User-scoped erasure has no store
        // backing and is not exposed through the query language.
        let store = MockStore::empty();
        let config = CalExecutorConfig {
            caller_scopes: vec!["read".to_string(), "write".to_string(), "admin".to_string()],
            allow_destructive_ops: true,
            ..Default::default()
        };
        let ex = CalExecutor::new(config);
        let err = ex
            .execute("FORGET USER \"nonexistent\"", &store)
            .expect_err("FORGET rejected unconditionally");
        assert_eq!(err.code(), "CAL-E002");
    }

    #[test]
    fn test_scope_empty_no_enforcement() {
        let store = MockStore::empty();
        let config = CalExecutorConfig {
            caller_scopes: vec![], // empty = CLI/test mode
            ..Default::default()
        };
        let ex = CalExecutor::new(config);
        let result = ex.execute(
            r#"ADD fact SET subject = "x" SET relation = "y" SET object = "z" REASON "test""#,
            &store,
        );
        if let Err(ref e) = result {
            assert_ne!(e.code(), "CAL-E114", "empty scopes should skip enforcement");
        }
    }

    #[test]
    fn test_scope_admin_bypasses_all() {
        let store = MockStore::empty();
        let config = CalExecutorConfig {
            caller_scopes: vec!["admin".to_string()],
            ..Default::default()
        };
        let ex = CalExecutor::new(config);
        // Admin with just "admin" scope (no explicit "write") should still allow ADD
        let result = ex.execute(
            r#"ADD fact SET subject = "x" SET relation = "y" SET object = "z" REASON "test""#,
            &store,
        );
        if let Err(ref e) = result {
            assert_ne!(e.code(), "CAL-E114", "admin scope should bypass all checks");
        }
    }

    // -- Deterministic grains bypass post-merge min_score (ClickUp 86d2x9j7k) -----
    //
    // RECALL sources without an ABOUT clause produce structurally-scored
    // (or sentinel-scored) grains. A `WITH min_score(...)` on a multi-source
    // ASSEMBLE was silently dropping these, even though the user selected
    // them via PRIORITY/BUDGET, not by relevance.
    #[test]
    fn test_assemble_post_merge_min_score_keeps_deterministic_grains() {
        let ex = exec();
        let store = MockStore::empty();

        let semantic_low = CalGrainResult {
            hash: "11".repeat(32),
            grain_type: "fact".into(),
            score: 0.10,
            fields: serde_json::json!({"subject": "a", "relation": "r", "object": "o"}),
            score_breakdown: None,
            explanation: None,
            relative_time: None,
            is_deterministic: false,
            contested_by: None,
        };
        let semantic_high = CalGrainResult {
            hash: "22".repeat(32),
            grain_type: "fact".into(),
            score: 0.90,
            fields: serde_json::json!({"subject": "b", "relation": "r", "object": "o"}),
            score_breakdown: None,
            explanation: None,
            relative_time: None,
            is_deterministic: false,
            contested_by: None,
        };
        let deterministic = CalGrainResult {
            hash: "33".repeat(32),
            grain_type: "workflow".into(),
            score: 0.0, // sentinel — no semantic comparison was performed
            fields: serde_json::json!({"name": "boot", "status": "active"}),
            score_breakdown: None,
            explanation: None,
            relative_time: None,
            is_deterministic: true,
            contested_by: None,
        };

        let payload = CalResultPayload::Assembled {
            grains: vec![
                semantic_low.clone(),
                semantic_high.clone(),
                deterministic.clone(),
            ],
            sources: vec![],
            total_tokens: 0,
            budget_limit: Some(4000),
            progressive: false,
            total_available: Some(3),
        };

        let mut warnings = Vec::new();
        let out = ex
            .apply_assemble_post_merge_options(
                payload,
                &[WithOption::MinScore { score: 0.5 }],
                &mut warnings,
                &store,
                "test topic",
            )
            .unwrap();

        match out {
            CalResultPayload::Assembled { grains, .. } => {
                let kept_hashes: Vec<&str> = grains.iter().map(|g| g.hash.as_str()).collect();
                assert!(
                    kept_hashes.contains(&semantic_high.hash.as_str()),
                    "high-scoring semantic grain must be retained, got: {:?}",
                    kept_hashes
                );
                assert!(
                    !kept_hashes.contains(&semantic_low.hash.as_str()),
                    "low-scoring semantic grain must be dropped, got: {:?}",
                    kept_hashes
                );
                assert!(
                    kept_hashes.contains(&deterministic.hash.as_str()),
                    "deterministic-source grain must NOT be dropped by min_score, got: {:?}",
                    kept_hashes
                );
            }
            other => panic!("expected Assembled payload, got: {:?}", other),
        }
    }

    // -- Recall with no ABOUT marks results as deterministic --------------------
    #[test]
    fn test_execute_recall_marks_no_about_as_deterministic() {
        let store = MockStore::with_grains(vec![
            make_fact("alice", "knows", "bob"),
            make_fact("alice", "knows", "carol"),
        ]);
        let ex = exec();

        let recall_stmt = RecallStmt {
            grain_type: GrainTypePlural::Facts,
            about: None, // deterministic — no semantic query
            where_clause: Some(super::super::ast::WhereClause {
                condition: super::super::ast::Condition::Comparison {
                    field: "subject".into(),
                    comparator: super::super::ast::Comparator::Eq,
                    value: super::super::ast::Value::String {
                        value: "alice".into(),
                    },
                    span: None,
                },
                span: None,
            }),
            recent: None,
            since: None,
            until: None,
            like: None,
            between: None,
            contradictions: None,
            limit: None,
            as_format: None,
            span: None,
        };

        let query = CalQuery {
            version: super::super::ast::CalVersion(1),
            statement: CalStatement::Recall(recall_stmt.clone()),
            pipeline: Vec::new(),
            with_options: Vec::new(),
            format: None,
            let_bindings: Vec::new(),
            let_values: Default::default(),
            user_vars: HashMap::new(),
            warnings: Vec::new(),
        };

        let mut warnings = Vec::new();
        let payload = ex
            .execute_recall(&recall_stmt, &store, &query, &mut warnings)
            .unwrap();

        match payload {
            CalResultPayload::Grains { grains, .. } => {
                assert!(!grains.is_empty(), "expected at least one grain");
                assert!(
                    grains.iter().all(|g| g.is_deterministic),
                    "every grain from a no-ABOUT RECALL must be flagged deterministic"
                );
            }
            other => panic!("expected Grains payload, got: {:?}", other),
        }
    }

    // -- Recall WITH ABOUT does NOT mark results as deterministic ---------------
    #[test]
    fn test_execute_recall_with_about_not_deterministic() {
        let store = MockStore::with_grains(vec![make_fact("alice", "likes", "coffee")]);
        let ex = exec();

        let recall_stmt = RecallStmt {
            grain_type: GrainTypePlural::Facts,
            about: Some(super::super::ast::AboutClause {
                text: "coffee preferences".into(),
                span: None,
            }),
            where_clause: None,
            recent: None,
            since: None,
            until: None,
            like: None,
            between: None,
            contradictions: None,
            limit: None,
            as_format: None,
            span: None,
        };

        let query = CalQuery {
            version: super::super::ast::CalVersion(1),
            statement: CalStatement::Recall(recall_stmt.clone()),
            pipeline: Vec::new(),
            with_options: Vec::new(),
            format: None,
            let_bindings: Vec::new(),
            let_values: Default::default(),
            user_vars: HashMap::new(),
            warnings: Vec::new(),
        };

        let mut warnings = Vec::new();
        let payload = ex
            .execute_recall(&recall_stmt, &store, &query, &mut warnings)
            .unwrap();

        match payload {
            CalResultPayload::Grains { grains, .. } => {
                assert!(
                    grains.iter().all(|g| !g.is_deterministic),
                    "grains from an ABOUT RECALL must NOT be flagged deterministic"
                );
            }
            other => panic!("expected Grains payload, got: {:?}", other),
        }
    }
}
