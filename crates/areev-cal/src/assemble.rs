//! ASSEMBLE engine — multi-source context assembly with budget allocation.
//!
//! This module handles the execution of multi-source `ASSEMBLE` statements.
//! When an `AssembleStmt` contains `sources` (Phase 2 multi-source syntax),
//! the executor delegates to [`AssembleEngine::execute`] instead of the
//! Phase 1 single-source path.
//!
//! # Budget allocation (CAL spec Section 8.2)
//!
//! Each source gets a proportional share of the total token budget based on
//! PRIORITY weights.  Spec-defined default weights:
//!
//! | Sources | Weights                        |
//! |---------|--------------------------------|
//! | 2       | `[0.65, 0.35]`                 |
//! | 3       | `[0.50, 0.30, 0.20]`           |
//! | 4       | `[0.40, 0.28, 0.20, 0.12]`     |
//! | 5+      | Exponential decay, normalized   |
//!
//! Surplus from sources that produce fewer tokens than allocated is
//! redistributed proportionally to remaining sources (single-pass greedy).
//!
//! # Token counting
//!
//! Delegates to `crate::render::estimate_tokens` — the one `chars / 4`
//! heuristic every budget consumer shares (this engine and the
//! areev-context allocators).
//!
//! # Compliance conditions
//!
//! - **C2-01**: Single-facade invariant.  The ASSEMBLE engine receives a
//!   `&dyn CalStoreFacade` and all source queries execute through it.
//!   Cross-tenant data composition cannot occur because the facade enforces
//!   namespace/user_id scoping at the engine level.
//! - **C2-04**: 1000-grain cap per LET binding (enforced in LetScope).
//!
//! # Security conditions
//!
//! - **S-05**: Per-source timeout at `min(10000/num_sources, 5000)ms`.
//!   Post-dedup grain cap at 2000 grains.
//! - **S-09**: `redact_budget_metadata` on CalExecutorConfig controls
//!   whether per-source token counts are included in the response.

use std::collections::{HashMap, HashSet};
use std::time::Instant;

use super::ast::{AssembleStmt, AssembleWithOption, CalQuery, NamedSource, PrioritySpec};
use super::errors::CalError;
use super::executor::{CalExecutor, CalGrainResult, CalResultPayload};
use super::facade::CalStoreFacade;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Maximum total ASSEMBLE execution time (S-05).
const ASSEMBLE_TIMEOUT_MS: u64 = 10_000;

/// Maximum per-source execution time (S-05).
const MAX_PER_SOURCE_MS: u64 = 5_000;

/// Maximum grains after dedup (S-05).
const MAX_GRAINS_POST_DEDUP: usize = 2_000;

/// Default token budget when BUDGET clause is absent.
const DEFAULT_BUDGET_TOKENS: u32 = 4_000;

// ---------------------------------------------------------------------------
// AssembleEngine
// ---------------------------------------------------------------------------

/// The ASSEMBLE engine — multi-source context assembly with budget allocation.
pub struct AssembleEngine<'a> {
    executor: &'a CalExecutor,
}

/// Result of a multi-source ASSEMBLE execution.
#[derive(Debug)]
pub struct AssembleResult {
    /// Assembled grains after budget and dedup processing.
    pub grains: Vec<CalGrainResult>,
    /// Per-source metadata.
    pub source_meta: Vec<SourceMeta>,
    /// Total tokens used across all sources.
    pub total_tokens: u32,
    /// Budget limit that was applied (if any).
    pub budget_limit: Option<u32>,
    /// Always false (progressive_disclosure has been removed).
    pub progressive: bool,
}

/// Metadata about a single source in the ASSEMBLE result.
#[derive(Debug, Clone, serde::Serialize)]
pub struct SourceMeta {
    /// Source label.
    pub label: String,
    /// Token budget allocated to this source.
    pub tokens_allocated: u32,
    /// Actual tokens used by this source.
    pub tokens_used: u32,
    /// Number of grains contributed by this source.
    pub grain_count: usize,
    /// Grains the budget forced out of this source, kept so a template's
    /// `ELEMENT_OMIT` section can account for them. Render-time only — not
    /// part of the wire payload, so the API shape is unchanged.
    #[serde(skip)]
    pub omitted: Vec<CalGrainResult>,
}

impl<'a> AssembleEngine<'a> {
    /// Create a new ASSEMBLE engine.
    pub fn new(executor: &'a CalExecutor) -> Self {
        Self { executor }
    }

    /// Execute a multi-source ASSEMBLE statement.
    ///
    /// # Flow
    ///
    /// 1. Execute each source query sequentially.
    /// 2. Apply dedup (if `WITH dedup` specified).
    /// 3. Cap total grains at 2000 (S-05).
    /// 4. Allocate token budget across sources.
    /// 5. Trim grains per source to fit allocated budget.
    /// 6. Build metadata.
    pub fn execute(
        &self,
        stmt: &AssembleStmt,
        store: &dyn CalStoreFacade,
        query: &CalQuery,
        warnings: &mut Vec<String>,
    ) -> Result<CalResultPayload, CalError> {
        let start = Instant::now();
        let Some(sources) = &stmt.sources else {
            return Err(CalError::BudgetExceeded {
                detail: "ASSEMBLE engine requires multi-source syntax".into(),
                span: stmt.span,
            });
        };

        if sources.is_empty() {
            return Ok(CalResultPayload::Grains {
                grains: Vec::new(),
                total_available: Some(0),
            });
        }

        let num_sources = sources.len();
        let per_source_timeout_ms =
            (ASSEMBLE_TIMEOUT_MS / num_sources as u64).min(MAX_PER_SOURCE_MS);

        // Check for inconsistent subject scoping across sources.
        // If some sources filter by subject but others don't, warn the user.
        let has_subject_filter: Vec<bool> = sources
            .iter()
            .map(|s| Self::statement_has_subject_filter(&s.query))
            .collect();
        let any_scoped = has_subject_filter.iter().any(|v| *v);
        let all_scoped = has_subject_filter.iter().all(|v| *v);
        if any_scoped && !all_scoped {
            let unscoped: Vec<&str> = sources
                .iter()
                .zip(has_subject_filter.iter())
                .filter(|(_, scoped)| !*scoped)
                .map(|(s, _)| s.label.as_str())
                .collect();
            warnings.push(format!(
                "CAL-W009: ASSEMBLE source(s) [{}] have no subject filter while other sources do — results may include data from unrelated subjects.",
                unscoped.join(", ")
            ));
        }

        // 1. Execute each source and collect results.
        let mut source_results: Vec<(String, Vec<CalGrainResult>)> = Vec::new();

        for source in sources {
            // Check overall timeout.
            if start.elapsed().as_millis() as u64 > ASSEMBLE_TIMEOUT_MS {
                return Err(CalError::QueryTimeout {
                    elapsed_ms: start.elapsed().as_millis() as u64,
                    limit_ms: ASSEMBLE_TIMEOUT_MS,
                    span: stmt.span,
                });
            }

            let source_start = Instant::now();
            let grains = self.execute_source(source, store, query, warnings)?;

            // Check per-source timeout.
            let elapsed = source_start.elapsed().as_millis() as u64;
            if elapsed > per_source_timeout_ms {
                warnings.push(format!(
                    "Source \"{}\" took {}ms (limit: {}ms)",
                    source.label, elapsed, per_source_timeout_ms
                ));
            }

            source_results.push((source.label.clone(), grains));
        }

        // 2. Dedup across sources (if WITH dedup specified).
        let dedup_field = self.extract_dedup_field(&stmt.assemble_with);
        let source_results = if let Some(ref field) = dedup_field {
            self.dedup_across_sources(source_results, field, sources, &stmt.priority)
        } else {
            // Hash-based dedup by default (exact duplicate removal).
            self.dedup_by_hash(source_results)
        };

        // 3. Cap total grains at MAX_GRAINS_POST_DEDUP (S-05).
        let total_grain_count: usize = source_results.iter().map(|(_, g)| g.len()).sum();
        let mut capped_omitted: HashMap<String, Vec<CalGrainResult>> = HashMap::new();
        let source_results = if total_grain_count > MAX_GRAINS_POST_DEDUP {
            warnings.push(format!(
                "Post-dedup grain count ({}) exceeds cap ({}); truncating",
                total_grain_count, MAX_GRAINS_POST_DEDUP
            ));
            self.cap_grains(
                source_results,
                MAX_GRAINS_POST_DEDUP,
                &mut capped_omitted,
            )
        } else {
            source_results
        };

        // 4. Allocate budget.
        let budget_tokens = stmt
            .budget
            .as_ref()
            .map(|b| b.tokens)
            .unwrap_or(DEFAULT_BUDGET_TOKENS);

        let labels: Vec<&str> = source_results.iter().map(|(l, _)| l.as_str()).collect();

        // ── PIN: satisfy the non-degradable sources first ────────────────
        //
        // PRIORITY only *weights* the split, so it can never express "this
        // section must survive verbatim" — a long conversation would summarise
        // away the one instruction that is contractually mandatory. A pinned
        // source is costed in full and taken off the top; PRIORITY then shares
        // whatever is left among the rest. If the pins alone do not fit, the
        // statement fails (CAL-E122) rather than silently degrading them —
        // the whole point is that a quiet partial answer is the unsafe one.
        let pinned_labels: HashSet<&str> = sources
            .iter()
            .filter(|s| s.pinned)
            .map(|s| s.label.as_str())
            .collect();
        let pinned_cost: u32 = source_results
            .iter()
            .filter(|(l, _)| pinned_labels.contains(l.as_str()))
            .map(|(_, g)| g.iter().map(estimate_grain_tokens).sum::<u32>())
            .sum();
        if pinned_cost > budget_tokens {
            let mut names: Vec<String> = source_results
                .iter()
                .filter(|(l, _)| pinned_labels.contains(l.as_str()))
                .map(|(l, _)| l.clone())
                .collect();
            names.sort();
            return Err(CalError::AssemblePinnedBudgetExceeded {
                labels: names,
                required: pinned_cost,
                budget: budget_tokens,
                span: stmt.span,
            });
        }
        let free_budget = budget_tokens - pinned_cost;
        let free_labels: Vec<&str> = labels
            .iter()
            .copied()
            .filter(|l| !pinned_labels.contains(l))
            .collect();
        let allocations = allocate_budget(&free_labels, free_budget, &stmt.priority);
        // Snapshot per-source allocations in source order so the trim loop can
        // take ownership of the grains: `allocations` borrows `labels`, which
        // borrows `source_results`.
        //
        // `Option`, not a `u32` with 0 standing in for "absent": a PRIORITY
        // clause that names some sources gives the rest weight 0.0, and that
        // zero is a deliberate "this source gets nothing". Only a label with no
        // entry at all falls back to an even share.
        let per_source: Vec<Option<u32>> = labels
            .iter()
            .map(|l| allocations.get(l).copied())
            .collect();
        let source_count = source_results.len();

        // 5. Trim grains per source to fit allocated budget (single-pass greedy).
        let mut final_grains: Vec<CalGrainResult> = Vec::new();
        let mut meta: Vec<SourceMeta> = Vec::new();
        let mut remaining_budget = budget_tokens;
        let mut dropped = 0usize; // grains the budget forced us to omit

        for (i, (label, mut grains)) in source_results.into_iter().enumerate() {
            // A pinned source was already costed in full and reserved off the
            // top, so it is handed exactly what it needs and `budget_prefix`
            // keeps all of it. `remaining_budget` still has the reservation in
            // it at this point, so this cannot underflow.
            let is_pinned = pinned_labels.contains(label.as_str());
            let allocated = if is_pinned {
                grains.iter().map(estimate_grain_tokens).sum::<u32>()
            } else {
                per_source
                    .get(i)
                    .copied()
                    .flatten()
                    .unwrap_or(remaining_budget / source_count.max(1) as u32)
            };

            // Use the smaller of allocated or remaining budget — except for a
            // pin, which is never reduced.
            let effective_allocation = if is_pinned {
                allocated
            } else {
                allocated.min(remaining_budget)
            };

            // Split rather than copy: the kept grains and the omitted tail are
            // both already in `grains`, and cloning either would hold a second
            // copy of an assembly's whole result set for the life of the query
            // — which is the opposite of what a budget is for.
            let (keep, tokens_used) = self.budget_prefix(&grains, effective_allocation);
            let budget_omitted = grains.split_off(keep);
            dropped += budget_omitted.len();
            let mut omitted = budget_omitted;
            if let Some(cap_tail) = capped_omitted.remove(&label) {
                dropped += cap_tail.len();
                omitted.extend(cap_tail);
            }
            remaining_budget = remaining_budget.saturating_sub(tokens_used);

            meta.push(SourceMeta {
                label,
                tokens_allocated: effective_allocation,
                tokens_used,
                grain_count: grains.len(),
                omitted,
            });

            final_grains.extend(grains);
        }

        let total_tokens = meta.iter().map(|m| m.tokens_used).sum();

        // Record one budget sample for the `budget_pressure` analyzer (telemetry
        // §8): overflow = the token budget forced grains to be dropped.
        store.note_assembly_budget(dropped > 0);

        // 6. Build result.
        let count = final_grains.len();
        Ok(CalResultPayload::Assembled {
            grains: final_grains,
            sources: meta,
            total_tokens,
            budget_limit: Some(budget_tokens),
            progressive: false,
            total_available: Some(count),
        })
    }

    // -----------------------------------------------------------------------
    // Source execution
    // -----------------------------------------------------------------------

    fn execute_source(
        &self,
        source: &NamedSource,
        store: &dyn CalStoreFacade,
        query: &CalQuery,
        warnings: &mut Vec<String>,
    ) -> Result<Vec<CalGrainResult>, CalError> {
        // A LITERAL source is host text, not a query — nothing to execute, no
        // store access, no timeout. It becomes ONE synthetic result so it
        // flows through dedup, budgeting and rendering exactly like any other
        // section, and therefore lands at its authored position (sources
        // render in FROM-clause order).
        //
        // The empty hash is load-bearing: this text was never stored, has no
        // content address, and must never be mistaken for a grain — dedup
        // keys on the hash and skips it explicitly.
        if let Some(text) = &source.literal {
            return Ok(vec![CalGrainResult {
                hash: String::new(),
                grain_type: "literal".to_string(),
                score: 1.0,
                fields: serde_json::json!({ "content": text }),
                score_breakdown: None,
                explanation: None,
                relative_time: None,
                is_deterministic: true,
                contested_by: None,
            }]);
        }

        // Use per-source WITH options if present, otherwise fall back to parent query's.
        let with_options = if source.with_options.is_empty() {
            query.with_options.clone()
        } else {
            source.with_options.clone()
        };
        let surrogate = CalQuery {
            // Inherit the enclosing query's resolved LET bindings so a source
            // can scope on them: `ASSEMBLE … FROM p: (RECALL … WHERE subject
            // IN $friends)` is the whole point of the two-step pattern.
            let_values: query.let_values.clone(),
            version: query.version,
            statement: *source.query.clone(),
            pipeline: Vec::new(),
            with_options,
            format: None,
            let_bindings: Vec::new(),
            user_vars: std::collections::HashMap::new(),
            warnings: Vec::new(),
        };

        let payload = self.executor.execute_statement_internal(
            &surrogate.statement,
            store,
            &surrogate,
            warnings,
        )?;

        Ok(extract_grains(payload))
    }

    // -----------------------------------------------------------------------
    // Subject-scoping detection
    // -----------------------------------------------------------------------

    /// Check if a CalStatement has a WHERE condition on `subject`.
    fn statement_has_subject_filter(stmt: &crate::ast::CalStatement) -> bool {
        match stmt {
            crate::ast::CalStatement::Recall(recall) => {
                if let Some(ref wc) = recall.where_clause {
                    Self::condition_references_subject(&wc.condition)
                } else {
                    false
                }
            }
            _ => false,
        }
    }

    fn condition_references_subject(cond: &crate::ast::Condition) -> bool {
        match cond {
            crate::ast::Condition::Comparison { field, .. } => field == "subject",
            crate::ast::Condition::In { field, .. } => field == "subject",
            crate::ast::Condition::And { left, right, .. } => {
                Self::condition_references_subject(left)
                    || Self::condition_references_subject(right)
            }
            crate::ast::Condition::Or { left, right, .. } => {
                Self::condition_references_subject(left)
                    || Self::condition_references_subject(right)
            }
            _ => false,
        }
    }

    // -----------------------------------------------------------------------
    // Dedup
    // -----------------------------------------------------------------------

    fn extract_dedup_field(&self, with_options: &[AssembleWithOption]) -> Option<String> {
        if let Some(opt) = with_options.iter().next() {
            let AssembleWithOption::Dedup { field } = opt;
            return field.clone().or_else(|| Some("_hash".to_string()));
        }
        None
    }

    /// Dedup by a specific field.  Within each group (same field value),
    /// keep only the copy from the highest-priority source.
    fn dedup_across_sources(
        &self,
        source_results: Vec<(String, Vec<CalGrainResult>)>,
        dedup_field: &str,
        _sources: &[NamedSource],
        priority: &Option<Vec<PrioritySpec>>,
    ) -> Vec<(String, Vec<CalGrainResult>)> {
        // Build priority ordering: label -> rank (0 = highest priority).
        let priority_map: HashMap<&str, usize> = if let Some(ref specs) = priority {
            // Sort by weight descending.
            let mut sorted: Vec<_> = specs.iter().collect();
            sorted.sort_by(|a, b| {
                b.weight
                    .partial_cmp(&a.weight)
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
            sorted
                .iter()
                .enumerate()
                .map(|(i, s)| (s.label.as_str(), i))
                .collect()
        } else {
            // Natural order = priority order.
            source_results
                .iter()
                .enumerate()
                .map(|(i, (l, _))| (l.as_str(), i))
                .collect()
        };

        // Collect all grains with their source label.
        let mut all_grains: Vec<(usize, String, CalGrainResult)> = Vec::new();
        for (label, grains) in &source_results {
            let rank = priority_map
                .get(label.as_str())
                .copied()
                .unwrap_or(usize::MAX);
            for grain in grains {
                all_grains.push((rank, label.clone(), grain.clone()));
            }
        }

        // Sort by rank (highest priority first = lowest rank number).
        all_grains.sort_by_key(|(rank, _, _)| *rank);

        // Dedup by field value.
        let mut seen: HashSet<String> = HashSet::new();
        let mut deduped: HashMap<String, Vec<CalGrainResult>> = HashMap::new();

        for (_, label, grain) in all_grains {
            let field_val = if dedup_field == "_hash" {
                grain.hash.clone()
            } else {
                grain
                    .fields
                    .get(dedup_field)
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string()
            };

            if seen.insert(field_val) {
                deduped.entry(label).or_default().push(grain);
            }
        }

        // Rebuild in source order.
        source_results
            .iter()
            .map(|(label, _)| {
                let grains = deduped.remove(label).unwrap_or_default();
                (label.clone(), grains)
            })
            .collect()
    }

    /// Hash-based dedup (exact duplicate removal by content-address hash).
    fn dedup_by_hash(
        &self,
        source_results: Vec<(String, Vec<CalGrainResult>)>,
    ) -> Vec<(String, Vec<CalGrainResult>)> {
        let mut seen: HashSet<String> = HashSet::new();
        source_results
            .into_iter()
            .map(|(label, grains)| {
                let deduped: Vec<_> = grains
                    .into_iter()
                    // A LITERAL section has no content address, so every
                    // literal carries the empty hash — deduping on it would
                    // drop every literal after the first.
                    .filter(|g| g.hash.is_empty() || seen.insert(g.hash.clone()))
                    .collect();
                (label, deduped)
            })
            .collect()
    }

    // -----------------------------------------------------------------------
    // Budget trimming
    // -----------------------------------------------------------------------

    /// Trim a grain list to fit within a token budget.
    /// Returns (trimmed grains, tokens actually used).
    fn budget_prefix(&self, grains: &[CalGrainResult], budget: u32) -> (usize, u32) {
        let mut kept = 0usize;
        let mut tokens_used: u32 = 0;

        for grain in grains {
            let grain_tokens = estimate_grain_tokens(grain);
            if tokens_used + grain_tokens > budget && kept > 0 {
                break;
            }
            tokens_used += grain_tokens;
            kept += 1;
        }

        (kept, tokens_used)
    }

    // -----------------------------------------------------------------------
    // Grain capping
    // -----------------------------------------------------------------------

    /// Cap total grains across all sources.  Distributes the cap
    /// proportionally based on each source's original grain count.
    fn cap_grains(
        &self,
        source_results: Vec<(String, Vec<CalGrainResult>)>,
        max_total: usize,
        omitted: &mut HashMap<String, Vec<CalGrainResult>>,
    ) -> Vec<(String, Vec<CalGrainResult>)> {
        let total: usize = source_results.iter().map(|(_, g)| g.len()).sum();
        if total <= max_total {
            return source_results;
        }

        source_results
            .into_iter()
            .map(|(label, mut grains)| {
                let proportion =
                    (grains.len() as f64 / total as f64 * max_total as f64).ceil() as usize;
                let cap = proportion.max(1).min(grains.len());
                let tail = grains.split_off(cap);
                if !tail.is_empty() {
                    omitted.insert(label.clone(), tail);
                }
                (label, grains)
            })
            .collect()
    }
}

// ---------------------------------------------------------------------------
// Budget allocation (pure function)
// ---------------------------------------------------------------------------

/// Compute per-source token budgets from total budget and priority weights.
///
/// Spec-defined default weights (when no PRIORITY clause is given):
///
/// | n sources | Weights                             |
/// |-----------|-------------------------------------|
/// | 1         | `[1.0]`                             |
/// | 2         | `[0.65, 0.35]`                      |
/// | 3         | `[0.50, 0.30, 0.20]`                |
/// | 4         | `[0.40, 0.28, 0.20, 0.12]`          |
/// | 5+        | Exponential decay (C * 0.6^i)       |
///
/// Returns a `HashMap<label, token_allocation>`.
pub fn allocate_budget<'a>(
    labels: &'a [&'a str],
    total_budget: u32,
    priority: &Option<Vec<PrioritySpec>>,
) -> HashMap<&'a str, u32> {
    if labels.is_empty() {
        return HashMap::new();
    }

    let weights = if let Some(ref specs) = priority {
        // Use explicit weights if provided.
        let mut w: Vec<f64> = Vec::new();
        for label in labels {
            let weight = specs
                .iter()
                .find(|s| s.label == *label)
                .map(|s| s.weight)
                .unwrap_or(0.0);
            w.push(weight);
        }
        // Normalize so sum == 1.0.
        let sum: f64 = w.iter().sum();
        if sum > 0.0 {
            w.iter().map(|v| v / sum).collect()
        } else {
            default_weights(labels.len())
        }
    } else {
        default_weights(labels.len())
    };

    let mut allocations = HashMap::new();
    for (i, label) in labels.iter().enumerate() {
        let tokens = (weights[i] * total_budget as f64).round() as u32;
        allocations.insert(*label, tokens);
    }

    allocations
}

/// Generate default weights for `n` sources per CAL spec.
fn default_weights(n: usize) -> Vec<f64> {
    match n {
        0 => vec![],
        1 => vec![1.0],
        2 => vec![0.65, 0.35],
        3 => vec![0.50, 0.30, 0.20],
        4 => vec![0.40, 0.28, 0.20, 0.12],
        _ => {
            // Exponential decay: w_i = C * 0.6^i, normalized.
            let decay = 0.6_f64;
            let raw: Vec<f64> = (0..n).map(|i| decay.powi(i as i32)).collect();
            let sum: f64 = raw.iter().sum();
            raw.iter().map(|v| v / sum).collect()
        }
    }
}

// ---------------------------------------------------------------------------
// Token estimation
// ---------------------------------------------------------------------------

/// Estimate the token count for a single grain via the one shared estimator
/// (`crate::render::estimate_tokens`), so a `BUDGET` means the same thing
/// here as it does on the areev-context allocation path.
pub fn estimate_grain_tokens(grain: &CalGrainResult) -> u32 {
    let view = crate::render::GrainView {
        grain_type: &grain.grain_type,
        hash: &grain.hash,
        fields: &grain.fields,
        created_at_sec: crate::render::created_at_sec_from_fields(&grain.fields),
    };
    crate::render::estimate_tokens(&view, crate::render::MetadataDetail::None) as u32
}

// ---------------------------------------------------------------------------
// Helper: extract grains from a CalResultPayload
// ---------------------------------------------------------------------------

fn extract_grains(payload: CalResultPayload) -> Vec<CalGrainResult> {
    match payload {
        CalResultPayload::Grains { grains, .. } => grains,
        _ => Vec::new(),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_weights_1_source() {
        let w = default_weights(1);
        assert_eq!(w, vec![1.0]);
    }

    #[test]
    fn test_default_weights_2_sources() {
        let w = default_weights(2);
        assert_eq!(w, vec![0.65, 0.35]);
    }

    #[test]
    fn test_default_weights_3_sources() {
        let w = default_weights(3);
        assert_eq!(w, vec![0.50, 0.30, 0.20]);
    }

    #[test]
    fn test_default_weights_4_sources() {
        let w = default_weights(4);
        assert_eq!(w, vec![0.40, 0.28, 0.20, 0.12]);
    }

    #[test]
    fn test_default_weights_5_sources() {
        let w = default_weights(5);
        assert_eq!(w.len(), 5);
        let sum: f64 = w.iter().sum();
        assert!((sum - 1.0).abs() < 0.001);
        // Each weight should be smaller than the previous.
        for i in 1..w.len() {
            assert!(w[i] < w[i - 1]);
        }
    }

    #[test]
    fn test_default_weights_8_sources() {
        let w = default_weights(8);
        assert_eq!(w.len(), 8);
        let sum: f64 = w.iter().sum();
        assert!((sum - 1.0).abs() < 0.001);
    }

    #[test]
    fn test_allocate_budget_no_priority() {
        let labels = vec!["facts", "goals"];
        let allocs = allocate_budget(&labels, 2000, &None);
        assert_eq!(*allocs.get("facts").unwrap(), 1300); // 0.65 * 2000
        assert_eq!(*allocs.get("goals").unwrap(), 700); // 0.35 * 2000
    }

    #[test]
    fn test_allocate_budget_with_priority() {
        let labels = vec!["a", "b"];
        let priority = Some(vec![
            PrioritySpec {
                label: "a".into(),
                weight: 0.8,
                span: None,
            },
            PrioritySpec {
                label: "b".into(),
                weight: 0.2,
                span: None,
            },
        ]);
        let allocs = allocate_budget(&labels, 1000, &priority);
        assert_eq!(*allocs.get("a").unwrap(), 800);
        assert_eq!(*allocs.get("b").unwrap(), 200);
    }

    #[test]
    fn test_allocate_budget_single_source() {
        let labels = vec!["only"];
        let allocs = allocate_budget(&labels, 3000, &None);
        assert_eq!(*allocs.get("only").unwrap(), 3000);
    }

    #[test]
    fn test_allocate_budget_empty() {
        let labels: Vec<&str> = vec![];
        let allocs = allocate_budget(&labels, 1000, &None);
        assert!(allocs.is_empty());
    }

    #[test]
    fn test_estimate_grain_tokens() {
        let grain = CalGrainResult {
            hash: "abc123".into(),
            grain_type: "fact".into(),
            score: 1.0,
            fields: serde_json::json!({"subject": "john", "relation": "likes", "object": "coffee"}),
            score_breakdown: None,
            explanation: None,
            relative_time: None,
            is_deterministic: false,
            contested_by: None,
        };
        let tokens = estimate_grain_tokens(&grain);
        // Should be roughly chars / 4, at least 1.
        assert!(tokens >= 1);
        assert!(tokens < 100); // Sanity check.
    }

    #[test]
    fn test_allocate_budget_3_sources_no_priority() {
        let labels = vec!["a", "b", "c"];
        let allocs = allocate_budget(&labels, 1000, &None);
        assert_eq!(*allocs.get("a").unwrap(), 500);
        assert_eq!(*allocs.get("b").unwrap(), 300);
        assert_eq!(*allocs.get("c").unwrap(), 200);
    }
}
