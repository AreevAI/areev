//! `areev pack` — validate, install, export a pack (#178).
//!
//! A **pack** is a directory: a manifest, the grains it seeds, and the code
//! blobs those grains name. It is the installable unit an agent ships as —
//! tool definitions, a plan, the triggers that start it — and it exists
//! because everyone who deploys an agent was otherwise writing the same
//! installer: put the blobs in the CAS, rewrite each `executor_uri` to the
//! address the bytes turned out to have, seed the grains in dependency order,
//! and check that the plan came out at the hash the deployment expected.
//!
//! ## The manifest
//!
//! ```json
//! {
//!   "pack": "invoice-to-accounting",
//!   "version": "1.0.0",
//!   "description": "Reads an AP mailbox, extracts, posts, asks a human.",
//!   "namespace": "ap",
//!   "blobs": { "poll": "blobs/mailbox.poll.wasm" },
//!   "grains": [
//!     "grains/010-tool-poll.json",
//!     { "file": "grains/020-workflow.json",
//!       "expected_hash": "8f2c…" }
//!   ]
//! }
//! ```
//!
//! Or, for an exported pack, `"bundle": "pack.mgb"` in place of `grains`.
//!
//! A grain file is one JSON object in the same shape `ADD … WITH JSON` and the
//! bindings' `add` take — the SAME builder
//! (`areev_cal::json_build::build_grain_from_json`), deliberately: a pack that
//! authored grains its own way would be a fourth set of semantics for what a
//! Workflow is, and the one that drifts is always the one only a deployment
//! path exercises.
//!
//! ## Three properties, and why each is load-bearing
//!
//! **Blob references are symbolic.** A grain names its code as
//! `"blob:poll"`, and install rewrites that to `cas://sha256:<hex>` after
//! storing the bytes. A pack author cannot write the address by hand — the
//! address IS the bytes, so anything written by hand is a claim that can be
//! wrong. This also means editing the connector's code changes the plan's
//! hash, which is exactly the property that makes `expected_hash` worth
//! checking.
//!
//! **`expected_hash` is refused, not warned.** A pack that installs a plan at
//! a different address than the deployment expects has changed what runs, and
//! everything that points at the old hash — every trigger above all — is now
//! pointing somewhere else. Install stops before writing anything.
//!
//! **Install is validate plus a destination.** Every grain is built and
//! addressed with no store at all (`serialize_grain` is the whole content
//! addressing story), the expectations are checked, and only then is anything
//! written. `pack validate` is the same pass with the writing left out, so a
//! CI check and a deployment cannot disagree about what a pack contains.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use areev_cal::{AreevFacade, CalStoreFacade};
use areev_core::error::Hash;
use areev_core::types::{Grain, GrainType};
use areev_store::Areev;
use serde_json::{json, Map, Value};

use crate::{flag, need};
use std::collections::HashMap;

/// One grain the pack seeds.
struct Entry {
    /// Relative path, for messages.
    file: String,
    /// Pack-local name other grains reference as `grain:<id>`. Defaults to the
    /// file stem, so a hand-written pack usually needs no `id` at all.
    id: String,
    grain_type: String,
    fields: Map<String, Value>,
    expected: Option<String>,
}

/// A parsed manifest plus everything it names, read but not yet written.
struct Pack {
    dir: PathBuf,
    name: String,
    version: String,
    namespace: Option<String>,
    /// name → (relative path, bytes, content address)
    blobs: BTreeMap<String, (String, Vec<u8>, String)>,
    entries: Vec<Entry>,
    /// An exported pack carries a bundle instead of grain files.
    bundle: Option<String>,
    /// `expect` entries an exported (bundle) pack asserts after import.
    expect: Vec<(String, String)>,
    /// File-truths that are not grains: saved queries (`qry:<name>`) and
    /// custom render templates (`tpl:<name>`).
    ///
    /// They live in the `meta` table rather than the grain log, so nothing
    /// above would carry them — and a pack without them installs a trigger
    /// whose `context_query` names a query that is not there. They replicate
    /// in a bundle (the v2 `MGB2` meta segment), so a bundle pack needs no
    /// equivalent.
    registry: BTreeMap<String, String>,
}

pub fn run_pack(
    m: Option<Areev>,
    ns: &str,
    flags: &HashMap<String, String>,
    positional: &[String],
) -> Result<(), String> {
    let sub = positional.first().map(|s| s.as_str()).unwrap_or("");
    let json_out = flag(flags, "format").as_deref() == Some("json");
    match sub {
        "validate" => {
            let dir = pack_dir(flags, positional)?;
            validate(&dir, json_out)
        }
        "install" => {
            let dir = pack_dir(flags, positional)?;
            let m = m.ok_or("pack install needs a memory — pass --db <file|dsn>")?;
            install(m, ns, &dir, flags, json_out)
        }
        "export" => {
            let m = m.ok_or("pack export needs a memory — pass --db <file|dsn>")?;
            export(m, ns, flags, json_out)
        }
        other => Err(format!(
            "unknown pack subcommand '{other}' (validate|install|export)"
        )),
    }
}

fn pack_dir(flags: &HashMap<String, String>, positional: &[String]) -> Result<PathBuf, String> {
    let raw = positional
        .get(1)
        .cloned()
        .or_else(|| flag(flags, "pack"))
        .ok_or("name the pack directory: areev pack <verb> <dir>")?;
    let dir = PathBuf::from(&raw);
    if !dir.join("pack.json").is_file() {
        return Err(format!("{raw}: no pack.json here — a pack is a directory with a manifest"));
    }
    Ok(dir)
}

// ---------------------------------------------------------------- reading

fn read_pack(dir: &Path) -> Result<Pack, String> {
    let manifest_path = dir.join("pack.json");
    let raw = std::fs::read_to_string(&manifest_path)
        .map_err(|e| format!("{}: {e}", manifest_path.display()))?;
    let manifest: Value = serde_json::from_str(&raw)
        .map_err(|e| format!("{}: not JSON: {e}", manifest_path.display()))?;
    let name = manifest
        .get("pack")
        .and_then(Value::as_str)
        .ok_or("pack.json names no \"pack\"")?
        .to_string();
    let version = manifest
        .get("version")
        .and_then(Value::as_str)
        .unwrap_or("0.0.0")
        .to_string();
    let namespace = manifest
        .get("namespace")
        .and_then(Value::as_str)
        .map(str::to_string);

    // Blobs first: a grain's `blob:<name>` reference resolves to an address
    // that only exists once the bytes are read, which is the point — an
    // address written by hand is a claim, and this one is a measurement.
    let mut blobs = BTreeMap::new();
    if let Some(map) = manifest.get("blobs").and_then(Value::as_object) {
        for (key, path) in map {
            let rel = path
                .as_str()
                .ok_or_else(|| format!("blobs.{key}: expected a path string"))?;
            let full = dir.join(rel);
            let bytes = std::fs::read(&full)
                .map_err(|e| format!("blobs.{key} ({}): {e}", full.display()))?;
            let addr = areev_core::format::header::content_address(&bytes);
            blobs.insert(key.clone(), (rel.to_string(), bytes, format!("cas://sha256:{}", addr.to_hex())));
        }
    }

    let bundle = manifest
        .get("bundle")
        .and_then(Value::as_str)
        .map(str::to_string);
    let mut expect = Vec::new();
    if let Some(list) = manifest.get("expect").and_then(Value::as_array) {
        for e in list {
            let hash = e
                .get("hash")
                .or_else(|| e.get("expected_hash"))
                .and_then(Value::as_str)
                .ok_or("expect entries need a \"hash\"")?;
            let label = e
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("grain")
                .to_string();
            expect.push((label, hash.to_ascii_lowercase()));
        }
    }

    let mut registry = BTreeMap::new();
    for (section, prefix) in [("queries", "qry:"), ("templates", "tpl:")] {
        if let Some(map) = manifest.get(section).and_then(Value::as_object) {
            for (name, body) in map {
                let text = match body {
                    Value::String(s) => s.clone(),
                    other => serde_json::to_string(other).unwrap_or_default(),
                };
                registry.insert(format!("{prefix}{name}"), text);
            }
        }
    }

    let mut entries = Vec::new();
    for item in manifest.get("grains").and_then(Value::as_array).unwrap_or(&Vec::new()) {
        let (rel, expected) = match item {
            Value::String(s) => (s.clone(), None),
            Value::Object(o) => (
                o.get("file")
                    .and_then(Value::as_str)
                    .ok_or("a grain entry needs a \"file\"")?
                    .to_string(),
                o.get("expected_hash")
                    .and_then(Value::as_str)
                    .map(|h| h.trim().to_ascii_lowercase()),
            ),
            other => return Err(format!("grains: expected a path or an object, got {other}")),
        };
        let full = dir.join(&rel);
        let text = std::fs::read_to_string(&full)
            .map_err(|e| format!("{}: {e}", full.display()))?;
        let doc: Value = serde_json::from_str(&text)
            .map_err(|e| format!("{}: not JSON: {e}", full.display()))?;
        let mut fields = doc
            .as_object()
            .cloned()
            .ok_or_else(|| format!("{}: expected one JSON object", full.display()))?;
        let grain_type = fields
            .remove("type")
            .or_else(|| fields.remove("grain_type"))
            .and_then(|v| v.as_str().map(str::to_string))
            .ok_or_else(|| format!("{}: names no \"type\"", full.display()))?;
        if namespace.is_some() && !fields.contains_key("namespace") {
            fields.insert("namespace".into(), json!(namespace.clone().unwrap()));
        }
        resolve_blob_refs(&mut fields, &blobs, &rel)?;
        let id = fields
            .remove("id")
            .and_then(|v| v.as_str().map(str::to_string))
            .unwrap_or_else(|| {
                Path::new(&rel)
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or(&rel)
                    .to_string()
            });
        entries.push(Entry { file: rel, id, grain_type, fields, expected });
    }

    if entries.is_empty() && bundle.is_none() && registry.is_empty() {
        return Err("pack.json names neither \"grains\" nor \"bundle\" — a pack that seeds \
                    nothing installs nothing"
            .into());
    }
    Ok(Pack {
        dir: dir.to_path_buf(),
        name,
        version,
        namespace,
        blobs,
        entries,
        bundle,
        expect,
        registry,
    })
}

/// Rewrite every `"blob:<name>"` string to the address those bytes have.
///
/// Recursive, so it reaches an `executor_uri` at the top level and a
/// `content_refs[].uri` nested in a list alike — one rule, no per-field table
/// to fall behind the grain types.
fn resolve_blob_refs(
    value: &mut Map<String, Value>,
    blobs: &BTreeMap<String, (String, Vec<u8>, String)>,
    file: &str,
) -> Result<(), String> {
    fn walk(
        v: &mut Value,
        blobs: &BTreeMap<String, (String, Vec<u8>, String)>,
        file: &str,
    ) -> Result<(), String> {
        match v {
            Value::String(s) => {
                if let Some(name) = s.strip_prefix("blob:") {
                    let (_, _, addr) = blobs.get(name).ok_or_else(|| {
                        format!("{file}: names blob:{name}, which pack.json does not declare")
                    })?;
                    *s = addr.clone();
                }
            }
            Value::Array(items) => {
                for item in items {
                    walk(item, blobs, file)?;
                }
            }
            Value::Object(map) => {
                for (_, item) in map.iter_mut() {
                    walk(item, blobs, file)?;
                }
            }
            _ => {}
        }
        Ok(())
    }
    for (_, v) in value.iter_mut() {
        walk(v, blobs, file)?;
    }
    Ok(())
}

/// Rewrite every `"grain:<id>"` string to the address that grain built to.
///
/// The same walk `resolve_blob_refs` does, and for the same reason: one rule
/// that reaches a `bindings` map, a `workflow` field and a `connector_tool`
/// alike, rather than a table of field names to keep in step with the grain
/// types.
fn resolve_grain_refs(
    value: &mut Map<String, Value>,
    minted: &BTreeMap<String, String>,
    file: &str,
) -> Result<(), String> {
    fn walk(v: &mut Value, minted: &BTreeMap<String, String>, file: &str) -> Result<(), String> {
        match v {
            Value::String(s) => {
                if let Some(id) = s.strip_prefix("grain:").filter(|i| !i.contains(':')) {
                    let hash = minted.get(id).ok_or_else(|| {
                        format!(
                            "{file}: names grain:{id}, which no earlier grain in this pack \
                             minted — a reference is a content address, so the grain it names \
                             must be built first; list it earlier in \"grains\""
                        )
                    })?;
                    *s = hash.clone();
                }
            }
            Value::Array(items) => {
                for item in items {
                    walk(item, minted, file)?;
                }
            }
            Value::Object(map) => {
                for (_, item) in map.iter_mut() {
                    walk(item, minted, file)?;
                }
            }
            _ => {}
        }
        Ok(())
    }
    for (_, v) in value.iter_mut() {
        walk(v, minted, file)?;
    }
    Ok(())
}

// ------------------------------------------------------------- addressing

/// Build a grain and return its content address, touching no store.
///
/// This is the whole of `validate`, and the first half of `install`: content
/// addressing is a pure function of the serialized grain, so what a pack will
/// install can be known exactly without opening — or creating — a memory.
struct AddressSink;

impl areev_cal::json_build::GrainSink for AddressSink {
    type Out = Hash;
    fn consume<G: Grain + Clone + 'static>(self, grain: &G) -> areev_core::error::Result<Hash> {
        areev_core::format::serialize::serialize_grain(grain).map(|(_, h)| h)
    }
}

/// Build every grain, address it, and check it against what the manifest
/// expects. Nothing is written.
///
/// Grains are addressed **in manifest order**, and each one's address becomes
/// available to the ones after it as `grain:<id>`. That ordering is not a
/// convenience: a plan binds its tools by hash and a trigger names its plan by
/// hash, so a pack author cannot write those references down — the address is
/// the bytes, and the bytes are not final until the grain is built. A forward
/// reference is refused rather than guessed at, which is also why the manifest
/// is an ordered list and not a set.
fn resolve_addresses(pack: &mut Pack) -> Result<Vec<(String, String, String)>, String> {
    let mut out = Vec::with_capacity(pack.entries.len());
    let mut mismatches = Vec::new();
    let mut minted: BTreeMap<String, String> = BTreeMap::new();
    for e in pack.entries.iter_mut() {
        resolve_grain_refs(&mut e.fields, &minted, &e.file)?;
        let hash =
            areev_cal::json_build::build_grain_from_json(&e.grain_type, &e.fields, AddressSink)
                .map_err(|err| format!("{}: {err}", e.file))?
                .to_hex();
        minted.insert(e.id.clone(), hash.clone());
        if let Some(want) = &e.expected {
            if want != &hash {
                mismatches.push(format!(
                    "  {} ({}): expected {want}, builds to {hash}",
                    e.file, e.grain_type
                ));
            }
        }
        out.push((e.file.clone(), e.grain_type.clone(), hash));
    }
    if !mismatches.is_empty() {
        return Err(format!(
            "this pack does not build what its manifest says it builds:\n{}\n\
             A content address covers the whole grain, so a mismatch means the \
             declaration changed — including any code blob it names, since the \
             address of the bytes is part of the plan. Nothing was written. Fix the \
             pack, or update expected_hash deliberately and re-point whatever named \
             the old hash (triggers do NOT follow heads).",
            mismatches.join("\n")
        ));
    }
    Ok(out)
}

// ------------------------------------------------------------ the verbs

fn validate(dir: &Path, json_out: bool) -> Result<(), String> {
    let mut pack = read_pack(dir)?;
    let addressed =
        if pack.bundle.is_some() { Vec::new() } else { resolve_addresses(&mut pack)? };
    // Tool schemas are checked the way the runtime checks them, by building
    // the Definition — `build_grain_from_json` refuses a malformed
    // `input_schema` and an invalid `tool_name` — plus the capability
    // declaration, which is what decides where a blob may reach.
    let mut warnings = Vec::new();
    for e in &pack.entries {
        if e.grain_type == "tool" {
            check_tool(e, &mut warnings)?;
        }
    }
    if json_out {
        println!(
            "{}",
            json!({
                "pack": pack.name,
                "version": pack.version,
                "namespace": pack.namespace,
                "grains": addressed.iter().map(|(f, t, h)| json!({"file": f, "type": t, "hash": h})).collect::<Vec<_>>(),
                "blobs": pack.blobs.iter().map(|(k, (p, b, a))| json!({"name": k, "file": p, "bytes": b.len(), "address": a})).collect::<Vec<_>>(),
                "bundle": pack.bundle,
                "registry": pack.registry.keys().collect::<Vec<_>>(),
                "warnings": warnings,
                "ok": true,
            })
        );
        return Ok(());
    }
    println!("pack {} {} — valid", pack.name, pack.version);
    for (name, (path, bytes, addr)) in &pack.blobs {
        println!("  blob  {name:16} {addr}  ({} bytes, {path})", bytes.len());
    }
    for (file, ty, hash) in &addressed {
        println!("  grain {ty:16} {hash}  ({file})");
    }
    for key in pack.registry.keys() {
        println!("  meta  {key}");
    }
    if let Some(b) = &pack.bundle {
        println!("  bundle {b}");
    }
    for w in &warnings {
        eprintln!("areev: pack: {w}");
    }
    let pins = pins(&pack);
    if !pins.is_empty() {
        println!(
            "\nPin the code before running anything from this pack:\n  --allow-executor {}",
            pins.join(",")
        );
    }
    Ok(())
}

/// The checks a Definition needs beyond building: a capability declaration
/// must parse, and it must have a runtime that can honour it.
fn check_tool(e: &Entry, warnings: &mut Vec<String>) -> Result<(), String> {
    let runtime = e.fields.get("runtime").and_then(Value::as_str);
    let uri = e.fields.get("executor_uri").and_then(Value::as_str);
    if let Some(caps) = e.fields.get("capabilities") {
        if runtime != Some("wasm32-areev-io") {
            return Err(format!(
                "{}: declares capabilities but runtime is {:?} — capabilities require \
                 runtime \"wasm32-areev-io\"",
                e.file,
                runtime.unwrap_or("native")
            ));
        }
        areev_core::types::capability::Declaration::parse(caps)
            .map_err(|err| format!("{}: malformed capabilities: {err}", e.file))?;
    } else if runtime == Some("wasm32-areev-io") {
        return Err(format!(
            "{}: declares runtime \"wasm32-areev-io\" but no capabilities — a capability \
             runtime with an empty declaration can reach nothing",
            e.file
        ));
    }
    if runtime.is_some() && uri.is_none() {
        return Err(format!(
            "{}: declares a runtime but names no executor_uri — a runtime routes a code \
             blob, and there is none",
            e.file
        ));
    }
    if uri.is_some_and(|u| !u.starts_with("cas://sha256:")) {
        warnings.push(format!(
            "{}: executor_uri is not a content address — this host can only dispatch \
             cas://sha256:<64 hex>",
            e.file
        ));
    }
    Ok(())
}

fn install(
    m: Areev,
    ns: &str,
    dir: &Path,
    flags: &HashMap<String, String>,
    json_out: bool,
) -> Result<(), String> {
    let mut pack = read_pack(dir)?;
    let ns = pack.namespace.clone().unwrap_or_else(|| ns.to_string());
    let facade = AreevFacade::with_session(m, Some(ns.clone()), None);

    if let Some(bundle) = &pack.bundle {
        return install_bundle(&facade, &pack, bundle, json_out);
    }

    // Everything is built and checked BEFORE anything is written: a refused
    // pack must leave the memory exactly as it found it, and a half-installed
    // agent is worse than an uninstalled one.
    let addressed = resolve_addresses(&mut pack)?;
    let mut warnings = Vec::new();
    for e in &pack.entries {
        if e.grain_type == "tool" {
            check_tool(e, &mut warnings)?;
        }
    }
    if flag(flags, "dry-run").is_some() {
        println!("pack {} {} — would install {} grains, {} blobs (dry run)",
                 pack.name, pack.version, addressed.len(), pack.blobs.len());
        return Ok(());
    }

    let mut stored_blobs = Vec::new();
    for (name, (_, bytes, addr)) in &pack.blobs {
        let stored = facade
            .with_store(|s| s.put_blob(bytes))
            .map_err(|e| format!("storing blob {name}: {e}"))?;
        if &stored != addr {
            // Cannot happen — both are SHA-256 of the same bytes — but a
            // silent disagreement here would mean a grain naming an address
            // the store does not hold.
            return Err(format!(
                "blob {name} stored as {stored} but addressed as {addr} — refusing"
            ));
        }
        stored_blobs.push((name.clone(), stored));
    }

    // Registry rows go in FIRST: a trigger naming a `context_query` is
    // installable either way (the reference is a name, resolved at fire
    // time), but an install that ordered them the other way would leave a
    // window where the memory says it can assemble context it cannot.
    for (key, body) in &pack.registry {
        facade
            .with_store(|s| s.meta_put(key, body))
            .map_err(|e| format!("installing {key}: {e}"))?;
    }

    let mut written = Vec::new();
    for (e, (_, ty, expected)) in pack.entries.iter().zip(addressed.iter()) {
        let hash = facade
            .cal_add(&e.grain_type, &e.fields)
            .map_err(|err| format!("{}: {err}", e.file))?;
        let hex = hash.to_hex();
        // The address a pure build predicted and the address the store
        // recorded are the same function of the same bytes; asserting it here
        // is what lets `validate` speak for `install`.
        if &hex != expected {
            return Err(format!(
                "{}: built to {expected} but stored as {hex} — refusing, because \
                 validate no longer describes install",
                e.file
            ));
        }
        written.push(json!({ "file": e.file, "type": ty, "hash": hex }));
    }

    if json_out {
        println!(
            "{}",
            json!({
                "pack": pack.name, "version": pack.version, "namespace": ns,
                "grains": written,
                "blobs": stored_blobs.iter().map(|(n, a)| json!({"name": n, "address": a})).collect::<Vec<_>>(),
                "allow_executor": pins(&pack),
                "warnings": warnings,
            })
        );
    } else {
        println!("installed pack {} {} into ns '{}'", pack.name, pack.version, ns);
        for key in pack.registry.keys() {
            println!("  meta  {key}");
        }
        for (name, addr) in &stored_blobs {
            println!("  blob  {name:16} {addr}");
        }
        for w in &written {
            println!(
                "  grain {:16} {}  ({})",
                w["type"].as_str().unwrap_or(""),
                w["hash"].as_str().unwrap_or(""),
                w["file"].as_str().unwrap_or("")
            );
        }
        for w in &warnings {
            eprintln!("areev: pack: {w}");
        }
        let pins = pins(&pack);
        if !pins.is_empty() {
            println!(
                "\nNothing code-carrying runs until this host pins it:\n  --allow-executor {}",
                pins.join(",")
            );
        }
    }
    Ok(())
}

/// The addresses a host must pin to run this pack's code.
///
/// Only what a Definition actually names as its `executor_uri`: a pack may
/// carry blobs that are DATA — a fixture feed, a rule table, a document a tool
/// reads by address — and telling an operator to pin those would ask them to
/// authorize as executable something that never executes. The pin is the
/// authorization to run code, so the list has to be exactly the code.
fn pins(pack: &Pack) -> Vec<String> {
    let mut out: Vec<String> = pack
        .entries
        .iter()
        .filter_map(|e| e.fields.get("executor_uri").and_then(Value::as_str))
        .map(|u| u.trim_start_matches("cas://sha256:").to_string())
        .collect();
    out.sort();
    out.dedup();
    out
}

/// An exported pack: import the bundle, then assert what it was supposed to
/// carry.
fn install_bundle(
    facade: &AreevFacade,
    pack: &Pack,
    bundle: &str,
    json_out: bool,
) -> Result<(), String> {
    let path = pack.dir.join(bundle);
    let stats = facade
        .with_store(|s| s.import_bundle(path.to_str().unwrap_or(bundle)))
        .map_err(|e| format!("importing {}: {e}", path.display()))?;
    let mut missing = Vec::new();
    for (label, hash) in &pack.expect {
        let h = Hash::from_hex(hash).map_err(|e| format!("expect {label}: {e}"))?;
        if facade.with_store(|s| s.get(&h)).is_err() {
            missing.push(format!("  {label}: {hash}"));
        }
    }
    if !missing.is_empty() {
        return Err(format!(
            "the bundle imported, but the pack does not contain what it says it does:\n{}\n\
             A bundle replicates ops; an expectation names a content address. \
             The two disagreeing means the bundle was cut from a different memory \
             than the manifest describes.",
            missing.join("\n")
        ));
    }
    if json_out {
        println!(
            "{}",
            json!({
                "pack": pack.name, "version": pack.version,
                "applied": stats.applied, "skipped": stats.skipped,
                "expect": pack.expect.iter().map(|(l, h)| json!({"name": l, "hash": h})).collect::<Vec<_>>(),
            })
        );
    } else {
        println!(
            "installed pack {} {} from {bundle}: {} ops applied, {} skipped; {} expectations met",
            pack.name,
            pack.version,
            stats.applied,
            stats.skipped,
            pack.expect.len()
        );
    }
    Ok(())
}

/// Grain types a pack carries. A pack is a DECLARATION — what an agent is —
/// so it takes definitions, plans, standing rules and seed knowledge, and
/// deliberately not the record of anything that ran: journal Tool grains,
/// Events, Observations and run State are history, they belong to the memory
/// that produced them, and shipping them would make every install a different
/// memory.
const PACKABLE: &[GrainType] = &[
    GrainType::Tool,
    GrainType::Workflow,
    GrainType::Trigger,
    GrainType::Fact,
    GrainType::Skill,
];

fn export(
    m: Areev,
    ns: &str,
    flags: &HashMap<String, String>,
    json_out: bool,
) -> Result<(), String> {
    let out = PathBuf::from(need(flags, "out")?);
    let name = flag(flags, "name").unwrap_or_else(|| ns.to_string());
    let version = flag(flags, "pack-version").unwrap_or_else(|| "1.0.0".into());
    // `--pack-format`, not `--format`: the latter already means "how should
    // this command print", and one flag meaning two things is how an operator
    // ends up writing a bundle when they wanted JSON output.
    let format = flag(flags, "pack-format").unwrap_or_else(|| "source".into());
    std::fs::create_dir_all(&out).map_err(|e| format!("{}: {e}", out.display()))?;
    let facade = AreevFacade::with_session(m, Some(ns.to_string()), None);

    if format == "bundle" {
        return export_bundle(&facade, &out, &name, &version, ns, json_out);
    }
    if format != "source" {
        return Err(format!("--pack-format: expected source|bundle, got {format:?}"));
    }

    std::fs::create_dir_all(out.join("grains")).map_err(|e| e.to_string())?;
    let limit: usize = flag(flags, "limit").and_then(|v| v.parse().ok()).unwrap_or(10_000);
    let mut grains = Vec::new();
    let mut blobs: BTreeMap<String, (String, String)> = BTreeMap::new();
    let mut files = Vec::new();
    let mut skipped = Vec::new();

    for ty in PACKABLE {
        let found = facade
            .with_store(|s| s.recent(ns, Some(*ty), limit))
            .map_err(|e| e.to_string())?;
        for g in found {
            // A Tool grain is either a definition or a journal entry, and only
            // one of those is part of what an agent IS.
            if *ty == GrainType::Tool && g.get_str("kind") != Some("definition") {
                continue;
            }
            let hash = g.hash.to_hex();
            let mut doc = Map::new();
            doc.insert("type".into(), json!(type_name(*ty)));
            for (k, v) in &g.fields {
                if v.is_null() {
                    continue;
                }
                doc.insert(k.clone(), v.clone());
            }
            // A blob a grain names travels with the pack, under a symbolic
            // name — an address in a source pack would be unverifiable text.
            if let Some(uri) = doc.get("executor_uri").and_then(Value::as_str).map(str::to_string) {
                match facade.with_store(|s| s.get_blob(&uri)) {
                    Ok(bytes) => {
                        let key = doc
                            .get("tool_name")
                            .and_then(Value::as_str)
                            .unwrap_or("code")
                            .replace(['.', '/', ' '], "_");
                        let rel = format!("blobs/{key}.wasm");
                        std::fs::create_dir_all(out.join("blobs")).map_err(|e| e.to_string())?;
                        std::fs::write(out.join(&rel), &bytes)
                            .map_err(|e| format!("{rel}: {e}"))?;
                        blobs.insert(key.clone(), (rel, uri.clone()));
                        doc.insert("executor_uri".into(), json!(format!("blob:{key}")));
                    }
                    Err(e) => skipped.push(format!(
                        "{hash}: names {uri}, which this memory does not hold ({e}) — \
                         exported as a bare address"
                    )),
                }
            }
            grains.push((hash, type_name(*ty).to_string(), doc));
        }
    }

    // Dependency order: a plan binds tool hashes, a trigger names a plan. A
    // pack that seeded them the other way round would install a trigger
    // pointing at a plan that is not there yet — legal (the reference is a
    // hash, not a foreign key) but unreadable when it goes wrong.
    grains.sort_by_key(|(_, ty, _)| match ty.as_str() {
        "tool" => 0,
        "skill" => 1,
        "fact" => 2,
        "workflow" => 3,
        "trigger" => 4,
        _ => 5,
    });

    let mut manifest_grains = Vec::new();
    let mut unreproducible = Vec::new();
    let mut used: BTreeMap<String, usize> = BTreeMap::new();
    for (i, (hash, ty, doc)) in grains.iter().enumerate() {
        // A readable name, because a pack is meant to be reviewed as a diff.
        // Two grains of one type can legitimately share one — two mailbox
        // triggers on one connector — so a repeat takes its address as the
        // tiebreak rather than silently overwriting the first file.
        let label = doc
            .get("tool_name")
            .or_else(|| doc.get("name"))
            .or_else(|| doc.get("connector"))
            .and_then(Value::as_str)
            .unwrap_or(ty)
            .replace(['.', '/', ' '], "-");
        let label = match used.entry(format!("{ty}-{label}")).or_insert(0) {
            n if *n == 0 => {
                *n += 1;
                label
            }
            n => {
                *n += 1;
                format!("{label}-{}", &hash[..8])
            }
        };
        let rel = format!("grains/{:03}-{ty}-{label}.json", (i + 1) * 10);
        // Prove the round trip BEFORE writing the manifest: an exported pack
        // that cannot be re-installed at the same addresses is not a pack, and
        // finding that out at install time on someone else's machine is the
        // failure this check exists to prevent.
        let mut fields = doc.clone();
        fields.remove("type");
        let mut resolved = fields.clone();
        let blob_map: BTreeMap<String, (String, Vec<u8>, String)> = blobs
            .iter()
            .map(|(k, (rel, addr))| (k.clone(), (rel.clone(), Vec::new(), addr.clone())))
            .collect();
        resolve_blob_refs(&mut resolved, &blob_map, &rel)?;
        let rebuilt = areev_cal::json_build::build_grain_from_json(ty, &resolved, AddressSink)
            .map(|h| h.to_hex());
        match rebuilt {
            Ok(h) if &h == hash => {
                std::fs::write(
                    out.join(&rel),
                    serde_json::to_string_pretty(&Value::Object(doc.clone())).unwrap() + "\n",
                )
                .map_err(|e| format!("{rel}: {e}"))?;
                manifest_grains.push(json!({ "file": rel, "expected_hash": hash }));
                files.push(rel);
            }
            Ok(h) => unreproducible.push(format!("  {hash} ({ty}) rebuilds as {h}")),
            Err(e) => unreproducible.push(format!("  {hash} ({ty}) will not rebuild: {e}")),
        }
    }
    if !unreproducible.is_empty() {
        return Err(format!(
            "these grains do not round-trip through the pack format, so the pack was not \
             written:\n{}\n\
             A source pack is re-BUILT at install, so anything it cannot reproduce byte for \
             byte would install at a different address than it was exported from. Export \
             this memory with --pack-format bundle instead, which replicates the grains \
             themselves.",
            unreproducible.join("\n")
        ));
    }

    // Saved queries and templates travel with the file in a bundle; a source
    // pack has to carry them explicitly, or an installed agent loses the
    // context assembly its triggers name.
    let mut queries = Map::new();
    let mut templates = Map::new();
    for (prefix, out) in [("qry:", &mut queries), ("tpl:", &mut templates)] {
        for (key, body) in facade
            .with_store(|s| s.meta_scan(prefix))
            .map_err(|e| e.to_string())?
        {
            let name = key.trim_start_matches(prefix).to_string();
            // Stored as JSON with usage stats stripped on read; keep the text
            // exactly as the store hands it back.
            out.insert(name, serde_json::from_str(&body).unwrap_or(json!(body)));
        }
    }

    let mut manifest = Map::new();
    manifest.insert("pack".into(), json!(name));
    manifest.insert("version".into(), json!(version));
    manifest.insert("namespace".into(), json!(ns));
    if !blobs.is_empty() {
        manifest.insert(
            "blobs".into(),
            Value::Object(
                blobs.iter().map(|(k, (rel, _))| (k.clone(), json!(rel))).collect::<Map<_, _>>(),
            ),
        );
    }
    manifest.insert("grains".into(), json!(manifest_grains));
    if !queries.is_empty() {
        manifest.insert("queries".into(), Value::Object(queries));
    }
    if !templates.is_empty() {
        manifest.insert("templates".into(), Value::Object(templates));
    }
    let manifest = Value::Object(manifest);
    std::fs::write(
        out.join("pack.json"),
        serde_json::to_string_pretty(&manifest).unwrap() + "\n",
    )
    .map_err(|e| e.to_string())?;

    for s in &skipped {
        eprintln!("areev: pack: {s}");
    }
    if json_out {
        println!(
            "{}",
            json!({ "pack": name, "version": version, "out": out.display().to_string(),
                    "grains": files.len(), "blobs": blobs.len(), "format": "source" })
        );
    } else {
        println!(
            "exported pack {name} {version} → {} ({} grains, {} blobs)",
            out.display(),
            files.len(),
            blobs.len()
        );
    }
    Ok(())
}

fn export_bundle(
    facade: &AreevFacade,
    out: &Path,
    name: &str,
    version: &str,
    ns: &str,
    json_out: bool,
) -> Result<(), String> {
    let bundle_rel = "pack.mgb";
    let bundle_path = out.join(bundle_rel);
    let stats = facade
        .with_store(|s| s.bundle_since(0, bundle_path.to_str().unwrap_or(bundle_rel)))
        .map_err(|e| format!("bundling: {e}"))?;
    // What the bundle is supposed to contain, so an install can say more than
    // "some ops applied". Plans first: a trigger names one by hash, so a plan
    // that did not arrive is the failure that matters.
    let mut expect = Vec::new();
    for ty in [GrainType::Workflow, GrainType::Trigger] {
        for g in facade
            .with_store(|s| s.recent(ns, Some(ty), 1000))
            .map_err(|e| e.to_string())?
        {
            let label = g
                .get_str("name")
                .unwrap_or(type_name(ty))
                .to_string();
            expect.push(json!({ "name": label, "kind": type_name(ty), "hash": g.hash.to_hex() }));
        }
    }
    let manifest = json!({
        "pack": name, "version": version, "namespace": ns,
        "bundle": bundle_rel, "expect": expect,
    });
    std::fs::write(
        out.join("pack.json"),
        serde_json::to_string_pretty(&manifest).unwrap() + "\n",
    )
    .map_err(|e| e.to_string())?;
    if json_out {
        println!(
            "{}",
            json!({ "pack": name, "version": version, "out": out.display().to_string(),
                    "format": "bundle", "ops": stats.ops, "bytes": stats.bytes,
                    "expect": expect.len() })
        );
    } else {
        println!(
            "exported pack {name} {version} → {} ({} ops, {} bytes, {} expectations)",
            out.display(),
            stats.ops,
            stats.bytes,
            expect.len()
        );
    }
    Ok(())
}

fn type_name(t: GrainType) -> &'static str {
    match t {
        GrainType::Tool => "tool",
        GrainType::Workflow => "workflow",
        GrainType::Trigger => "trigger",
        GrainType::Fact => "fact",
        GrainType::Skill => "skill",
        other => other.as_str(),
    }
}
