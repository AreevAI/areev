//! `AreevOptions::read_only` (issue #127): a handle opened read-only refuses
//! every write with the documented `STO-E004` code — IDENTICALLY on both
//! backends, which is the whole point: on postgres this is what lets a
//! least-privilege SELECT-only role open an existing, fully-migrated memory
//! at all (bootstrap DDL is skipped and never attempted), and the embedded
//! backend enforces the exact same contract even though it has no privilege
//! system to fail against. Reads must keep working exactly as before, and a
//! refused write must never partially apply.

use crate::{fact, Backend};
use areev_store::AreevOptions;

fn ro_opts() -> AreevOptions {
    AreevOptions { read_only: true, ..Default::default() }
}

pub fn read_only_open_succeeds_and_refuses_every_write(b: &dyn Backend) {
    let hash;
    {
        // Seed read-write, then drop the handle before reopening — both
        // backends require the previous handle gone before a name reopens.
        let mut m = b.open_named("ro");
        hash = m.add(&fact("caller", "alice", "prefers", "window seat")).unwrap();
        m.add(&fact("caller", "alice", "lives_in", "Berlin")).unwrap();
    }

    let mut m = b.open_named_with("ro", ro_opts());
    assert!(m.open_warnings().is_empty(), "[{}] {:?}", b.name(), m.open_warnings());

    // Reads work exactly as on a read-write handle.
    let got = m.recall("caller", "alice", None, 16).unwrap();
    assert_eq!(got.len(), 2, "[{}] recall", b.name());
    let got = m
        .recall_hybrid("caller", Some("alice"), None, None, 16, None)
        .unwrap();
    assert_eq!(got.len(), 2, "[{}] recall_hybrid", b.name());

    // Grain writes: add, supersede, forget, forget_subject.
    let err = m.add(&fact("caller", "bob", "prefers", "aisle seat")).unwrap_err().to_string();
    assert!(err.starts_with("STO-E004"), "[{}] add: {err}", b.name());

    let mut v2 = fact("caller", "alice", "prefers", "aisle seat");
    let err = m.supersede(&hash, &mut v2).unwrap_err().to_string();
    assert!(err.starts_with("STO-E004"), "[{}] supersede: {err}", b.name());

    let err = m.forget(&hash).unwrap_err().to_string();
    assert!(err.starts_with("STO-E004"), "[{}] forget: {err}", b.name());

    let err = m.forget_subject("caller", "alice").unwrap_err().to_string();
    assert!(err.starts_with("STO-E004"), "[{}] forget_subject: {err}", b.name());

    // Meta-row writes (retention/anon/saved-query family all ride meta_put).
    let policy = areev_store::RetentionPolicy { days: 30.0, grain_type: None, because: None };
    let err = m.set_retention_policy("caller", &policy).unwrap_err().to_string();
    assert!(err.starts_with("STO-E004"), "[{}] set_retention_policy: {err}", b.name());

    // CAS blob writes.
    let err = m.put_blob(b"hello").unwrap_err().to_string();
    assert!(err.starts_with("STO-E004"), "[{}] put_blob: {err}", b.name());

    // Every refusal above must have applied nothing.
    let got = m.recall("caller", "alice", None, 16).unwrap();
    assert_eq!(got.len(), 2, "[{}] a refused write must not partially apply", b.name());
    assert!(
        m.retention_policies().unwrap().is_empty(),
        "[{}] a refused retention policy must not be recorded",
        b.name()
    );
}

/// A read-only open of a name NOTHING has ever created must refuse rather
/// than bring the memory into existence — on the embedded backend a bare
/// open otherwise happily creates the file, its `-wal`, and its `.blobs`
/// sidecar, which would turn a typo'd path into a silent "this memory is
/// empty" instead of "this memory does not exist". `STO-E005` on both
/// backends (the postgres half of this same code path is "schema absent");
/// `try_open_named_with` is used instead of `open_named_with` because the
/// latter panics on the very failure this case asserts. A plain read-write
/// open of the same name right afterward must still see a genuinely fresh,
/// empty memory — proof the refused attempt bootstrapped nothing on either
/// backend. The embedded-only twin
/// (`crates/areev-store/tests/read_only_tests.rs`) additionally reads the
/// directory itself, since "creates nothing" is a filesystem claim postgres
/// has no analogue for.
pub fn read_only_open_of_missing_memory_refuses_and_creates_nothing(b: &dyn Backend) {
    let err = b.try_open_named_with("missing", ro_opts()).map(|_| ()).unwrap_err().to_string();
    assert!(err.starts_with("STO-E005"), "[{}] {err}", b.name());

    let mut m = b.open_named("missing");
    assert_eq!(
        m.count().unwrap(),
        0,
        "[{}] a refused read-only open must not have created or partially bootstrapped \
         the memory",
        b.name()
    );
}
