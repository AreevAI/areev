//! Deferred FTS indexing (`defer_text_index` / `rebuild_text_index`) and the
//! `embedding_text` projection on the write path. These are the bulk-import
//! primitives: with the index deferred, writes skip Turso's per-transaction
//! FTS bookkeeping (~150ms/txn); rebuild re-creates the index over all
//! existing rows and backfills `text` for rows written while indexing was off.

use areev_core::types::{Fact, Grain};
use areev_store::{Areev, AreevOptions};

fn opts() -> AreevOptions {
    AreevOptions::default() // index_text: true
}

fn fact(ns: &str, s: &str, r: &str, o: &str, ts: i64) -> Fact {
    let mut f = Fact::new(s, r, o).created_at(ts);
    f.common.namespace = Some(ns.to_string());
    f
}

#[test]
fn defer_then_rebuild_keeps_bm25_working() {
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("bulk.db");
    let mut m = Areev::open_with(path.to_str().unwrap(), opts()).unwrap();

    assert!(m.defer_text_index().unwrap(), "first defer drops the index");
    assert!(!m.defer_text_index().unwrap(), "second defer is a no-op");

    for i in 0..50 {
        m.add(&fact("main", &format!("user:{i}"), "prefers", "espresso", 1_700_000_000_000 + i))
            .unwrap();
    }
    let backfilled = m.rebuild_text_index().unwrap();
    // text column was populated inline (index_text stays on during defer)
    assert_eq!(backfilled, 0, "no NULL text rows to backfill");

    let hits = m.search_text("main", "espresso", 64).unwrap();
    assert_eq!(hits.len(), 50, "all bulk-loaded rows are BM25-searchable");
}

#[test]
fn rebuild_backfills_rows_written_with_indexing_off() {
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("flip.db");
    // voice/edge profile: no text indexing at write time
    let mut m = Areev::open_with(
        path.to_str().unwrap(),
        AreevOptions { index_text: false, ..AreevOptions::default() },
    )
    .unwrap();
    for i in 0..20 {
        m.add(&fact("main", &format!("svc:{i}"), "uses", "postgres", 1_700_000_000_000 + i))
            .unwrap();
    }
    // rebuilding while the file declares indexing off is an error, not a no-op
    assert!(m.rebuild_text_index().is_err());
    drop(m);

    // flip the declaration on, then rebuild: rows become searchable
    let mut m = Areev::open_with(path.to_str().unwrap(), opts()).unwrap();
    let backfilled = m.rebuild_text_index().unwrap();
    assert_eq!(backfilled, 20, "NULL text rows derived from their blobs");
    let hits = m.search_text("main", "postgres", 64).unwrap();
    assert_eq!(hits.len(), 20);
}

#[test]
fn crash_between_defer_and_rebuild_self_heals_on_open() {
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("crash.db");
    {
        let mut m = Areev::open_with(path.to_str().unwrap(), opts()).unwrap();
        m.defer_text_index().unwrap();
        m.add(&fact("main", "user:a", "prefers", "matcha", 1_700_000_000_000)).unwrap();
        // "crash": drop without rebuild_text_index
    }
    // next open re-creates the index (CREATE INDEX IF NOT EXISTS) and Turso
    // indexes the existing rows at creation
    let mut m = Areev::open(path.to_str().unwrap()).unwrap();
    let hits = m.search_text("main", "matcha", 16).unwrap();
    assert_eq!(hits.len(), 1);
}

#[test]
fn embedding_text_override_is_indexed_for_bm25() {
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("et.db");
    let mut m = Areev::open_with(path.to_str().unwrap(), opts()).unwrap();

    // A fact whose object is an opaque digest, with the prose in
    // embedding_text (the import-pipeline shape): BM25 must find the prose.
    let mut f = fact("main", "mem0/abc123", "mem0_memory", "v:9f2c11", 1_700_000_000_000);
    f.common.embedding_text =
        Some("Vegetarian since 2019, allergic to shellfish".to_string());
    m.add(&f).unwrap();

    let hits = m.search_text("main", "shellfish", 16).unwrap();
    assert_eq!(hits.len(), 1, "prose in embedding_text is BM25-searchable");
    // and the digest-shaped object is NOT what got indexed
    assert!(m.search_text("main", "9f2c11", 16).unwrap().is_empty());
}

#[test]
fn memory_tool_file_bodies_are_searchable() {
    use areev_store::memory_tool::MemoryTool;
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("memtool.db");
    let mut m = Areev::open_with(path.to_str().unwrap(), opts()).unwrap();
    {
        let mut t = MemoryTool::new(&mut m, "main");
        t.execute(&serde_json::json!({
            "command": "create",
            "path": "/memories/preferences.md",
            "file_text": "Prefers dark roast coffee and window seats."
        }))
        .unwrap();
    }
    let hits = m.search_text("main", "roast coffee", 16).unwrap();
    assert_eq!(hits.len(), 1, "memory-tool file body reaches the BM25 leg");
}

#[test]
fn text_mention_erasure_needs_a_complete_index() {
    use areev_store::ErasureOptions;
    let dir = tempfile::TempDir::new().unwrap();

    // Indexing off: requesting text mentions is an error — and it must fire
    // even when the namespace does not exist yet (a mistyped ns must not
    // turn the capability error into a silent zero-count success).
    let path = dir.path().join("noidx.db");
    let mut m = Areev::open_with(
        path.to_str().unwrap(),
        AreevOptions { index_text: false, ..AreevOptions::default() },
    )
    .unwrap();
    let mentions = ErasureOptions { text_mentions: true };
    assert!(m.forget_subject_with("no-such-ns", "pat", mentions).is_err());
    m.add(&fact("main", "pat", "condition", "stable", 1_700_000_000_000)).unwrap();
    assert!(m.forget_subject_with("main", "pat", mentions).is_err());
    // Without the opt-in the structured erasure still works.
    assert_eq!(m.forget_subject("main", "pat").unwrap().grains_erased, 1);
    drop(m);

    // Index on but DEFERRED: grains written now have no postings, so a
    // text-mention erasure would silently miss them — refuse until rebuilt.
    let path = dir.path().join("deferred.db");
    let mut m = Areev::open_with(path.to_str().unwrap(), opts()).unwrap();
    m.defer_text_index().unwrap();
    m.add(&fact("main", "kim", "note", "spoke about pat", 1_700_000_000_000)).unwrap();
    assert!(m.forget_subject_with("main", "pat", mentions).is_err());
    m.rebuild_text_index().unwrap();
    let rep = m.forget_subject_with("main", "pat", mentions).unwrap();
    assert_eq!(rep.grains_erased, 1, "after rebuild the mention is reachable again");
}
