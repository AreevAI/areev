//! Ingress mode (docs/anonymization-proposal.md P2): value-derived
//! pseudonyms at the write chokepoints, the D8 content-address gate,
//! REQ-ANON-7 erasure/DSAR aliasing, and the encrypted-memory requirement.

use areev_store::{Capture, FactDraft, Areev};
use tempfile::TempDir;

const KEY: [u8; 32] = [7u8; 32];

fn open_encrypted(dir: &TempDir, name: &str) -> Areev {
    Areev::open_encrypted(dir.path().join(name).to_str().unwrap(), KEY).unwrap()
}

const INGRESS: &str = r#"{"mode": "ingress"}"#;

#[test]
fn ingress_and_memory_scope_need_an_encrypted_memory() {
    let dir = TempDir::new().unwrap();
    let mut plain = Areev::open(dir.path().join("plain.db").to_str().unwrap()).unwrap();
    for policy in [
        INGRESS,
        r#"{"mode": "both"}"#,
        r#"{"mode": "egress", "scope": "memory"}"#,
    ] {
        let err = plain.set_anon_policy("caller", policy).unwrap_err().to_string();
        assert!(
            err.starts_with("VAL-E001") && err.contains("encrypted"),
            "want encrypted-memory refusal, got: {err}"
        );
    }
    // The same declarations are accepted on an encrypted memory.
    let mut enc = open_encrypted(&dir, "enc.db");
    enc.set_anon_policy("caller", INGRESS).unwrap();
    enc.set_anon_policy("other", r#"{"mode": "egress", "scope": "memory"}"#).unwrap();
}

#[test]
fn ingress_transforms_before_the_content_address_commits() {
    // The D8 gate: the stored blob (and therefore the hash) holds the
    // transformed text; the same raw text always maps to the same grain.
    let dir = TempDir::new().unwrap();
    let mut m = open_encrypted(&dir, "a.db");
    m.set_anon_policy("caller", INGRESS).unwrap();

    let raw = "my user name is john, and pin number is 1462";
    let h1 = m.capture("caller", raw, &Capture::default()).unwrap();
    let g = m.get(&h1).unwrap(); // mode=ingress → reads are raw: what you see IS what is stored
    let content = g.fields["content"].as_str().unwrap();
    assert!(!content.contains("1462"), "stored blob leaked the pin: {content}");
    assert!(content.contains("[PIN_"), "expected value-derived token: {content}");

    // Determinism survives: identical raw text → identical tokens. (Events
    // carry ms timestamps, so hash equality is not the invariant here —
    // token equality is what keeps value-level dedup and add_if_novel
    // working for timestamp-free values.)
    let h2 = m.capture("caller", raw, &Capture::default()).unwrap();
    let g2 = m.get(&h2).unwrap();
    assert_eq!(
        g.fields["content"], g2.fields["content"],
        "value-derived tokens must be deterministic"
    );
}

#[test]
fn remember_extractor_sees_transformed_content_and_drafts_are_covered() {
    let dir = TempDir::new().unwrap();
    let mut m = open_encrypted(&dir, "a.db");
    m.set_anon_policy("caller", INGRESS).unwrap();

    let seen = std::cell::RefCell::new(String::new());
    let extractor = |content: &str| -> Vec<FactDraft> {
        *seen.borrow_mut() = content.to_string();
        vec![FactDraft {
            subject: "caller:john".into(),
            relation: "prefers".into(),
            object: "call me at +1 415 555 0142".into(),
            confidence: 0.9,
        }]
    };
    let out = m.remember("caller", "john's pin number is 1462", "cli", Some(&extractor)).unwrap();

    let seen = seen.borrow();
    assert!(!seen.contains("1462"), "the extractor saw raw content: {seen}");

    // Host-supplied draft fields are transformed too: subject as a person
    // identity, object through detection.
    let fact = m.get(&out.facts[0]).unwrap();
    let subject = fact.fields["subject"].as_str().unwrap();
    let object = fact.fields["object"].as_str().unwrap();
    assert!(subject.starts_with("[PERSON_"), "draft subject leaked: {subject}");
    assert!(!object.contains("415 555"), "draft object leaked: {object}");
}

#[test]
fn req_anon_7_erasure_and_dsar_select_under_both_names() {
    let dir = TempDir::new().unwrap();
    let mut m = open_encrypted(&dir, "a.db");
    m.set_anon_policy("caller", INGRESS).unwrap();

    // Store through the ingress boundary: the subject at rest is a pseudonym.
    let drafts = [FactDraft {
        subject: "caller:john".into(),
        relation: "prefers".into(),
        object: "tea".into(),
        confidence: 0.9,
    }];
    let hash = Hash_of_first(&mut m, &drafts);
    let stored = m.get(&hash).unwrap();
    let alias = stored.fields["subject"].as_str().unwrap().to_string();
    assert!(alias.starts_with("[PERSON_"), "precondition: pseudonymized at rest");

    // The DSAR by the REAL name discloses the pseudonymized grain...
    let report = m.subject_report("caller", "caller:john").unwrap();
    assert!(
        report.grains.iter().any(|g| g.hash == hash),
        "REPORT SUBJECT must reach the pseudonymized grain"
    );

    // ...and erasure by the REAL name removes it.
    let erased = m.forget_subject("caller", "caller:john").unwrap();
    assert!(erased.grains_erased >= 1, "{erased:?}");
    assert!(m.get(&hash).is_err(), "pseudonymized-at-rest must never mean erasure-proof");
}

#[allow(non_snake_case)]
fn Hash_of_first(m: &mut Areev, drafts: &[FactDraft]) -> areev_core::error::Hash {
    let ev = m.capture("caller", "seed", &Capture::default()).unwrap();
    let facts = m
        .attach_facts("caller", &ev, drafts, &areev_store::FactAttribution::default())
        .unwrap();
    facts[0]
}

#[test]
fn memory_scope_tokens_are_stable_across_handles() {
    let dir = TempDir::new().unwrap();
    let token_a;
    {
        let mut m = open_encrypted(&dir, "a.db");
        m.set_anon_policy("caller", r#"{"mode": "egress", "scope": "memory"}"#).unwrap();
        let mut f = areev_core::types::Fact::new("caller:john", "prefers", "tea");
        f.common.namespace = Some("caller".to_string());
        m.add(&f).unwrap();
        token_a = m.recall("caller", "caller:john", None, 4).unwrap()[0].fields["subject"]
            .as_str()
            .unwrap()
            .to_string();
        assert!(token_a.starts_with("[PERSON_"));
    }
    // A fresh handle (fresh process, same file+key) derives the same token —
    // that is what makes yesterday's response rehydratable tomorrow.
    let mut m = open_encrypted(&dir, "a.db");
    let token_b = m.recall("caller", "caller:john", None, 4).unwrap()[0].fields["subject"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(token_a, token_b, "memory scope must be stable across handles");
}

#[test]
fn memory_tool_bodies_pass_the_ingress_boundary() {
    let dir = TempDir::new().unwrap();
    let mut m = open_encrypted(&dir, "a.db");
    m.set_anon_policy("agent", INGRESS).unwrap();
    let mut tool = areev_store::memory_tool::MemoryTool::new(&mut m, "agent");
    tool.execute(&serde_json::json!({
        "command": "create",
        "path": "/memories/notes.md",
        "file_text": "call john at +1 415 555 0142"
    }))
    .unwrap();
    let view = tool
        .execute(&serde_json::json!({"command": "view", "path": "/memories/notes.md"}))
        .unwrap();
    assert!(!view.contains("415 555"), "memory-file body leaked: {view}");
    assert!(view.contains("[PHONE_"), "expected token in body: {view}");
}
