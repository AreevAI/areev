//! In-file loop config + state — file-truths persisted through the
//! substrate's `load_state`/`store_state` as one JSON blob. Carries a schema
//! version; unknown keys are ignored (serde default), so an older binary opens
//! a newer file unchanged (proposal §7.3).

use crate::model::Severity;
use crate::recommendation::{MetricSnapshot, RecStatus};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::BTreeMap;

/// Current persisted-state schema version.
pub const SCHEMA_VERSION: u32 = 1;

/// The whole loop persisted blob.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoopPersisted {
    #[serde(default = "default_schema_version")]
    pub schema_version: u32,
    /// Per-analyzer config, keyed by full analyzer id.
    #[serde(default)]
    pub config: BTreeMap<String, AnalyzerConfig>,
    #[serde(default)]
    pub state: LoopState,
    /// Rebuildable lifecycle cache: recommendation hash → status.
    #[serde(default)]
    pub status_index: BTreeMap<String, RecStatus>,
    /// Per-recommendation latest audit hash, for hash-chaining.
    #[serde(default)]
    pub audit_heads: BTreeMap<String, String>,
    /// The creating actor per recommendation (for the self-approval block).
    #[serde(default)]
    pub creators: BTreeMap<String, String>,
    /// The principal that triggered the run which stored an LLM or
    /// external-command recommendation — the self-approval block fires
    /// against these too (the trigger must not approve their own model's
    /// output). Omitted when empty: deterministic-only histories never
    /// carry the key.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub co_creators: BTreeMap<String, String>,
    /// Rejection cooldowns keyed by dedup_key → cooldown-until epoch-ms.
    #[serde(default)]
    pub cooldowns: BTreeMap<String, i64>,
    /// How many times each dedup_key has been rejected — drives the exponential
    /// backoff of `cooldowns` (7d, 14d, 28d, …) so a repeatedly-rejected finding
    /// stops re-surfacing on a fixed cadence. Omitted when empty (no churn for
    /// states without a rejection).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub cooldown_strikes: BTreeMap<String, u32>,
    /// Applied-recommendation records (inverse plan, metric, timing).
    #[serde(default)]
    pub applied: BTreeMap<String, AppliedRecord>,
    /// Per-recommendation set of horizons (ms after apply) already measured, so
    /// each checkpoint is measured exactly once.
    #[serde(default)]
    pub measured: BTreeMap<String, Vec<i64>>,
    /// Measured outcome time series (the Verify gate's output), keyed by
    /// recommendation — one entry per horizon checkpoint.
    #[serde(default)]
    pub outcomes: BTreeMap<String, Vec<crate::recommendation::OutcomeResult>>,
}

fn default_schema_version() -> u32 {
    SCHEMA_VERSION
}

impl Default for LoopPersisted {
    fn default() -> Self {
        LoopPersisted {
            schema_version: SCHEMA_VERSION,
            config: BTreeMap::new(),
            state: LoopState::default(),
            status_index: BTreeMap::new(),
            audit_heads: BTreeMap::new(),
            creators: BTreeMap::new(),
            co_creators: BTreeMap::new(),
            cooldowns: BTreeMap::new(),
            cooldown_strikes: BTreeMap::new(),
            applied: BTreeMap::new(),
            measured: BTreeMap::new(),
            outcomes: BTreeMap::new(),
        }
    }
}

impl LoopPersisted {
    /// Decode from the substrate state blob; `Value::Null` (nothing stored) →
    /// defaults.
    pub fn from_value(v: Value) -> crate::error::Result<Self> {
        if v.is_null() {
            return Ok(Self::default());
        }
        serde_json::from_value(v)
            .map_err(|e| crate::error::Error::Internal(format!("decode loop state: {e}")))
    }

    pub fn to_value(&self) -> crate::error::Result<Value> {
        serde_json::to_value(self)
            .map_err(|e| crate::error::Error::Internal(format!("encode loop state: {e}")))
    }
}

/// Per-analyzer configuration. The file may enable/disable, raise severity
/// floors, override params, and scope namespaces — never raise engine caps.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AnalyzerConfig {
    /// `None` = follow the manifest default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    #[serde(default)]
    pub params: Map<String, Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub severity_floor: Option<Severity>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub namespaces: Vec<String>,
}

/// A partial update to one analyzer's [`AnalyzerConfig`] — every field absent
/// (`None`/`false`) leaves the stored value untouched, so the console can PATCH
/// a single toggle. Deserialized straight from the `POST /api/loop/config`
/// body.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct AnalyzerConfigUpdate {
    /// Enable/disable the analyzer. `None` leaves it as-is.
    #[serde(default)]
    pub enabled: Option<bool>,
    /// Set the severity floor. `None` leaves it as-is; to CLEAR an existing
    /// floor, send `clear_floor: true` instead.
    #[serde(default)]
    pub severity_floor: Option<Severity>,
    #[serde(default)]
    pub clear_floor: bool,
    /// Replace the param overrides (validated against the manifest before store).
    /// `None` leaves them as-is.
    #[serde(default)]
    pub params: Option<Map<String, Value>>,
    /// Replace the namespace scoping. `None` leaves it as-is; `Some([])` clears.
    #[serde(default)]
    pub namespaces: Option<Vec<String>>,
}

/// One analyzer's effective settings for the Setup view: the manifest facts plus
/// the resolved file-config (override or manifest default).
#[derive(Debug, Clone, Serialize)]
pub struct AnalyzerSetting {
    pub id: String,
    pub title: String,
    /// The manifest's one-line "what it does", so the Setup view can say what
    /// a toggle turns off without the reader having to know the analyzer.
    pub description: String,
    pub tier: String,
    pub trust_class: String,
    pub default_on: bool,
    /// The effective on/off state (file override, else the manifest default).
    pub enabled: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub severity_floor: Option<String>,
}

/// Run state: the watermark that makes repeat runs cheap no-ops.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LoopState {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_run_ms: Option<i64>,
    /// Highest grain `created_at` processed so far.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub watermark_ms: Option<i64>,
}

/// Record of an applied recommendation: how to undo it and what to re-measure.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppliedRecord {
    pub applied_at_ms: i64,
    pub target_ref: String,
    pub rollbackable: bool,
    /// Grain hashes created by the apply, retracted on rollback (ADD inverse).
    #[serde(default)]
    pub created_hashes: Vec<String>,
    /// CAL that undoes a change with no grain to retract.
    ///
    /// `created_hashes` is the inverse of an ADD: rollback retracts what the
    /// apply created. A `DEFINE QUERY` / `DEFINE TEMPLATE` creates no grain —
    /// it replaces a `qry:`/`tpl:` registry row — so retracting nothing would
    /// let a rollback report success while the new definition stayed live.
    /// This holds the statement that restores the previous definition (or
    /// `DROP` when there was none), captured at apply time from the state
    /// being replaced.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inverse_cal: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metric: Option<MetricSnapshot>,
}
