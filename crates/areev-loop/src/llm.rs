//! Optional LLM enrichment (proposal §9).
//!
//! The engine's deterministic output stays a pure function of `(store, params,
//! now)`. This layer is strictly **additive**: with a backend attached the
//! pipeline gains two optional stages —
//!
//! ```text
//! ANALYZE (deterministic) → DISCOVER (LLM) → ENRICH (LLM) → VALIDATE+DEDUP → STORE
//! ```
//!
//! and with no backend those stages are the identity function, so the no-LLM
//! path is byte-for-byte the deterministic path. The LLM can only:
//!   - **DISCOVER**: propose *new* draft recommendations, which enter through
//!     the ordinary candidate/dedup/store path stamped `origin = llm` — so they
//!     can **never auto-apply** and never target prompt/host surfaces; and
//!   - **ENRICH**: add a whitelisted `guidance` note to a deterministic
//!     recommendation. The engine-templated summary is always kept; the model
//!     never rewrites it.
//!
//! Trust floor (enforced by the engine, not the backend): responses are parsed
//! to a fixed schema (unknown fields dropped, strings capped), DISCOVER drafts
//! must cite evidence hashes present in the bundle, instructions never
//! interleave with evidence, and a failed/timed-out/garbled call drops the LLM
//! contribution for the run rather than failing it.
//!
//! `CommandLlm` mirrors the shipped `CommandEmbed`: whitespace-split argv (no
//! shell), one process per call, a JSON request on stdin and a JSON response on
//! stdout, and a construction-time probe that fails loud.

use crate::error::{Error, Result};
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::process::{Command, Stdio};

/// Caps that bound what a single LLM contribution can inject (defense in depth;
/// the engine enforces them after parsing).
pub const MAX_LLM_DRAFTS: usize = 8;
pub const MAX_GUIDANCE_LEN: usize = 600;
pub const MAX_SUMMARY_LEN: usize = 200;

/// A backend that answers one JSON request with one JSON response. Object-safe
/// so the engine can hold a `Box<dyn LlmBackend>`.
pub trait LlmBackend: Send + Sync {
    /// Model identifier, stamped as provenance on `origin = llm` grains.
    fn model(&self) -> &str;
    /// Run one request. `request` is a JSON string; the returned text is
    /// expected to be JSON and is validated by the caller.
    fn complete(&self, request: &str) -> Result<String>;
}

/// Boxed backends forward — lets decorators wrap `Box<dyn LlmBackend>`
/// without knowing the concrete type.
impl<T: LlmBackend + ?Sized> LlmBackend for Box<T> {
    fn model(&self) -> &str {
        (**self).model()
    }
    fn complete(&self, request: &str) -> Result<String> {
        (**self).complete(request)
    }
}

// ---- wire schema (request) -------------------------------------------------

/// One deterministic finding, handed to DISCOVER as context (never as an
/// instruction — see `LlmRequest`).
#[derive(Debug, Clone, Serialize)]
pub struct FindingBrief {
    pub analyzer: String,
    pub summary: String,
    pub target: String,
    pub severity: String,
}

/// One evidence grain, provenance-tagged.
#[derive(Debug, Clone, Serialize)]
pub struct EvidenceItem {
    pub hash: String,
    pub grain_type: String,
    pub text: String,
}

/// The request envelope. `op` selects the stage; `instructions` is a fixed
/// engine string kept in its own field so it never interleaves with evidence.
#[derive(Debug, Clone, Serialize)]
pub struct LlmRequest<'a> {
    #[serde(rename = "loop")]
    pub loop_proto: u8,
    pub op: &'a str,
    pub instructions: &'a str,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub findings: Vec<FindingBrief>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub evidence: Vec<EvidenceItem>,
    /// The operator's recent decisions — what they reject/approve — so the
    /// model learns this reviewer's taste. (Bounded by the engine.)
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub rejected: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub approved: Vec<String>,
}

// ---- wire schema (response) ------------------------------------------------

/// One DISCOVER draft as returned by the model. Unknown fields are dropped by
/// serde; the engine further validates (cite-check, caps, target class,
/// grounding, and independent verification before it is ever stored).
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub struct LlmDraft {
    pub summary: String,
    pub target: String,
    pub guidance: String,
    pub evidence: Vec<String>,
    /// The model's self-reported confidence 0.0–1.0 that this finding is both
    /// correct and materially useful (§5.1). Missing/garbled → 0.0 (rejected by
    /// the confidence floor), a safe default.
    pub confidence: f64,
}

/// The DISCOVER response.
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub struct DiscoverResponse {
    pub recommendations: Vec<LlmDraft>,
}

/// The ENRICH response: guidance keyed by target_ref of a deterministic rec.
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub struct EnrichResponse {
    /// `[{ "target": "...", "guidance": "..." }]`
    pub notes: Vec<EnrichNote>,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub struct EnrichNote {
    pub target: String,
    pub guidance: String,
}

// ---- verifier stages (§5.2 GROUND, §5.3 VERIFY) ----------------------------

/// GROUND request: for each candidate draft, does its cited evidence actually
/// *entail* the claim? Decompose-then-entail is asked of the model here; a
/// stronger deployment can swap a dedicated entailment checker behind the same
/// shape. Kept a separate op/call from DISCOVER (proposer ≠ grounder).
#[derive(Debug, Clone, Serialize)]
pub struct GroundRequest<'a> {
    #[serde(rename = "loop")]
    pub loop_proto: u8,
    pub op: &'a str, // "ground"
    pub instructions: &'a str,
    pub claims: Vec<GroundItem>,
}

#[derive(Debug, Clone, Serialize)]
pub struct GroundItem {
    pub id: usize,
    pub claim: String,
    pub evidence: Vec<EvidenceItem>,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub struct GroundResponse {
    pub results: Vec<GroundResult>,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub struct GroundResult {
    pub id: usize,
    pub supported: bool,
    pub reason: String,
}

/// VERIFY request: an **independent** adversarial pass (a separate call from the
/// proposer — the anti-Goodhart rule) that tries to refute each grounded draft
/// on novelty / reality / out-of-context grounds and returns keep/kill + a
/// calibrated confidence. Deterministic findings are passed as context so the
/// verifier can reject drafts that merely restate them.
#[derive(Debug, Clone, Serialize)]
pub struct VerifyRequest<'a> {
    #[serde(rename = "loop")]
    pub loop_proto: u8,
    pub op: &'a str, // "verify"
    pub instructions: &'a str,
    pub findings: Vec<VerifyItem>,
}

#[derive(Debug, Clone, Serialize)]
pub struct VerifyItem {
    pub id: usize,
    pub summary: String,
    pub target: String,
    pub evidence: Vec<EvidenceItem>,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub struct VerifyResponse {
    pub results: Vec<VerifyResult>,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub struct VerifyResult {
    pub id: usize,
    pub keep: bool,
    pub confidence: f64,
    pub reason: String,
}

/// The probe response.
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
struct ProbeResponse {
    model: String,
}

/// A subprocess LLM backend. One process per call; argv is whitespace-split
/// with no shell (identical rules to `CommandEmbed`).
pub struct CommandLlm {
    argv: Vec<String>,
    model: String,
}

impl CommandLlm {
    /// Construct and probe. The probe (`{"loop":1,"op":"probe"}`) must return
    /// JSON with a `model` (or one is supplied), so a misconfigured command
    /// fails at construction, not mid-run.
    pub fn new(cmd: &str, model: Option<&str>) -> Result<Self> {
        let argv: Vec<String> = cmd.split_whitespace().map(str::to_string).collect();
        if argv.is_empty() {
            return Err(Error::LlmBackend("--llm-cmd is empty".into()));
        }
        let mut me = CommandLlm {
            argv,
            model: model.unwrap_or("").to_string(),
        };
        let probe = me.run(r#"{"loop":1,"op":"probe"}"#)?;
        let parsed: ProbeResponse = serde_json::from_str(probe.trim()).map_err(|e| {
            Error::LlmBackend(format!("--llm-cmd probe did not return JSON with a model: {e}"))
        })?;
        if me.model.is_empty() {
            me.model = if parsed.model.is_empty() {
                "unspecified".to_string()
            } else {
                parsed.model
            };
        }
        Ok(me)
    }

    fn run(&self, request: &str) -> Result<String> {
        let mut child = Command::new(&self.argv[0])
            .args(&self.argv[1..])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .map_err(|e| Error::LlmBackend(format!("spawn --llm-cmd {:?}: {e}", self.argv[0])))?;
        {
            let mut stdin = child.stdin.take().expect("stdin piped");
            stdin
                .write_all(request.as_bytes())
                .map_err(|e| Error::LlmBackend(format!("write to --llm-cmd: {e}")))?;
        }
        let out = child
            .wait_with_output()
            .map_err(|e| Error::LlmBackend(format!("--llm-cmd wait: {e}")))?;
        if !out.status.success() {
            return Err(Error::LlmBackend(format!(
                "--llm-cmd exited with {}",
                out.status
            )));
        }
        String::from_utf8(out.stdout)
            .map_err(|e| Error::LlmBackend(format!("--llm-cmd stdout not UTF-8: {e}")))
    }
}

impl LlmBackend for CommandLlm {
    fn model(&self) -> &str {
        &self.model
    }
    fn complete(&self, request: &str) -> Result<String> {
        self.run(request)
    }
}

/// Parse a DISCOVER response, dropping anything malformed. Never errors on
/// model garbage — a bad response yields no drafts.
pub fn parse_discover(raw: &str) -> DiscoverResponse {
    serde_json::from_str(raw.trim()).unwrap_or_default()
}

/// Parse an ENRICH response, dropping anything malformed.
pub fn parse_enrich(raw: &str) -> EnrichResponse {
    serde_json::from_str(raw.trim()).unwrap_or_default()
}

/// Parse a GROUND response; garbage → no results (⇒ every draft is treated as
/// ungrounded and dropped, the safe default).
pub fn parse_ground(raw: &str) -> GroundResponse {
    serde_json::from_str(raw.trim()).unwrap_or_default()
}

/// Parse a VERIFY response; garbage → no results (⇒ every draft is dropped).
pub fn parse_verify(raw: &str) -> VerifyResponse {
    serde_json::from_str(raw.trim()).unwrap_or_default()
}

/// Truncate to a char cap without splitting a UTF-8 boundary.
pub fn cap(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        s.chars().take(max).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_discover_drops_garbage() {
        assert!(parse_discover("not json").recommendations.is_empty());
        let r = parse_discover(r#"{"recommendations":[{"summary":"s","target":"entity:x/y","evidence":["h1"],"junk":1}]}"#);
        assert_eq!(r.recommendations.len(), 1);
        assert_eq!(r.recommendations[0].summary, "s");
        assert_eq!(r.recommendations[0].evidence, vec!["h1"]);
    }

    #[test]
    fn parse_enrich_reads_notes() {
        let r = parse_enrich(r#"{"notes":[{"target":"entity:a/b","guidance":"g"}]}"#);
        assert_eq!(r.notes.len(), 1);
        assert_eq!(r.notes[0].guidance, "g");
    }

    #[test]
    fn cap_respects_char_boundaries() {
        assert_eq!(cap("hello", 3), "hel");
        assert_eq!(cap("héllo", 2), "hé");
        assert_eq!(cap("hi", 5), "hi");
    }
}
