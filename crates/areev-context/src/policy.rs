//! Format policy types for context rendering.
//!
//! `FormatPolicy` controls how recalled grains are rendered into LLM-ready strings:
//! output format (SML/Markdown/PlainText/JSON), metadata verbosity, ordering,
//! token budget, and section grouping.

use areev_cal::store_types::GrainTypeDiversityConfig;
use areev_core::types::GrainType;
use std::collections::HashMap;

/// Output format for rendered context.
#[derive(Debug, Clone, PartialEq)]
pub enum OutputFormat {
    /// SML tags: `<fact>`, `<event>`, etc. Optimized for Claude.
    Sml,
    /// TOON (Token-Oriented Object Notation). Compact, indentation-based format optimized for LLM token efficiency.
    Toon,
    /// Markdown with headers and formatting. Good for GPT-4/Gemini.
    Markdown,
    /// Plain text with `===` section headers.
    PlainText,
    /// Structured JSON. For programmatic consumers and A2A.
    Json,
}

/// How much metadata to include per grain.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum MetadataLevel {
    /// Content fields only — no hash, no confidence, no timestamps.
    None,
    /// Confidence + created_at only.
    Minimal,
    /// All common metadata: hash, confidence, tags, source_type, created_at,
    /// verification_status, namespace, author_did.
    Full,
}

/// Ordering strategy for grains in output.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Ordering {
    /// By retrieval score (highest first). Default.
    ByRelevance,
    /// Oldest first (ascending created_at).
    Chronological,
    /// Newest first (descending created_at).
    ReverseChronological,
    /// Group by entity (subject field), within each group order by relevance.
    ByEntity,
}

/// Section grouping configuration.
#[derive(Debug, Clone, Default)]
pub struct SectionConfig {
    /// Whether to group grains by type with section headers.
    pub group_by_type: bool,
    /// Custom section order. When empty, uses default:
    /// State, Goal, Fact, Tool, Event, Observation, Reasoning,
    /// Workflow, Consensus, Consent.
    pub type_order: Vec<GrainType>,
}

/// Per-grain-type overrides.
#[derive(Debug, Clone)]
pub struct GrainTypeOverride {
    /// Whether to include this grain type. Default: true.
    pub include: bool,
    /// Max grains of this type. None = no limit (budget-constrained).
    pub max_count: Option<usize>,
}

impl Default for GrainTypeOverride {
    fn default() -> Self {
        Self {
            include: true,
            max_count: None,
        }
    }
}

/// How a decision backend installed with
/// [`ContextAssembler::with_decider`](crate::ContextAssembler::with_decider)
/// may shape assembly (decision-backend phase 3, rows A2/A3). With no backend
/// installed this is inert; with one installed and `FormatPolicy.decide`
/// left `None`, [`DecidePolicy::default`] applies.
///
/// Rule 2 of `docs/decision-model-proposal.md` binds the thresholds: they act
/// only when the backend's `Decision.calibrated` is true. An uncalibrated
/// backend's relevance reorders the allocation and nothing else.
#[derive(Debug, Clone, PartialEq)]
pub struct DecidePolicy {
    /// A3: ask whether the query wants a timeline, the current value, or
    /// neither, in place of the keyword lists. Default true.
    pub intent: bool,
    /// A2: ask per candidate how relevant it is and whether a summary would
    /// lose a needed detail, and feed both to the allocator. Default true.
    pub disclosure: bool,
    /// Calibrated only: relevance (`score / 3`) below this is Omitted
    /// regardless of budget. Default 0.10.
    pub drop_below: f32,
    /// Calibrated only: `p(summary loses a needed detail)` at or above this
    /// prefers the Full render — the budget's 95% line still bounds it.
    /// Default 0.50.
    pub full_above: f32,
    /// Deadline for ALL of one assembly's backend calls together, in
    /// milliseconds. `None` = the backend's (chain's) own default.
    pub deadline_ms: Option<u64>,
}

impl Default for DecidePolicy {
    fn default() -> Self {
        Self {
            intent: true,
            disclosure: true,
            drop_below: areev_cal::judge::DEFAULT_DROP_BELOW,
            full_above: areev_cal::judge::DEFAULT_FULL_ABOVE,
            deadline_ms: None,
        }
    }
}

/// Complete formatting policy. Constructed via builder pattern.
#[derive(Debug, Clone)]
pub struct FormatPolicy {
    pub format: OutputFormat,
    pub metadata: MetadataLevel,
    pub ordering: Ordering,
    pub sections: SectionConfig,
    pub token_budget: Option<usize>,
    pub grain_overrides: HashMap<GrainType, GrainTypeOverride>,
    /// Original query text. Used by Knowledge Update chain rendering to detect
    /// recency intent ("currently", "most recently", etc.) for update-only
    /// suppression mode.
    pub query_text: Option<String>,
    /// Grain type diversity floor. When `Some`, the budget allocator reserves
    /// slots for underrepresented grain types before filling by priority.
    /// Default: `Some(GrainTypeDiversityConfig::default())`.
    pub grain_type_diversity: Option<GrainTypeDiversityConfig>,
    /// Admit grains whose `verification_status` is `retracted`. Off by
    /// default: a retraction withholds a memory from being treated as true,
    /// so assembly must not hand one to a model. Turn it on for audit and
    /// forensic reads, which want the withdrawn record precisely because it
    /// was withdrawn.
    pub include_retracted: bool,
    /// How an installed decision backend may shape this assembly. `None`
    /// with a backend installed = [`DecidePolicy::default`]; with no backend
    /// installed this field is never read.
    pub decide: Option<DecidePolicy>,
}

impl FormatPolicy {
    pub fn new(format: OutputFormat) -> Self {
        Self {
            format,
            metadata: MetadataLevel::Minimal,
            ordering: Ordering::ByRelevance,
            sections: SectionConfig::default(),
            token_budget: None,
            grain_overrides: HashMap::new(),
            query_text: None,
            grain_type_diversity: Some(GrainTypeDiversityConfig::default()),
            include_retracted: false,
            decide: None,
        }
    }

    /// Set how an installed decision backend may shape this assembly.
    pub fn decide(mut self, policy: DecidePolicy) -> Self {
        self.decide = Some(policy);
        self
    }

    pub fn include_retracted(mut self, on: bool) -> Self {
        self.include_retracted = on;
        self
    }

    pub fn metadata(mut self, level: MetadataLevel) -> Self {
        self.metadata = level;
        self
    }

    pub fn ordering(mut self, ordering: Ordering) -> Self {
        self.ordering = ordering;
        self
    }

    pub fn token_budget(mut self, budget: usize) -> Self {
        self.token_budget = Some(budget);
        self
    }

    pub fn group_by_type(mut self) -> Self {
        self.sections.group_by_type = true;
        self
    }

    pub fn grain_override(mut self, gt: GrainType, ovr: GrainTypeOverride) -> Self {
        self.grain_overrides.insert(gt, ovr);
        self
    }

    /// Set the original query text for recency detection in KU rendering.
    pub fn query_text(mut self, text: impl Into<String>) -> Self {
        self.query_text = Some(text.into());
        self
    }

    /// Set grain type diversity configuration.
    pub fn grain_type_diversity(mut self, config: GrainTypeDiversityConfig) -> Self {
        self.grain_type_diversity = Some(config);
        self
    }

    /// Disable grain type diversity floor (pure priority allocation).
    pub fn no_grain_type_diversity(mut self) -> Self {
        self.grain_type_diversity = None;
        self
    }
}

impl Default for FormatPolicy {
    fn default() -> Self {
        Self::new(OutputFormat::PlainText)
            .metadata(MetadataLevel::Minimal)
            .ordering(Ordering::ByRelevance)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_policy() {
        let p = FormatPolicy::default();
        assert_eq!(p.format, OutputFormat::PlainText);
        assert_eq!(p.metadata, MetadataLevel::Minimal);
        assert_eq!(p.ordering, Ordering::ByRelevance);
        assert!(p.token_budget.is_none());
        assert!(!p.sections.group_by_type);
    }

    #[test]
    fn test_builder_chain() {
        let p = FormatPolicy::new(OutputFormat::Sml)
            .metadata(MetadataLevel::Full)
            .ordering(Ordering::Chronological)
            .token_budget(4096)
            .group_by_type()
            .grain_override(
                GrainType::Consent,
                GrainTypeOverride {
                    include: true,
                    max_count: Some(5),
                },
            );

        assert_eq!(p.format, OutputFormat::Sml);
        assert_eq!(p.metadata, MetadataLevel::Full);
        assert_eq!(p.ordering, Ordering::Chronological);
        assert_eq!(p.token_budget, Some(4096));
        assert!(p.sections.group_by_type);
        assert_eq!(
            p.grain_overrides
                .get(&GrainType::Consent)
                .unwrap()
                .max_count,
            Some(5)
        );
    }
}
