//! The `OmsSubstrate` trait — the engine's only contact with a store. It is
//! defined in terms of the OMS Level-2 protocol (CAL text ↔ JSON rows, grain
//! get/put/supersede) plus curated typed reads the built-in analyzers use.
//! Areev is the first substrate; the in-repo `ReferenceSubstrate` lets engine
//! CI run with zero Areev, and doubles as the third-party conformance kit.

use crate::error::Result;
use crate::model::GrainRecord;
use serde_json::{Map, Value};

/// Optional substrate capabilities, declared once and matched against each
/// analyzer manifest's `requires` list. A missing capability degrades an
/// analyzer to an activation-ladder entry, never a silent no-op (§8).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Capabilities {
    /// Multiple concurrent heads per entity are tracked and queryable
    /// (fork surfacing needs this).
    pub forks: bool,
    /// A telemetry sidecar records recall/access history.
    pub telemetry: bool,
    /// An embedder is installed (upgrades T0 analyzers to T1).
    pub embeddings: bool,
    /// Workflow plan grains can be structurally validated
    /// ([`SubstrateRead::validate_plan`]). Without it a `plan_revision`
    /// proposal stays advisory: the engine will not stamp an executable edit
    /// to a plan it cannot have checked for reachability and cycle bounds.
    pub plans: bool,
    /// Executable tool code is modelled — the §7.4 blob seam plus
    /// [`SubstrateRead::tool_evalset`]. Without it a `code_revision` proposal
    /// stays advisory, because Rule E1's pin cannot be resolved.
    pub code: bool,
}

/// Read filters for curated grain reads.
#[derive(Debug, Clone, Copy)]
pub struct ReadOpts {
    /// When true (default), only live (non-superseded) grains are returned.
    pub live_only: bool,
    /// When set, only grains created at or after this epoch-ms are returned
    /// (the incremental watermark scan, §8).
    pub since_ms: Option<i64>,
}

impl Default for ReadOpts {
    fn default() -> Self {
        ReadOpts {
            live_only: true,
            since_ms: None,
        }
    }
}

/// A grain to be written by an apply. `derived_from` and other provenance go
/// in `fields`; the substrate computes the content address.
#[derive(Debug, Clone, PartialEq)]
pub struct GrainSpec {
    pub grain_type: String,
    pub namespace: String,
    pub fields: Map<String, Value>,
}

impl GrainSpec {
    pub fn new(grain_type: impl Into<String>, namespace: impl Into<String>) -> Self {
        GrainSpec {
            grain_type: grain_type.into(),
            namespace: namespace.into(),
            fields: Map::new(),
        }
    }

    pub fn with_field(mut self, key: impl Into<String>, value: impl Into<Value>) -> Self {
        self.fields.insert(key.into(), value.into());
        self
    }
}

/// One entity holding more than one live head (fork surfacing input).
#[derive(Debug, Clone, PartialEq)]
pub struct HeadGroup {
    /// Entity identity, e.g. `"caller/john"` (namespace-qualified subject).
    pub entity: String,
    /// The competing head hashes.
    pub heads: Vec<String>,
}

/// A snapshot of the recall-telemetry sidecar's rollups (§8). Telemetry-fed
/// analyzers (`cold_grains`, `coverage_gap`, `budget_pressure`) read this via
/// [`crate::analyzer::AnalyzeCtx::telemetry`]. It is an owned snapshot, not a
/// live handle — analyzers stay read-only and can't reach the sidecar. A
/// substrate without a sidecar returns `None`, so the analyzer degrades to an
/// activation-ladder entry rather than firing on absent evidence.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TelemetryView {
    /// Per-grain recall rollups. A grain **absent** here has never been
    /// recalled (the `cold_grains` signal).
    pub access: Vec<GrainAccess>,
    /// Per-question rollups over free-text recalls (the `coverage_gap` signal).
    pub queries: Vec<QueryUsage>,
    /// Assembly-budget pressure rollup (the `budget_pressure` signal).
    pub budget: BudgetUsage,
}

/// How often one grain has been surfaced by recall.
#[derive(Debug, Clone, PartialEq)]
pub struct GrainAccess {
    pub hash: String,
    pub recall_count: i64,
    pub last_ms: i64,
}

/// How a recurring recall question has fared.
#[derive(Debug, Clone, PartialEq)]
pub struct QueryUsage {
    /// A short human-readable sample of the query intent.
    pub sample: String,
    pub run_count: i64,
    /// How many of those runs returned nothing — the coverage-gap signal.
    pub empty_count: i64,
    pub sum_results: i64,
    pub last_ms: i64,
}

/// Assembly-budget pressure rollup.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct BudgetUsage {
    pub sample_count: i64,
    pub overflow_count: i64,
}

/// The read-only slice of the substrate. Analyzers receive this (via
/// `AnalyzeCtx`) and nothing else — the trust floor's "analyzers execute
/// read-only" is enforced by the type system: a `&dyn SubstrateRead` cannot
/// reach any mutating method. It is object-safe (no generics) so
/// `builtin_analyzers()` can hand out `Box<dyn Analyzer>`.
pub trait SubstrateRead {
    /// Declared optional capabilities.
    fn capabilities(&self) -> Capabilities;

    /// Curated read: all grains of one OMS type, optionally namespace-scoped
    /// and watermark/liveness filtered.
    fn grains_of_type(
        &self,
        grain_type: &str,
        namespace: Option<&str>,
        opts: ReadOpts,
    ) -> Result<Vec<GrainRecord>>;

    /// Fetch one grain by content address.
    fn grain(&self, hash: &str) -> Result<Option<GrainRecord>>;

    /// Entities with more than one live head. Requires the `forks` capability;
    /// the default impl reports it missing so non-fork substrates degrade
    /// cleanly rather than pretend.
    fn heads(&self, _namespace: Option<&str>) -> Result<Vec<HeadGroup>> {
        Err(crate::error::Error::CapabilityMissing("forks".into()))
    }

    /// A snapshot of the recall-telemetry rollups (§8). Requires the
    /// `telemetry` capability; the default returns `None` so substrates without
    /// a sidecar degrade cleanly. `namespace` scopes the snapshot when set.
    fn telemetry(&self, _namespace: Option<&str>) -> Result<Option<TelemetryView>> {
        Ok(None)
    }

    /// Structurally validate a candidate Workflow grain body — the same
    /// checks the runtime would run before executing it (unique and reachable
    /// nodes, conditions parse, every cycle bounded). The engine calls this
    /// before it will stamp a `plan_revision` as executable, so a proposal
    /// that would produce an unrunnable plan never reaches a reviewer as
    /// something they could apply.
    ///
    /// This is deliberately the substrate's job: the engine owns no plan
    /// grammar, exactly as it owns no CAL grammar (see [`OmsSubstrate::validate_cal`]).
    /// Requires the `plans` capability; the default reports it missing so a
    /// substrate that does not model workflows degrades the proposal to
    /// advisory rather than pretending to have checked it.
    fn validate_plan(&self, _workflow: &Value) -> Result<()> {
        Err(crate::error::Error::CapabilityMissing("plans".into()))
    }

    /// The evalset a code revision of `tool` must be gated against (Rule E1's
    /// pin). Returns `Ok(None)` when the tool declares none, which makes a
    /// `code_revision` for it advisory — an unpinnable revision is one no
    /// gating run could ever satisfy.
    ///
    /// Resolved from the substrate and never from the model: a proposer that
    /// could name its own grader is not gated.
    fn tool_evalset(&self, _tool: &str) -> Result<Option<String>> {
        Ok(None)
    }
}

/// The full store protocol the engine binds to: reads (via the supertrait)
/// plus governed writes, CAL, and state persistence. All methods are fallible;
/// a substrate fault surfaces as [`crate::error::Error::Substrate`].
pub trait OmsSubstrate: SubstrateRead {
    /// Append a new grain; returns its content address.
    fn put_grain(&mut self, spec: &GrainSpec) -> Result<String>;

    /// Supersede `target_hash` with a new grain carrying `justification`;
    /// returns the new grain's address. Atomic and distinct from put
    /// (OMS §28.4).
    fn supersede(
        &mut self,
        target_hash: &str,
        spec: &GrainSpec,
        justification: &str,
    ) -> Result<String>;

    /// Index-layer retraction (`verification_status = retracted`) — the
    /// inverse of an applied ADD, used by rollback. Not destructive (the grain
    /// stays content-addressed; only the index marks it retracted). The
    /// default reports it unsupported so substrates opt in.
    fn retract(&mut self, hash: &str, reason: &str) -> Result<()> {
        Err(crate::error::Error::Substrate(format!(
            "retract not supported by this substrate ({hash}: {reason})"
        )))
    }

    /// Store an opaque blob (candidate tool CODE, evalset payloads) in the
    /// substrate's CAS, returning its address. §7.4's blob seam —
    /// CAPABILITY-GATED: the default refuses, so a loop can only carry code
    /// on substrates that explicitly opt in. Code enters the substrate only
    /// through this seam or an authored add — never from a git mirror.
    fn put_blob(&mut self, bytes: &[u8]) -> Result<String> {
        let _ = bytes;
        Err(crate::error::Error::Substrate(
            "put_blob not supported by this substrate (code-carrying loops \
             need an opted-in blob seam)"
            .into(),
        ))
    }

    /// Fetch a blob by the address `put_blob` returned. Same capability gate.
    fn get_blob(&mut self, address: &str) -> Result<Vec<u8>> {
        Err(crate::error::Error::Substrate(format!(
            "get_blob not supported by this substrate ({address})"
        )))
    }

    /// Execute CAL text, returning result rows as JSON. Used to regenerate
    /// evidence sets (`evidence_query`) and to apply `proposal_cal`. A
    /// substrate MAY reject CAL it cannot run with [`Error::CalUnsupported`].
    ///
    /// [`Error::CalUnsupported`]: crate::error::Error::CalUnsupported
    fn execute_cal(&mut self, cal: &str) -> Result<Vec<Value>>;

    /// The CAL that would restore the CURRENT definition named by a
    /// `DEFINE QUERY` / `DEFINE TEMPLATE` statement — the rollback inverse,
    /// captured before the definition is replaced.
    ///
    /// Returns `Ok(None)` when the substrate cannot produce one, which the
    /// engine treats as "this definition rewrite is not applicable": a
    /// definition change with no recorded inverse is one that `ROLLBACK`
    /// would silently fail to undo, and a rollback that reports success
    /// without restoring anything is worse than a refusal.
    ///
    /// The default implementation returns `None`, so a substrate that does
    /// not model saved definitions simply cannot execute definition
    /// rewrites — fail closed, no opt-out needed.
    fn definition_inverse(&self, _statement: &str) -> Result<Option<String>> {
        Ok(None)
    }

    /// Validate a CAL batch without executing it (statement classification,
    /// destructive-op detection). Delegated to the substrate — the engine
    /// contains a CAL *writer*, never a parser.
    fn validate_cal(&self, cal: &str) -> Result<()>;

    /// Load the persisted loop state blob (config + watermarks/cooldowns).
    /// Returns `Value::Null` when nothing has been stored yet.
    fn load_state(&self) -> Result<Value>;

    /// Persist the loop state blob (a file-truth, so it travels with the
    /// file on sync).
    fn store_state(&mut self, state: &Value) -> Result<()>;
}
