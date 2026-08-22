//! The Trigger grain (type 0x0D) — a standing rule that starts a workflow.
//!
//! ## Why this is its own type rather than an Observation
//!
//! OMS §27.6 proposed carrying triggers on Observation grains: `observer_type =
//! "trigger:*"`, with the configuration in the `context` map under `int:` keys.
//! Two things make that unworkable, and they are worth recording here so nobody
//! re-litigates it from the spec text alone.
//!
//! 1. **The convention is non-conformant against the spec's own closed enums.**
//!    §27.6 puts `"periodic"` / `"continuous"` / `"scheduled"` in
//!    `observation_mode`, which §25 declares closed over `passive | active |
//!    reflective | real_time`, and a watch target in `observation_scope`, which
//!    §26 declares closed over the *temporal breadth* values `point | interval |
//!    session | longitudinal`. This implementation defines a third vocabulary
//!    again (`realtime|batch|streaming`, `private|shared|public`). No two of the
//!    three agree.
//! 2. **Config in `context` cannot be queried.** Observation's
//!    `queryable_fields` does not include `context`, and the CAL evaluator's
//!    field lookup is top-level only — no nesting, no pointer traversal. So
//!    `RECALL observations WHERE int:connector = "gmail"` does not merely fail,
//!    it *fails open*: an unrecognized `WHERE` field is dropped from push-down
//!    with a warning, and the query silently returns everything. Moving the keys
//!    top-level would make them queryable but puts them outside `context`, which
//!    is where OMS A.7 says `int:` fields live.
//!
//! A trigger is also not a perception. §24 splits observers into Physical
//! ("measurements of the material world") and Cognitive ("observations of the
//! information space"); a standing rule is neither.
//!
//! So: typed fields, queryable and validated, with `config` retaining the `int:`
//! vocabulary for what it was actually designed for — connector transport
//! settings.
//!
//! ## What lives here and what does not
//!
//! The declaration is immutable and replicates. Evaluation state — next due
//! time, cursor, lease, fence — is deliberately **not** here: it is per-host
//! usage rather than definition, and a dev memory restored from prod must not
//! inherit prod's cursor and silently skip real work. That state lives in
//! non-replicating `trg:` rows in the store's `meta` table, following the same
//! reasoning that keeps a saved query's `last_run_at` local.

use std::collections::HashMap;

use super::grain::{Grain, GrainCommon, GrainType};

/// How a trigger decides it should fire.
///
/// Eight declared kinds over four irreducible primitives. `Interval`, `Schedule`
/// and `Once` are three generators for one primitive — they all reduce to
/// computing a next-due instant — and are kept distinct because the declarations
/// read differently, not because the evaluators do.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TriggerKind {
    /// Fixed interval. Anchors to the *last run*, so it drifts with runtime.
    #[default]
    Interval,
    /// Cron expression. Anchors to wall-clock boundaries, so it does not drift.
    Schedule,
    /// A single absolute instant, then never again.
    Once,
    /// Poll an external system through a connector, with a cursor.
    Polling,
    /// Fire when grains matching a predicate appear in this memory.
    Memory,
    /// The host receives a delivery and hands it to Areev. Areev opens no port.
    Webhook,
    /// Fired explicitly by an operator.
    Manual,
    /// A gate over other triggers: a boolean expression, optionally correlated
    /// and windowed.
    Composite,
}

impl TriggerKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            TriggerKind::Interval => "interval",
            TriggerKind::Schedule => "schedule",
            TriggerKind::Once => "once",
            TriggerKind::Polling => "polling",
            TriggerKind::Memory => "memory",
            TriggerKind::Webhook => "webhook",
            TriggerKind::Manual => "manual",
            TriggerKind::Composite => "composite",
        }
    }

    /// Parse a wire value. `None` for anything unknown — callers fall back to
    /// the default rather than failing a whole read, matching how every other
    /// enum on the wire behaves here.
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "interval" => TriggerKind::Interval,
            "schedule" => TriggerKind::Schedule,
            "once" => TriggerKind::Once,
            "polling" => TriggerKind::Polling,
            "memory" => TriggerKind::Memory,
            "webhook" => TriggerKind::Webhook,
            "manual" => TriggerKind::Manual,
            "composite" => TriggerKind::Composite,
            _ => return None,
        })
    }

    /// Whether this kind reaches an external system through a connector
    /// command. Drives whether a missing connector is an error.
    pub fn needs_connector(&self) -> bool {
        matches!(self, TriggerKind::Polling)
    }
}

/// What to do when a firing is still running and the next one comes due.
///
/// The Kubernetes CronJob vocabulary, which Argo and others adopted; Temporal's
/// six-value overlap policy is the superset if buffering is ever wanted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Concurrency {
    /// Skip the new firing while the previous one is still running.
    #[default]
    Forbid,
    Allow,
    /// Cancel the running firing and start the new one.
    Replace,
}

impl Concurrency {
    pub fn as_str(&self) -> &'static str {
        match self {
            Concurrency::Forbid => "forbid",
            Concurrency::Allow => "allow",
            Concurrency::Replace => "replace",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "forbid" => Concurrency::Forbid,
            "allow" => Concurrency::Allow,
            "replace" => Concurrency::Replace,
            _ => return None,
        })
    }
}

/// What to do about occurrences missed while nothing was evaluating.
///
/// Kestra's `recoverMissedSchedules` naming, which is the same three-way Quartz
/// reaches through misfire instructions: fire once now / skip / replay all.
/// `Last` is the default for the reason anacron defaults to it — a laptop closed
/// over a weekend should wake to one firing, not four hundred.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Catchup {
    /// Collapse every missed occurrence into a single firing.
    #[default]
    Last,
    /// Drop missed occurrences entirely.
    None,
    /// Replay every missed occurrence.
    All,
}

impl Catchup {
    pub fn as_str(&self) -> &'static str {
        match self {
            Catchup::Last => "last",
            Catchup::None => "none",
            Catchup::All => "all",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "last" => Catchup::Last,
            "none" => Catchup::None,
            "all" => Catchup::All,
            _ => return None,
        })
    }
}

/// A Trigger grain — a standing rule that starts a Workflow.
#[derive(Debug, Clone)]
pub struct Trigger {
    /// How this trigger decides it is due.
    pub kind: TriggerKind,
    /// Content address of the Workflow grain to start. Required: a trigger that
    /// starts nothing is a declaration with no meaning.
    ///
    /// The binding points trigger → plan, never the reverse. A Workflow is
    /// content-addressed and a run's manifest pins its hash, so a plan that grew
    /// a list of triggers would change address every time a trigger was added
    /// and orphan its own run history.
    pub workflow: String,
    /// Connector name — `"gmail"`, `"stripe"`. Half of the firing identity.
    pub connector: Option<String>,
    /// What is watched, in the connector's own vocabulary:
    /// `"mailbox:accounts@example.com"`.
    pub scope: Option<String>,
    /// Whether the trigger is live. Omitted when true, so the common case costs
    /// nothing on the wire.
    pub enabled: bool,
    /// JSON pointer naming the field that identifies an item — `/message_id`.
    /// Several pointers joined in order form a composite key, which is how a
    /// "changed" semantic is expressed over a source that only reports
    /// "created" (`/id` + `/updated_at`).
    pub dedup_key: Vec<String>,
    /// Seconds between firings, for `Interval` and `Polling`.
    pub interval_secs: Option<u32>,
    /// Cron expression, for `Schedule`. UTC only in this release.
    pub cron: Option<String>,
    /// Absolute epoch-ms instant, for `Once`.
    pub at_ms: Option<i64>,
    /// A serialized CAL `Condition` tree. For `Memory` it selects grains; for
    /// `Composite` it composes member trigger ids. Stored as a structure rather
    /// than an expression string, which is what keeps this out of the frozen
    /// edge-condition grammar's standing exclusion on expression languages.
    pub predicate: Option<serde_json::Value>,
    /// Member triggers for a `Composite`, as **alias → content address**.
    ///
    /// Aliases exist because a gate expression references its members by name,
    /// and a 64-hex content address is not a legal identifier in any expression
    /// grammar — CAL's lexer reads it as a number. Argo Events reached the same
    /// shape: `dependencies` carry names and `conditions` reference the names.
    /// Aliases also make an expression readable (`invoice AND purchase_order`)
    /// and survive a member being re-declared at a new address.
    pub members: HashMap<String, String>,
    /// JSON pointer whose value correlates member firings — `/thread_id`.
    /// Partial matches are keyed by `(trigger, correlation value)`, which is what
    /// stops Monday's A pairing with Tuesday's B.
    pub correlate: Option<String>,
    /// How long a partial composite match stays live.
    pub window_ms: Option<i64>,
    pub concurrency: Concurrency,
    pub catchup: Catchup,
    /// Connector transport configuration, under the OMS A.7 `int:` vocabulary
    /// (`int:cursor_field`, `int:webhook_path`, `int:timezone`, …). Inner keys
    /// are never compacted, so arbitrary connector settings round-trip verbatim.
    pub config: Option<serde_json::Value>,
    /// A saved query (`qry:<name>`) the EVALUATOR runs when this trigger
    /// fires; its result rides into the run input as `context`. This is how
    /// a trigger-started run carries assembled context on the embedded
    /// backend, where a tool cannot open the memory its own run holds (#85):
    /// the evaluator already holds it, and the declaration replicates with
    /// the trigger — auditable, not host-local. Omit-default: absent means
    /// no context assembly, exactly the pre-1.5 behaviour.
    pub context_query: Option<String>,
    pub common: GrainCommon,
}

impl Trigger {
    /// A trigger of `kind` that starts `workflow`.
    pub fn new(kind: TriggerKind, workflow: &str) -> Self {
        Trigger {
            kind,
            workflow: workflow.to_string(),
            connector: None,
            scope: None,
            enabled: true,
            dedup_key: Vec::new(),
            interval_secs: None,
            cron: None,
            at_ms: None,
            predicate: None,
            members: HashMap::new(),
            correlate: None,
            window_ms: None,
            concurrency: Concurrency::default(),
            catchup: Catchup::default(),
            config: None,
            context_query: None,
            common: GrainCommon { confidence: 1.0, ..Default::default() },
        }
    }

    pub fn connector(mut self, connector: &str) -> Self {
        self.connector = Some(connector.to_string());
        self
    }

    pub fn scope(mut self, scope: &str) -> Self {
        self.scope = Some(scope.to_string());
        self
    }

    pub fn enabled(mut self, enabled: bool) -> Self {
        self.enabled = enabled;
        self
    }

    pub fn dedup_key(mut self, pointer: &str) -> Self {
        self.dedup_key.push(pointer.to_string());
        self
    }

    pub fn interval_secs(mut self, secs: u32) -> Self {
        self.interval_secs = Some(secs);
        self
    }

    pub fn cron(mut self, expr: &str) -> Self {
        self.cron = Some(expr.to_string());
        self
    }

    pub fn at_ms(mut self, at: i64) -> Self {
        self.at_ms = Some(at);
        self
    }

    pub fn predicate(mut self, predicate: serde_json::Value) -> Self {
        self.predicate = Some(predicate);
        self
    }

    /// Declare a member under an alias the gate expression can name.
    pub fn member(mut self, alias: &str, hash: &str) -> Self {
        self.members.insert(alias.to_string(), hash.to_string());
        self
    }

    /// The content address behind an alias.
    pub fn member_hash(&self, alias: &str) -> Option<&str> {
        self.members.get(alias).map(String::as_str)
    }

    /// The alias a member hash was declared under.
    pub fn member_alias(&self, hash: &str) -> Option<&str> {
        self.members.iter().find(|(_, h)| h.as_str() == hash).map(|(a, _)| a.as_str())
    }

    pub fn correlate(mut self, pointer: &str) -> Self {
        self.correlate = Some(pointer.to_string());
        self
    }

    pub fn window_ms(mut self, ms: i64) -> Self {
        self.window_ms = Some(ms);
        self
    }

    pub fn concurrency(mut self, c: Concurrency) -> Self {
        self.concurrency = c;
        self
    }

    pub fn catchup(mut self, c: Catchup) -> Self {
        self.catchup = c;
        self
    }

    /// Name the saved query whose result becomes the run input's `context`.
    pub fn context_query(mut self, name: &str) -> Self {
        self.context_query = Some(name.to_string());
        self
    }

    pub fn config(mut self, config: serde_json::Value) -> Self {
        self.config = Some(config);
        self
    }

    /// Read one `int:` key out of `config`.
    pub fn config_str(&self, key: &str) -> Option<&str> {
        self.config.as_ref()?.get(key)?.as_str()
    }

    /// Whether the declaration is internally coherent — checked at write time,
    /// so a trigger that could never fire is refused when it is authored rather
    /// than discovered as silence weeks later.
    ///
    /// Returns the reason it is not, or `None` if it is fine.
    pub fn incoherence(&self) -> Option<String> {
        if self.workflow.trim().is_empty() {
            return Some("a trigger must name the workflow it starts".into());
        }
        match self.kind {
            TriggerKind::Interval | TriggerKind::Polling => {
                if self.interval_secs.is_none_or(|s| s == 0) {
                    return Some(format!(
                        "a {} trigger needs a non-zero interval_secs",
                        self.kind.as_str()
                    ));
                }
            }
            TriggerKind::Schedule => {
                if self.cron.as_ref().is_none_or(|c| c.trim().is_empty()) {
                    return Some("a schedule trigger needs a cron expression".into());
                }
            }
            TriggerKind::Once => {
                if self.at_ms.is_none() {
                    return Some("a once trigger needs at_ms".into());
                }
            }
            TriggerKind::Memory => {
                if self.predicate.is_none() {
                    return Some("a memory trigger needs a predicate".into());
                }
            }
            TriggerKind::Composite => {
                if self.members.len() < 2 {
                    return Some("a composite trigger needs at least two members".into());
                }
                if self.predicate.is_none() {
                    return Some("a composite trigger needs a predicate over its members".into());
                }
            }
            TriggerKind::Webhook | TriggerKind::Manual => {}
        }
        if self.kind.needs_connector() && self.connector.as_ref().is_none_or(|c| c.trim().is_empty())
        {
            return Some(format!(
                "a {} trigger needs a connector name",
                self.kind.as_str()
            ));
        }
        None
    }
}

impl Grain for Trigger {
    fn grain_type(&self) -> GrainType {
        GrainType::Trigger
    }

    fn common(&self) -> &GrainCommon {
        &self.common
    }

    fn common_mut(&mut self) -> &mut GrainCommon {
        &mut self.common
    }

    fn text(&self) -> String {
        // For embedding/indexing: what it watches and how, in words a person
        // would use to search for it.
        let mut parts = vec![self.kind.as_str().to_string()];
        if let Some(ref c) = self.connector {
            parts.push(c.clone());
        }
        if let Some(ref s) = self.scope {
            parts.push(s.clone());
        }
        if let Some(ref c) = self.cron {
            parts.push(c.clone());
        }
        if let Some(ref q) = self.context_query {
            parts.push(q.clone());
        }
        parts.join(" ")
    }
}

/// Connector transport keys registered in the OMS A.7 Integration profile that
/// this implementation reads. Kept together so the `config` contract is one
/// place rather than scattered string literals.
pub mod config_keys {
    pub const CURSOR_FIELD: &str = "int:cursor_field";
    pub const CURSOR_TYPE: &str = "int:cursor_type";
    pub const WEBHOOK_PATH: &str = "int:webhook_path";
    pub const WEBHOOK_SECRET_HEADER: &str = "int:webhook_secret_header";
    pub const TIMEZONE: &str = "int:timezone";
    pub const CONFIG_SCHEMA: &str = "int:config_schema";
    pub const EVENT_SCHEMA: &str = "int:event_schema";
    pub const ALLOWED_OUTBOUND_HOSTS: &str = "int:allowed_outbound_hosts";
}

/// Reverse map from a `Trigger` back to the `HashMap` shape the JSON builders
/// use. Kept next to the struct so a new field is obvious in both directions.
impl Trigger {
    pub fn to_field_map(&self) -> HashMap<String, serde_json::Value> {
        let mut m = HashMap::new();
        m.insert("kind".into(), serde_json::json!(self.kind.as_str()));
        m.insert("workflow".into(), serde_json::json!(self.workflow));
        if let Some(ref c) = self.connector {
            m.insert("connector".into(), serde_json::json!(c));
        }
        if let Some(ref s) = self.scope {
            m.insert("scope".into(), serde_json::json!(s));
        }
        if !self.enabled {
            m.insert("enabled".into(), serde_json::json!(false));
        }
        if let Some(ref q) = self.context_query {
            m.insert("context_query".into(), serde_json::json!(q));
        }
        m
    }
}
