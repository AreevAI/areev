//! The cross-surface parity golden of the render unification.
//!
//! The CAL executor and the ContextAssembler render grains through the same
//! `areev_cal::render` functions, but each builds its `GrainView` differently:
//! CAL results carry `created_at` as epoch-milliseconds inside `fields`, while
//! deserialized grains carry epoch-seconds in the `.mg` header. This test
//! pins that both constructions of the same stored grain render to the same
//! bytes, per format, for every grain type. If a view construction drifts
//! (units, hash encoding, field routing), this fails before a user notices
//! two surfaces disagreeing.

use std::collections::HashMap;

use areev_cal::render as shared;
use areev_context::policy::{FormatPolicy, MetadataLevel, OutputFormat};
use areev_context::render::RendererRegistry;
use areev_core::format::deserialize::DeserializedGrain;
use areev_core::format::header::MgHeader;
use areev_core::types::GrainType;

const CREATED_AT_SEC: u32 = 1_740_000_000; // 2025-02-19

fn deserialized(gt: GrainType, fields: &[(&str, serde_json::Value)]) -> DeserializedGrain {
    let mut map = HashMap::new();
    for (k, v) in fields {
        map.insert(k.to_string(), v.clone());
    }
    DeserializedGrain {
        header: MgHeader {
            version: 1,
            flags: 0,
            grain_type: gt.type_byte(),
            ns_hash: 0,
            created_at_sec: CREATED_AT_SEC,
        },
        grain_type: gt,
        fields: map,
        hash: areev_core::error::Hash::from_bytes(&[0xAB; 32]),
    }
}

/// The CAL-result view of the same grain: fields as a JSON object carrying
/// `created_at` in epoch milliseconds, hash as the same hex string.
fn cal_render(
    gt: GrainType,
    fields: &[(&str, serde_json::Value)],
    hash_hex: &str,
    format: &OutputFormat,
    level: MetadataLevel,
) -> String {
    let mut map = serde_json::Map::new();
    for (k, v) in fields {
        map.insert(k.to_string(), v.clone());
    }
    map.insert(
        "created_at".into(),
        serde_json::json!((CREATED_AT_SEC as i64) * 1000),
    );
    let fields_value = serde_json::Value::Object(map);
    let view = shared::GrainView {
        grain_type: gt.as_str(),
        hash: hash_hex,
        fields: &fields_value,
        created_at_sec: shared::created_at_sec_from_fields(&fields_value),
    };
    let detail = match level {
        MetadataLevel::None => shared::MetadataDetail::None,
        MetadataLevel::Minimal => shared::MetadataDetail::Minimal,
        MetadataLevel::Full => shared::MetadataDetail::Full,
    };
    match format {
        OutputFormat::Sml => shared::render_grain_sml(&view, detail),
        OutputFormat::Markdown => shared::render_grain_markdown(&view),
        OutputFormat::PlainText => shared::render_grain_text_line(&view, None),
        OutputFormat::Json => shared::grain_json(&view).to_string(),
        OutputFormat::Toon => shared::toon_row(&view),
    }
}

fn representative_fields(gt: GrainType) -> Vec<(&'static str, serde_json::Value)> {
    match gt {
        GrainType::Fact => vec![
            ("subject", serde_json::json!("john")),
            ("relation", serde_json::json!("likes")),
            ("object", serde_json::json!("coffee")),
            ("confidence", serde_json::json!(0.95)),
        ],
        GrainType::Event => vec![
            ("content", serde_json::json!("what's the refund window?")),
            ("role", serde_json::json!("user")),
        ],
        GrainType::State => vec![(
            "context",
            serde_json::json!({"label": "checkout step 2", "cart_items": 3}),
        )],
        GrainType::Workflow => vec![
            ("name", serde_json::json!("deploy")),
            ("trigger", serde_json::json!("push")),
            ("nodes", serde_json::json!(["build", "test"])),
            (
                "edges",
                serde_json::json!([{"src": "build", "dst": "test", "cond": "green"}]),
            ),
        ],
        GrainType::Tool => vec![
            ("tool_name", serde_json::json!("search_api")),
            ("tool_content", serde_json::json!("12 results")),
            ("is_error", serde_json::json!(false)),
            ("duration_ms", serde_json::json!(340)),
        ],
        GrainType::Observation => vec![
            ("observer_id", serde_json::json!("agent-1")),
            ("subject", serde_json::json!("latency")),
            ("object", serde_json::json!("p95 rising")),
        ],
        GrainType::Goal => vec![
            ("description", serde_json::json!("Ship the launch")),
            ("goal_state", serde_json::json!("active")),
            ("priority", serde_json::json!("high")),
            ("progress", serde_json::json!(0.4)),
        ],
        GrainType::Reasoning => vec![
            ("conclusion", serde_json::json!("prefer smaller batches")),
            ("inference_method", serde_json::json!("induction")),
            ("premises", serde_json::json!(["a", "b"])),
        ],
        GrainType::Consensus => vec![
            ("agreed_content", serde_json::json!("release on Friday")),
            ("agreement_count", serde_json::json!(3)),
            ("dissent_count", serde_json::json!(1)),
        ],
        GrainType::Consent => vec![
            ("subject_did", serde_json::json!("did:ex:john")),
            ("scope", serde_json::json!("marketing")),
            ("is_withdrawal", serde_json::json!(false)),
        ],
        GrainType::Skill => vec![
            ("name", serde_json::json!("negotiation")),
            ("description", serde_json::json!("closing enterprise deals")),
            ("domain", serde_json::json!("sales")),
        ],
        GrainType::Recommendation => vec![
            ("target_ref", serde_json::json!("qry:agent_context")),
            ("severity", serde_json::json!("warn")),
            (
                "summary",
                serde_json::json!({"template": "t1", "args": {"metric": "hit_rate"}}),
            ),
        ],
    }
}

#[test]
fn both_surfaces_render_every_type_identically() {
    let registry = RendererRegistry::new();
    let all = [
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
    let formats = [
        OutputFormat::Sml,
        OutputFormat::Markdown,
        OutputFormat::PlainText,
        OutputFormat::Toon,
    ];
    let levels = [
        MetadataLevel::None,
        MetadataLevel::Minimal,
        MetadataLevel::Full,
    ];

    for gt in all {
        let fields = representative_fields(gt);
        let grain = deserialized(gt, &fields);
        let hash_hex = grain.hash.to_hex();
        for format in &formats {
            for level in levels {
                let via_context = registry
                    .get(gt)
                    .unwrap()
                    .render(&grain, &FormatPolicy::new(format.clone()).metadata(level));
                let via_cal = cal_render(gt, &fields, &hash_hex, format, level);
                assert_eq!(
                    via_context, via_cal,
                    "render diverged for {gt:?} in {format:?} at {level:?}"
                );
            }
        }
    }
}

#[test]
fn json_envelope_matches_modulo_surface_supplied_created_at() {
    // The JSON envelope serializes `fields` verbatim, and only the CAL
    // surface injects `created_at` into fields — so compare after removing
    // it. Everything else must match byte-for-byte.
    let registry = RendererRegistry::new();
    let fields = representative_fields(GrainType::Fact);
    let grain = deserialized(GrainType::Fact, &fields);
    let hash_hex = grain.hash.to_hex();

    let via_context: serde_json::Value = serde_json::from_str(
        &registry.get(GrainType::Fact).unwrap().render(
            &grain,
            &FormatPolicy::new(OutputFormat::Json).metadata(MetadataLevel::None),
        ),
    )
    .unwrap();
    let mut via_cal: serde_json::Value = serde_json::from_str(&cal_render(
        GrainType::Fact,
        &fields,
        &hash_hex,
        &OutputFormat::Json,
        MetadataLevel::None,
    ))
    .unwrap();
    via_cal["fields"].as_object_mut().unwrap().remove("created_at");
    assert_eq!(via_context, via_cal);
}

#[test]
fn summaries_agree_across_surfaces() {
    let registry = RendererRegistry::new();
    for gt in [GrainType::Fact, GrainType::Goal, GrainType::Tool] {
        let fields = representative_fields(gt);
        let grain = deserialized(gt, &fields);
        let hash_hex = grain.hash.to_hex();

        let mut map = serde_json::Map::new();
        for (k, v) in &fields {
            map.insert(k.to_string(), v.clone());
        }
        let fields_value = serde_json::Value::Object(map);
        let view = shared::GrainView {
            grain_type: gt.as_str(),
            hash: &hash_hex,
            fields: &fields_value,
            created_at_sec: CREATED_AT_SEC,
        };
        assert_eq!(
            registry
                .get(gt)
                .unwrap()
                .render_summary(&grain, &FormatPolicy::default()),
            shared::render_grain_summary(&view),
            "summary diverged for {gt:?}"
        );
    }
}
