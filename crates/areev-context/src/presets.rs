//! Pre-built `FormatPolicy` configurations for common LLM targets.
//!
//! Each preset configures format, metadata, and ordering for a model family.
//! Presets do not set `token_budget` — the caller always knows their budget
//! better than a preset.

use super::policy::{FormatPolicy, MetadataLevel, Ordering, OutputFormat};

impl FormatPolicy {
    /// SML format, minimal metadata, relevance-ordered, grouped by type.
    /// Optimized for Claude models which handle SML context natively.
    pub fn claude() -> Self {
        Self::new(OutputFormat::Sml)
            .metadata(MetadataLevel::Minimal)
            .ordering(Ordering::ByRelevance)
            .group_by_type()
    }

    /// Markdown format, minimal metadata, relevance-ordered.
    /// Good for GPT-4, GPT-4o, and similar models.
    pub fn gpt4() -> Self {
        Self::new(OutputFormat::Markdown)
            .metadata(MetadataLevel::Minimal)
            .ordering(Ordering::ByRelevance)
    }

    /// Markdown format, minimal metadata, relevance-ordered.
    /// Alias for `gpt4()` — Gemini also handles markdown well.
    pub fn gemini() -> Self {
        Self::gpt4()
    }

    /// Plain text, no metadata, relevance-ordered.
    /// For small local models with tight context windows.
    pub fn local_small() -> Self {
        Self::new(OutputFormat::PlainText)
            .metadata(MetadataLevel::None)
            .ordering(Ordering::ByRelevance)
    }

    /// JSON format, full metadata, relevance-ordered.
    /// For programmatic consumers, A2A, and API responses.
    pub fn json_api() -> Self {
        Self::new(OutputFormat::Json)
            .metadata(MetadataLevel::Full)
            .ordering(Ordering::ByRelevance)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_claude_preset() {
        let p = FormatPolicy::claude();
        assert_eq!(p.format, OutputFormat::Sml);
        assert_eq!(p.metadata, MetadataLevel::Minimal);
        assert_eq!(p.ordering, Ordering::ByRelevance);
        assert!(p.sections.group_by_type);
        assert!(p.token_budget.is_none());
    }

    #[test]
    fn test_gpt4_preset() {
        let p = FormatPolicy::gpt4();
        assert_eq!(p.format, OutputFormat::Markdown);
        assert_eq!(p.metadata, MetadataLevel::Minimal);
        assert!(!p.sections.group_by_type);
    }

    #[test]
    fn test_gemini_matches_gpt4() {
        let g = FormatPolicy::gemini();
        let gpt = FormatPolicy::gpt4();
        assert_eq!(g.format, gpt.format);
        assert_eq!(g.metadata, gpt.metadata);
        assert_eq!(g.ordering, gpt.ordering);
    }

    #[test]
    fn test_local_small_preset() {
        let p = FormatPolicy::local_small();
        assert_eq!(p.format, OutputFormat::PlainText);
        assert_eq!(p.metadata, MetadataLevel::None);
    }

    #[test]
    fn test_json_api_preset() {
        let p = FormatPolicy::json_api();
        assert_eq!(p.format, OutputFormat::Json);
        assert_eq!(p.metadata, MetadataLevel::Full);
    }

    #[test]
    fn test_preset_with_budget_override() {
        let p = FormatPolicy::claude().token_budget(4096);
        assert_eq!(p.format, OutputFormat::Sml);
        assert_eq!(p.token_budget, Some(4096));
    }
}
