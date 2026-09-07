//! Postgres runner: the IDENTICAL conformance case list against a
//! schema-per-test Postgres backend, plus the Pg-only single-writer test.
//!
//! Needs a reachable server (pgvector image recommended):
//!   docker run --rm -d -p 5432:5432 -e POSTGRES_PASSWORD=postgres pgvector/pgvector:pg16
//!   export DATABASE_URL=postgres://postgres:postgres@127.0.0.1:5432/postgres
//! Skips (loudly) without DATABASE_URL/AREEV_PG_URL — except under CI=true,
//! where a missing database is a hard failure so a broken job can never look
//! like a skipped one.
#![cfg(feature = "postgres")]

use areev_conformance::{cases, Backend, PgBackend};

fn pg_url() -> Option<String> {
    std::env::var("AREEV_PG_URL")
        .or_else(|_| std::env::var("DATABASE_URL"))
        .ok()
        .filter(|u| u.starts_with("postgres"))
}

fn backend() -> Option<PgBackend> {
    match pg_url() {
        Some(url) => Some(PgBackend::new(&url)),
        None => {
            if std::env::var("CI").as_deref() == Ok("true") {
                panic!(
                    "CI=true but no DATABASE_URL — the postgres job must not silently skip"
                );
            }
            eprintln!(
                "skipping: no DATABASE_URL/AREEV_PG_URL (start one: docker run --rm -d \
                 -p 5432:5432 -e POSTGRES_PASSWORD=postgres pgvector/pgvector:pg16)"
            );
            None
        }
    }
}

// The case list lives in `for_each_conformance_case!` in the crate root —
// one list, every backend, by construction.
macro_rules! pg_case {
    ($name:ident) => {
        #[test]
        fn $name() {
            let Some(b) = backend() else { return };
            cases::$name(&b);
        }
    };
}

areev_conformance::for_each_conformance_case!(pg_case);

/// Multiple writers per memory: two instances (own handles, own
/// connections) write the same schema concurrently — everything lands,
/// nothing collides, and the op-log is gapless and strictly ordered (id
/// blocks are claimed inside each write transaction, which also serializes
/// them).
#[test]
fn concurrent_writers_all_land() {
    let Some(b) = backend() else { return };
    let url = pg_url().unwrap();
    let schema = b.schema_for("mw");
    drop(b.open_named("mw")); // create + seed the schema
    let writer = |tag: &'static str| {
        let url = url.clone();
        let schema = schema.clone();
        std::thread::spawn(move || {
            let mut m = areev_store::Areev::open_postgres(&url, &schema).unwrap();
            for i in 0..25 {
                m.add(&areev_conformance::fact("ns", &format!("{tag}{i}"), "writes", "ok"))
                    .unwrap();
            }
        })
    };
    let (t1, t2) = (writer("a"), writer("b"));
    t1.join().unwrap();
    t2.join().unwrap();
    let mut m = b.open_named("mw");
    assert_eq!(m.count().unwrap(), 50, "every concurrent write must land");
    let ops = m.changes_since(0, 1000).unwrap();
    assert_eq!(ops.len(), 50);
    for w in ops.windows(2) {
        assert!(w[0].op_seq < w[1].op_seq, "op_seq strictly increasing");
    }
    assert_eq!(
        ops.last().unwrap().op_seq - ops[0].op_seq + 1,
        50,
        "op_seq gapless — followers can never miss an op"
    );
    // cross-writer recall through a third handle
    assert_eq!(m.recall("ns", "a3", Some("writes"), 4).unwrap().len(), 1);
    assert_eq!(m.recall("ns", "b7", Some("writes"), 4).unwrap().len(), 1);
}

/// Two processes opening a BRAND-NEW memory simultaneously: the schema
/// bootstrap (DDL + seeding) runs under an advisory lock, so both succeed —
/// Postgres's IF NOT EXISTS DDL alone is racy and the loser would otherwise
/// fail open with a spurious 23505.
#[test]
fn concurrent_first_open_bootstraps_once() {
    let Some(b) = backend() else { return };
    let url = pg_url().unwrap();
    let schema = b.schema_for("boot");
    let opener = || {
        let url = url.clone();
        let schema = schema.clone();
        std::thread::spawn(move || {
            areev_store::Areev::open_postgres(&url, &schema).map(|mut m| m.count().unwrap())
        })
    };
    let (t1, t2) = (opener(), opener());
    let (r1, r2) = (t1.join().unwrap(), t2.join().unwrap());
    assert!(
        r1.is_ok() && r2.is_ok(),
        "both concurrent first-openers must succeed: {r1:?} / {r2:?}"
    );
    drop(b.open_named("boot")); // register the schema for cleanup
}

/// Two instances racing to supersede the SAME head: exactly one winner and
/// one clean SupersessionConflict — the single-writer contract, kept under
/// concurrency by the in-transaction FOR UPDATE recheck.
#[test]
fn concurrent_supersede_one_wins() {
    let Some(b) = backend() else { return };
    let url = pg_url().unwrap();
    let schema = b.schema_for("race");
    let v1 = {
        let mut m = b.open_named("race");
        m.add(&areev_conformance::fact("ns", "j", "plan", "basic")).unwrap()
    };
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
    let racer = |val: &'static str| {
        let url = url.clone();
        let schema = schema.clone();
        let barrier = barrier.clone();
        std::thread::spawn(move || {
            let mut m = areev_store::Areev::open_postgres(&url, &schema).unwrap();
            let mut v2 = areev_conformance::fact("ns", "j", "plan", val);
            barrier.wait();
            m.supersede(&v1, &mut v2).map(|_| val)
        })
    };
    let (t1, t2) = (racer("pro"), racer("max"));
    let (r1, r2) = (t1.join().unwrap(), t2.join().unwrap());
    let (winner, loser_err) = match (r1, r2) {
        (Ok(w), Err(e)) | (Err(e), Ok(w)) => (w, e),
        other => panic!("expected exactly one winner, got {other:?}"),
    };
    assert!(
        matches!(loser_err, areev_core::error::AreevError::SupersessionConflict(_)),
        "loser must get SupersessionConflict, got {loser_err:?}"
    );
    let mut m = b.open_named("race");
    assert_eq!(m.heads("ns", "j", "plan").unwrap().len(), 1, "one head, no fork");
    assert_eq!(m.latest("ns", "j", "plan").unwrap().unwrap().get_str("object"), Some(winner));
    assert_eq!(m.count().unwrap(), 2, "loser's grain rolled back with its transaction");
}

/// The recall-telemetry sidecar on Postgres: tables ride the memory's own
/// schema, rollups accumulate across flushes, and a forgotten grain is
/// scrubbed from them.
#[test]
fn telemetry_rollups_on_pg() {
    let Some(b) = backend() else { return };
    let url = pg_url().unwrap();
    let mut m = areev_store::Areev::open_postgres_with_telemetry(
        &url,
        &b.schema_for("telem"),
        areev_store::TelemetryMode::Aggregate,
    )
    .unwrap();
    assert_eq!(m.telemetry_mode(), areev_store::TelemetryMode::Aggregate);
    let h = m.add(&areev_conformance::fact("ns", "ana", "prefers", "quiet peaceful rooms")).unwrap();
    m.recall("ns", "ana", Some("prefers"), 4).unwrap();
    m.recall("ns", "ana", Some("prefers"), 4).unwrap();
    m.recall_hybrid("ns", None, None, Some("nonexistent topic"), 4, None).unwrap();
    m.telemetry_flush().unwrap();
    let access = m.telemetry_access_stats(Some("ns")).unwrap();
    assert_eq!(access.len(), 1, "one grain accessed");
    assert_eq!(access[0].recall_count, 2, "two structural recalls counted");
    assert_eq!(access[0].hash, h.to_hex());
    let queries = m.telemetry_query_stats(Some("ns")).unwrap();
    assert_eq!(queries.len(), 1, "one free-text question recorded");
    assert_eq!(queries[0].empty_count, 1, "the coverage-gap signal");
    m.telemetry_note_budget(true).unwrap();
    m.telemetry_note_budget(false).unwrap();
    let budget = m.telemetry_budget_stats().unwrap();
    assert_eq!((budget.sample_count, budget.overflow_count), (2, 1));
    // forget scrubs the sidecar so it never outlives an erased grain
    m.forget(&h).unwrap();
    assert!(m.telemetry_access_stats(Some("ns")).unwrap().is_empty(), "scrubbed on forget");
}

/// One instance holding several memories at once (the schema-per-org
/// shape): handles to distinct schemas coexist in one process and stay
/// strictly isolated.
#[test]
fn multi_memory_per_instance() {
    let Some(b) = backend() else { return };
    let mut org1 = b.open_named("org1");
    let mut org2 = b.open_named("org2");
    org1.add(&areev_conformance::fact("ns", "pat", "sees", "dr_lee")).unwrap();
    org2.add(&areev_conformance::fact("ns", "pat", "sees", "dr_kim")).unwrap();
    assert_eq!(org1.recall("ns", "pat", None, 8).unwrap().len(), 1);
    assert_eq!(org2.recall("ns", "pat", None, 8).unwrap().len(), 1);
    assert_eq!(
        org1.latest("ns", "pat", "sees").unwrap().unwrap().get_str("object"),
        Some("dr_lee"),
        "memories must not bleed into each other"
    );
    assert_eq!(
        org2.latest("ns", "pat", "sees").unwrap().unwrap().get_str("object"),
        Some("dr_kim")
    );
    // both handles stay usable interleaved
    org1.add(&areev_conformance::fact("ns", "kai", "sees", "dr_lee")).unwrap();
    assert_eq!(org2.count().unwrap(), 1);
    assert_eq!(org1.count().unwrap(), 2);
}

/// A handle opened BEFORE another instance wrote still sees the new data —
/// including subjects whose dictionary terms were interned by the other
/// writer (the DB-authoritative dictionary fallback).
#[test]
fn cross_instance_visibility() {
    let Some(b) = backend() else { return };
    let url = pg_url().unwrap();
    let mut reader = b.open_named("vis");
    let mut writer = areev_store::Areev::open_postgres(&url, &b.schema_for("vis")).unwrap();
    writer.add(&areev_conformance::fact("ns", "zoe", "likes", "tea")).unwrap();
    assert_eq!(
        reader.recall("ns", "zoe", Some("likes"), 4).unwrap().len(),
        1,
        "reader must see subjects interned after it opened"
    );
    assert_eq!(
        reader.latest("ns", "zoe", "likes").unwrap().unwrap().get_str("object"),
        Some("tea")
    );
}

/// File-backend capabilities are refused with clear errors, not ignored.
#[test]
fn file_only_capabilities_are_rejected() {
    let Some(b) = backend() else { return };
    let url = pg_url().unwrap();
    let opts = areev_store::AreevOptions {
        encryption_key: Some([7u8; 32]),
        ..Default::default()
    };
    let err = match areev_store::Areev::open_postgres_with(&url, &b.schema_for("caps"), opts) {
        Err(e) => e,
        Ok(_) => panic!("encryption_key must be rejected on the postgres backend"),
    };
    assert!(err.to_string().contains("file-backend capability"), "{err}");
}

/// A managed database restarts under a long-lived handle.
///
/// This is the finding that would have caused a production incident: `PgDb`
/// held ONE `tokio_postgres::Client` with no reconnect, so a routine Cloud SQL
/// maintenance restart (`57P01 terminating connection due to administrator
/// command`) permanently poisoned the handle — the process stayed alive and
/// every later call failed until the instance happened to be recycled.
///
/// The contract asserted here is deliberately asymmetric:
///   * a READ is replayed transparently — it had no effect, so running it
///     twice is running it once;
///   * a WRITE is NOT replayed — the connection may have died AFTER the server
///     committed it, and replaying would risk applying it twice. It reports the
///     failure and reconnects for the next call.
///
/// The kill is scoped by `application_name` to THIS handle's connection.
/// Terminating every backend on the database (the obvious way to write it)
/// also kills the connections of every other test sharing the server, since
/// cargo runs them in parallel.
///
/// The one case in this suite that needs its OWN connection rather than a
/// `Areev` handle — the side channel that issues `pg_terminate_backend`. It
/// speaks `NoTls`, so a DSN that demands encryption skips this test rather
/// than failing it: the store's own sessions handle TLS (`pgtls::connect`),
/// this probe does not.
#[test]
fn a_handle_recovers_from_a_database_outage() {
    let Some(b) = backend() else { return };
    let base = pg_url().unwrap();
    if base.contains("sslmode=require")
        || base.contains("sslmode=verify")
        || base.contains("sslrootcert=")
    {
        eprintln!(
            "skipping a_handle_recovers_from_a_database_outage: the pg_terminate_backend \
             side channel connects with NoTls, and this DSN demands encryption"
        );
        return;
    }
    let schema = b.schema_for("outage");
    const APP: &str = "areev_outage_probe";
    let sep = if base.contains('?') { '&' } else { '?' };
    let tagged = format!("{base}{sep}application_name={APP}");

    let mut m = areev_store::Areev::open_postgres(&tagged, &schema).unwrap();
    m.add(&areev_conformance::fact("ns", "before", "wrote", "ok"))
        .unwrap();
    assert_eq!(m.count().unwrap(), 1);

    // What a restart or failover looks like from the client's side.
    let kill = |url: &str| {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let (client, conn) = tokio_postgres::connect(url, tokio_postgres::NoTls)
                .await
                .unwrap();
            tokio::spawn(async move {
                let _ = conn.await;
            });
            client
                .simple_query(&format!(
                    "SELECT pg_terminate_backend(pid) FROM pg_stat_activity \
                     WHERE datname = current_database() AND application_name = '{APP}' \
                       AND pid <> pg_backend_pid()"
                ))
                .await
                .unwrap();
        });
    };

    // ── A read survives the outage ───────────────────────────────────────
    kill(&base);
    assert_eq!(
        m.count().unwrap(),
        1,
        "a read must reconnect and replay — before this it failed forever"
    );

    // ── Reads keep working afterwards (the statement cache was rebuilt) ──
    // A reconnect that kept the old session's prepared statements would fail
    // here with 26000 invalid_sql_statement_name instead.
    assert_eq!(
        m.recall("ns", "before", Some("wrote"), 4).unwrap().len(),
        1,
        "hot/prepared paths must work on the new session"
    );

    // ── A write is not silently replayed, but the handle recovers ────────
    kill(&base);
    // This call may fail (its connection is gone) — what must NOT happen is
    // the handle staying dead.
    let _ = m.add(&areev_conformance::fact("ns", "during", "wrote", "maybe"));
    m.add(&areev_conformance::fact("ns", "after", "wrote", "ok"))
        .expect("the handle must be usable again after an outage");

    // And it is a real, queryable write on the same memory.
    assert_eq!(m.recall("ns", "after", Some("wrote"), 4).unwrap().len(), 1);
}

/// #160: a schema written by a build that keyed `terms` by the text itself
/// (`term text UNIQUE`, no `term_hash`) converges on the first read-write
/// open — the column is added and backfilled with the SAME digest the Rust
/// side computes, the unique index appears, the text constraint that
/// capped values at ~2704 bytes goes — and every term the old schema held
/// still resolves to its old id rather than being re-interned.
#[test]
fn legacy_text_keyed_dictionary_migrates_on_open() {
    let Some(b) = backend() else { return };
    let url = pg_url().unwrap();
    let schema = b.schema_for("legacy_terms");
    areev_store::pg::execute_raw(
        &url,
        &format!(
            "CREATE SCHEMA \"{schema}\"; \
             CREATE TABLE \"{schema}\".terms(id bigint GENERATED BY DEFAULT AS IDENTITY PRIMARY KEY, term text, \
                 CONSTRAINT restored_with_another_name UNIQUE (term)); \
             CREATE UNIQUE INDEX hand_added_term_idx ON \"{schema}\".terms(term); \
             INSERT INTO \"{schema}\".terms(term) VALUES ('ns'), ('john'), ('prefers');"
        ),
    )
    .unwrap();
    let q = |sql: String| areev_store::pg::query_raw_i64(&url, &sql).unwrap();

    // The open runs the migration.
    let mut m = b.open_named("legacy_terms");
    assert_eq!(
        q(format!(
            "SELECT count(*) FROM information_schema.columns WHERE table_schema = '{schema}' \
             AND table_name = 'terms' AND column_name = 'term_hash'"
        )),
        1,
        "term_hash column added"
    );
    assert_eq!(q(format!("SELECT count(*) FROM \"{schema}\".terms WHERE term_hash IS NULL")), 0);
    assert_eq!(
        q(format!(
            "SELECT count(*) FROM pg_indexes WHERE schemaname = '{schema}' \
             AND tablename = 'terms' AND indexname = 'idx_terms_hash'"
        )),
        1,
        "unique digest index created"
    );
    // The cap is matched by DEFINITION: a constraint under a non-default
    // name and a standalone unique index on (term) both go; nothing unique
    // on `term` may survive, whatever it was called.
    assert_eq!(
        q(format!(
            "SELECT count(*) FROM pg_constraint c JOIN pg_class t ON c.conrelid = t.oid \
             JOIN pg_namespace n ON t.relnamespace = n.oid \
             JOIN pg_attribute a ON a.attrelid = t.oid AND a.attname = 'term' \
             WHERE n.nspname = '{schema}' AND t.relname = 'terms' \
               AND c.contype = 'u' AND c.conkey = ARRAY[a.attnum]"
        )),
        0,
        "every unique constraint on (term) — the ~2704-byte cap — is gone"
    );
    assert_eq!(
        q(format!(
            "SELECT count(*) FROM pg_index i JOIN pg_class t ON i.indrelid = t.oid \
             JOIN pg_namespace n ON t.relnamespace = n.oid \
             JOIN pg_attribute a ON a.attrelid = t.oid AND a.attname = 'term' \
             WHERE n.nspname = '{schema}' AND t.relname = 'terms' \
               AND i.indisunique AND i.indnkeyatts = 1 AND i.indkey[0] = a.attnum"
        )),
        0,
        "and so is a hand-added unique index on (term)"
    );

    // Pre-existing strings resolve to their pre-existing rows.
    m.add(&areev_conformance::fact("ns", "john", "prefers", "tea")).unwrap();
    assert_eq!(
        q(format!(
            "SELECT count(*) FROM \"{schema}\".terms WHERE term IN ('ns', 'john', 'prefers')"
        )),
        3,
        "no duplicate rows for migrated terms"
    );
    assert_eq!(m.recall("ns", "john", Some("prefers"), 8).unwrap().len(), 1);

    // A term ANOTHER writer interns after this handle opened (so it is not
    // in the process-local cache) is found through the backfill digest —
    // which is also the proof that SQL's sha256 and Rust's agree. Were they
    // to differ, the intern below would insert a second row.
    areev_store::pg::execute_raw(
        &url,
        &format!(
            "INSERT INTO \"{schema}\".terms(term, term_hash) \
             VALUES ('late', sha256(convert_to('late', 'UTF8')))"
        ),
    )
    .unwrap();
    m.add(&areev_conformance::fact("ns", "late", "prefers", "coffee")).unwrap();
    assert_eq!(q(format!("SELECT count(*) FROM \"{schema}\".terms WHERE term = 'late'")), 1);
    assert_eq!(m.recall("ns", "late", Some("prefers"), 8).unwrap().len(), 1);

    // And the value that used to fail with SQLSTATE 54000 now stores.
    let big = areev_conformance::cases::incompressible(6000);
    m.add(&areev_conformance::fact("ns", "john", "blob", &big)).unwrap();
    assert_eq!(
        m.latest("ns", "john", "blob").unwrap().unwrap().get_str("object"),
        Some(big.as_str())
    );

    // A read-only open of a schema WITHOUT the column is refused by name,
    // never answered empty.
    let stale = b.schema_for("legacy_terms_ro");
    areev_store::pg::execute_raw(
        &url,
        &format!(
            "CREATE SCHEMA \"{stale}\"; \
             CREATE TABLE \"{stale}\".meta(k text PRIMARY KEY, v text); \
             CREATE TABLE \"{stale}\".terms(id bigint PRIMARY KEY, term text UNIQUE); \
             CREATE TABLE \"{stale}\".grains(seq bigint PRIMARY KEY); \
             CREATE TABLE \"{stale}\".oplog(op_seq bigint PRIMARY KEY); \
             CREATE TABLE \"{stale}\".fts_doc(seq bigint PRIMARY KEY);"
        ),
    )
    .unwrap();
    let err = areev_store::Areev::open_postgres_with(
        &url,
        &stale,
        areev_store::AreevOptions { read_only: true, ..Default::default() },
    )
    .err()
    .expect("read-only open of a pre-migration schema must refuse");
    let msg = err.to_string();
    assert!(msg.starts_with("STO-E005"), "{msg}");
    assert!(msg.contains("term_hash"), "{msg}");
}
