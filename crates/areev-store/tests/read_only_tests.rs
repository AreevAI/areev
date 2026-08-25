//! `AreevOptions::read_only` (issue #127): a handle opened read-only must
//! refuse every write with the documented `STO-E004` code, while reads keep
//! working exactly as before. Backend-agnostic by design — see
//! `crates/areev-conformance/src/cases/read_only.rs` for the cross-backend
//! twin of this file.

use areev_core::types::{Fact, Grain};
use areev_store::{Areev, AreevOptions};
use tempfile::TempDir;

fn fact(ns: &str, s: &str, r: &str, o: &str) -> Fact {
    let mut f = Fact::new(s, r, o).confidence(0.9);
    f.common.namespace = Some(ns.to_string());
    f
}

fn ro_opts() -> AreevOptions {
    AreevOptions { read_only: true, ..Default::default() }
}

#[test]
fn read_only_open_of_existing_memory_succeeds() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("m.db");
    let path = path.to_str().unwrap();

    // Seed it read-write, then close the handle (drop) before reopening —
    // the embedded backend is single-writer-per-file, so two live handles
    // on one path is a different error (STO-E002) this test must not hit.
    {
        let mut m = Areev::open(path).unwrap();
        m.add(&fact("caller", "alice", "prefers", "window seat")).unwrap();
    }

    let m = Areev::open_with(path, ro_opts()).unwrap();
    assert!(m.open_warnings().is_empty(), "{:?}", m.open_warnings());
}

#[test]
fn read_only_write_is_refused_with_sto_e004() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("m.db");
    let path = path.to_str().unwrap();
    {
        let mut m = Areev::open(path).unwrap();
        m.add(&fact("caller", "alice", "prefers", "window seat")).unwrap();
    }

    let mut m = Areev::open_with(path, ro_opts()).unwrap();
    let err = m.add(&fact("caller", "alice", "lives_in", "Berlin")).unwrap_err();
    assert_eq!(err.code(), "STO-E004");
    assert!(err.to_string().starts_with("STO-E004: "), "{err}");
}

#[test]
fn read_only_refuses_every_write_family() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("m.db");
    let path = path.to_str().unwrap();
    let hash;
    {
        let mut m = Areev::open(path).unwrap();
        hash = m.add(&fact("caller", "alice", "prefers", "window seat")).unwrap();
    }

    let mut m = Areev::open_with(path, ro_opts()).unwrap();

    // grain writes
    assert_eq!(
        m.add(&fact("caller", "bob", "prefers", "aisle seat")).unwrap_err().code(),
        "STO-E004"
    );
    let mut replacement = fact("caller", "alice", "prefers", "aisle seat");
    assert_eq!(m.supersede(&hash, &mut replacement).unwrap_err().code(), "STO-E004");
    assert_eq!(m.forget(&hash).unwrap_err().code(), "STO-E004");
    assert_eq!(
        m.forget_subject("caller", "alice").unwrap_err().code(),
        "STO-E004"
    );
    assert_eq!(m.rebuild_link_indexes().unwrap_err().code(), "STO-E004");

    // meta-row writes (retention/anon/saved-query family all ride meta_put)
    let policy = areev_store::RetentionPolicy { days: 30.0, grain_type: None, because: None };
    assert_eq!(
        m.set_retention_policy("caller", &policy).unwrap_err().code(),
        "STO-E004"
    );

    // CAS blob writes
    assert_eq!(m.put_blob(b"hello").unwrap_err().code(), "STO-E004");
    assert_eq!(m.gc_blobs().unwrap_err().code(), "STO-E004");
}

#[test]
fn read_only_reads_work_normally() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("m.db");
    let path = path.to_str().unwrap();
    {
        let mut m = Areev::open(path).unwrap();
        m.add(&fact("caller", "alice", "prefers", "window seat")).unwrap();
        m.add(&fact("caller", "alice", "lives_in", "Berlin")).unwrap();
    }

    let mut m = Areev::open_with(path, ro_opts()).unwrap();
    let got = m.recall("caller", "alice", None, 16).unwrap();
    assert_eq!(got.len(), 2);
    let got = m.recall_hybrid("caller", Some("alice"), None, None, 16, None).unwrap();
    assert_eq!(got.len(), 2);
}

#[test]
fn read_only_combined_with_a_conflicting_explicit_index_text_is_refused() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("m.db");
    let path = path.to_str().unwrap();
    // Created (and declared) with text indexing ON, the default.
    Areev::open(path).unwrap();

    let err = Areev::open_with(
        path,
        AreevOptions { read_only: true, index_text: false, ..Default::default() },
    )
    .map(|_| ())
    .unwrap_err();
    assert_eq!(err.code(), "STO-E004");
}

/// A read-only open must never bring a memory into existence — checked at
/// the filesystem level, since that is exactly the bug: a bare
/// `TursoDb::open` on a missing path happily creates the `.db` file (plus
/// its `-wal` once anything touches it, and `finish_open`'s
/// `create_dir_all` for `.blobs`), which would turn a typo'd `--db` path
/// into a silent "this memory is empty" instead of "this memory does not
/// exist". The directory must be byte-for-byte the same before and after.
#[test]
fn read_only_open_of_missing_memory_creates_nothing_on_disk() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("nope.db");
    let path = path.to_str().unwrap();

    let list = || -> Vec<String> {
        std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok().map(|e| e.file_name().to_string_lossy().to_string()))
            .collect()
    };
    let before = list();
    assert!(before.is_empty(), "precondition: tempdir starts empty: {before:?}");

    let err = Areev::open_with(path, ro_opts()).map(|_| ()).unwrap_err();
    assert_eq!(err.code(), "STO-E005");
    assert!(err.to_string().contains("does not exist"), "{err}");

    let after = list();
    assert!(
        after.is_empty(),
        "a refused read-only open must create nothing on disk (no .db/-wal/.blobs): {after:?}"
    );
}

/// The refusal above is specifically about the MAIN file's absence — an
/// existing, freshly-checkpointed memory (no `-wal`, no `.blobs` yet because
/// nothing was ever blobbed) must still open read-only normally.
#[test]
fn read_only_open_of_an_existing_file_with_no_sidecars_still_succeeds() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("m.db");
    let path_str = path.to_str().unwrap();
    {
        let mut m = Areev::open(path_str).unwrap();
        m.add(&fact("caller", "alice", "prefers", "window seat")).unwrap();
    }
    // No `-wal` file and no `.blobs` dir need to exist for this to pass —
    // whichever of them the embedded engine leaves behind after a clean
    // drop is incidental, not part of the contract being asserted here.

    let m = Areev::open_with(path_str, ro_opts()).unwrap();
    assert!(m.open_warnings().is_empty(), "{:?}", m.open_warnings());
}
