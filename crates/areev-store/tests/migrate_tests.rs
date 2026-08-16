//! Migration importers, driven with fixture payloads shaped like each
//! source's real export (see docs/migrate.md for how the payloads are
//! produced). What these pin down:
//! - mem0 history replays into real supersession chains with original
//!   timestamps (the fidelity edge over the official mem0→Zep/Supermemory
//!   guides, which keep only final state);
//! - re-running an import is a no-op (skipped, not duplicated, not an error);
//! - note-shaped sources land as `memory_file` chains the Anthropic
//!   memory-tool backend can immediately view/edit;
//! - imported prose is BM25-searchable via `embedding_text`;
//! - Zep's bi-temporal `valid_at`/`invalid_at` maps onto world-time validity.

use areev_store::migrate::*;
use areev_store::{Areev, AreevOptions};
use serde_json::json;

fn open(dir: &tempfile::TempDir, name: &str) -> Areev {
    let path = dir.path().join(name);
    Areev::open_with(path.to_str().unwrap(), AreevOptions::default()).unwrap()
}

#[test]
fn mem0_history_becomes_supersession_chain() {
    let dir = tempfile::TempDir::new().unwrap();
    let mut m = open(&dir, "mem0.db");
    let history = json!([
        { "memory_id": "m-1", "event": "ADD", "new_memory": "Works at Acme",
          "created_at": "2024-03-01T10:00:00Z" },
        { "memory_id": "m-1", "event": "UPDATE", "new_memory": "Works at Initech",
          "created_at": "2024-06-01T10:00:00Z" },
        { "memory_id": "m-2", "event": "ADD", "new_memory": "Allergic to peanuts",
          "created_at": "2024-04-01T09:00:00Z" }
    ]);
    let rep = migrate_mem0(&mut m, "main", None, Some(&history)).unwrap();
    assert_eq!((rep.added, rep.superseded, rep.forgotten), (2, 1, 0));

    // current view has exactly the updated value, history keeps both
    let head = m.latest("main", "mem0/m-1", "mem0_memory").unwrap().unwrap();
    let content = head.fields["context"]["content"].as_str().unwrap();
    assert_eq!(content, "Works at Initech");
    let versions = m.history("main", "mem0/m-1", "mem0_memory").unwrap();
    assert_eq!(versions.len(), 2);
    // original timestamps preserved (2024-03-01T10:00:00Z)
    assert!(versions.iter().any(|v| v.created_at == 1_709_287_200_000));

    // the stale value does not co-rank: recall shows one current grain
    assert_eq!(m.recall("main", "mem0/m-1", None, 16).unwrap().len(), 1);

    // imported prose is BM25-searchable
    assert_eq!(m.search_text("main", "Initech", 16).unwrap().len(), 1);
    assert!(m.search_text("main", "Acme", 16).unwrap().is_empty(), "stale text not current");
}

#[test]
fn mem0_delete_event_forgets_the_chain() {
    let dir = tempfile::TempDir::new().unwrap();
    let mut m = open(&dir, "mem0del.db");
    let history = json!([
        { "memory_id": "m-9", "event": "ADD", "new_memory": "Lives in Berlin",
          "created_at": "2024-01-01T00:00:00Z" },
        { "memory_id": "m-9", "event": "DELETE",
          "created_at": "2024-02-01T00:00:00Z" }
    ]);
    let rep = migrate_mem0(&mut m, "main", None, Some(&history)).unwrap();
    assert_eq!((rep.added, rep.forgotten), (1, 1));
    assert!(m.latest("main", "mem0/m-9", "mem0_memory").unwrap().is_none());
}

// Regression (#A4F5): re-importing a delete-terminated chain must be idempotent
// — its grains are forgotten (history() is empty), so the history-based guard
// alone would re-add then re-delete, resurrecting the memory if interrupted.
#[test]
fn mem0_delete_terminated_rerun_is_idempotent() {
    let dir = tempfile::TempDir::new().unwrap();
    let mut m = open(&dir, "mem0delrerun.db");
    let history = json!([
        { "memory_id": "m-9", "event": "ADD", "new_memory": "Lives in Berlin",
          "created_at": "2024-01-01T00:00:00Z" },
        { "memory_id": "m-9", "event": "DELETE", "created_at": "2024-02-01T00:00:00Z" }
    ]);
    let r1 = migrate_mem0(&mut m, "main", None, Some(&history)).unwrap();
    assert_eq!((r1.added, r1.forgotten), (1, 1));

    // Re-run the SAME history: recognized as already imported, not re-processed.
    let r2 = migrate_mem0(&mut m, "main", None, Some(&history)).unwrap();
    assert_eq!(r2.added, 0, "must not re-add a delete-terminated chain");
    assert_eq!(r2.forgotten, 0, "and must not re-forget");
    assert!(r2.skipped >= 1, "it is skipped");
    assert!(m.latest("main", "mem0/m-9", "mem0_memory").unwrap().is_none(), "stays deleted");
}

#[test]
fn mem0_export_only_and_rerun_is_idempotent() {
    let dir = tempfile::TempDir::new().unwrap();
    let mut m = open(&dir, "mem0exp.db");
    let export = json!({ "results": [
        { "id": "a1", "memory": "Prefers vegetarian restaurants",
          "user_id": "user-7", "categories": ["food"],
          "metadata": { "src": "chat" },
          "created_at": "2024-07-26T10:29:11.982509-07:00" }
    ]});
    let rep = migrate_mem0(&mut m, "main", Some(&export), None).unwrap();
    assert_eq!(rep.added, 1);

    let head = m.latest("main", "mem0/a1", "mem0_memory").unwrap().unwrap();
    assert_eq!(head.fields["context"]["import"]["source"], json!("mem0"));
    assert_eq!(head.fields["context"]["import"]["user_id"], json!("user-7"));

    // second run: same payload → skipped, nothing duplicated, no error
    let rep2 = migrate_mem0(&mut m, "main", Some(&export), None).unwrap();
    assert_eq!((rep2.added, rep2.skipped), (0, 1));
    assert_eq!(m.recall("main", "mem0/a1", None, 16).unwrap().len(), 1);
}

#[test]
fn mem0_history_rerun_skips_existing_chains() {
    let dir = tempfile::TempDir::new().unwrap();
    let mut m = open(&dir, "mem0rerun.db");
    let history = json!([
        { "memory_id": "m-1", "event": "ADD", "new_memory": "v1",
          "created_at": "2024-01-01T00:00:00Z" },
        { "memory_id": "m-1", "event": "UPDATE", "new_memory": "v2",
          "created_at": "2024-01-02T00:00:00Z" }
    ]);
    migrate_mem0(&mut m, "main", None, Some(&history)).unwrap();
    let rep2 = migrate_mem0(&mut m, "main", None, Some(&history)).unwrap();
    assert_eq!((rep2.added, rep2.superseded, rep2.skipped), (0, 0, 2));
    assert_eq!(m.history("main", "mem0/m-1", "mem0_memory").unwrap().len(), 2);
}

#[test]
fn langgraph_store_dump_imports() {
    let dir = tempfile::TempDir::new().unwrap();
    let mut m = open(&dir, "lg.db");
    let jsonl = concat!(
        r#"{"prefix": ["memories", "user-1"], "key": "diet", "value": {"content": "vegan since 2020"}, "created_at": "2025-05-01T12:00:00Z"}"#, "\n",
        r#"{"prefix": "memories.user-1", "key": "city", "value": "Lisbon", "updated_at": 1714564800}"#, "\n",
        "not json\n",
    );
    let rep = migrate_langgraph(&mut m, "main", jsonl).unwrap();
    assert_eq!((rep.added, rep.skipped), (2, 1));
    let head = m
        .latest("main", "langgraph/memories/user-1/diet", "langgraph_item")
        .unwrap()
        .unwrap();
    assert_eq!(head.fields["context"]["content"], json!("vegan since 2020"));
    assert_eq!(m.search_text("main", "vegan", 16).unwrap().len(), 1);
}

#[test]
fn basic_memory_note_becomes_live_memory_file() {
    use areev_store::memory_tool::MemoryTool;
    let dir = tempfile::TempDir::new().unwrap();
    let mut m = open(&dir, "bm.db");
    let note = "---\ntitle: Coffee Brewing\npermalink: notes/coffee\ntags:\n- brewing\n---\n# Coffee\nUse a burr grinder; 1:16 ratio.\n";
    let mut rep = MigrateReport::default();
    migrate_basic_memory_note(&mut m, "main", "notes/coffee.md", note, Some(1_700_000_000_000), &mut rep).unwrap();
    assert_eq!(rep.added, 1);

    // re-import skips (never clobbers a chain the agent may have edited)
    migrate_basic_memory_note(&mut m, "main", "notes/coffee.md", note, None, &mut rep).unwrap();
    assert_eq!(rep.skipped, 1);

    // the imported note is a live file for the Anthropic memory tool
    let mut t = MemoryTool::new(&mut m, "main");
    let listing = t.execute(&json!({ "command": "view", "path": "/memories" })).unwrap();
    assert!(listing.contains("/memories/notes/coffee"), "listing: {listing}");
    let body = t
        .execute(&json!({ "command": "view", "path": "/memories/notes/coffee" }))
        .unwrap();
    assert!(body.contains("burr grinder"));

    // and its prose is searchable
    assert_eq!(m.search_text("main", "burr grinder", 16).unwrap().len(), 1);
}

#[test]
fn letta_af_blocks_and_messages() {
    let dir = tempfile::TempDir::new().unwrap();
    let mut m = open(&dir, "letta.db");
    let af = json!({
        "agents": [{
            "name": "sam",
            "created_at": "2025-02-01T00:00:00Z",
            "core_memory": [
                { "label": "human", "value": "Name is Dana, prefers direct answers" },
                { "label": "persona", "value": "Helpful project assistant" }
            ],
            "messages": [
                { "role": "user", "content": "Remember the launch is May 5", "created_at": "2025-02-02T00:00:00Z" },
                { "role": "assistant", "content": [{ "type": "text", "text": "Noted: launch May 5." }] },
                { "role": "system", "content": "internal prompt" }
            ]
        }]
    });
    let rep = migrate_letta(&mut m, "main", &af).unwrap();
    assert_eq!(rep.added, 4, "2 blocks + 2 conversational messages (system skipped)");
    let head = m
        .latest("main", "/memories/letta/sam/human", "memory_file")
        .unwrap()
        .unwrap();
    assert_eq!(
        head.fields["context"]["content"],
        json!("Name is Dana, prefers direct answers")
    );
    assert_eq!(m.search_text("main", "launch", 16).unwrap().len(), 2);
}

#[test]
fn letta_archival_jsonl() {
    let dir = tempfile::TempDir::new().unwrap();
    let mut m = open(&dir, "lettaarch.db");
    let jsonl = concat!(
        r#"{"id": "p1", "text": "Dana ran the Lisbon half marathon in 2024", "created_at": "2024-10-13T09:00:00Z", "tags": ["running"]}"#, "\n",
        r#"{"id": "p2", "text": "Dana is learning Portuguese", "created_at": "2025-01-05T08:00:00Z"}"#, "\n",
    );
    let rep = migrate_letta_archival(&mut m, "main", jsonl).unwrap();
    assert_eq!(rep.added, 2);
    assert_eq!(m.search_text("main", "marathon", 16).unwrap().len(), 1);
    // rerun: both skipped via content address
    let rep2 = migrate_letta_archival(&mut m, "main", jsonl).unwrap();
    assert_eq!((rep2.added, rep2.skipped), (0, 2));
}

#[test]
fn zep_edges_carry_bitemporal_validity() {
    let dir = tempfile::TempDir::new().unwrap();
    let mut m = open(&dir, "zep.db");
    let payload = json!({
        "edges": [
            { "uuid": "e1", "name": "WORKS_AT", "fact": "Dana works at Acme",
              "source_node_uuid": "n-dana",
              "valid_at": "2022-01-01T00:00:00Z", "invalid_at": "2024-01-01T00:00:00Z",
              "created_at": "2022-01-02T00:00:00Z" },
            { "uuid": "e2", "name": "WORKS_AT", "fact": "Dana works at Initech",
              "source_node_uuid": "n-dana",
              "valid_at": "2024-01-01T00:00:00Z",
              "created_at": "2024-01-02T00:00:00Z" }
        ],
        "episodes": [
            { "uuid": "ep1", "content": "Dana: I started at Initech this week!",
              "role": "user", "thread_id": "t-42", "created_at": "2024-01-03T00:00:00Z" }
        ]
    });
    let rep = migrate_zep(&mut m, "main", &payload).unwrap();
    assert_eq!(rep.added, 3);

    // both facts import; the invalidated one carries valid_to
    let grains = m.recall("main", "zep/n-dana", None, 16).unwrap();
    assert_eq!(grains.len(), 2);
    let acme = grains
        .iter()
        .find(|g| g.fields["context"]["content"].as_str().unwrap().contains("Acme"))
        .unwrap();
    assert_eq!(acme.fields["valid_to"], json!(1_704_067_200_000i64));
    let initech = grains
        .iter()
        .find(|g| g.fields["context"]["content"].as_str().unwrap().contains("Initech"))
        .unwrap();
    assert!(!initech.fields.contains_key("valid_to") || initech.fields["valid_to"].is_null());

    assert_eq!(m.search_text("main", "Initech", 16).unwrap().len(), 2);
}

#[test]
fn generic_jsonl_facts_and_events() {
    let dir = tempfile::TempDir::new().unwrap();
    let mut m = open(&dir, "gen.db");
    let jsonl = concat!(
        r#"{"subject": "user:dana", "relation": "prefers", "object": "window seats", "created_at": "2024-05-05T00:00:00Z", "confidence": 0.7}"#, "\n",
        r#"{"content": "Dana asked about gluten-free options at the offsite", "session_id": "offsite-1", "created_at": 1714867200}"#, "\n",
        r#"{"unmappable": true}"#, "\n",
    );
    let rep = migrate_jsonl(&mut m, "main", jsonl).unwrap();
    assert_eq!((rep.added, rep.skipped), (2, 1));
    assert!(m.latest("main", "user:dana", "prefers").unwrap().is_some());
    assert_eq!(m.search_text("main", "gluten", 16).unwrap().len(), 1);
}
