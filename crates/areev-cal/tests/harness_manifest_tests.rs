use areev_cal::{CalExecutor, CalExecutorConfig, AreevFacade};
use areev_core::authz::HARNESS_NS;
use areev_core::types::{Fact, Grain, GrainType};
use areev_store::{Areev, AreevOptions, TelemetryMode};
use sha2::{Digest, Sha256};

#[test]
fn identical_run_configs_share_one_address_and_are_reverse_joinable() {
    let dir = tempfile::tempdir().unwrap();
    let store = Areev::open(dir.path().join("harness.db").to_str().unwrap()).unwrap();
    let facade = AreevFacade::with_session(store, Some("caller".into()), None);
    let config = serde_json::json!({
        "model": {
            "base": "model-x",
            "build": "2026-08-12",
            "adapter_hash": null,
            "quantization": "bf16",
            "runtime": "test/1"
        },
        "sampling": {
            "temperature": 0.2,
            "top_p": 0.95,
            "top_k": 40,
            "seed": 7,
            "max_tokens": 1024
        },
        "system_prompt": "You are a test agent.",
        "tools": [{"name": "search"}]
    })
    .to_string();

    let (config_a, link_a) = facade.record_run_manifest("run-a", &config).unwrap();
    let (config_b, link_b) = facade.record_run_manifest("run-b", &config).unwrap();
    assert_eq!(config_a, config_b, "the config hash is its stable id");
    assert_ne!(link_a, link_b, "each run has its own link fact");

    let state = facade.with_store(|m| m.get(&config_a)).unwrap();
    assert_eq!(state.get_str("namespace"), Some(HARNESS_NS));
    assert_eq!(state.get_i64("created_at"), Some(0));
    assert_eq!(state.fields["context"]["sampling"]["seed"], 7);

    let runs = facade
        .with_store(|m| m.grains_by_object(HARNESS_NS, &config_a.to_hex(), 10))
        .unwrap();
    assert_eq!(runs.len(), 2, "OSP answers config -> runs");
    let mut subjects: Vec<&str> = runs.iter().filter_map(|g| g.get_str("subject")).collect();
    subjects.sort_unstable();
    assert_eq!(subjects, ["run:run-a", "run:run-b"]);
}

#[test]
fn run_manifest_rejects_non_object_config() {
    let dir = tempfile::tempdir().unwrap();
    let store = Areev::open(dir.path().join("harness.db").to_str().unwrap()).unwrap();
    let facade = AreevFacade::with_session(store, Some("caller".into()), None);
    let err = facade.record_run_manifest("run-a", "[]").unwrap_err();
    assert!(err.to_string().contains("JSON object"), "{err}");
}

#[test]
fn single_source_assembly_records_budget_overflow() {
    let dir = tempfile::tempdir().unwrap();
    let store = Areev::open_with(
        dir.path().join("budget.db").to_str().unwrap(),
        AreevOptions { telemetry: TelemetryMode::Aggregate, ..Default::default() },
    )
    .unwrap();
    let facade = AreevFacade::with_session(store, Some("caller".into()), None);
    let ex = CalExecutor::new(CalExecutorConfig::default());
    for object in ["tea", "coffee"] {
        ex.execute(
            &format!(
                r#"ADD fact SET subject = "alice" SET relation = "prefers" SET object = "{object}" REASON "seed""#
            ),
            &facade,
        )
        .unwrap();
    }
    ex.execute(
        r#"ASSEMBLE "brief" FROM (RECALL facts WHERE subject = "alice") BUDGET 1 tokens"#,
        &facade,
    )
    .unwrap();
    let budget = facade.with_store(|m| m.telemetry_budget_stats()).unwrap();
    assert_eq!(budget.sample_count, 1);
    assert_eq!(budget.overflow_count, 1);
}

#[test]
fn sampled_assembly_persists_order_digest_budget_and_drops() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = Areev::open(dir.path().join("assembly.db").to_str().unwrap()).unwrap();
    for (relation, object, stamp) in [
        ("prefers", "tea", 1_000),
        ("avoids", "coffee", 2_000),
    ] {
        store
            .add(
                &Fact::new("alice", relation, object)
                    .namespace("caller")
                    .created_at(stamp),
            )
            .unwrap();
    }
    let facade = AreevFacade::with_session(store, Some("caller".into()), None);
    let ex = CalExecutor::new(CalExecutorConfig {
        assembly_manifest_sample_rate: 1.0,
        ..CalExecutorConfig::default()
    });
    let result = ex
        .execute(
            r#"ASSEMBLE "prompt" FROM p: (RECALL facts WHERE subject = "alice") BUDGET 1 tokens FORMAT sml"#,
            &facade,
        )
        .unwrap();
    let rendered = match &result.result {
        areev_cal::executor::CalResultPayload::Formatted { text, .. } => text.as_bytes(),
        other => panic!("expected formatted assembly, got {other:?}"),
    };
    let expected_digest = hex::encode(Sha256::digest(rendered));

    let observations = facade
        .with_store(|m| m.recent(HARNESS_NS, Some(GrainType::Observation), 10))
        .unwrap();
    assert_eq!(observations.len(), 1);
    let manifest = &observations[0].fields["manifest"];
    assert_eq!(manifest["rendered_sha256"], expected_digest);
    assert_eq!(manifest["budget"]["limit"], 1);
    assert_eq!(manifest["budget"]["unit"], "tokens");
    // ASSEMBLE preserves its one-grain-per-source minimum even when that one
    // grain exceeds the tiny budget; the manifest must still account for the
    // other candidate as dropped.
    assert_eq!(manifest["dropped_hashes"].as_array().map(Vec::len), Some(1));
    assert_eq!(manifest["included_hashes"].as_array().map(Vec::len), Some(1));

    // The statement is proven by digest, never retained. This manifest is an
    // immutable Observation that replicates and lives in `agent:harness`, which
    // the subject selector does not reach — so a literal identity here would
    // survive FORGET SUBJECT and stay invisible to REPORT SUBJECT.
    let query_sha256 = manifest["query_sha256"]
        .as_str()
        .expect("manifest records a query digest");
    assert_eq!(query_sha256.len(), 64, "expected a sha256 hex digest");
    assert!(manifest.get("query").is_none(), "the raw statement must not be stored");
    let serialized = serde_json::to_string(&observations[0].fields).unwrap();
    assert!(
        !serialized.contains("alice"),
        "the assembled subject leaked into a durable manifest: {serialized}"
    );
}

#[test]
fn assembly_manifest_sampling_is_off_by_default() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = Areev::open(dir.path().join("assembly-off.db").to_str().unwrap()).unwrap();
    store
        .add(&Fact::new("alice", "prefers", "tea").namespace("caller"))
        .unwrap();
    let facade = AreevFacade::with_session(store, Some("caller".into()), None);
    CalExecutor::with_defaults()
        .execute(
            r#"ASSEMBLE "prompt" FROM p: (RECALL facts WHERE subject = "alice")"#,
            &facade,
        )
        .unwrap();
    let observations = facade
        .with_store(|m| m.recent(HARNESS_NS, Some(GrainType::Observation), 10))
        .unwrap();
    assert!(observations.is_empty());
}
