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
//!     can **never auto-apply** and never target prompt/host surfaces. A draft
//!     MAY author a `lesson` (one capped imperative line); a lesson-bearing
//!     draft that survives GROUND + VERIFY stamps as an *applicable*,
//!     rollbackable `ADD fact` proposal instead of an advisory flag — still
//!     `origin = llm`, so applying it always takes a human review with a
//!     BECAUSE plus an explicit apply; and
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
use serde_json::Value;

/// Caps that bound what a single LLM contribution can inject (defense in depth;
/// the engine enforces them after parsing).
pub const MAX_LLM_DRAFTS: usize = 8;
pub const MAX_GUIDANCE_LEN: usize = 600;
pub const MAX_SUMMARY_LEN: usize = 200;
/// An authored lesson is one imperative line — anything longer is a document,
/// not a lesson, and a bound on what a single approved apply can put into
/// every future prompt.
pub const MAX_LESSON_LEN: usize = 240;
/// Caps on the rest of the proposal vocabulary. Each one bounds what a single
/// approved apply can put into every future run of the agent, so they are part
/// of the trust floor rather than tuning knobs.
pub const MAX_RELATION_LEN: usize = 64;
pub const MAX_OBJECT_LEN: usize = 480;
pub const MAX_QUERY_BODY_LEN: usize = 2_000;
pub const MAX_PLAN_EDITS: usize = 8;
pub const MAX_CODE_LEN: usize = 20_000;

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
    /// Optional authored lesson: one imperative rule the model proposes to
    /// record as a Fact grain. Empty (the default) keeps the draft advisory.
    /// A non-empty lesson makes the surviving recommendation *applicable* —
    /// through human review + apply only, never auto-apply — and the lesson
    /// text is folded into the GROUND claim and VERIFY summary so both gates
    /// judge exactly what an apply would write.
    pub lesson: String,
    /// The generalized proposal vocabulary (§9.1): what change this draft asks
    /// a reviewer to make. Held as raw JSON so one draft naming an unknown or
    /// malformed `kind` degrades to advisory instead of dropping the whole
    /// response — read it through [`LlmDraft::parsed_proposal`].
    pub proposal: Option<Value>,
}

/// One field-level edit to a Workflow plan grain. `from` is a staleness check
/// (it must equal what the live plan holds at `path`), which is what stops a
/// proposal authored against a superseded plan from applying to a newer one —
/// the role `base_digest` plays on [`super::recommendation::Proposal::Edit`].
#[derive(Debug, Clone, Deserialize, Default, PartialEq)]
#[serde(default)]
pub struct PlanEdit {
    /// A dotted path into the plan body, e.g. `edges.2.max_cycles`. The
    /// engine's allowlist decides which paths are editable at all.
    pub path: String,
    pub from: Value,
    pub to: Value,
}

/// What a DISCOVER draft proposes to change. Closed vocabulary: every variant
/// maps onto an apply path that already records an inverse, and anything the
/// model returns outside it leaves the draft advisory.
///
/// Note what each variant does NOT carry. The subject of a `Fact`, the name of
/// a `QueryRevision`, the hash of a `PlanRevision` and the tool of a
/// `CodeRevision` all come from the draft's `target`, and the evalset a
/// `CodeRevision` is gated against comes from the substrate — so the model
/// names the change but never names its own scope or its own grader.
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DraftProposal {
    /// One imperative line recorded as a Fact with `relation = "lesson"`.
    /// The pre-vocabulary shape, and still the default one.
    Lesson {
        #[serde(default)]
        lesson: String,
    },
    /// A durable fact under a model-chosen relation — the "stop making a
    /// person re-supply this every time" proposal.
    Fact {
        #[serde(default)]
        relation: String,
        #[serde(default)]
        object: String,
    },
    /// A rewrite of the saved CAL query or template named by the target: the
    /// agent changing how it assembles its own context.
    QueryRevision {
        #[serde(default)]
        body: String,
    },
    /// Field-level edits to the Workflow plan named by the target. Node
    /// topology is not expressible here by construction — only the paths the
    /// engine's allowlist admits.
    PlanRevision {
        #[serde(default)]
        edits: Vec<PlanEdit>,
    },
    /// New source for the executable tool named by the target. Applies only
    /// through §7.4's recorded evalset-run edge (Rule E1).
    CodeRevision {
        #[serde(default)]
        source: String,
    },
}

impl LlmDraft {
    /// The proposal this draft makes, or `None` when it is advisory.
    ///
    /// An explicit `proposal` decides on its own: if it names an unknown kind
    /// or fails to parse, the draft is advisory rather than being quietly
    /// re-read as something the model did not ask for. `lesson` is the
    /// fallback only when no `proposal` was sent at all, which is what keeps
    /// every transcript recorded before the vocabulary existed parsing — and
    /// therefore keeps the published runs comparable.
    pub fn parsed_proposal(&self) -> Option<DraftProposal> {
        if let Some(v) = &self.proposal {
            return serde_json::from_value::<DraftProposal>(v.clone()).ok();
        }
        if self.lesson.trim().is_empty() {
            None
        } else {
            Some(DraftProposal::Lesson {
                lesson: self.lesson.clone(),
            })
        }
    }
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
        let out = crate::proc::run_argv(&self.argv, request, Some(crate::proc::DEFAULT_TIMEOUT))
            .map_err(|e| Error::LlmBackend(format!("spawn --llm-cmd {:?}: {e}", self.argv[0])))?;
        if let Some(why) = out.failure("--llm-cmd") {
            return Err(Error::LlmBackend(why));
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
    fn lesson_desugars_when_no_proposal_is_sent() {
        // Every transcript recorded before the vocabulary existed must still
        // resolve to the same change, or the published runs stop being
        // comparable with anything measured after it.
        let r = parse_discover(
            r#"{"recommendations":[{"summary":"s","target":"entity:a/b","evidence":["h"],"lesson":"Do the thing"}]}"#,
        );
        assert_eq!(
            r.recommendations[0].parsed_proposal(),
            Some(DraftProposal::Lesson { lesson: "Do the thing".into() })
        );
    }

    #[test]
    fn an_explicit_proposal_wins_over_lesson() {
        let r = parse_discover(
            r#"{"recommendations":[{"summary":"s","target":"entity:a/b","evidence":["h"],
                "lesson":"ignored","proposal":{"kind":"fact","relation":"alias_of","object":"Cobalt Cloud"}}]}"#,
        );
        assert_eq!(
            r.recommendations[0].parsed_proposal(),
            Some(DraftProposal::Fact {
                relation: "alias_of".into(),
                object: "Cobalt Cloud".into()
            })
        );
    }

    #[test]
    fn an_unparseable_proposal_is_advisory_not_reinterpreted() {
        // A garbled proposal must NOT fall back to the lesson: applying an
        // `ADD fact` when the model asked for something we could not read is
        // doing something it never proposed.
        for body in [
            r#""proposal":{"kind":"teleport","x":1}"#,
            r#""proposal":{"kind":"plan_revision","edits":"not-a-list"}"#,
            r#""proposal":42"#,
        ] {
            let raw = format!(
                r#"{{"recommendations":[{{"summary":"s","target":"entity:a/b","evidence":["h"],"lesson":"L",{body}}}]}}"#
            );
            let r = parse_discover(&raw);
            assert_eq!(r.recommendations.len(), 1, "{body}");
            assert_eq!(r.recommendations[0].parsed_proposal(), None, "{body}");
        }
    }

    #[test]
    fn one_bad_proposal_does_not_drop_its_siblings() {
        // Per-draft tolerance: the whole response surviving is what keeps a
        // single malformed kind from silently costing a run its findings.
        let r = parse_discover(
            r#"{"recommendations":[
                {"summary":"a","target":"entity:a/b","evidence":["h"],"proposal":{"kind":"nope"}},
                {"summary":"b","target":"entity:a/c","evidence":["h"],"proposal":{"kind":"lesson","lesson":"Keep me"}}
            ]}"#,
        );
        assert_eq!(r.recommendations.len(), 2);
        assert_eq!(r.recommendations[0].parsed_proposal(), None);
        assert_eq!(
            r.recommendations[1].parsed_proposal(),
            Some(DraftProposal::Lesson { lesson: "Keep me".into() })
        );
    }

    #[test]
    fn plan_edits_carry_their_staleness_check() {
        let r = parse_discover(
            r#"{"recommendations":[{"summary":"s","target":"grain:abc","evidence":["h"],
                "proposal":{"kind":"plan_revision","edits":[{"path":"retries.fetch","from":null,"to":3}]}}]}"#,
        );
        let Some(DraftProposal::PlanRevision { edits }) =
            r.recommendations[0].parsed_proposal()
        else {
            panic!("expected a plan revision");
        };
        assert_eq!(edits.len(), 1);
        assert_eq!(edits[0].path, "retries.fetch");
        assert_eq!(edits[0].from, Value::Null);
        assert_eq!(edits[0].to, Value::from(3));
    }

    #[test]
    fn cap_respects_char_boundaries() {
        assert_eq!(cap("hello", 3), "hel");
        assert_eq!(cap("héllo", 2), "hé");
        assert_eq!(cap("hi", 5), "hi");
    }
}
