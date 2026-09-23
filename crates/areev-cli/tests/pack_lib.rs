//! `areev::pack` as a LIBRARY (#315): a Rust host installs an agent without
//! shipping and spawning the binary.
//!
//! The cases mirror `pack_smoke.rs`, run through the API with no subprocess —
//! which is the point: a service that provisions a memory per tenant had to
//! carry the `areev` binary in its image, spawn `pack install --format json`,
//! parse the stdout, and map string errors back to causes.

use areev::pack::{install_pack, validate_pack, InstallOptions, PackError};
use areev_cal::AreevFacade;
use areev_core::authz::{AUTHZ_NS, REL_PERMITS};
use areev_core::types::{Fact, Grain};
use areev_store::Areev;
use std::path::{Path, PathBuf};
use tempfile::TempDir;

fn write(dir: &Path, rel: &str, body: &str) {
    let p = dir.join(rel);
    if let Some(parent) = p.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(p, body).unwrap();
}

/// A minimal pack: one Definition, one manifest.
fn simple_pack(dir: &TempDir) -> PathBuf {
    let root = dir.path().join("pack");
    std::fs::create_dir_all(&root).unwrap();
    write(
        &root,
        "grains/010-tool.json",
        r#"{"type": "tool", "kind": "definition", "tool_name": "lookup",
            "tool_description": "look something up", "created_at": 500}"#,
    );
    write(
        &root,
        "pack.json",
        r#"{"pack": "demo", "version": "1.0.0", "namespace": "ap",
            "grains": ["grains/010-tool.json"]}"#,
    );
    root
}

#[test]
fn validate_opens_no_memory_at_all() {
    // Content addressing needs no store, so CI can check a pack without
    // provisioning one.
    let dir = TempDir::new().unwrap();
    let root = simple_pack(&dir);
    let r = validate_pack(&root).expect("valid");
    assert_eq!(r.pack, "demo");
    assert_eq!(r.version, "1.0.0");
    assert_eq!(r.namespace.as_deref(), Some("ap"));
    assert_eq!(r.grains.len(), 1);
    assert_eq!(r.grains[0].grain_type, "tool");
    assert_eq!(r.grains[0].hash.len(), 64);
    assert!(r.warnings.is_empty(), "{:?}", r.warnings);
}

#[test]
fn a_mismatched_expected_hash_returns_pck_e002_with_nothing_written() {
    let dir = TempDir::new().unwrap();
    let root = simple_pack(&dir);
    write(
        &root,
        "pack.json",
        r#"{"pack": "demo", "version": "1.0.0", "namespace": "ap",
            "grains": [{"file": "grains/010-tool.json",
                        "expected_hash": "00000000000000000000000000000000000000000000000000000000deadbeef"}]}"#,
    );
    let err = validate_pack(&root).unwrap_err();
    assert_eq!(err.code(), "PCK-E002", "{err}");
    match err {
        PackError::ExpectationMismatch { expected, built, .. } => {
            assert!(expected.starts_with("0000"), "{expected}");
            assert_eq!(built.len(), 64, "{built}");
        }
        other => panic!("wrong cause: {other}"),
    }

    // And an install of the same pack writes nothing.
    let m = Areev::open(dir.path().join("m.db").to_str().unwrap()).unwrap();
    let facade = AreevFacade::with_session(m, Some("ap".into()), None);
    let before = facade.with_store(|s| s.head_op_seq()).unwrap();
    let err = install_pack(&facade, &root, &InstallOptions::default()).unwrap_err();
    assert_eq!(err.code(), "PCK-E002", "{err}");
    let after = facade.with_store(|s| s.head_op_seq()).unwrap();
    assert_eq!(before, after, "a refused pack leaves the memory untouched");
}

#[test]
fn a_forward_grain_reference_returns_pck_e003() {
    let dir = TempDir::new().unwrap();
    let root = dir.path().join("pack");
    std::fs::create_dir_all(&root).unwrap();
    // The workflow names a tool that is listed AFTER it.
    write(
        &root,
        "grains/010-workflow.json",
        r#"{"type": "workflow", "nodes": ["step"], "bindings": {"step": "grain:tool"},
            "created_at": 600}"#,
    );
    write(
        &root,
        "grains/020-tool.json",
        r#"{"type": "tool", "id": "tool", "kind": "definition", "tool_name": "lookup",
            "tool_description": "x", "created_at": 500}"#,
    );
    write(
        &root,
        "pack.json",
        r#"{"pack": "demo", "version": "1.0.0", "namespace": "ap",
            "grains": ["grains/010-workflow.json", "grains/020-tool.json"]}"#,
    );
    let err = validate_pack(&root).unwrap_err();
    assert_eq!(err.code(), "PCK-E003", "{err}");
}

#[test]
fn install_runs_under_the_callers_bound_principal() {
    // The facade, not an owner store: a service installing on a tenant's
    // behalf is authorized like every other write.
    let dir = TempDir::new().unwrap();
    let root = simple_pack(&dir);
    let mut m = Areev::open(dir.path().join("m.db").to_str().unwrap()).unwrap();
    m.add(
        &Fact::new("user:installer", REL_PERMITS, "read ON ap")
            .namespace(AUTHZ_NS)
            .created_at(1_000),
    )
    .unwrap();
    let facade = AreevFacade::with_session(m, Some("ap".into()), None)
        .with_principal("user:installer")
        .unwrap();
    let before = facade.with_store(|s| s.head_op_seq()).unwrap();
    let err = install_pack(&facade, &root, &InstallOptions::default()).unwrap_err();
    assert!(err.to_string().contains("AUT-E001"), "{err}");
    assert_eq!(
        facade.with_store(|s| s.head_op_seq()).unwrap(),
        before,
        "a refused install leaves the op-log unchanged"
    );
}

#[test]
fn a_dry_run_reports_and_writes_nothing() {
    let dir = TempDir::new().unwrap();
    let root = simple_pack(&dir);
    let m = Areev::open(dir.path().join("m.db").to_str().unwrap()).unwrap();
    let facade = AreevFacade::with_session(m, Some("ap".into()), None);
    let before = facade.with_store(|s| s.head_op_seq()).unwrap();
    let r = install_pack(
        &facade,
        &root,
        &InstallOptions { dry_run: true, ..Default::default() },
    )
    .expect("dry run");
    assert_eq!(r.grains.len(), 1);
    assert_eq!(facade.with_store(|s| s.head_op_seq()).unwrap(), before);
}

#[test]
fn an_owner_install_writes_the_grains_at_the_validated_addresses() {
    let dir = TempDir::new().unwrap();
    let root = simple_pack(&dir);
    let validated = validate_pack(&root).unwrap();
    let m = Areev::open(dir.path().join("m.db").to_str().unwrap()).unwrap();
    let facade = AreevFacade::with_session(m, Some("ap".into()), None);
    let installed = install_pack(&facade, &root, &InstallOptions::default()).expect("install");
    assert_eq!(
        validated.grains[0].hash, installed.grains[0].hash,
        "validate describes install"
    );
    let h = areev_core::error::Hash::from_hex(&installed.grains[0].hash).unwrap();
    assert!(facade.with_store(|s| s.has(&h)).unwrap());
}

// ---------------------------------------------------------------------------
// #316 — manifest surface
// ---------------------------------------------------------------------------

#[test]
fn an_unknown_top_level_key_is_warned_not_dropped_in_silence() {
    let dir = TempDir::new().unwrap();
    let root = simple_pack(&dir);
    write(
        &root,
        "pack.json",
        r#"{"pack": "demo", "version": "1.0.0", "namespace": "ap",
            "templats": {"brief": "..."},
            "grains": ["grains/010-tool.json"]}"#,
    );
    let r = validate_pack(&root).expect("a misspelling warns, it does not refuse");
    assert!(
        r.warnings.iter().any(|w| w.contains("templats")),
        "the warning must name the key: {:?}",
        r.warnings
    );
}

#[test]
fn the_reserved_host_object_comes_back_verbatim_and_warns_about_nothing() {
    let dir = TempDir::new().unwrap();
    let root = simple_pack(&dir);
    write(
        &root,
        "pack.json",
        r#"{"pack": "demo", "version": "1.0.0", "namespace": "ap",
            "host": {"config_schema": {"type": "object"}, "fixtures": ["a"]},
            "grains": ["grains/010-tool.json"]}"#,
    );
    let r = validate_pack(&root).expect("valid");
    assert!(r.warnings.is_empty(), "`host` is reserved: {:?}", r.warnings);
    let host = r.host.expect("returned verbatim");
    assert_eq!(host["config_schema"]["type"], "object");
    assert_eq!(host["fixtures"][0], "a");
}

#[test]
fn an_evalset_in_a_pack_is_checked_the_way_eval_create_checks_one() {
    let dir = TempDir::new().unwrap();
    let root = dir.path().join("pack");
    std::fs::create_dir_all(&root).unwrap();
    // A case with neither `input` nor `expect` — `areev eval create` refuses
    // it, and `pack validate` used to address it cleanly.
    write(
        &root,
        "grains/010-evalset.json",
        r#"{"type": "fact", "subject": "evalset:gate", "relation": "mg:evalset",
            "object": "{\"name\":\"gate\",\"cases\":[{\"name\":\"a\"}]}",
            "namespace": "agent:harness", "created_at": 500}"#,
    );
    write(
        &root,
        "pack.json",
        r#"{"pack": "demo", "version": "1.0.0", "grains": ["grains/010-evalset.json"]}"#,
    );
    let err = validate_pack(&root).unwrap_err();
    assert_eq!(err.code(), "PCK-E001", "{err}");
    assert!(err.to_string().contains("expect"), "{err}");

    // A valid one addresses.
    write(
        &root,
        "grains/010-evalset.json",
        r#"{"type": "fact", "subject": "evalset:gate", "relation": "mg:evalset",
            "object": "{\"name\":\"gate\",\"cases\":[{\"name\":\"a\",\"input\":{\"q\":1},\"expect\":{\"equals\":{\"q\":1}}}]}",
            "namespace": "agent:harness", "created_at": 500}"#,
    );
    let r = validate_pack(&root).expect("a well-formed evalset validates");
    assert_eq!(r.grains.len(), 1);
}

// ---------------------------------------------------------------------------
// #341 — executor pins, expected plan hash, authorization before any write
// ---------------------------------------------------------------------------

/// A pack with a code blob, a Definition naming it, a plan binding it, and a
/// saved query — every kind of write an install makes.
fn code_pack(dir: &TempDir) -> PathBuf {
    let root = dir.path().join("code-pack");
    std::fs::create_dir_all(root.join("blobs")).unwrap();
    std::fs::write(root.join("blobs/poll.wasm"), b"\0asm\x01\0\0\0not-a-real-module").unwrap();
    write(
        &root,
        "grains/010-tool.json",
        r#"{"type": "tool", "id": "poll", "kind": "definition", "tool_name": "poll",
            "tool_description": "read the queue", "created_at": 500,
            "executor_uri": "blob:poll", "runtime": "wasm32-areev-io",
            "capabilities": [ { "blob": { "read": true } } ]}"#,
    );
    write(
        &root,
        "grains/020-workflow.json",
        r#"{"type": "workflow", "id": "plan", "name": "queue", "nodes": ["poll"],
            "bindings": {"poll": "grain:poll"}, "created_at": 600}"#,
    );
    write(
        &root,
        "pack.json",
        r#"{"pack": "queue", "version": "1.0.0", "namespace": "ap",
            "blobs": {"poll": "blobs/poll.wasm"},
            "queries": {"pulse": {"body": "RECALL facts LIMIT 5"}},
            "grains": ["grains/010-tool.json", "grains/020-workflow.json"]}"#,
    );
    root
}

fn owner(dir: &TempDir) -> AreevFacade {
    let m = Areev::open(dir.path().join("m.db").to_str().unwrap()).unwrap();
    AreevFacade::with_session(m, Some("ap".into()), None)
}

fn plan_of(r: &areev::pack::PackReport) -> String {
    r.grains.iter().find(|g| g.grain_type == "workflow").unwrap().hash.clone()
}

/// Nothing at all landed: no op, no blob, no registry row.
fn assert_untouched(facade: &AreevFacade, before: i64, blob: &str) {
    assert_eq!(facade.with_store(|s| s.head_op_seq()).unwrap(), before, "op-log moved");
    assert!(
        facade.with_store(|s| s.get_blob(blob)).is_err(),
        "the blob landed ahead of a refusal"
    );
    assert!(
        facade.with_store(|s| s.meta_get("qry:pulse")).unwrap().is_none(),
        "the registry row landed ahead of a refusal"
    );
}

fn pins(key: &str, value: &str) -> std::collections::BTreeMap<String, String> {
    [(key.to_string(), value.to_string())].into_iter().collect()
}

#[test]
fn the_report_lists_every_code_carrying_tool_with_its_address() {
    let dir = TempDir::new().unwrap();
    let root = code_pack(&dir);
    let r = validate_pack(&root).unwrap();
    assert_eq!(r.executors.len(), 1, "{:?}", r.executors);
    let x = &r.executors[0];
    assert_eq!(x.tool, "poll");
    assert_eq!(x.file, "grains/010-tool.json");
    assert_eq!(x.executor_uri, r.blobs[0].address, "the executor IS the blob's address");
    assert!(!x.pinned, "validate takes no pins");
    assert_eq!(
        r.allow_executor,
        vec![x.executor_uri.trim_start_matches("cas://sha256:").to_string()]
    );
}

#[test]
fn a_matching_pin_installs_marks_the_tool_and_writes_nothing_extra() {
    let dir = TempDir::new().unwrap();
    let root = code_pack(&dir);
    let validated = validate_pack(&root).unwrap();
    let addr = validated.executors[0].executor_uri.clone();

    // Without pins, as the baseline op count.
    let base_dir = TempDir::new().unwrap();
    let base = owner(&base_dir);
    let b0 = base.with_store(|s| s.head_op_seq()).unwrap();
    install_pack(&base, &root, &InstallOptions::default()).unwrap();
    let unpinned_ops = base.with_store(|s| s.head_op_seq()).unwrap() - b0;

    let facade = owner(&dir);
    let before = facade.with_store(|s| s.head_op_seq()).unwrap();
    // Every name a host might hold resolves — tool_name, pack-local id,
    // symbolic blob name — and every address form parses.
    let bare = addr.trim_start_matches("cas://sha256:").to_string();
    for (key, value) in [
        ("poll", addr.clone()),
        ("poll", format!("sha256:{bare}")),
        ("poll", bare.to_uppercase()),
    ] {
        let opts = InstallOptions {
            executor_pins: pins(key, &value),
            dry_run: true,
            ..Default::default()
        };
        let r = install_pack(&facade, &root, &opts).expect("a matching pin installs");
        assert!(r.executors[0].pinned);
    }
    let opts = InstallOptions { executor_pins: pins("poll", &addr), ..Default::default() };
    let r = install_pack(&facade, &root, &opts).expect("a matching pin installs");
    assert!(r.executors[0].pinned, "{:?}", r.executors);
    // The pin is host-side: the plan hash is the one validate reports, and
    // the op-log grew exactly as much as an unpinned install's.
    assert_eq!(plan_of(&r), plan_of(&validated), "a pin must not change the plan hash");
    assert_eq!(
        facade.with_store(|s| s.head_op_seq()).unwrap() - before,
        unpinned_ops,
        "a pin must not add a write"
    );
}

#[test]
fn a_mismatched_pin_refuses_the_whole_install_with_pck_e005() {
    let dir = TempDir::new().unwrap();
    let root = code_pack(&dir);
    let addr = validate_pack(&root).unwrap().blobs[0].address.clone();
    let facade = owner(&dir);
    let before = facade.with_store(|s| s.head_op_seq()).unwrap();
    let opts = InstallOptions {
        executor_pins: pins("poll", &"ab".repeat(32)),
        ..Default::default()
    };
    let err = install_pack(&facade, &root, &opts).unwrap_err();
    assert_eq!(err.code(), "PCK-E005", "{err}");
    assert!(matches!(err, PackError::ExecutorPin(_)));
    assert!(err.to_string().contains("Nothing was written"), "{err}");
    assert_untouched(&facade, before, &addr);
}

#[test]
fn a_pin_naming_no_code_carrying_tool_is_refused_not_ignored() {
    let dir = TempDir::new().unwrap();
    let root = code_pack(&dir);
    let addr = validate_pack(&root).unwrap().blobs[0].address.clone();
    let facade = owner(&dir);
    let before = facade.with_store(|s| s.head_op_seq()).unwrap();
    for (key, value) in [
        ("pol", addr.as_str()),         // a typo pins nothing
        ("queue", addr.as_str()),       // the plan's name: not code-carrying
        ("poll", "not-an-address"),     // not a content address
    ] {
        let opts = InstallOptions { executor_pins: pins(key, value), ..Default::default() };
        let err = install_pack(&facade, &root, &opts).unwrap_err();
        assert_eq!(err.code(), "PCK-E005", "{key}: {err}");
    }
    assert_untouched(&facade, before, &addr);
}

#[test]
fn an_expected_plan_hash_is_checked_before_anything_is_written() {
    let dir = TempDir::new().unwrap();
    let root = code_pack(&dir);
    let validated = validate_pack(&root).unwrap();
    let addr = validated.blobs[0].address.clone();
    let facade = owner(&dir);
    let before = facade.with_store(|s| s.head_op_seq()).unwrap();
    let opts = InstallOptions { expected_hash: Some("0".repeat(64)), ..Default::default() };
    let err = install_pack(&facade, &root, &opts).unwrap_err();
    assert_eq!(err.code(), "PCK-E002", "{err}");
    assert_untouched(&facade, before, &addr);

    let opts = InstallOptions {
        expected_hash: Some(format!("sha256:{}", plan_of(&validated))),
        ..Default::default()
    };
    let r = install_pack(&facade, &root, &opts).expect("the expected plan installs");
    assert_eq!(plan_of(&r), plan_of(&validated));
}

#[test]
fn a_refused_principal_writes_no_blob_and_no_registry_row_either() {
    // The blobs and saved queries used to go in through the ungated
    // `with_store` ahead of the batch that then refused: the op-log stayed
    // put, but the memory did not.
    let dir = TempDir::new().unwrap();
    let root = code_pack(&dir);
    let addr = validate_pack(&root).unwrap().blobs[0].address.clone();
    let mut m = Areev::open(dir.path().join("m.db").to_str().unwrap()).unwrap();
    m.add(
        &Fact::new("user:reader", REL_PERMITS, "read ON ap")
            .namespace(AUTHZ_NS)
            .created_at(1_000),
    )
    .unwrap();
    let facade = AreevFacade::with_session(m, Some("ap".into()), None)
        .with_principal("user:reader")
        .unwrap();
    let before = facade.with_store(|s| s.head_op_seq()).unwrap();
    let err = install_pack(&facade, &root, &InstallOptions::default()).unwrap_err();
    assert_eq!(err.code(), "AUT-E001", "{err}");
    assert_untouched(&facade, before, &addr);
}

#[test]
fn a_writer_installs_new_registry_rows_but_cannot_replace_a_different_one() {
    let dir = TempDir::new().unwrap();
    let root = code_pack(&dir);
    let mut m = Areev::open(dir.path().join("m.db").to_str().unwrap()).unwrap();
    m.add(
        &Fact::new("user:writer", REL_PERMITS, "write ON ap")
            .namespace(AUTHZ_NS)
            .created_at(1_000),
    )
    .unwrap();
    let facade = AreevFacade::with_session(m, Some("ap".into()), None)
        .with_principal("user:writer")
        .unwrap();
    // A fresh memory: every row is new, so `write` on the namespace is enough.
    install_pack(&facade, &root, &InstallOptions::default()).expect("a writer installs");
    // Re-installing the same pack is a no-op for the registry.
    install_pack(&facade, &root, &InstallOptions::default()).expect("idempotent");

    // A DIFFERENT body for an existing query is `DEFINE QUERY`'s job: admin.
    write(
        &root,
        "pack.json",
        r#"{"pack": "queue", "version": "1.0.1", "namespace": "ap",
            "blobs": {"poll": "blobs/poll.wasm"},
            "queries": {"pulse": {"body": "RECALL facts LIMIT 50"}},
            "grains": ["grains/010-tool.json", "grains/020-workflow.json"]}"#,
    );
    let before = facade.with_store(|s| s.head_op_seq()).unwrap();
    let err = install_pack(&facade, &root, &InstallOptions::default()).unwrap_err();
    assert_eq!(err.code(), "AUT-E001", "{err}");
    assert!(err.to_string().contains("admin"), "{err}");
    assert_eq!(facade.with_store(|s| s.head_op_seq()).unwrap(), before);
}
