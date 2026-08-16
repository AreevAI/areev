//! LR-13 memory-tool adapter (cookbook flows) + remember() seam tests.

use areev_store::memory_tool::MemoryTool;
use areev_store::{FactDraft, Areev};
use serde_json::json;
use tempfile::TempDir;

fn open_mem() -> (Areev, TempDir) {
    let dir = TempDir::new().unwrap();
    let m = Areev::open(dir.path().join("m.db").to_str().unwrap()).unwrap();
    (m, dir)
}

#[test]
fn memory_tool_cookbook_flow() {
    let (mut m, _d) = open_mem();
    let mut t = MemoryTool::new(&mut m, "agent");

    // create → view (numbered lines, cookbook format)
    t.execute(&json!({"command": "create", "path": "/memories/preferences.md",
        "file_text": "# Preferences\n- indent: 2 spaces\n- tests first"})).unwrap();
    let v = t.execute(&json!({"command": "view", "path": "/memories/preferences.md"})).unwrap();
    assert!(v.contains("   1: # Preferences"), "{v}");
    assert!(v.contains("   3: - tests first"), "{v}");

    // directory listing
    t.execute(&json!({"command": "create", "path": "/memories/projects.md", "file_text": "areev"})).unwrap();
    let dir = t.execute(&json!({"command": "view", "path": "/memories"})).unwrap();
    assert!(dir.contains("/memories/preferences.md") && dir.contains("/memories/projects.md"), "{dir}");

    // str_replace requires uniqueness
    t.execute(&json!({"command": "str_replace", "path": "/memories/preferences.md",
        "old_str": "2 spaces", "new_str": "4 spaces"})).unwrap();
    let v = t.execute(&json!({"command": "view", "path": "/memories/preferences.md"})).unwrap();
    assert!(v.contains("4 spaces"), "{v}");
    let err = t.execute(&json!({"command": "str_replace", "path": "/memories/preferences.md",
        "old_str": "nonexistent", "new_str": "x"}));
    assert!(err.is_err());

    // insert
    t.execute(&json!({"command": "insert", "path": "/memories/preferences.md",
        "insert_line": 1, "insert_text": "- never push to main"})).unwrap();
    let v = t.execute(&json!({"command": "view", "path": "/memories/preferences.md"})).unwrap();
    assert!(v.contains("   2: - never push to main"), "{v}");

    // every edit was a supersession — history is real
    let hist = m.history("agent", "/memories/preferences.md", "memory_file").unwrap();
    assert_eq!(hist.len(), 3, "create + str_replace + insert = 3 versions");

    // rename keeps content, erases old chain, links provenance
    let mut t = MemoryTool::new(&mut m, "agent");
    t.execute(&json!({"command": "rename", "old_path": "/memories/projects.md",
        "new_path": "/memories/work.md"})).unwrap();
    let v = t.execute(&json!({"command": "view", "path": "/memories/work.md"})).unwrap();
    assert!(v.contains("areev"), "{v}");
    assert!(t.execute(&json!({"command": "view", "path": "/memories/projects.md"})).is_err());

    // delete erases the chain
    t.execute(&json!({"command": "delete", "path": "/memories/work.md"})).unwrap();
    assert!(t.execute(&json!({"command": "view", "path": "/memories/work.md"})).is_err());

    // path traversal is rejected
    assert!(t.execute(&json!({"command": "view", "path": "/etc/passwd"})).is_err());
    assert!(t.execute(&json!({"command": "view", "path": "/memories/../etc"})).is_err());
}

#[test]
fn memory_tool_persists_across_reopen() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("m.db");
    let path = path.to_str().unwrap();
    {
        let mut m = Areev::open(path).unwrap();
        let mut t = MemoryTool::new(&mut m, "agent");
        t.execute(&json!({"command": "create", "path": "/memories/notes.md", "file_text": "hello"})).unwrap();
    }
    let mut m = Areev::open(path).unwrap();
    let mut t = MemoryTool::new(&mut m, "agent");
    let v = t.execute(&json!({"command": "view", "path": "/memories/notes.md"})).unwrap();
    assert!(v.contains("hello"));
}

#[test]
fn remember_extracts_facts_with_provenance() {
    let (mut m, _d) = open_mem();
    let extractor = |content: &str| -> Vec<FactDraft> {
        assert!(content.contains("Berlin"));
        vec![
            FactDraft { subject: "alice".into(), relation: "lives_in".into(), object: "Berlin".into(), confidence: 0.9 },
            FactDraft { subject: "alice".into(), relation: "prefers".into(), object: "morning calls".into(), confidence: 0.7 },
        ]
    };
    let res = m
        .remember("caller", "Alice said she moved to Berlin and prefers morning calls", "extractor:test", Some(&extractor))
        .unwrap();
    assert_eq!(res.facts.len(), 2);

    // derived facts are recallable and provenance-linked to the observation
    let facts = m.recall("caller", "alice", None, 16).unwrap();
    assert_eq!(facts.len(), 2);
    for f in &facts {
        assert_eq!(f.get_str("derived_from"), Some(res.event.to_hex().as_str()));
        assert_eq!(f.get_str("source_type"), Some("derived"));
    }
    // the Event grain holds the raw content in its native `content` field,
    // which is what the FTS/embedding projection indexes
    let obs = m.get(&res.event).unwrap();
    assert!(obs.fields["content"].as_str().unwrap().contains("morning calls"));
}

#[test]
fn remember_without_extractor_stores_the_event_only() {
    let (mut m, _d) = open_mem();
    let res = m.remember("caller", "raw note", "cli", None).unwrap();
    assert!(res.facts.is_empty());
    assert!(m.get(&res.event).is_ok());
}

// --- capture + attach_facts: the split seam for fallible extractors --------

fn draft(subject: &str, relation: &str, object: &str) -> FactDraft {
    FactDraft {
        subject: subject.into(),
        relation: relation.into(),
        object: object.into(),
        confidence: 0.9,
    }
}

fn observer(id: &str) -> areev_store::Capture<'_> {
    areev_store::Capture { observer: Some(id), ..Default::default() }
}

#[test]
fn capture_stores_the_raw_text_before_any_extraction() {
    let (mut m, _d) = open_mem();
    let obs = m
        .capture("caller", "Alice moved to Berlin", &observer("voice-agent"))
        .unwrap();
    let stored = m.get(&obs).unwrap();
    assert_eq!(stored.fields["content"], "Alice moved to Berlin");
    assert_eq!(stored.fields["observer"], "voice-agent");
    // Nothing else was written — an extraction that never happens costs
    // nothing but the facts.
    assert_eq!(m.recall("caller", "alice", None, 16).unwrap().len(), 0);
}

#[test]
fn captured_text_is_full_text_searchable() {
    // The Observation shape this replaced buried the text in `context.content`,
    // which `projected_text` does not index — remembered text was invisible to
    // search. An Event's native `content` field is indexed.
    let (mut m, _d) = open_mem();
    m.capture("caller", "the marmalade incident happened in Lisbon", &observer("cli"))
        .unwrap();
    let hits = m.search_text("caller", "marmalade", 8).unwrap();
    assert_eq!(hits.len(), 1, "remembered text must be searchable");
}

#[test]
fn capture_threads_a_turn_by_session_and_role() {
    let (mut m, _d) = open_mem();
    let meta = areev_store::Capture {
        observer: Some("voice-agent"),
        session_id: Some("sess-1"),
        role: Some("user"),
        run_id: Some("run-1"),
    };
    let h = m.capture("caller", "hello there", &meta).unwrap();
    let g = m.get(&h).unwrap();
    assert_eq!(g.fields["session_id"], "sess-1");
    assert_eq!(g.fields["role"], "user");
    // A captured turn is reachable through the run join, not just the thread
    // index — `run_id` had no write path outside Rust before this.
    assert_eq!(g.fields["run_id"], "run-1");
    assert_eq!(m.run_trace("caller", "run-1", 10).unwrap().len(), 1);
    // Reachable as part of its transcript, not just as a loose grain.
    let tail = m.thread_tail("caller", "sess-1", 8).unwrap();
    assert_eq!(tail.len(), 1);
}

#[test]
fn capture_drops_an_unrecognized_role_rather_than_failing() {
    let (mut m, _d) = open_mem();
    let meta = areev_store::Capture { role: Some("wizard"), ..Default::default() };
    let h = m.capture("caller", "abracadabra", &meta).unwrap();
    let g = m.get(&h).unwrap();
    assert!(g.fields.get("role").is_none_or(|v| v.is_null()));
    assert_eq!(g.fields["content"], "abracadabra");
}

#[test]
fn attach_facts_stamps_model_provenance_and_verification_status() {
    let (mut m, _d) = open_mem();
    let obs = m
        .capture("caller", "Alice moved to Berlin", &observer("voice-agent"))
        .unwrap();
    let attr = areev_store::FactAttribution {
        verification_status: Some("unverified"),
        extractor_model: Some("openai:gpt-4o-mini"),
    };
    let facts = m
        .attach_facts("caller", &obs, &[draft("alice", "lives_in", "Berlin")], &attr)
        .unwrap();
    assert_eq!(facts.len(), 1);

    let g = m.get(&facts[0]).unwrap();
    assert_eq!(g.fields["derived_from"], obs.to_hex());
    assert_eq!(g.fields["source_type"], "derived");
    assert_eq!(g.fields["verification_status"], "unverified");
    // An audit can answer "which model wrote this?" from the grain itself —
    // and it survives the .mg round trip, which a provenance_chain entry
    // would not (nothing in areev-core serializes that field).
    assert_eq!(g.fields["extractor_model"], "openai:gpt-4o-mini");
}

#[test]
fn attach_facts_with_no_attribution_matches_the_host_supplied_shape() {
    let (mut m, _d) = open_mem();
    let obs = m.capture("caller", "note", &observer("cli")).unwrap();
    let facts = m
        .attach_facts(
            "caller",
            &obs,
            &[draft("alice", "prefers", "tea")],
            &areev_store::FactAttribution::default(),
        )
        .unwrap();
    let g = m.get(&facts[0]).unwrap();
    // A host asserting its own facts is not relaying a model's claim: the
    // provenance link is there, the model attribution is not.
    assert_eq!(g.fields["derived_from"], obs.to_hex());
    assert!(g.fields.get("verification_status").is_none_or(|v| v.is_null()));
    assert!(g.fields.get("extractor_model").is_none_or(|v| v.is_null()));
}

// --- FactDraft::from_json_array — the shared CLI/py/js parse ----------------

#[test]
fn from_json_array_parses_and_defaults_confidence() {
    let drafts = FactDraft::from_json_array(
        r#"[{"subject":"john","relation":"prefers","object":"window seat","confidence":0.9},
            {"subject":" john ","relation":"diet","object":"vegetarian"}]"#,
    )
    .unwrap();
    assert_eq!(drafts.len(), 2);
    assert_eq!(drafts[0].confidence, 0.9);
    assert_eq!(drafts[1].confidence, areev_store::DRAFT_DEFAULT_CONFIDENCE);
    assert_eq!(drafts[1].subject, "john", "fields are trimmed");
}

#[test]
fn from_json_array_clamps_confidence() {
    let drafts = FactDraft::from_json_array(
        r#"[{"subject":"a","relation":"b","object":"c","confidence":7},
            {"subject":"a","relation":"b","object":"d","confidence":-2}]"#,
    )
    .unwrap();
    assert_eq!(drafts[0].confidence, 1.0);
    assert_eq!(drafts[1].confidence, 0.0);
}

#[test]
fn from_json_array_rejects_incomplete_triples() {
    // An incomplete row used to become a Fact with empty subject/relation/
    // object — a silently corrupt write. It is an error now.
    for bad in [
        r#"[{"subject":"john"}]"#,
        r#"[{"subject":"john","relation":"prefers"}]"#,
        r#"[{"subject":"","relation":"a","object":"b"}]"#,
        r#"[{"subject":"a","relation":"a","object":"   "}]"#,
    ] {
        let e = FactDraft::from_json_array(bad).unwrap_err().to_string();
        assert!(e.contains("facts[0]"), "{bad} → {e}");
        assert!(e.starts_with("VAL-E001"), "carries its error code: {e}");
    }
}

#[test]
fn from_json_array_rejects_malformed_json() {
    let e = FactDraft::from_json_array("not json").unwrap_err().to_string();
    assert!(e.starts_with("VAL-E001"), "{e}");
}
