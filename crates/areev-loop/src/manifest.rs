//! Analyzer manifests and the flat `ParamSpec` list. One manifest is the
//! single source feeding the CLI listing, the HTTP `/analyzers` route, MCP
//! listing, param validation, docs, and the console's analyzer cards
//! (proposal §11). Hand-rolled with serde_json only — no JSON-Schema dep.

use crate::error::{Error, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// Analysis tier: T0 pure statistics, T1 embedding-assisted, T2 LLM.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Tier {
    T0,
    T1,
    T2,
}

/// How often an analyzer is worth running; rides per-analyzer time budgets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CadenceClass {
    /// Cheap; safe to run every pass.
    Fast,
    /// Moderate; the default batch cadence.
    Batch,
    /// Expensive; run sparingly.
    Slow,
}

/// Optional substrate capability an analyzer may require.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Capability {
    Forks,
    Telemetry,
    Embeddings,
}

/// Target classes an analyzer may propose against.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TargetClass {
    Memory,
    Query,
    Prompt,
    Host,
}

/// Whether an analyzer's structural proposals are *eligible* for auto-apply.
/// Eligibility is necessary, never sufficient — the engine still requires host
/// opt-in, policy allowlisting, and per-draft shape verification (§6.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AutoApplyClass {
    /// Never auto-applies (the default and the only safe class for anything
    /// carrying evidence-derived text).
    Never,
    /// Structural curation with zero attacker-influenced free text; still
    /// re-verified per draft by the engine.
    StructuralCuration,
}

/// Trust class of the analyzer's code path (drives the console badge).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TrustClass {
    /// Built-in or statically linked — compiling it in *is* the trust decision.
    Builtin,
    /// External command analyzer; never auto-applies.
    Command,
    /// LLM-drafted; never auto-applies, never prompt/host targets.
    Llm,
}

/// One declared parameter. Six kinds, hand-rolled (proposal §11).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ParamSpec {
    Bool {
        name: String,
        default: bool,
        description: String,
    },
    Int {
        name: String,
        default: i64,
        min: i64,
        max: i64,
        description: String,
    },
    Float {
        name: String,
        default: f64,
        min: f64,
        max: f64,
        description: String,
    },
    Str {
        name: String,
        default: String,
        max_len: usize,
        description: String,
    },
    Enum {
        name: String,
        default: String,
        choices: Vec<String>,
        description: String,
    },
    /// A duration in whole seconds.
    Duration {
        name: String,
        default_secs: i64,
        description: String,
    },
}

impl ParamSpec {
    pub fn name(&self) -> &str {
        match self {
            ParamSpec::Bool { name, .. }
            | ParamSpec::Int { name, .. }
            | ParamSpec::Float { name, .. }
            | ParamSpec::Str { name, .. }
            | ParamSpec::Enum { name, .. }
            | ParamSpec::Duration { name, .. } => name,
        }
    }

    fn default_value(&self) -> Value {
        match self {
            ParamSpec::Bool { default, .. } => Value::Bool(*default),
            ParamSpec::Int { default, .. } => Value::from(*default),
            ParamSpec::Float { default, .. } => Value::from(*default),
            ParamSpec::Str { default, .. } => Value::from(default.clone()),
            ParamSpec::Enum { default, .. } => Value::from(default.clone()),
            ParamSpec::Duration { default_secs, .. } => Value::from(*default_secs),
        }
    }

    /// Validate a supplied override value against this spec.
    fn validate(&self, v: &Value) -> Result<()> {
        let bad = |m: String| Err(Error::ParamInvalid(format!("{}: {m}", self.name())));
        match self {
            ParamSpec::Bool { .. } => {
                if !v.is_boolean() {
                    return bad(format!("expected bool, got {v}"));
                }
            }
            ParamSpec::Int { min, max, .. } => {
                let n = v.as_i64().ok_or_else(|| {
                    Error::ParamInvalid(format!("{}: expected integer, got {v}", self.name()))
                })?;
                if n < *min || n > *max {
                    return bad(format!("{n} out of range [{min}, {max}]"));
                }
            }
            ParamSpec::Float { min, max, .. } => {
                let n = v.as_f64().ok_or_else(|| {
                    Error::ParamInvalid(format!("{}: expected number, got {v}", self.name()))
                })?;
                if n < *min || n > *max {
                    return bad(format!("{n} out of range [{min}, {max}]"));
                }
            }
            ParamSpec::Str { max_len, .. } => {
                let s = v.as_str().ok_or_else(|| {
                    Error::ParamInvalid(format!("{}: expected string, got {v}", self.name()))
                })?;
                if s.chars().count() > *max_len {
                    return bad(format!(
                        "length {} exceeds max {max_len}",
                        s.chars().count()
                    ));
                }
            }
            ParamSpec::Enum { choices, .. } => {
                let s = v.as_str().ok_or_else(|| {
                    Error::ParamInvalid(format!("{}: expected string, got {v}", self.name()))
                })?;
                if !choices.iter().any(|c| c == s) {
                    return bad(format!("{s:?} not in {choices:?}"));
                }
            }
            ParamSpec::Duration { .. } => {
                let n = v.as_i64().ok_or_else(|| {
                    Error::ParamInvalid(format!(
                        "{}: expected integer seconds, got {v}",
                        self.name()
                    ))
                })?;
                if n < 0 {
                    return bad(format!("duration {n} must be non-negative"));
                }
            }
        }
        Ok(())
    }
}

/// A fully resolved parameter set: manifest defaults with validated overrides
/// applied. Typed getters never panic — an absent key means the manifest is
/// out of sync, which is an engine bug, so they fall back to the type zero.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Params(Map<String, Value>);

impl Params {
    pub fn get_bool(&self, name: &str) -> bool {
        self.0.get(name).and_then(Value::as_bool).unwrap_or(false)
    }
    pub fn get_int(&self, name: &str) -> i64 {
        self.0.get(name).and_then(Value::as_i64).unwrap_or(0)
    }
    pub fn get_float(&self, name: &str) -> f64 {
        self.0.get(name).and_then(Value::as_f64).unwrap_or(0.0)
    }
    pub fn get_str(&self, name: &str) -> &str {
        self.0.get(name).and_then(Value::as_str).unwrap_or("")
    }
    pub fn get_duration_secs(&self, name: &str) -> i64 {
        self.get_int(name)
    }
    /// The raw snapshot, stored on each recommendation for full "why"
    /// provenance.
    pub fn snapshot(&self) -> Map<String, Value> {
        self.0.clone()
    }
}

/// An analyzer's self-description.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AnalyzerManifest {
    /// Versioned logic id `publisher.name/major`, e.g. `loop.duplicate_sweep/1`.
    pub id: String,
    pub title: String,
    pub description: String,
    pub tier: Tier,
    pub cadence: CadenceClass,
    #[serde(default)]
    pub requires: Vec<Capability>,
    pub target_classes: Vec<TargetClass>,
    pub auto_apply: AutoApplyClass,
    pub trust_class: TrustClass,
    #[serde(default)]
    pub params: Vec<ParamSpec>,
    /// Whether this analyzer is on by default (decided by measured precision,
    /// never by assertion — proposal §8).
    pub default_on: bool,
}

impl AnalyzerManifest {
    /// The dedup family: `publisher.name` with the `/major` suffix stripped.
    /// Keeping major out of the family is why an analyzer upgrade does not
    /// re-propose its whole queue as novel (proposal §7.1).
    pub fn family(&self) -> &str {
        analyzer_family(&self.id)
    }

    /// Resolve defaults + validated overrides into a `Params`. Unknown
    /// override keys are rejected (fail-closed, like the trust-floor schema).
    pub fn resolve_params(&self, overrides: &Map<String, Value>) -> Result<Params> {
        for key in overrides.keys() {
            if !self.params.iter().any(|p| p.name() == key) {
                return Err(Error::ParamInvalid(format!("unknown parameter {key:?}")));
            }
        }
        let mut resolved = Map::new();
        for spec in &self.params {
            let value = match overrides.get(spec.name()) {
                Some(v) => {
                    spec.validate(v)?;
                    v.clone()
                }
                None => spec.default_value(),
            };
            resolved.insert(spec.name().to_string(), value);
        }
        Ok(Params(resolved))
    }
}

/// `publisher.name/major` → `publisher.name`.
pub fn analyzer_family(id: &str) -> &str {
    id.split('/').next().unwrap_or(id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn spec_manifest() -> AnalyzerManifest {
        AnalyzerManifest {
            id: "loop.duplicate_sweep/1".into(),
            title: "Duplicate sweep".into(),
            description: "d".into(),
            tier: Tier::T0,
            cadence: CadenceClass::Batch,
            requires: vec![],
            target_classes: vec![TargetClass::Memory],
            auto_apply: AutoApplyClass::StructuralCuration,
            trust_class: TrustClass::Builtin,
            params: vec![
                ParamSpec::Float {
                    name: "jaccard".into(),
                    default: 0.9,
                    min: 0.0,
                    max: 1.0,
                    description: "near-dup threshold".into(),
                },
                ParamSpec::Int {
                    name: "window_days".into(),
                    default: 30,
                    min: 1,
                    max: 365,
                    description: "lookback".into(),
                },
            ],
            default_on: true,
        }
    }

    #[test]
    fn family_strips_major() {
        assert_eq!(
            analyzer_family("loop.duplicate_sweep/1"),
            "loop.duplicate_sweep"
        );
        assert_eq!(
            analyzer_family("loop.duplicate_sweep/2"),
            "loop.duplicate_sweep"
        );
        assert_eq!(spec_manifest().family(), "loop.duplicate_sweep");
    }

    #[test]
    fn resolve_fills_defaults() {
        let p = spec_manifest().resolve_params(&Map::new()).unwrap();
        assert_eq!(p.get_float("jaccard"), 0.9);
        assert_eq!(p.get_int("window_days"), 30);
    }

    #[test]
    fn resolve_validates_overrides() {
        let m = spec_manifest();
        let mut ov = Map::new();
        ov.insert("jaccard".into(), json!(1.5));
        assert!(
            m.resolve_params(&ov).is_err(),
            "out-of-range float rejected"
        );

        let mut ok = Map::new();
        ok.insert("jaccard".into(), json!(0.95));
        assert_eq!(m.resolve_params(&ok).unwrap().get_float("jaccard"), 0.95);
    }

    #[test]
    fn resolve_rejects_unknown_keys() {
        let mut ov = Map::new();
        ov.insert("bogus".into(), json!(1));
        assert!(spec_manifest().resolve_params(&ov).is_err());
    }
}
