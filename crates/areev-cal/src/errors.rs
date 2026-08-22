//! CAL error model — ~30 error codes for Phase 1 (Core conformance).
//!
//! Error codes follow the CAL specification section 22:
//! - CAL-E001..CAL-E019: Parse errors
//! - CAL-E020..CAL-E022: Type errors
//! - CAL-E030..CAL-E031: Execution errors
//! - CAL-E060: Shortcut / field resolution errors
//! - CAL-E100: Version errors
//! - CAL-W001..CAL-W004: Warnings

use thiserror::Error;

// ---------------------------------------------------------------------------
// Span — source location for diagnostics
// ---------------------------------------------------------------------------

/// A byte-offset span within a CAL query string.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Span {
    /// Byte offset of the first character (inclusive).
    pub start: usize,
    /// Byte offset past the last character (exclusive).
    pub end: usize,
    /// 1-based line number.
    pub line: usize,
    /// 1-based column number (byte offset from line start).
    pub col: usize,
}

impl Span {
    /// Create a new span.
    pub fn new(start: usize, end: usize, line: usize, col: usize) -> Self {
        Self {
            start,
            end,
            line,
            col,
        }
    }

    /// A zero-width span at the start of input (used when no better location
    /// is available).
    pub fn zero() -> Self {
        Self {
            start: 0,
            end: 0,
            line: 1,
            col: 1,
        }
    }
}

impl std::fmt::Display for Span {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}", self.line, self.col)
    }
}

// ---------------------------------------------------------------------------
// CalError — the 27 Phase-1 error codes
// ---------------------------------------------------------------------------

/// All CAL query errors.
///
/// Each variant carries its CAL spec error code in the `#[error]` message so
/// that `Display` output always starts with `CAL-Exxx:`.
#[derive(Debug, Error)]
pub enum CalError {
    // ── Parse errors (CAL-E001 – CAL-E019) ──────────────────────────────
    /// CAL-E001 — Query exceeds the maximum allowed byte length.
    #[error("CAL-E001: Query exceeds maximum length ({length} bytes, max {max})")]
    QueryTooLong {
        length: usize,
        max: usize,
        span: Option<Span>,
    },

    /// CAL-E002 — The parser encountered a token it did not expect.
    #[error("CAL-E002: Unexpected token: expected {expected}, found {found}")]
    UnexpectedToken {
        expected: String,
        found: String,
        span: Option<Span>,
        suggestion: Option<String>,
    },

    /// CAL-E003 — A grain type name was used that does not match any of
    /// the 11 OMS types (singular or plural form).
    #[error("CAL-E003: Unknown grain type \"{found}\"")]
    UnknownGrainType {
        found: String,
        span: Option<Span>,
        suggestion: Option<String>,
    },

    /// CAL-E004 — A field name was used that is not a recognised common or
    /// type-specific field.
    #[error("CAL-E004: Unknown field \"{found}\"")]
    UnknownField {
        found: String,
        span: Option<Span>,
        suggestion: Option<String>,
    },

    /// CAL-E005 — A string literal was opened but never closed.
    #[error("CAL-E005: Unterminated string literal")]
    UnterminatedString { span: Option<Span> },

    /// CAL-E006 — A numeric literal could not be parsed.
    #[error("CAL-E006: Invalid number \"{found}\"")]
    InvalidNumber { found: String, span: Option<Span> },

    /// CAL-E007 — Parenthesised or sub-query nesting exceeds the allowed
    /// depth.
    #[error("CAL-E007: Nesting too deep ({depth} levels, max {max})")]
    NestingTooDeep {
        depth: usize,
        max: usize,
        span: Option<Span>,
    },

    /// CAL-E008 — A `$parameter` was referenced but never bound.
    #[error("CAL-E008: Unbound parameter \"${name}\"")]
    UnboundParameter { name: String, span: Option<Span> },

    /// CAL-E009 — The same parameter name was bound more than once.
    #[error("CAL-E009: Duplicate parameter \"${name}\"")]
    DuplicateParameter { name: String, span: Option<Span> },

    /// CAL-E010 — A `LIMIT` value exceeds the server-configured maximum.
    #[error("CAL-E010: Limit {value} exceeds maximum allowed ({max})")]
    LimitExceeded {
        value: u64,
        max: u64,
        span: Option<Span>,
    },

    /// CAL-E011 — An `IN (...)` set contains more elements than permitted.
    #[error("CAL-E011: IN set too large ({count} elements, max {max})")]
    InSetTooLarge {
        count: usize,
        max: usize,
        span: Option<Span>,
    },

    /// CAL-E012 — Too many pipeline stages (`|`) in a single query.
    #[error("CAL-E012: Too many pipeline stages ({count}, max {max})")]
    TooManyPipelineStages {
        count: usize,
        max: usize,
        span: Option<Span>,
    },

    /// CAL-E013 — A set operation (UNION / INTERSECT / EXCEPT) has more
    /// operands than allowed.
    #[error("CAL-E013: Too many set operands ({count}, max {max})")]
    TooManySetOperands {
        count: usize,
        max: usize,
        span: Option<Span>,
    },

    /// CAL-E014 — The query string is empty or contains only whitespace.
    #[error("CAL-E014: Empty query")]
    EmptyQuery { span: Option<Span> },

    /// CAL-E015 — A hash literal is not valid hex or has the wrong length.
    #[error("CAL-E015: Invalid hash \"{found}\"")]
    InvalidHash { found: String, span: Option<Span> },

    /// CAL-E016 — A reason string (e.g. `BECAUSE "..."`) exceeds the
    /// maximum length.  Tier 1 statement, but the parser validates it.
    #[error("CAL-E016: Reason too long ({length} chars, max {max})")]
    ReasonTooLong {
        length: usize,
        max: usize,
        span: Option<Span>,
    },

    /// CAL-E017 — An `EVOLVE ... SET` clause references a field that does
    /// not exist on the target grain type.  Tier 1 statement.
    #[error("CAL-E017: Unknown EVOLVE field \"{found}\"")]
    UnknownEvolveField {
        found: String,
        span: Option<Span>,
        suggestion: Option<String>,
    },

    /// CAL-E018 — A write statement that requires `BECAUSE` was issued
    /// without one.  Tier 1 statement.
    #[error("CAL-E018: Missing BECAUSE reason clause")]
    MissingReason { span: Option<Span> },

    /// CAL-E019 — A `SUPERSEDE` or `EVOLVE` is missing its `SET` clause.
    /// Tier 1 statement.
    #[error("CAL-E019: Missing SET clause")]
    MissingSetClause { span: Option<Span> },

    // ── Type errors (CAL-E020 – CAL-E022) ───────────────────────────────
    /// CAL-E020 — A comparison or operation was attempted between
    /// incompatible types (e.g. string vs number).
    #[error("CAL-E020: Incompatible types: {left} vs {right}")]
    IncompatibleTypes {
        left: String,
        right: String,
        span: Option<Span>,
        suggestion: Option<String>,
    },

    /// CAL-E021 — A pipeline stage received input of a type it cannot
    /// process.
    #[error(
        "CAL-E021: Pipeline type mismatch: stage \"{stage}\" expected {expected}, got {found}"
    )]
    PipelineTypeMismatch {
        stage: String,
        expected: String,
        found: String,
        span: Option<Span>,
    },

    /// CAL-E022 — An extractor (SUBJECTS / OBJECTS / HASHES) was used on
    /// a non-Fact grain type.
    #[error("CAL-E022: Extractor \"{extractor}\" requires facts, got {found}")]
    ExtractorRequiresFacts {
        extractor: String,
        found: String,
        span: Option<Span>,
    },

    // ── Execution errors (CAL-E030 – CAL-E031) ─────────────────────────
    /// CAL-E030 — The query exceeded its resource budget (e.g. result-set
    /// size or intermediate working-set cap).
    #[error("CAL-E030: Budget exceeded: {detail}")]
    BudgetExceeded { detail: String, span: Option<Span> },

    /// CAL-E031 — The query exceeded the per-query timeout.
    #[error("CAL-E031: Query timeout after {elapsed_ms}ms (limit {limit_ms}ms)")]
    QueryTimeout {
        elapsed_ms: u64,
        limit_ms: u64,
        span: Option<Span>,
    },

    /// CAL-E092 — The store rejected the query as invalid input during
    /// execution (a validation failure, e.g. a malformed or under-specified
    /// filter). Distinct from `BudgetExceeded` (CAL-E030, a resource overrun):
    /// nothing was over budget, the request itself was not valid. Carries the
    /// store's `VAL-Ennn` detail so the underlying reason stays visible.
    #[error("CAL-E092: Invalid query: {detail}")]
    InvalidQuery { detail: String, span: Option<Span> },

    /// CAL-E090 — A cryptographic operation failed while executing a CAL
    /// statement (typically AES-GCM decrypt of an encrypted grain blob).
    /// This is **not** a budget overrun — it indicates a key-material
    /// mismatch, envelope corruption, or missing key manager. Common
    /// operator causes: master key changed between write and read
    /// (Vault key rotation, different unseal), missing `blob_owner`
    /// mapping, per-user DEK destroyed via crypto-erasure.
    #[error("CAL-E090: Crypto error during query execution: {detail}")]
    CryptoError { detail: String, span: Option<Span> },

    /// CAL-E091 — A grain referenced by content address (sha256 hash) was
    /// not found in the store. Distinct from `InvalidHash` (CAL-E015,
    /// malformed literal) and `BudgetExceeded` (CAL-E030, resource overrun).
    #[error("CAL-E091: Grain not found for hash \"{hash}\"")]
    HashNotFound { hash: String, span: Option<Span> },

    // ── Shortcut / field resolution errors (CAL-E060) ──────────────────
    /// CAL-E060 — A shorthand field name (e.g. `subject`) is ambiguous
    /// because the query targets a grain type that does not have that
    /// field, or the field only exists on a different type.
    #[error("CAL-E060: Field \"{field}\" is not available on grain type \"{grain_type}\"")]
    FieldNotOnGrainType {
        field: String,
        grain_type: String,
        span: Option<Span>,
        suggestion: Option<String>,
    },

    /// CAL-E061 — An engine-level filter field (`query`, `time`, `entity`,
    /// `contradicted`, `scope`, `tags`, …) was used where it cannot be
    /// honoured: under `NOT`/`OR`, or with a comparator its push-down does
    /// not support. These fields narrow the scan and have no per-grain
    /// value, so the executor refuses rather than silently widening (#91).
    #[error("CAL-E061: Engine-level field \"{field}\" cannot be used {context}; it narrows the scan and has no per-grain value to filter on")]
    EngineFieldNotFilterable {
        field: String,
        /// Where it appeared, e.g. `"under NOT/OR"` or `"with comparator !="`.
        context: String,
        span: Option<Span>,
    },

    // ── Phase 2: ASSEMBLE errors (CAL-E032 – CAL-E035) ─────────────────
    /// CAL-E032 — ASSEMBLE FROM has more than 8 named sources.
    #[error("CAL-E032: Too many ASSEMBLE sources ({count}, max {max})")]
    AssembleTooManySources {
        count: usize,
        max: usize,
        span: Option<Span>,
    },

    /// CAL-E033 — ASSEMBLE BUDGET exceeds the maximum allowed value.
    #[error("CAL-E033: ASSEMBLE budget exceeded ({value} {unit}, max {max})")]
    AssembleBudgetExceeded {
        value: u64,
        max: u64,
        unit: String,
        span: Option<Span>,
    },

    /// CAL-E034 — Two ASSEMBLE sources share the same label.
    #[error("CAL-E034: Duplicate ASSEMBLE source label \"{label}\"")]
    AssembleDuplicateLabel { label: String, span: Option<Span> },

    /// CAL-E122 — The `PIN`ned sources alone do not fit the `BUDGET`.
    ///
    /// A pin is a promise of full, verbatim disclosure, so there is no
    /// degraded answer to fall back to: summarising the section would break
    /// the guarantee the pin exists to make, and dropping it silently is
    /// worse. Failing loudly is the only honest outcome — the budget or the
    /// pinned text has to change.
    #[error(
        "CAL-E122: pinned ASSEMBLE source(s) [{}] need {required} tokens but BUDGET is {budget} — a PIN is never summarised or dropped, so raise the budget or shorten the pinned text",
        labels.join(", ")
    )]
    AssemblePinnedBudgetExceeded {
        labels: Vec<String>,
        required: u32,
        budget: u32,
        span: Option<Span>,
    },

    /// CAL-E035 — PRIORITY references a label not in the FROM clause.
    #[error("CAL-E035: PRIORITY references unknown source label \"{label}\"")]
    AssemblePriorityMismatch { label: String, span: Option<Span> },

    // ── Phase 2: LET binding errors (CAL-E036 – CAL-E038) ──────────────
    /// CAL-E036 — More than 5 LET bindings in a single query.
    #[error("CAL-E036: Too many LET bindings ({count}, max {max})")]
    TooManyLetBindings {
        count: usize,
        max: usize,
        span: Option<Span>,
    },

    /// CAL-E037 — A LET binding references itself or creates a cycle.
    #[error("CAL-E037: Circular reference in LET binding \"${name}\"")]
    LetCircularReference { name: String, span: Option<Span> },

    /// CAL-E038 — LET chain depth exceeds the maximum (3).
    #[error("CAL-E038: LET chain depth exceeded ({depth}, max {max})")]
    LetDepthExceeded {
        depth: usize,
        max: usize,
        span: Option<Span>,
    },

    // ── Phase 2: COALESCE errors (CAL-E039) ─────────────────────────────
    /// CAL-E039 — COALESCE has more than 5 branches.
    #[error("CAL-E039: Too many COALESCE branches ({count}, max {max})")]
    CoalesceTooManyBranches {
        count: usize,
        max: usize,
        span: Option<Span>,
    },

    // ── Phase 2: Timeout error (CAL-E071) ───────────────────────────────
    /// CAL-E071 — ASSEMBLE execution exceeded the timeout.
    #[error("CAL-E071: ASSEMBLE timeout after {elapsed_ms}ms (limit {limit_ms}ms)")]
    AssembleTimeout {
        elapsed_ms: u64,
        limit_ms: u64,
        span: Option<Span>,
    },

    // ── Template limits and inheritance, OMS CAL §10.7–§10.8
    //    (CAL-E117 – CAL-E119) ─────────────────────────────────────────
    /// CAL-E117 — Template conditional nesting exceeds the §10.8 limit.
    #[error("CAL-E117: Template nesting too deep (max {max} levels)")]
    TemplateNestingTooDeep { max: usize, span: Option<Span> },

    /// CAL-E118 — Namespace is at the §10.8 template limit.
    #[error("CAL-E118: Too many templates ({count}, max {max})")]
    TooManyTemplates {
        count: usize,
        max: usize,
        span: Option<Span>,
    },

    /// CAL-E119 — The `data` preset outputs structural JSON, not
    /// template-driven text, so §10.7 forbids extending it.
    #[error("CAL-E119: Template \"{name}\" cannot extend the 'data' preset")]
    CannotExtendData { name: String, span: Option<Span> },

    // ── JSON wire format error (CAL-E120) ─────────────────────────────
    /// CAL-E120 — JSON wire format (`application/json+cal`) parse failure.
    #[error("CAL-E120: Invalid JSON+CAL: {detail}")]
    InvalidJsonCal { detail: String, span: Option<Span> },

    // ── Authorization (CAL-E121) ───────────────────────────────────────
    /// CAL-E121 — The session's grants don't cover this statement. Carries
    /// the store's `AUT-Ennn` detail verbatim: the refused verb, namespace,
    /// and principal are the caller's own session facts and exactly what a
    /// granting admin needs to fix it.
    #[error("CAL-E121: Not authorized: {detail}")]
    NotAuthorized { detail: String, span: Option<Span> },

    /// CAL-E070 — Query input contains invalid UTF-8 byte sequences or
    /// bidi-override characters. HTTP body extractors typically reject
    /// non-UTF-8 upstream; this variant covers in-band rejection
    /// (bidi runs, mixed-script confusables) surfaced by the lexer.
    #[error("CAL-E070: Invalid UTF-8 or unsafe character in query: {detail}")]
    InvalidUtf8 { detail: String, span: Option<Span> },

    // ── ACCUMULATE errors (CAL-E080 – CAL-E082) ────────────────────────
    /// CAL-E080 — ACCUMULATE requires at least one ADD operation.
    #[error("CAL-E080: ACCUMULATE requires at least one ADD operation")]
    MissingAccumulateOps { span: Option<Span> },

    /// CAL-E081 — ADD targets a non-numeric field (detected at execution time).
    #[error(
        "CAL-E081: ADD delta applied to non-numeric field \"{field}\" (current value: {current})"
    )]
    AccumulateNonNumericField {
        field: String,
        current: String,
        span: Option<Span>,
    },

    /// CAL-E082 — ACCUMULATE WHERE matched no grain (tip not found).
    #[error("CAL-E082: No grain found for ACCUMULATE target (subject=\"{subject}\", relation=\"{relation}\")")]
    AccumulateTipNotFound {
        subject: String,
        relation: String,
        span: Option<Span>,
    },

    /// CAL-E083 — ACCUMULATE retry budget exhausted under sustained
    /// contention (CU-86d2wr4n4). With per-key serialization in place
    /// this should never fire under normal contention; defensive belt
    /// against unforeseen retry pathologies. HTTP status: 409 Conflict.
    /// Body echoes only `subject` / `relation` (security C4) — inner
    /// cause is logged separately with `request_id`.
    #[error(
        "CAL-E083: ACCUMULATE retry budget exhausted (subject=\"{subject}\", relation=\"{relation}\")"
    )]
    AccumulateRetryExhausted {
        subject: String,
        relation: String,
        span: Option<Span>,
    },

    /// CAL-E084 — ACCUMULATE failed for an internal reason that is
    /// neither user validation nor contention. HTTP status: 500.
    /// Inner-error text MUST NOT be in the wire body (security C3) —
    /// surfaced only through `tracing::error!` with request_id.
    #[error("CAL-E084: ACCUMULATE internal failure")]
    AccumulateInternal { span: Option<Span> },

    /// CAL-E085 — ACCUMULATE rejected at admission control (CU-86d2wr4n4
    /// v2.1). Either the per-key inflight cap or the global retry-permit
    /// semaphore was saturated. HTTP status: 429 Too Many Requests with
    /// a fixed `Retry-After: 1` header (no queue-depth signaling —
    /// security review condition). Body echoes only `subject` /
    /// `relation` (security C4) — same sanitization as CAL-E083.
    #[error(
        "CAL-E085: ACCUMULATE backpressure: per-key inflight cap exceeded (subject=\"{subject}\", relation=\"{relation}\")"
    )]
    AccumulateBackpressureRejected {
        subject: String,
        relation: String,
        span: Option<Span>,
    },

    // ── Phase 4: Template errors (CAL-E040 – CAL-E050) ────────────────
    /// CAL-E040 — Template source exceeds maximum allowed size.
    #[error("CAL-E040: Template too large ({size} bytes, max {max})")]
    TemplateTooLarge {
        size: usize,
        max: usize,
        span: Option<Span>,
    },

    /// CAL-E041 — Template contains nested {{#each}} blocks.
    #[error("CAL-E041: Nested {{{{#each}}}} blocks are not allowed")]
    TemplateNestedEach { span: Option<Span> },

    /// CAL-E042 — Template references an unknown variable.
    #[error("CAL-E042: Unknown template variable \"{name}\"")]
    TemplateUnknownVariable {
        name: String,
        span: Option<Span>,
        suggestion: Option<String>,
    },

    /// CAL-E043 — Template uses an unknown filter.
    #[error("CAL-E043: Unknown template filter \"{name}\"")]
    TemplateUnknownFilter { name: String, span: Option<Span> },

    /// CAL-E115 — Template name is invalid (must start with a letter,
    /// max 64 chars, only letters/digits/spaces/hyphens/underscores).
    #[error("CAL-E115: Invalid template name \"{name}\"")]
    TemplateInvalidName { name: String, span: Option<Span> },

    /// CAL-E044 — Tier 1 (Evolve) statement was issued while Tier 1 is
    /// disabled on the server. The parser accepts the statement but the
    /// executor refuses to run it because the capability is gated off.
    #[error("CAL-E044: Tier 1 (Evolve) is not enabled: {statement}")]
    Tier1NotEnabled {
        statement: String,
        span: Option<Span>,
    },

    /// CAL-E045 — Referenced template does not exist in the registry.
    #[error("CAL-E045: Template \"{name}\" not found")]
    TemplateNotFound { name: String, span: Option<Span> },

    /// CAL-E046 — Attempted to delete or overwrite a built-in template.
    #[error("CAL-E046: Built-in template \"{name}\" cannot be modified")]
    TemplateBuiltinImmutable { name: String, span: Option<Span> },

    /// CAL-E047 — Template inheritance parent not found.
    #[error("CAL-E047: Template \"{name}\" extends unknown parent \"{parent}\"")]
    TemplateParentNotFound {
        name: String,
        parent: String,
        span: Option<Span>,
    },

    /// CAL-E048 — Template inheritance depth exceeds 1 level.
    #[error("CAL-E048: Template \"{name}\" exceeds maximum inheritance depth (1 level)")]
    TemplateInheritanceDepth { name: String, span: Option<Span> },

    /// CAL-E049 — Template syntax error (unclosed tag, malformed filter, etc.).
    #[error("CAL-E049: Template syntax error: {detail}")]
    TemplateSyntaxError { detail: String, span: Option<Span> },

    /// CAL-E050 — Rendered output exceeds maximum allowed size (F1 safety).
    #[error("CAL-E050: Rendered output too large ({size} bytes, max {max})")]
    RenderOutputTooLarge {
        size: usize,
        max: usize,
        span: Option<Span>,
    },

    // ── Phase 5: Saved query errors (CAL-E051 – CAL-E059) ──────────────
    /// CAL-E051 — Referenced saved query does not exist.
    #[error("CAL-E051: Saved query \"{name}\" not found")]
    QueryNotFound { name: String, span: Option<Span> },

    /// CAL-E052 — A saved query with this name already exists.
    #[error("CAL-E052: Saved query \"{name}\" already exists")]
    DuplicateQueryName { name: String, span: Option<Span> },

    /// CAL-E053 — Too many saved queries in this namespace.
    #[error("CAL-E053: Too many saved queries ({count}, max {max})")]
    TooManyQueries {
        count: usize,
        max: usize,
        span: Option<Span>,
    },

    /// CAL-E054 — Query body exceeds maximum allowed size.
    #[error("CAL-E054: Query body too large ({size} bytes, max {max})")]
    QueryBodyTooLarge {
        size: usize,
        max: usize,
        span: Option<Span>,
    },

    /// CAL-E055 — Too many parameters declared on a saved query.
    #[error("CAL-E055: Too many query parameters ({count}, max {max})")]
    TooManyQueryParams {
        count: usize,
        max: usize,
        span: Option<Span>,
    },

    /// CAL-E056 — A required parameter was not supplied at the RUN call site.
    #[error("CAL-E056: Missing required parameter \"${name}\" for query \"{query}\"")]
    MissingQueryParam {
        name: String,
        query: String,
        span: Option<Span>,
    },

    /// CAL-E057 — RUN found inside DEFINE QUERY body (recursion not allowed).
    #[error("CAL-E057: RUN is not allowed inside DEFINE QUERY body")]
    RecursiveQuery { span: Option<Span> },

    /// CAL-E058 — Write statement found in DEFINE QUERY body (read-tier only).
    #[error("CAL-E058: Write statement \"{stmt}\" not allowed in DEFINE QUERY body")]
    WriteInQueryBody { stmt: String, span: Option<Span> },

    /// CAL-E059 — General query body parse error.
    #[error("CAL-E059: Invalid query body: {detail}")]
    InvalidQueryBody { detail: String, span: Option<Span> },

    // ── Version errors (CAL-E100) ──────────────────────────────────────
    /// CAL-E100 — The `CAL/<version>` prefix specifies a version the
    /// server does not support.
    #[error("CAL-E100: Unsupported CAL version {version}")]
    UnsupportedVersion { version: u32, span: Option<Span> },

    // ── Multi-format errors (CAL-E110) ──────────────────────────────
    /// CAL-E110 — A multi-format list contains more formats than allowed.
    #[error("CAL-E110: Too many formats in multi-format list ({count}, max {max})")]
    TooManyFormats {
        count: usize,
        max: usize,
        span: Option<Span>,
    },

    // ── User vars errors (CAL-E111, CAL-E112) ────────────────────────
    /// CAL-E111 — Too many user variables in WITH VARS clause.
    #[error("CAL-E111: Too many user variables ({count}, max {max})")]
    TooManyUserVars {
        count: usize,
        max: usize,
        span: Option<Span>,
    },

    /// CAL-E112 — A user variable value exceeds the maximum allowed size.
    #[error("CAL-E112: User variable \"{key}\" too large ({size} bytes, max {max})")]
    UserVarTooLarge {
        key: String,
        size: usize,
        max: usize,
        span: Option<Span>,
    },

    // ── Format alias errors (CAL-E113) ──────────────────────────────
    /// CAL-E113 — Duplicate key in multi-format list (alias or canonical name collision).
    #[error("CAL-E113: Duplicate format key \"{key}\" in multi-format list")]
    DuplicateFormatKey { key: String, span: Option<Span> },

    // ── Scope enforcement (CAL-E114) ─────────────────────────────────
    /// CAL-E114 — Caller lacks the required scope for this statement type.
    #[error("CAL-E114: insufficient scope: '{statement}' requires '{required}' scope")]
    InsufficientScope { required: String, statement: String },

    // ── LLM-dependent feature (CAL-E116) ─────────────────────────────
    /// CAL-E116 — A `WITH` option that intrinsically needs an external LLM
    /// (e.g. `hyde`, `llm_rerank`). Areev is a passive, dependency-light
    /// engine and takes no LLM dependency by policy — these live in the host's
    /// agent loop. Surfaced as a clear error instead of a silent no-op.
    #[error(
        "CAL-E116: WITH {feature} needs an external LLM and is not implemented in Areev — \
         the engine takes no LLM dependency by design (these belong in your agent loop). \
         Want it built in? Open a feature request at \
         https://github.com/AreevAI/areev/issues — we'll build it if there's demand."
    )]
    LlmFeatureUnavailable { feature: String },
}

impl CalError {
    /// Return the CAL spec error code (e.g. `"CAL-E001"`).
    pub fn code(&self) -> &'static str {
        match self {
            Self::QueryTooLong { .. } => "CAL-E001",
            Self::UnexpectedToken { .. } => "CAL-E002",
            Self::UnknownGrainType { .. } => "CAL-E003",
            Self::UnknownField { .. } => "CAL-E004",
            Self::UnterminatedString { .. } => "CAL-E005",
            Self::InvalidNumber { .. } => "CAL-E006",
            Self::NestingTooDeep { .. } => "CAL-E007",
            Self::UnboundParameter { .. } => "CAL-E008",
            Self::DuplicateParameter { .. } => "CAL-E009",
            Self::LimitExceeded { .. } => "CAL-E010",
            Self::InSetTooLarge { .. } => "CAL-E011",
            Self::TooManyPipelineStages { .. } => "CAL-E012",
            Self::TooManySetOperands { .. } => "CAL-E013",
            Self::EmptyQuery { .. } => "CAL-E014",
            Self::InvalidHash { .. } => "CAL-E015",
            Self::ReasonTooLong { .. } => "CAL-E016",
            Self::UnknownEvolveField { .. } => "CAL-E017",
            Self::MissingReason { .. } => "CAL-E018",
            Self::MissingSetClause { .. } => "CAL-E019",
            Self::IncompatibleTypes { .. } => "CAL-E020",
            Self::PipelineTypeMismatch { .. } => "CAL-E021",
            Self::ExtractorRequiresFacts { .. } => "CAL-E022",
            Self::BudgetExceeded { .. } => "CAL-E030",
            Self::QueryTimeout { .. } => "CAL-E031",
            Self::CryptoError { .. } => "CAL-E090",
            Self::HashNotFound { .. } => "CAL-E091",
            Self::InvalidQuery { .. } => "CAL-E092",
            Self::FieldNotOnGrainType { .. } => "CAL-E060",
            Self::EngineFieldNotFilterable { .. } => "CAL-E061",
            Self::AssembleTooManySources { .. } => "CAL-E032",
            Self::AssembleBudgetExceeded { .. } => "CAL-E033",
            Self::AssembleDuplicateLabel { .. } => "CAL-E034",
            Self::AssemblePinnedBudgetExceeded { .. } => "CAL-E122",
            Self::AssemblePriorityMismatch { .. } => "CAL-E035",
            Self::TooManyLetBindings { .. } => "CAL-E036",
            Self::LetCircularReference { .. } => "CAL-E037",
            Self::LetDepthExceeded { .. } => "CAL-E038",
            Self::CoalesceTooManyBranches { .. } => "CAL-E039",
            Self::AssembleTimeout { .. } => "CAL-E071",
            Self::TemplateNestingTooDeep { .. } => "CAL-E117",
            Self::TooManyTemplates { .. } => "CAL-E118",
            Self::CannotExtendData { .. } => "CAL-E119",
            Self::InvalidJsonCal { .. } => "CAL-E120",
            Self::NotAuthorized { .. } => "CAL-E121",
            Self::InvalidUtf8 { .. } => "CAL-E070",
            Self::TemplateTooLarge { .. } => "CAL-E040",
            Self::TemplateNestedEach { .. } => "CAL-E041",
            Self::TemplateUnknownVariable { .. } => "CAL-E042",
            Self::TemplateUnknownFilter { .. } => "CAL-E043",
            Self::TemplateInvalidName { .. } => "CAL-E115",
            Self::Tier1NotEnabled { .. } => "CAL-E044",
            Self::TemplateNotFound { .. } => "CAL-E045",
            Self::TemplateBuiltinImmutable { .. } => "CAL-E046",
            Self::TemplateParentNotFound { .. } => "CAL-E047",
            Self::TemplateInheritanceDepth { .. } => "CAL-E048",
            Self::TemplateSyntaxError { .. } => "CAL-E049",
            Self::RenderOutputTooLarge { .. } => "CAL-E050",
            Self::UnsupportedVersion { .. } => "CAL-E100",
            Self::TooManyFormats { .. } => "CAL-E110",
            Self::TooManyUserVars { .. } => "CAL-E111",
            Self::UserVarTooLarge { .. } => "CAL-E112",
            Self::DuplicateFormatKey { .. } => "CAL-E113",
            Self::InsufficientScope { .. } => "CAL-E114",
            Self::LlmFeatureUnavailable { .. } => "CAL-E116",
            Self::MissingAccumulateOps { .. } => "CAL-E080",
            Self::AccumulateNonNumericField { .. } => "CAL-E081",
            Self::AccumulateTipNotFound { .. } => "CAL-E082",
            Self::AccumulateRetryExhausted { .. } => "CAL-E083",
            Self::AccumulateInternal { .. } => "CAL-E084",
            Self::AccumulateBackpressureRejected { .. } => "CAL-E085",
            Self::QueryNotFound { .. } => "CAL-E051",
            Self::DuplicateQueryName { .. } => "CAL-E052",
            Self::TooManyQueries { .. } => "CAL-E053",
            Self::QueryBodyTooLarge { .. } => "CAL-E054",
            Self::TooManyQueryParams { .. } => "CAL-E055",
            Self::MissingQueryParam { .. } => "CAL-E056",
            Self::RecursiveQuery { .. } => "CAL-E057",
            Self::WriteInQueryBody { .. } => "CAL-E058",
            Self::InvalidQueryBody { .. } => "CAL-E059",
        }
    }

    /// Return the source span, if one was recorded.
    pub fn span(&self) -> Option<Span> {
        match self {
            Self::QueryTooLong { span, .. }
            | Self::UnexpectedToken { span, .. }
            | Self::UnknownGrainType { span, .. }
            | Self::UnknownField { span, .. }
            | Self::UnterminatedString { span, .. }
            | Self::InvalidNumber { span, .. }
            | Self::NestingTooDeep { span, .. }
            | Self::UnboundParameter { span, .. }
            | Self::DuplicateParameter { span, .. }
            | Self::LimitExceeded { span, .. }
            | Self::InSetTooLarge { span, .. }
            | Self::TooManyPipelineStages { span, .. }
            | Self::TooManySetOperands { span, .. }
            | Self::EmptyQuery { span, .. }
            | Self::InvalidHash { span, .. }
            | Self::ReasonTooLong { span, .. }
            | Self::UnknownEvolveField { span, .. }
            | Self::MissingReason { span, .. }
            | Self::MissingSetClause { span, .. }
            | Self::IncompatibleTypes { span, .. }
            | Self::PipelineTypeMismatch { span, .. }
            | Self::ExtractorRequiresFacts { span, .. }
            | Self::BudgetExceeded { span, .. }
            | Self::QueryTimeout { span, .. }
            | Self::InvalidQuery { span, .. }
            | Self::CryptoError { span, .. }
            | Self::FieldNotOnGrainType { span, .. }
            | Self::EngineFieldNotFilterable { span, .. }
            | Self::AssemblePinnedBudgetExceeded { span, .. }
            | Self::AssembleTooManySources { span, .. }
            | Self::AssembleBudgetExceeded { span, .. }
            | Self::AssembleDuplicateLabel { span, .. }
            | Self::AssemblePriorityMismatch { span, .. }
            | Self::TooManyLetBindings { span, .. }
            | Self::LetCircularReference { span, .. }
            | Self::LetDepthExceeded { span, .. }
            | Self::CoalesceTooManyBranches { span, .. }
            | Self::AssembleTimeout { span, .. }
            | Self::InvalidJsonCal { span, .. }
            | Self::NotAuthorized { span, .. }
            | Self::TemplateTooLarge { span, .. }
            | Self::TemplateNestedEach { span, .. }
            | Self::TemplateUnknownVariable { span, .. }
            | Self::TemplateUnknownFilter { span, .. }
            | Self::TemplateInvalidName { span, .. }
            | Self::TemplateNotFound { span, .. }
            | Self::TemplateBuiltinImmutable { span, .. }
            | Self::TemplateParentNotFound { span, .. }
            | Self::TemplateInheritanceDepth { span, .. }
            | Self::TemplateSyntaxError { span, .. }
            | Self::RenderOutputTooLarge { span, .. }
            | Self::UnsupportedVersion { span, .. }
            | Self::TooManyFormats { span, .. }
            | Self::TooManyUserVars { span, .. }
            | Self::UserVarTooLarge { span, .. }
            | Self::DuplicateFormatKey { span, .. }
            | Self::MissingAccumulateOps { span, .. }
            | Self::AccumulateNonNumericField { span, .. }
            | Self::AccumulateTipNotFound { span, .. }
            | Self::AccumulateRetryExhausted { span, .. }
            | Self::AccumulateInternal { span, .. }
            | Self::AccumulateBackpressureRejected { span, .. }
            | Self::QueryNotFound { span, .. }
            | Self::DuplicateQueryName { span, .. }
            | Self::TooManyQueries { span, .. }
            | Self::TemplateNestingTooDeep { span, .. }
            | Self::TooManyTemplates { span, .. }
            | Self::CannotExtendData { span, .. }
            | Self::QueryBodyTooLarge { span, .. }
            | Self::TooManyQueryParams { span, .. }
            | Self::MissingQueryParam { span, .. }
            | Self::RecursiveQuery { span, .. }
            | Self::WriteInQueryBody { span, .. }
            | Self::InvalidQueryBody { span, .. }
            | Self::HashNotFound { span, .. }
            | Self::Tier1NotEnabled { span, .. }
            | Self::InvalidUtf8 { span, .. } => *span,
            Self::InsufficientScope { .. } | Self::LlmFeatureUnavailable { .. } => None,
        }
    }

    /// Return the suggestion, if one was attached.
    pub fn suggestion(&self) -> Option<&str> {
        match self {
            Self::UnexpectedToken { suggestion, .. }
            | Self::UnknownGrainType { suggestion, .. }
            | Self::UnknownField { suggestion, .. }
            | Self::UnknownEvolveField { suggestion, .. }
            | Self::IncompatibleTypes { suggestion, .. }
            | Self::FieldNotOnGrainType { suggestion, .. }
            | Self::TemplateUnknownVariable { suggestion, .. } => suggestion.as_deref(),
            _ => None,
        }
    }

    /// Attach a human-readable suggestion to this error.
    ///
    /// Only affects variants that carry a `suggestion` field; for others
    /// the error is returned unchanged.
    pub fn with_suggestion(self, hint: &str) -> Self {
        let hint = Some(hint.to_string());
        match self {
            Self::UnexpectedToken {
                expected,
                found,
                span,
                ..
            } => Self::UnexpectedToken {
                expected,
                found,
                span,
                suggestion: hint,
            },
            Self::UnknownGrainType { found, span, .. } => Self::UnknownGrainType {
                found,
                span,
                suggestion: hint,
            },
            Self::UnknownField { found, span, .. } => Self::UnknownField {
                found,
                span,
                suggestion: hint,
            },
            Self::UnknownEvolveField { found, span, .. } => Self::UnknownEvolveField {
                found,
                span,
                suggestion: hint,
            },
            Self::IncompatibleTypes {
                left, right, span, ..
            } => Self::IncompatibleTypes {
                left,
                right,
                span,
                suggestion: hint,
            },
            Self::FieldNotOnGrainType {
                field,
                grain_type,
                span,
                ..
            } => Self::FieldNotOnGrainType {
                field,
                grain_type,
                span,
                suggestion: hint,
            },
            Self::TemplateUnknownVariable { name, span, .. } => Self::TemplateUnknownVariable {
                name,
                span,
                suggestion: hint,
            },
            other => other,
        }
    }

    /// Attach a span to this error, replacing any existing span.
    pub fn with_span(self, new_span: Span) -> Self {
        let s = Some(new_span);
        match self {
            Self::QueryTooLong { length, max, .. } => Self::QueryTooLong {
                length,
                max,
                span: s,
            },
            Self::UnexpectedToken {
                expected,
                found,
                suggestion,
                ..
            } => Self::UnexpectedToken {
                expected,
                found,
                span: s,
                suggestion,
            },
            Self::UnknownGrainType {
                found, suggestion, ..
            } => Self::UnknownGrainType {
                found,
                span: s,
                suggestion,
            },
            Self::UnknownField {
                found, suggestion, ..
            } => Self::UnknownField {
                found,
                span: s,
                suggestion,
            },
            Self::UnterminatedString { .. } => Self::UnterminatedString { span: s },
            Self::InvalidNumber { found, .. } => Self::InvalidNumber { found, span: s },
            Self::NestingTooDeep { depth, max, .. } => Self::NestingTooDeep {
                depth,
                max,
                span: s,
            },
            Self::UnboundParameter { name, .. } => Self::UnboundParameter { name, span: s },
            Self::DuplicateParameter { name, .. } => Self::DuplicateParameter { name, span: s },
            Self::LimitExceeded { value, max, .. } => Self::LimitExceeded {
                value,
                max,
                span: s,
            },
            Self::InSetTooLarge { count, max, .. } => Self::InSetTooLarge {
                count,
                max,
                span: s,
            },
            Self::TooManyPipelineStages { count, max, .. } => Self::TooManyPipelineStages {
                count,
                max,
                span: s,
            },
            Self::TooManySetOperands { count, max, .. } => Self::TooManySetOperands {
                count,
                max,
                span: s,
            },
            Self::EmptyQuery { .. } => Self::EmptyQuery { span: s },
            Self::InvalidHash { found, .. } => Self::InvalidHash { found, span: s },
            Self::ReasonTooLong { length, max, .. } => Self::ReasonTooLong {
                length,
                max,
                span: s,
            },
            Self::UnknownEvolveField {
                found, suggestion, ..
            } => Self::UnknownEvolveField {
                found,
                span: s,
                suggestion,
            },
            Self::MissingReason { .. } => Self::MissingReason { span: s },
            Self::MissingSetClause { .. } => Self::MissingSetClause { span: s },
            Self::IncompatibleTypes {
                left,
                right,
                suggestion,
                ..
            } => Self::IncompatibleTypes {
                left,
                right,
                span: s,
                suggestion,
            },
            Self::PipelineTypeMismatch {
                stage,
                expected,
                found,
                ..
            } => Self::PipelineTypeMismatch {
                stage,
                expected,
                found,
                span: s,
            },
            Self::ExtractorRequiresFacts {
                extractor, found, ..
            } => Self::ExtractorRequiresFacts {
                extractor,
                found,
                span: s,
            },
            Self::BudgetExceeded { detail, .. } => Self::BudgetExceeded { detail, span: s },
            Self::InvalidQuery { detail, .. } => Self::InvalidQuery { detail, span: s },
            Self::CryptoError { detail, .. } => Self::CryptoError { detail, span: s },
            Self::HashNotFound { hash, .. } => Self::HashNotFound { hash, span: s },
            Self::Tier1NotEnabled { statement, .. } => Self::Tier1NotEnabled { statement, span: s },
            Self::InvalidUtf8 { detail, .. } => Self::InvalidUtf8 { detail, span: s },
            Self::QueryTimeout {
                elapsed_ms,
                limit_ms,
                ..
            } => Self::QueryTimeout {
                elapsed_ms,
                limit_ms,
                span: s,
            },
            Self::FieldNotOnGrainType {
                field,
                grain_type,
                suggestion,
                ..
            } => Self::FieldNotOnGrainType {
                field,
                grain_type,
                span: s,
                suggestion,
            },
            Self::EngineFieldNotFilterable { field, context, .. } => {
                Self::EngineFieldNotFilterable {
                    field,
                    context,
                    span: s,
                }
            }
            Self::AssembleTooManySources { count, max, .. } => Self::AssembleTooManySources {
                count,
                max,
                span: s,
            },
            Self::AssembleBudgetExceeded {
                value, max, unit, ..
            } => Self::AssembleBudgetExceeded {
                value,
                max,
                unit,
                span: s,
            },
            Self::AssembleDuplicateLabel { label, .. } => {
                Self::AssembleDuplicateLabel { label, span: s }
            }
            Self::AssemblePriorityMismatch { label, .. } => {
                Self::AssemblePriorityMismatch { label, span: s }
            }
            Self::TooManyLetBindings { count, max, .. } => Self::TooManyLetBindings {
                count,
                max,
                span: s,
            },
            Self::LetCircularReference { name, .. } => Self::LetCircularReference { name, span: s },
            Self::LetDepthExceeded { depth, max, .. } => Self::LetDepthExceeded {
                depth,
                max,
                span: s,
            },
            Self::CoalesceTooManyBranches { count, max, .. } => Self::CoalesceTooManyBranches {
                count,
                max,
                span: s,
            },
            Self::AssembleTimeout {
                elapsed_ms,
                limit_ms,
                ..
            } => Self::AssembleTimeout {
                elapsed_ms,
                limit_ms,
                span: s,
            },
            Self::InvalidJsonCal { detail, .. } => Self::InvalidJsonCal { detail, span: s },
            Self::NotAuthorized { detail, .. } => Self::NotAuthorized { detail, span: s },
            Self::TemplateTooLarge { size, max, .. } => {
                Self::TemplateTooLarge { size, max, span: s }
            }
            Self::TemplateNestedEach { .. } => Self::TemplateNestedEach { span: s },
            Self::TemplateUnknownVariable {
                name, suggestion, ..
            } => Self::TemplateUnknownVariable {
                name,
                span: s,
                suggestion,
            },
            Self::TemplateUnknownFilter { name, .. } => {
                Self::TemplateUnknownFilter { name, span: s }
            }
            Self::TemplateInvalidName { name, .. } => Self::TemplateInvalidName { name, span: s },
            Self::TemplateNotFound { name, .. } => Self::TemplateNotFound { name, span: s },
            Self::TemplateBuiltinImmutable { name, .. } => {
                Self::TemplateBuiltinImmutable { name, span: s }
            }
            Self::TemplateParentNotFound { name, parent, .. } => Self::TemplateParentNotFound {
                name,
                parent,
                span: s,
            },
            Self::TemplateInheritanceDepth { name, .. } => {
                Self::TemplateInheritanceDepth { name, span: s }
            }
            Self::TemplateSyntaxError { detail, .. } => {
                Self::TemplateSyntaxError { detail, span: s }
            }
            Self::RenderOutputTooLarge { size, max, .. } => {
                Self::RenderOutputTooLarge { size, max, span: s }
            }
            Self::UnsupportedVersion { version, .. } => {
                Self::UnsupportedVersion { version, span: s }
            }
            Self::TooManyFormats { count, max, .. } => Self::TooManyFormats {
                count,
                max,
                span: s,
            },
            Self::TooManyUserVars { count, max, .. } => Self::TooManyUserVars {
                count,
                max,
                span: s,
            },
            Self::UserVarTooLarge { key, size, max, .. } => Self::UserVarTooLarge {
                key,
                size,
                max,
                span: s,
            },
            Self::DuplicateFormatKey { key, .. } => Self::DuplicateFormatKey { key, span: s },
            Self::AssemblePinnedBudgetExceeded {
                labels,
                required,
                budget,
                ..
            } => Self::AssemblePinnedBudgetExceeded {
                labels,
                required,
                budget,
                span: s,
            },
            Self::MissingAccumulateOps { .. } => Self::MissingAccumulateOps { span: s },
            Self::AccumulateNonNumericField { field, current, .. } => {
                Self::AccumulateNonNumericField {
                    field,
                    current,
                    span: s,
                }
            }
            Self::AccumulateTipNotFound {
                subject, relation, ..
            } => Self::AccumulateTipNotFound {
                subject,
                relation,
                span: s,
            },
            Self::AccumulateRetryExhausted {
                subject, relation, ..
            } => Self::AccumulateRetryExhausted {
                subject,
                relation,
                span: s,
            },
            Self::AccumulateInternal { .. } => Self::AccumulateInternal { span: s },
            Self::AccumulateBackpressureRejected {
                subject, relation, ..
            } => Self::AccumulateBackpressureRejected {
                subject,
                relation,
                span: s,
            },
            Self::QueryNotFound { name, .. } => Self::QueryNotFound { name, span: s },
            Self::DuplicateQueryName { name, .. } => Self::DuplicateQueryName { name, span: s },
            Self::CannotExtendData { name, .. } => Self::CannotExtendData { name, span: s },
            Self::TemplateNestingTooDeep { max, .. } => {
                Self::TemplateNestingTooDeep { max, span: s }
            }
            Self::TooManyTemplates { count, max, .. } => Self::TooManyTemplates {
                count,
                max,
                span: s,
            },
            Self::TooManyQueries { count, max, .. } => Self::TooManyQueries {
                count,
                max,
                span: s,
            },
            Self::QueryBodyTooLarge { size, max, .. } => {
                Self::QueryBodyTooLarge { size, max, span: s }
            }
            Self::TooManyQueryParams { count, max, .. } => Self::TooManyQueryParams {
                count,
                max,
                span: s,
            },
            Self::MissingQueryParam { name, query, .. } => Self::MissingQueryParam {
                name,
                query,
                span: s,
            },
            Self::RecursiveQuery { .. } => Self::RecursiveQuery { span: s },
            Self::WriteInQueryBody { stmt, .. } => Self::WriteInQueryBody { stmt, span: s },
            Self::InvalidQueryBody { detail, .. } => Self::InvalidQueryBody { detail, span: s },
            // InsufficientScope has no source span — return unchanged.
            Self::InsufficientScope {
                required,
                statement,
            } => Self::InsufficientScope {
                required,
                statement,
            },
            // LlmFeatureUnavailable has no source span — return unchanged.
            Self::LlmFeatureUnavailable { feature } => Self::LlmFeatureUnavailable { feature },
        }
    }

    /// Format a diagnostic message suitable for terminal or JSON error
    /// responses.  Includes the error code, message, location (if known),
    /// and suggestion (if any).
    pub fn diagnostic(&self) -> String {
        let mut msg = self.to_string();
        if let Some(span) = self.span() {
            msg.push_str(&format!(" at {}", span));
        }
        if let Some(hint) = self.suggestion() {
            msg.push_str(&format!(" (hint: {})", hint));
        }
        msg
    }

    /// Return a sanitized error message safe for client-facing responses.
    ///
    /// Strips the free-form `detail` field from variants that carry inner
    /// error strings (typically `AreevError::to_string()` passed through
    /// from the executor / assembler / crypto path). These strings can leak
    /// internal paths, identifiers, backend errors, and key names (CWE-209).
    ///
    /// The full diagnostic is still available via `Display` /
    /// `diagnostic()` for server-side logging — only the client-facing
    /// surface is stripped.
    ///
    /// Variants WITHOUT a `detail` field are returned via `diagnostic()`
    /// unchanged: their messages are bounded constants or caller-supplied
    /// values that the parser already validated (identifier names, limits,
    /// counts, etc.) and do not expose internal implementation details.
    pub fn sanitize_for_client(&self) -> String {
        let code = self.code();
        let span_suffix = self
            .span()
            .map(|s| format!(" at {}", s))
            .unwrap_or_default();
        match self {
            // Variants whose `#[error]` message ends in `: {detail}` —
            // the detail is constructed from inner errors (e.g. crypto
            // failures, executor errors, backend message from `AreevError`).
            // Replace with the code + a generic description, never the detail.
            Self::BudgetExceeded { .. } => {
                format!("{}: budget exceeded{}", code, span_suffix)
            }
            Self::InvalidQuery { .. } => {
                format!("{}: invalid query{}", code, span_suffix)
            }
            Self::CryptoError { .. } => {
                format!(
                    "{}: crypto error during query execution{}",
                    code, span_suffix
                )
            }
            Self::InvalidJsonCal { .. } => {
                format!("{}: invalid JSON+CAL input{}", code, span_suffix)
            }
            Self::NotAuthorized { detail, .. } => {
                // Deliberately unredacted: the detail names the caller's own
                // principal, the refused verb, and the namespace — no
                // internals, and it is the on-ramp to the GRANT that fixes
                // it.
                format!("{}: not authorized: {}{}", code, detail, span_suffix)
            }
            Self::TemplateSyntaxError { .. } => {
                format!("{}: template syntax error{}", code, span_suffix)
            }
            Self::InvalidQueryBody { .. } => {
                format!("{}: invalid query body{}", code, span_suffix)
            }
            // CAL-E083 — strip control chars from caller-supplied
            // subject/relation before echoing (security C4 — log-injection
            // and odd-byte safety). Body remains: code + stable message
            // + sanitized subject/relation. Inner cause never reaches
            // the wire (security C3).
            Self::AccumulateRetryExhausted {
                subject, relation, ..
            } => {
                format!(
                    "{}: ACCUMULATE retry budget exhausted (subject=\"{}\", relation=\"{}\"){}",
                    code,
                    sanitize_echo(subject),
                    sanitize_echo(relation),
                    span_suffix
                )
            }
            // CAL-E084 — never echo inner-error text (security C3).
            Self::AccumulateInternal { .. } => {
                format!("{}: ACCUMULATE internal failure{}", code, span_suffix)
            }
            // CAL-E085 — same echo handling as CAL-E083 (sanitize
            // caller-supplied subject/relation; no queue-depth signal in
            // the body — security review condition).
            Self::AccumulateBackpressureRejected {
                subject, relation, ..
            } => {
                format!(
                    "{}: ACCUMULATE backpressure: per-key inflight cap exceeded (subject=\"{}\", relation=\"{}\"){}",
                    code,
                    sanitize_echo(subject),
                    sanitize_echo(relation),
                    span_suffix
                )
            }
            // All other variants: their messages are bounded strings
            // (codes, counts, limits, parser-validated identifiers) —
            // safe to pass through with span + suggestion.
            _ => self.diagnostic(),
        }
    }
}

/// Strip control characters and trim caller-supplied identifiers before
/// echoing them in error messages (CU-86d2wr4n4 security C4).
///
/// Replaces ASCII control bytes (incl. CR/LF and the bidi-override range
/// already rejected by the lexer for query bodies, but reapplied here in
/// case error construction sites bypass the lexer) with `?`. Caps the
/// echoed length at 128 chars so untrusted callers cannot bloat error
/// bodies.
fn sanitize_echo(s: &str) -> String {
    const MAX_ECHO_LEN: usize = 128;
    let mut out = String::with_capacity(s.len().min(MAX_ECHO_LEN));
    for ch in s.chars().take(MAX_ECHO_LEN) {
        if ch.is_control() || ('\u{202A}'..='\u{202E}').contains(&ch) {
            out.push('?');
        } else {
            out.push(ch);
        }
    }
    out
}

// ---------------------------------------------------------------------------
// CalWarning — non-fatal diagnostics
// ---------------------------------------------------------------------------

/// Non-fatal CAL warnings emitted during parsing or execution.
#[derive(Debug, Clone, PartialEq)]
pub enum CalWarning {
    /// CAL-W001 — The relation name in a Fact grain is not one of the
    /// well-known OMS relations.
    UnknownRelation {
        relation: String,
        span: Option<Span>,
    },

    /// CAL-W002 — A domain-prefixed field was used without a
    /// corresponding `@tag` on the query.
    DomainFieldWithoutTag { field: String, span: Option<Span> },

    /// CAL-W003 — A domain prefix was not recognised.
    UnknownDomainPrefix { prefix: String, span: Option<Span> },

    /// CAL-W004 — An extension option in a `WITH` clause was not
    /// recognised and will be ignored.
    UnknownExtensionOption { option: String, span: Option<Span> },

    /// CAL-W005 — A SET field name was specified more than once in the
    /// same statement; only the last value is used.
    DuplicateSetField { field: String, span: Option<Span> },

    /// CAL-W006 — A parameter was supplied at the RUN call site but is not
    /// referenced in the saved query body.
    UnusedQueryParam {
        name: String,
        query: String,
        span: Option<Span>,
    },

    /// CAL-W007 — The bare pipe operator `|` before pipeline stages is
    /// deprecated (removed in CAL 1.1). Use direct clause syntax instead
    /// (e.g. `RECALL facts ORDER BY confidence DESC LIMIT 10`).
    DeprecatedPipeOperator { span: Option<Span> },

    /// CAL-W008 — IS CATEGORY used on a non-relation field. The IS CATEGORY
    /// check is only meaningful on the `relation` field; using it on other
    /// fields silently produces no matches.
    IsCategoryOnNonRelation {
        field: String,
        category: String,
        span: Option<Span>,
    },

    /// CAL-W009 — ASSEMBLE sources have inconsistent subject scoping.
    /// Some sources filter by subject while others don't, which may return
    /// data from unrelated subjects.
    AssembleUnscopedSource {
        labels: Vec<String>,
        span: Option<Span>,
    },

    /// CAL-W010 — A WHERE field on an untyped (`RECALL all`) query is not a
    /// recognized field on any grain type. The filter is still applied per
    /// grain (matching only grains that carry the field — likely none), so
    /// this usually signals a misspelled field name. On a *typed* recall the
    /// same situation is a hard `CAL-E060` instead (#91: a filter that
    /// cannot be honoured refuses rather than widening).
    UnrecognizedWhereField { field: String, span: Option<Span> },

    /// CAL-W011 — A `{{#each}}` block hit the OMS CAL §10.8 iteration cap,
    /// so the rendered output covers only the first `max` grains. The result
    /// set itself is complete; only this rendering is short.
    EachIterationCapped {
        rendered: usize,
        total: usize,
        max: usize,
    },

    /// CAL-W012 — A `CONTRADICTIONS` query's candidate scan hit the executor's
    /// `max_limit`, so grains past it were never examined for fork status.
    ///
    /// This exists because the useful answer to `CONTRADICTIONS` is often the
    /// *empty* one, and an agent may act on it. "Nothing is contested" and
    /// "nothing among the first N is contested" are different claims; without
    /// this warning the second would be indistinguishable from the first.
    ContradictionScanBounded { scanned: usize },

    /// CAL-W014 — A `WITH` option parsed and ran, but does nothing on the
    /// statement it was attached to.
    ///
    /// §5 promises that an option needing an unavailable backend "returns an
    /// honest error rather than silently degrading". Several did neither: they
    /// parsed, ran, and returned output byte-identical to the same query
    /// without them. A hard error would break callers who have been passing
    /// these since 1.0, so the honest form is a warning that names the option
    /// and the surface — silence was the actual defect.
    WithOptionInert {
        option: &'static str,
        statement: &'static str,
        why: &'static str,
    },

    /// CAL-W015 — A post-retrieval stage (ORDER BY, a type-specific WHERE
    /// filter, COUNT) widened its scan to the executor's `max_limit` and
    /// still filled it, so it ranked/filtered/counted a bounded window rather
    /// than the whole matching set.
    ///
    /// The sibling of `ContradictionScanBounded`, generalized. `ORDER BY`
    /// sorts the grains a statement already returned; without widening, that
    /// is a page of `default_limit` rows, so `ORDER BY priority DESC LIMIT 5`
    /// returned the top 5 *of the newest 50* and looked exactly like the top
    /// 5 overall. Widening fixes every corpus up to `max_limit`; past that the
    /// only honest thing left is to say so, because the answer is still a
    /// well-formed list that happens to be wrong.
    ScanBounded {
        /// What forced the wide scan — "ORDER BY priority", "WHERE tool_name", "COUNT".
        stage: String,
        scanned: usize,
    },

    /// CAL-W016 — A pipeline stage was attached to a payload it cannot act on
    /// (e.g. `ORDER BY` on a multi-source `ASSEMBLE`, which returns an
    /// assembled section list rather than a flat grain list).
    ///
    /// These used to hit a catch-all passthrough arm and vanish with no error
    /// and no warning, which contradicts `docs/cal-reference.md` §5: silence
    /// means the option did something. Ordering an assembly is exactly the
    /// case a host reaches for when rendering authored instruction blocks in
    /// an intended order — and it was the one case that silently did nothing.
    PipelineStageInert {
        stage: String,
        payload: &'static str,
        why: &'static str,
    },
}

impl CalWarning {
    /// Return the CAL spec warning code.
    pub fn code(&self) -> &'static str {
        match self {
            Self::UnknownRelation { .. } => "CAL-W001",
            Self::DomainFieldWithoutTag { .. } => "CAL-W002",
            Self::UnknownDomainPrefix { .. } => "CAL-W003",
            Self::UnknownExtensionOption { .. } => "CAL-W004",
            Self::DuplicateSetField { .. } => "CAL-W005",
            Self::UnusedQueryParam { .. } => "CAL-W006",
            Self::DeprecatedPipeOperator { .. } => "CAL-W007",
            Self::IsCategoryOnNonRelation { .. } => "CAL-W008",
            Self::AssembleUnscopedSource { .. } => "CAL-W009",
            Self::UnrecognizedWhereField { .. } => "CAL-W010",
            Self::EachIterationCapped { .. } => "CAL-W011",
            Self::ContradictionScanBounded { .. } => "CAL-W012",
            Self::WithOptionInert { .. } => "CAL-W014",
            Self::ScanBounded { .. } => "CAL-W015",
            Self::PipelineStageInert { .. } => "CAL-W016",
        }
    }

    /// Return the source span, if one was recorded.
    pub fn span(&self) -> Option<Span> {
        match self {
            Self::UnknownRelation { span, .. }
            | Self::DomainFieldWithoutTag { span, .. }
            | Self::UnknownDomainPrefix { span, .. }
            | Self::UnknownExtensionOption { span, .. }
            | Self::DuplicateSetField { span, .. }
            | Self::UnusedQueryParam { span, .. }
            | Self::DeprecatedPipeOperator { span }
            | Self::IsCategoryOnNonRelation { span, .. }
            | Self::AssembleUnscopedSource { span, .. }
            | Self::UnrecognizedWhereField { span, .. } => *span,
            Self::EachIterationCapped { .. }
            | Self::ContradictionScanBounded { .. }
            | Self::WithOptionInert { .. }
            | Self::ScanBounded { .. }
            | Self::PipelineStageInert { .. } => None,
        }
    }
}

impl std::fmt::Display for CalWarning {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownRelation { relation, .. } => {
                write!(f, "CAL-W001: Unknown relation \"{}\"", relation)
            }
            Self::DomainFieldWithoutTag { field, .. } => {
                write!(f, "CAL-W002: Domain field \"{}\" used without @tag", field)
            }
            Self::UnknownDomainPrefix { prefix, .. } => {
                write!(f, "CAL-W003: Unknown domain prefix \"{}\"", prefix)
            }
            Self::UnknownExtensionOption { option, .. } => {
                write!(
                    f,
                    "CAL-W004: Unknown extension option \"{}\" (ignored)",
                    option
                )
            }
            Self::DuplicateSetField { field, .. } => {
                write!(
                    f,
                    "CAL-W005: Duplicate SET field \"{}\" — only the last value is used",
                    field
                )
            }
            Self::UnusedQueryParam { name, query, .. } => {
                write!(
                    f,
                    "CAL-W006: Parameter \"${}\" supplied but not referenced in query \"{}\"",
                    name, query
                )
            }
            Self::DeprecatedPipeOperator { .. } => {
                write!(
                    f,
                    "CAL-W007: Bare pipe operator `|` is deprecated (CAL 1.1). Use direct clause syntax instead, e.g. `RECALL facts ORDER BY confidence DESC LIMIT 10`"
                )
            }
            Self::IsCategoryOnNonRelation {
                field, category, ..
            } => {
                write!(
                    f,
                    "CAL-W008: IS {} used on field '{}' — IS CATEGORY is only meaningful on the 'relation' field; this condition was ignored",
                    category, field
                )
            }
            Self::AssembleUnscopedSource { labels, .. } => {
                write!(
                    f,
                    "CAL-W009: ASSEMBLE source(s) [{}] have no subject filter while other sources do — results may include data from unrelated subjects",
                    labels.join(", ")
                )
            }
            Self::UnrecognizedWhereField { field, .. } => {
                write!(
                    f,
                    "CAL-W010: WHERE field '{}' is not a recognized field on any grain type; it will match only grains that carry it. Check the field name.",
                    field
                )
            }
            Self::EachIterationCapped {
                rendered,
                total,
                max,
            } => {
                write!(
                    f,
                    "CAL-W011: {{{{#each}}}} rendered {rendered} of {total} grains (§10.8 caps iteration at {max}) — the result set is complete, this rendering is not"
                )
            }
            Self::ContradictionScanBounded { scanned } => {
                write!(
                    f,
                    "CAL-W012: CONTRADICTIONS examined the first {scanned} matching grains (the executor's max_limit) — grains past that were not checked, so this is not a complete all-clear. Narrow the query with WHERE/ABOUT/SINCE to be sure."
                )
            }
            Self::WithOptionInert {
                option,
                statement,
                why,
            } => {
                write!(
                    f,
                    "CAL-W014: WITH {option} has no effect on {statement} — {why}. The result is the same as without it."
                )
            }
            Self::ScanBounded { stage, scanned } => {
                write!(
                    f,
                    "CAL-W015: {stage} ran over the first {scanned} matching grains (the executor's max_limit) and that scan came back full — grains past it were never considered, so this is a bounded answer, not the true one. Narrow the query with WHERE/ABOUT/SINCE, or raise max_limit."
                )
            }
            Self::PipelineStageInert {
                stage,
                payload,
                why,
            } => {
                write!(
                    f,
                    "CAL-W016: {stage} has no effect on a {payload} result — {why}. The stage was skipped; the result is the same as without it."
                )
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Result alias
// ---------------------------------------------------------------------------

/// Convenience alias used throughout the CAL module.
pub type CalResult<T> = std::result::Result<T, CalError>;

// ---------------------------------------------------------------------------
// Conversion: CalError → AreevError
// ---------------------------------------------------------------------------

impl From<CalError> for areev_core::error::AreevError {
    fn from(e: CalError) -> Self {
        areev_core::error::AreevError::Validation(e.diagnostic())
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_error_codes_match_display() {
        let err = CalError::QueryTooLong {
            length: 5000,
            max: 4096,
            span: None,
        };
        assert!(err.to_string().starts_with("CAL-E001"));
        assert_eq!(err.code(), "CAL-E001");
    }

    #[test]
    fn test_invalid_query_is_e092_not_budget() {
        // A store validation failure must not masquerade as CAL-E030
        // "Budget exceeded" (the mislabel the persona review flagged).
        let err = CalError::InvalidQuery {
            detail: "VAL-E001: validation error: bad filter".into(),
            span: None,
        };
        assert_eq!(err.code(), "CAL-E092");
        assert!(err.to_string().starts_with("CAL-E092"));
        // Inner store detail is stripped on the client-facing path (CWE-209).
        let sanitized = err.sanitize_for_client();
        assert!(sanitized.starts_with("CAL-E092"));
        assert!(!sanitized.contains("bad filter"));
    }

    #[test]
    fn test_with_suggestion() {
        let err = CalError::UnknownGrainType {
            found: "facts".into(),
            span: None,
            suggestion: None,
        };
        let err = err.with_suggestion("did you mean \"facts\"?");
        assert_eq!(err.suggestion(), Some("did you mean \"facts\"?"));
    }

    #[test]
    fn test_with_span() {
        let err = CalError::EmptyQuery { span: None };
        assert!(err.span().is_none());
        let err = err.with_span(Span::new(0, 5, 1, 1));
        assert_eq!(err.span(), Some(Span::new(0, 5, 1, 1)));
    }

    #[test]
    fn test_diagnostic_with_span_and_suggestion() {
        let err = CalError::UnknownField {
            found: "titel".into(),
            span: Some(Span::new(10, 15, 1, 11)),
            suggestion: Some("did you mean \"title\"?".into()),
        };
        let diag = err.diagnostic();
        assert!(diag.contains("CAL-E004"));
        assert!(diag.contains("at 1:11"));
        assert!(diag.contains("hint: did you mean \"title\"?"));
    }

    /// Follow-up #3: `CalError::sanitize_for_client()` must strip the inner
    /// `detail` field for variants that carry inner error strings.
    /// These details often come from `AreevError::to_string()` passed through
    /// from the executor/assemble/crypto paths — they can leak internal
    /// paths, identifiers, and backend error shapes to the client (CWE-209).
    #[test]
    fn test_sanitize_strips_detail_for_leaky_variants() {
        // BudgetExceeded carries inner AreevError text in `detail` on the
        // executor error-mapping paths (see src/cal/executor.rs). The
        // sanitised form must NOT include that detail.
        let leaky_detail = "user_id=alice@example.com /var/lib/areev/db blob 0xABCDEF missing dek";
        let err = CalError::BudgetExceeded {
            detail: leaky_detail.into(),
            span: Some(Span::new(10, 15, 2, 5)),
        };
        let sanitized = err.sanitize_for_client();
        assert!(
            sanitized.starts_with("CAL-E030"),
            "sanitised error must carry the CAL code, got: {sanitized}"
        );
        assert!(
            !sanitized.contains(leaky_detail),
            "sanitised error must NOT contain the inner detail: {sanitized}"
        );
        assert!(
            !sanitized.contains("alice@example.com"),
            "sanitised error must NOT contain user identifiers: {sanitized}"
        );
        assert!(
            !sanitized.contains("/var/lib/areev/db"),
            "sanitised error must NOT contain internal paths: {sanitized}"
        );
        // Span may still appear — it is a public input position, not an
        // internal identifier.
        assert!(
            sanitized.contains("2:5"),
            "sanitised error should keep the public span: {sanitized}"
        );

        // The full diagnostic should STILL contain the detail for
        // server-side logging — only the client-facing sanitisation strips it.
        let diag = err.diagnostic();
        assert!(
            diag.contains(leaky_detail),
            "diagnostic() must preserve the full detail for server logs"
        );
    }

    #[test]
    fn test_sanitize_strips_detail_for_all_leaky_variants() {
        // All five CalError variants that carry a free-form `detail`.
        let variants = [
            CalError::BudgetExceeded {
                detail: "internal backend=Fjall key=aabbcc".into(),
                span: None,
            },
            CalError::CryptoError {
                detail: "DEK 0xDEADBEEF destroyed for user alice".into(),
                span: None,
            },
            CalError::InvalidJsonCal {
                detail: "expected field `tok_xyz` at pointer /auth/token".into(),
                span: None,
            },
            CalError::TemplateSyntaxError {
                detail: "unclosed {{alice.secret}} at /tmpl/1".into(),
                span: None,
            },
            CalError::InvalidQueryBody {
                detail: "grain 0xA1B2 under namespace ns_internal".into(),
                span: None,
            },
        ];
        for err in variants {
            let sanitized = err.sanitize_for_client();
            let code = err.code();
            assert!(
                sanitized.starts_with(code),
                "{code}: sanitised output must start with the code, got: {sanitized}"
            );
            // Inner detail strings contain tokens like "0x", "alice",
            // "DEK", "namespace" — none should leak.
            for leaky in ["0xDEADBEEF", "alice", "0xA1B2", "aabbcc", "tok_xyz"] {
                assert!(
                    !sanitized.contains(leaky),
                    "{code}: sanitised must not contain '{leaky}', got: {sanitized}"
                );
            }
        }
    }

    #[test]
    fn test_sanitize_passthrough_for_bounded_variants() {
        // Bounded variants (no free-form `detail` field) pass through
        // their `diagnostic()` output unchanged: the message is built from
        // parser-validated identifiers, numeric limits, and constants that
        // the server itself generated — safe to surface to clients.
        let err = CalError::UnknownField {
            found: "titel".into(),
            span: Some(Span::new(10, 15, 1, 11)),
            suggestion: Some("did you mean \"title\"?".into()),
        };
        let sanitized = err.sanitize_for_client();
        assert_eq!(sanitized, err.diagnostic());
        assert!(sanitized.contains("CAL-E004"));
        assert!(sanitized.contains("titel"));
        assert!(sanitized.contains("at 1:11"));
        assert!(sanitized.contains("hint: did you mean \"title\"?"));
    }

    #[test]
    fn test_warning_codes() {
        let w = CalWarning::UnknownRelation {
            relation: "foobar".into(),
            span: None,
        };
        assert_eq!(w.code(), "CAL-W001");
        assert!(w.to_string().starts_with("CAL-W001"));
    }

    #[test]
    fn test_span_display() {
        let span = Span::new(10, 20, 3, 5);
        assert_eq!(format!("{}", span), "3:5");
    }

    #[test]
    fn test_into_areev_error() {
        let err = CalError::EmptyQuery { span: None };
        let areev_err: areev_core::error::AreevError = err.into();
        match areev_err {
            areev_core::error::AreevError::Validation(msg) => {
                assert!(msg.contains("CAL-E014"));
            }
            other => panic!("expected Validation, got {:?}", other),
        }
    }

    #[test]
    fn test_with_suggestion_on_non_suggestion_variant() {
        // Calling with_suggestion on a variant without a suggestion field
        // should return the error unchanged.
        let err = CalError::EmptyQuery { span: None };
        let err = err.with_suggestion("this should be ignored");
        assert!(err.suggestion().is_none());
    }

    // -----------------------------------------------------------------------
    // Phase 2 error codes: verify code() matches Display prefix
    // -----------------------------------------------------------------------

    #[test]
    fn test_phase2_error_codes_match_display() {
        let test_cases: Vec<(CalError, &str)> = vec![
            (
                CalError::AssembleTooManySources {
                    count: 10,
                    max: 8,
                    span: None,
                },
                "CAL-E032",
            ),
            (
                CalError::AssembleBudgetExceeded {
                    value: 200_000,
                    max: 100_000,
                    unit: "tokens".into(),
                    span: None,
                },
                "CAL-E033",
            ),
            (
                CalError::AssembleDuplicateLabel {
                    label: "src1".into(),
                    span: None,
                },
                "CAL-E034",
            ),
            (
                CalError::AssemblePriorityMismatch {
                    label: "src2".into(),
                    span: None,
                },
                "CAL-E035",
            ),
            (
                CalError::TooManyLetBindings {
                    count: 6,
                    max: 5,
                    span: None,
                },
                "CAL-E036",
            ),
            (
                CalError::LetCircularReference {
                    name: "x".into(),
                    span: None,
                },
                "CAL-E037",
            ),
            (
                CalError::LetDepthExceeded {
                    depth: 4,
                    max: 3,
                    span: None,
                },
                "CAL-E038",
            ),
            (
                CalError::CoalesceTooManyBranches {
                    count: 6,
                    max: 5,
                    span: None,
                },
                "CAL-E039",
            ),
            (
                // InvalidJsonCal lives at CAL-E120; CAL-E070 is InvalidUtf8.
                CalError::InvalidJsonCal {
                    detail: "bad json".into(),
                    span: None,
                },
                "CAL-E120",
            ),
            (
                CalError::NotAuthorized {
                    detail: "AUT-E001: principal agent:bot lacks write on namespace \"caller\"".into(),
                    span: None,
                },
                "CAL-E121",
            ),
            (
                CalError::AssembleTimeout {
                    elapsed_ms: 6000,
                    limit_ms: 5000,
                    span: None,
                },
                "CAL-E071",
            ),
            (CalError::MissingAccumulateOps { span: None }, "CAL-E080"),
            (
                CalError::AccumulateNonNumericField {
                    field: "alpha".into(),
                    current: "str".into(),
                    span: None,
                },
                "CAL-E081",
            ),
            (
                CalError::AccumulateTipNotFound {
                    subject: "x".into(),
                    relation: "y".into(),
                    span: None,
                },
                "CAL-E082",
            ),
        ];
        for (err, expected_code) in test_cases {
            assert_eq!(
                err.code(),
                expected_code,
                "code() mismatch for error: {}",
                err
            );
            assert!(
                err.to_string().starts_with(expected_code),
                "Display output should start with {}, got: {}",
                expected_code,
                err
            );
        }
    }

    #[test]
    fn test_all_error_codes_have_unique_codes() {
        // Ensure no two error variants accidentally share the same code string.
        let errors: Vec<CalError> = vec![
            CalError::QueryTooLong {
                length: 0,
                max: 0,
                span: None,
            },
            CalError::UnexpectedToken {
                expected: "".into(),
                found: "".into(),
                span: None,
                suggestion: None,
            },
            CalError::UnknownGrainType {
                found: "".into(),
                span: None,
                suggestion: None,
            },
            CalError::UnknownField {
                found: "".into(),
                span: None,
                suggestion: None,
            },
            CalError::UnterminatedString { span: None },
            CalError::InvalidNumber {
                found: "".into(),
                span: None,
            },
            CalError::NestingTooDeep {
                depth: 0,
                max: 0,
                span: None,
            },
            CalError::UnboundParameter {
                name: "".into(),
                span: None,
            },
            CalError::DuplicateParameter {
                name: "".into(),
                span: None,
            },
            CalError::LimitExceeded {
                value: 0,
                max: 0,
                span: None,
            },
            CalError::InSetTooLarge {
                count: 0,
                max: 0,
                span: None,
            },
            CalError::TooManyPipelineStages {
                count: 0,
                max: 0,
                span: None,
            },
            CalError::TooManySetOperands {
                count: 0,
                max: 0,
                span: None,
            },
            CalError::EmptyQuery { span: None },
            CalError::InvalidHash {
                found: "".into(),
                span: None,
            },
            CalError::ReasonTooLong {
                length: 0,
                max: 0,
                span: None,
            },
            CalError::UnknownEvolveField {
                found: "".into(),
                span: None,
                suggestion: None,
            },
            CalError::MissingReason { span: None },
            CalError::MissingSetClause { span: None },
            CalError::IncompatibleTypes {
                left: "".into(),
                right: "".into(),
                span: None,
                suggestion: None,
            },
            CalError::PipelineTypeMismatch {
                stage: "".into(),
                expected: "".into(),
                found: "".into(),
                span: None,
            },
            CalError::ExtractorRequiresFacts {
                extractor: "".into(),
                found: "".into(),
                span: None,
            },
            CalError::BudgetExceeded {
                detail: "".into(),
                span: None,
            },
            CalError::QueryTimeout {
                elapsed_ms: 0,
                limit_ms: 0,
                span: None,
            },
            CalError::InvalidQuery {
                detail: "".into(),
                span: None,
            },
            CalError::FieldNotOnGrainType {
                field: "".into(),
                grain_type: "".into(),
                span: None,
                suggestion: None,
            },
            CalError::AssembleTooManySources {
                count: 0,
                max: 0,
                span: None,
            },
            CalError::AssembleBudgetExceeded {
                value: 0,
                max: 0,
                unit: "".into(),
                span: None,
            },
            CalError::AssembleDuplicateLabel {
                label: "".into(),
                span: None,
            },
            CalError::AssemblePriorityMismatch {
                label: "".into(),
                span: None,
            },
            CalError::TooManyLetBindings {
                count: 0,
                max: 0,
                span: None,
            },
            CalError::LetCircularReference {
                name: "".into(),
                span: None,
            },
            CalError::LetDepthExceeded {
                depth: 0,
                max: 0,
                span: None,
            },
            CalError::CoalesceTooManyBranches {
                count: 0,
                max: 0,
                span: None,
            },
            CalError::AssembleTimeout {
                elapsed_ms: 0,
                limit_ms: 0,
                span: None,
            },
            CalError::InvalidJsonCal {
                detail: "".into(),
                span: None,
            },
            CalError::TemplateTooLarge {
                size: 0,
                max: 0,
                span: None,
            },
            CalError::TemplateNestedEach { span: None },
            CalError::TemplateUnknownVariable {
                name: "".into(),
                span: None,
                suggestion: None,
            },
            CalError::TemplateUnknownFilter {
                name: "".into(),
                span: None,
            },
            CalError::TemplateInvalidName {
                name: "".into(),
                span: None,
            },
            CalError::TemplateNotFound {
                name: "".into(),
                span: None,
            },
            CalError::TemplateBuiltinImmutable {
                name: "".into(),
                span: None,
            },
            CalError::TemplateParentNotFound {
                name: "".into(),
                parent: "".into(),
                span: None,
            },
            CalError::TemplateInheritanceDepth {
                name: "".into(),
                span: None,
            },
            CalError::TemplateSyntaxError {
                detail: "".into(),
                span: None,
            },
            CalError::RenderOutputTooLarge {
                size: 0,
                max: 0,
                span: None,
            },
            CalError::UnsupportedVersion {
                version: 0,
                span: None,
            },
            CalError::TooManyFormats {
                count: 0,
                max: 0,
                span: None,
            },
            CalError::TooManyUserVars {
                count: 0,
                max: 0,
                span: None,
            },
            CalError::UserVarTooLarge {
                key: "".into(),
                size: 0,
                max: 0,
                span: None,
            },
            CalError::DuplicateFormatKey {
                key: "".into(),
                span: None,
            },
            CalError::MissingAccumulateOps { span: None },
            CalError::AccumulateNonNumericField {
                field: "".into(),
                current: "".into(),
                span: None,
            },
            CalError::AccumulateTipNotFound {
                subject: "".into(),
                relation: "".into(),
                span: None,
            },
        ];
        let mut codes = std::collections::HashSet::new();
        for err in &errors {
            let code = err.code();
            assert!(
                codes.insert(code),
                "Duplicate error code found: {} (shared between multiple variants)",
                code
            );
        }
        // 50 total CalError variants (27 Phase 1 + 9 Phase 2 + 11 Phase 4 + 1 multi-format + 2 user vars)
        assert_eq!(
            codes.len(),
            errors.len(),
            "all error variants should have unique codes"
        );
    }

    #[test]
    fn test_phase2_with_span_preserves_fields() {
        let span = Span::new(10, 20, 1, 11);
        let err = CalError::TooManyLetBindings {
            count: 6,
            max: 5,
            span: None,
        };
        let err = err.with_span(span);
        assert_eq!(err.span(), Some(span));
        // Verify count and max are preserved through with_span
        match err {
            CalError::TooManyLetBindings { count, max, .. } => {
                assert_eq!(count, 6);
                assert_eq!(max, 5);
            }
            _ => panic!("wrong variant after with_span"),
        }
    }
}
