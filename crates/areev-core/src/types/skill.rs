use serde::{Deserialize, Serialize};

use super::grain::{Grain, GrainCommon, GrainType};

/// One context-dependent strategy a held skill has learned (OMS 1.4 §8.11
/// learned-competence).
///
/// `workflow` MUST be a Workflow grain content address (Rule 4) — validated
/// on write by `validate_skill_refs`. `condition` and `description` are
/// free text and are included in the write-path PII/PHI scan (BC2).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillStrategy {
    /// When this strategy applies (free text, PII/PHI-scanned).
    pub condition: String,
    /// Workflow grain content address to run for this strategy.
    pub workflow: String,
    /// Optional human description (free text, PII/PHI-scanned).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// A Skill grain (OMS 1.4 §8.11, type byte `0x0B`) — a packaged, reusable
/// agent capability.
///
/// A skill is a *hybrid* type. A pure **definition** sets only the durable
/// definition fields (`instructions`, `when_to_use`, `allowed_tools`, …). A
/// **held instance** additionally sets the learned-competence fields
/// (`holder_did`, proficiency, `practice_count`, `strategies`, …) that
/// capture a particular agent's mastery. The distinction is presence-based
/// (`is_definition()`), not a discriminator enum (Rule 1).
///
/// `proficiency` is NOT a struct field — it aliases `common.confidence`
/// (D3). Read it via [`Skill::proficiency`] and set it via
/// [`Skill::with_proficiency`]; this makes the spec's "proficiency SHOULD
/// equal confidence" rule structurally impossible to violate.
///
/// `created_at` lives in [`GrainCommon`] (epoch ms) and is not duplicated
/// here.
#[derive(Debug, Clone)]
pub struct Skill {
    // ── Required ──
    /// Skill name, e.g. `"code_review"` (compaction key `skname`).
    pub name: String,
    /// Required human description. Serialized under the SHARED `desc` key
    /// (same as Goal).
    pub description: String,

    // ── Optional: definition (durable, shareable) ──
    /// Full how-to body (markdown). Excluded from `text()`/`embedding_text()`
    /// for embedding quality, but IS scanned for PII/PHI on write (BC2).
    pub instructions: Option<String>,
    /// When the skill should be applied (routing cue; feeds `text()`).
    pub when_to_use: Option<String>,
    /// Opaque version string, e.g. `"2.1.0"` — NOT an int.
    pub version: Option<String>,
    /// Tool definition content addresses this skill may use (SHOULD be Tool
    /// def CAs; not validated — spec is SHOULD).
    pub allowed_tools: Vec<String>,
    /// Resource grain content addresses.
    pub resources: Vec<String>,
    /// Skill grain content addresses this skill depends on (MUST be Skill
    /// CAs; validated on write).
    pub dependencies: Vec<String>,
    /// Input modalities (open enum, e.g. `"text"`, `"image"`).
    pub input_modalities: Vec<String>,
    /// Output modalities (open enum).
    pub output_modalities: Vec<String>,
    /// Domain (open enum, e.g. `"software_engineering"`).
    pub domain: Option<String>,

    // ── Optional: learned-competence (a particular agent's mastery) ──
    /// The agent that holds this skill instance (distinct from
    /// `common.author_did`). When set, this grain is a held instance and
    /// `common.user_id` MUST also be set (BC1, enforced on write).
    pub holder_did: Option<String>,
    // NOTE: proficiency is NOT a field — it aliases `common.confidence` (D3).
    /// Number of successful practice records folded into this instance.
    pub practice_count: Option<u32>,
    /// When the skill was last practiced (epoch ms).
    pub last_practiced_at: Option<i64>,
    /// Context-dependent learned strategies.
    pub strategies: Vec<SkillStrategy>,
    /// Whether this skill may be acquired by another agent (Rule 5).
    pub transferable: Option<bool>,

    pub common: GrainCommon,
}

impl Skill {
    /// Construct a pure skill definition. `confidence` defaults to `1.0`
    /// (the "fully-specified" sense for a definition, NOT a mastery level).
    pub fn new(name: &str, description: &str) -> Self {
        Skill {
            name: name.to_string(),
            description: description.to_string(),
            instructions: None,
            when_to_use: None,
            version: None,
            allowed_tools: Vec::new(),
            resources: Vec::new(),
            dependencies: Vec::new(),
            input_modalities: Vec::new(),
            output_modalities: Vec::new(),
            domain: None,
            holder_did: None,
            practice_count: None,
            last_practiced_at: None,
            strategies: Vec::new(),
            transferable: None,
            common: GrainCommon {
                confidence: 1.0,
                ..Default::default()
            },
        }
    }

    /// Set the full instructions body.
    pub fn instructions(mut self, instructions: &str) -> Self {
        self.instructions = Some(instructions.to_string());
        self
    }

    /// Set the when-to-use routing cue.
    pub fn when_to_use(mut self, when_to_use: &str) -> Self {
        self.when_to_use = Some(when_to_use.to_string());
        self
    }

    /// Set the opaque version string.
    pub fn version(mut self, version: &str) -> Self {
        self.version = Some(version.to_string());
        self
    }

    /// Set the domain.
    pub fn domain(mut self, domain: &str) -> Self {
        self.domain = Some(domain.to_string());
        self
    }

    /// Mark the holder DID (turns this into a held instance).
    pub fn holder_did(mut self, did: &str) -> Self {
        self.holder_did = Some(did.to_string());
        self
    }

    /// Mark the skill transferable (or not).
    pub fn transferable(mut self, transferable: bool) -> Self {
        self.transferable = Some(transferable);
        self
    }

    /// Set proficiency. The ONLY sanctioned way to set mastery — it writes
    /// `common.confidence` (D3) and clamps to `[0.0, 1.0]`.
    pub fn with_proficiency(mut self, proficiency: f64) -> Self {
        self.common.confidence = proficiency.clamp(0.0, 1.0);
        self
    }

    /// True when this grain is a pure definition (no learned-competence
    /// fields set) — Rule 1.
    pub fn is_definition(&self) -> bool {
        self.holder_did.is_none()
            && self.practice_count.is_none()
            && self.last_practiced_at.is_none()
            && self.strategies.is_empty()
    }

    /// True when this grain is a held instance (has learned-competence). The
    /// `prof` compaction key is emitted only for held instances.
    pub fn is_held(&self) -> bool {
        self.holder_did.is_some() || self.practice_count.is_some()
    }

    /// Proficiency reads from `common.confidence` (D3). Returns `None` for a
    /// pure definition, where `confidence` carries the "fully-specified"
    /// sense rather than a mastery level.
    pub fn proficiency(&self) -> Option<f64> {
        if self.is_held() {
            Some(self.common.confidence)
        } else {
            None
        }
    }

    /// Free text reachable from this skill that the write-path PII/PHI
    /// scanner must inspect IN ADDITION to `text()`/`embedding_text()` — the
    /// `instructions` body plus each strategy's `condition`/`description`
    /// (BC2). Kept out of `text()` so embedding quality is unaffected.
    ///
    /// Appends to `out` using the Event content-block precedent
    /// (`write.rs::append_content_block_text`).
    pub fn append_pii_scan_text(&self, out: &mut String) {
        if let Some(ref instr) = self.instructions {
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(instr);
        }
        for strat in &self.strategies {
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(&strat.condition);
            if let Some(ref d) = strat.description {
                out.push('\n');
                out.push_str(d);
            }
        }
    }
}

impl Grain for Skill {
    fn grain_type(&self) -> GrainType {
        GrainType::Skill
    }

    fn common(&self) -> &GrainCommon {
        &self.common
    }

    fn common_mut(&mut self) -> &mut GrainCommon {
        &mut self.common
    }

    /// Routing-relevant prose for embedding + reranking: name + description +
    /// when_to_use + domain. `instructions` is deliberately excluded (it can
    /// be large markdown and would dilute the discovery cue). Authors who
    /// want the body embedded set `common.embedding_text`, honored by the
    /// base `embedding_text()`.
    fn text(&self) -> String {
        let mut s = format!("{}: {}", self.name, self.description);
        if let Some(ref w) = self.when_to_use {
            s.push_str(" — ");
            s.push_str(w);
        }
        if let Some(ref d) = self.domain {
            s.push_str(" [");
            s.push_str(d);
            s.push(']');
        }
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_is_a_definition() {
        let s = Skill::new("code_review", "Review code for defects");
        assert!(s.is_definition());
        assert!(!s.is_held());
        assert_eq!(s.proficiency(), None);
        assert_eq!(s.common.confidence, 1.0);
    }

    #[test]
    fn holder_makes_it_held() {
        let s = Skill::new("code_review", "Review code")
            .holder_did("did:key:agentA")
            .with_proficiency(0.7);
        assert!(!s.is_definition());
        assert!(s.is_held());
        assert_eq!(s.proficiency(), Some(0.7));
    }

    #[test]
    fn proficiency_aliases_confidence() {
        let s = Skill::new("x", "y")
            .holder_did("did:key:a")
            .with_proficiency(0.42);
        // proficiency() and confidence are literally the same number (D3).
        assert_eq!(s.proficiency(), Some(s.common.confidence));
        assert_eq!(s.common.confidence, 0.42);
    }

    #[test]
    fn with_proficiency_clamps() {
        assert_eq!(
            Skill::new("x", "y")
                .holder_did("a")
                .with_proficiency(1.5)
                .common
                .confidence,
            1.0
        );
        assert_eq!(
            Skill::new("x", "y")
                .holder_did("a")
                .with_proficiency(-0.5)
                .common
                .confidence,
            0.0
        );
    }

    #[test]
    fn strategies_make_it_held() {
        let mut s = Skill::new("x", "y");
        s.strategies.push(SkillStrategy {
            condition: "when stuck".into(),
            workflow: "abc".into(),
            description: None,
        });
        assert!(!s.is_definition());
    }

    #[test]
    fn text_excludes_instructions() {
        let s = Skill::new("code_review", "Review code")
            .when_to_use("before merge")
            .domain("swe")
            .instructions("VERY LONG MARKDOWN BODY that should not be embedded");
        let t = s.text();
        assert!(t.contains("code_review"));
        assert!(t.contains("Review code"));
        assert!(t.contains("before merge"));
        assert!(t.contains("swe"));
        assert!(!t.contains("VERY LONG MARKDOWN"));
    }

    #[test]
    fn pii_scan_text_includes_instructions_and_strategies() {
        let mut s = Skill::new("x", "y").instructions("contact john@example.com");
        s.strategies.push(SkillStrategy {
            condition: "ssn 123-45-6789".into(),
            workflow: "wf".into(),
            description: Some("call 555-1234".into()),
        });
        let mut scan = String::new();
        s.append_pii_scan_text(&mut scan);
        assert!(scan.contains("john@example.com"));
        assert!(scan.contains("123-45-6789"));
        assert!(scan.contains("555-1234"));
    }
}
