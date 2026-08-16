//! P3: the sealed vault (persist + reseed + reveal), REQ-ANON-1 (erasure
//! reaches the vault), REQ-ANON-6 (TTL), and the pluggable detector seam
//! with its fail-closed contract (D6).

use areev_core::anon::{Detection, DetectorBackend};
use areev_core::error::Result as CoreResult;
use areev_store::Areev;
use tempfile::TempDir;

const KEY: [u8; 32] = [7u8; 32];

fn open_encrypted(dir: &TempDir, name: &str) -> Areev {
    Areev::open_encrypted(dir.path().join(name).to_str().unwrap(), KEY).unwrap()
}

fn fact(ns: &str, s: &str, r: &str, o: &str) -> areev_core::types::Fact {
    let mut f = areev_core::types::Fact::new(s, r, o);
    f.common.namespace = Some(ns.to_string());
    f
}

const VAULTED: &str = r#"{"mode": "egress", "scope": "session", "vault": true}"#;

#[test]
fn vault_needs_encryption_and_session_or_memory_scope() {
    let dir = TempDir::new().unwrap();
    let mut plain = Areev::open(dir.path().join("p.db").to_str().unwrap()).unwrap();
    let err = plain.set_anon_policy("caller", VAULTED).unwrap_err().to_string();
    assert!(err.contains("encrypted"), "{err}");

    let mut enc = open_encrypted(&dir, "e.db");
    let err = enc
        .set_anon_policy("caller", r#"{"mode": "egress", "vault": true}"#)
        .unwrap_err()
        .to_string();
    assert!(err.contains("session"), "context-scope vault must refuse: {err}");
    enc.set_anon_policy("caller", VAULTED).unwrap();
}

#[test]
fn vault_persists_reseeds_and_reveals_across_handles() {
    let dir = TempDir::new().unwrap();
    let token;
    {
        let mut m = open_encrypted(&dir, "a.db");
        m.set_anon_policy("caller", VAULTED).unwrap();
        m.add(&fact("caller", "caller:john", "prefers", "tea")).unwrap();
        token = m.recall("caller", "caller:john", None, 4).unwrap()[0].fields["subject"]
            .as_str()
            .unwrap()
            .to_string();

        // At rest the row is sealed: the raw meta value must not leak.
        let row = m.meta_get(&format!("vault:caller:{token}")).unwrap().expect("vault row");
        assert!(!row.contains("caller:john"), "vault row leaked plaintext: {row}");
    }
    // A fresh handle reseeds from the vault: the token CONTINUES (no
    // [PERSON_1] collision with a new person) and reveal works.
    let mut m = open_encrypted(&dir, "a.db");
    m.add(&fact("caller", "caller:mary", "prefers", "coffee")).unwrap();
    let mary = m.recall("caller", "caller:mary", None, 4).unwrap()[0].fields["subject"]
        .as_str()
        .unwrap()
        .to_string();
    assert_ne!(mary, token, "reseeded session must not reuse john's token for mary");
    assert_eq!(m.anon_reveal("caller", &token).unwrap().as_deref(), Some("caller:john"));
    assert_eq!(m.anon_reveal("caller", &mary).unwrap().as_deref(), Some("caller:mary"));
    assert_eq!(m.anon_reveal("caller", "[PERSON_99]").unwrap(), None);
}

#[test]
fn req_anon_1_erasure_reaches_the_vault_and_live_mappings() {
    let dir = TempDir::new().unwrap();
    let mut m = open_encrypted(&dir, "a.db");
    m.set_anon_policy("caller", VAULTED).unwrap();
    m.add(&fact("caller", "caller:john", "prefers", "tea")).unwrap();
    let token = m.recall("caller", "caller:john", None, 4).unwrap()[0].fields["subject"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(m.meta_get(&format!("vault:caller:{token}")).unwrap().is_some());

    m.forget_subject("caller", "caller:john").unwrap();

    assert_eq!(
        m.meta_get(&format!("vault:caller:{token}")).unwrap(),
        None,
        "the sealed vault row must die with the subject (REQ-ANON-1)"
    );
    assert_eq!(m.anon_reveal("caller", &token).unwrap(), None);
    assert!(
        !m.anon_mappings().unwrap().iter().any(|(_, _, map)| map.values().any(|v| v == "caller:john")),
        "no live mapping may keep re-identifying an erased subject"
    );
}

#[test]
fn req_anon_6_ttl_sweep_expires_vault_rows() {
    let dir = TempDir::new().unwrap();
    let mut m = open_encrypted(&dir, "a.db");
    m.set_anon_policy(
        "caller",
        r#"{"mode": "egress", "scope": "session", "vault": true, "vault_ttl_days": 1}"#,
    )
    .unwrap();
    m.add(&fact("caller", "caller:john", "prefers", "tea")).unwrap();
    let _ = m.recall("caller", "caller:john", None, 4).unwrap();
    assert!(!m.meta_scan("vault:caller:").unwrap().is_empty());

    // Far-future sweep: every row is past its TTL.
    let removed = m.sweep_anon_vault(i64::MAX).unwrap();
    assert!(removed.iter().any(|(ns, n)| ns == "caller" && *n >= 1), "{removed:?}");
    assert!(m.meta_scan("vault:caller:").unwrap().is_empty());
}

struct MockNer;
impl DetectorBackend for MockNer {
    fn kind(&self) -> &str {
        "ner"
    }
    fn id(&self) -> &str {
        "mock-ner/1"
    }
    fn detect(&self, text: &str) -> CoreResult<Vec<Detection>> {
        // "Detects" the literal word "nightingale" as an organization.
        Ok(text
            .match_indices("nightingale")
            .map(|(i, w)| Detection {
                start: i,
                end: i + w.len(),
                category: "org".to_string(),
                confidence: 0.9,
                detector: String::new(),
            })
            .collect())
    }
}

#[test]
fn ner_seam_fails_closed_without_a_backend_and_detects_with_one() {
    let dir = TempDir::new().unwrap();
    let mut m = open_encrypted(&dir, "a.db");
    m.set_anon_policy("caller", r#"{"mode": "egress", "detectors": ["tier0", "ner"]}"#)
        .unwrap();
    m.add(&fact("caller", "caller:john", "note", "the nightingale files")).unwrap();

    // Demanded but not installed → the covered read fails closed (D6).
    let err = m.recall("caller", "caller:john", None, 4).unwrap_err().to_string();
    assert!(err.contains("fails closed"), "{err}");

    // Installed → the backend's category flows through the policy chain
    // (unmapped category takes default_action = pseudonym).
    m.set_anonymizer(Box::new(MockNer));
    let got = m.recall("caller", "caller:john", None, 4).unwrap();
    let note = got[0].fields["object"].as_str().unwrap();
    assert!(!note.contains("nightingale"), "NER span leaked: {note}");
    assert!(note.contains("[ORG_"), "expected org token: {note}");
}
