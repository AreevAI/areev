//! CAS blob sidecar (§5.11) and bundle backup/sync (§5.10) tests.

use areev_core::types::{Fact, Grain};
use areev_store::Areev;
use tempfile::TempDir;

fn fact(ns: &str, s: &str, r: &str, o: &str) -> Fact {
    let mut f = Fact::new(s, r, o).confidence(0.9).source_type("user_explicit");
    f.common.namespace = Some(ns.to_string());
    f
}

#[test]
fn blob_cas_roundtrip_dedup_gc() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("m.db");
    let mut m = Areev::open(path.to_str().unwrap()).unwrap();

    let audio = vec![7u8; 4096];
    let uri = m.put_blob(&audio).unwrap();
    assert!(uri.starts_with("cas://sha256:"));
    // idempotent put
    let uri2 = m.put_blob(&audio).unwrap();
    assert_eq!(uri, uri2);
    // verified read
    assert_eq!(m.get_blob(&uri).unwrap(), audio);

    // a grain referencing the blob keeps it alive through GC
    let mut f = fact("caller", "alice", "said", "play my message");
    f.common.content_refs = vec![areev_core::types::ContentRef {
        uri: uri.clone(),
        modality: Some("audio".to_string()),
        mime_type: Some("audio/wav".to_string()),
        size_bytes: Some(4096),
        checksum: Some(uri.trim_start_matches("cas://").to_string()),
        metadata: None,
    }];
    m.add(&f).unwrap();

    // an orphan blob gets collected
    let orphan = m.put_blob(&[9u8; 128]).unwrap();
    let removed = m.gc_blobs().unwrap();
    assert_eq!(removed, 1);
    assert!(m.get_blob(&uri).is_ok());
    assert!(m.get_blob(&orphan).is_err());
}

#[test]
fn bundle_fast_forward_replication() {
    let da = TempDir::new().unwrap();
    let db_ = TempDir::new().unwrap();
    let pa = da.path().join("a.db");
    let pb = db_.path().join("b.db");
    let mut a = Areev::open(pa.to_str().unwrap()).unwrap();

    // source history: adds + supersede + forget
    let h1 = a.add(&fact("ns", "alice", "prefers", "tea")).unwrap();
    let h2 = a.add(&fact("ns", "alice", "lives_in", "Berlin")).unwrap();
    a.add(&fact("ns", "bob", "prefers", "coffee")).unwrap();
    let mut mv = fact("ns", "alice", "lives_in", "Munich");
    a.supersede(&h2, &mut mv).unwrap();
    a.forget(&h1).unwrap();

    let bpath = da.path().join("delta.mgb");
    let stats = a.bundle_since(0, bpath.to_str().unwrap()).unwrap();
    assert_eq!(stats.ops, 6); // 4 adds (incl. supersede's new grain) + supersede op + forget

    // replica applies the bundle
    let mut b = Areev::open(pb.to_str().unwrap()).unwrap();
    let imp = b.import_bundle(bpath.to_str().unwrap()).unwrap();
    assert!(imp.applied >= 4);

    // state equivalence on the observable surface
    let head = b.latest("ns", "alice", "lives_in").unwrap().unwrap();
    assert_eq!(head.get_str("object"), Some("Munich"));
    assert_eq!(b.recall("ns", "alice", Some("prefers"), 16).unwrap().len(), 0); // forgotten
    assert_eq!(b.recall("ns", "bob", None, 16).unwrap().len(), 1);
    // superseded old version is excluded from current recall on the replica
    assert_eq!(
        b.recall("ns", "alice", Some("lives_in"), 16).unwrap().len(),
        1
    );

    // idempotent re-import
    let again = b.import_bundle(bpath.to_str().unwrap()).unwrap();
    assert_eq!(again.applied, 0);

    // incremental: new op on A → delta bundle → replica converges
    a.add(&fact("ns", "alice", "speaks", "German")).unwrap();
    let b2 = da.path().join("delta2.mgb");
    let s2 = a.bundle_since(stats.last_op_seq, b2.to_str().unwrap()).unwrap();
    assert_eq!(s2.ops, 1);
    b.import_bundle(b2.to_str().unwrap()).unwrap();
    assert_eq!(
        b.recall("ns", "alice", Some("speaks"), 16).unwrap().len(),
        1
    );
}
