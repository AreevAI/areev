//! The Tier-2 destruction audit chain (#280).
//!
//! `docs/procurement.md` says the destruction trail exports "hash-chain
//! verified; truncation is flagged, never silent". Before this, destruction
//! records were standalone Observations with no predecessor and no sequence:
//! forgetting one left the export clean. One chain per memory, shared by
//! every writer, is what makes a missing record detectable.

use areev_core::authz::{audit_observation, AUTHZ_NS};
use areev_store::Areev;
use tempfile::TempDir;

fn open_mem() -> (Areev, TempDir) {
    let d = TempDir::new().unwrap();
    let m = Areev::open(d.path().join("m.db").to_str().unwrap()).unwrap();
    (m, d)
}

fn seq_of(m: &mut Areev, h: &areev_core::Hash) -> u64 {
    let g = m.get(h).unwrap();
    g.fields
        .get("context")
        .and_then(|c| c.get("seq"))
        .and_then(serde_json::Value::as_u64)
        .expect("a chained record carries its sequence")
}

#[test]
fn ten_appends_form_one_chain_numbered_one_to_ten() {
    let (mut m, _d) = open_mem();
    let mut hashes = Vec::new();
    for i in 0..10 {
        let mut obs = audit_observation(
            "counsel:jane",
            "delete",
            &format!("hash:{i:02x}"),
            Some("dsar"),
            1,
            1_000 + i as i64,
        );
        hashes.push(m.append_audit(&mut obs).unwrap());
    }
    for (i, h) in hashes.iter().enumerate() {
        assert_eq!(seq_of(&mut m, h), i as u64 + 1);
    }
    // Each record names its predecessor, and the head names the last.
    for (i, h) in hashes.iter().enumerate().skip(1) {
        let g = m.get(h).unwrap();
        assert_eq!(
            g.fields["context"]["previous_audit"].as_str().unwrap(),
            hashes[i - 1].to_hex()
        );
    }
    let (seq, head) = m.audit_chain_head().unwrap().unwrap();
    assert_eq!(seq, 10);
    assert_eq!(head, hashes[9].to_hex());
}

#[test]
fn the_first_record_is_a_chain_root_with_no_predecessor() {
    let (mut m, _d) = open_mem();
    let mut obs = audit_observation("u", "erase", "subject:ab ns:n", Some("dsar"), 1, 1_000);
    let h = m.append_audit(&mut obs).unwrap();
    let g = m.get(&h).unwrap();
    assert_eq!(g.fields["context"]["chain_root"], serde_json::json!(true));
    assert!(g.fields["context"].get("previous_audit").is_none());
    assert_eq!(g.fields["context"]["seq"], serde_json::json!(1));
}

#[test]
fn chained_records_land_in_the_authz_namespace() {
    let (mut m, _d) = open_mem();
    let mut obs = audit_observation("u", "erase", "subject:ab ns:n", Some("dsar"), 1, 1_000);
    m.append_audit(&mut obs).unwrap();
    assert_eq!(obs.common.namespace.as_deref(), Some(AUTHZ_NS));
}

#[test]
fn a_chained_audit_record_is_not_forgettable_by_hash() {
    // #280 item 6: otherwise the trail the export verifies is deletable by
    // exactly the session whose destruction it records.
    let (mut m, _d) = open_mem();
    let mut obs = audit_observation("u", "erase", "subject:ab ns:n", Some("dsar"), 1, 1_000);
    let h = m.append_audit(&mut obs).unwrap();
    let err = m.forget(&h).unwrap_err();
    assert!(
        err.to_string().contains("not deletable by hash"),
        "got {err}"
    );
    assert!(m.has(&h).unwrap());
}

#[test]
fn an_ordinary_grain_is_still_forgettable() {
    use areev_core::types::Fact;
    let (mut m, _d) = open_mem();
    let mut f = Fact::new("a", "b", "c");
    f.common.namespace = Some("ns".into());
    let h = m.add(&f).unwrap();
    m.forget(&h).unwrap();
    assert!(!m.has(&h).unwrap());
}

#[test]
fn the_head_is_a_cache_and_survives_a_reopen() {
    let d = TempDir::new().unwrap();
    let path = d.path().join("m.db");
    let path = path.to_str().unwrap();
    {
        let mut m = Areev::open(path).unwrap();
        let mut obs = audit_observation("u", "erase", "subject:ab ns:n", Some("x"), 1, 1_000);
        m.append_audit(&mut obs).unwrap();
    }
    let mut m = Areev::open(path).unwrap();
    let mut obs = audit_observation("u", "delete", "hash:ff", None, 1, 2_000);
    let h = m.append_audit(&mut obs).unwrap();
    assert_eq!(seq_of(&mut m, &h), 2, "the chain continues across a reopen");
}
