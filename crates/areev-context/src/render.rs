//! Grain renderers for context formatting — a thin delegation layer.
//!
//! Since the render unification, the
//! actual per-grain rendering lives in `areev_cal::render` — ONE
//! implementation shared with CAL's `FORMAT` arms, so a grain renders
//! identically on every surface. What stays here is orchestration-side:
//! the `GrainRenderer` trait and its registry (the seam a host can override
//! with `ContextAssembler::with_renderer`), the per-type context priorities
//! that feed budget allocation, and the per-type summaries.

use std::collections::HashMap;

use areev_cal::render as shared;
use areev_cal::store_types::SearchHit;
use areev_core::format::deserialize::DeserializedGrain;
use areev_core::verification::Trust;
use areev_core::types::GrainType;

use super::policy::{FormatPolicy, MetadataLevel, OutputFormat};

/// Renders a single grain type into formatted context strings.
///
/// Object-safe: stored in `RendererRegistry` keyed by `GrainType`.
pub trait GrainRenderer: Send + Sync {
    /// Which grain type this renderer handles.
    fn grain_type(&self) -> GrainType;

    /// Full render of the grain's content.
    fn render(&self, grain: &DeserializedGrain, policy: &FormatPolicy) -> String;

    /// Compact one-line summary for budget-constrained contexts.
    fn render_summary(&self, grain: &DeserializedGrain, policy: &FormatPolicy) -> String {
        self.render(grain, policy)
    }

    /// Estimated token count for the full render (~4 chars per token).
    fn token_estimate(&self, grain: &DeserializedGrain, policy: &FormatPolicy) -> usize {
        with_view(grain, |view| {
            shared::estimate_tokens(view, detail(policy.metadata))
        })
    }

    /// Context priority for this grain [0.0, 1.0].
    fn context_priority(&self, grain: &DeserializedGrain, hit: &SearchHit) -> f32;
}

/// Registry mapping GrainType to its renderer.
pub struct RendererRegistry {
    renderers: HashMap<GrainType, Box<dyn GrainRenderer>>,
}

impl Default for RendererRegistry {
    fn default() -> Self {
        Self::new()
    }
}

/// Every grain type, for registry construction.
const ALL_TYPES: [GrainType; 12] = [
    GrainType::Fact,
    GrainType::Event,
    GrainType::State,
    GrainType::Workflow,
    GrainType::Tool,
    GrainType::Observation,
    GrainType::Goal,
    GrainType::Reasoning,
    GrainType::Consensus,
    GrainType::Consent,
    GrainType::Skill,
    GrainType::Recommendation,
];

impl RendererRegistry {
    /// Create with the default (shared-implementation) renderer registered
    /// for every grain type.
    pub fn new() -> Self {
        let mut registry = Self {
            renderers: HashMap::with_capacity(ALL_TYPES.len()),
        };
        for gt in ALL_TYPES {
            registry.register(Box::new(SharedRenderer { gt }));
        }
        registry
    }

    /// Get renderer for a grain type.
    pub fn get(&self, gt: GrainType) -> Option<&dyn GrainRenderer> {
        self.renderers.get(&gt).map(|r| r.as_ref())
    }

    /// Register a custom renderer (replaces existing for that type).
    pub fn register(&mut self, renderer: Box<dyn GrainRenderer>) {
        self.renderers.insert(renderer.grain_type(), renderer);
    }
}

/// TOON column names per grain type, per CAL spec Section 10.9.3. Reads from
/// the grain-type metadata registry — the single source of truth.
pub fn toon_columns(gt: &GrainType) -> &'static [&'static str] {
    areev_core::types::registry::meta(*gt).toon_columns
}

// ---------------------------------------------------------------------------
// View bridging
// ---------------------------------------------------------------------------

fn detail(level: MetadataLevel) -> shared::MetadataDetail {
    match level {
        MetadataLevel::None => shared::MetadataDetail::None,
        MetadataLevel::Minimal => shared::MetadataDetail::Minimal,
        MetadataLevel::Full => shared::MetadataDetail::Full,
    }
}

/// Build the shared-renderer view of a deserialized grain and run `f` on it.
/// The fields map is materialized as a JSON object for the duration of the
/// call (BTreeMap-backed, so iteration order is deterministic).
fn with_view<R>(grain: &DeserializedGrain, f: impl FnOnce(&shared::GrainView<'_>) -> R) -> R {
    let fields = serde_json::Value::Object(
        grain
            .fields
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect(),
    );
    let hash_hex = grain.hash.to_hex();
    let view = shared::GrainView {
        grain_type: grain.grain_type.as_str(),
        hash: &hash_hex,
        fields: &fields,
        created_at_sec: grain.header.created_at_sec,
    };
    f(&view)
}

// ---------------------------------------------------------------------------
// The default renderer — delegates rendering, keeps allocation semantics
// ---------------------------------------------------------------------------

/// The default per-type renderer: rendering and summaries delegate to
/// `areev_cal::render`; context priority (an allocation concern, not a
/// rendering one) is decided here per type.
struct SharedRenderer {
    gt: GrainType,
}

impl GrainRenderer for SharedRenderer {
    fn grain_type(&self) -> GrainType {
        self.gt
    }

    fn render(&self, grain: &DeserializedGrain, policy: &FormatPolicy) -> String {
        with_view(grain, |view| match &policy.format {
            OutputFormat::Sml => shared::render_grain_sml(view, detail(policy.metadata)),
            OutputFormat::Markdown => shared::render_grain_markdown(view),
            OutputFormat::PlainText => shared::render_grain_text_line(view, None),
            OutputFormat::Json => shared::grain_json(view).to_string(),
            OutputFormat::Toon => shared::toon_row(view),
        })
    }

    fn render_summary(&self, grain: &DeserializedGrain, policy: &FormatPolicy) -> String {
        with_view(grain, |view| {
            let summary = shared::render_grain_summary(view);
            match &policy.format {
                // Keep SML well-formed under budget pressure: the semantic
                // tag survives, the metadata does not.
                OutputFormat::Sml => {
                    let tag = view.grain_type;
                    format!("<{tag}>{}</{tag}>", shared::sml_escape(&summary))
                }
                // A summary is still a list item.
                OutputFormat::Markdown => format!("- {summary}"),
                // Structured formats never receive summaries (the assembler
                // degrades Summary to Omit for them); prose text gets the
                // bare line.
                _ => summary,
            }
        })
    }

    fn context_priority(&self, grain: &DeserializedGrain, hit: &SearchHit) -> f32 {
        let base = match self.gt {
            GrainType::Fact => 0.7,
            GrainType::Event => 0.6,
            // A state snapshot is what the agent resumes from.
            GrainType::State => 0.9,
            GrainType::Workflow => 0.5,
            GrainType::Tool => {
                // A failed call is more informative than a successful one.
                if grain.get_bool("is_error").unwrap_or(false) {
                    0.65
                } else {
                    0.5
                }
            }
            GrainType::Observation => 0.6,
            GrainType::Goal => {
                let state_mod = match grain.get_str("goal_state") {
                    Some("active") => 0.15,
                    Some("suspended") => 0.05,
                    Some("satisfied" | "failed") => -0.2,
                    _ => 0.0,
                };
                let priority_mod = match grain.get_str("priority") {
                    Some("critical") => 0.1,
                    Some("high") => 0.05,
                    Some("low") => -0.1,
                    _ => 0.0,
                };
                0.8 + state_mod + priority_mod
            }
            GrainType::Reasoning => 0.7,
            GrainType::Consensus => 0.65,
            // Consent grains get highest base priority — legally required
            // context.
            GrainType::Consent => 0.95,
            GrainType::Skill => 0.6,
            // A pending proposal is something a reviewer must act on, so it
            // ranks above passive records but below the facts it reasons
            // about.
            GrainType::Recommendation => 0.7,
            // A declaration of what the agent is supposed to be doing on its
            // own is worth surfacing, but it is standing configuration rather
            // than something that just happened — below an Event, above a plan.
            GrainType::Trigger => 0.55,
        };
        adjusted_priority(base, grain, hit)
    }
}

/// Apply priority modifiers common to all grain types.
fn adjusted_priority(base: f32, grain: &DeserializedGrain, hit: &SearchHit) -> f32 {
    let score_boost = hit.score as f32 * 0.15;
    let confidence = grain.get_f64("confidence").unwrap_or(1.0) as f32;
    let confidence_boost = (confidence - 0.5).max(0.0) * 0.1;
    let verification_penalty =
        Trust::from_field(grain.get_str("verification_status")).priority_delta();
    (base + score_boost + confidence_boost + verification_penalty).clamp(0.0, 1.0)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use areev_core::format::header::MgHeader;
    use std::collections::HashMap;

    /// Build a test grain with given type and fields.
    fn test_grain(gt: GrainType, fields: Vec<(&str, serde_json::Value)>) -> DeserializedGrain {
        let mut map = HashMap::new();
        for (k, v) in fields {
            map.insert(k.to_string(), v);
        }
        DeserializedGrain {
            header: MgHeader {
                version: 1,
                flags: 0,
                grain_type: gt.type_byte(),
                ns_hash: 0,
                created_at_sec: 1740000000, // 2025-02-19
            },
            grain_type: gt,
            fields: map,
            hash: areev_core::error::Hash::from_bytes(&[0u8; 32]),
        }
    }

    fn test_hit(grain: DeserializedGrain) -> SearchHit {
        SearchHit {
            hash: grain.hash,
            score: 0.85,
            grain,
            score_breakdown: None,
            #[cfg(feature = "rerank")]
            rerank_score: None,
            #[cfg(feature = "llm-rerank")]
            llm_rerank_score: None,
            explanation: None,
            scope_depth: None,
            source_namespace: None,
            relative_time: None,
            conflict_status: None,
            supersession_status: None,
            superseded_by_hash: None,
            recall_source: None,
        }
    }

    fn fact() -> DeserializedGrain {
        test_grain(
            GrainType::Fact,
            vec![
                ("subject", serde_json::json!("john")),
                ("relation", serde_json::json!("likes")),
                ("object", serde_json::json!("coffee")),
                ("confidence", serde_json::json!(0.95)),
            ],
        )
    }

    #[test]
    fn fact_sml_is_semantic_and_identical_to_the_shared_renderer() {
        let grain = fact();
        let policy = FormatPolicy::new(OutputFormat::Sml).metadata(MetadataLevel::Minimal);
        let registry = RendererRegistry::new();
        let via_registry = registry.get(GrainType::Fact).unwrap().render(&grain, &policy);
        let direct = with_view(&grain, |v| {
            shared::render_grain_sml(v, shared::MetadataDetail::Minimal)
        });
        assert_eq!(via_registry, direct);
        assert!(via_registry.starts_with("<fact"));
        assert!(via_registry.contains("confidence=\"0.95\""));
        assert!(via_registry.contains("john likes coffee"));
    }

    #[test]
    fn fact_markdown_matches_the_documented_cal_shape() {
        let policy = FormatPolicy::new(OutputFormat::Markdown).metadata(MetadataLevel::None);
        let registry = RendererRegistry::new();
        let grain = test_grain(
            GrainType::Fact,
            vec![
                ("subject", serde_json::json!("john")),
                ("relation", serde_json::json!("likes")),
                ("object", serde_json::json!("coffee")),
            ],
        );
        let result = registry.get(GrainType::Fact).unwrap().render(&grain, &policy);
        assert_eq!(result, "- **john** likes coffee *(2025-02-19)*");
    }

    #[test]
    fn fact_summary_is_the_bare_triple() {
        let policy = FormatPolicy::default();
        let registry = RendererRegistry::new();
        let result = registry
            .get(GrainType::Fact)
            .unwrap()
            .render_summary(&fact(), &policy);
        assert_eq!(result, "john likes coffee");
    }

    #[test]
    fn json_is_the_envelope() {
        let policy = FormatPolicy::new(OutputFormat::Json).metadata(MetadataLevel::None);
        let registry = RendererRegistry::new();
        let out = registry.get(GrainType::Fact).unwrap().render(&fact(), &policy);
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["grain_type"], "fact");
        assert_eq!(v["fields"]["subject"], "john");
        assert!(v["hash"].as_str().is_some());
    }

    #[test]
    fn every_grain_type_has_a_default_renderer() {
        let registry = RendererRegistry::new();
        for gt in ALL_TYPES {
            assert!(registry.get(gt).is_some(), "missing renderer for {gt:?}");
        }
    }

    #[test]
    fn custom_renderer_replaces_the_default() {
        struct Loud;
        impl GrainRenderer for Loud {
            fn grain_type(&self) -> GrainType {
                GrainType::Fact
            }
            fn render(&self, _g: &DeserializedGrain, _p: &FormatPolicy) -> String {
                "LOUD".into()
            }
            fn context_priority(&self, _g: &DeserializedGrain, _h: &SearchHit) -> f32 {
                1.0
            }
        }
        let mut registry = RendererRegistry::new();
        registry.register(Box::new(Loud));
        let policy = FormatPolicy::default();
        assert_eq!(
            registry.get(GrainType::Fact).unwrap().render(&fact(), &policy),
            "LOUD"
        );
    }

    #[test]
    fn priorities_keep_their_per_type_semantics() {
        let registry = RendererRegistry::new();
        let policy_hit = |gt: GrainType, fields: Vec<(&str, serde_json::Value)>| {
            let grain = test_grain(gt, fields);
            let hit = test_hit(grain);
            registry
                .get(gt)
                .unwrap()
                .context_priority(&hit.grain, &hit)
        };
        // Consent outranks facts; a failed tool call outranks a clean one.
        let consent = policy_hit(GrainType::Consent, vec![]);
        let fact_p = policy_hit(GrainType::Fact, vec![]);
        assert!(consent > fact_p);
        let tool_ok = policy_hit(GrainType::Tool, vec![("is_error", serde_json::json!(false))]);
        let tool_err = policy_hit(GrainType::Tool, vec![("is_error", serde_json::json!(true))]);
        assert!(tool_err > tool_ok);
        // An active goal outranks a satisfied one.
        let g_active = policy_hit(
            GrainType::Goal,
            vec![("goal_state", serde_json::json!("active"))],
        );
        let g_done = policy_hit(
            GrainType::Goal,
            vec![("goal_state", serde_json::json!("satisfied"))],
        );
        assert!(g_active > g_done);
    }

    #[test]
    fn token_estimate_uses_the_shared_estimator() {
        let registry = RendererRegistry::new();
        let grain = fact();
        let none = registry.get(GrainType::Fact).unwrap().token_estimate(
            &grain,
            &FormatPolicy::new(OutputFormat::Sml).metadata(MetadataLevel::None),
        );
        let full = registry.get(GrainType::Fact).unwrap().token_estimate(
            &grain,
            &FormatPolicy::new(OutputFormat::Sml).metadata(MetadataLevel::Full),
        );
        assert!(none >= 1);
        assert_eq!(full, none + 20); // 80 chars of metadata overhead / 4
    }

    #[test]
    fn event_toon_row_carries_the_header_date() {
        let registry = RendererRegistry::new();
        let grain = test_grain(
            GrainType::Event,
            vec![
                ("content", serde_json::json!("hello")),
                ("role", serde_json::json!("user")),
            ],
        );
        let policy = FormatPolicy::new(OutputFormat::Toon).metadata(MetadataLevel::None);
        let row = registry.get(GrainType::Event).unwrap().render(&grain, &policy);
        assert_eq!(row, "user,2025-02-19,hello");
    }
}
