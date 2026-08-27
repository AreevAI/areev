//! Migration importers — read another memory system's export and write it
//! into an Areev file with original timestamps, provenance, and (where the
//! source has one) the full edit history preserved as supersession chains.
//!
//! File-based by design: no network calls, no source-SDK dependencies. Each
//! function takes the export payload the caller read from disk (JSON value or
//! JSONL text) — `docs/migrate.md` documents the one-liner that produces each
//! payload from its source system. The CLI surface is `areev migrate --from
//! <source> --file <path>`; bindings expose the same via `migrate()`.
//!
//! Import conventions, shared by every source:
//! - `created_at` = the source's original timestamp (epoch ms); the op-log
//!   HLC records when *this store* learned it — both truths are kept.
//! - `source_type = "import"`, and `context.import` carries the source name
//!   plus original ids, so every imported grain is auditable back to where
//!   it came from.
//! - Prose goes in `context.content` (never the term dictionary) and, capped,
//!   in `embedding_text` so the FTS and vector legs index the original text.
//! - Sources with edit history (mem0) replay it: ADD → add, UPDATE →
//!   supersede, DELETE → forget — arriving with their original timestamps,
//!   so `history()` shows the pre-import evolution.
//! - Note-shaped sources (Basic Memory, Letta core-memory blocks) import as
//!   `memory_file` chains under `/memories/...` — the same shape the
//!   Anthropic memory-tool backend edits, so imported notes are immediately
//!   live for `view`/`str_replace`/`insert` (see `memory_tool.rs`).

use std::collections::HashMap;

use areev_core::error::{AreevError, Hash, Result};
use areev_core::format::serialize::serialize_grain;
use areev_core::types::{Event, Fact, Grain, Role};
use serde_json::{json, Value};

use crate::memory_tool::MEMORY_FILE_RELATION;
use crate::Areev;

/// Cap on `embedding_text` (documented grain-level limit: 8 KiB).
const ET_MAX_BYTES: usize = 8192;
/// Cap on collected anomaly notes so a messy export can't balloon the report.
const NOTES_MAX: usize = 50;

/// What an import did. `notes` collects per-record anomalies (skipped rows,
/// unparseable timestamps) up to a cap — an import never fails because one
/// record is malformed.
#[derive(Debug, Default)]
pub struct MigrateReport {
    pub added: usize,
    pub superseded: usize,
    pub forgotten: usize,
    pub skipped: usize,
    pub notes: Vec<String>,
}

impl MigrateReport {
    fn note(&mut self, s: String) {
        if self.notes.len() < NOTES_MAX {
            self.notes.push(s);
        } else if self.notes.len() == NOTES_MAX {
            self.notes.push("… further notes suppressed".to_string());
        }
    }

    pub fn to_json(&self) -> Value {
        json!({
            "added": self.added,
            "superseded": self.superseded,
            "forgotten": self.forgotten,
            "skipped": self.skipped,
            "notes": self.notes,
        })
    }
}

// ---- shared helpers -------------------------------------------------------

/// Add a grain unless the store already holds its content address —
/// re-running the same import is a no-op, not an error (the UNIQUE(hash)
/// index rejects duplicates; we probe first to report them as `skipped`).
///
/// Caveat: a record with no source timestamp gets the import time as
/// `created_at`, which lands in the blob — so only records that carry their
/// own timestamp (all real exports do) are exactly re-run-dedupable.
fn add_dedup<G: Grain + 'static>(
    m: &mut Areev,
    grain: &G,
    rep: &mut MigrateReport,
) -> Result<()> {
    let (_, hash) = serialize_grain(grain)?;
    if m.get(&hash).is_ok() {
        rep.skipped += 1;
        return Ok(());
    }
    m.add(grain)?;
    rep.added += 1;
    Ok(())
}

/// Short content digest for the `object` slot, so the term dictionary never
/// stores bodies (same convention as `memory_tool::write_version`).
fn digest6(content: &str) -> String {
    use sha2::{Digest, Sha256};
    hex::encode(&Sha256::digest(content.as_bytes())[..6])
}

/// Clip to the `embedding_text` byte cap on a char boundary.
pub(crate) fn clip_et(s: &str) -> String {
    if s.len() <= ET_MAX_BYTES {
        return s.to_string();
    }
    let mut end = ET_MAX_BYTES;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_string()
}

/// Parse an ISO-8601 timestamp to epoch milliseconds.
///
/// Re-exported from [`areev_core::time`], which is where the one
/// implementation lives — the credential map's `expires_at` needs the same
/// parse and `authz` sits below this crate. Kept as a `pub use` under the old
/// path so importer call sites (and anything outside the workspace) do not
/// move.
pub use areev_core::time::iso8601_to_ms;

/// Timestamp from a JSON value: ISO-8601 string, epoch seconds, or epoch ms
/// (heuristic: ≥ 10^11 is already milliseconds).
fn parse_ts(v: Option<&Value>) -> Option<i64> {
    match v? {
        Value::String(s) => iso8601_to_ms(s),
        Value::Number(n) => {
            let x = n.as_f64()?;
            if x >= 1e11 {
                Some(x as i64)
            } else {
                Some((x * 1000.0) as i64)
            }
        }
        _ => None,
    }
}

fn as_str<'a>(v: &'a Value, keys: &[&str]) -> Option<&'a str> {
    keys.iter().find_map(|k| v.get(k).and_then(Value::as_str))
}

/// The list payload of an export that is either a bare array or wrapped in
/// one of the given keys (`{"results": [...]}` etc.).
fn item_array<'a>(v: &'a Value, keys: &[&str]) -> Option<&'a Vec<Value>> {
    if let Some(arr) = v.as_array() {
        return Some(arr);
    }
    keys.iter().find_map(|k| v.get(k).and_then(Value::as_array))
}

/// A prose-bearing Fact in the shared import shape (see module docs).
#[allow(clippy::too_many_arguments)]
fn import_fact(
    ns: &str,
    subject: &str,
    relation: &str,
    content: &str,
    source: &str,
    import_ctx: Value,
    created_at: Option<i64>,
    user_id: Option<&str>,
    tags: Vec<String>,
) -> Fact {
    let mut f = Fact::new(subject, relation, &format!("v:{}", digest6(content)));
    f.common.namespace = Some(ns.to_string());
    f.common.source_type = Some("import".to_string());
    f.common.embedding_text = Some(clip_et(content));
    f.common.created_at = created_at;
    f.common.user_id = user_id.map(str::to_string);
    f.common.tags = tags;
    let mut import = import_ctx;
    import["source"] = json!(source);
    f.common.context = Some(json!({ "content": content, "import": import }));
    f
}

/// String-payload dispatcher for the language bindings (the CLI has its own
/// file-based dispatcher, which adds the `basic-memory` vault-directory
/// walk). Wraps the load in [`Areev::defer_text_index`] /
/// [`Areev::rebuild_text_index`] so bulk imports skip the per-transaction
/// FTS tax.
pub fn migrate_payload(
    m: &mut Areev,
    ns: &str,
    source: &str,
    payload: &str,
    history: Option<&str>,
) -> Result<MigrateReport> {
    let parse = |s: &str| {
        serde_json::from_str::<Value>(s)
            .map_err(|e| AreevError::Validation(format!("bad JSON payload: {e}")))
    };
    let deferred = m.defer_text_index()?;
    let rep = match source {
        "mem0" => {
            let x = parse(payload)?;
            let h = history.map(parse).transpose()?;
            migrate_mem0(m, ns, Some(&x), h.as_ref())
        }
        "mem0-history" => {
            let h = parse(payload)?;
            migrate_mem0(m, ns, None, Some(&h))
        }
        "langgraph" | "langmem" => migrate_langgraph(m, ns, payload),
        "letta" => {
            let af = parse(payload)?;
            migrate_letta(m, ns, &af)
        }
        "letta-archival" => migrate_letta_archival(m, ns, payload),
        "zep" | "graphiti" => {
            let v = parse(payload)?;
            migrate_zep(m, ns, &v)
        }
        "jsonl" => migrate_jsonl(m, ns, payload),
        other => Err(AreevError::Validation(format!(
            "unknown migrate source '{other}' — mem0, mem0-history, langgraph, letta, \
             letta-archival, zep, jsonl (basic-memory is CLI-only: areev migrate)"
        ))),
    };
    if deferred {
        m.rebuild_text_index()?;
    }
    rep
}

// ---- mem0 -----------------------------------------------------------------

/// Import a mem0 export: `export` is the get-all payload (Platform
/// `POST /v3/memories/` pages or OSS `Memory.get_all()` — bare array or
/// `{"results": [...]}`), `history` optionally the concatenated per-memory
/// `history()` events (Platform) or an OSS `history` table dump.
///
/// With history, each memory id becomes a real supersession chain — ADD →
/// add, UPDATE → supersede, DELETE → forget — with original timestamps, so
/// the pre-import evolution is queryable via `HISTORY`. (The official
/// mem0→Zep and mem0→Supermemory guides drop this history; we keep it.)
/// Content-address of the first grain a mem0 history chain would ADD (its first
/// ADD/UPDATE event carrying non-empty text). Recomputed deterministically from
/// the same `import_fact` inputs, so it matches the grain a prior run stored —
/// used to detect a re-import even after the chain was DELETEd, which forgets
/// its grains and empties `history()` (so the history guard alone would
/// re-process it, double-counting and — if interrupted mid-chain — resurrecting
/// a deleted memory).
fn first_chain_grain_hash(ns: &str, subject: &str, id: &str, evs: &[&Value]) -> Option<Hash> {
    for e in evs {
        let action = as_str(e, &["event", "action"]).unwrap_or("").to_ascii_uppercase();
        if action != "ADD" && action != "UPDATE" {
            continue;
        }
        let Some(text) =
            as_str(e, &["new_memory", "new_value", "memory"]).filter(|t| !t.trim().is_empty())
        else {
            continue;
        };
        let ts = parse_ts(e.get("created_at").or_else(|| e.get("updated_at")));
        let f = import_fact(
            ns,
            subject,
            "mem0_memory",
            text,
            "mem0",
            json!({ "id": id, "event": action }),
            ts,
            None,
            Vec::new(),
        );
        return serialize_grain(&f).ok().map(|(_, h)| h);
    }
    None
}

pub fn migrate_mem0(
    m: &mut Areev,
    ns: &str,
    export: Option<&Value>,
    history: Option<&Value>,
) -> Result<MigrateReport> {
    let mut rep = MigrateReport::default();
    if export.is_none() && history.is_none() {
        return Err(AreevError::Validation(
            "mem0 import needs an export payload, a history payload, or both".into(),
        ));
    }

    let subject_of = |id: &str| format!("mem0/{id}");
    // Memory ids the history replay settled (imported, pre-existing, or
    // deleted) — the export pass must not touch them again.
    let mut handled: std::collections::HashSet<String> = Default::default();

    if let Some(h) = history {
        let empty = Vec::new();
        let mut events: Vec<&Value> = item_array(h, &["results", "history", "memories"])
            .unwrap_or(&empty)
            .iter()
            .collect();
        // Source order: by timestamp; ties keep input order (stable sort).
        events.sort_by_key(|e| {
            parse_ts(e.get("created_at").or_else(|| e.get("updated_at"))).unwrap_or(0)
        });
        // Group into per-memory chains, preserving time order.
        let mut chains: Vec<(String, Vec<&Value>)> = Vec::new();
        let mut idx: HashMap<String, usize> = HashMap::new();
        for e in events {
            let Some(id) = as_str(e, &["memory_id", "id"]) else {
                rep.skipped += 1;
                rep.note("history event without memory_id".into());
                continue;
            };
            match idx.get(id) {
                Some(&i) => chains[i].1.push(e),
                None => {
                    idx.insert(id.to_string(), chains.len());
                    chains.push((id.to_string(), vec![e]));
                }
            }
        }
        // Every grain hash this file has ever recorded (an OP_ADD record
        // survives a later forget), snapshotted before replay so a chain
        // imported by a PRIOR run — even one later DELETEd — is recognized.
        let ever_added: std::collections::HashSet<Hash> = m
            .changes_since(0, usize::MAX / 2)?
            .into_iter()
            .filter(|o| o.op == crate::OP_ADD)
            .map(|o| o.hash)
            .collect();
        for (id, evs) in chains {
            let subject = subject_of(&id);
            // Re-run safety: skip a chain already imported — present, edited, OR
            // deleted. history() catches present/edited chains; the
            // content-address probe additionally catches delete-terminated ones
            // (their grains are forgotten, so history() is empty), which a
            // re-run would otherwise re-add then re-delete.
            let already = first_chain_grain_hash(ns, &subject, &id, &evs)
                .is_some_and(|h| ever_added.contains(&h))
                || !m.history(ns, &subject, "mem0_memory")?.is_empty();
            if already {
                rep.skipped += evs.len();
                rep.note(format!("mem0 {id}: already imported — skipped"));
                handled.insert(id);
                continue;
            }
            let mut head: Option<areev_core::error::Hash> = None;
            for e in evs {
                let action = as_str(e, &["event", "action"]).unwrap_or("").to_ascii_uppercase();
                let new_text = as_str(e, &["new_memory", "new_value", "memory"]);
                let ts = parse_ts(e.get("created_at").or_else(|| e.get("updated_at")));
                match action.as_str() {
                    "ADD" | "UPDATE" => {
                        let Some(text) = new_text.filter(|t| !t.trim().is_empty()) else {
                            rep.skipped += 1;
                            rep.note(format!("{action} event for {id} without new text"));
                            continue;
                        };
                        let mut f = import_fact(
                            ns,
                            &subject,
                            "mem0_memory",
                            text,
                            "mem0",
                            json!({ "id": id, "event": action }),
                            ts,
                            None,
                            Vec::new(),
                        );
                        head = Some(match head {
                            Some(prev) => {
                                rep.superseded += 1;
                                m.supersede(&prev, &mut f)?
                            }
                            None => {
                                rep.added += 1;
                                m.add(&f)?
                            }
                        });
                    }
                    "DELETE" => {
                        for v in m.history(ns, &subject, "mem0_memory")? {
                            m.forget(&v.hash)?;
                            rep.forgotten += 1;
                        }
                        head = None;
                    }
                    "NOOP" | "" => {}
                    other => {
                        rep.skipped += 1;
                        rep.note(format!("unknown mem0 history event '{other}' for {id}"));
                    }
                }
            }
            handled.insert(id);
        }
    }

    if let Some(x) = export {
        let empty = Vec::new();
        let items = item_array(x, &["results", "memories"]).unwrap_or(&empty);
        for it in items {
            let Some(text) = as_str(it, &["memory", "content", "text"]) else {
                rep.skipped += 1;
                rep.note("export item without memory text".into());
                continue;
            };
            let id = as_str(it, &["id"]).map(str::to_string).unwrap_or_else(|| digest6(text));
            // History already settled this memory — the export is just its
            // final state. A chain from a previous run is likewise left alone.
            if handled.contains(&id) {
                continue;
            }
            if !m.history(ns, &subject_of(&id), "mem0_memory")?.is_empty() {
                rep.skipped += 1;
                continue;
            }
            let user = as_str(it, &["user_id", "agent_id", "run_id", "app_id"]);
            let tags: Vec<String> = it
                .get("categories")
                .and_then(Value::as_array)
                .map(|a| a.iter().filter_map(|t| t.as_str().map(str::to_string)).collect())
                .unwrap_or_default();
            let mut import = json!({ "id": id });
            for k in ["user_id", "agent_id", "run_id", "app_id"] {
                if let Some(v) = it.get(k).and_then(Value::as_str) {
                    import[k] = json!(v);
                }
            }
            if let Some(md) = it.get("metadata").filter(|v| !v.is_null()) {
                import["metadata"] = md.clone();
            }
            let f = import_fact(
                ns,
                &subject_of(&id),
                "mem0_memory",
                text,
                "mem0",
                import,
                parse_ts(it.get("created_at")).or_else(|| parse_ts(it.get("updated_at"))),
                user,
                tags,
            );
            m.add(&f)?;
            handled.insert(id);
            rep.added += 1;
        }
    }
    Ok(rep)
}

// ---- LangGraph / LangMem store --------------------------------------------

/// Import a LangGraph BaseStore dump (LangMem persists through it): JSONL,
/// one `{"prefix": ..., "key": ..., "value": {...}, "created_at": ...}` per
/// line — the shape of `SELECT row_to_json(t) FROM store t` on the
/// `langgraph` Postgres schema (`prefix` may be a string or an array).
pub fn migrate_langgraph(m: &mut Areev, ns: &str, jsonl: &str) -> Result<MigrateReport> {
    let mut rep = MigrateReport::default();
    for (lineno, line) in jsonl.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            rep.skipped += 1;
            rep.note(format!("line {}: not valid JSON", lineno + 1));
            continue;
        };
        let prefix = match v.get("prefix") {
            Some(Value::String(s)) => s.clone(),
            Some(Value::Array(a)) => a
                .iter()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join("/"),
            _ => String::new(),
        };
        let Some(key) = as_str(&v, &["key"]) else {
            rep.skipped += 1;
            rep.note(format!("line {}: missing key", lineno + 1));
            continue;
        };
        let Some(value) = v.get("value") else {
            rep.skipped += 1;
            rep.note(format!("line {}: missing value", lineno + 1));
            continue;
        };
        // A store value is arbitrary JSON; when it is (or wraps) a single
        // prose string, index that — otherwise the compact JSON.
        let content = value
            .as_str()
            .map(str::to_string)
            .or_else(|| as_str(value, &["content", "text", "memory"]).map(str::to_string))
            .unwrap_or_else(|| value.to_string());
        let subject = if prefix.is_empty() {
            format!("langgraph/{key}")
        } else {
            format!("langgraph/{prefix}/{key}")
        };
        let mut f = import_fact(
            ns,
            &subject,
            "langgraph_item",
            &content,
            "langgraph",
            json!({ "prefix": prefix, "key": key }),
            parse_ts(v.get("created_at")).or_else(|| parse_ts(v.get("updated_at"))),
            None,
            Vec::new(),
        );
        // Keep the full structured value when the prose was extracted from it.
        if !value.is_string() {
            if let Some(ctx) = f.common.context.as_mut() {
                ctx["value"] = value.clone();
            }
        }
        add_dedup(m, &f, &mut rep)?;
    }
    Ok(rep)
}

// ---- Basic Memory ---------------------------------------------------------

/// Import one Basic Memory note (a markdown file with optional YAML
/// frontmatter). Notes become `memory_file` chains under
/// `/memories/<permalink|path>` — the exact shape the Anthropic memory-tool
/// backend serves, so an imported vault is immediately editable by an agent.
/// The caller walks the vault directory and supplies each file's relative
/// path and mtime (used only when the frontmatter has no date).
pub fn migrate_basic_memory_note(
    m: &mut Areev,
    ns: &str,
    rel_path: &str,
    markdown: &str,
    mtime_ms: Option<i64>,
    rep: &mut MigrateReport,
) -> Result<()> {
    let (title, permalink, tags, created) = parse_frontmatter(markdown);
    let stem = permalink.unwrap_or_else(|| {
        rel_path.trim_end_matches(".md").trim_matches('/').to_string()
    });
    if stem.is_empty() || markdown.trim().is_empty() {
        rep.skipped += 1;
        rep.note(format!("{rel_path}: empty note or path"));
        return Ok(());
    }
    let subject = format!("/memories/{stem}");
    // Re-run safety: an already-imported note may have been edited by the
    // memory tool since — never clobber its chain.
    if !m.history(ns, &subject, MEMORY_FILE_RELATION)?.is_empty() {
        rep.skipped += 1;
        return Ok(());
    }
    let et = match &title {
        Some(t) => format!("{t}\n{markdown}"),
        None => markdown.to_string(),
    };
    let mut f = import_fact(
        ns,
        &subject,
        MEMORY_FILE_RELATION,
        markdown,
        "basic-memory",
        json!({ "path": rel_path, "title": title }),
        created.or(mtime_ms),
        None,
        tags,
    );
    f.common.embedding_text = Some(clip_et(&et));
    m.add(&f)?;
    rep.added += 1;
    Ok(())
}

/// Minimal YAML frontmatter reader: `key: value` lines and one-level `- item`
/// lists between `---` fences. Returns (title, permalink, tags, created-ms).
/// No YAML dependency by policy; anything fancier is preserved verbatim in
/// the note body anyway.
fn parse_frontmatter(md: &str) -> (Option<String>, Option<String>, Vec<String>, Option<i64>) {
    let mut title = None;
    let mut permalink = None;
    let mut tags: Vec<String> = Vec::new();
    let mut created = None;
    let mut lines = md.lines();
    if lines.next().map(str::trim) != Some("---") {
        return (title, permalink, tags, created);
    }
    let mut in_tags = false;
    for line in lines {
        let t = line.trim();
        if t == "---" {
            break;
        }
        if in_tags {
            if let Some(item) = t.strip_prefix("- ") {
                tags.push(item.trim().trim_matches('"').to_string());
                continue;
            }
            in_tags = false;
        }
        let Some((k, v)) = t.split_once(':') else { continue };
        let (k, v) = (k.trim(), v.trim().trim_matches('"'));
        match k {
            "title" => title = Some(v.to_string()),
            "permalink" => permalink = Some(v.trim_matches('/').to_string()),
            "created" | "date" => created = iso8601_to_ms(v),
            "tags" => {
                if v.is_empty() {
                    in_tags = true; // dash-list follows
                } else {
                    tags = v
                        .trim_start_matches('[')
                        .trim_end_matches(']')
                        .split(',')
                        .map(|s| s.trim().trim_matches('"').to_string())
                        .filter(|s| !s.is_empty())
                        .collect();
                }
            }
            _ => {}
        }
    }
    (title, permalink, tags, created)
}

// ---- Letta ----------------------------------------------------------------

/// Import a Letta agent file (`.af` v1/v2 JSON: a single agent object or
/// `{"agents": [...]}`). Core-memory blocks become editable `memory_file`
/// chains under `/memories/letta/<agent>/<label>`; the message history
/// becomes thread-indexed Events. Archival passages are NOT in `.af` —
/// export them separately and use [`migrate_letta_archival`].
pub fn migrate_letta(m: &mut Areev, ns: &str, af: &Value) -> Result<MigrateReport> {
    let mut rep = MigrateReport::default();
    let agents: Vec<&Value> = match af.get("agents").and_then(Value::as_array) {
        Some(a) => a.iter().collect(),
        None => vec![af],
    };
    for agent in agents {
        let name = as_str(agent, &["name", "id"]).unwrap_or("agent").to_string();
        // Blocks live at .core_memory, .blocks, or .memory.blocks across
        // .af revisions; each is {"label": ..., "value": ...}.
        let blocks = agent
            .get("core_memory")
            .and_then(Value::as_array)
            .or_else(|| agent.get("blocks").and_then(Value::as_array))
            .or_else(|| agent.get("memory").and_then(|mm| mm.get("blocks")).and_then(Value::as_array));
        if let Some(blocks) = blocks {
            for b in blocks {
                let Some(label) = as_str(b, &["label", "name"]) else {
                    rep.skipped += 1;
                    continue;
                };
                let Some(value) = as_str(b, &["value", "content"]) else {
                    rep.skipped += 1;
                    continue;
                };
                let subject = format!("/memories/letta/{name}/{label}");
                // An already-imported block may have been edited since.
                if !m.history(ns, &subject, MEMORY_FILE_RELATION)?.is_empty() {
                    rep.skipped += 1;
                    continue;
                }
                let f = import_fact(
                    ns,
                    &subject,
                    MEMORY_FILE_RELATION,
                    value,
                    "letta",
                    json!({ "agent": name, "block": label }),
                    parse_ts(agent.get("created_at")),
                    None,
                    Vec::new(),
                );
                m.add(&f)?;
                rep.added += 1;
            }
        }
        if let Some(messages) = agent.get("messages").and_then(Value::as_array) {
            for msg in messages {
                let role = as_str(msg, &["role"]).unwrap_or("");
                if role != "user" && role != "assistant" {
                    continue; // skip system/tool plumbing
                }
                let text = match msg.get("content") {
                    Some(Value::String(s)) => s.clone(),
                    Some(Value::Array(parts)) => parts
                        .iter()
                        .filter_map(|p| p.get("text").and_then(Value::as_str))
                        .collect::<Vec<_>>()
                        .join("\n"),
                    _ => as_str(msg, &["text"]).unwrap_or("").to_string(),
                };
                if text.trim().is_empty() {
                    continue;
                }
                let mut e = Event::new(&text);
                e.common.namespace = Some(ns.to_string());
                e.common.source_type = Some("import".to_string());
                e.common.created_at = parse_ts(msg.get("created_at"));
                e.session_id = Some(format!("letta/{name}"));
                e.role = Role::from_str(role);
                add_dedup(m, &e, &mut rep)?;
            }
        }
    }
    Ok(rep)
}

/// Import Letta archival memory (Passages): JSONL, one passage per line —
/// the items of paginated `GET /v1/agents/{id}/archival-memory` responses
/// (`{"text": ..., "created_at": ..., "tags": [...]}`).
pub fn migrate_letta_archival(m: &mut Areev, ns: &str, jsonl: &str) -> Result<MigrateReport> {
    let mut rep = MigrateReport::default();
    for (lineno, line) in jsonl.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            rep.skipped += 1;
            rep.note(format!("line {}: not valid JSON", lineno + 1));
            continue;
        };
        let Some(text) = as_str(&v, &["text", "content", "memory"]) else {
            rep.skipped += 1;
            rep.note(format!("line {}: no passage text", lineno + 1));
            continue;
        };
        let mut e = Event::new(text);
        e.common.namespace = Some(ns.to_string());
        e.common.source_type = Some("import".to_string());
        e.common.created_at = parse_ts(v.get("created_at"));
        e.common.tags = v
            .get("tags")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(|t| t.as_str().map(str::to_string)).collect())
            .unwrap_or_default();
        if let Some(id) = as_str(&v, &["id"]) {
            e.common.context = Some(json!({ "import": { "source": "letta", "id": id } }));
        }
        add_dedup(m, &e, &mut rep)?;
    }
    Ok(rep)
}

// ---- Zep / Graphiti --------------------------------------------------------

/// Import Zep Cloud / Graphiti data: `{"edges": [...], "episodes": [...]}`
/// (either key optional), or a bare array classified per item. Edge facts
/// carry Zep's bi-temporal `valid_at`/`invalid_at` on Areev's world-time
/// validity axis (`valid_from`/`valid_to`), so invalidated facts import as
/// no-longer-valid instead of current. Episodes become thread-indexed Events.
pub fn migrate_zep(m: &mut Areev, ns: &str, payload: &Value) -> Result<MigrateReport> {
    let mut rep = MigrateReport::default();
    let mut edges: Vec<&Value> = Vec::new();
    let mut episodes: Vec<&Value> = Vec::new();
    if let Some(arr) = payload.as_array() {
        for it in arr {
            if it.get("fact").is_some() {
                edges.push(it);
            } else {
                episodes.push(it);
            }
        }
    } else {
        if let Some(a) = payload.get("edges").and_then(Value::as_array) {
            edges.extend(a.iter());
        }
        if let Some(a) = payload.get("episodes").and_then(Value::as_array) {
            episodes.extend(a.iter());
        }
    }
    if edges.is_empty() && episodes.is_empty() {
        return Err(AreevError::Validation(
            "zep import: no edges or episodes found in payload".into(),
        ));
    }
    for e in edges {
        let Some(fact) = as_str(e, &["fact"]).filter(|f| !f.trim().is_empty()) else {
            rep.skipped += 1;
            rep.note("edge without fact text".into());
            continue;
        };
        let uuid = as_str(e, &["uuid", "id"]).unwrap_or("");
        let source = as_str(e, &["source_node_uuid"]).unwrap_or("graph");
        let relation = as_str(e, &["name"]).filter(|n| !n.is_empty()).unwrap_or("zep_fact");
        let mut import = json!({ "uuid": uuid });
        if let Some(t) = as_str(e, &["target_node_uuid"]) {
            import["target_node_uuid"] = json!(t);
        }
        let mut f = import_fact(
            ns,
            &format!("zep/{source}"),
            relation,
            fact,
            "zep",
            import,
            parse_ts(e.get("created_at")).or_else(|| parse_ts(e.get("valid_at"))),
            None,
            Vec::new(),
        );
        f.common.valid_from = parse_ts(e.get("valid_at"));
        f.common.valid_to =
            parse_ts(e.get("invalid_at")).or_else(|| parse_ts(e.get("expired_at")));
        add_dedup(m, &f, &mut rep)?;
    }
    for ep in episodes {
        let Some(content) = as_str(ep, &["content", "text"]).filter(|c| !c.trim().is_empty())
        else {
            rep.skipped += 1;
            rep.note("episode without content".into());
            continue;
        };
        let mut e = Event::new(content);
        e.common.namespace = Some(ns.to_string());
        e.common.source_type = Some("import".to_string());
        e.common.created_at = parse_ts(ep.get("created_at"));
        e.session_id = as_str(ep, &["thread_id", "session_id"]).map(|s| format!("zep/{s}"));
        if let Some(role) = as_str(ep, &["role"]) {
            e.role = Role::from_str(role);
        }
        if let Some(id) = as_str(ep, &["uuid", "id"]) {
            e.common.context = Some(json!({ "import": { "source": "zep", "id": id } }));
        }
        add_dedup(m, &e, &mut rep)?;
    }
    Ok(rep)
}

// ---- generic JSONL ---------------------------------------------------------

/// Import generic JSONL — the escape hatch for pgvector/Chroma/homegrown
/// stores: dump your table with one JSON object per line. `subject` +
/// `relation` + `object` → Fact; otherwise `content`/`text` → Event.
/// Optional per-line fields: `created_at`, `confidence`, `tags`, `user_id`,
/// `session_id`, `embedding_text`.
pub fn migrate_jsonl(m: &mut Areev, ns: &str, jsonl: &str) -> Result<MigrateReport> {
    let mut rep = MigrateReport::default();
    for (lineno, line) in jsonl.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            rep.skipped += 1;
            rep.note(format!("line {}: not valid JSON", lineno + 1));
            continue;
        };
        let created = parse_ts(v.get("created_at"));
        let tags: Vec<String> = v
            .get("tags")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(|t| t.as_str().map(str::to_string)).collect())
            .unwrap_or_default();
        let conf = v.get("confidence").and_then(Value::as_f64);
        let et = as_str(&v, &["embedding_text"]).map(clip_et);
        let content = as_str(&v, &["content", "text", "memory"]);
        if let (Some(s), Some(r), Some(o)) = (
            as_str(&v, &["subject"]),
            as_str(&v, &["relation"]),
            as_str(&v, &["object"]),
        ) {
            let mut f = Fact::new(s, r, o);
            f.common.namespace = Some(ns.to_string());
            f.common.source_type = Some("import".to_string());
            f.common.created_at = created;
            f.common.tags = tags;
            f.common.user_id = as_str(&v, &["user_id"]).map(str::to_string);
            if let Some(c) = conf {
                f.common.confidence = c;
            }
            if let Some(c) = content {
                f.common.context = Some(json!({ "content": c }));
                f.common.embedding_text = Some(clip_et(&format!("{s} {r} {o} {c}")));
            }
            if let Some(et) = et {
                f.common.embedding_text = Some(et);
            }
            add_dedup(m, &f, &mut rep)?;
        } else if let Some(c) = content.filter(|c| !c.trim().is_empty()) {
            let mut e = Event::new(c);
            e.common.namespace = Some(ns.to_string());
            e.common.source_type = Some("import".to_string());
            e.common.created_at = created;
            e.common.tags = tags;
            e.common.user_id = as_str(&v, &["user_id"]).map(str::to_string);
            e.session_id = as_str(&v, &["session_id"]).map(str::to_string);
            if let Some(et) = et {
                e.common.embedding_text = Some(et);
            }
            add_dedup(m, &e, &mut rep)?;
        } else {
            rep.skipped += 1;
            rep.note(format!(
                "line {}: needs subject+relation+object or content",
                lineno + 1
            ));
        }
    }
    Ok(rep)
}

#[cfg(test)]
mod tests {
    use super::*;

    // The parser's own cases moved with it to `areev_core::time` — this
    // crate re-exports one implementation rather than keeping a second.
    #[test]
    fn parse_ts_handles_seconds_and_ms() {
        assert_eq!(parse_ts(Some(&json!(1_704_067_200))), Some(1_704_067_200_000));
        assert_eq!(parse_ts(Some(&json!(1_704_067_200_000i64))), Some(1_704_067_200_000));
        assert_eq!(parse_ts(Some(&json!("2024-01-01T00:00:00Z"))), Some(1_704_067_200_000));
        assert_eq!(parse_ts(None), None);
    }

    #[test]
    fn frontmatter_variants() {
        let (t, p, tags, _) = parse_frontmatter(
            "---\ntitle: Coffee Brewing\npermalink: notes/coffee\ntags:\n- brewing\n- espresso\n---\nbody",
        );
        assert_eq!(t.as_deref(), Some("Coffee Brewing"));
        assert_eq!(p.as_deref(), Some("notes/coffee"));
        assert_eq!(tags, vec!["brewing", "espresso"]);
        let (_, _, tags, _) = parse_frontmatter("---\ntags: [a, b]\n---\nbody");
        assert_eq!(tags, vec!["a", "b"]);
        let (t, p, _, _) = parse_frontmatter("no frontmatter at all");
        assert!(t.is_none() && p.is_none());
    }

    #[test]
    fn clip_et_respects_char_boundaries() {
        let s = "é".repeat(5000); // 2 bytes each
        let clipped = clip_et(&s);
        assert!(clipped.len() <= ET_MAX_BYTES);
        assert!(clipped.chars().all(|c| c == 'é'));
    }
}
