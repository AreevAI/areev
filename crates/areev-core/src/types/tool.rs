use std::fmt;

use serde::{Deserialize, Serialize};

use super::grain::{Grain, GrainCommon, GrainType};
use crate::types::executor_kind::ExecutorKind;

/// Phase distinction for an Tool grain.
///
/// - `Definition`: a tool catalog entry. Carries `input_schema`,
///   `executor_uri`, `locked_params`, `description`, `annotations`,
///   `examples`. Lives in memory-scoped `harnesses/<slug>/def`.
/// - `Execution`: a tool invocation record. Carries `input`, `content`,
///   `is_error`, `error`, `duration_ms`, `tool_call_id`. Lives in
///   user-scoped `users/<uid>/...` (crypto-erased on `forget_user`).
///
/// On deserialize, when the field is absent, defaults to `Execution`
/// for backward-compatibility (most existing Tool grains were
/// execution records before Phase 1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum ToolKind {
    Definition,
    #[default]
    Execution,
}

impl ToolKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            ToolKind::Definition => "definition",
            ToolKind::Execution => "execution",
        }
    }

    /// Parse a kind from its lowercase wire string (`"definition"` or
    /// `"execution"`). Returns `None` for unknown values — callers
    /// typically fall back to the `Default::default()` (`Execution`).
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "definition" => Some(ToolKind::Definition),
            "execution" => Some(ToolKind::Execution),
            _ => None,
        }
    }
}

/// Lifecycle status of an Execution-kind Tool grain.
///
/// `None` on the wire reads as [`ExecutionStatus::Completed`] —
/// every pre-Phase-2 execution grain was implicitly completed at
/// commit time. New async paths stamp `Pending`, then supersede with
/// `Completed` or `Failed` on resolution.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ExecutionStatus {
    Pending,
    Completed,
    Failed,
}

impl ExecutionStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Completed => "completed",
            Self::Failed => "failed",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "pending" => Some(Self::Pending),
            "completed" => Some(Self::Completed),
            "failed" => Some(Self::Failed),
            _ => None,
        }
    }
}

/// Typed failure classifier for `Tool { kind: Execution }`.
///
/// Pairs with the free-text `failure_detail` (user-scoped, contractual
/// "no PII"). Aggregating on this enum lets compliance + ops dashboards
/// bucket failures without parsing strings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureCause {
    Timeout,
    ExecutorError,
    SchemaValidationFailed,
    UserAborted,
    Unknown,
}

impl FailureCause {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Timeout => "timeout",
            Self::ExecutorError => "executor_error",
            Self::SchemaValidationFailed => "schema_validation_failed",
            Self::UserAborted => "user_aborted",
            Self::Unknown => "unknown",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "timeout" => Some(Self::Timeout),
            "executor_error" => Some(Self::ExecutorError),
            "schema_validation_failed" => Some(Self::SchemaValidationFailed),
            "user_aborted" => Some(Self::UserAborted),
            "unknown" => Some(Self::Unknown),
            _ => None,
        }
    }
}

/// Execution-environment classification for EU AI Act Art. 12/14 evidence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActorExecutionEnvironment {
    ServerSideHost,
    /// v1: reserved for future server-set attestation. Client-supplied
    /// values are coerced to `ClientSideUnattested` at the request
    /// boundary. Server cannot trust this variant in v1.
    ClientSideAttested,
    ClientSideUnattested,
}

impl ActorExecutionEnvironment {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::ServerSideHost => "server_side_host",
            Self::ClientSideAttested => "client_side_attested",
            Self::ClientSideUnattested => "client_side_unattested",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "server_side_host" => Some(Self::ServerSideHost),
            "client_side_attested" => Some(Self::ClientSideAttested),
            "client_side_unattested" => Some(Self::ClientSideUnattested),
            _ => None,
        }
    }
}

/// MCP-compatible tool annotations describing side-effect properties.
///
/// All flags default to `false`. The combination
/// `read_only && destructive` is rejected at write time
/// (`SchemaSubsetError` / `MEM-E103`) — a tool cannot be both read-only
/// and destructive.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[cfg_attr(feature = "http", derive(utoipa::ToSchema))]
pub struct ToolAnnotations {
    #[serde(default)]
    pub read_only: bool,
    #[serde(default)]
    pub destructive: bool,
    #[serde(default)]
    pub idempotent: bool,
}

/// An Tool grain — tool definition (catalog entry) or tool execution
/// record (invocation result).
///
/// See [`ToolKind`] for the `kind` discriminator. Fields are partitioned
/// by kind in conventional usage but the struct holds both — readers must
/// inspect `kind` to know which fields are meaningful.
#[derive(Clone)]
pub struct Tool {
    /// Discriminator: definition vs execution. Default `Execution`.
    pub kind: ToolKind,

    /// Canonical tool id (e.g. `slack.post_message`). Required.
    /// Must match `^[a-zA-Z0-9_.-]{1,64}$` when written via `bind_tool`.
    pub tool_name: String,

    // ── Execution-record fields ────────────────────────────────────────
    /// Arguments the LLM provided (or locked params merged in).
    pub input: Option<serde_json::Value>,
    /// Result text or human description (definition).
    pub content: Option<String>,
    pub is_error: Option<bool>,
    pub error: Option<String>,
    pub duration_ms: Option<u64>,
    pub parent_task_id: Option<String>,
    /// LLM tool-call id this Tool is the result of.
    pub tool_call_id: Option<String>,
    /// Batch id if multiple parallel tool calls share a response turn.
    pub call_batch_id: Option<String>,

    // ── Definition fields (Phase 1 promotion from extra_fields) ────────
    /// Long-form description shown to the LLM in the tool list.
    pub tool_description: Option<String>,
    /// JSON Schema for arguments the LLM may fill. OMS 1.2 `input_schema`.
    /// Validated at write time via `SchemaValidator::tool_schema()`.
    pub input_schema: Option<serde_json::Value>,
    /// JSON Schema for the tool's return value. OMS 1.3 `output_schema`.
    pub output_schema: Option<serde_json::Value>,
    /// Whether the LLM provider should run in "strict" schema mode.
    pub strict: Option<bool>,
    /// Opaque host executor reference (e.g. `executor://slack.post_message@v3`)
    /// used by the invoker to look up endpoint + auth at invoke time.
    /// **Must not be logged at INFO+ — see `Debug` impl below (SR-F5).**
    pub executor_uri: Option<String>,
    /// Parameters the LLM must NOT choose for itself; merged over LLM
    /// input at invoke time. Object-shaped JSON.
    pub locked_params: Option<serde_json::Value>,
    /// Example argument objects shown in the builder UI.
    pub examples: Option<Vec<serde_json::Value>>,
    /// MCP-compatible side-effect annotations.
    pub annotations: Option<ToolAnnotations>,
    /// Hash of the upstream executor spec at bind time. Stale-binding
    /// detection.
    pub spec_hash: Option<String>,
    /// HPL Phase 4.1 — runtime executor for this tool. Absent on
    /// pre-HPL grains; deserialize defaults to `Host` so legacy
    /// bindings continue to run inline without a migration. `Client`
    /// bindings cause the Flow-A loop to pause and return a
    /// `requires_action` envelope to the caller.
    pub executor_kind: Option<ExecutorKind>,
    /// Definition-only flag — when `Some(true)`, `invoke_tool`
    /// dispatches asynchronously: write a Pending Execution grain,
    /// acquire a permit, return `{status: pending, correlation_id}`,
    /// and resolve via the `/triggers/tool-result/callback` sink.
    /// Absent / `Some(false)` keeps the synchronous path.
    pub async_mode: Option<bool>,

    // ── Async execution lifecycle ─────────────────────────────────────
    /// Lifecycle status for Execution grains. `None` reads as
    /// [`ExecutionStatus::Completed`] for backward-compat.
    pub status: Option<ExecutionStatus>,
    /// Per-Pending opaque correlator (hex). Derivation lives in the
    /// triggers module; present only on async execution paths.
    pub correlation_id: Option<String>,
    /// Epoch seconds when a Pending grain auto-fails with `Timeout`.
    pub expires_at_sec: Option<i64>,
    /// HMAC-derived hash of an overlay (request-scoped, transient) tool
    /// schema, present only when the execution was driven by such a
    /// tool. Crypto-erases with the user when their key is destroyed.
    pub transient_definition_hash: Option<[u8; 32]>,
    /// Typed failure classifier; pairs with `failure_detail`.
    pub failure_cause: Option<FailureCause>,
    /// Free-text complement to `failure_cause`. User-scoped, crypto-
    /// erased on `forget_user`. Contractual "no PII" — see SDK README.
    pub failure_detail: Option<String>,
    /// Execution environment evidence for EU AI Act Art. 12/14.
    pub actor_execution_environment: Option<ActorExecutionEnvironment>,

    pub common: GrainCommon,
}

impl Tool {
    pub fn new(tool_name: &str) -> Self {
        Tool {
            kind: ToolKind::Execution,
            tool_name: tool_name.to_string(),
            input: None,
            content: None,
            is_error: None,
            error: None,
            duration_ms: None,
            parent_task_id: None,
            tool_call_id: None,
            call_batch_id: None,
            tool_description: None,
            input_schema: None,
            output_schema: None,
            strict: None,
            executor_uri: None,
            locked_params: None,
            examples: None,
            annotations: None,
            spec_hash: None,
            executor_kind: None,
            async_mode: None,
            status: None,
            correlation_id: None,
            expires_at_sec: None,
            transient_definition_hash: None,
            failure_cause: None,
            failure_detail: None,
            actor_execution_environment: None,
            common: GrainCommon {
                confidence: 1.0,
                ..Default::default()
            },
        }
    }

    /// Set the executor classification. HPL Phase 4.1 — callers
    /// typically parse this from `executor_uri` at bind time.
    pub fn executor_kind(mut self, kind: ExecutorKind) -> Self {
        self.executor_kind = Some(kind);
        self
    }

    pub fn kind(mut self, kind: ToolKind) -> Self {
        self.kind = kind;
        self
    }

    pub fn input(mut self, args: serde_json::Value) -> Self {
        self.input = Some(args);
        self
    }

    pub fn input_str(mut self, args: &str) -> Self {
        self.input = Some(serde_json::Value::String(args.to_string()));
        self
    }

    pub fn content(mut self, result: &str) -> Self {
        self.content = Some(result.to_string());
        self
    }

    pub fn is_error(mut self, is_error: bool) -> Self {
        self.is_error = Some(is_error);
        self
    }

    pub fn error(mut self, error: &str) -> Self {
        self.error = Some(error.to_string());
        self
    }

    pub fn duration_ms(mut self, ms: u64) -> Self {
        self.duration_ms = Some(ms);
        self
    }

    pub fn parent_task_id(mut self, id: &str) -> Self {
        self.parent_task_id = Some(id.to_string());
        self
    }

    pub fn tool_call_id(mut self, id: &str) -> Self {
        self.tool_call_id = Some(id.to_string());
        self
    }

    pub fn call_batch_id(mut self, id: &str) -> Self {
        self.call_batch_id = Some(id.to_string());
        self
    }

    pub fn tool_description(mut self, desc: &str) -> Self {
        self.tool_description = Some(desc.to_string());
        self
    }

    pub fn input_schema(mut self, schema: serde_json::Value) -> Self {
        self.input_schema = Some(schema);
        self
    }

    /// OMS 1.3: Set the output schema (JSON Schema for the action's return value).
    pub fn output_schema(mut self, schema: serde_json::Value) -> Self {
        self.output_schema = Some(schema);
        self
    }

    pub fn strict(mut self, strict: bool) -> Self {
        self.strict = Some(strict);
        self
    }

    pub fn executor_uri(mut self, uri: &str) -> Self {
        self.executor_uri = Some(uri.to_string());
        self
    }

    pub fn locked_params(mut self, params: serde_json::Value) -> Self {
        self.locked_params = Some(params);
        self
    }

    pub fn examples(mut self, examples: Vec<serde_json::Value>) -> Self {
        self.examples = Some(examples);
        self
    }

    pub fn annotations(mut self, annotations: ToolAnnotations) -> Self {
        self.annotations = Some(annotations);
        self
    }

    pub fn spec_hash(mut self, hash: &str) -> Self {
        self.spec_hash = Some(hash.to_string());
        self
    }
}

/// Custom `Debug` redacting `executor_uri` to `<redacted>` (SR-F5).
///
/// Auto-derived `Debug` would dump the URI cleartext into any
/// `tracing::debug!("{action:?}")` call. The URI carries host-executor-internal
/// routing / spec versioning that should not surface in INFO+ logs.
impl fmt::Debug for Tool {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Tool")
            .field("kind", &self.kind)
            .field("tool_name", &self.tool_name)
            .field("input", &self.input)
            .field("content", &self.content)
            .field("is_error", &self.is_error)
            .field("error", &self.error)
            .field("duration_ms", &self.duration_ms)
            .field("parent_task_id", &self.parent_task_id)
            .field("tool_call_id", &self.tool_call_id)
            .field("call_batch_id", &self.call_batch_id)
            .field("tool_description", &self.tool_description)
            .field("input_schema", &self.input_schema)
            .field("output_schema", &self.output_schema)
            .field("strict", &self.strict)
            .field(
                "executor_uri",
                &self.executor_uri.as_ref().map(|_| "<redacted>"),
            )
            .field("locked_params", &self.locked_params)
            .field("examples", &self.examples)
            .field("annotations", &self.annotations)
            .field("spec_hash", &self.spec_hash)
            .field("executor_kind", &self.executor_kind)
            .field("async_mode", &self.async_mode)
            .field("status", &self.status)
            .field("correlation_id", &self.correlation_id)
            .field("expires_at_sec", &self.expires_at_sec)
            .field(
                "transient_definition_hash",
                &self.transient_definition_hash.as_ref().map(hex::encode),
            )
            .field("failure_cause", &self.failure_cause)
            .field("failure_detail", &self.failure_detail)
            .field(
                "actor_execution_environment",
                &self.actor_execution_environment,
            )
            .field("common", &self.common)
            .finish()
    }
}

impl Grain for Tool {
    fn grain_type(&self) -> GrainType {
        GrainType::Tool
    }

    fn common(&self) -> &GrainCommon {
        &self.common
    }

    fn common_mut(&mut self) -> &mut GrainCommon {
        &mut self.common
    }

    fn text(&self) -> String {
        match &self.content {
            Some(r) => format!("{} {}", self.tool_name, r),
            None => self.tool_name.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_kind_is_execution() {
        let a = Tool::new("calculator");
        assert_eq!(a.kind, ToolKind::Execution);
    }

    #[test]
    fn kind_builder_sets_definition() {
        let a = Tool::new("calculator").kind(ToolKind::Definition);
        assert_eq!(a.kind, ToolKind::Definition);
    }

    #[test]
    fn debug_redacts_executor_uri() {
        let a = Tool::new("slack.post_message").executor_uri("executor://slack.post_message@v3");
        let s = format!("{a:?}");
        assert!(
            !s.contains("executor://"),
            "executor_uri must not appear in Debug output: {s}"
        );
        assert!(s.contains("<redacted>"));
    }

    #[test]
    fn debug_omits_redaction_when_uri_absent() {
        let a = Tool::new("calculator");
        let s = format!("{a:?}");
        // executor_uri: None → no redacted marker
        assert!(s.contains("executor_uri: None"));
    }

    #[test]
    fn annotations_default_all_false() {
        let a = ToolAnnotations::default();
        assert!(!a.read_only);
        assert!(!a.destructive);
        assert!(!a.idempotent);
    }

    #[test]
    fn action_kind_round_trip_serde() {
        let json = serde_json::to_string(&ToolKind::Definition).unwrap();
        assert_eq!(json, "\"definition\"");
        let back: ToolKind = serde_json::from_str(&json).unwrap();
        assert_eq!(back, ToolKind::Definition);
    }

    #[test]
    fn executor_kind_defaults_to_none_on_new() {
        // A freshly constructed Tool has `executor_kind = None`; the
        // Flow-A dispatch classifier treats None as Host for legacy.
        let a = Tool::new("calc");
        assert!(a.executor_kind.is_none());
    }

    #[test]
    fn executor_kind_builder_stamps_value() {
        let a = Tool::new("calc").executor_kind(ExecutorKind::Client);
        assert_eq!(a.executor_kind, Some(ExecutorKind::Client));
    }

    // ── Serde back-compat + round-trip ──

    #[test]
    fn action_with_none_status_reads_as_completed_for_backward_compat() {
        use crate::format::deserialize::deserialize_blob;
        use crate::format::serialize::serialize_grain;
        // Construct without `status` (mirrors pre-Phase-2 wire shape).
        let action = Tool::new("calc").content("42").created_at(1);
        let (blob, _) = serialize_grain(&action).unwrap();
        let dg = deserialize_blob(&blob).unwrap();
        assert!(
            dg.get_str("status").is_none(),
            "status must be omitted from the wire when None"
        );
        let back = dg.to_tool().unwrap();
        let effective = back.status.unwrap_or(ExecutionStatus::Completed);
        assert_eq!(effective, ExecutionStatus::Completed);
    }

    #[test]
    fn action_with_all_new_fields_round_trips_via_canonical_msgpack() {
        use crate::format::deserialize::deserialize_blob;
        use crate::format::serialize::serialize_grain;
        let tdh = [7u8; 32];
        let mut action = Tool::new("flaky.tool").created_at(1);
        action.status = Some(ExecutionStatus::Pending);
        action.correlation_id = Some("deadbeef".to_string());
        action.expires_at_sec = Some(1_700_000_000);
        action.transient_definition_hash = Some(tdh);
        action.failure_cause = Some(FailureCause::Timeout);
        action.failure_detail = Some("executor 504 after 30s".to_string());
        action.actor_execution_environment = Some(ActorExecutionEnvironment::ClientSideUnattested);
        let (blob, _) = serialize_grain(&action).unwrap();
        let back = deserialize_blob(&blob).unwrap().to_tool().unwrap();
        assert_eq!(back.status, Some(ExecutionStatus::Pending));
        assert_eq!(back.correlation_id.as_deref(), Some("deadbeef"));
        assert_eq!(back.expires_at_sec, Some(1_700_000_000));
        assert_eq!(back.transient_definition_hash, Some(tdh));
        assert_eq!(back.failure_cause, Some(FailureCause::Timeout));
        assert_eq!(back.failure_detail.as_deref(), Some("executor 504 after 30s"));
        assert_eq!(
            back.actor_execution_environment,
            Some(ActorExecutionEnvironment::ClientSideUnattested)
        );
    }

    #[test]
    fn action_default_execution_excludes_new_pending_fields_from_wire_when_none() {
        use crate::format::deserialize::deserialize_blob;
        use crate::format::serialize::serialize_grain;
        let action = Tool::new("calc").content("ok").created_at(1);
        let (blob, _) = serialize_grain(&action).unwrap();
        let dg = deserialize_blob(&blob).unwrap();
        for expanded in [
            "status",
            "correlation_id",
            "expires_at_sec",
            "transient_definition_hash",
            "failure_cause",
            "failure_detail",
            "actor_execution_environment",
        ] {
            assert!(
                !dg.fields.contains_key(expanded),
                "field '{expanded}' must be omitted from the wire when None"
            );
        }
    }

    #[test]
    fn failure_cause_enum_round_trip() {
        for v in [
            FailureCause::Timeout,
            FailureCause::ExecutorError,
            FailureCause::SchemaValidationFailed,
            FailureCause::UserAborted,
            FailureCause::Unknown,
        ] {
            let json = serde_json::to_string(&v).unwrap();
            let back: FailureCause = serde_json::from_str(&json).unwrap();
            assert_eq!(back, v);
            assert_eq!(FailureCause::parse(v.as_str()), Some(v));
        }
    }

    #[test]
    fn actor_execution_environment_enum_round_trip() {
        for v in [
            ActorExecutionEnvironment::ServerSideHost,
            ActorExecutionEnvironment::ClientSideAttested,
            ActorExecutionEnvironment::ClientSideUnattested,
        ] {
            let json = serde_json::to_string(&v).unwrap();
            let back: ActorExecutionEnvironment = serde_json::from_str(&json).unwrap();
            assert_eq!(back, v);
            assert_eq!(ActorExecutionEnvironment::parse(v.as_str()), Some(v));
        }
    }
}
