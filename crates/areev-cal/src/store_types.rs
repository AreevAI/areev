//! Engine query/response types.
//!
//! Contains all public-facing types used by the Areev engine API:
//! - Query parameter types (`RecallParams`, `DiversityConfig`, `DiversityMethod`)
//! - Result/response types (`SearchHit`, `ScoreBreakdown`, `DetailedStats`, etc.)
//! - Session types (`SessionBootstrap`, `ToolSummary`, `ToolStats`)
//! - Intelligence types (`ConsolidationResult`, `ConsolidationGroupInfo`, `CompiledContext`)
//! - Engine events (`EngineEvent`)
//! - Internal cache alias (`GrainCache`)
//!
//! Provenance chain types (`ProvenanceRecord`, `QueryMode`, `ExcludedCandidate`, etc.)
//! live in `super::provenance`.

use std::collections::HashMap;
use std::sync::Arc;

use parking_lot::Mutex;

use areev_core::error::Hash;
use areev_core::format::deserialize::DeserializedGrain;
use areev_core::types::GrainType;

/// Request-scoped callback for forwarding LLM usage into `MeteringContext`.
///
/// Wraps an `Arc<dyn Fn>` so `RecallParams` can still derive `Debug` + `Clone`
/// (the raw `Arc<dyn Fn>` is not `Debug`). The closure is zero-cost when
/// absent (`Option::None`) and captures exactly one `Arc<MeteringContext>` in
/// the HTTP/gRPC/MCP/A2A handler construction site.
#[cfg(feature = "llm-rerank")]
#[derive(Clone)]
pub struct InferenceHook(pub Arc<dyn Fn(&InferenceUsage) + Send + Sync>);

#[cfg(feature = "llm-rerank")]
impl InferenceHook {
    pub fn new<F>(f: F) -> Self
    where
        F: Fn(&InferenceUsage) + Send + Sync + 'static,
    {
        Self(Arc::new(f))
    }

    pub fn call(&self, usage: &InferenceUsage) {
        (self.0)(usage);
    }
}

#[cfg(feature = "llm-rerank")]
impl std::fmt::Debug for InferenceHook {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InferenceHook").finish_non_exhaustive()
    }
}

/// Origin of a search result in the recall pipeline.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecallSource {
    /// From the primary recall query.
    Primary,
    /// From rule-based query expansion (FR-A008).
    Expansion,
    /// From session-census namespace backfill (FR-005).
    Census,
}

/// Report returned by `Areev::rebuild_indexes()`.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct RebuildReport {
    pub grains_scanned: usize,
    pub fts_indexed: usize,
    pub vectors_indexed: usize,
    pub errors: Vec<String>,
    pub elapsed_ms: u64,
}

/// Detailed database statistics for dashboard.
#[derive(Debug, serde::Serialize)]
pub struct DetailedStats {
    pub total_grains: usize,
    pub disk_space_bytes: u64,
    pub namespaces: Vec<String>,
    pub users: Vec<String>,
    pub type_counts: std::collections::BTreeMap<String, usize>,
    pub audit_entries: Option<usize>,
    pub has_encryption: bool,
    pub has_policy: bool,
    /// True when the HNSW node_id→hash mapping was restored from the `graph_store`
    /// checkpoint partition on startup (fast path), rather than rebuilt from a full
    /// scan of the `vectors` partition (slow path). Always `false` when the vector
    /// feature is disabled or the index is in brute-force mode.
    pub vector_mapping_loaded: bool,
}

/// Whether a grain is the current or outdated version in a conflict group.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConflictStatus {
    Current,
    Outdated,
}

/// Whether a grain is current, superseded, or a superseder in the recall results.
/// Set during post-retrieval supersession-aware scoring (RF-2).
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SupersessionStatus {
    /// This grain is the current version (not superseded, or is itself a superseder).
    Current,
    /// This grain has been superseded by another grain.
    Superseded,
}

/// Topic coverage level in recall results.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TopicCoverage {
    None,
    Partial,
    Full,
}

/// Per-component score breakdown for observability and debugging.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct ScoreBreakdown {
    /// BM25 rank position (1-indexed), if FTS was used.
    pub bm25_rank: Option<usize>,
    /// Vector similarity score [0, 1], if vector search was used.
    pub vector_score: Option<f64>,
    /// RRF fusion score.
    pub rrf_score: f64,
    /// Interference/contradiction penalty applied (negative value).
    pub interference_penalty: Option<f64>,
    /// Recency decay multiplier applied [0, 1].
    pub recency_decay: Option<f64>,
    /// Session affinity boost applied (multiplicative factor, e.g., 1.15).
    /// None when session affinity is not active or grain was not from dominant namespace.
    pub session_affinity: Option<f64>,
    /// Subject affinity boost applied (multiplicative factor, e.g., 1.20).
    /// None when subject affinity is not active or grain was not from dominant subject.
    pub subject_affinity: Option<f64>,
    /// Target-date proximity score [0, 1]. Set when target_date + target_date_weight are active.
    pub target_date_proximity: Option<f64>,
    /// Supersession demotion applied. Negative value when this grain was demoted
    /// because it is superseded. None when grain is current or RF-2 is inactive.
    pub supersession_demotion: Option<f64>,
    /// RF-5: Gaussian temporal decay boost applied (additive, [0, 0.15]).
    /// Set when target_date is active. Within 7 days: up to +0.15, within 30 days: up to +0.05.
    pub temporal_decay_boost: Option<f64>,
    /// Final score after all adjustments.
    pub final_score: f64,
}

/// One open fork: a `(namespace, subject, relation)` key that currently has
/// more than one live head.
///
/// A fork is created when two writers supersede — or independently add — the
/// same key and their histories are then synced. Immutability means both tips
/// survive; this is how CAL's `CONTRADICTIONS` clause finds them. Mirrors
/// `areev_store::ForkGroup`, but stays hash-hex so the facade contract does
/// not leak store types.
#[derive(Debug, Clone, PartialEq)]
pub struct ForkGroupInfo {
    pub namespace: String,
    pub subject: String,
    pub relation: String,
    /// Every live tip, provisional head first (the value recall serves).
    pub heads: Vec<String>,
}

/// A search result with score information.
#[derive(Debug, Clone)]
pub struct SearchHit {
    pub grain: DeserializedGrain,
    pub score: f64,
    pub hash: Hash,
    /// Optional per-component score breakdown. Populated when RecallParams::score_breakdown is true.
    pub score_breakdown: Option<ScoreBreakdown>,
    /// Cross-encoder rerank score (raw logit). Set when reranking was applied, else None.
    /// Higher values indicate greater relevance. Not normalized — do not compare with [0,1] ranges.
    #[cfg(feature = "rerank")]
    pub rerank_score: Option<f32>,
    /// LLM rerank position score. Set when LLM reranking was applied, else None.
    /// Value is the inverse of the position in the LLM-ranked list (1/position, 1-indexed).
    /// Higher values indicate the LLM ranked this result higher.
    #[cfg(feature = "llm-rerank")]
    pub llm_rerank_score: Option<f32>,
    /// Human-readable explanation of why this grain was recalled.
    /// Populated when RecallParams::explanation is Some(true).
    /// Template-based, no LLM required. Suitable for EU AI Act Art. 86.
    pub explanation: Option<String>,
    /// Scoped memory: depth of the scope this grain came from (0=root).
    /// `None` when scope_path is not used.
    pub scope_depth: Option<u8>,
    /// Scoped memory: which namespace this grain came from.
    /// `None` when scope_path is not used.
    pub source_namespace: Option<String>,
    /// Human-readable relative time label (e.g., "2 weeks ago", "3 hours ago").
    /// Populated when RecallParams::annotate_relative_time is Some(true).
    pub relative_time: Option<String>,
    /// Conflict status: Current (preferred) or Outdated (non-preferred).
    /// Set when conflict_resolution is active.
    pub conflict_status: Option<ConflictStatus>,
    /// Supersession status: Current or Superseded.
    /// Set during post-retrieval supersession-aware scoring (RF-2).
    /// None when supersession scoring has not annotated this grain.
    pub supersession_status: Option<SupersessionStatus>,
    /// Hash of the grain that superseded this one, if any.
    /// Populated during RF-2 supersession scoring. Used by rendering
    /// to determine if the superseder is in the same result set.
    pub superseded_by_hash: Option<Hash>,
    /// Origin of this result in the recall pipeline (Primary, Expansion, Census).
    /// Populated by session-census backfill (FR-005 RQ-2) and query expansion (FR-A008).
    pub recall_source: Option<RecallSource>,
}

// ---------------------------------------------------------------------------
// H1-3: Session Bootstrap types
// ---------------------------------------------------------------------------

/// Compiled session context returned by `bootstrap_session()`.
///
/// Contains the latest state, active goals, and recent actions for a session —
/// everything an agent harness needs to compile working context for the next turn.
#[derive(Debug, Clone)]
pub struct SessionBootstrap {
    /// The session ID.
    pub session_id: String,
    /// The most recent State grain, if any.
    pub state: Option<DeserializedGrain>,
    /// Active Goal grains, sorted by priority (Critical first).
    pub active_goals: Vec<DeserializedGrain>,
    /// Recent Tool grains, sorted by created_at descending.
    pub recent_tools: Vec<DeserializedGrain>,
}

// ---------------------------------------------------------------------------
// H1-2: Tool Chain Query types
// ---------------------------------------------------------------------------

/// Aggregated statistics for Tool grains in a session.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ToolSummary {
    /// Total number of Tool grains.
    pub total_calls: usize,
    /// Number of successful actions (is_error != true).
    pub successful: usize,
    /// Number of failed actions (is_error == true).
    pub failed: usize,
    /// Sum of duration_ms across all actions.
    pub total_duration_ms: u64,
    /// Per-tool breakdown of calls, failures, and duration.
    pub by_tool: HashMap<String, ToolStats>,
    /// Top error patterns: (error_message, count), sorted descending by count.
    pub error_patterns: Vec<(String, usize)>,
}

/// Per-tool statistics within an ToolSummary.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ToolStats {
    /// Number of calls to this tool.
    pub calls: usize,
    /// Number of failures from this tool.
    pub failures: usize,
    /// Sum of duration_ms for this tool.
    pub total_duration_ms: u64,
}

// ---------------------------------------------------------------------------
// H2: Goal Orchestration types
// ---------------------------------------------------------------------------


// ---------------------------------------------------------------------------
// H3: Multi-Agent + Intelligence types
// ---------------------------------------------------------------------------

/// Result of consolidating grains in a session.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ConsolidationResult {
    /// Session that was consolidated.
    pub session_id: String,
    /// Groups of similar grains found.
    pub groups: Vec<ConsolidationGroupInfo>,
    /// Total grains analyzed.
    pub total_analyzed: usize,
    /// Similarity threshold used.
    pub threshold: f64,
}

/// Info about a single consolidation group (serializable).
#[derive(Debug, Clone, serde::Serialize)]
pub struct ConsolidationGroupInfo {
    /// Hash of the canonical (best) grain.
    pub canonical_hash: Hash,
    /// Hashes of duplicate/near-duplicate grains.
    pub duplicate_hashes: Vec<Hash>,
    /// Average similarity score.
    pub similarity: f64,
    /// Total confirmation count (canonical + duplicates).
    pub confirmation_count: usize,
}

/// Compiled context for an agent turn, respecting a token budget.
#[derive(Debug, Clone, serde::Serialize)]
pub struct CompiledContext {
    /// Session ID.
    pub session_id: String,
    /// The latest state snapshot, if any.
    pub state: Option<DeserializedGrain>,
    /// Active goals included in the context.
    pub goals: Vec<DeserializedGrain>,
    /// Recent tools included.
    pub tools: Vec<DeserializedGrain>,
    /// Relevant facts included.
    pub facts: Vec<DeserializedGrain>,
    /// Estimated token count of the compiled context.
    pub estimated_tokens: usize,
    /// Token budget that was requested.
    pub token_budget: usize,
    /// Whether the context was truncated to fit the budget.
    pub truncated: bool,
}

// ---------------------------------------------------------------------------
// FR-A006: Multi-hop recall_chain types
// ---------------------------------------------------------------------------

/// Result of a multi-hop recall chain (FR-A006).
///
/// Contains the decomposed sub-queries, per-sub-query results, and the merged
/// deduplicated hit list. Each sub-query uses the caller's RecallParams as a base,
/// overriding only the query text.
#[derive(Debug, Clone)]
pub struct RecallChainResult {
    /// Sub-queries derived from the original question.
    pub sub_queries: Vec<String>,
    /// Per-sub-query results (parallel with `sub_queries`).
    pub sub_results: Vec<Vec<SearchHit>>,
    /// Merged and deduplicated hits across all sub-queries, sorted by score descending.
    pub merged_hits: Vec<SearchHit>,
    /// Language model reasoning about the decomposition (empty when fast-path bypasses the model).
    pub reasoning: String,
}

/// An engine event broadcast to watchers.
#[derive(Debug, Clone, serde::Serialize)]
pub enum EngineEvent {
    /// A grain was added.
    Added { hash: Hash, grain_type: GrainType },
    /// A grain was forgotten.
    Forgotten { hash: Hash },
    /// A grain was superseded.
    Superseded { old_hash: Hash, new_hash: Hash },
    /// Memories were extracted from a source grain via the add-intelligence pipeline.
    MemoriesExtracted {
        source_hash: Hash,
        memory_hashes: Vec<Hash>,
    },
}

// ---------------------------------------------------------------------------
// Add-Intelligence types
// ---------------------------------------------------------------------------

/// Per-call options for `add_with_options()`. All fields are optional — `None`
/// inherits database-level defaults set on `AreevOptions`.
#[derive(Debug, Default, Clone)]
pub struct AddOptions {
    /// Extract temporal references from content → auto-populate `valid_from`.
    /// Default: inherits from AreevOptions (false).
    pub extract_event_date: Option<bool>,

    /// Auto-detect updates/extends relationships with existing grains.
    /// Default: inherits from AreevOptions (false).
    pub auto_relate: Option<bool>,

    /// Force immediate commit (bypass write batch buffer).
    /// Default: false (use batch buffering if configured).
    pub sync: Option<bool>,
}

/// Result of an `add_with_options()` call that may include memory extraction metadata.
///
/// The `hash` field is always present (the source grain is always preserved).
/// When `extract_memories` was requested, `extracted_count` and `extraction_warnings`
/// report the outcome of the extraction pipeline.
#[derive(Debug, Clone)]
pub struct AddResult {
    /// Content-address hash of the added grain.
    pub hash: areev_core::error::Hash,
    /// Number of Fact grains extracted from the source content.
    /// Zero when extraction was not requested or when extraction failed.
    pub extracted_count: usize,
    /// Warnings from the extraction pipeline (e.g., LLM provider errors).
    /// Non-empty when extraction was requested but failed or partially failed.
    pub extraction_warnings: Vec<String>,
    /// Extraction marker status. `Some(Pending)` when async extraction was queued
    /// (extraction_markers_enabled=true). `None` when markers are not enabled.
    pub marker_status: Option<ExtractionMarkerStatus>,
}

impl AddResult {
    /// Create a result for a plain add (no extraction).
    pub fn plain(hash: areev_core::error::Hash) -> Self {
        Self {
            hash,
            extracted_count: 0,
            extraction_warnings: vec![],
            marker_status: None,
        }
    }

    /// Return the hash, consuming this result.
    pub fn into_hash(self) -> areev_core::error::Hash {
        self.hash
    }
}

impl From<AddResult> for areev_core::error::Hash {
    fn from(r: AddResult) -> Self {
        r.hash
    }
}

/// Which timestamp field to filter by when `time_start`/`time_end` are set.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
#[derive(Default)]
pub enum TemporalField {
    /// Filter by `created_at` (when the grain was stored). This is the default.
    #[default]
    CreatedAt,
    /// Filter by `valid_from` (when the described event occurred).
    EventDate,
    /// Filter by both — union of created_at and event_date matches.
    Both,
}

/// Status of the engine's write buffer.
#[derive(Debug, Clone, serde::Serialize)]
pub struct WriteStatus {
    /// Number of grains buffered but not yet committed to storage.
    pub pending_writes: usize,
}

// ---------------------------------------------------------------------------
// Diversity reranking types
// ---------------------------------------------------------------------------

/// Configuration for post-retrieval diversity reranking.
///
/// Applied after base scoring to reduce near-duplicate results.
/// Requires `vector` feature — grains must have stored embeddings.
/// Silently skipped if embeddings are unavailable.
#[derive(Debug, Clone)]
pub struct DiversityConfig {
    pub method: DiversityMethod,
}

/// Diversity algorithm selection.
#[derive(Debug, Clone)]
pub enum DiversityMethod {
    /// Maximal Marginal Relevance.
    /// lambda=1.0 = pure relevance order, lambda=0.0 = maximum diversity.
    Mmr { lambda: f32 },
    /// Cosine similarity threshold: skip a result if it's too similar to any already-selected result.
    Threshold(f32),
}

impl DiversityConfig {
    pub fn mmr() -> Self {
        Self {
            method: DiversityMethod::Mmr { lambda: 0.5 },
        }
    }
    pub fn mmr_with_lambda(lambda: f32) -> Self {
        Self {
            method: DiversityMethod::Mmr {
                lambda: lambda.clamp(0.0, 1.0),
            },
        }
    }
    pub fn threshold(t: f32) -> Self {
        Self {
            method: DiversityMethod::Threshold(t.clamp(0.0, 1.0)),
        }
    }
}

// ---------------------------------------------------------------------------
// WI-EXHAUST: Exhaustive entity-class recall types
// ---------------------------------------------------------------------------

/// Configuration for exhaustive entity-class recall (WI-EXHAUST).
///
/// When set on RecallParams, enables iterative entity expansion:
/// the engine performs an initial broad recall, extracts entity mentions
/// from results, generates synonym variants, and recalls with each
/// variant until no new grains are discovered or max_rounds is reached.
///
/// Designed for aggregation/counting queries ("how many X", "total Y")
/// where standard top-K retrieval misses items spread across sessions
/// with varied vocabulary.
#[derive(Debug, Clone)]
pub struct ExhaustiveConfig {
    /// Maximum expansion rounds after the initial recall.
    /// Each round extracts new entities from the previous round's results
    /// and queries for their synonyms.
    /// Range: 1-5. Default: 3.
    pub max_rounds: u8,

    /// Multiplier for candidate_limit on the initial recall pass.
    /// The initial pass uses max(limit * candidate_multiplier, 2000)
    /// to cast a wide net before entity extraction.
    /// Range: 5-50. Default: 10.
    pub candidate_multiplier: u16,

    /// Whether to use entity param (FR-A007 hexastore lookup) for
    /// expansion queries. When true, expansion queries use the entity
    /// field for efficient hexastore union. When false, expansion
    /// queries use BM25 text search only.
    /// Default: true.
    pub use_entity_lookup: bool,

    /// RF-1 Fix 3: Minimum similarity score for expansion grains.
    /// Expansion hits scoring below this threshold are discarded before
    /// merging into the result set. Prevents low-relevance grains from
    /// polluting context.
    /// Range: 0.0-1.0. Default: 0.60.
    pub min_score: f64,

    /// RF-1 Fix 5: Maximum new grains added per expansion round.
    /// Caps the number of additional grains each round can contribute,
    /// preventing unbounded context growth.
    /// Range: 1-100. Default: 20.
    pub max_grains_per_round: u16,
}

impl Default for ExhaustiveConfig {
    fn default() -> Self {
        Self {
            max_rounds: 3,
            candidate_multiplier: 10,
            use_entity_lookup: true,
            min_score: 0.60,
            max_grains_per_round: 20,
        }
    }
}

impl ExhaustiveConfig {
    /// Validate and clamp fields to safe ranges.
    pub fn validate(&mut self) {
        self.max_rounds = self.max_rounds.clamp(1, 5);
        self.candidate_multiplier = self.candidate_multiplier.clamp(5, 50);
        self.min_score = self.min_score.clamp(0.0, 1.0);
        self.max_grains_per_round = self.max_grains_per_round.clamp(1, 100);
    }
}

/// Configuration for session-census retrieval (RF-3).
///
/// When set on RecallParams, after initial recall the engine identifies
/// sessions (namespaces) not represented in results and runs a targeted
/// follow-up query into each, recovering grains that embedding proximity
/// alone would miss.
///
/// Designed for cross-session queries ("across all conversations, what X
/// did I mention?") where embedding search clusters into 1-2 sessions.
#[derive(Debug, Clone)]
pub struct SessionCensusConfig {
    /// Minimum grains to retrieve per unrepresented session.
    /// Each follow-up query uses this as its limit.
    /// Range: 1-10. Default: 2.
    pub min_per_session: u8,

    /// Minimum score threshold for census grains.
    /// Grains below this score are dropped even if the session is
    /// unrepresented. Prevents low-relevance noise from diluting results.
    /// Range: [0.0, 1.0]. Default: 0.35.
    pub min_score: f64,

    /// Maximum number of additional follow-up queries (one per
    /// unrepresented session). Caps the worst-case cost when a user
    /// has hundreds of sessions.
    /// Range: 1-50. Default: 10.
    pub max_additional_queries: u8,
}

impl Default for SessionCensusConfig {
    fn default() -> Self {
        Self {
            min_per_session: 2,
            min_score: 0.35,
            max_additional_queries: 10,
        }
    }
}

impl SessionCensusConfig {
    /// Validate and clamp fields to safe ranges.
    pub fn validate(&mut self) {
        self.min_per_session = self.min_per_session.clamp(1, 10);
        self.min_score = self.min_score.clamp(0.0, 1.0);
        self.max_additional_queries = self.max_additional_queries.clamp(1, 50);
    }
}

// ---------------------------------------------------------------------------
// Grain type diversity floor (context assembly)
// ---------------------------------------------------------------------------

/// Configuration for grain type diversity in context assembly.
///
/// Ensures a minimum number of grains per represented grain type are included
/// in the formatted context, preventing high-scoring types from crowding out
/// less-frequent but valuable types (e.g., Goals, Workflows).
///
/// Configured on `FormatPolicy` — does not affect engine recall, only
/// budget allocation during context rendering.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct GrainTypeDiversityConfig {
    /// Minimum grains to include per represented grain type.
    /// Range: 1–10. Default: 1.
    pub min_per_type: u8,
    /// Maximum fraction of the token budget that diversity reservations may
    /// consume. Range: [0.05, 0.50]. Default: 0.30.
    pub max_reservation_pct: f32,
}

impl Default for GrainTypeDiversityConfig {
    fn default() -> Self {
        Self {
            min_per_type: 1,
            max_reservation_pct: 0.30,
        }
    }
}

impl GrainTypeDiversityConfig {
    /// Validate and clamp fields to safe ranges.
    pub fn validate(&mut self) {
        self.min_per_type = self.min_per_type.clamp(1, 10);
        self.max_reservation_pct = self.max_reservation_pct.clamp(0.05, 0.50);
    }
}

/// Metadata about session-census execution, returned in RecallResult.
///
/// Provides observability into the census process: how many sessions
/// exist, how many were already represented, how many were queried,
/// and how many grains each census query contributed.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
#[cfg_attr(feature = "http", derive(utoipa::ToSchema))]
pub struct SessionCensusMetadata {
    /// Total sessions (namespaces) discovered for this user.
    pub total_sessions: usize,

    /// Sessions already represented in the initial recall results.
    pub represented_sessions: usize,

    /// Sessions queried by census (min of unrepresented, max_additional_queries).
    pub census_queries_issued: usize,

    /// Total new grains added from census queries (after dedup + min_score filter).
    pub grains_added: usize,

    /// Per-session census stats.
    pub session_stats: Vec<CensusSessionStat>,
}

/// Per-session statistics for session-census retrieval.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[cfg_attr(feature = "http", derive(utoipa::ToSchema))]
pub struct CensusSessionStat {
    /// The namespace (session) that was queried.
    pub namespace: String,

    /// Number of grains returned by the follow-up query (before dedup).
    pub grains_found: usize,

    /// Number of grains that passed min_score and dedup to be merged.
    pub grains_merged: usize,

    /// Top score among the merged grains (for observability).
    pub top_score: f64,
}

/// Metadata about exhaustive recall execution, returned in RecallResult.
///
/// Provides observability into the expansion process: how many rounds
/// ran, which entities were discovered, and how many unique grains each
/// round contributed. Useful for debugging, benchmarking, and provenance.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
#[cfg_attr(feature = "http", derive(utoipa::ToSchema))]
pub struct ExhaustiveMetadata {
    /// Number of expansion rounds executed (0 = only initial pass).
    pub rounds_executed: u8,

    /// Unique entity mentions (subjects) discovered across all rounds.
    pub entities_found: Vec<String>,

    /// Number of unique grain hashes after the initial recall pass.
    pub initial_unique_count: usize,

    /// Number of unique grain hashes after all expansion rounds.
    pub final_unique_count: usize,

    /// Per-round stats.
    pub round_stats: Vec<ExhaustiveRoundStat>,

    /// True when expansion terminated because a round added 0 new hashes
    /// (convergence). False when terminated by max_rounds limit.
    pub converged: bool,

    /// RF-1: Number of expansion grains dropped by min_score gate.
    #[serde(default)]
    pub expansion_grains_filtered: usize,

    /// RF-1: Number of expansion grains dropped by per-round budget cap.
    #[serde(default)]
    pub expansion_grains_budget_capped: usize,

    /// RF-1: The score cap applied to expansion grains
    /// (original_top10_min - 0.05), or None if fewer than 1 initial hit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expansion_score_cap: Option<f64>,
}

/// Per-round statistics for exhaustive recall.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[cfg_attr(feature = "http", derive(utoipa::ToSchema))]
pub struct ExhaustiveRoundStat {
    /// 1-indexed round number.
    pub round: u8,
    /// Number of entity variants queried in this round.
    pub queries_issued: usize,
    /// Number of new unique grain hashes discovered in this round
    /// (not seen in any previous round).
    pub new_hashes: usize,
    /// Number of grains filtered by min_score in this round.
    #[serde(default)]
    pub filtered_by_min_score: usize,
    /// Number of grains dropped by budget cap in this round.
    #[serde(default)]
    pub budget_capped: usize,
}

/// Postgres-Everything Phase 2 — hybrid recall tunables.
///
/// Controls the BM25 + vector + RRF fusion path in
/// `src/engine/recall_pg.rs` (active when the `pg-store` feature is on and
/// the engine holds a `PgStore` handle). Per ADR-004 §"Hybrid params per
/// tier", each field has a default that callers can override, clamped by
/// the `[hybrid]` block in `config/tiers.toml` so a Free-tier caller
/// cannot blow the cell with a top-10000 query.
///
/// ## Defaults
///
/// - `rrf_k = 60` — standard RRF constant (Cormack/Clarke/Buettcher 2009).
/// - `bm25_topk = 200` — per ADR-004 §"RRF CTE" parameter `$3`.
/// - `vec_topk = 200` — same.
/// - `final_topk = 20` — feeds into the SQL `LIMIT $6`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HybridParams {
    /// RRF reciprocal-rank constant. `1 / (k + rk)` per row.
    pub rrf_k: u32,
    /// Top-N candidates from the BM25 leg before fusion.
    pub bm25_topk: u32,
    /// Top-N candidates from the vector leg before fusion.
    pub vec_topk: u32,
    /// Final result count after fusion + scoring.
    pub final_topk: u32,
}

impl Default for HybridParams {
    fn default() -> Self {
        Self {
            rrf_k: 60,
            bm25_topk: 200,
            vec_topk: 200,
            final_topk: 20,
        }
    }
}

impl HybridParams {
    /// Clamp every field against an inclusive `[min, max]` range. Used by
    /// the per-tier policy gate (see `crate::engine::recall_pg::clamp`).
    pub fn clamped(self, range: &HybridParamsRange) -> Self {
        Self {
            rrf_k: self.rrf_k.clamp(range.rrf_k_min, range.rrf_k_max),
            bm25_topk: self
                .bm25_topk
                .clamp(range.bm25_topk_min, range.bm25_topk_max),
            vec_topk: self.vec_topk.clamp(range.vec_topk_min, range.vec_topk_max),
            final_topk: self
                .final_topk
                .clamp(range.final_topk_min, range.final_topk_max),
        }
    }
}

/// Per-tier upper/lower bounds for `HybridParams`. Loaded from the
/// `[tiers.<id>.hybrid]` block in `config/tiers.toml`. Engineering
/// defaults below mirror the Free tier; per-tier overrides are applied at
/// runtime via `crate::tier_limits` (Phase 2 introduces the loader).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HybridParamsRange {
    pub rrf_k_min: u32,
    pub rrf_k_max: u32,
    pub bm25_topk_min: u32,
    pub bm25_topk_max: u32,
    pub vec_topk_min: u32,
    pub vec_topk_max: u32,
    pub final_topk_min: u32,
    pub final_topk_max: u32,
}

impl Default for HybridParamsRange {
    /// Free-tier-equivalent defaults. Cosmos and Scale tiers override the
    /// upper bounds in `config/tiers.toml`.
    fn default() -> Self {
        Self {
            rrf_k_min: 10,
            rrf_k_max: 200,
            bm25_topk_min: 10,
            bm25_topk_max: 500,
            vec_topk_min: 10,
            vec_topk_max: 500,
            final_topk_min: 1,
            final_topk_max: 100,
        }
    }
}

/// Parameters for recall queries.
#[derive(Debug, Default, Clone)]
pub struct RecallParams {
    /// Free-text query string (used for BM25 search when FTS is enabled).
    pub query: Option<String>,
    /// Structured: match subject.
    pub subject: Option<String>,
    /// Structured: match relation/predicate.
    pub relation: Option<String>,
    /// Structured: match object.
    pub object: Option<String>,
    /// Multi-value subject filter (CAL `IN` operator). Union of hexastore lookups.
    pub subject_in: Option<Vec<String>>,
    /// Multi-value relation filter (CAL `IN` operator). Union of hexastore lookups.
    pub relation_in: Option<Vec<String>>,
    /// Multi-value object filter (CAL `IN` operator). Union of hexastore lookups.
    pub object_in: Option<Vec<String>>,
    /// Filter by namespace — an exact name or a `"org.*"` prefix scope
    /// (parent + descendants; see `areev_core::ns::NsScope`).
    pub namespace: Option<String>,
    /// Scoped memory: scope path for hierarchical recall (e.g., "acme/prod/bot1").
    /// When set, expands to ancestor namespaces and applies precedence rules.
    pub scope_path: Option<String>,
    /// The `namespace IN (…)` set. Consumed by `AreevFacade::recall` as the
    /// scope-term list (each element may itself be a `"org.*"` pattern);
    /// takes precedence over `namespace` when set. A pinned session
    /// (`namespace_override`) clears it.
    pub(crate) namespaces: Option<Vec<String>>,
    /// Scoped memory: include sibling namespaces at the same depth.
    pub include_siblings: Option<bool>,
    /// Filter by user_id.
    pub user_id: Option<String>,
    /// Filter by grain type.
    pub grain_type: Option<GrainType>,
    /// Time range start (epoch milliseconds).
    pub time_start: Option<i64>,
    /// Time range end (epoch milliseconds).
    pub time_end: Option<i64>,
    /// Minimum confidence threshold.
    pub confidence_threshold: Option<f64>,
    /// Maximum results to return.
    pub limit: Option<usize>,
    /// Skip superseded grains (default: true).
    pub exclude_superseded: Option<bool>,
    /// Natural-language temporal expression (e.g., "last 7 days", "yesterday").
    /// Resolved to time_start/time_end via the temporal parser.
    pub temporal_expr: Option<String>,
    /// Filter: all these tags must be present on the grain.
    pub tags: Option<Vec<String>>,
    /// Filter: none of these tags may be present on the grain.
    pub exclude_tags: Option<Vec<String>>,
    /// Minimum importance threshold.
    pub importance_threshold: Option<f64>,
    /// Include contradicted facts (default: true — no filtering).
    pub include_contradicted: Option<bool>,
    /// Apply interference detection and penalize contradicted facts.
    pub detect_contradictions: Option<bool>,
    /// Pre-computed embedding vector for KNN semantic search (requires `vector` feature).
    pub embedding: Option<Vec<f32>>,
    /// First-stage candidate retrieval count for two-stage retrieval (default: None).
    /// When set, retrieves this many candidates from vector/BM25 before scoring+filtering down to `limit`.
    /// Recommended: 50–200. If None, defaults to max(limit * 5, 50) when reranking is active.
    pub candidate_limit: Option<usize>,
    /// Minimum score threshold. Results with final score below this are dropped.
    /// Applied after all scoring. Range: [0.0, 1.0].
    pub min_score: Option<f64>,
    /// Include per-component score breakdown in SearchHit results (default: false).
    pub score_breakdown: Option<bool>,
    /// Post-retrieval diversity configuration (MMR or threshold deduplication).
    /// Silently skipped if embeddings unavailable.
    pub diversity: Option<DiversityConfig>,
    /// Per-query HNSW ef_search override. Overrides the AreevOptions default for this query.
    /// Range: 8–1000.
    /// Cross-encoder reranking configuration.
    /// When set, first stage retrieves `rerank.candidate_k` candidates,
    /// then cross-encoder scores each (query, grain_text) pair.
    /// Requires `rerank` feature and a model path on AreevOptions.
    /// Silently skipped if no model is loaded or no query string is provided.
    pub rerank: Option<RerankConfig>,
    /// LLM listwise reranking configuration (requires `llm-rerank` feature + AreevOptions::llm_reranker()).
    /// Applied after cross-encoder reranking (if any). Silently skipped if no LLM reranker is loaded.
    #[cfg(feature = "llm-rerank")]
    pub llm_rerank: Option<crate::store_types::LlmRerankConfig>,
    /// Callback invoked once per successful LLM call made during recall with
    /// the token counts + USD cost. HTTP/gRPC/MCP/A2A handlers build this
    /// hook from the request-scoped `MeteringContext` (billing pipeline);
    /// library-mode callers (PyO3, CLI) leave it `None` — no metering.
    #[cfg(feature = "llm-rerank")]
    pub inference_hook: Option<InferenceHook>,
    /// Whether to record a provenance record for this recall() invocation.
    /// Default: None (inherits from Areev instance — true when audit trail is active).
    /// Set to Some(false) to explicitly disable provenance for high-frequency queries.
    pub record_provenance: Option<bool>,
    /// Whether to include a human-readable explanation in each SearchHit.
    /// Default: None (false). When Some(true), SearchHit::explanation is populated.
    pub explanation: Option<bool>,
    /// Only return grains whose `subject` field contains this substring (case-insensitive).
    pub subject_contains: Option<String>,
    /// Only return grains whose `object` field contains this substring (case-insensitive).
    pub object_contains: Option<String>,
    /// Per-query recency weight [0.0–1.0] for temporal freshness scoring.
    /// When set, applies: final = (1 - w) * relevance + w * freshness
    /// where freshness = 1.0 / (1.0 + age_hours).
    /// Takes priority over builder-level recency_decay when set.
    /// State facts (temporal_type == "state") are exempt from recency weighting.
    pub recency_weight: Option<f64>,
    /// When true, remove older grains when multiple grains share the same
    /// (subject, relation) but have different objects, keeping only the most recent.
    /// Independent of `detect_contradictions` (which applies interference penalties
    /// but does not remove grains).
    pub conflict_resolution: Option<bool>,
    /// Entity-centric recall (FR-A007): find all grains where this entity appears
    /// as either subject OR object. Uses hexastore union of SPO(entity, _, _) and
    /// OSP(entity, _, _) for efficient lookup. Takes priority over individual
    /// subject/relation/object filters when set.
    pub entity: Option<String>,
    /// FR-A008 Phase 1: Enable rule-based query expansion.
    /// When Some(true), expands the query using stemming + synonym substitution,
    /// runs multiple BM25 searches, and fuses results via RRF.
    /// Default: None (false). No LLM required.
    pub query_expansion: Option<bool>,
    /// FR-A008 Phase 2: Enable HyDE (Hypothetical Document Embeddings).
    /// When Some(true) and a HydeFn is configured, generates a hypothetical answer
    /// document from the query, embeds that instead of the raw query for vector search.
    /// Requires both `embedding_fn` and `hyde_fn` on AreevOptions. Default: None (false).
    pub hyde: Option<bool>,
    /// FR-005: Minimum grains per namespace in recall results.
    /// When set, after initial retrieval the engine groups results by namespace and
    /// backfills underrepresented namespaces with additional grains from those namespaces.
    /// Ensures cross-session coverage for counting/aggregation queries.
    /// Default: None (disabled, no behavior change).
    pub min_per_namespace: Option<usize>,
    /// FR-005: Maximum unique namespaces in recall results.
    /// When set, only the top N namespaces by average grain score are kept.
    /// Grains from dropped namespaces are excluded with NamespaceCapped provenance.
    /// Default: None (unlimited).
    pub max_namespaces: Option<usize>,
    /// Apply string-similarity deduplication to recall results.
    /// Groups grains with same (subject, relation) and similar objects, keeping only
    /// the canonical (highest confidence, most recent) from each group.
    /// Default: None (false).
    pub deduplicate: Option<bool>,
    /// Similarity threshold for deduplication (0.0–1.0). Default: 0.85.
    /// Only used when `deduplicate` is Some(true).
    pub deduplicate_threshold: Option<f64>,
    /// Which timestamp to filter by when `time_start`/`time_end` are set.
    /// Default: None (CreatedAt — backward compatible).
    pub temporal_field: Option<TemporalField>,
    /// Include source grains for results that have `derived_from` links.
    /// Default: false. When true, response includes a sources array alongside results.
    pub include_sources: Option<bool>,
    /// Annotate each SearchHit with a human-readable relative time label
    /// (e.g., "2 weeks ago", "just now"). Default: None (false).
    pub annotate_relative_time: Option<bool>,
    /// Reference point for relative time annotations (epoch milliseconds).
    /// Default: None (uses current wall-clock time).
    pub reference_date: Option<i64>,
    /// Session affinity boost factor [0.0–1.0]. When set, identifies the dominant
    /// namespace among top-K results and boosts all grains from that namespace
    /// by this factor (e.g., 0.15 = 15% boost). Default: None (disabled).
    pub session_affinity_boost: Option<f64>,
    /// Subject affinity boost factor [0.0–1.0]. When set, identifies the dominant
    /// subject among top-K results and boosts all grains from that subject
    /// by this factor (e.g., 0.20 = 20% boost). Default: None (disabled).
    /// Mirrors session_affinity but operates on the subject field instead of namespace.
    pub subject_affinity_boost: Option<f64>,
    /// Entity-graph multi-hop: follow entity links from top-K results.
    /// Value is the number of hops (1-3). None = disabled.
    /// After first-pass recall, takes the entities named by the top results'
    /// subject/object fields, anchors a fresh recall on each, and adds what
    /// comes back to the candidate pool — repeating for each hop. Expansion
    /// happens before post-filtering and LIMIT, so hops compete for the slots
    /// the caller asked for rather than extending past them, and a direct match
    /// always outranks something reached by association.
    ///
    /// Each entity is followed in both directions — as a subject (what it
    /// points at) and as an object (what points at it). The reverse leg reads
    /// the OSP index, so it covers the relations the file declares as entity
    /// relations, matching every other reverse traversal in the engine.
    pub multi_hop: Option<u8>,
    /// Target date (epoch ms) for proximity-based scoring.
    /// When set with target_date_weight, boosts grains near this specific date.
    pub target_date: Option<i64>,
    /// Weight [0.0–1.0] for target_date proximity scoring.
    /// final = (1 - w) * score + w * proximity where proximity = 1/(1 + hours_distance).
    pub target_date_weight: Option<f64>,
    /// Conflict similarity threshold [0.0–1.0]. When set, conflict_resolution only
    /// treats grains as conflicting when their objects have normalized Levenshtein
    /// similarity >= this threshold.
    pub conflict_similarity_threshold: Option<f64>,
    /// When true, group result grains by subject and return entity_count + entities
    /// in the RecallResult metadata.
    pub count_entities: Option<bool>,
    /// ADR-023: Enable rule-based query decomposition (generates 2-4 sub-queries per strategy).
    /// Analyzes the ABOUT text, detects temporal/attribute/recency/multi-facet patterns,
    /// generates sub-queries targeting different retrieval pathways, and merges via RRF.
    /// Default: None (false). No LLM required.
    pub query_decompose: Option<bool>,
    /// When true, RF-2 supersession scoring skips demotion — both old and new
    /// values are kept at natural scores. Used for aggregation/counting queries
    /// where superseded data points are still relevant.
    pub aggregation_intent: Option<bool>,
    /// When true, preference queries are enriched with co-occurring grains
    /// from sessions that contributed preference-related facts.
    /// Auto-detected from query text when None.
    pub preference_enrichment: Option<bool>,
    /// Internal recursion guard — prevents infinite decomposition when sub-queries
    /// call recall() recursively. Not exposed via any API or serialization.
    /// Default: 0. Set to 1 on sub-queries.
    pub(crate) _decompose_depth: u8,
    /// Exhaustive entity-class recall configuration (WI-EXHAUST).
    /// When Some, enables iterative entity expansion for aggregation queries.
    /// Opt-in due to performance cost (2-5x standard recall).
    /// Default: None (disabled).
    pub exhaustive: Option<ExhaustiveConfig>,
    /// RF-3: Session-census retrieval configuration.
    /// When Some, after initial recall the engine queries unrepresented
    /// sessions to recover cross-session grains.
    /// Opt-in due to additional query cost (1-10 sub-queries).
    /// Default: None (disabled).
    pub session_census: Option<SessionCensusConfig>,
    /// Issue #536: caller-supplied run identifier used to gate the
    /// `distinct_run_pull_count` increment in pull-stats telemetry.
    /// When `Some`, the run_id is HMAC-blinded before being mixed into a
    /// per-grain Bloom filter — the raw run_id is never stored.
    /// `None`/empty → only `pull_count` + `last_pulled_at` are touched.
    pub run_id: Option<String>,
    /// Postgres-Everything Phase 2 — hybrid recall tunables (RRF k,
    /// per-leg top-N, final top-N). Defaults are applied when `None`.
    /// Tier-clamped via `HybridParams::clamped` against the per-tier
    /// `[hybrid]` block in `config/tiers.toml`.
    ///
    /// Only used when the `pg-store` feature is active AND the cell has
    /// a `PgStore` plumbed in; falls back to the legacy Fjall+Tantivy
    /// path otherwise.
    pub hybrid: Option<HybridParams>,
    /// Skill discovery post-filter (OMS 1.4, D6.2): minimum proficiency.
    /// Maps to the grain's `confidence` (proficiency aliases confidence, D3).
    /// Applied as a post-filter after retrieval.
    pub min_proficiency: Option<f64>,
    /// Skill discovery post-filter: keep only transferable skills.
    pub skill_transferable: Option<bool>,
    /// Skill discovery post-filter: exact `domain` match.
    pub skill_domain: Option<String>,
    /// Skill discovery post-filter: exact `holder_did` match.
    pub holder_did: Option<String>,
}

impl RecallParams {
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether the PRIMARY recall tail must run the payload-dependent
    /// post-filter pass (`Areev::payload_postfilters_match`).
    /// A cheap `Option` scan (ADR-014 §2): `true` when any payload-only /
    /// tail-only filter is set. Column-backed filters (`user_id`, `importance`,
    /// `created_at` range, exact `namespace`, `grain_type`) are NOT listed —
    /// they are enforced in SQL, so they alone do not require the tail.
    ///
    /// The GDPR Art. 18 restriction membership check is gated SEPARATELY on the
    /// policy flag (`policy_requires_restriction_check`) at the call site, so it
    /// is intentionally not part of this predicate — a restriction-free memory
    /// issues zero extra queries even when this returns `true`, and a restricted
    /// memory runs the tail regardless of this predicate.
    pub fn needs_payload_postfilter(&self) -> bool {
        self.confidence_threshold.is_some()
            || self.namespaces.is_some()
            || self.tags.is_some()
            || self.exclude_tags.is_some()
            || self.subject_contains.is_some()
            || self.object_contains.is_some()
            || self.include_contradicted == Some(false)
            || self.entity.is_some()
            || self.subject.is_some()
            || self.relation.is_some()
            || self.object.is_some()
            || self.subject_in.is_some()
            || self.relation_in.is_some()
            || self.object_in.is_some()
            || self.min_proficiency.is_some()
            || self.skill_domain.is_some()
            || self.skill_transferable.is_some()
            || self.holder_did.is_some()
            // EventDate / Both time range reads payload `valid_from` (CreatedAt
            // range is SQL-pushed). Any time bound with a non-CreatedAt field
            // needs the tail.
            || ((self.time_start.is_some() || self.time_end.is_some())
                && !matches!(
                    self.temporal_field.unwrap_or_default(),
                    TemporalField::CreatedAt
                ))
    }

    pub fn query(mut self, q: &str) -> Self {
        self.query = Some(q.to_string());
        self
    }

    pub fn subject(mut self, s: &str) -> Self {
        self.subject = Some(s.to_string());
        self
    }

    pub fn relation(mut self, r: &str) -> Self {
        self.relation = Some(r.to_string());
        self
    }

    pub fn object(mut self, o: &str) -> Self {
        self.object = Some(o.to_string());
        self
    }

    pub fn namespace(mut self, ns: &str) -> Self {
        self.namespace = Some(ns.to_string());
        self
    }

    pub fn user_id(mut self, uid: &str) -> Self {
        self.user_id = Some(uid.to_string());
        self
    }

    /// Set a scope path for hierarchical recall.
    pub fn scope_path(mut self, path: &str) -> Self {
        self.scope_path = Some(path.to_string());
        self
    }

    /// Include sibling namespaces at the same depth (requires scope_path).
    pub fn include_siblings(mut self, include: bool) -> Self {
        self.include_siblings = Some(include);
        self
    }

    pub fn grain_type(mut self, gt: GrainType) -> Self {
        self.grain_type = Some(gt);
        self
    }

    pub fn time_range(mut self, start: i64, end: i64) -> Self {
        self.time_start = Some(start);
        self.time_end = Some(end);
        self
    }

    pub fn confidence_threshold(mut self, threshold: f64) -> Self {
        self.confidence_threshold = Some(threshold);
        self
    }

    pub fn limit(mut self, n: usize) -> Self {
        self.limit = Some(n);
        self
    }

    /// Issue #536: caller-supplied run identifier (HMAC-blinded by the
    /// engine before mixing into per-grain telemetry Bloom).
    pub fn run_id(mut self, run_id: &str) -> Self {
        self.run_id = Some(run_id.to_string());
        self
    }

    pub fn temporal_expr(mut self, expr: &str) -> Self {
        self.temporal_expr = Some(expr.to_string());
        self
    }

    pub fn tags(mut self, tags: Vec<String>) -> Self {
        self.tags = Some(tags);
        self
    }

    pub fn exclude_tags(mut self, tags: Vec<String>) -> Self {
        self.exclude_tags = Some(tags);
        self
    }

    pub fn importance_threshold(mut self, threshold: f64) -> Self {
        self.importance_threshold = Some(threshold);
        self
    }

    /// Skill discovery (OMS 1.4): minimum proficiency post-filter.
    pub fn min_proficiency(mut self, threshold: f64) -> Self {
        self.min_proficiency = Some(threshold);
        self
    }

    /// Skill discovery: keep only transferable skills.
    pub fn skill_transferable(mut self, transferable: bool) -> Self {
        self.skill_transferable = Some(transferable);
        self
    }

    /// Skill discovery: exact domain match.
    pub fn skill_domain(mut self, domain: &str) -> Self {
        self.skill_domain = Some(domain.to_string());
        self
    }

    /// Skill discovery: exact holder_did match.
    pub fn holder_did(mut self, holder_did: &str) -> Self {
        self.holder_did = Some(holder_did.to_string());
        self
    }

    pub fn include_contradicted(mut self, include: bool) -> Self {
        self.include_contradicted = Some(include);
        self
    }

    pub fn embedding(mut self, vec: Vec<f32>) -> Self {
        self.embedding = Some(vec);
        self
    }

    pub fn detect_contradictions(mut self, detect: bool) -> Self {
        self.detect_contradictions = Some(detect);
        self
    }

    pub fn candidate_limit(mut self, k: usize) -> Self {
        self.candidate_limit = Some(k);
        self
    }

    pub fn min_score(mut self, s: f64) -> Self {
        self.min_score = Some(s.clamp(0.0, 1.0));
        self
    }

    pub fn with_score_breakdown(mut self) -> Self {
        self.score_breakdown = Some(true);
        self
    }

    pub fn diversity(mut self, d: DiversityConfig) -> Self {
        self.diversity = Some(d);
        self
    }

    /// Enable cross-encoder reranking with the given configuration.
    pub fn rerank(mut self, cfg: RerankConfig) -> Self {
        self.rerank = Some(cfg);
        self
    }

    /// Enable LLM listwise reranking with the given configuration.
    /// Applied after cross-encoder reranking (if any).
    /// Requires `llm-rerank` feature and `AreevOptions::llm_reranker()`.
    #[cfg(feature = "llm-rerank")]
    pub fn llm_rerank(mut self, cfg: crate::store_types::LlmRerankConfig) -> Self {
        self.llm_rerank = Some(cfg);
        self
    }

    /// Request score breakdown per result.
    pub fn score_breakdown(mut self, enabled: bool) -> Self {
        self.score_breakdown = Some(enabled);
        self
    }

    /// Request human-readable explanation per result (EU AI Act Art. 86).
    /// Requires score_breakdown to be enabled (explanations reference breakdown fields).
    pub fn explanation(mut self, enabled: bool) -> Self {
        self.explanation = Some(enabled);
        self
    }

    /// Control provenance recording for this specific query.
    /// None = auto (record when audit + provenance are available).
    /// Some(true) = force record, Some(false) = explicitly disable.
    pub fn record_provenance(mut self, enabled: bool) -> Self {
        self.record_provenance = Some(enabled);
        self
    }

    /// Include or exclude superseded grains from results.
    /// Default is true (superseded grains are excluded).
    pub fn exclude_superseded(mut self, exclude: bool) -> Self {
        self.exclude_superseded = Some(exclude);
        self
    }

    /// Set per-query recency weight [0.0–1.0].
    /// 0.0 = pure relevance, 1.0 = pure recency.
    pub fn recency_weight(mut self, w: f64) -> Self {
        self.recency_weight = Some(w.clamp(0.0, 1.0));
        self
    }

    /// Enable conflict resolution: keep only the newest grain when multiple
    /// grains share the same (subject, relation) but have different objects.
    pub fn conflict_resolution(mut self, enabled: bool) -> Self {
        self.conflict_resolution = Some(enabled);
        self
    }

    /// Entity-centric recall: find all grains mentioning this entity as subject or object.
    pub fn entity(mut self, e: &str) -> Self {
        self.entity = Some(e.to_string());
        self
    }

    /// Enable rule-based query expansion (FR-A008 Phase 1).
    pub fn query_expansion(mut self, enabled: bool) -> Self {
        self.query_expansion = Some(enabled);
        self
    }

    /// Enable HyDE (FR-A008 Phase 2).
    pub fn hyde(mut self, enabled: bool) -> Self {
        self.hyde = Some(enabled);
        self
    }

    /// FR-005: Set minimum grains per namespace for cross-session coverage.
    pub fn min_per_namespace(mut self, n: usize) -> Self {
        self.min_per_namespace = Some(n);
        self
    }

    /// FR-005: Set maximum unique namespaces in results.
    pub fn max_namespaces(mut self, n: usize) -> Self {
        self.max_namespaces = Some(n);
        self
    }

    pub fn deduplicate(mut self, enabled: bool) -> Self {
        self.deduplicate = Some(enabled);
        self
    }

    pub fn deduplicate_threshold(mut self, threshold: f64) -> Self {
        self.deduplicate_threshold = Some(threshold.clamp(0.0, 1.0));
        self
    }

    pub fn subject_contains(mut self, pattern: impl Into<String>) -> Self {
        self.subject_contains = Some(pattern.into());
        self
    }

    pub fn object_contains(mut self, pattern: impl Into<String>) -> Self {
        self.object_contains = Some(pattern.into());
        self
    }

    /// Set which timestamp field to filter by for temporal queries.
    pub fn temporal_field(mut self, tf: TemporalField) -> Self {
        self.temporal_field = Some(tf);
        self
    }

    /// Include source grains (via `derived_from` links) in recall results.
    pub fn include_sources(mut self, enabled: bool) -> Self {
        self.include_sources = Some(enabled);
        self
    }

    /// Enable human-readable relative time annotations on each SearchHit.
    pub fn annotate_relative_time(mut self, enabled: bool) -> Self {
        self.annotate_relative_time = Some(enabled);
        self
    }

    /// Set the reference point for relative time annotations (epoch milliseconds).
    /// If not set, uses current wall-clock time.
    pub fn reference_date(mut self, reference_ms: i64) -> Self {
        self.reference_date = Some(reference_ms);
        self
    }

    /// Set session affinity boost factor [0.0–1.0].
    /// Boosts grains from the dominant namespace among top-K results.
    pub fn session_affinity_boost(mut self, boost: f64) -> Self {
        self.session_affinity_boost = Some(boost.clamp(0.0, 1.0));
        self
    }

    /// Set subject affinity boost factor [0.0–1.0].
    /// Boosts grains from the dominant subject among top-K results.
    pub fn subject_affinity_boost(mut self, boost: f64) -> Self {
        self.subject_affinity_boost = Some(boost.clamp(0.0, 1.0));
        self
    }

    /// Enable entity-graph multi-hop retrieval (1-3 hops).
    pub fn multi_hop(mut self, hops: u8) -> Self {
        self.multi_hop = Some(hops.clamp(1, 3));
        self
    }

    /// Set target date (epoch ms) for proximity scoring.
    pub fn target_date(mut self, ts: i64) -> Self {
        self.target_date = Some(ts);
        self
    }

    /// Set target_date proximity weight [0.0–1.0].
    pub fn target_date_weight(mut self, w: f64) -> Self {
        self.target_date_weight = Some(w.clamp(0.0, 1.0));
        self
    }

    /// Set conflict similarity threshold [0.0–1.0].
    pub fn conflict_similarity_threshold(mut self, t: f64) -> Self {
        self.conflict_similarity_threshold = Some(t.clamp(0.0, 1.0));
        self
    }

    /// Enable entity counting in recall results.
    pub fn count_entities(mut self, enabled: bool) -> Self {
        self.count_entities = Some(enabled);
        self
    }

    /// ADR-023: Enable rule-based query decomposition.
    pub fn query_decompose(mut self, v: bool) -> Self {
        self.query_decompose = Some(v);
        self
    }

    /// RQ-3: Enable aggregation intent — superseded grains keep natural scores.
    pub fn aggregation_intent(mut self, v: bool) -> Self {
        self.aggregation_intent = Some(v);
        self
    }

    /// Enable preference enrichment: enrich results with co-occurring session grains.
    pub fn preference_enrichment(mut self, v: bool) -> Self {
        self.preference_enrichment = Some(v);
        self
    }

    /// Multi-value subject filter (CAL `WHERE subject IN (...)`).
    /// Returns grains where subject matches ANY of the provided values.
    pub fn subject_in(mut self, values: Vec<String>) -> Self {
        self.subject_in = Some(values);
        self
    }

    /// Multi-value relation filter (CAL `WHERE relation IN (...)`).
    /// Returns grains where relation matches ANY of the provided values.
    pub fn relation_in(mut self, values: Vec<String>) -> Self {
        self.relation_in = Some(values);
        self
    }

    /// Multi-value object filter (CAL `WHERE object IN (...)`).
    /// Returns grains where object matches ANY of the provided values.
    pub fn object_in(mut self, values: Vec<String>) -> Self {
        self.object_in = Some(values);
        self
    }

    /// Enable exhaustive entity-class recall with the given configuration.
    /// For default settings, use `ExhaustiveConfig::default()`.
    pub fn exhaustive(mut self, config: ExhaustiveConfig) -> Self {
        self.exhaustive = Some(config);
        self
    }

    /// Enable session-census retrieval with the given configuration.
    /// For default settings, use `SessionCensusConfig::default()`.
    pub fn session_census(mut self, config: SessionCensusConfig) -> Self {
        self.session_census = Some(config);
        self
    }
}

/// P2 fix: Sharded LRU cache to reduce contention under concurrent recall.
/// 16 shards, each with its own Mutex, so concurrent readers on different
/// shards don't block each other.
#[allow(dead_code)] // dormant P2 cache: ported but not yet wired into the recall path
const GRAIN_CACHE_SHARDS: usize = 16;

#[allow(dead_code)] // dormant P2 cache: ported but not yet wired into the recall path
pub(crate) struct GrainCache {
    shards: Vec<Mutex<lru::LruCache<Hash, Arc<DeserializedGrain>>>>,
}

#[allow(dead_code)] // dormant P2 cache: ported but not yet wired into the recall path
impl GrainCache {
    pub(crate) fn new(total_capacity: usize) -> Self {
        let per_shard = (total_capacity / GRAIN_CACHE_SHARDS).max(1);
        let shards = (0..GRAIN_CACHE_SHARDS)
            .map(|_| {
                Mutex::new(lru::LruCache::new(
                    std::num::NonZeroUsize::new(per_shard).unwrap(),
                ))
            })
            .collect();
        GrainCache { shards }
    }

    fn shard_index(hash: &Hash) -> usize {
        // Use first byte of hash for shard selection (uniform distribution).
        (hash.as_bytes()[0] as usize) % GRAIN_CACHE_SHARDS
    }

    pub(crate) fn get(&self, hash: &Hash) -> Option<Arc<DeserializedGrain>> {
        let idx = Self::shard_index(hash);
        self.shards[idx].lock().get(hash).cloned()
    }

    pub(crate) fn put(&self, hash: Hash, grain: Arc<DeserializedGrain>) {
        let idx = Self::shard_index(&hash);
        self.shards[idx].lock().put(hash, grain);
    }

    pub(crate) fn pop(&self, hash: &Hash) {
        let idx = Self::shard_index(hash);
        self.shards[idx].lock().pop(hash);
    }
}

// ---- ported companions (areev-cal store_types) ----

/// One version in a supersession chain (HISTORY statement).
#[derive(Debug, Clone)]
pub struct VersionEntry {
    /// Content-address hash of this version.
    pub hash: Hash,
    /// The object/value of this fact version.
    pub object: String,
    /// When this version was created (epoch milliseconds).
    pub created_at: i64,
    /// Confidence score of this version.
    pub confidence: f64,
    /// Hash of the grain that superseded this one, if any.
    pub superseded_by: Option<Hash>,
}


/// Cross-encoder rerank knobs (stub config — reranking lands in M4).
#[derive(Debug, Clone)]
pub struct RerankConfig { pub candidate_k: usize, pub return_n: Option<usize>, pub min_rerank_score: Option<f32>, pub model: Option<String> }
impl Default for RerankConfig { fn default() -> Self { RerankConfig { candidate_k: 30, return_n: None, min_rerank_score: None, model: None } } }

/// LLM listwise rerank knobs (stub config — reranking lands in M4).
#[derive(Debug, Clone)]
pub struct LlmRerankConfig { pub candidate_k: usize, pub return_n: Option<usize>, pub user_id: Option<String>, pub model: Option<String> }
impl Default for LlmRerankConfig { fn default() -> Self { LlmRerankConfig { candidate_k: 20, return_n: None, user_id: None, model: None } } }

/// Erasure receipt (crypto-erasure is file-level in Areev).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ErasureProof { pub user_id: String, pub count: u64, pub key_fingerprint: String, pub timestamp: i64, pub user_record_deleted: bool }

/// `REPORT SUBJECT` result — the read-only DSAR selection: the matched
/// identity strings (exact + partition keys) and the matched grains as
/// `{hash, type, fields}` JSON, full history included.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct SubjectReportResult {
    pub identity_names: Vec<String>,
    pub grains: Vec<serde_json::Value>,
}

/// Token/cost usage of one LLM rerank call (stub — reranking lands in M4).
#[derive(Debug, Clone)]
pub struct InferenceUsage {
    pub input_tokens: u32,
    pub output_tokens: u32,
    pub provider_cost_usd: f64,
    pub model: String,
    pub provider: &'static str,
}

/// Extraction pipeline marker status (consolidation flag companion).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum ExtractionMarkerStatus {
    Pending = 0x00,
    InProgress = 0x01,
    Complete = 0x02,
    Failed = 0x03,
}
