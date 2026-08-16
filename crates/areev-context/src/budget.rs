//! Token budget allocation.
//!
//! Given a budget and scored grains, decides which get full rendering,
//! which degrade to a compact summary, and which are omitted — progressive
//! disclosure: Full while under ~70% of the budget, then Summary while
//! under ~95%, then Omit. Degrading beats dropping: a grain that survives
//! as one line still tells the model it exists.
//!
//! Two allocation strategies:
//! - `allocate()` — pure priority-based allocation.
//! - `allocate_with_diversity()` — reserves slots per grain type before
//!   filling by priority, ensuring rare types are not crowded out.

use std::collections::{HashMap, HashSet};

use areev_cal::store_types::GrainTypeDiversityConfig;
use areev_core::types::GrainType;

/// Allocation decision for a single grain.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Allocation {
    /// Include with full render.
    Full,
    /// Include with compact summary render.
    Summary,
    /// Omit entirely (budget exhausted).
    Omit,
}

/// A grain scored and measured for budget allocation.
pub struct ScoredEntry {
    pub priority: f32,
    pub full_tokens: usize,
    pub summary_tokens: usize,
    /// Original index in the input slice (for stable ordering after sort).
    pub original_index: usize,
    /// Grain type — used by `allocate_with_diversity()` for type-aware reservations.
    pub grain_type: GrainType,
}

/// Allocate budget across grains using priority-based allocation.
///
/// Algorithm:
/// 1. Sort entries by priority descending.
/// 2. Allocate Full to highest-priority grains until ~70% budget consumed.
/// 3. Degrade to Summary while under ~95% of the budget.
/// 4. Remaining: Omit.
///
/// When token_budget is None, everything gets Full.
///
/// Returns a `Vec<Allocation>` aligned with the *original* entry order (by original_index),
/// not the sorted order. This allows callers to zip allocations with their input slice.
pub fn allocate(entries: &mut [ScoredEntry], token_budget: Option<usize>) -> Vec<Allocation> {
    let n = entries.len();
    if n == 0 {
        return Vec::new();
    }

    let Some(budget) = token_budget else {
        // No budget: everything gets Full
        return vec![Allocation::Full; n];
    };

    // Sort by priority descending (stable sort preserves original_index order for ties)
    entries.sort_by(|a, b| {
        b.priority
            .partial_cmp(&a.priority)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    let full_threshold = budget * 70 / 100;
    let summary_threshold = budget * 95 / 100;

    // Allocate by sorted order, then scatter results back to original positions
    let mut result_by_original = vec![Allocation::Omit; n];
    let mut used = 0usize;

    for entry in entries.iter() {
        if used + entry.full_tokens <= full_threshold {
            result_by_original[entry.original_index] = Allocation::Full;
            used += entry.full_tokens;
        } else if used + entry.summary_tokens <= summary_threshold {
            result_by_original[entry.original_index] = Allocation::Summary;
            used += entry.summary_tokens;
        }
        // else: stays Omit
    }

    result_by_original
}

/// Allocate budget with grain type diversity floor.
///
/// Guarantees a minimum number of grains per represented grain type are
/// included before filling the remaining budget by priority. This prevents
/// dominant types from crowding out rarer types that carry unique signal.
///
/// ## Algorithm (5 phases)
///
/// 1. **Group** entries by grain type.
/// 2. **Reserve** up to `min_per_type` highest-priority entries per type.
/// 3. **Trim** if reserved tokens exceed `max_reservation_pct * budget`
///    (remove lowest-priority reservations, but keep at least 1 per type).
/// 4. **Mark** reserved entries as `Full`.
/// 5. **Fill** remaining budget with standard priority sort (70% threshold).
///
/// When `token_budget` is `None`, everything gets `Full` (same as `allocate`).
pub fn allocate_with_diversity(
    entries: &mut [ScoredEntry],
    token_budget: Option<usize>,
    diversity: &GrainTypeDiversityConfig,
) -> Vec<Allocation> {
    let n = entries.len();
    if n == 0 {
        return Vec::new();
    }

    let Some(budget) = token_budget else {
        return vec![Allocation::Full; n];
    };

    // Phase 1: Group entry indices by grain type.
    let mut groups: HashMap<GrainType, Vec<usize>> = HashMap::new();
    for (i, entry) in entries.iter().enumerate() {
        groups.entry(entry.grain_type).or_default().push(i);
    }

    // Sort each group by priority descending.
    for indices in groups.values_mut() {
        indices.sort_by(|&a, &b| {
            entries[b]
                .priority
                .partial_cmp(&entries[a].priority)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
    }

    // Phase 2: Reserve up to min_per_type highest-priority entries per type.
    let mut reserved: HashSet<usize> = HashSet::new();
    for indices in groups.values() {
        let take = (diversity.min_per_type as usize).min(indices.len());
        for &idx in &indices[..take] {
            reserved.insert(idx);
        }
    }

    // Phase 3: Trim if reserved tokens exceed cap.
    let cap = (budget as f64 * diversity.max_reservation_pct as f64) as usize;
    let reserved_tokens: usize = reserved.iter().map(|&i| entries[i].full_tokens).sum();

    if reserved_tokens > cap {
        // Sort reserved by priority ASC (lowest priority removed first).
        let mut removable: Vec<usize> = reserved.iter().copied().collect();
        removable.sort_by(|&a, &b| {
            entries[a]
                .priority
                .partial_cmp(&entries[b].priority)
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        // Count reserved entries per type to enforce keep-at-least-1.
        let mut type_counts: HashMap<GrainType, usize> = HashMap::new();
        for &idx in &reserved {
            *type_counts.entry(entries[idx].grain_type).or_insert(0) += 1;
        }

        let mut current_tokens = reserved_tokens;
        for &idx in &removable {
            if current_tokens <= cap {
                break;
            }
            let gt = entries[idx].grain_type;
            let count = type_counts.get(&gt).copied().unwrap_or(0);
            if count <= 1 {
                continue; // Keep at least 1 per type.
            }
            reserved.remove(&idx);
            current_tokens -= entries[idx].full_tokens;
            type_counts.insert(gt, count - 1);
        }
    }

    // Phase 4: Mark reserved entries as Full.
    let mut result = vec![Allocation::Omit; n];
    let reserved_used: usize = reserved.iter().map(|&i| entries[i].full_tokens).sum();
    for &i in &reserved {
        result[entries[i].original_index] = Allocation::Full;
    }

    // Phase 5: Fill remaining budget by priority — Full under the ~70%
    // threshold, degrading to Summary under ~95%, then Omit.
    let remaining_budget = budget.saturating_sub(reserved_used);
    let threshold = remaining_budget * 70 / 100;
    let summary_threshold = remaining_budget * 95 / 100;

    // Collect non-reserved indices, sorted by priority descending.
    let mut non_reserved: Vec<usize> = (0..n).filter(|i| !reserved.contains(i)).collect();
    non_reserved.sort_by(|&a, &b| {
        entries[b]
            .priority
            .partial_cmp(&entries[a].priority)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    let mut used = 0usize;
    for idx in non_reserved {
        if used + entries[idx].full_tokens <= threshold {
            result[entries[idx].original_index] = Allocation::Full;
            used += entries[idx].full_tokens;
        } else if used + entries[idx].summary_tokens <= summary_threshold {
            result[entries[idx].original_index] = Allocation::Summary;
            used += entries[idx].summary_tokens;
        }
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper to create a `ScoredEntry` with default grain type (Fact).
    fn entry(priority: f32, full_tokens: usize, summary_tokens: usize, idx: usize) -> ScoredEntry {
        ScoredEntry {
            priority,
            full_tokens,
            summary_tokens,
            original_index: idx,
            grain_type: GrainType::Fact,
        }
    }

    /// Helper to create a `ScoredEntry` with a specific grain type.
    fn typed_entry(
        priority: f32,
        full_tokens: usize,
        grain_type: GrainType,
        idx: usize,
    ) -> ScoredEntry {
        ScoredEntry {
            priority,
            full_tokens,
            summary_tokens: full_tokens / 3,
            original_index: idx,
            grain_type,
        }
    }

    #[test]
    fn test_no_budget_all_full() {
        let mut entries = vec![entry(0.5, 100, 30, 0), entry(0.8, 200, 60, 1)];
        let allocs = allocate(&mut entries, None);
        assert_eq!(allocs, vec![Allocation::Full, Allocation::Full]);
    }

    #[test]
    fn test_empty_entries() {
        let mut entries: Vec<ScoredEntry> = Vec::new();
        let allocs = allocate(&mut entries, Some(1000));
        assert!(allocs.is_empty());
    }

    #[test]
    fn test_budget_overflow_degrades_then_omits() {
        let mut entries = vec![entry(0.9, 800, 200, 0), entry(0.5, 800, 200, 1)];
        // Budget=300: full_threshold=210, summary_threshold=285.
        // Entry 0: 800 > 210 -> summary 200 <= 285 -> Summary, used=200.
        // Entry 1: full and summary both exceed what's left -> Omit.
        let allocs = allocate(&mut entries, Some(300));
        assert_eq!(allocs[0], Allocation::Summary);
        assert_eq!(allocs[1], Allocation::Omit);
    }

    #[test]
    fn test_budget_full_then_summary() {
        let mut entries = vec![
            entry(0.9, 500, 150, 0),
            entry(0.7, 500, 150, 1),
            entry(0.3, 500, 150, 2),
        ];
        // Budget=1000: full_threshold=700, summary_threshold=950.
        // Entry 0 (pri 0.9): 500 <= 700 -> Full, used=500
        // Entry 1 (pri 0.7): full would blow 700; summary 500+150=650 <= 950 -> Summary
        // Entry 2 (pri 0.3): summary 650+150=800 <= 950 -> Summary
        let allocs = allocate(&mut entries, Some(1000));
        assert_eq!(allocs[0], Allocation::Full);
        assert_eq!(allocs[1], Allocation::Summary);
        assert_eq!(allocs[2], Allocation::Summary);
    }

    #[test]
    fn test_priority_ordering_respected() {
        let mut entries = vec![entry(0.3, 100, 30, 0), entry(0.9, 100, 30, 1)];
        // Budget=200: full_threshold=140. Entry with pri 0.9 gets Full first (100 <= 140).
        // Entry with pri 0.3: full would blow 140; summary 100+30=130 <= 190 -> Summary.
        let allocs = allocate(&mut entries, Some(200));
        // original_index 1 (high pri) -> Full, original_index 0 (low pri) degrades.
        assert_eq!(allocs[0], Allocation::Summary);
        assert_eq!(allocs[1], Allocation::Full);
    }

    #[test]
    fn test_exceeds_full_threshold_degrades_to_summary() {
        let mut entries = vec![entry(0.9, 600, 100, 0), entry(0.5, 600, 100, 1)];
        // Budget=1000: full_threshold=700. Entry 0: 600 <= 700 -> Full.
        // Entry 1: full 600+600 blows 700; summary 600+100=700 <= 950 -> Summary.
        let allocs = allocate(&mut entries, Some(1000));
        assert_eq!(allocs[0], Allocation::Full);
        assert_eq!(allocs[1], Allocation::Summary);
    }

    // -----------------------------------------------------------------------
    // Diversity allocation tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_diversity_no_budget_all_full() {
        let mut entries = vec![
            typed_entry(0.9, 100, GrainType::Fact, 0),
            typed_entry(0.5, 100, GrainType::Goal, 1),
        ];
        let cfg = GrainTypeDiversityConfig::default();
        let allocs = allocate_with_diversity(&mut entries, None, &cfg);
        assert_eq!(allocs, vec![Allocation::Full, Allocation::Full]);
    }

    #[test]
    fn test_diversity_empty() {
        let mut entries: Vec<ScoredEntry> = Vec::new();
        let cfg = GrainTypeDiversityConfig::default();
        let allocs = allocate_with_diversity(&mut entries, Some(1000), &cfg);
        assert!(allocs.is_empty());
    }

    #[test]
    fn test_diversity_reserves_rare_type() {
        // 3 high-priority Facts + 1 low-priority Goal.
        // Without diversity, Goal (pri 0.1) would be omitted.
        // With diversity, Goal is reserved.
        let mut entries = vec![
            typed_entry(0.9, 200, GrainType::Fact, 0),
            typed_entry(0.8, 200, GrainType::Fact, 1),
            typed_entry(0.7, 200, GrainType::Fact, 2),
            typed_entry(0.1, 200, GrainType::Goal, 3),
        ];
        let cfg = GrainTypeDiversityConfig {
            min_per_type: 1,
            max_reservation_pct: 0.50,
        };
        // Budget=1000, cap=500 tokens for reservations.
        // Reservations: 1 Fact (pri 0.9, 200 tok) + 1 Goal (pri 0.1, 200 tok) = 400 <= 500.
        // Remaining budget = 1000-400 = 600, threshold = 420.
        // Non-reserved sorted by pri: Fact(0.8, 200), Fact(0.7, 200).
        // 200 <= 420 -> Full, 200+200=400 <= 420 -> Full.
        let allocs = allocate_with_diversity(&mut entries, Some(1000), &cfg);
        assert_eq!(allocs[3], Allocation::Full, "Goal should be reserved");
        assert_eq!(allocs[0], Allocation::Full, "Top Fact should be reserved");
    }

    #[test]
    fn test_diversity_trims_when_over_cap() {
        // 3 types, each with 1 grain of 300 tokens. Budget=1000, cap=0.05 -> 50 tokens.
        // Reservations would be 900 tokens, far over 50. Trim to 1 per type.
        // Since each grain is 300 > cap, trimming stops when only 1 per type remains.
        let mut entries = vec![
            typed_entry(0.9, 300, GrainType::Fact, 0),
            typed_entry(0.5, 300, GrainType::Event, 1),
            typed_entry(0.1, 300, GrainType::Goal, 2),
        ];
        let cfg = GrainTypeDiversityConfig {
            min_per_type: 1,
            max_reservation_pct: 0.05, // Very tight cap: 50 tokens
        };
        let allocs = allocate_with_diversity(&mut entries, Some(1000), &cfg);
        // Even with tight cap, at least 1 per type is kept.
        // Total reserved = 900 > 50, but can't trim below 1 per type.
        // All 3 reserved as Full. Remaining budget = 1000-900=100, threshold=70.
        // No non-reserved entries.
        assert_eq!(allocs[0], Allocation::Full);
        assert_eq!(allocs[1], Allocation::Full);
        assert_eq!(allocs[2], Allocation::Full);
    }
}
