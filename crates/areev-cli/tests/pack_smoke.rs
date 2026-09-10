//! `areev pack` through the real binary (#178).
//!
//! A pack is what a deployment installs, so the properties worth testing are
//! the ones a deployment depends on: that an address written by hand cannot
//! sneak in, that a mismatch is refused with **nothing written**, that a pack
//! exported from a memory reinstalls into a fresh one at identical addresses,
//! and that the pins an operator is told to grant are exactly the code.

use std::path::Path;
use std::process::Command;
use tempfile::TempDir;

fn areev(args: &[&str]) -> (bool, String, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_areev")).args(args).output().expect("spawn areev");
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).to_string(),
        String::from_utf8_lossy(&out.stderr).to_string(),
    )
}

/// A pack with a code blob, a Definition naming it symbolically, a plan
/// binding that Definition symbolically, and a trigger naming the plan.
///
/// Nothing here is a hash: every reference is resolved at install from bytes
/// that do not exist until install stores them.
fn write_pack(dir: &Path, expected_workflow: Option<&str>) {
    std::fs::create_dir_all(dir.join("grains")).unwrap();
    std::fs::create_dir_all(dir.join("blobs")).unwrap();
    // Not a real module — nothing executes in this test, and a pack's job is
    // to place bytes at their address, whatever they are.
    std::fs::write(dir.join("blobs/poll.wasm"), b"\0asm\x01\0\0\0not-a-real-module").unwrap();
    std::fs::write(
        dir.join("grains/010-tool-poll.json"),
        r#"{ "id": "poll", "type": "tool", "tool_name": "poll", "kind": "definition",
             "tool_description": "read the queue", "created_at": 1788134400000,
             "executor_uri": "blob:poll", "runtime": "wasm32-areev-io",
             "capabilities": [ { "blob": { "read": true } } ] }"#,
    )
    .unwrap();
    std::fs::write(
        dir.join("grains/020-workflow.json"),
        r#"{ "id": "plan", "type": "workflow", "name": "queue",
             "nodes": ["poll"], "bindings": { "poll": "grain:poll" },
             "created_at": 1788134400000 }"#,
    )
    .unwrap();
    std::fs::write(
        dir.join("grains/030-trigger.json"),
        r#"{ "id": "watch", "type": "trigger", "kind": "polling",
             "workflow": "grain:plan", "connector": "queue",
             "connector_tool": "grain:poll", "interval_secs": 60,
             "dedup_key": ["/id"], "created_at": 1788134400000,
             "because": "the desk watches this queue" }"#,
    )
    .unwrap();
    let expect = match expected_workflow {
        Some(h) => format!(
            r#"{{ "file": "grains/020-workflow.json", "expected_hash": "{h}" }}"#
        ),
        None => "\"grains/020-workflow.json\"".to_string(),
    };
    std::fs::write(
        dir.join("pack.json"),
        format!(
            r#"{{ "pack": "queue-watch", "version": "1.0.0", "namespace": "ops",
                  "blobs": {{ "poll": "blobs/poll.wasm" }},
                  "grains": [ "grains/010-tool-poll.json", {expect},
                              "grains/030-trigger.json" ],
                  "queries": {{ "pulse": {{ "body": "RECALL facts LIMIT 5" }} }} }}"#
        ),
    )
    .unwrap();
}

fn hash_of(json: &str, ty: &str) -> String {
    let v: serde_json::Value = serde_json::from_str(json).expect("json output");
    v["grains"]
        .as_array()
        .expect("grains")
        .iter()
        .find(|g| g["type"] == ty)
        .unwrap_or_else(|| panic!("no {ty} in {json}"))["hash"]
        .as_str()
        .unwrap()
        .to_string()
}

#[test]
fn validate_opens_no_memory_and_addresses_every_grain() {
    let dir = TempDir::new().unwrap();
    let pack = dir.path().join("pack");
    write_pack(&pack, None);

    let (ok, out, err) = areev(&["pack", "validate", pack.to_str().unwrap()]);
    assert!(ok, "validate failed: {err}");
    assert!(out.contains("valid"), "{out}");
    // The pin an operator is told to grant is the CODE, not the pack's blobs:
    // a data blob is not something to authorize as executable.
    assert!(out.contains("--allow-executor"), "{out}");
    assert_eq!(out.matches("--allow-executor").count(), 1, "{out}");
    // No --db was given and none was created: content addressing is a pure
    // function of the grain, so what a pack WILL install is knowable without a
    // memory to install it into.
    let created: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().to_string())
        .collect();
    assert_eq!(created, vec!["pack".to_string()], "validate created {created:?}");
}

#[test]
fn install_resolves_blob_and_grain_references_to_the_addresses_they_earn() {
    let dir = TempDir::new().unwrap();
    let pack = dir.path().join("pack");
    write_pack(&pack, None);
    let db = dir.path().join("m.db");

    let (ok, out, err) = areev(&[
        "pack", "install", pack.to_str().unwrap(), "--db", db.to_str().unwrap(), "--format", "json",
    ]);
    assert!(ok, "install failed: {err}");
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    let plan = hash_of(&out, "workflow");
    let tool = hash_of(&out, "tool");

    // The plan binds the tool by the address the tool actually got, and the
    // trigger names the plan by the address the plan actually got — none of
    // which the pack author could have written down.
    let (ok, shown, err) = areev(&[
        "cal", "RECALL workflows RECENT 5", "--db", db.to_str().unwrap(), "--ns", "ops",
        "--format", "json",
    ]);
    assert!(ok, "recall failed: {err}");
    assert!(shown.contains(&plan), "the plan is not in the memory: {shown}");
    assert!(shown.contains(&tool), "the plan does not bind {tool}: {shown}");

    let (ok, triggers, err) = areev(&[
        "trigger", "list", "--db", db.to_str().unwrap(), "--ns", "ops", "--format", "json",
    ]);
    assert!(ok, "{err}");
    assert!(triggers.contains(&plan), "the trigger does not name {plan}: {triggers}");

    // And the pin reported is the code blob's address.
    let pin = v["allow_executor"][0].as_str().unwrap();
    assert_eq!(pin.len(), 64, "{out}");
    assert_eq!(v["allow_executor"].as_array().unwrap().len(), 1, "only the code is pinned: {out}");
}

#[test]
fn a_mismatched_expected_hash_is_refused_with_nothing_written() {
    let dir = TempDir::new().unwrap();
    let pack = dir.path().join("pack");
    write_pack(&pack, Some(&"0".repeat(64)));
    let db = dir.path().join("m.db");

    let (ok, _out, err) = areev(&[
        "pack", "install", pack.to_str().unwrap(), "--db", db.to_str().unwrap(),
    ]);
    assert!(!ok, "a pack that does not build what it claims must be refused");
    assert!(err.contains("expected"), "{err}");
    assert!(err.contains("Nothing was written"), "{err}");

    // Nothing means nothing: a refused install must leave the memory exactly
    // as it found it, half an agent being worse than none.
    let (ok, out, _) = areev(&[
        "cal", "RECALL tools RECENT 5", "--db", db.to_str().unwrap(), "--ns", "ops",
        "--format", "json",
    ]);
    assert!(ok, "{out}");
    assert!(out.contains("\"total_available\": 0"), "a refused install left grains behind: {out}");
}

#[test]
fn a_forward_grain_reference_is_refused_rather_than_guessed_at() {
    let dir = TempDir::new().unwrap();
    let pack = dir.path().join("pack");
    write_pack(&pack, None);
    // Order the plan BEFORE the tool it binds: the address it needs does not
    // exist yet, and inventing one would install a plan bound to nothing.
    std::fs::write(
        pack.join("pack.json"),
        r#"{ "pack": "queue-watch", "version": "1.0.0", "namespace": "ops",
             "blobs": { "poll": "blobs/poll.wasm" },
             "grains": [ "grains/020-workflow.json", "grains/010-tool-poll.json" ] }"#,
    )
    .unwrap();
    let (ok, _out, err) = areev(&["pack", "validate", pack.to_str().unwrap()]);
    assert!(!ok, "a forward reference must be refused");
    assert!(err.contains("grain:poll"), "{err}");
    assert!(err.contains("built first"), "{err}");
}

#[test]
fn a_source_pack_round_trips_into_a_fresh_memory_at_the_same_addresses() {
    let dir = TempDir::new().unwrap();
    let pack = dir.path().join("pack");
    write_pack(&pack, None);
    let first = dir.path().join("first.db");
    let (ok, installed, err) = areev(&[
        "pack", "install", pack.to_str().unwrap(), "--db", first.to_str().unwrap(),
        "--format", "json",
    ]);
    assert!(ok, "{err}");
    let plan = hash_of(&installed, "workflow");

    // Export what was installed…
    let exported = dir.path().join("exported");
    let (ok, _out, err) = areev(&[
        "pack", "export", "--db", first.to_str().unwrap(), "--ns", "ops",
        "--out", exported.to_str().unwrap(), "--name", "queue-watch",
    ]);
    assert!(ok, "export failed: {err}");
    // …and the export carries the saved query too, which is not a grain and
    // would otherwise be lost — a trigger's context_query would name nothing.
    let manifest = std::fs::read_to_string(exported.join("pack.json")).unwrap();
    assert!(manifest.contains("pulse"), "the saved query did not travel: {manifest}");

    // …then install it somewhere else and get the same agent.
    let second = dir.path().join("second.db");
    let (ok, again, err) = areev(&[
        "pack", "install", exported.to_str().unwrap(), "--db", second.to_str().unwrap(),
        "--format", "json",
    ]);
    assert!(ok, "reinstall failed: {err}");
    assert_eq!(hash_of(&again, "workflow"), plan, "the round trip changed the plan");
}

#[test]
fn a_bundle_pack_round_trips_and_asserts_what_it_carries() {
    let dir = TempDir::new().unwrap();
    let pack = dir.path().join("pack");
    write_pack(&pack, None);
    let first = dir.path().join("first.db");
    let (ok, installed, err) = areev(&[
        "pack", "install", pack.to_str().unwrap(), "--db", first.to_str().unwrap(),
        "--format", "json",
    ]);
    assert!(ok, "{err}");
    let plan = hash_of(&installed, "workflow");

    let exported = dir.path().join("bundle-pack");
    let (ok, _out, err) = areev(&[
        "pack", "export", "--db", first.to_str().unwrap(), "--ns", "ops",
        "--out", exported.to_str().unwrap(), "--pack-format", "bundle",
    ]);
    assert!(ok, "bundle export failed: {err}");
    let manifest = std::fs::read_to_string(exported.join("pack.json")).unwrap();
    assert!(manifest.contains(&plan), "the manifest does not expect its own plan: {manifest}");
    assert!(exported.join("pack.mgb").is_file());

    let second = dir.path().join("second.db");
    let (ok, out, err) = areev(&[
        "pack", "install", exported.to_str().unwrap(), "--db", second.to_str().unwrap(),
    ]);
    assert!(ok, "bundle install failed: {err}");
    assert!(out.contains("expectations met"), "{out}");
    let (ok, shown, _) = areev(&[
        "cal", "RECALL workflows RECENT 5", "--db", second.to_str().unwrap(), "--ns", "ops",
        "--format", "json",
    ]);
    assert!(ok && shown.contains(&plan), "the bundle did not carry the plan: {shown}");
}

#[test]
fn a_capability_declaration_without_the_runtime_that_honours_it_is_refused() {
    let dir = TempDir::new().unwrap();
    let pack = dir.path().join("pack");
    write_pack(&pack, None);
    std::fs::write(
        pack.join("grains/010-tool-poll.json"),
        r#"{ "id": "poll", "type": "tool", "tool_name": "poll", "kind": "definition",
             "tool_description": "read the queue", "created_at": 1788134400000,
             "executor_uri": "blob:poll",
             "capabilities": [ { "blob": { "read": true } } ] }"#,
    )
    .unwrap();
    let (ok, _out, err) = areev(&["pack", "validate", pack.to_str().unwrap()]);
    assert!(!ok, "a declaration no runtime can honour describes nothing");
    assert!(err.contains("wasm32-areev-io"), "{err}");
}
