//! Snapshot tests locking the exact rendered output of `ContextAssembler`
//! across every output format (SML / TOON / Markdown / JSON), plus a
//! budget-constrained case that locks truncation behavior.
//!
//! The fixtures are FIXED and deterministic — no clock, no randomness. Every
//! grain carries an explicit `created_at_sec` and a distinct content hash so
//! the rendered strings (including the hashes injected into JSON output and the
//! `date="2025-02-19"` metadata attributes) are stable across runs.
//!
//! Regenerate the `.snap` files after an intentional rendering change with:
//! `INSTA_UPDATE=always cargo test -p areev-context`

use std::collections::HashMap;

use areev_cal::store_types::SearchHit;
use areev_context::{ContextAssembler, FormatPolicy, FormattedContext, MetadataLevel, OutputFormat};
use areev_core::error::Hash;
use areev_core::format::deserialize::DeserializedGrain;
use areev_core::format::header::MgHeader;
use areev_core::types::GrainType;

/// Fixed timestamp for every fixture grain: 2025-02-19 (UTC).
const CREATED_AT: u32 = 1_740_000_000;

/// Build a fixture grain with a fixed header and a distinct content hash.
///
/// `hash_byte` seeds the 32-byte hash so each grain is content-addressed
/// distinctly (JSON output injects the hex hash) while staying deterministic.
fn grain(gt: GrainType, hash_byte: u8, fields: Vec<(&str, serde_json::Value)>) -> DeserializedGrain {
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
            created_at_sec: CREATED_AT,
        },
        grain_type: gt,
        fields: map,
        hash: Hash::from_bytes(&[hash_byte; 32]),
    }
}

/// Wrap a grain in a `SearchHit` with a fixed score and no recall-pipeline
/// annotations (so no census/expansion/supersession/timeline modes fire).
fn hit(grain: DeserializedGrain) -> SearchHit {
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

/// The FIXED recall result set shared by all format snapshots: one grain of
/// each of five distinct grain types, in a stable order.
fn fixture_hits() -> Vec<SearchHit> {
    vec![
        hit(grain(
            GrainType::State,
            0x11,
            // `context` is what the `ctx` wire key expands to (OMS §8.3). The
            // fixture previously used `context_data` — the Rust field name, which
            // never appears in a deserialized grain — so these goldens locked in
            // a State that rendered as the literal "state".
            vec![(
                "context",
                serde_json::json!({ "label": "planning_phase" }),
            )],
        )),
        hit(grain(
            GrainType::Goal,
            0x22,
            vec![
                ("description", serde_json::json!("Ship the OSS launch")),
                ("goal_state", serde_json::json!("active")),
                ("priority", serde_json::json!("high")),
                ("progress", serde_json::json!(0.4)),
            ],
        )),
        hit(grain(
            GrainType::Fact,
            0x33,
            vec![
                ("subject", serde_json::json!("john")),
                ("relation", serde_json::json!("likes")),
                ("object", serde_json::json!("coffee")),
                ("confidence", serde_json::json!(0.95)),
            ],
        )),
        hit(grain(
            GrainType::Tool,
            0x44,
            vec![
                ("tool_name", serde_json::json!("search_api")),
                ("is_error", serde_json::json!(false)),
                ("tool_content", serde_json::json!("12 results")),
                ("duration_ms", serde_json::json!(340)),
            ],
        )),
        hit(grain(
            GrainType::Event,
            0x55,
            vec![("content", serde_json::json!("User asked about pricing tiers."))],
        )),
    ]
}

/// Render a `FormattedContext` into a stable, snapshot-friendly string that
/// locks both the metadata counts and the exact rendered text.
fn annotate(ctx: &FormattedContext) -> String {
    format!(
        "included={} omitted={} truncated={} estimated_tokens={}\n---\n{}",
        ctx.included_count, ctx.omitted_count, ctx.truncated, ctx.estimated_tokens, ctx.text,
    )
}

// ---------------------------------------------------------------------------
// Full-fidelity snapshots — one per output format, Minimal metadata.
// ---------------------------------------------------------------------------

#[test]
fn snapshot_sml() {
    let assembler = ContextAssembler::new();
    let policy = FormatPolicy::new(OutputFormat::Sml).metadata(MetadataLevel::Minimal);
    let ctx = assembler.format(&fixture_hits(), &policy);
    insta::assert_snapshot!("sml", annotate(&ctx));
}

#[test]
fn snapshot_toon() {
    let assembler = ContextAssembler::new();
    let policy = FormatPolicy::new(OutputFormat::Toon).metadata(MetadataLevel::Minimal);
    let ctx = assembler.format(&fixture_hits(), &policy);
    insta::assert_snapshot!("toon", annotate(&ctx));
}

#[test]
fn snapshot_markdown() {
    let assembler = ContextAssembler::new();
    let policy = FormatPolicy::new(OutputFormat::Markdown).metadata(MetadataLevel::Minimal);
    let ctx = assembler.format(&fixture_hits(), &policy);
    insta::assert_snapshot!("markdown", annotate(&ctx));
}

#[test]
fn snapshot_json() {
    let assembler = ContextAssembler::new();
    let policy = FormatPolicy::new(OutputFormat::Json).metadata(MetadataLevel::Minimal);
    let ctx = assembler.format(&fixture_hits(), &policy);
    insta::assert_snapshot!("json", annotate(&ctx));
}

// ---------------------------------------------------------------------------
// Budget-constrained snapshots — a tight token budget forces omission.
//
// Diversity reservation is disabled so allocation is pure priority-based and
// fully deterministic: the 70%-of-budget `full_threshold` admits only the
// highest-priority grains until it is exhausted, and the rest are omitted.
// The snapshot locks exactly which grains survive and the omitted count.
// ---------------------------------------------------------------------------

#[test]
fn snapshot_budget_truncation_sml() {
    let assembler = ContextAssembler::new();
    let policy = FormatPolicy::new(OutputFormat::Sml)
        .metadata(MetadataLevel::Minimal)
        .no_grain_type_diversity()
        .token_budget(40);
    let ctx = assembler.format(&fixture_hits(), &policy);
    assert!(ctx.truncated, "tight budget must force truncation");
    assert!(ctx.omitted_count > 0, "at least one grain must be omitted");
    assert!(ctx.included_count > 0, "at least one grain must survive");
    insta::assert_snapshot!("budget_truncation_sml", annotate(&ctx));
}

#[test]
fn snapshot_budget_truncation_json() {
    let assembler = ContextAssembler::new();
    let policy = FormatPolicy::new(OutputFormat::Json)
        .metadata(MetadataLevel::Minimal)
        .no_grain_type_diversity()
        .token_budget(40);
    let ctx = assembler.format(&fixture_hits(), &policy);
    assert!(ctx.truncated, "tight budget must force truncation");
    assert!(ctx.omitted_count > 0, "at least one grain must be omitted");
    insta::assert_snapshot!("budget_truncation_json", annotate(&ctx));
}
