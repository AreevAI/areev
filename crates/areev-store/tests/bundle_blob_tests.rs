//! CAS blob sidecar (§5.11) and bundle backup/sync (§5.10) tests.

use areev_core::types::{Fact, Grain};
use areev_store::{read_blob_offline, Areev};
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

// ── Issue #27: reading a blob without opening the database ────────────────
// The embedded backend's file lock is EXCLUSIVE — while a run holds a memory,
// a second process is refused even for a read. That put an attachment out of
// reach of the very `--tool-cmd` subprocess the run spawned to process it.
// `read_blob_offline` answers that without a read-only open mode: blobs are
// immutable, live in the `.blobs` sidecar rather than in the file, and carry
// their checksum as their address, so there is no lock to take and no
// consistency question to answer.

#[test]
fn blobs_are_readable_without_opening_the_database() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("m.db");
    let path = path.to_str().unwrap().to_string();

    let payload = b"invoice attachment \x00\x01\xff bytes".to_vec();
    let uri = {
        let mut m = Areev::open(&path).unwrap();
        m.put_blob(&payload).unwrap()
    };

    // The handle above is dropped, but the point is that this call never opens
    // the database at all — it is the same code path a subprocess takes while
    // the writer is still held.
    let got = read_blob_offline(&path, &uri).unwrap();
    assert_eq!(got.as_deref(), Some(payload.as_slice()));
}

#[test]
fn the_offline_read_holds_no_lock_against_a_live_handle() {
    // The actual scenario: a handle is OPEN and holding the file when the read
    // happens. A second `Areev::open` here would fail; this must not.
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("m.db");
    let path = path.to_str().unwrap().to_string();

    let mut held = Areev::open(&path).unwrap();
    let uri = held.put_blob(b"held-open payload").unwrap();

    assert!(Areev::open(&path).is_err(), "a second open must still be refused");
    assert_eq!(
        read_blob_offline(&path, &uri).unwrap().as_deref(),
        Some(b"held-open payload".as_slice()),
        "the sidecar read must not need the lock the writer holds"
    );
    drop(held);
}

#[test]
fn the_offline_read_verifies_the_address_and_refuses_junk() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("m.db");
    let path = path.to_str().unwrap().to_string();
    {
        let mut m = Areev::open(&path).unwrap();
        m.put_blob(b"payload").unwrap();
    }

    for bad in ["not-a-uri", "cas://sha256:", "cas://sha256:abc", "cas://sha256:\u{20ac}"] {
        assert!(read_blob_offline(&path, bad).is_err(), "must reject {bad:?}");
    }
    // Well-formed but absent is a clean Err, not a panic or a silent empty.
    assert!(read_blob_offline(&path, &format!("cas://sha256:{}", "a".repeat(64))).is_err());

    // A corrupted sidecar file must fail the content-address check rather than
    // hand back wrong bytes — the address IS the integrity guarantee here,
    // since no database row corroborates it.
    let uri = {
        let mut m = Areev::open(&path).unwrap();
        m.put_blob(b"tamper me").unwrap()
    };
    let hex = uri.strip_prefix("cas://sha256:").unwrap();
    let blob_file = std::path::PathBuf::from(format!("{path}.blobs"))
        .join(&hex[..2])
        .join(&hex[2..]);
    std::fs::write(&blob_file, b"different bytes entirely").unwrap();
    let e = read_blob_offline(&path, &uri).unwrap_err().to_string();
    assert!(e.contains("corrupt"), "tampering must be caught: {e}");
}
