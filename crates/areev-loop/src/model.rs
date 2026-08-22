//! Shared value types: grain records the engine reads, target references it
//! proposes against, severity, action kinds, and provenance origin.
//!
//! Text normalization note: grains arriving from an OMS substrate are already
//! NFC-normalized by canonical serialization (a frozen OMS invariant), so the
//! engine only case-folds and trims for identity comparisons — it deliberately
//! carries no Unicode-normalization dependency (dependency-light policy).

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// Canonical grain-type names as the substrate reports them.
pub mod grain_type {
    pub const FACT: &str = "fact";
    pub const EVENT: &str = "event";
    /// OMS Tool grain (0x05) — how a captured tool call is stored, carrying
    /// `tool_name`/`is_error`/`content` natively (the flagship analyzer's food).
    pub const TOOL: &str = "tool";
    pub const OBSERVATION: &str = "observation";
    /// OMS Skill grain (0x0B) — `proficiency` (aliases `confidence`) +
    /// `practice_count`; the chain is the learning history.
    pub const SKILL: &str = "skill";
    /// OMS Goal grain (0x07) — `goal_state` + `progress`.
    pub const GOAL: &str = "goal";
    pub const RECOMMENDATION: &str = "recommendation";
}

/// A grain as read from the substrate: the content address, the OMS type name,
/// index-layer facts (`superseded_by`), and the decoded field map. The engine
/// treats fields as JSON and pulls typed values through the accessors below.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GrainRecord {
    pub hash: String,
    pub grain_type: String,
    #[serde(default)]
    pub namespace: String,
    #[serde(default)]
    pub created_at_ms: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub valid_to_ms: Option<i64>,
    /// Index-layer supersession pointer; `Some` means this grain is not a head.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub superseded_by: Option<String>,
    #[serde(default)]
    pub fields: Map<String, Value>,
}

impl GrainRecord {
    /// A grain is *live* when no supersession has retired it.
    pub fn is_live(&self) -> bool {
        self.superseded_by.is_none()
    }

    pub fn str_field(&self, key: &str) -> Option<&str> {
        self.fields.get(key).and_then(Value::as_str)
    }

    pub fn bool_field(&self, key: &str) -> Option<bool> {
        self.fields.get(key).and_then(Value::as_bool)
    }

    // --- Fact accessors (subject/relation/object) ---
    pub fn fact_subject(&self) -> Option<&str> {
        self.str_field("subject")
    }
    pub fn fact_relation(&self) -> Option<&str> {
        self.str_field("relation")
    }
    pub fn fact_object(&self) -> Option<&str> {
        self.str_field("object")
    }

    // --- Tool-grain accessors (captured tool calls / results) ---
    pub fn tool_name(&self) -> Option<&str> {
        self.str_field("tool_name")
            .or_else(|| self.str_field("name"))
    }
    pub fn is_error(&self) -> bool {
        self.bool_field("is_error").unwrap_or(false)
    }
    /// The tool result text used for error-signature extraction. Tool grains
    /// carry it as `tool_content` (compact `cnt`, distinct from Event's
    /// uncompacted `content`); other shapes fall back.
    pub fn tool_content(&self) -> Option<&str> {
        self.str_field("tool_content")
            .or_else(|| self.str_field("content"))
            .or_else(|| self.str_field("result"))
            .or_else(|| self.str_field("error"))
            .or_else(|| self.str_field("body"))
    }

    fn f64_field(&self, key: &str) -> Option<f64> {
        self.fields.get(key).and_then(Value::as_f64)
    }
    fn i64_field(&self, key: &str) -> Option<i64> {
        self.fields.get(key).and_then(Value::as_i64)
    }

    // --- Skill accessors ---
    pub fn skill_name(&self) -> Option<&str> {
        self.str_field("name").or_else(|| self.str_field("skill_name"))
    }
    /// Skill proficiency (dedicated `proficiency` key; aliases `confidence`).
    pub fn skill_proficiency(&self) -> Option<f64> {
        self.f64_field("proficiency").or_else(|| self.f64_field("confidence"))
    }
    pub fn skill_practice_count(&self) -> i64 {
        self.i64_field("practice_count").unwrap_or(0)
    }

    // --- Goal accessors ---
    pub fn goal_state(&self) -> Option<&str> {
        self.str_field("goal_state").or_else(|| self.str_field("state"))
    }
    pub fn goal_progress(&self) -> f64 {
        self.f64_field("progress").unwrap_or(0.0)
    }
}

/// Severity ranks proposals for review triage. Ordering is
/// `Info < Low < Medium < High`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Info,
    Low,
    Medium,
    High,
}

impl Severity {
    pub fn as_str(&self) -> &'static str {
        match self {
            Severity::Info => "info",
            Severity::Low => "low",
            Severity::Medium => "medium",
            Severity::High => "high",
        }
    }
}

/// Provenance of a recommendation, engine-stamped (never settable by an
/// analyzer draft). Drives the trust class and the auto-apply gate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum Origin {
    /// A built-in or statically-linked Rust analyzer (same trust class).
    Builtin,
    /// An external command analyzer, by id. Never auto-applies.
    Command { id: String },
    /// An LLM DISCOVER/ENRICH draft, by model. Never auto-applies; never
    /// touches prompt/host targets.
    Llm { model: String },
}

impl Origin {
    /// Only builtin (incl. statically-linked) origins are eligible for
    /// auto-apply; command and llm origins never are (trust floor, §6.3).
    pub fn auto_apply_eligible(&self) -> bool {
        matches!(self, Origin::Builtin)
    }
}

/// The kind of change a proposal makes. Combined with analyzer-family and
/// target_ref it forms the dedup identity — so it must be a stable, small
/// vocabulary, not free text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActionKind {
    /// Supersede duplicate members with one consolidated grain.
    Consolidate,
    /// Flag a subject holding contradictory values under a functional relation.
    FlagContradiction,
    /// Record a recurring tool-failure cluster as a lesson.
    ClusterFailure,
    /// Tombstone a grain whose declared validity has elapsed.
    Expire,
    /// Merge multiple heads of one entity.
    MergeHeads,
    /// Revert an applied recommendation whose outcome regressed.
    Revert,
    /// Surface an advisory finding for human attention (no automatic fix) —
    /// e.g. a stalled skill or goal.
    Flag,
    /// Propose a new revision of an executable tool's code (§7.4). NEVER
    /// auto-applied — categorically, by name — and the recommendation must
    /// pin the evalset hash it was gated against (Rule E1): apply is refused
    /// without a recorded gating run, and a superseded evalset invalidates
    /// the pin (the recommendation re-gates).
    CodeRevision,
    /// Propose promoting a host-trained adapter into service for a `model:`
    /// target (the tuning seam). Same governance shape as `CodeRevision`:
    /// NEVER auto-applied, must pin the evalset it was gated against
    /// (Rule E1), and apply is refused without a recorded gating run.
    AdapterRevision,
}

impl ActionKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            ActionKind::Consolidate => "consolidate",
            ActionKind::FlagContradiction => "flag_contradiction",
            ActionKind::ClusterFailure => "cluster_failure",
            ActionKind::Expire => "expire",
            ActionKind::MergeHeads => "merge_heads",
            ActionKind::Revert => "revert",
            ActionKind::Flag => "flag",
            ActionKind::CodeRevision => "code_revision",
            ActionKind::AdapterRevision => "adapter_revision",
        }
    }
}

/// A parsed `target_ref`: `<scheme>:<opaque>`. The scheme is the target-kind
/// discriminator (proposal §7.1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TargetRef {
    scheme: String,
    opaque: String,
}

impl TargetRef {
    /// Parse `<scheme>:<opaque>`. Fails if there is no scheme or empty opaque.
    pub fn parse(s: &str) -> crate::error::Result<Self> {
        let (scheme, opaque) = s.split_once(':').ok_or_else(|| {
            crate::error::Error::InvalidTargetRef(format!("missing scheme in {s:?}"))
        })?;
        if scheme.is_empty() || opaque.is_empty() {
            return Err(crate::error::Error::InvalidTargetRef(format!(
                "empty scheme or opaque in {s:?}"
            )));
        }
        if !KNOWN_SCHEMES.contains(&scheme) {
            return Err(crate::error::Error::InvalidTargetRef(format!(
                "unknown scheme {scheme:?} in {s:?}"
            )));
        }
        Ok(TargetRef {
            scheme: scheme.to_string(),
            opaque: opaque.to_string(),
        })
    }

    pub fn scheme(&self) -> &str {
        &self.scheme
    }
    pub fn opaque(&self) -> &str {
        &self.opaque
    }

    /// Memory/query targets are the only classes eligible for auto-apply
    /// (§6.3); prompt (`doc:`), `host:`, the §7.4 `tool:`/`evalset:` classes,
    /// and the tuning seam's `model:` class are never auto-applied — code,
    /// adapters, and their gate are excluded BY NAME, not by accident of the
    /// grant vocabulary.
    pub fn auto_apply_eligible_class(&self) -> bool {
        matches!(
            self.scheme.as_str(),
            "grain" | "entity" | "query" | "template"
        )
    }

    /// The policy target class: `memory` (grain/entity), `query`
    /// (query/template), `prompt` (doc), `code` (tool), `evalset`, or `host`.
    pub fn target_class(&self) -> &'static str {
        match self.scheme.as_str() {
            "grain" | "entity" => "memory",
            "query" | "template" => "query",
            "doc" => "prompt",
            "tool" => "code",
            "evalset" => "evalset",
            "model" => "model",
            _ => "host",
        }
    }

    pub fn as_string(&self) -> String {
        format!("{}:{}", self.scheme, self.opaque)
    }
}

const KNOWN_SCHEMES: &[&str] = &[
    "grain", "entity", "query", "template", "doc", "host", "tool", "evalset", "model",
];

/// Case-fold + trim for identity comparison. Upstream NFC is assumed.
pub(crate) fn normalize_ident(s: &str) -> String {
    s.trim().to_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn severity_orders() {
        assert!(Severity::High > Severity::Medium);
        assert!(Severity::Low > Severity::Info);
    }

    #[test]
    fn target_ref_parses_known_schemes() {
        let t = TargetRef::parse("entity:caller/john").unwrap();
        assert_eq!(t.scheme(), "entity");
        assert_eq!(t.opaque(), "caller/john");
        assert!(t.auto_apply_eligible_class());

        let doc = TargetRef::parse("doc:claude.md").unwrap();
        assert!(
            !doc.auto_apply_eligible_class(),
            "prompt targets never auto-apply"
        );
    }

    #[test]
    fn target_ref_rejects_junk() {
        assert!(TargetRef::parse("no-scheme").is_err());
        assert!(TargetRef::parse("bogus:x").is_err());
        assert!(TargetRef::parse("grain:").is_err());
    }

    #[test]
    fn origin_auto_apply_gate() {
        assert!(Origin::Builtin.auto_apply_eligible());
        assert!(!Origin::Command { id: "x".into() }.auto_apply_eligible());
        assert!(!Origin::Llm { model: "m".into() }.auto_apply_eligible());
    }
}
