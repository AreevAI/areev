//! Context assembly — orchestrates rendering, budget allocation, ordering, and sections.
//!
//! `ContextAssembler` takes `&[SearchHit]` + `FormatPolicy` and produces a
//! `FormattedContext` ready for LLM consumption.
//!
//! ## RF-4: Structured Context Rendering
//!
//! Three intent-aware rendering modes improve how grains are presented:
//!
//! 1. **Aggregation mode** — entity summary header + grouped-by-entity rendering
//!    when `entity_count` is present in the `RecallResult`.
//! 2. **Timeline mode** — chronological rendering with date prefixes when temporal
//!    intent is detected (temporal_expr, time range, or multi-day span).
//! 3. **Relevance highlighting** — top-5 "Most Relevant" section + "Additional
//!    Context" when result set exceeds 10 grains.
//!
//! Modes are mutually exclusive, checked in priority order (aggregation > timeline >
//! relevance). JSON output bypasses all modes.

use std::collections::{HashMap, HashSet};

use areev_cal::store_types::{RecallSource, SearchHit, SupersessionStatus};
use areev_core::types::GrainType;

use super::budget::{self, Allocation, ScoredEntry};
use super::policy::{FormatPolicy, Ordering, OutputFormat};
use super::render::{GrainRenderer, RendererRegistry};

/// RF-4: Hints from the recall query/result that guide rendering mode selection.
///
/// Constructed from `RecallParams` and `RecallResult` metadata. When no hints are
/// provided, the assembler falls back to default rendering. When temporal intent
/// is detected, the assembler switches to chronological timeline rendering with
/// explicit deltas between consecutive events.
#[derive(Debug, Clone, Default)]
pub struct RenderingHints {
    /// Number of distinct entities — set when `count_entities = true` was used.
    pub entity_count: Option<usize>,
    /// Distinct entity subjects — set when `count_entities = true` was used.
    pub entities: Option<Vec<String>>,
    /// Whether a temporal expression was used in the query.
    pub has_temporal_expr: bool,
    /// Whether both time_start and time_end were set.
    pub has_time_range: bool,
    /// The original query text, used for temporal pattern matching.
    pub query_text: Option<String>,
}

/// Assembled context ready for LLM consumption.
#[derive(Debug, Clone, serde::Serialize)]
pub struct FormattedContext {
    /// The rendered context string.
    pub text: String,
    /// Estimated token count of the rendered text.
    pub estimated_tokens: usize,
    /// Number of grains included (full + summary).
    pub included_count: usize,
    /// Number of grains omitted due to budget.
    pub omitted_count: usize,
    /// Whether any grains were omitted.
    pub truncated: bool,
}

// ---------------------------------------------------------------------------
// Knowledge Update chain types and helpers (RQ-5)
// ---------------------------------------------------------------------------

/// A supersession chain linking an outdated grain to its current replacement.
struct SupersessionChain {
    /// Index of the superseded (old) grain in the hits array.
    old_index: usize,
    /// Index of the current (new) grain in the hits array.
    new_index: usize,
    /// Subject of the update (from the grain's `subject` field).
    subject: String,
    /// Old value (from the superseded grain's `object` field).
    old_value: String,
    /// Old date as ISO-8601 date string.
    old_date: Option<String>,
    /// New/current value (from the superseder grain's `object` field).
    new_value: String,
    /// New date as ISO-8601 date string.
    new_date: Option<String>,
}

/// Check if the query text contains recency signals indicating the user wants
/// only the current value (suppressing outdated values entirely).
fn is_recency_query(query_text: Option<&str>) -> bool {
    if let Some(q) = query_text {
        let q = q.to_lowercase();
        let patterns = [
            "currently",
            "right now",
            "most recently",
            "at this point",
            "latest",
            "what is",
            "what's",
        ];
        patterns.iter().any(|p| q.contains(p))
    } else {
        false
    }
}

/// Extract supersession chains from the search hits.
///
/// Finds pairs where grain A has `supersession_status == Superseded` and
/// `superseded_by_hash` points to grain B that is in the same result set
/// with `supersession_status == Current`.
///
/// For multi-hop chains (A -> B -> C), the oldest and newest are paired.
fn extract_supersession_chains(hits: &[SearchHit]) -> Vec<SupersessionChain> {
    // Build hash-to-index map for O(1) lookups.
    let hash_to_index: HashMap<_, _> = hits.iter().enumerate().map(|(i, h)| (h.hash, i)).collect();

    let mut chains = Vec::new();
    // Track which indices are already consumed as intermediaries.
    let mut consumed: HashSet<usize> = HashSet::new();

    for (i, hit) in hits.iter().enumerate() {
        if consumed.contains(&i) {
            continue;
        }
        if hit.supersession_status != Some(SupersessionStatus::Superseded) {
            continue;
        }
        let Some(superseder_hash) = &hit.superseded_by_hash else {
            continue;
        };

        // Follow the chain to find the terminal (current) grain.
        let mut current_hash = *superseder_hash;
        let mut intermediaries = Vec::new();
        let mut terminal_index = None;

        for _ in 0..10 {
            // Max 10 hops to prevent infinite loops.
            if let Some(&idx) = hash_to_index.get(&current_hash) {
                if hits[idx].supersession_status == Some(SupersessionStatus::Current) {
                    terminal_index = Some(idx);
                    break;
                } else if hits[idx].supersession_status == Some(SupersessionStatus::Superseded) {
                    // Intermediate — mark as consumed and follow.
                    intermediaries.push(idx);
                    if let Some(next) = &hits[idx].superseded_by_hash {
                        current_hash = *next;
                    } else {
                        break;
                    }
                } else {
                    break;
                }
            } else {
                break;
            }
        }

        if let Some(new_idx) = terminal_index {
            // Extract subject/object from grain fields.
            let subject = field_str(&hits[i].grain.fields, "subject")
                .or_else(|| field_str(&hits[i].grain.fields, "description"))
                .unwrap_or_default();
            // State's snapshot lands under `context` (the `ctx` wire key, OMS §8.3);
            // `context_data` is the Rust field name and never appears in `fields`.
            let old_value = field_str(&hits[i].grain.fields, "object")
                .or_else(|| field_str(&hits[i].grain.fields, "content"))
                .or_else(|| field_str(&hits[i].grain.fields, "context"))
                .unwrap_or_default();
            let new_value = field_str(&hits[new_idx].grain.fields, "object")
                .or_else(|| field_str(&hits[new_idx].grain.fields, "content"))
                .or_else(|| field_str(&hits[new_idx].grain.fields, "context"))
                .unwrap_or_default();

            let old_date = format_timestamp(hits[i].grain.header.created_at_sec);
            let new_date = format_timestamp(hits[new_idx].grain.header.created_at_sec);

            consumed.insert(i);
            consumed.insert(new_idx);
            for &mid in &intermediaries {
                consumed.insert(mid);
            }

            chains.push(SupersessionChain {
                old_index: i,
                new_index: new_idx,
                subject,
                old_value,
                old_date,
                new_value,
                new_date,
            });
        }
    }

    chains
}

/// Extract a string value from grain fields.
fn field_str(
    fields: &std::collections::HashMap<String, serde_json::Value>,
    key: &str,
) -> Option<String> {
    fields.get(key).and_then(|v| v.as_str()).map(String::from)
}

/// Format a UNIX timestamp (seconds) as an ISO-8601 date string (YYYY-MM-DD).
/// Progressive disclosure degrades a grain to a prose summary — which has no
/// place inside a structured dump. JSON arrays and TOON tables get whole
/// entries or nothing, so a Summary admission becomes Omit there.
fn strip_summaries_for_structured_formats(allocations: &mut [Allocation], policy: &FormatPolicy) {
    if matches!(policy.format, OutputFormat::Json | OutputFormat::Toon) {
        for a in allocations.iter_mut() {
            if *a == Allocation::Summary {
                *a = Allocation::Omit;
            }
        }
    }
}

fn format_timestamp(secs: u32) -> Option<String> {
    if secs == 0 {
        return None;
    }
    // Simple date calculation from epoch seconds.
    let total_days = secs as i64 / 86400;
    // Days from 1970-01-01
    let mut year = 1970i32;
    let mut remaining = total_days;
    loop {
        let days_in_year = if is_leap(year) { 366 } else { 365 };
        if remaining < days_in_year {
            break;
        }
        remaining -= days_in_year;
        year += 1;
    }
    let leap = is_leap(year);
    let month_days = if leap {
        [31, 29, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
    } else {
        [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
    };
    let mut month = 1u32;
    for &md in &month_days {
        if remaining < md {
            break;
        }
        remaining -= md;
        month += 1;
    }
    let day = remaining + 1;
    Some(format!("{year:04}-{month:02}-{day:02}"))
}

fn is_leap(y: i32) -> bool {
    (y % 4 == 0 && y % 100 != 0) || y % 400 == 0
}

/// The main assembly engine.
pub struct ContextAssembler {
    registry: RendererRegistry,
}

impl Default for ContextAssembler {
    fn default() -> Self {
        Self::new()
    }
}

impl ContextAssembler {
    pub fn new() -> Self {
        Self {
            registry: RendererRegistry::new(),
        }
    }

    /// Register a custom renderer (replaces default for that grain type).
    pub fn with_renderer(mut self, renderer: Box<dyn GrainRenderer>) -> Self {
        self.registry.register(renderer);
        self
    }

    /// Format a set of search hits into LLM-ready context.
    ///
    /// Equivalent to `format_with_hints(hits, policy, &RenderingHints::default())` —
    /// no structured rendering modes are activated.
    pub fn format(&self, hits: &[SearchHit], policy: &FormatPolicy) -> FormattedContext {
        self.format_with_hints(hits, policy, &RenderingHints::default())
    }

    /// RF-4: Format with structured rendering hints.
    ///
    /// Selects one of three intent-aware rendering modes based on the hints,
    /// checked in priority order:
    /// 1. Aggregation mode (entity_count present)
    /// 2. Timeline mode (temporal intent detected — chronological with deltas)
    /// 3. Census mode (primary + census section split)
    /// 4. Relevance highlighting (> 10 grains)
    /// 5. Default rendering (unchanged)
    ///
    /// JSON output bypasses all modes and renders normally.
    pub fn format_with_hints(
        &self,
        hits: &[SearchHit],
        policy: &FormatPolicy,
        hints: &RenderingHints,
    ) -> FormattedContext {
        if hits.is_empty() {
            return FormattedContext {
                text: String::new(),
                estimated_tokens: 0,
                included_count: 0,
                omitted_count: 0,
                truncated: false,
            };
        }

        // RF-4 Mode 2: Check for temporal intent — timeline rendering takes
        // priority over census/relevance highlighting.
        let hit_refs: Vec<&SearchHit> = hits.iter().collect();
        if detect_temporal_intent(hints, &hit_refs) && hits.len() >= 2 {
            return self.format_timeline_pass(hits, policy);
        }

        // Split hits into primary and census groups based on recall_source.
        let has_census = hits
            .iter()
            .any(|h| h.recall_source == Some(RecallSource::Census));

        if !has_census {
            return self.format_single_pass(hits, policy);
        }

        // Partition indices into primary and census.
        let mut primary_indices: Vec<usize> = Vec::new();
        let mut census_indices: Vec<usize> = Vec::new();
        for (i, hit) in hits.iter().enumerate() {
            if hit.recall_source == Some(RecallSource::Census) {
                census_indices.push(i);
            } else {
                primary_indices.push(i);
            }
        }

        // Budget split: primary gets 80%, census gets 20%.
        let (primary_budget, census_budget) = match policy.token_budget {
            Some(total) => (Some((total * 4) / 5), Some(total / 5)),
            None => (None, None),
        };

        // Render primary grains with 80% budget.
        let primary_policy = match primary_budget {
            Some(b) => {
                let mut p = policy.clone();
                p.token_budget = Some(b);
                p
            }
            None => policy.clone(),
        };
        let primary_hits: Vec<&SearchHit> = primary_indices.iter().map(|&i| &hits[i]).collect();
        let primary_ctx = if primary_hits.is_empty() {
            FormattedContext {
                text: String::new(),
                estimated_tokens: 0,
                included_count: 0,
                omitted_count: 0,
                truncated: false,
            }
        } else {
            // Build a temporary vec for format_single_pass (borrows as slice).
            let primary_owned: Vec<SearchHit> = primary_hits.iter().map(|h| (*h).clone()).collect();
            self.format_single_pass(&primary_owned, &primary_policy)
        };

        // Render census grains with 20% budget in a separate section.
        let census_policy = match census_budget {
            Some(b) => {
                let mut p = policy.clone();
                p.token_budget = Some(b);
                p
            }
            None => policy.clone(),
        };
        let census_hits: Vec<SearchHit> = census_indices.iter().map(|&i| hits[i].clone()).collect();
        let census_ctx = if census_hits.is_empty() {
            None
        } else {
            let ctx = self.format_census_section(&census_hits, &census_policy);
            if ctx.text.is_empty() {
                None
            } else {
                Some(ctx)
            }
        };

        // Combine primary and census.
        let mut text = primary_ctx.text;
        let mut included_count = primary_ctx.included_count;
        let mut omitted_count = primary_ctx.omitted_count;

        if let Some(ref census) = census_ctx {
            if policy.format == OutputFormat::Json {
                // JSON is a single structured document: primary and census
                // grains form ONE array (census grains already carry
                // recall_source). Concatenating two top-level arrays with a
                // comma produced invalid JSON — see merge_json_census.
                text = merge_json_census(&text, &census.text);
            } else {
                let sep = section_separator(&policy.format);
                if !text.is_empty() {
                    text.push_str(sep);
                }
                text.push_str(&census.text);
            }
            included_count += census.included_count;
            omitted_count += census.omitted_count;
        }

        let estimated_tokens = text.len() / 4;

        FormattedContext {
            text,
            estimated_tokens,
            included_count,
            omitted_count,
            truncated: omitted_count > 0,
        }
    }

    /// Format a set of hits without census separation (standard pipeline).
    fn format_single_pass(&self, hits: &[SearchHit], policy: &FormatPolicy) -> FormattedContext {
        if hits.is_empty() {
            return FormattedContext {
                text: String::new(),
                estimated_tokens: 0,
                included_count: 0,
                omitted_count: 0,
                truncated: false,
            };
        }

        // Step 0: Extract Knowledge Update chains (RQ-5).
        // Detect supersession pairs and render them as a dedicated section.
        let chains = extract_supersession_chains(hits);
        let recency = is_recency_query(policy.query_text.as_deref());
        let ku_section = self.render_knowledge_updates(&chains, &policy.format, recency);

        // Collect indices consumed by KU chains — these are removed from
        // the main context to avoid duplication.
        let ku_consumed: HashSet<usize> = chains
            .iter()
            .flat_map(|c| {
                if recency {
                    // Recency mode: suppress the outdated grain entirely,
                    // and also remove the current grain (it's shown in KU).
                    vec![c.old_index, c.new_index]
                } else {
                    // Non-recency: both old and new shown in KU section.
                    vec![c.old_index, c.new_index]
                }
            })
            .collect();

        // Step 1: Apply grain-type overrides (include/exclude + max_count)
        let filtered: Vec<usize> = self
            .apply_overrides(hits, policy)
            .into_iter()
            .filter(|idx| !ku_consumed.contains(idx))
            .collect();

        // RF-2: Exclude superseded grains whose superseder is in the result set,
        // unless metadata is Full (keep both, annotated with [OUTDATED]).
        let filtered = if policy.metadata != super::policy::MetadataLevel::Full {
            self.exclude_superseded_pairs(hits, &filtered)
        } else {
            filtered
        };

        // Step 2: Score + measure each hit
        let mut scored: Vec<ScoredEntry> = filtered
            .iter()
            .enumerate()
            .map(|(i, &idx)| {
                let hit = &hits[idx];
                let gt = hit.grain.grain_type;
                let renderer = self.registry.get(gt);
                let (priority, full_tokens) = match renderer {
                    Some(r) => (
                        r.context_priority(&hit.grain, hit),
                        r.token_estimate(&hit.grain, policy),
                    ),
                    None => (0.5, estimate_default_tokens(hit)),
                };
                // Summary is roughly 1/3 of full (heuristic from architecture design)
                let summary_tokens = (full_tokens / 3).max(1);
                ScoredEntry {
                    priority,
                    full_tokens,
                    summary_tokens,
                    original_index: i,
                    grain_type: gt,
                }
            })
            .collect();

        // Step 3: Allocate budget (diversity-aware when configured).
        let mut allocations = match policy.grain_type_diversity {
            Some(ref diversity) => {
                budget::allocate_with_diversity(&mut scored, policy.token_budget, diversity)
            }
            None => budget::allocate(&mut scored, policy.token_budget),
        };
        strip_summaries_for_structured_formats(&mut allocations, policy);

        // Step 4: Collect included entries with their allocation
        let included: Vec<(usize, Allocation)> = allocations
            .iter()
            .enumerate()
            .filter(|(_, alloc)| **alloc != Allocation::Omit)
            .map(|(i, alloc)| (filtered[i], *alloc))
            .collect();

        let omitted_count = allocations
            .iter()
            .filter(|a| **a == Allocation::Omit)
            .count();

        // Step 5: Split primary and expansion hits.
        let has_expansion = included.iter().any(|(idx, _)| {
            hits[*idx].recall_source == Some(areev_cal::store_types::RecallSource::Expansion)
        });

        let (primary_included, expansion_included): (Vec<_>, Vec<_>) = if has_expansion {
            included.iter().partition(|(idx, _)| {
                hits[*idx].recall_source != Some(areev_cal::store_types::RecallSource::Expansion)
            })
        } else {
            (included, Vec::new())
        };

        // Step 6: Order included entries (primary only).
        let mut primary_ordered = primary_included;
        self.apply_ordering(&mut primary_ordered, hits, policy);

        // Step 7: Render primary section.
        let primary_text = if policy.sections.group_by_type {
            self.render_grouped(&primary_ordered, hits, policy)
        } else {
            self.render_flat(&primary_ordered, hits, policy)
        };

        // Step 8: Render expansion section (if present).
        let expansion_count = expansion_included.len();
        let main_text = if !expansion_included.is_empty() {
            let mut exp_ordered = expansion_included;
            self.apply_ordering(&mut exp_ordered, hits, policy);
            let sep = section_separator(&policy.format);
            let expansion_section = match policy.format {
                OutputFormat::Sml => {
                    let inner = self.render_flat(&exp_ordered, hits, policy);
                    format!("<expansion_results>\n{inner}\n</expansion_results>")
                }
                OutputFormat::Json => {
                    let mut parts = Vec::new();
                    for (idx, _alloc) in &exp_ordered {
                        let hit = &hits[*idx];
                        let gt = hit.grain.grain_type;
                        let renderer = self.registry.get(gt);
                        let rendered = match renderer {
                            Some(r) => r.render(&hit.grain, policy),
                            None => format!("[{}]", gt.as_str()),
                        };
                        if let Ok(mut obj) = serde_json::from_str::<serde_json::Value>(&rendered) {
                            if let Some(map) = obj.as_object_mut() {
                                map.insert(
                                    "recall_source".to_string(),
                                    serde_json::Value::String("expansion".to_string()),
                                );
                            }
                            parts.push(obj.to_string());
                        } else {
                            parts.push(rendered);
                        }
                    }
                    format!("[{}]", parts.join(",\n"))
                }
                _ => {
                    let header = expansion_section_header(&policy.format);
                    let inner = self.render_flat(&exp_ordered, hits, policy);
                    format!("{header}{sep}{inner}")
                }
            };
            format!("{primary_text}{sep}{expansion_section}")
        } else {
            primary_text
        };

        // Step 9: Combine KU section + main context (RQ-5).
        let ku_grain_count = chains.len() * 2;
        let text = match (&ku_section, &policy.format) {
            (Some(ku), OutputFormat::Json) => {
                // For JSON, wrap in an object with knowledge_updates and context keys.
                format!("{{\"knowledge_updates\":{ku},\"context\":{main_text}}}",)
            }
            (Some(ku), _) => {
                if main_text.is_empty() {
                    ku.clone()
                } else {
                    let sep = section_separator(&policy.format);
                    format!("{ku}{sep}{main_text}")
                }
            }
            (None, _) => main_text,
        };

        let estimated_tokens = text.len() / 4;

        FormattedContext {
            text,
            estimated_tokens,
            included_count: primary_ordered.len() + expansion_count + ku_grain_count,
            omitted_count,
            truncated: omitted_count > 0,
        }
    }

    /// RF-4 Mode 2: Format hits as a chronological timeline with deltas.
    ///
    /// Runs the standard budget-allocation pipeline, then sorts included
    /// entries by `created_at` ascending and delegates to `render_timeline`.
    fn format_timeline_pass(&self, hits: &[SearchHit], policy: &FormatPolicy) -> FormattedContext {
        // Reuse the standard pipeline for filtering + budget allocation.
        let filtered = self.apply_overrides(hits, policy);
        let filtered = if policy.metadata != super::policy::MetadataLevel::Full {
            self.exclude_superseded_pairs(hits, &filtered)
        } else {
            filtered
        };

        let mut scored: Vec<ScoredEntry> = filtered
            .iter()
            .enumerate()
            .map(|(i, &idx)| {
                let hit = &hits[idx];
                let gt = hit.grain.grain_type;
                let renderer = self.registry.get(gt);
                let (priority, full_tokens) = match renderer {
                    Some(r) => (
                        r.context_priority(&hit.grain, hit),
                        r.token_estimate(&hit.grain, policy),
                    ),
                    None => (0.5, estimate_default_tokens(hit)),
                };
                let summary_tokens = (full_tokens / 3).max(1);
                ScoredEntry {
                    priority,
                    full_tokens,
                    summary_tokens,
                    original_index: i,
                    grain_type: gt,
                }
            })
            .collect();

        let mut allocations = match policy.grain_type_diversity {
            Some(ref diversity) => {
                budget::allocate_with_diversity(&mut scored, policy.token_budget, diversity)
            }
            None => budget::allocate(&mut scored, policy.token_budget),
        };
        strip_summaries_for_structured_formats(&mut allocations, policy);
        let mut included: Vec<(usize, Allocation)> = allocations
            .iter()
            .enumerate()
            .filter(|(_, alloc)| **alloc != Allocation::Omit)
            .map(|(i, alloc)| (filtered[i], *alloc))
            .collect();

        let omitted_count = allocations
            .iter()
            .filter(|a| **a == Allocation::Omit)
            .count();

        // Sort chronologically for timeline rendering.
        included.sort_by_key(|(idx, _)| hits[*idx].grain.header.created_at_sec);

        let text = self.render_timeline(&included, hits, policy);
        let estimated_tokens = text.len() / 4;

        FormattedContext {
            text,
            estimated_tokens,
            included_count: included.len(),
            omitted_count,
            truncated: omitted_count > 0,
        }
    }

    /// Render census grains in a dedicated section with a census header.
    fn format_census_section(
        &self,
        census_hits: &[SearchHit],
        policy: &FormatPolicy,
    ) -> FormattedContext {
        // Format the census hits using the standard pipeline.
        let inner = self.format_single_pass(census_hits, policy);
        if inner.text.is_empty() {
            return inner;
        }

        // Honor the census sub-budget: render only the hits `inner` actually
        // included (its default allocator is priority-prefix). Otherwise the
        // section renders EVERY census hit regardless of budget, and the
        // included/omitted counts returned below (taken from `inner`) disagree
        // with the text a caller sees.
        let shown = &census_hits[..inner.included_count.min(census_hits.len())];

        // Wrap in a census section header.
        let text = match policy.format {
            OutputFormat::Sml => {
                let mut rendered_grains = Vec::new();
                for hit in shown {
                    let session = hit.source_namespace.as_deref().unwrap_or("unknown");
                    let gt = hit.grain.grain_type;
                    let renderer = self.registry.get(gt);
                    let content = match renderer {
                        Some(r) => r.render(&hit.grain, policy),
                        None => format!("[{}]", gt.as_str()),
                    };
                    let tag = gt.as_str();
                    if content.starts_with(&format!("<{}>", tag)) {
                        rendered_grains.push(content.replacen(
                            &format!("<{}>", tag),
                            &format!("<{} session=\"{}\">", tag, session),
                            1,
                        ));
                    } else {
                        rendered_grains.push(content);
                    }
                }
                format!(
                    "<census_results>\n{}\n</census_results>",
                    rendered_grains.join("\n")
                )
            }
            OutputFormat::Markdown => {
                let mut lines = vec!["## Additional Sessions (census)".to_string()];
                for hit in shown {
                    let session = hit.source_namespace.as_deref().unwrap_or("unknown");
                    let gt = hit.grain.grain_type;
                    let renderer = self.registry.get(gt);
                    let content = match renderer {
                        Some(r) => r.render(&hit.grain, policy),
                        None => format!("[{}]", gt.as_str()),
                    };
                    lines.push(format!("{} (from {})", content, session));
                }
                lines.join("\n")
            }
            OutputFormat::PlainText => {
                let mut lines = vec!["=== Additional Sessions (census) ===".to_string()];
                for hit in shown {
                    let session = hit.source_namespace.as_deref().unwrap_or("unknown");
                    let gt = hit.grain.grain_type;
                    let renderer = self.registry.get(gt);
                    let content = match renderer {
                        Some(r) => r.render(&hit.grain, policy),
                        None => format!("[{}]", gt.as_str()),
                    };
                    lines.push(format!("{} [session: {}]", content, session));
                }
                lines.join("\n")
            }
            OutputFormat::Json => {
                // JSON: census grains are in the main array but with recall_source.
                // Re-render each grain as JSON with the recall_source field injected.
                let mut parts = Vec::new();
                for hit in shown {
                    let gt = hit.grain.grain_type;
                    let renderer = self.registry.get(gt);
                    let rendered = match renderer {
                        Some(r) => r.render(&hit.grain, policy),
                        None => format!("[{}]", gt.as_str()),
                    };
                    if let Ok(mut obj) = serde_json::from_str::<serde_json::Value>(&rendered) {
                        if let Some(map) = obj.as_object_mut() {
                            map.insert(
                                "hash".to_string(),
                                serde_json::Value::String(hit.grain.hash.to_hex()),
                            );
                            map.insert(
                                "recall_source".to_string(),
                                serde_json::Value::String("census".to_string()),
                            );
                            if let Some(ref ns) = hit.source_namespace {
                                map.insert(
                                    "source_session".to_string(),
                                    serde_json::Value::String(ns.clone()),
                                );
                            }
                        }
                        parts.push(obj.to_string());
                    } else {
                        parts.push(rendered);
                    }
                }
                format!("[{}]", parts.join(",\n"))
            }
            OutputFormat::Toon => {
                // TOON: separate census section header.
                let mut sections = Vec::new();
                let mut groups: HashMap<GrainType, Vec<&SearchHit>> = HashMap::new();
                let mut type_order: Vec<GrainType> = Vec::new();
                for hit in shown {
                    let gt = hit.grain.grain_type;
                    if !groups.contains_key(&gt) {
                        type_order.push(gt);
                    }
                    groups.entry(gt).or_default().push(hit);
                }
                for gt in &type_order {
                    if let Some(entries) = groups.get(gt) {
                        let mut rows = Vec::new();
                        for hit in entries {
                            let renderer = self.registry.get(*gt);
                            let rendered = match renderer {
                                Some(r) => r.render(&hit.grain, policy),
                                None => format!("[{}]", gt.as_str()),
                            };
                            if !rendered.is_empty() {
                                rows.push(rendered);
                            }
                        }
                        if !rows.is_empty() {
                            let name = plural_name(gt);
                            let cols = super::render::toon_columns(gt).join(",");
                            let header = format!("census_{}[{}]{{{}}}:", name, rows.len(), cols);
                            let mut section = header;
                            for row in &rows {
                                section.push('\n');
                                section.push_str(row);
                            }
                            sections.push(section);
                        }
                    }
                }
                sections.join("\n\n")
            }
        };

        FormattedContext {
            text,
            estimated_tokens: inner.estimated_tokens,
            included_count: inner.included_count,
            omitted_count: inner.omitted_count,
            truncated: inner.truncated,
        }
    }

    // -----------------------------------------------------------------------
    // Core rendering methods
    // -----------------------------------------------------------------------

    /// Apply grain_overrides to filter and cap grain types.
    fn apply_overrides(&self, hits: &[SearchHit], policy: &FormatPolicy) -> Vec<usize> {
        let mut type_counts: HashMap<GrainType, usize> = HashMap::new();
        let mut result = Vec::with_capacity(hits.len());

        for (i, hit) in hits.iter().enumerate() {
            let gt = hit.grain.grain_type;

            // Check per-type override
            if let Some(ovr) = policy.grain_overrides.get(&gt) {
                if !ovr.include {
                    continue;
                }
                if let Some(max) = ovr.max_count {
                    let count = type_counts.entry(gt).or_insert(0);
                    if *count >= max {
                        continue;
                    }
                    *count += 1;
                }
            }

            result.push(i);
        }

        result
    }

    /// RF-2: Exclude superseded grains whose superseder is present in the result set.
    ///
    /// When a grain is marked `SupersessionStatus::Superseded` and its `superseded_by_hash`
    /// is present in the candidate set (marked `Current`), exclude the superseded grain
    /// from rendering — the superseder conveys the same information in its updated form.
    fn exclude_superseded_pairs(&self, hits: &[SearchHit], indices: &[usize]) -> Vec<usize> {
        use std::collections::HashSet;

        let current_hashes: HashSet<areev_core::error::Hash> = indices
            .iter()
            .filter(|&&i| {
                hits[i].supersession_status
                    == Some(areev_cal::store_types::SupersessionStatus::Current)
            })
            .map(|&i| hits[i].hash)
            .collect();

        if current_hashes.is_empty() {
            return indices.to_vec();
        }

        indices
            .iter()
            .filter(|&&i| {
                match &hits[i].supersession_status {
                    Some(areev_cal::store_types::SupersessionStatus::Superseded) => {
                        // Exclude only if superseder is in the result set.
                        !hits[i]
                            .superseded_by_hash
                            .as_ref()
                            .map(|sbh| current_hashes.contains(sbh))
                            .unwrap_or(false)
                    }
                    _ => true,
                }
            })
            .copied()
            .collect()
    }

    /// Sort included entries by the policy ordering.
    fn apply_ordering(
        &self,
        included: &mut [(usize, Allocation)],
        hits: &[SearchHit],
        policy: &FormatPolicy,
    ) {
        match policy.ordering {
            Ordering::ByRelevance => {
                // recall() already returns relevance-ordered, preserve original order
            }
            Ordering::Chronological => {
                included.sort_by_key(|(idx, _)| hits[*idx].grain.header.created_at_sec);
            }
            Ordering::ReverseChronological => {
                included.sort_by(|(a, _), (b, _)| {
                    hits[*b]
                        .grain
                        .header
                        .created_at_sec
                        .cmp(&hits[*a].grain.header.created_at_sec)
                });
            }
            Ordering::ByEntity => {
                // Group by subject field, within each group order by relevance (score).
                included.sort_by(|(a, _), (b, _)| {
                    let subj_a = hits[*a].grain.get_str("subject").unwrap_or("");
                    let subj_b = hits[*b].grain.get_str("subject").unwrap_or("");
                    subj_a.cmp(subj_b).then_with(|| {
                        hits[*b]
                            .score
                            .partial_cmp(&hits[*a].score)
                            .unwrap_or(std::cmp::Ordering::Equal)
                    })
                });
            }
        }
    }

    /// Render entries sequentially without section grouping.
    fn render_flat(
        &self,
        included: &[(usize, Allocation)],
        hits: &[SearchHit],
        policy: &FormatPolicy,
    ) -> String {
        match policy.format {
            OutputFormat::Json => self.render_flat_json(included, hits, policy),
            OutputFormat::Toon => self.render_flat_toon(included, hits, policy),
            _ => {
                let separator = grain_separator(&policy.format);
                let mut parts: Vec<String> = Vec::with_capacity(included.len());
                for &(idx, alloc) in included {
                    let hit = &hits[idx];
                    let rendered = self.render_one(hit, alloc, policy);
                    if !rendered.is_empty() {
                        parts.push(rendered);
                    }
                }
                parts.join(separator)
            }
        }
    }

    /// Render entries grouped by grain type with section headers.
    fn render_grouped(
        &self,
        included: &[(usize, Allocation)],
        hits: &[SearchHit],
        policy: &FormatPolicy,
    ) -> String {
        // Group by grain type, preserving order within each group
        let mut groups: HashMap<GrainType, Vec<(usize, Allocation)>> = HashMap::new();
        for &(idx, alloc) in included {
            groups
                .entry(hits[idx].grain.grain_type)
                .or_default()
                .push((idx, alloc));
        }

        let type_order = if policy.sections.type_order.is_empty() {
            default_type_order()
        } else {
            policy.sections.type_order.clone()
        };

        match policy.format {
            OutputFormat::Json => self.render_grouped_json(&groups, &type_order, hits, policy),
            OutputFormat::Toon => self.render_grouped_toon(&groups, &type_order, hits, policy),
            _ => {
                let mut sections: Vec<String> = Vec::new();
                for gt in &type_order {
                    if let Some(entries) = groups.get(gt) {
                        let header = section_header(&policy.format, gt);
                        let separator = grain_separator(&policy.format);
                        let mut parts: Vec<String> = Vec::with_capacity(entries.len() + 1);
                        parts.push(header);
                        for &(idx, alloc) in entries {
                            let rendered = self.render_one(&hits[idx], alloc, policy);
                            if !rendered.is_empty() {
                                parts.push(rendered);
                            }
                        }
                        // Close SML section tags
                        if policy.format == OutputFormat::Sml {
                            parts.push(format!("</{}>", section_tag(gt)));
                        }
                        sections.push(parts.join(separator));
                    }
                }
                let section_sep = section_separator(&policy.format);
                sections.join(section_sep)
            }
        }
    }

    /// Render a single grain with the appropriate renderer.
    /// Includes the grain hash so auto-recalled context is actionable for supersede.
    fn render_one(&self, hit: &SearchHit, alloc: Allocation, policy: &FormatPolicy) -> String {
        let gt = hit.grain.grain_type;
        let rendered = match self.registry.get(gt) {
            Some(renderer) => match alloc {
                Allocation::Full => renderer.render(&hit.grain, policy),
                Allocation::Summary => renderer.render_summary(&hit.grain, policy),
                Allocation::Omit => return String::new(),
            },
            None => {
                // Fallback: show grain type + fields as plain text
                format!("[{}]", gt.as_str())
            }
        };
        // WI-RENDER: Prefix conflict annotations when metadata is Minimal or Full.
        let conflict_prefix = if policy.metadata != super::policy::MetadataLevel::None {
            match &hit.conflict_status {
                Some(areev_cal::store_types::ConflictStatus::Outdated) => "[OUTDATED] ",
                Some(areev_cal::store_types::ConflictStatus::Current) => "[CURRENT] ",
                None => "",
            }
        } else {
            ""
        };
        // RF-2: Supersession status takes priority over conflict status.
        let supersession_prefix = if policy.metadata != super::policy::MetadataLevel::None {
            match &hit.supersession_status {
                Some(areev_cal::store_types::SupersessionStatus::Superseded) => "[OUTDATED] ",
                Some(areev_cal::store_types::SupersessionStatus::Current) => "[CURRENT] ",
                None => "",
            }
        } else {
            ""
        };
        let status_prefix = if !supersession_prefix.is_empty() {
            supersession_prefix
        } else {
            conflict_prefix
        };
        match &policy.format {
            OutputFormat::Json => {
                let hash_hex = hit.grain.hash.to_hex();
                if let Ok(mut obj) = serde_json::from_str::<serde_json::Value>(&rendered) {
                    if let Some(map) = obj.as_object_mut() {
                        map.insert("hash".to_string(), serde_json::Value::String(hash_hex));
                        if let Some(ref cs) = hit.conflict_status {
                            let status_str = match cs {
                                areev_cal::store_types::ConflictStatus::Current => "current",
                                areev_cal::store_types::ConflictStatus::Outdated => "outdated",
                            };
                            map.insert(
                                "conflict_status".to_string(),
                                serde_json::Value::String(status_str.to_string()),
                            );
                        }
                        if let Some(ref ss) = hit.supersession_status {
                            let status_str = match ss {
                                areev_cal::store_types::SupersessionStatus::Current => "current",
                                areev_cal::store_types::SupersessionStatus::Superseded => {
                                    "superseded"
                                }
                            };
                            map.insert(
                                "supersession_status".to_string(),
                                serde_json::Value::String(status_str.to_string()),
                            );
                        }
                    }
                    obj.to_string()
                } else {
                    rendered
                }
            }
            // TOON is a tabular format whose header declares fixed columns
            // (e.g. facts[N]{subject,content,confidence}); a text status prefix
            // would land inside the first column value ("[CURRENT] john") and
            // corrupt the declared schema. Emit the row unprefixed — status is
            // not part of the TOON column set (JSON carries it structurally).
            OutputFormat::Toon => rendered,
            _ => format!("{status_prefix}{rendered}"),
        }
    }

    /// Render flat JSON array.
    fn render_flat_json(
        &self,
        included: &[(usize, Allocation)],
        hits: &[SearchHit],
        policy: &FormatPolicy,
    ) -> String {
        let mut parts: Vec<String> = Vec::with_capacity(included.len());
        for &(idx, alloc) in included {
            let rendered = self.render_one(&hits[idx], alloc, policy);
            if !rendered.is_empty() {
                parts.push(rendered);
            }
        }
        format!("[{}]", parts.join(",\n"))
    }

    /// Render grouped JSON with type keys.
    fn render_grouped_json(
        &self,
        groups: &HashMap<GrainType, Vec<(usize, Allocation)>>,
        type_order: &[GrainType],
        hits: &[SearchHit],
        policy: &FormatPolicy,
    ) -> String {
        let mut sections: Vec<String> = Vec::new();
        for gt in type_order {
            if let Some(entries) = groups.get(gt) {
                let mut parts: Vec<String> = Vec::with_capacity(entries.len());
                for &(idx, alloc) in entries {
                    let rendered = self.render_one(&hits[idx], alloc, policy);
                    if !rendered.is_empty() {
                        parts.push(rendered);
                    }
                }
                let key = plural_name(gt);
                sections.push(format!("\"{}\":[{}]", key, parts.join(",\n")));
            }
        }
        format!("{{{}}}", sections.join(",\n"))
    }

    /// Render flat TOON in tabular CSV format (CAL spec §10.9.3).
    ///
    /// Even in flat mode, TOON groups by grain type since each type has
    /// different columns. Rows are at depth 0 (no indentation).
    fn render_flat_toon(
        &self,
        included: &[(usize, Allocation)],
        hits: &[SearchHit],
        policy: &FormatPolicy,
    ) -> String {
        // Group by grain type (preserving order of first occurrence)
        let mut groups: HashMap<GrainType, Vec<(usize, Allocation)>> = HashMap::new();
        let mut type_order: Vec<GrainType> = Vec::new();
        for &(idx, alloc) in included {
            let gt = hits[idx].grain.grain_type;
            if !groups.contains_key(&gt) {
                type_order.push(gt);
            }
            groups.entry(gt).or_default().push((idx, alloc));
        }

        let mut sections = Vec::new();
        for gt in &type_order {
            if let Some(entries) = groups.get(gt) {
                let mut rows = Vec::new();
                for &(idx, alloc) in entries {
                    let rendered = self.render_one(&hits[idx], alloc, policy);
                    if !rendered.is_empty() {
                        rows.push(rendered);
                    }
                }
                if !rows.is_empty() {
                    let name = plural_name(gt);
                    let cols = super::render::toon_columns(gt).join(",");
                    let header = format!("{}[{}]{{{}}}:", name, rows.len(), cols);
                    let mut section = header;
                    for row in &rows {
                        section.push('\n');
                        section.push_str(row);
                    }
                    sections.push(section);
                }
            }
        }
        sections.join("\n\n")
    }

    /// Render grouped TOON in tabular CSV format (CAL spec §10.9.4).
    ///
    /// For ASSEMBLE results, rows are indented 2 spaces since they are
    /// named properties of a root-level object document.
    fn render_grouped_toon(
        &self,
        groups: &HashMap<GrainType, Vec<(usize, Allocation)>>,
        type_order: &[GrainType],
        hits: &[SearchHit],
        policy: &FormatPolicy,
    ) -> String {
        let mut sections = Vec::new();
        for gt in type_order {
            if let Some(entries) = groups.get(gt) {
                let mut rows = Vec::new();
                for &(idx, alloc) in entries {
                    let rendered = self.render_one(&hits[idx], alloc, policy);
                    if !rendered.is_empty() {
                        rows.push(rendered);
                    }
                }
                if !rows.is_empty() {
                    let name = plural_name(gt);
                    let cols = super::render::toon_columns(gt).join(",");
                    let header = format!("{}[{}]{{{}}}:", name, rows.len(), cols);
                    let mut section = header;
                    for row in &rows {
                        section.push('\n');
                        // ASSEMBLE: 2-space indent per CAL spec §10.9.4
                        section.push_str("  ");
                        section.push_str(row);
                    }
                    sections.push(section);
                }
            }
        }
        sections.join("\n\n")
    }

    /// Render a chronological timeline with explicit deltas between consecutive events.
    ///
    /// RF-4 timeline mode: sorts grains by `created_at` ascending and inserts
    /// delta annotations (days/months/years) between consecutive entries.
    fn render_timeline(
        &self,
        included: &[(usize, Allocation)],
        hits: &[SearchHit],
        policy: &FormatPolicy,
    ) -> String {
        match policy.format {
            OutputFormat::Json => self.render_timeline_json(included, hits, policy),
            OutputFormat::Toon => self.render_timeline_toon(included, hits, policy),
            OutputFormat::Sml => self.render_timeline_sml(included, hits, policy),
            OutputFormat::Markdown => self.render_timeline_text(included, hits, policy, true),
            OutputFormat::PlainText => self.render_timeline_text(included, hits, policy, false),
        }
    }

    /// Timeline in PlainText/Markdown.
    fn render_timeline_text(
        &self,
        included: &[(usize, Allocation)],
        hits: &[SearchHit],
        policy: &FormatPolicy,
        markdown: bool,
    ) -> String {
        let header = if markdown {
            "## Timeline (earliest to latest)".to_string()
        } else {
            "=== Timeline (earliest to latest) ===".to_string()
        };

        let mut parts = vec![header];
        for (i, &(idx, alloc)) in included.iter().enumerate() {
            let hit = &hits[idx];
            let date = format_epoch_date(hit.grain.header.created_at_sec);
            let rendered = self.render_one(hit, alloc, policy);
            if rendered.is_empty() {
                continue;
            }
            parts.push(format!("{}. {}: {}", i + 1, date, rendered));

            // Delta to next entry
            if i + 1 < included.len() {
                let next_idx = included[i + 1].0;
                let next_ts = hits[next_idx].grain.header.created_at_sec;
                let curr_ts = hit.grain.header.created_at_sec;
                if next_ts > curr_ts {
                    let delta_secs = (next_ts - curr_ts) as u64;
                    let label = format_time_delta(delta_secs);
                    if markdown {
                        parts.push(format!("   \u{2193} *{}*", label));
                    } else {
                        parts.push(format!("   \u{2193} {}", label));
                    }
                }
            }
        }
        parts.join("\n")
    }

    /// Timeline in SML format.
    fn render_timeline_sml(
        &self,
        included: &[(usize, Allocation)],
        hits: &[SearchHit],
        policy: &FormatPolicy,
    ) -> String {
        let mut parts = vec!["<timeline order=\"chronological\">".to_string()];
        for (i, &(idx, alloc)) in included.iter().enumerate() {
            let hit = &hits[idx];
            let rendered = self.render_one(hit, alloc, policy);
            if rendered.is_empty() {
                continue;
            }
            parts.push(rendered);

            // Delta to next entry
            if i + 1 < included.len() {
                let next_idx = included[i + 1].0;
                let next_ts = hits[next_idx].grain.header.created_at_sec;
                let curr_ts = hit.grain.header.created_at_sec;
                if next_ts > curr_ts {
                    let delta_secs = (next_ts - curr_ts) as u64;
                    let days = delta_secs / 86400;
                    let label = format_time_delta(delta_secs);
                    parts.push(format!("<delta days=\"{}\" label=\"{}\"/>", days, label));
                }
            }
        }
        parts.push("</timeline>".to_string());
        parts.join("\n")
    }

    /// Timeline in JSON format.
    fn render_timeline_json(
        &self,
        included: &[(usize, Allocation)],
        hits: &[SearchHit],
        policy: &FormatPolicy,
    ) -> String {
        let mut entries: Vec<String> = Vec::with_capacity(included.len());
        for (i, &(idx, alloc)) in included.iter().enumerate() {
            let hit = &hits[idx];
            let rendered = self.render_one(hit, alloc, policy);
            if rendered.is_empty() {
                continue;
            }
            // Try to inject delta_to_next into the JSON object
            if i + 1 < included.len() {
                let next_idx = included[i + 1].0;
                let next_ts = hits[next_idx].grain.header.created_at_sec;
                let curr_ts = hit.grain.header.created_at_sec;
                if next_ts > curr_ts {
                    let delta_secs = (next_ts - curr_ts) as u64;
                    let days = delta_secs / 86400;
                    let label = format_time_delta(delta_secs);
                    if let Ok(mut obj) = serde_json::from_str::<serde_json::Value>(&rendered) {
                        if let Some(map) = obj.as_object_mut() {
                            let delta = serde_json::json!({
                                "days": days,
                                "label": label,
                            });
                            map.insert("delta_to_next".to_string(), delta);
                        }
                        entries.push(obj.to_string());
                        continue;
                    }
                }
            }
            entries.push(rendered);
        }
        format!("{{\"timeline\":[{}]}}", entries.join(",\n"))
    }

    /// Timeline in TOON format.
    fn render_timeline_toon(
        &self,
        included: &[(usize, Allocation)],
        hits: &[SearchHit],
        policy: &FormatPolicy,
    ) -> String {
        let mut parts = vec![format!("timeline[{}]:", included.len())];
        for (i, &(idx, alloc)) in included.iter().enumerate() {
            let hit = &hits[idx];
            let date = format_epoch_date(hit.grain.header.created_at_sec);
            let rendered = self.render_one(hit, alloc, policy);
            if rendered.is_empty() {
                continue;
            }
            parts.push(format!("  {}: {}", date, rendered));

            // Delta to next entry
            if i + 1 < included.len() {
                let next_idx = included[i + 1].0;
                let next_ts = hits[next_idx].grain.header.created_at_sec;
                let curr_ts = hit.grain.header.created_at_sec;
                if next_ts > curr_ts {
                    let delta_secs = (next_ts - curr_ts) as u64;
                    let label = format_time_delta(delta_secs);
                    parts.push(format!("  \u{2193} {}", label));
                }
            }
        }
        parts.join("\n")
    }

    /// Render the Knowledge Updates section for supersession chains.
    ///
    /// Returns `None` when no chains are present. When `recency` is true,
    /// only the current value is shown (the outdated value is suppressed).
    fn render_knowledge_updates(
        &self,
        chains: &[SupersessionChain],
        format: &OutputFormat,
        recency: bool,
    ) -> Option<String> {
        if chains.is_empty() {
            return None;
        }

        match format {
            OutputFormat::PlainText => {
                let mut lines = vec!["=== Knowledge Updates ===".to_string()];
                for chain in chains {
                    let old_date_str = chain
                        .old_date
                        .as_deref()
                        .map(|d| format!(" ({d})"))
                        .unwrap_or_default();
                    let new_date_str = chain
                        .new_date
                        .as_deref()
                        .map(|d| format!(" ({d})"))
                        .unwrap_or_default();
                    if recency {
                        lines.push(format!(
                            "{}: \"{}\" [CURRENT]",
                            chain.subject, chain.new_value,
                        ));
                    } else {
                        lines.push(format!(
                            "{}: \"{}\"{} → \"{}\"{} [CURRENT]",
                            chain.subject,
                            chain.old_value,
                            old_date_str,
                            chain.new_value,
                            new_date_str,
                        ));
                    }
                }
                Some(lines.join("\n"))
            }
            OutputFormat::Markdown => {
                let mut lines = vec!["## Knowledge Updates".to_string()];
                for chain in chains {
                    let old_date_str = chain
                        .old_date
                        .as_deref()
                        .map(|d| format!(" ({d})"))
                        .unwrap_or_default();
                    let new_date_str = chain
                        .new_date
                        .as_deref()
                        .map(|d| format!(" ({d})"))
                        .unwrap_or_default();
                    if recency {
                        lines.push(format!(
                            "**{}**: **{}**{} [CURRENT]",
                            chain.subject, chain.new_value, new_date_str,
                        ));
                    } else {
                        lines.push(format!(
                            "**{}**: ~~{}~~{} → **{}**{} [CURRENT]",
                            chain.subject,
                            chain.old_value,
                            old_date_str,
                            chain.new_value,
                            new_date_str,
                        ));
                    }
                }
                Some(lines.join("\n"))
            }
            OutputFormat::Sml => {
                let mut lines = vec!["<knowledge_updates>".to_string()];
                for chain in chains {
                    let old_date_attr = chain
                        .old_date
                        .as_deref()
                        .map(|d| format!(" old_date=\"{d}\""))
                        .unwrap_or_default();
                    let new_date_attr = chain
                        .new_date
                        .as_deref()
                        .map(|d| format!(" new_date=\"{d}\""))
                        .unwrap_or_default();
                    if recency {
                        lines.push(format!(
                            "<update subject=\"{}\" new=\"{}\"{} />",
                            chain.subject, chain.new_value, new_date_attr,
                        ));
                    } else {
                        lines.push(format!(
                            "<update subject=\"{}\" old=\"{}\"{} new=\"{}\"{} />",
                            chain.subject,
                            chain.old_value,
                            old_date_attr,
                            chain.new_value,
                            new_date_attr,
                        ));
                    }
                }
                lines.push("</knowledge_updates>".to_string());
                Some(lines.join("\n"))
            }
            OutputFormat::Json => {
                let updates: Vec<serde_json::Value> = chains
                    .iter()
                    .map(|chain| {
                        if recency {
                            serde_json::json!({
                                "subject": chain.subject,
                                "new_value": chain.new_value,
                                "new_date": chain.new_date,
                            })
                        } else {
                            serde_json::json!({
                                "subject": chain.subject,
                                "old_value": chain.old_value,
                                "old_date": chain.old_date,
                                "new_value": chain.new_value,
                                "new_date": chain.new_date,
                            })
                        }
                    })
                    .collect();
                Some(serde_json::to_string(&updates).unwrap_or_else(|_| "[]".to_string()))
            }
            OutputFormat::Toon => {
                let mut lines = vec!["--- knowledge updates ---".to_string()];
                for chain in chains {
                    if recency {
                        lines.push(format!(
                            "{}: \"{}\" [CURRENT]",
                            chain.subject, chain.new_value,
                        ));
                    } else {
                        lines.push(format!(
                            "{}: \"{}\" → \"{}\" [CURRENT]",
                            chain.subject, chain.old_value, chain.new_value,
                        ));
                    }
                }
                Some(lines.join("\n"))
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Detect whether the query has temporal intent, warranting timeline rendering.
///
/// Returns `true` when any of these conditions hold:
/// - `hints.has_temporal_expr` is set (temporal expression parsed from query)
/// - `hints.has_time_range` is set (explicit time_start/time_end)
/// - `hints.query_text` contains temporal-intent patterns (20 patterns)
fn detect_temporal_intent(hints: &RenderingHints, _hits: &[&SearchHit]) -> bool {
    // Existing flag checks
    if hints.has_temporal_expr || hints.has_time_range {
        return true;
    }

    // Query text pattern matching
    if let Some(ref query) = hints.query_text {
        let q = query.to_lowercase();
        let temporal_patterns = [
            "order of",
            "in what order",
            "sequence of",
            "from earliest",
            "from latest",
            "from first",
            "from last",
            "how many days",
            "how long between",
            "time between",
            "which came first",
            "which came last",
            "which was earlier",
            "which was later",
            "before or after",
            "earlier or later",
            "when did",
            "what date",
            "what time",
            "chronolog",
        ];
        if temporal_patterns.iter().any(|p| q.contains(p)) {
            return true;
        }
    }

    false
}

/// Format a duration in seconds into a human-readable delta string.
///
/// - < 90 days: "N days" (or "1 day")
/// - 90–365 days: "N months"
/// - > 365 days: "N years, M months" (or "N years" if M == 0)
fn format_time_delta(seconds: u64) -> String {
    let days = seconds / 86400;
    if days == 0 {
        return "same day".to_string();
    }
    if days < 90 {
        if days == 1 {
            "1 day".to_string()
        } else {
            format!("{} days", days)
        }
    } else if days < 365 {
        let months = days / 30;
        if months == 1 {
            "1 month".to_string()
        } else {
            format!("{} months", months)
        }
    } else {
        let years = days / 365;
        let remaining_months = (days % 365) / 30;
        if remaining_months > 0 {
            if years == 1 {
                format!("1 year, {} months", remaining_months)
            } else {
                format!("{} years, {} months", years, remaining_months)
            }
        } else if years == 1 {
            "1 year".to_string()
        } else {
            format!("{} years", years)
        }
    }
}

/// Default section order per architecture design.
fn default_type_order() -> Vec<GrainType> {
    vec![
        GrainType::State,
        GrainType::Goal,
        GrainType::Fact,
        GrainType::Tool,
        GrainType::Event,
        GrainType::Observation,
        GrainType::Reasoning,
        GrainType::Workflow,
        GrainType::Consensus,
        GrainType::Consent,
        GrainType::Skill,
        GrainType::Recommendation,
    ]
}

/// Section header for a grain type in the given format.
fn section_header(format: &OutputFormat, gt: &GrainType) -> String {
    let name = plural_name(gt);
    match format {
        OutputFormat::Sml => format!("<{}>", section_tag(gt)),
        OutputFormat::Markdown => format!("## {}", capitalize(name)),
        OutputFormat::PlainText => format!("=== {} ===", name.to_uppercase()),
        // SAFETY: render_grouped() matches Json and Toon before calling this function.
        // JSON uses render_grouped_json(), Toon uses render_grouped_toon().
        OutputFormat::Json | OutputFormat::Toon => unreachable!("handled by dedicated methods"),
    }
}

/// SML section tag name (plural, lowercase).
fn section_tag(gt: &GrainType) -> &'static str {
    match gt {
        GrainType::Fact => "facts",
        GrainType::Event => "events",
        GrainType::State => "states",
        GrainType::Workflow => "workflows",
        GrainType::Tool => "tools",
        GrainType::Observation => "observations",
        GrainType::Goal => "goals",
        GrainType::Reasoning => "reasoning",
        GrainType::Consensus => "consensus",
        GrainType::Consent => "consents",
        GrainType::Skill => "skills",
        GrainType::Recommendation => "recommendations",
    }
}

/// Plural display name for section headers.
fn plural_name(gt: &GrainType) -> &'static str {
    section_tag(gt)
}

/// Capitalize first letter.
fn capitalize(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        None => String::new(),
        Some(c) => c.to_uppercase().to_string() + chars.as_str(),
    }
}

/// Separator between grains within a section.
fn grain_separator(format: &OutputFormat) -> &'static str {
    match format {
        OutputFormat::Sml => "\n",
        OutputFormat::Toon => "\n",
        OutputFormat::Markdown => "\n",
        OutputFormat::PlainText => "\n",
        OutputFormat::Json => ",\n",
    }
}

/// Section header for expansion results.
fn expansion_section_header(format: &OutputFormat) -> &'static str {
    match format {
        OutputFormat::Markdown => "## Additional Context (expansion)",
        OutputFormat::PlainText => "=== Additional Context (expansion) ===",
        OutputFormat::Toon => "--- expansion ---",
        // SML and JSON are handled inline in format() with proper wrapping.
        OutputFormat::Sml | OutputFormat::Json => "",
    }
}

/// Separator between sections.
fn section_separator(format: &OutputFormat) -> &'static str {
    match format {
        OutputFormat::Sml => "\n",
        OutputFormat::Toon => "\n\n",
        OutputFormat::Markdown => "\n\n",
        OutputFormat::PlainText => "\n\n",
        // JSON census merges structurally (merge_json_census); this separator
        // is only reached for the non-census JSON paths that never concatenate.
        OutputFormat::Json => ",\n",
    }
}

/// Merge the primary and census JSON documents into ONE valid JSON value.
/// Both sides are normally arrays → concatenate their elements (census grains
/// keep their injected `recall_source`). If the primary is a grouped object,
/// attach the census grains under a `"census"` key. Falls back to the primary
/// alone if either side is unparseable, so the result is always valid JSON.
fn merge_json_census(primary: &str, census: &str) -> String {
    let p = serde_json::from_str::<serde_json::Value>(primary);
    let c = serde_json::from_str::<serde_json::Value>(census);
    match (p, c) {
        (Ok(serde_json::Value::Array(mut pa)), Ok(serde_json::Value::Array(ca))) => {
            pa.extend(ca);
            serde_json::to_string(&serde_json::Value::Array(pa)).unwrap_or_else(|_| primary.to_string())
        }
        (Ok(serde_json::Value::Object(mut po)), Ok(ca)) => {
            po.insert("census".to_string(), ca);
            serde_json::to_string(&serde_json::Value::Object(po)).unwrap_or_else(|_| primary.to_string())
        }
        _ => primary.to_string(),
    }
}

/// Fallback token estimate when no renderer is found.
fn estimate_default_tokens(hit: &SearchHit) -> usize {
    let chars: usize = hit
        .grain
        .fields
        .iter()
        .map(|(k, v)| k.len() + v.to_string().len() + 4)
        .sum();
    (chars + 30) / 4
}

/// RF-4: Format an epoch-seconds timestamp as YYYY-MM-DD.
fn format_epoch_date(epoch_sec: u32) -> String {
    use chrono::{TimeZone, Utc};
    match Utc.timestamp_opt(epoch_sec as i64, 0) {
        chrono::offset::LocalResult::Single(dt) => dt.format("%Y-%m-%d").to_string(),
        _ => format!("epoch:{}", epoch_sec),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::{MetadataLevel, OutputFormat};
    use areev_core::error::Hash;
    use areev_core::format::deserialize::DeserializedGrain;
    use areev_core::format::header::MgHeader;
    use std::collections::HashMap;

    fn make_hit(gt: GrainType, fields: Vec<(&str, &str)>, score: f64) -> SearchHit {
        let header = MgHeader {
            version: 1,
            flags: 0,
            grain_type: gt.type_byte(),
            ns_hash: 0,
            created_at_sec: 1740700000,
        };
        let mut field_map = HashMap::new();
        for (k, v) in fields {
            field_map.insert(k.to_string(), serde_json::Value::String(v.to_string()));
        }
        SearchHit {
            grain: DeserializedGrain {
                header,
                grain_type: gt,
                fields: field_map,
                hash: Hash::from_bytes(&[0u8; 32]),
            },
            score,
            hash: Hash::from_bytes(&[0u8; 32]),
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

    #[test]
    fn test_format_empty_hits() {
        let assembler = ContextAssembler::new();
        let policy = FormatPolicy::default();
        let ctx = assembler.format(&[], &policy);
        assert_eq!(ctx.text, "");
        assert_eq!(ctx.included_count, 0);
        assert_eq!(ctx.omitted_count, 0);
        assert!(!ctx.truncated);
    }

    #[test]
    fn test_format_single_fact_plaintext() {
        let assembler = ContextAssembler::new();
        let policy = FormatPolicy::new(OutputFormat::PlainText).metadata(MetadataLevel::None);
        let hits = vec![make_hit(
            GrainType::Fact,
            vec![
                ("subject", "john"),
                ("relation", "likes"),
                ("object", "coffee"),
            ],
            0.9,
        )];
        let ctx = assembler.format(&hits, &policy);
        assert!(ctx.text.contains("john"));
        assert!(ctx.text.contains("likes"));
        assert!(ctx.text.contains("coffee"));
        assert_eq!(ctx.included_count, 1);
        assert!(!ctx.truncated);
    }

    #[test]
    fn test_format_single_fact_sml() {
        let assembler = ContextAssembler::new();
        let policy = FormatPolicy::new(OutputFormat::Sml).metadata(MetadataLevel::None);
        let hits = vec![make_hit(
            GrainType::Fact,
            vec![
                ("subject", "john"),
                ("relation", "likes"),
                ("object", "coffee"),
            ],
            0.9,
        )];
        let ctx = assembler.format(&hits, &policy);
        assert!(ctx.text.contains("<fact>"));
        assert!(ctx.text.contains("</fact>"));
        assert!(ctx.text.contains("john"));
        assert_eq!(ctx.included_count, 1);
    }

    #[test]
    fn test_format_single_fact_toon() {
        let assembler = ContextAssembler::new();
        let policy = FormatPolicy::new(OutputFormat::Toon).metadata(MetadataLevel::None);
        let hits = vec![make_hit(
            GrainType::Fact,
            vec![
                ("subject", "john"),
                ("relation", "likes"),
                ("object", "coffee"),
            ],
            0.9,
        )];
        let ctx = assembler.format(&hits, &policy);
        // TOON tabular format: header line with columns, then CSV rows
        assert!(
            ctx.text.contains("facts[1]{subject,content,confidence}:"),
            "TOON should have tabular header, got: {}",
            ctx.text
        );
        assert!(
            ctx.text.contains("john,likes coffee"),
            "TOON should have CSV row with john, got: {}",
            ctx.text
        );
        // TOON should NOT contain SML tags or list-item markers
        assert!(
            !ctx.text.contains("<fact>"),
            "TOON should not contain SML tags"
        );
        assert!(
            !ctx.text.contains("  - "),
            "TOON should not contain list-item markers"
        );
        assert_eq!(ctx.included_count, 1);
    }

    #[test]
    fn test_format_multiple_grains_flat() {
        let assembler = ContextAssembler::new();
        let policy = FormatPolicy::new(OutputFormat::PlainText).metadata(MetadataLevel::None);
        let hits = vec![
            make_hit(
                GrainType::Fact,
                vec![
                    ("subject", "john"),
                    ("relation", "likes"),
                    ("object", "coffee"),
                ],
                0.9,
            ),
            make_hit(
                GrainType::Goal,
                vec![("description", "Deploy v2"), ("goal_state", "active")],
                0.8,
            ),
        ];
        let ctx = assembler.format(&hits, &policy);
        assert!(ctx.text.contains("john"));
        assert!(ctx.text.contains("Deploy v2"));
        assert_eq!(ctx.included_count, 2);
    }

    #[test]
    fn test_format_grouped_by_type_sml() {
        let assembler = ContextAssembler::new();
        let policy = FormatPolicy::new(OutputFormat::Sml)
            .metadata(MetadataLevel::None)
            .group_by_type();
        let hits = vec![
            make_hit(
                GrainType::Fact,
                vec![
                    ("subject", "john"),
                    ("relation", "likes"),
                    ("object", "coffee"),
                ],
                0.9,
            ),
            make_hit(
                GrainType::Goal,
                vec![("description", "Deploy v2"), ("goal_state", "active")],
                0.8,
            ),
            make_hit(
                GrainType::Fact,
                vec![("subject", "bob"), ("relation", "likes"), ("object", "tea")],
                0.7,
            ),
        ];
        let ctx = assembler.format(&hits, &policy);
        // Should have section wrappers
        assert!(ctx.text.contains("<facts>"));
        assert!(ctx.text.contains("</facts>"));
        assert!(ctx.text.contains("<goals>"));
        assert!(ctx.text.contains("</goals>"));
        assert_eq!(ctx.included_count, 3);
    }

    #[test]
    fn test_format_grouped_by_type_markdown() {
        let assembler = ContextAssembler::new();
        let policy = FormatPolicy::new(OutputFormat::Markdown)
            .metadata(MetadataLevel::None)
            .group_by_type();
        let hits = vec![
            make_hit(
                GrainType::State,
                vec![("context", "session running")],
                0.9,
            ),
            make_hit(
                GrainType::Goal,
                vec![("description", "Deploy v2"), ("goal_state", "active")],
                0.8,
            ),
        ];
        let ctx = assembler.format(&hits, &policy);
        assert!(ctx.text.contains("## States"));
        assert!(ctx.text.contains("## Goals"));
    }

    #[test]
    fn test_budget_truncation() {
        let assembler = ContextAssembler::new();
        let policy = FormatPolicy::new(OutputFormat::PlainText)
            .metadata(MetadataLevel::None)
            .token_budget(10); // Very tight budget
        let hits = vec![
            make_hit(
                GrainType::Fact,
                vec![
                    ("subject", "john"),
                    ("relation", "likes"),
                    (
                        "object",
                        "coffee with extra long description that should cause truncation",
                    ),
                ],
                0.9,
            ),
            make_hit(
                GrainType::Fact,
                vec![
                    ("subject", "bob"),
                    ("relation", "likes"),
                    ("object", "another long value to exceed budget"),
                ],
                0.5,
            ),
        ];
        let ctx = assembler.format(&hits, &policy);
        assert!(ctx.truncated);
        assert!(ctx.omitted_count > 0);
    }

    #[test]
    fn test_grain_type_override_exclude() {
        let assembler = ContextAssembler::new();
        use crate::policy::GrainTypeOverride;
        let policy = FormatPolicy::new(OutputFormat::PlainText)
            .metadata(MetadataLevel::None)
            .grain_override(
                GrainType::Event,
                GrainTypeOverride {
                    include: false,
                    max_count: None,
                },
            );
        let hits = vec![
            make_hit(
                GrainType::Fact,
                vec![
                    ("subject", "john"),
                    ("relation", "likes"),
                    ("object", "coffee"),
                ],
                0.9,
            ),
            make_hit(
                GrainType::Event,
                vec![("content", "something happened")],
                0.8,
            ),
        ];
        let ctx = assembler.format(&hits, &policy);
        // Event should be excluded
        assert!(!ctx.text.contains("something happened"));
        assert_eq!(ctx.included_count, 1);
    }

    #[test]
    fn test_grain_type_override_max_count() {
        let assembler = ContextAssembler::new();
        use crate::policy::GrainTypeOverride;
        let policy = FormatPolicy::new(OutputFormat::PlainText)
            .metadata(MetadataLevel::None)
            .grain_override(
                GrainType::Fact,
                GrainTypeOverride {
                    include: true,
                    max_count: Some(1),
                },
            );
        let hits = vec![
            make_hit(
                GrainType::Fact,
                vec![
                    ("subject", "john"),
                    ("relation", "likes"),
                    ("object", "coffee"),
                ],
                0.9,
            ),
            make_hit(
                GrainType::Fact,
                vec![("subject", "bob"), ("relation", "likes"), ("object", "tea")],
                0.8,
            ),
        ];
        let ctx = assembler.format(&hits, &policy);
        // Only first fact should be included
        assert!(ctx.text.contains("john"));
        assert!(!ctx.text.contains("bob"));
        assert_eq!(ctx.included_count, 1);
    }

    #[test]
    fn test_chronological_ordering() {
        let assembler = ContextAssembler::new();
        let policy = FormatPolicy::new(OutputFormat::PlainText)
            .metadata(MetadataLevel::None)
            .ordering(Ordering::Chronological);
        let mut hits = vec![
            make_hit(
                GrainType::Fact,
                vec![
                    ("subject", "newer"),
                    ("relation", "is"),
                    ("object", "second"),
                ],
                0.9,
            ),
            make_hit(
                GrainType::Fact,
                vec![
                    ("subject", "older"),
                    ("relation", "is"),
                    ("object", "first"),
                ],
                0.8,
            ),
        ];
        // Set different timestamps
        hits[0].grain.header.created_at_sec = 1740700100;
        hits[1].grain.header.created_at_sec = 1740700000;

        let ctx = assembler.format(&hits, &policy);
        // "older" should appear before "newer" in chronological order
        let older_pos = ctx.text.find("older").unwrap();
        let newer_pos = ctx.text.find("newer").unwrap();
        assert!(
            older_pos < newer_pos,
            "Chronological: older should come first"
        );
    }

    #[test]
    fn test_json_flat_format() {
        let assembler = ContextAssembler::new();
        let policy = FormatPolicy::new(OutputFormat::Json).metadata(MetadataLevel::None);
        let hits = vec![make_hit(
            GrainType::Fact,
            vec![
                ("subject", "john"),
                ("relation", "likes"),
                ("object", "coffee"),
            ],
            0.9,
        )];
        let ctx = assembler.format(&hits, &policy);
        assert!(ctx.text.starts_with('['));
        assert!(ctx.text.ends_with(']'));
        // Per-grain JSON is the shared envelope: hash + grain_type + fields.
        assert!(ctx.text.contains("\"grain_type\":\"fact\""));
        assert!(ctx.text.contains("\"subject\":\"john\""));
    }

    #[test]
    fn test_json_grouped_format() {
        let assembler = ContextAssembler::new();
        let policy = FormatPolicy::new(OutputFormat::Json)
            .metadata(MetadataLevel::None)
            .group_by_type();
        let hits = vec![
            make_hit(
                GrainType::Fact,
                vec![
                    ("subject", "john"),
                    ("relation", "likes"),
                    ("object", "coffee"),
                ],
                0.9,
            ),
            make_hit(
                GrainType::Goal,
                vec![("description", "Deploy v2"), ("goal_state", "active")],
                0.8,
            ),
        ];
        let ctx = assembler.format(&hits, &policy);
        assert!(ctx.text.starts_with('{'));
        assert!(ctx.text.ends_with('}'));
        assert!(ctx.text.contains("\"facts\":"));
        assert!(ctx.text.contains("\"goals\":"));
    }

    #[test]
    fn test_formatted_context_serializable() {
        let ctx = FormattedContext {
            text: "test".to_string(),
            estimated_tokens: 1,
            included_count: 1,
            omitted_count: 0,
            truncated: false,
        };
        let json = serde_json::to_string(&ctx).unwrap();
        assert!(json.contains("\"text\":\"test\""));
        assert!(json.contains("\"truncated\":false"));
    }

    #[test]
    fn test_all_grains_budget_omitted() {
        let assembler = ContextAssembler::new();
        // Budget of 1 token — nothing can fit (diversity disabled for pure budget test).
        let policy = FormatPolicy::new(OutputFormat::PlainText)
            .metadata(MetadataLevel::None)
            .token_budget(1)
            .no_grain_type_diversity();
        let hits = vec![
            make_hit(
                GrainType::Fact,
                vec![
                    ("subject", "john"),
                    ("relation", "likes"),
                    ("object", "coffee"),
                ],
                0.9,
            ),
            make_hit(
                GrainType::Goal,
                vec![("description", "Deploy v2"), ("goal_state", "active")],
                0.8,
            ),
        ];
        let ctx = assembler.format(&hits, &policy);
        assert_eq!(ctx.included_count, 0);
        assert_eq!(ctx.omitted_count, 2);
        assert!(ctx.truncated);
        assert!(ctx.text.is_empty());
    }

    #[test]
    fn test_fallback_renderer_for_unknown_type() {
        // ContextAssembler registers all 10 types, so we can't truly have an
        // unknown type. Instead, test that render_one handles the fallback
        // path by verifying the assembler works with all grain types without panicking.
        let assembler = ContextAssembler::new();
        let policy = FormatPolicy::new(OutputFormat::PlainText).metadata(MetadataLevel::None);
        let all_types = [
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
        ];
        for gt in &all_types {
            let hits = vec![make_hit(*gt, vec![("content", "test")], 0.5)];
            let ctx = assembler.format(&hits, &policy);
            assert_eq!(ctx.included_count, 1, "Failed for {:?}", gt);
            assert!(!ctx.text.is_empty(), "Empty render for {:?}", gt);
        }
    }

    #[test]
    fn test_tight_budget_rendering() {
        let assembler = ContextAssembler::new();
        // Budget tight enough that second grain gets omitted
        let policy = FormatPolicy::new(OutputFormat::PlainText)
            .metadata(MetadataLevel::None)
            .token_budget(100);
        let hits = vec![
            make_hit(
                GrainType::Fact,
                vec![
                    ("subject", "john"),
                    ("relation", "likes"),
                    ("object", "coffee"),
                ],
                0.9,
            ),
            make_hit(
                GrainType::Fact,
                vec![("subject", "bob"), ("relation", "likes"), ("object", "tea")],
                0.5,
            ),
        ];
        let ctx = assembler.format(&hits, &policy);
        // At least one grain should be included
        assert!(ctx.included_count >= 1);
    }

    fn make_census_hit(
        gt: GrainType,
        fields: Vec<(&str, &str)>,
        score: f64,
        session: &str,
    ) -> SearchHit {
        let mut hit = make_hit(gt, fields, score);
        hit.recall_source = Some(RecallSource::Census);
        hit.source_namespace = Some(session.to_string());
        hit
    }

    // -----------------------------------------------------------------------
    // RF-4 Timeline rendering tests
    // -----------------------------------------------------------------------

    fn make_dated_hit(
        gt: GrainType,
        fields: Vec<(&str, &str)>,
        score: f64,
        epoch_secs: u32,
    ) -> SearchHit {
        let mut hit = make_hit(gt, fields, score);
        hit.grain.header.created_at_sec = epoch_secs;
        hit
    }

    #[test]
    fn test_census_grains_tagged() {
        let hit = make_census_hit(
            GrainType::Fact,
            vec![("subject", "bob"), ("relation", "likes"), ("object", "tea")],
            0.5,
            "session-7",
        );
        assert_eq!(hit.recall_source, Some(RecallSource::Census));
        assert_eq!(hit.source_namespace.as_deref(), Some("session-7"));
    }

    #[test]
    fn test_census_section_sml() {
        let assembler = ContextAssembler::new();
        let policy = FormatPolicy::new(OutputFormat::Sml).metadata(MetadataLevel::None);
        let hits = vec![
            make_hit(
                GrainType::Fact,
                vec![
                    ("subject", "john"),
                    ("relation", "likes"),
                    ("object", "coffee"),
                ],
                0.9,
            ),
            make_census_hit(
                GrainType::Fact,
                vec![("subject", "bob"), ("relation", "likes"), ("object", "tea")],
                0.4,
                "session-7",
            ),
        ];
        let ctx = assembler.format(&hits, &policy);
        assert!(
            ctx.text.contains("<census_results>"),
            "SML should have census_results tag, got: {}",
            ctx.text
        );
        assert!(
            ctx.text.contains("</census_results>"),
            "SML should close census_results tag"
        );
        assert!(
            ctx.text.contains("session-7"),
            "SML census should show session"
        );
        assert!(ctx.text.contains("john"), "Primary grain should be present");
        assert!(ctx.text.contains("bob"), "Census grain should be present");
        assert_eq!(ctx.included_count, 2);
    }

    #[test]
    fn test_context_expansion_section_plaintext() {
        let assembler = ContextAssembler::new();
        let policy = FormatPolicy::new(OutputFormat::PlainText).metadata(MetadataLevel::None);
        let mut primary = make_hit(
            GrainType::Fact,
            vec![
                ("subject", "john"),
                ("relation", "likes"),
                ("object", "coffee"),
            ],
            0.9,
        );
        primary.recall_source = Some(areev_cal::store_types::RecallSource::Primary);

        let mut expansion = make_hit(
            GrainType::Fact,
            vec![("subject", "bob"), ("relation", "likes"), ("object", "tea")],
            0.4,
        );
        expansion.recall_source = Some(areev_cal::store_types::RecallSource::Expansion);

        let hits = vec![primary, expansion];
        let ctx = assembler.format(&hits, &policy);
        assert!(ctx.text.contains("john"));
        assert!(ctx.text.contains("Additional Context (expansion)"));
        assert!(ctx.text.contains("bob"));
        assert_eq!(ctx.included_count, 2);
    }

    #[test]
    fn test_census_section_markdown() {
        let assembler = ContextAssembler::new();
        let policy = FormatPolicy::new(OutputFormat::Markdown).metadata(MetadataLevel::None);
        let hits = vec![
            make_hit(
                GrainType::Fact,
                vec![
                    ("subject", "john"),
                    ("relation", "likes"),
                    ("object", "coffee"),
                ],
                0.9,
            ),
            make_census_hit(
                GrainType::Fact,
                vec![("subject", "bob"), ("relation", "likes"), ("object", "tea")],
                0.4,
                "session-7",
            ),
        ];
        let ctx = assembler.format(&hits, &policy);
        assert!(
            ctx.text.contains("## Additional Sessions (census)"),
            "Markdown should have census header, got: {}",
            ctx.text
        );
        assert!(
            ctx.text.contains("(from session-7)"),
            "Markdown census should show session origin"
        );
    }

    // -----------------------------------------------------------------------
    // Knowledge Update chain tests (RQ-5)
    // -----------------------------------------------------------------------

    /// Create a supersession pair: old grain superseded by new grain.
    /// Returns (old_hit, new_hit) with distinct hashes and timestamps.
    fn make_supersession_pair(
        subject: &str,
        old_object: &str,
        new_object: &str,
        old_ts: u32,
        new_ts: u32,
    ) -> (SearchHit, SearchHit) {
        let old_hash = Hash::from_bytes(&[1u8; 32]);
        let new_hash = Hash::from_bytes(&[2u8; 32]);

        let old_header = MgHeader {
            version: 1,
            flags: 0,
            grain_type: GrainType::Fact.type_byte(),
            ns_hash: 0,
            created_at_sec: old_ts,
        };
        let new_header = MgHeader {
            version: 1,
            flags: 0,
            grain_type: GrainType::Fact.type_byte(),
            ns_hash: 0,
            created_at_sec: new_ts,
        };

        let mut old_fields = HashMap::new();
        old_fields.insert("subject".to_string(), serde_json::json!(subject));
        old_fields.insert("relation".to_string(), serde_json::json!("has"));
        old_fields.insert("object".to_string(), serde_json::json!(old_object));

        let mut new_fields = HashMap::new();
        new_fields.insert("subject".to_string(), serde_json::json!(subject));
        new_fields.insert("relation".to_string(), serde_json::json!("has"));
        new_fields.insert("object".to_string(), serde_json::json!(new_object));

        let old_hit = SearchHit {
            grain: DeserializedGrain {
                header: old_header,
                grain_type: GrainType::Fact,
                fields: old_fields,
                hash: old_hash,
            },
            score: 0.8,
            hash: old_hash,
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
            recall_source: None,
            supersession_status: Some(SupersessionStatus::Superseded),
            superseded_by_hash: Some(new_hash),
        };

        let new_hit = SearchHit {
            grain: DeserializedGrain {
                header: new_header,
                grain_type: GrainType::Fact,
                fields: new_fields,
                hash: new_hash,
            },
            score: 0.9,
            hash: new_hash,
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
            recall_source: None,
            supersession_status: Some(SupersessionStatus::Current),
            superseded_by_hash: None,
        };

        (old_hit, new_hit)
    }

    #[test]
    fn test_ku_chain_plaintext() {
        let assembler = ContextAssembler::new();
        let policy = FormatPolicy::new(OutputFormat::PlainText).metadata(MetadataLevel::None);
        // 2023-02-10 = 1675987200, 2023-04-15 = 1681516800
        let (old_hit, new_hit) = make_supersession_pair(
            "therapy_frequency",
            "every two weeks",
            "every week",
            1675987200,
            1681516800,
        );
        let hits = vec![old_hit, new_hit];
        let ctx = assembler.format(&hits, &policy);

        assert!(
            ctx.text.contains("=== Knowledge Updates ==="),
            "Should have KU header, got: {}",
            ctx.text
        );
        assert!(
            ctx.text.contains("therapy_frequency"),
            "Should mention subject, got: {}",
            ctx.text
        );
        assert!(
            ctx.text.contains("every two weeks"),
            "Should mention old value, got: {}",
            ctx.text
        );
        assert!(
            ctx.text.contains("every week"),
            "Should mention new value, got: {}",
            ctx.text
        );
        assert!(
            ctx.text.contains("[CURRENT]"),
            "Should have CURRENT tag, got: {}",
            ctx.text
        );
        assert!(
            ctx.text.contains("→"),
            "Should have arrow between old and new, got: {}",
            ctx.text
        );
    }

    #[test]
    fn test_ku_chain_markdown() {
        let assembler = ContextAssembler::new();
        let policy = FormatPolicy::new(OutputFormat::Markdown).metadata(MetadataLevel::None);
        let (old_hit, new_hit) = make_supersession_pair(
            "therapy_frequency",
            "every two weeks",
            "every week",
            1675987200,
            1681516800,
        );
        let hits = vec![old_hit, new_hit];
        let ctx = assembler.format(&hits, &policy);

        assert!(
            ctx.text.contains("## Knowledge Updates"),
            "Should have markdown KU header, got: {}",
            ctx.text
        );
        assert!(
            ctx.text.contains("~~every two weeks~~"),
            "Should have strikethrough old value, got: {}",
            ctx.text
        );
        assert!(
            ctx.text.contains("**every week**"),
            "Should have bold new value, got: {}",
            ctx.text
        );
        assert!(
            ctx.text.contains("[CURRENT]"),
            "Should have CURRENT tag, got: {}",
            ctx.text
        );
    }

    #[test]
    fn test_ku_chain_sml() {
        let assembler = ContextAssembler::new();
        let policy = FormatPolicy::new(OutputFormat::Sml).metadata(MetadataLevel::None);
        let (old_hit, new_hit) = make_supersession_pair(
            "therapy_frequency",
            "every two weeks",
            "every week",
            1675987200,
            1681516800,
        );
        let hits = vec![old_hit, new_hit];
        let ctx = assembler.format(&hits, &policy);

        assert!(
            ctx.text.contains("<knowledge_updates>"),
            "Should have SML KU opening tag, got: {}",
            ctx.text
        );
        assert!(
            ctx.text.contains("</knowledge_updates>"),
            "Should have SML KU closing tag, got: {}",
            ctx.text
        );
        assert!(
            ctx.text.contains("subject=\"therapy_frequency\""),
            "Should have subject attr, got: {}",
            ctx.text
        );
        assert!(
            ctx.text.contains("old=\"every two weeks\""),
            "Should have old attr, got: {}",
            ctx.text
        );
        assert!(
            ctx.text.contains("new=\"every week\""),
            "Should have new attr, got: {}",
            ctx.text
        );
    }

    #[test]
    fn test_ku_chain_json() {
        let assembler = ContextAssembler::new();
        let policy = FormatPolicy::new(OutputFormat::Json).metadata(MetadataLevel::None);
        let (old_hit, new_hit) = make_supersession_pair(
            "therapy_frequency",
            "every two weeks",
            "every week",
            1675987200,
            1681516800,
        );
        let hits = vec![old_hit, new_hit];
        let ctx = assembler.format(&hits, &policy);

        assert!(
            ctx.text.contains("\"knowledge_updates\""),
            "Should have knowledge_updates key, got: {}",
            ctx.text
        );
        assert!(
            ctx.text.contains("\"subject\":\"therapy_frequency\""),
            "Should have subject in JSON, got: {}",
            ctx.text
        );
        assert!(
            ctx.text.contains("\"old_value\":\"every two weeks\""),
            "Should have old_value in JSON, got: {}",
            ctx.text
        );
        assert!(
            ctx.text.contains("\"new_value\":\"every week\""),
            "Should have new_value in JSON, got: {}",
            ctx.text
        );
        assert!(
            ctx.text.contains("\"context\""),
            "Should have context key, got: {}",
            ctx.text
        );
    }

    #[test]
    fn test_ku_recency_suppression() {
        let assembler = ContextAssembler::new();
        let policy = FormatPolicy::new(OutputFormat::PlainText)
            .metadata(MetadataLevel::None)
            .query_text("what is the therapy frequency currently?");
        let (old_hit, new_hit) = make_supersession_pair(
            "therapy_frequency",
            "every two weeks",
            "every week",
            1675987200,
            1681516800,
        );
        let hits = vec![old_hit, new_hit];
        let ctx = assembler.format(&hits, &policy);

        assert!(
            ctx.text.contains("=== Knowledge Updates ==="),
            "Should have KU header, got: {}",
            ctx.text
        );
        // In recency mode, only the current value is shown.
        assert!(
            ctx.text.contains("every week"),
            "Should contain current value, got: {}",
            ctx.text
        );
        // The old value should NOT appear in the KU section.
        assert!(
            !ctx.text.contains("every two weeks"),
            "Should NOT contain old value in recency mode, got: {}",
            ctx.text
        );
        assert!(
            ctx.text.contains("[CURRENT]"),
            "Should have CURRENT tag, got: {}",
            ctx.text
        );
    }

    #[test]
    fn test_census_section_plaintext() {
        let assembler = ContextAssembler::new();
        let policy = FormatPolicy::new(OutputFormat::PlainText).metadata(MetadataLevel::None);
        let hits = vec![
            make_hit(
                GrainType::Fact,
                vec![
                    ("subject", "john"),
                    ("relation", "likes"),
                    ("object", "coffee"),
                ],
                0.9,
            ),
            make_census_hit(
                GrainType::Fact,
                vec![("subject", "bob"), ("relation", "likes"), ("object", "tea")],
                0.4,
                "session-7",
            ),
        ];
        let ctx = assembler.format(&hits, &policy);
        assert!(
            ctx.text.contains("=== Additional Sessions (census) ==="),
            "PlainText should have census header, got: {}",
            ctx.text
        );
        assert!(
            ctx.text.contains("[session: session-7]"),
            "PlainText census should show session origin"
        );
    }

    #[test]
    fn test_census_empty_suppression() {
        let assembler = ContextAssembler::new();
        let policy = FormatPolicy::new(OutputFormat::PlainText).metadata(MetadataLevel::None);
        // No census grains — should not have census header.
        let hits = vec![make_hit(
            GrainType::Fact,
            vec![
                ("subject", "john"),
                ("relation", "likes"),
                ("object", "coffee"),
            ],
            0.9,
        )];
        let ctx = assembler.format(&hits, &policy);
        assert!(
            !ctx.text.contains("census"),
            "No census grains -> no census section"
        );
    }

    #[test]
    fn test_ku_no_chain_when_no_supersession() {
        let assembler = ContextAssembler::new();
        let policy = FormatPolicy::new(OutputFormat::PlainText).metadata(MetadataLevel::None);
        // Regular hits without supersession status.
        let hits = vec![
            make_hit(
                GrainType::Fact,
                vec![
                    ("subject", "john"),
                    ("relation", "likes"),
                    ("object", "coffee"),
                ],
                0.9,
            ),
            make_hit(
                GrainType::Fact,
                vec![("subject", "bob"), ("relation", "likes"), ("object", "tea")],
                0.8,
            ),
        ];
        let ctx = assembler.format(&hits, &policy);

        // No KU section should appear.
        assert!(
            !ctx.text.contains("Knowledge Updates"),
            "Should NOT have KU section without supersession, got: {}",
            ctx.text
        );
        // Regular grains should still render.
        assert!(ctx.text.contains("john"), "Should contain john");
        assert!(ctx.text.contains("bob"), "Should contain bob");
    }

    #[test]
    fn test_ku_grains_removed_from_main_context() {
        let assembler = ContextAssembler::new();
        let policy = FormatPolicy::new(OutputFormat::PlainText).metadata(MetadataLevel::None);
        let (old_hit, new_hit) = make_supersession_pair(
            "therapy_frequency",
            "every two weeks",
            "every week",
            1675987200,
            1681516800,
        );

        // Add a third non-supersession grain that should appear in main context.
        let extra = make_hit(
            GrainType::Fact,
            vec![
                ("subject", "john"),
                ("relation", "likes"),
                ("object", "coffee"),
            ],
            0.7,
        );
        let hits = vec![old_hit, new_hit, extra];
        let ctx = assembler.format(&hits, &policy);

        // KU section should be present.
        assert!(
            ctx.text.contains("=== Knowledge Updates ==="),
            "Should have KU header, got: {}",
            ctx.text
        );
        // The extra grain should be in the main context.
        assert!(
            ctx.text.contains("john"),
            "Extra grain should be in main context, got: {}",
            ctx.text
        );
        // The KU chain grains should NOT be duplicated in the main context.
        // Count occurrences of the KU subject — it should only appear once (in KU section).
        let ku_count = ctx.text.matches("therapy_frequency").count();
        assert_eq!(
            ku_count, 1,
            "therapy_frequency should appear exactly once (in KU section), found {}",
            ku_count
        );
    }

    #[test]
    fn test_census_json_has_recall_source() {
        let assembler = ContextAssembler::new();
        let policy = FormatPolicy::new(OutputFormat::Json).metadata(MetadataLevel::None);
        let hits = vec![
            make_hit(
                GrainType::Fact,
                vec![
                    ("subject", "john"),
                    ("relation", "likes"),
                    ("object", "coffee"),
                ],
                0.9,
            ),
            make_census_hit(
                GrainType::Fact,
                vec![("subject", "bob"), ("relation", "likes"), ("object", "tea")],
                0.4,
                "session-7",
            ),
        ];
        let ctx = assembler.format(&hits, &policy);
        assert!(
            ctx.text.contains("\"recall_source\":\"census\""),
            "JSON should include recall_source for census grains, got: {}",
            ctx.text
        );
    }

    #[test]
    fn test_ku_toon_format() {
        let assembler = ContextAssembler::new();
        let policy = FormatPolicy::new(OutputFormat::Toon).metadata(MetadataLevel::None);
        let (old_hit, new_hit) = make_supersession_pair(
            "therapy_frequency",
            "every two weeks",
            "every week",
            1675987200,
            1681516800,
        );
        let hits = vec![old_hit, new_hit];
        let ctx = assembler.format(&hits, &policy);

        assert!(
            ctx.text.contains("--- knowledge updates ---"),
            "Should have TOON KU header, got: {}",
            ctx.text
        );
        assert!(
            ctx.text.contains("therapy_frequency"),
            "Should mention subject in TOON, got: {}",
            ctx.text
        );
        assert!(
            ctx.text.contains("\"every two weeks\""),
            "Should have old value in TOON, got: {}",
            ctx.text
        );
        assert!(
            ctx.text.contains("\"every week\""),
            "Should have new value in TOON, got: {}",
            ctx.text
        );
    }

    #[test]
    fn test_census_score_cap() {
        // Verify RecallSource::Census is set and score cap logic is correct.
        // The score cap change is in query.rs apply_namespace_coverage,
        // but we verify the type tagging here.
        let mut hit = make_hit(
            GrainType::Fact,
            vec![
                ("subject", "test"),
                ("relation", "is"),
                ("object", "census"),
            ],
            0.8,
        );
        hit.recall_source = Some(RecallSource::Census);
        assert_eq!(hit.recall_source, Some(RecallSource::Census));

        // Verify primary tagging.
        let mut primary = make_hit(
            GrainType::Fact,
            vec![
                ("subject", "test"),
                ("relation", "is"),
                ("object", "primary"),
            ],
            0.9,
        );
        primary.recall_source = Some(RecallSource::Primary);
        assert_eq!(primary.recall_source, Some(RecallSource::Primary));
    }

    #[test]
    fn test_recall_source_serde_roundtrip() {
        let sources = vec![
            RecallSource::Primary,
            RecallSource::Expansion,
            RecallSource::Census,
        ];
        for source in &sources {
            let json = serde_json::to_string(source).unwrap();
            let back: RecallSource = serde_json::from_str(&json).unwrap();
            assert_eq!(&back, source);
        }
        assert_eq!(
            serde_json::to_string(&RecallSource::Census).unwrap(),
            "\"census\""
        );
        assert_eq!(
            serde_json::to_string(&RecallSource::Primary).unwrap(),
            "\"primary\""
        );
    }

    #[test]
    fn test_context_expansion_section_markdown() {
        let assembler = ContextAssembler::new();
        let policy = FormatPolicy::new(OutputFormat::Markdown).metadata(MetadataLevel::None);
        let mut primary = make_hit(
            GrainType::Fact,
            vec![
                ("subject", "john"),
                ("relation", "likes"),
                ("object", "coffee"),
            ],
            0.9,
        );
        primary.recall_source = Some(areev_cal::store_types::RecallSource::Primary);

        let mut expansion = make_hit(
            GrainType::Fact,
            vec![("subject", "bob"), ("relation", "likes"), ("object", "tea")],
            0.4,
        );
        expansion.recall_source = Some(areev_cal::store_types::RecallSource::Expansion);

        let hits = vec![primary, expansion];
        let ctx = assembler.format(&hits, &policy);
        assert!(ctx.text.contains("## Additional Context (expansion)"));
    }

    #[test]
    fn test_context_expansion_section_sml() {
        let assembler = ContextAssembler::new();
        let policy = FormatPolicy::new(OutputFormat::Sml).metadata(MetadataLevel::None);
        let mut primary = make_hit(
            GrainType::Fact,
            vec![
                ("subject", "john"),
                ("relation", "likes"),
                ("object", "coffee"),
            ],
            0.9,
        );
        primary.recall_source = Some(areev_cal::store_types::RecallSource::Primary);

        let mut expansion = make_hit(
            GrainType::Fact,
            vec![("subject", "bob"), ("relation", "likes"), ("object", "tea")],
            0.4,
        );
        expansion.recall_source = Some(areev_cal::store_types::RecallSource::Expansion);

        let hits = vec![primary, expansion];
        let ctx = assembler.format(&hits, &policy);
        assert!(ctx.text.contains("<expansion_results>"));
    }

    #[test]
    fn test_context_no_expansion_when_all_primary() {
        let assembler = ContextAssembler::new();
        let policy = FormatPolicy::new(OutputFormat::PlainText).metadata(MetadataLevel::None);
        let mut primary = make_hit(
            GrainType::Fact,
            vec![
                ("subject", "john"),
                ("relation", "likes"),
                ("object", "coffee"),
            ],
            0.9,
        );
        primary.recall_source = Some(areev_cal::store_types::RecallSource::Primary);

        let hits = vec![primary];
        let ctx = assembler.format(&hits, &policy);
        assert!(!ctx.text.contains("Additional Context"));
        assert!(ctx.text.contains("john"));
    }

    #[test]
    fn test_temporal_pattern_detection() {
        let patterns = vec![
            "what is the order of events?",
            "in what order did things happen?",
            "sequence of actions taken",
            "from earliest to latest",
            "from latest event",
            "from first to last",
            "how many days between events?",
            "how long between the two meetings?",
            "time between signup and purchase",
            "which came first, A or B?",
            "which came last in the list?",
            "which was earlier, the call or the email?",
            "which was later?",
            "was it before or after the meeting?",
            "earlier or later than Tuesday?",
            "when did they join?",
            "what date was the event?",
            "what time did it start?",
            "show events in chronological order",
        ];

        for pattern in &patterns {
            let hints = RenderingHints {
                entity_count: None,
                entities: None,
                has_temporal_expr: false,
                has_time_range: false,
                query_text: Some(pattern.to_string()),
            };
            let hits = [
                make_dated_hit(GrainType::Event, vec![("content", "A")], 0.9, 1_700_000_000),
                make_dated_hit(GrainType::Event, vec![("content", "B")], 0.8, 1_700_100_000),
            ];
            let refs: Vec<&SearchHit> = hits.iter().collect();
            assert!(
                detect_temporal_intent(&hints, &refs),
                "Pattern '{}' should trigger temporal intent",
                pattern
            );
        }
    }

    #[test]
    fn test_non_temporal_patterns_do_not_trigger() {
        let hints = RenderingHints {
            entity_count: None,
            entities: None,
            has_temporal_expr: false,
            has_time_range: false,
            query_text: Some("what does john like?".to_string()),
        };
        // Hits within same day (< 1 day span)
        let hits = [
            make_dated_hit(
                GrainType::Fact,
                vec![("subject", "john")],
                0.9,
                1_700_000_000,
            ),
            make_dated_hit(
                GrainType::Fact,
                vec![("subject", "bob")],
                0.8,
                1_700_000_100,
            ),
        ];
        let refs: Vec<&SearchHit> = hits.iter().collect();
        assert!(
            !detect_temporal_intent(&hints, &refs),
            "Non-temporal query within same day should not trigger"
        );
    }

    #[test]
    fn test_temporal_flags_trigger() {
        let hints_expr = RenderingHints {
            entity_count: None,
            entities: None,
            has_temporal_expr: true,
            has_time_range: false,
            query_text: None,
        };
        let hints_range = RenderingHints {
            entity_count: None,
            entities: None,
            has_temporal_expr: false,
            has_time_range: true,
            query_text: None,
        };
        let refs: Vec<&SearchHit> = Vec::new();
        assert!(detect_temporal_intent(&hints_expr, &refs));
        assert!(detect_temporal_intent(&hints_range, &refs));
    }

    #[test]
    fn test_delta_formatting() {
        assert_eq!(format_time_delta(0), "same day");
        assert_eq!(format_time_delta(86400), "1 day");
        assert_eq!(format_time_delta(86400 * 5), "5 days");
        assert_eq!(format_time_delta(86400 * 89), "89 days");
        assert_eq!(format_time_delta(86400 * 90), "3 months");
        assert_eq!(format_time_delta(86400 * 180), "6 months");
        assert_eq!(format_time_delta(86400 * 364), "12 months");
        assert_eq!(format_time_delta(86400 * 365), "1 year");
        assert_eq!(format_time_delta(86400 * 500), "1 year, 4 months");
        assert_eq!(format_time_delta(86400 * 730), "2 years");
        assert_eq!(format_time_delta(86400 * 800), "2 years, 2 months");
    }

    #[test]
    fn test_timeline_with_deltas_plaintext() {
        let assembler = ContextAssembler::new();
        let policy = FormatPolicy::new(OutputFormat::PlainText).metadata(MetadataLevel::None);
        let hints = RenderingHints {
            entity_count: None,
            entities: None,
            has_temporal_expr: false,
            has_time_range: false,
            query_text: Some("in what order did events happen?".to_string()),
        };
        // 2023-01-15 = 1673740800, 2023-04-24 = 1682294400, 2023-09-10 = 1694304000
        let hits = vec![
            make_dated_hit(
                GrainType::Event,
                vec![("content", "Joined volleyball league")],
                0.9,
                1_673_740_800,
            ),
            make_dated_hit(
                GrainType::Event,
                vec![("content", "Completed charity 5K run")],
                0.8,
                1_682_294_400,
            ),
            make_dated_hit(
                GrainType::Event,
                vec![("content", "Started yoga classes")],
                0.7,
                1_694_304_000,
            ),
        ];

        let ctx = assembler.format_with_hints(&hits, &policy, &hints);

        // Should contain timeline header
        assert!(
            ctx.text.contains("Timeline (earliest to latest)"),
            "Missing timeline header, got:\n{}",
            ctx.text
        );
        // Should contain all events
        assert!(
            ctx.text.contains("Joined volleyball league"),
            "Missing event 1"
        );
        assert!(
            ctx.text.contains("Completed charity 5K run"),
            "Missing event 2"
        );
        assert!(ctx.text.contains("Started yoga classes"), "Missing event 3");
        // Should contain delta arrows
        assert!(
            ctx.text.contains("\u{2193}"),
            "Missing delta arrows, got:\n{}",
            ctx.text
        );
        // Should include count 3
        assert_eq!(ctx.included_count, 3);
    }

    #[test]
    fn test_timeline_with_deltas_markdown() {
        let assembler = ContextAssembler::new();
        let policy = FormatPolicy::new(OutputFormat::Markdown).metadata(MetadataLevel::None);
        let hints = RenderingHints {
            entity_count: None,
            entities: None,
            has_temporal_expr: false,
            has_time_range: false,
            query_text: Some("when did things happen?".to_string()),
        };
        let hits = vec![
            make_dated_hit(
                GrainType::Event,
                vec![("content", "Event A")],
                0.9,
                1_673_740_800,
            ),
            make_dated_hit(
                GrainType::Event,
                vec![("content", "Event B")],
                0.8,
                1_682_294_400,
            ),
        ];

        let ctx = assembler.format_with_hints(&hits, &policy, &hints);
        // Markdown should use *italic* for delta labels
        assert!(
            ctx.text.contains("*"),
            "Markdown timeline should use italic deltas, got:\n{}",
            ctx.text
        );
        assert!(ctx.text.contains("## Timeline"));
    }

    #[test]
    fn test_timeline_with_deltas_sml() {
        let assembler = ContextAssembler::new();
        let policy = FormatPolicy::new(OutputFormat::Sml).metadata(MetadataLevel::None);
        let hints = RenderingHints {
            entity_count: None,
            entities: None,
            has_temporal_expr: true,
            has_time_range: false,
            query_text: None,
        };
        let hits = vec![
            make_dated_hit(
                GrainType::Event,
                vec![("content", "First")],
                0.9,
                1_673_740_800,
            ),
            make_dated_hit(
                GrainType::Event,
                vec![("content", "Second")],
                0.8,
                1_682_294_400,
            ),
        ];

        let ctx = assembler.format_with_hints(&hits, &policy, &hints);
        assert!(ctx.text.contains("<timeline"), "Missing timeline tag");
        assert!(
            ctx.text.contains("</timeline>"),
            "Missing closing timeline tag"
        );
        assert!(ctx.text.contains("<delta"), "Missing delta element");
    }

    #[test]
    fn test_timeline_with_deltas_json() {
        let assembler = ContextAssembler::new();
        let policy = FormatPolicy::new(OutputFormat::Json).metadata(MetadataLevel::None);
        let hints = RenderingHints {
            entity_count: None,
            entities: None,
            has_temporal_expr: true,
            has_time_range: false,
            query_text: None,
        };
        let hits = vec![
            make_dated_hit(
                GrainType::Event,
                vec![("content", "Alpha")],
                0.9,
                1_673_740_800,
            ),
            make_dated_hit(
                GrainType::Event,
                vec![("content", "Beta")],
                0.8,
                1_682_294_400,
            ),
        ];

        let ctx = assembler.format_with_hints(&hits, &policy, &hints);
        assert!(
            ctx.text.contains("\"timeline\""),
            "Missing timeline key in JSON"
        );
        assert!(
            ctx.text.contains("\"delta_to_next\""),
            "Missing delta_to_next in JSON"
        );
    }

    #[test]
    fn test_timeline_priority_over_relevance() {
        // Even with >10 grains, temporal intent should produce a timeline
        let assembler = ContextAssembler::new();
        let policy = FormatPolicy::new(OutputFormat::PlainText).metadata(MetadataLevel::None);
        let hints = RenderingHints {
            entity_count: None,
            entities: None,
            has_temporal_expr: false,
            has_time_range: false,
            query_text: Some("what is the chronological order?".to_string()),
        };

        let mut hits = Vec::new();
        for i in 0..15 {
            hits.push(make_dated_hit(
                GrainType::Event,
                vec![("content", &format!("Event {}", i))],
                0.9 - (i as f64) * 0.05,
                1_673_740_800 + (i as u32) * 86400 * 30, // ~30 days apart
            ));
        }
        // Leak the format strings so they live long enough
        let hit_contents: Vec<String> = (0..15).map(|i| format!("Event {}", i)).collect();
        let mut hits = Vec::new();
        for (i, content) in hit_contents.iter().enumerate() {
            hits.push(make_dated_hit(
                GrainType::Event,
                vec![("content", content.as_str())],
                0.9 - (i as f64) * 0.05,
                1_673_740_800 + (i as u32) * 86400 * 30,
            ));
        }

        let ctx = assembler.format_with_hints(&hits, &policy, &hints);

        // Should produce timeline format, not standard relevance format
        assert!(
            ctx.text.contains("Timeline (earliest to latest)"),
            "With >10 grains and temporal intent, timeline should take priority, got:\n{}",
            ctx.text
        );
        assert!(ctx.included_count > 10, "Should include all grains");
    }

    #[test]
    fn test_timeline_chronological_order() {
        // Verify events are sorted by created_at regardless of input order
        let assembler = ContextAssembler::new();
        let policy = FormatPolicy::new(OutputFormat::PlainText).metadata(MetadataLevel::None);
        let hints = RenderingHints {
            entity_count: None,
            entities: None,
            has_temporal_expr: false,
            has_time_range: false,
            query_text: Some("in what order did these happen?".to_string()),
        };
        // Insert in reverse order
        let hits = vec![
            make_dated_hit(
                GrainType::Event,
                vec![("content", "Third event")],
                0.9,
                1_694_304_000,
            ),
            make_dated_hit(
                GrainType::Event,
                vec![("content", "First event")],
                0.8,
                1_673_740_800,
            ),
            make_dated_hit(
                GrainType::Event,
                vec![("content", "Second event")],
                0.7,
                1_682_294_400,
            ),
        ];

        let ctx = assembler.format_with_hints(&hits, &policy, &hints);

        let first_pos = ctx.text.find("First event").unwrap();
        let second_pos = ctx.text.find("Second event").unwrap();
        let third_pos = ctx.text.find("Third event").unwrap();
        assert!(
            first_pos < second_pos && second_pos < third_pos,
            "Events should be in chronological order, got:\n{}",
            ctx.text
        );
    }

    #[test]
    fn test_format_epoch_date() {
        assert_eq!(format_epoch_date(0), "1970-01-01");
        assert_eq!(format_epoch_date(1_673_740_800), "2023-01-15");
        assert_eq!(format_epoch_date(1_682_294_400), "2023-04-24");
        assert_eq!(format_epoch_date(1_694_304_000), "2023-09-10");
    }

    #[test]
    fn test_format_with_hints_no_temporal_falls_through() {
        // When no temporal intent, format_with_hints behaves like format
        let assembler = ContextAssembler::new();
        let policy = FormatPolicy::new(OutputFormat::PlainText).metadata(MetadataLevel::None);
        let hints = RenderingHints::default();
        let hits = vec![make_hit(
            GrainType::Fact,
            vec![
                ("subject", "john"),
                ("relation", "likes"),
                ("object", "coffee"),
            ],
            0.9,
        )];

        let ctx_hints = assembler.format_with_hints(&hits, &policy, &hints);
        let ctx_plain = assembler.format(&hits, &policy);

        assert_eq!(ctx_hints.text, ctx_plain.text);
        assert_eq!(ctx_hints.included_count, ctx_plain.included_count);
    }

    #[test]
    fn test_is_recency_query() {
        assert!(is_recency_query(Some(
            "what is the therapy frequency currently?"
        )));
        assert!(is_recency_query(Some("What's the latest status?")));
        assert!(is_recency_query(Some("tell me right now")));
        assert!(is_recency_query(Some("what is my address")));
        assert!(is_recency_query(Some("most recently updated")));
        assert!(is_recency_query(Some("at this point in time")));
        assert!(!is_recency_query(Some("tell me about therapy")));
        assert!(!is_recency_query(Some("history of changes")));
        assert!(!is_recency_query(None));
    }

    #[test]
    fn test_format_timestamp() {
        // 2023-02-10 = 1675987200 seconds since epoch
        assert_eq!(format_timestamp(1675987200), Some("2023-02-10".to_string()));
        // 2023-04-15 = 1681516800 seconds since epoch
        assert_eq!(format_timestamp(1681516800), Some("2023-04-15".to_string()));
        // Zero returns None
        assert_eq!(format_timestamp(0), None);
    }

    // Regression (2026-07-22 combination-hunt).

    /// #7 — census + JSON must be ONE valid JSON array, with the census grains'
    /// recall_source preserved (was two top-level arrays joined by a comma).
    #[test]
    fn census_json_is_valid_and_keeps_recall_source() {
        let assembler = ContextAssembler::new();
        let policy = FormatPolicy::new(OutputFormat::Json).metadata(MetadataLevel::None);
        let mut census = make_hit(
            GrainType::Fact,
            vec![("subject", "bob"), ("relation", "likes"), ("object", "tea")],
            0.4,
        );
        census.recall_source = Some(RecallSource::Census);
        let hits = vec![
            make_hit(
                GrainType::Fact,
                vec![("subject", "john"), ("relation", "likes"), ("object", "coffee")],
                0.9,
            ),
            census,
        ];
        let ctx = assembler.format(&hits, &policy);
        let v: serde_json::Value = serde_json::from_str(&ctx.text)
            .unwrap_or_else(|e| panic!("census+JSON must be valid JSON: {e}\n{}", ctx.text));
        let arr = v.as_array().expect("one top-level array");
        assert_eq!(arr.len(), 2, "primary + census grains in one array");
        assert!(ctx.text.contains("\"recall_source\":\"census\""), "census marker preserved");
    }

    /// #11 — TOON rows must not carry a `[CURRENT]`/`[OUTDATED]` text prefix
    /// that corrupts the declared subject column.
    #[test]
    fn toon_row_has_no_status_prefix() {
        for status in [SupersessionStatus::Current, SupersessionStatus::Superseded] {
            let mut hit = make_hit(
                GrainType::Fact,
                vec![("subject", "john"), ("relation", "likes"), ("object", "coffee")],
                0.9,
            );
            hit.supersession_status = Some(status);
            let ctx = ContextAssembler::new().format(
                &[hit],
                &FormatPolicy::new(OutputFormat::Toon).metadata(MetadataLevel::Minimal),
            );
            assert!(!ctx.text.contains("[CURRENT]"), "no marker in TOON column:\n{}", ctx.text);
            assert!(!ctx.text.contains("[OUTDATED]"), "no marker in TOON column:\n{}", ctx.text);
            assert!(ctx.text.contains("john"), "subject column intact:\n{}", ctx.text);
        }
    }

    /// #A5F4 — the census section renders only its budgeted hits, not all of
    /// them, and stays consistent with the reported included_count.
    #[test]
    fn census_section_honors_budget() {
        let assembler = ContextAssembler::new();
        let policy = FormatPolicy::new(OutputFormat::PlainText)
            .metadata(MetadataLevel::None)
            .token_budget(40); // census gets ~1/5 → not enough for all six
        let mut hits = Vec::new();
        for i in 0..6 {
            let mut h = make_hit(
                GrainType::Fact,
                vec![("subject", "s"), ("relation", "notes"), ("object", "x")],
                0.5,
            );
            h.grain
                .fields
                .insert("object".into(), serde_json::Value::String(format!("censusobject{i}")));
            h.recall_source = Some(RecallSource::Census);
            hits.push(h);
        }
        let ctx = assembler.format(&hits, &policy);
        let rendered = (0..6).filter(|i| ctx.text.contains(&format!("censusobject{i}"))).count();
        assert!(rendered < 6, "census must honor its budget (rendered {rendered}/6):\n{}", ctx.text);
        assert!(rendered > 0, "but some census hits still render");
        assert!(rendered <= ctx.included_count, "text renders no more grains than included_count reports");
    }
}
