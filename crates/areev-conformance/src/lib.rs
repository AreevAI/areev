//! Backend-parameterized conformance suite for the Areev store.
//!
//! Every case in [`cases`] is a plain function over [`Backend`] pinning a
//! storage-semantics contract — fork/head election, oplog replication,
//! tombstones, novelty — that must hold IDENTICALLY on every storage backend.
//! The Turso runner lives in `tests/turso.rs`; a Postgres runner adds a
//! `Backend` impl whose `open_named` maps names to schemas instead of files
//! and runs the same case list unchanged.
//!
//! Determinism rules (from the areev-testing skill) apply to every case:
//! isolation via the backend's unit (tempdir file / schema), fixed
//! `created_at` wherever an ordering or election is asserted, and no
//! assertions on unordered result order.

use std::path::Path;

use areev_core::types::{Fact, Grain};
use areev_store::Areev;

pub mod cases;

/// A conformance backend: a factory of isolated stores.
///
/// One `Backend` instance = one isolation unit (a tempdir, a schema prefix).
/// `open_named` maps a stable name to the same underlying storage every time,
/// which is how reopen semantics (counter re-seeding, meta honoring) are
/// exercised — callers must drop the previous handle before reopening a name
/// (single writer per memory holds on every backend).
pub trait Backend {
    /// Open (or reopen) the store identified by `name` within this backend's
    /// isolation unit.
    fn open_named(&self, name: &str) -> Areev;

    /// Like [`open_named_with`](Self::open_named_with) but returns the
    /// `Result` instead of panicking on failure — for cases that assert a
    /// REFUSED open (e.g. `read_only` against a name nothing has ever
    /// created). Each backend still performs whatever bookkeeping
    /// `open_named_with` would (Postgres schema registration for Drop
    /// cleanup), so a refused open still cleans up after itself.
    fn try_open_named_with(
        &self,
        name: &str,
        opts: areev_store::AreevOptions,
    ) -> areev_core::error::Result<Areev>;

    /// [`open_named`](Self::open_named) with explicit host options — the seam
    /// for capabilities that are host config rather than file-truths (an
    /// `anon_key`, an embedder). Each backend routes to its own
    /// `open_with`-family constructor, so a case can assert that a capability
    /// behaves identically whether the memory is a file or a schema.
    fn open_named_with(&self, name: &str, opts: areev_store::AreevOptions) -> Areev {
        self.try_open_named_with(name, opts).expect("open store")
    }

    /// A scratch directory for interchange files (bundles). Backend-neutral:
    /// bundles are files regardless of where the store lives.
    fn scratch(&self) -> &Path;

    /// Label used in failure messages.
    fn name(&self) -> &'static str;

    fn open(&self) -> Areev {
        self.open_named("default")
    }
}

/// The embedded (Turso file) backend: names map to files in one tempdir.
pub struct TursoBackend {
    dir: tempfile::TempDir,
}

impl TursoBackend {
    #[allow(clippy::new_without_default)]
    pub fn new() -> Self {
        Self { dir: tempfile::TempDir::new().expect("tempdir") }
    }

    /// The path `open_named(name)` maps to — for tests that need to assert
    /// on the file directly (existence, absence) without opening it.
    /// Mirrors `PgBackend::schema_for`.
    pub fn path_for(&self, name: &str) -> std::path::PathBuf {
        self.dir.path().join(format!("{name}.db"))
    }
}

impl Backend for TursoBackend {
    fn open_named(&self, name: &str) -> Areev {
        let path = self.dir.path().join(format!("{name}.db"));
        Areev::open(path.to_str().expect("utf8 path")).expect("open turso store")
    }

    fn try_open_named_with(
        &self,
        name: &str,
        opts: areev_store::AreevOptions,
    ) -> areev_core::error::Result<Areev> {
        let path = self.dir.path().join(format!("{name}.db"));
        Areev::open_with(path.to_str().expect("utf8 path"), opts)
    }

    fn scratch(&self) -> &Path {
        self.dir.path()
    }

    fn name(&self) -> &'static str {
        "turso"
    }
}

/// The Postgres backend: names map to schemas under a per-instance prefix
/// (`conf_<pid>_<n>_<name>`), dropped on `Drop` — the schema-per-test
/// analogue of the tempdir rule. No clock or RNG in the prefix (determinism
/// rule), and a leaked schema from a killed run is prefix-recognizable.
#[cfg(feature = "postgres")]
pub struct PgBackend {
    url: String,
    prefix: String,
    scratch: tempfile::TempDir,
    opened: std::cell::RefCell<std::collections::HashSet<String>>,
}

#[cfg(feature = "postgres")]
static PG_BACKEND_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

#[cfg(feature = "postgres")]
impl PgBackend {
    pub fn new(url: &str) -> Self {
        let n = PG_BACKEND_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Self {
            url: url.to_string(),
            prefix: format!("conf_{}_{}", std::process::id(), n),
            scratch: tempfile::TempDir::new().expect("tempdir"),
            opened: Default::default(),
        }
    }

    /// The schema `open_named(name)` maps to — for tests that reach the same
    /// storage through a raw `open_postgres`. Registers the name for Drop
    /// cleanup exactly like `open_named`, so direct-open tests can't leak
    /// schemas.
    pub fn schema_for(&self, name: &str) -> String {
        let schema = format!("{}_{}", self.prefix, name);
        self.opened.borrow_mut().insert(schema.clone());
        schema
    }
}

#[cfg(feature = "postgres")]
impl Backend for PgBackend {
    fn open_named(&self, name: &str) -> Areev {
        let schema = format!("{}_{}", self.prefix, name);
        self.opened.borrow_mut().insert(schema.clone());
        Areev::open_postgres(&self.url, &schema).expect("open postgres store")
    }

    fn try_open_named_with(
        &self,
        name: &str,
        opts: areev_store::AreevOptions,
    ) -> areev_core::error::Result<Areev> {
        let schema = format!("{}_{}", self.prefix, name);
        self.opened.borrow_mut().insert(schema.clone());
        Areev::open_postgres_with(&self.url, &schema, opts)
    }

    fn scratch(&self) -> &Path {
        self.scratch.path()
    }

    fn name(&self) -> &'static str {
        "postgres"
    }
}

#[cfg(feature = "postgres")]
impl Drop for PgBackend {
    fn drop(&mut self) {
        for schema in self.opened.borrow().iter() {
            let _ = areev_store::pg::drop_postgres_schema(&self.url, schema);
        }
    }
}

/// THE conformance case list — the single source of truth both runners
/// consume, so a case added here runs on every backend and a case added
/// anywhere else does not compile. Each runner passes its own
/// test-generating macro:
///
/// ```ignore
/// macro_rules! my_case { ($name:ident) => { #[test] fn $name() { … } }; }
/// areev_conformance::for_each_conformance_case!(my_case);
/// ```
#[macro_export]
macro_rules! for_each_conformance_case {
    ($per_case:ident) => {
        // add / recall / reopen
        $per_case!(add_recall_roundtrip);
        $per_case!(unknown_terms_short_circuit_empty);
        $per_case!(recall_orders_newest_first_and_honors_k);
        $per_case!(reopen_preserves_state_and_counters);
        // supersede × forget
        $per_case!(supersede_returns_only_head);
        $per_case!(forget_clears_head_row);
        $per_case!(forget_new_head_does_not_resurrect_old);
        $per_case!(forget_superseded_old_keeps_new_head);
        $per_case!(double_supersede_is_a_local_conflict);
        $per_case!(forget_missing_grain_is_not_found);
        $per_case!(add_if_novel_dedupes_current_value);
        $per_case!(reasserting_superseded_value_is_novel);
        // heads / forks / merge
        $per_case!(concurrent_supersede_forks_then_merges);
        $per_case!(provisional_head_election_is_deterministic);
        $per_case!(same_supersede_replay_stays_idempotent);
        $per_case!(open_forks_enumerates_and_clears_on_merge);
        $per_case!(fork_then_forget_one_tip_resolves_fork);
        $per_case!(merge_requires_an_open_fork);
        $per_case!(forget_tip_reelection_ignores_link_rows);
        $per_case!(supersede_changed_key_reelection_ignores_link_rows);
        $per_case!(supersede_changed_relation_reconciles_old_key);
        // oplog / bundles / PITR
        $per_case!(supersede_two_hop_replication_converges);
        $per_case!(merge_replicates_as_fork_closure);
        $per_case!(merge_heads_closure_logged);
        $per_case!(forget_replicates_as_tombstone);
        $per_case!(changes_since_cursor_pages_in_order);
        $per_case!(pitr_max_hlc_cutoff_is_inclusive);
        // registry meta segment (saved queries / templates / retention)
        $per_case!(registry_rides_a_full_bundle);
        $per_case!(registry_merge_is_latest_wins_and_keeps_local_last_run);
        $per_case!(pitr_import_skips_the_registry);
        $per_case!(retention_row_never_clobbers_local_policy);
        $per_case!(anon_policy_replicates_write_if_absent);
        $per_case!(vault_rows_never_replicate);
        $per_case!(trigger_state_never_replicates);
        $per_case!(meta_cas_admits_one_claimer_and_fences_the_loser);
        $per_case!(value_derived_anon_refuses_without_key_material);
        $per_case!(host_anon_key_unlocks_the_vault);
        // CAS blobs + hybrid legs
        $per_case!(cas_blob_roundtrip_and_gc);
        $per_case!(forget_reclaims_sole_referenced_blob);
        $per_case!(bm25_leg_finds_text);
        $per_case!(vector_leg_roundtrip);
        // bulk erasure (right-to-erasure + retention)
        $per_case!(subject_erasure_is_complete);
        $per_case!(partition_keys_and_text_mentions_erase);
        $per_case!(subject_erasure_replicates);
        $per_case!(retention_erases_only_older);
        $per_case!(subject_report_mirrors_erasure);
        $per_case!(subject_without_relation_is_indexed);
        // run journal (the runtime's resume-correctness substrate)
        $per_case!(run_id_indexes_on_any_grain_type);
        $per_case!(run_grains_pages_completely);
        $per_case!(step_action_survives_supersession_only_by_restatement);
        // namespace prefix scoping ("org.*") + the ns registry
        $per_case!(ns_prefix_scope_selects_tree_exactly);
        $per_case!(ns_registry_tracks_lifecycle);
        $per_case!(ns_registry_replicates_via_bundles);
        $per_case!(ns_scope_guards_hold);
        $per_case!(ns_scoped_recall_spans_supersession_chains);
        // read-only opens (least-privilege postgres roles, issue #127)
        $per_case!(read_only_open_succeeds_and_refuses_every_write);
        $per_case!(read_only_open_of_missing_memory_refuses_and_creates_nothing);
    };
}

/// Standard test fact: namespaced, mid confidence.
pub fn fact(ns: &str, s: &str, r: &str, o: &str) -> Fact {
    let mut f = Fact::new(s, r, o).confidence(0.9);
    f.common.namespace = Some(ns.to_string());
    f
}

/// A fact with a pinned `created_at` — REQUIRED whenever the case asserts an
/// election or ordering outcome (provisional head = max (created_at, hash)).
pub fn fact_at(ns: &str, s: &str, r: &str, o: &str, created_ms: i64) -> Fact {
    let mut f = fact(ns, s, r, o);
    f.common.created_at = Some(created_ms);
    f
}
