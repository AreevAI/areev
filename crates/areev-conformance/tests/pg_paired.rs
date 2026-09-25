//! Postgres runner, PAIRED layout (#353): the IDENTICAL conformance case list
//! against a backend whose every memory is a memory schema plus a physically
//! separate metadata schema (`?meta_schema=`), followed by the cases only a
//! pair can exhibit — where each table landed, two pairs in one database,
//! the layout-mismatch refusals, `provision=never`/read-only parity, erasure
//! of the pair, concurrent writers, and a least-privilege role granted on
//! both schemas.
//!
//! Same server contract as `tests/pg.rs`: `DATABASE_URL`/`AREEV_PG_URL`,
//! skips loudly without one, hard-fails under `CI=true`.
#![cfg(feature = "postgres")]

use areev_conformance::{cases, fact, Backend, PgBackend};
use areev_store::pg::{self, META_TABLES, PG_TABLES};
use areev_store::{Areev, AreevOptions, TelemetryMode};

fn pg_url() -> Option<String> {
    std::env::var("AREEV_PG_URL")
        .or_else(|_| std::env::var("DATABASE_URL"))
        .ok()
        .filter(|u| u.starts_with("postgres"))
}

fn backend() -> Option<PgBackend> {
    match pg_url() {
        Some(url) => Some(PgBackend::new_paired(&url)),
        None => {
            if std::env::var("CI").as_deref() == Ok("true") {
                panic!("CI=true but no DATABASE_URL — the postgres job must not silently skip");
            }
            eprintln!(
                "skipping: no DATABASE_URL/AREEV_PG_URL (start one: docker run --rm -d \
                 -p 5432:5432 -e POSTGRES_PASSWORD=postgres pgvector/pgvector:pg16)"
            );
            None
        }
    }
}

// The whole case list, a second time, with every memory split across two
// schemas. Forks, replication, PITR, holds, the registry segment, trigger
// state, read-only opens, the in-run recall ceiling through a real Runner —
// none of it may notice.
macro_rules! paired_case {
    ($name:ident) => {
        #[test]
        fn $name() {
            let Some(b) = backend() else { return };
            cases::$name(&b);
        }
    };
}

areev_conformance::for_each_conformance_case!(paired_case);

fn q(url: &str, sql: String) -> i64 {
    pg::query_raw_i64(url, &sql).unwrap_or_else(|e| panic!("{sql}: {e}"))
}

fn quoted_list(tables: &[&str]) -> String {
    tables.iter().map(|t| format!("'{t}'")).collect::<Vec<_>>().join(",")
}

fn schema_exists(url: &str, schema: &str) -> bool {
    q(
        url,
        format!("SELECT count(*) FROM information_schema.schemata WHERE schema_name = '{schema}'"),
    ) == 1
}

/// The classification, proved by introspection after real use (issue #353,
/// acceptance 1 and 5): every `META_TABLES` entry is in the metadata schema
/// and NONE is in the memory schema; every other table the backend creates
/// is in the memory schema and none in the metadata schema; and the `meta`
/// rows a deployment produces — stamps, declarations, a saved query, a hold,
/// a trigger lease, telemetry — all sit where the pair says.
#[test]
fn engine_metadata_lives_only_in_the_metadata_schema() {
    let Some(b) = backend() else { return };
    let url = pg_url().unwrap();
    let name = "split";
    let schema = b.schema_for(name);
    let meta = b.meta_schema_for(name).unwrap();
    {
        let mut m = Areev::open_postgres_with_telemetry(&b.url_for(name), &schema, TelemetryMode::Aggregate)
            .unwrap();
        m.add(&fact("ns", "ana", "prefers", "quiet rooms")).unwrap();
        m.add(&fact("ns.sub", "ben", "prefers", "tea")).unwrap();
        assert_eq!(m.recall("ns", "ana", Some("prefers"), 4).unwrap().len(), 1);
        m.meta_put("qry:latest", r#"{"body":"SELECT * FROM ns LIMIT 1"}"#).unwrap();
        m.place_hold("ns", "litigation", "legal-ops", 1_700_000_000_000).unwrap();
        assert!(m.meta_cas("trg:abc", None, "{}").unwrap());
        m.telemetry_flush().unwrap();
    }

    let meta_list = quoted_list(META_TABLES);
    assert_eq!(
        q(&url, format!(
            "SELECT count(*) FROM information_schema.tables \
             WHERE table_schema = '{schema}' AND table_name IN ({meta_list})"
        )),
        0,
        "no classified metadata table may be left in the memory schema"
    );
    assert_eq!(
        q(&url, format!(
            "SELECT count(*) FROM information_schema.tables \
             WHERE table_schema = '{meta}' AND table_name NOT IN ({meta_list})"
        )),
        0,
        "no memory table may land in the metadata schema"
    );
    assert_eq!(
        q(&url, format!("SELECT count(*) FROM information_schema.tables WHERE table_schema = '{meta}'")),
        META_TABLES.len() as i64,
        "with telemetry on, the metadata schema holds exactly the classified set"
    );
    let memory_tables: Vec<&str> = PG_TABLES.iter().copied().filter(|t| !META_TABLES.contains(t)).collect();
    assert_eq!(
        q(&url, format!(
            "SELECT count(*) FROM information_schema.tables \
             WHERE table_schema = '{schema}' AND table_name IN ({})",
            quoted_list(&memory_tables)
        )),
        memory_tables.len() as i64,
        "every memory table is in the memory schema"
    );

    // The meta KEYS: stamps, declarations, registry, hold, lease — all here.
    for k in ["pg_schema", "link_index", "ns_registry", "text_index", "entity_relations", "qry:latest", "trg:abc"] {
        assert_eq!(
            q(&url, format!("SELECT count(*) FROM \"{meta}\".meta WHERE k = '{k}'")),
            1,
            "meta key {k} must be in the metadata schema"
        );
    }
    assert_eq!(q(&url, format!("SELECT count(*) FROM \"{meta}\".meta WHERE k LIKE 'hold:%'")), 1);
    assert_eq!(q(&url, format!("SELECT count(*) FROM \"{meta}\".counters")), 3);
    assert_eq!(q(&url, format!("SELECT count(*) FROM \"{meta}\".ns_reg")), 2, "two namespaces registered");
    assert_eq!(
        q(&url, format!("SELECT count(*) FROM \"{meta}\".telem_meta WHERE k = 'schema_version'")),
        1
    );
    assert_eq!(q(&url, format!("SELECT count(*) FROM \"{schema}\".grains")), 2);
    assert_eq!(q(&url, format!("SELECT count(*) FROM \"{schema}\".terms")), q(&url, format!("SELECT count(*) FROM \"{schema}\".terms")));

    // And the memory reopens on the fast path (stamp read from the metadata
    // schema) fully usable, holds included.
    let mut m = b.open_named(name);
    assert_eq!(m.recall("ns", "ana", Some("prefers"), 4).unwrap().len(), 1);
    let held = m.hold_records().unwrap();
    assert_eq!(held.len(), 1, "the hold placed before reopen is still in force");
    let h = m.add(&fact("ns", "cal", "prefers", "coffee")).unwrap();
    let e = m.forget(&h).unwrap_err();
    assert_eq!(e.code(), "STO-E009", "{e}");
}

/// Two pairs in one database share no registry and no counters (acceptance
/// 5, "two-memory registry isolation").
#[test]
fn two_pairs_in_one_database_share_no_registry_or_counters() {
    let Some(b) = backend() else { return };
    let url = pg_url().unwrap();
    let mut a = b.open_named("pair_a");
    let mut c = b.open_named("pair_c");
    a.add(&fact("alpha", "s1", "r", "o1")).unwrap();
    a.add(&fact("alpha", "s2", "r", "o2")).unwrap();
    c.add(&fact("gamma", "s1", "r", "o1")).unwrap();
    assert_eq!(a.namespaces().unwrap(), vec![("alpha".to_string(), 2)]);
    assert_eq!(c.namespaces().unwrap(), vec![("gamma".to_string(), 1)]);
    assert_eq!(a.count().unwrap(), 2);
    assert_eq!(c.count().unwrap(), 1);
    let (ma, mc) = (b.meta_schema_for("pair_a").unwrap(), b.meta_schema_for("pair_c").unwrap());
    assert_eq!(q(&url, format!("SELECT v FROM \"{ma}\".counters WHERE name = 'seq'")), 2);
    assert_eq!(q(&url, format!("SELECT v FROM \"{mc}\".counters WHERE name = 'seq'")), 1);
    assert_eq!(q(&url, format!("SELECT count(*) FROM \"{ma}\".ns_reg")), 1);
    assert_eq!(q(&url, format!("SELECT count(*) FROM \"{mc}\".ns_reg")), 1);
    // A hold on one pair binds nothing in the other.
    a.place_hold("alpha", "litigation", "legal-ops", 1).unwrap();
    let h = c.add(&fact("gamma", "s2", "r", "o2")).unwrap();
    c.forget(&h).expect("a hold in pair A must not reach pair C");
}

/// An open never changes a layout (acceptance 2, "the source of truth isn't
/// duplicated"): a paired DSN over a single-schema memory, and a bare DSN
/// over a paired memory, are both refused with `STO-E011` before any DDL —
/// on read-write, `provision=never` and read-only opens alike — and the
/// memory stays exactly as it was.
#[test]
fn an_open_never_changes_a_layout() {
    let Some(b) = backend() else { return };
    let url = pg_url().unwrap();

    // Single-schema memory, then every kind of paired open over it.
    let single = b.schema_for("was_single");
    let single_meta = b.meta_schema_for("was_single").unwrap();
    {
        let mut m = Areev::open_postgres(&url, &single).unwrap();
        m.add(&fact("ns", "ana", "prefers", "quiet rooms")).unwrap();
    }
    let paired_url = b.url_for("was_single");
    let e = Areev::open_postgres(&paired_url, &single).err().expect("the open must be refused");
    assert_eq!(e.code(), "STO-E011", "{e}");
    assert!(e.to_string().contains("single-schema layout"), "{e}");
    let e = Areev::open_postgres(&format!("{paired_url}&provision=never"), &single).err().expect("the open must be refused");
    assert_eq!(e.code(), "STO-E011", "provision=never must refuse the same way: {e}");
    let e = Areev::open_postgres_with(
        &paired_url,
        &single,
        AreevOptions { read_only: true, ..Default::default() },
    ).err().expect("the open must be refused");
    assert_eq!(e.code(), "STO-E011", "a read-only open must refuse too: {e}");
    assert!(!schema_exists(&url, &single_meta), "the refusal must create no metadata schema");
    assert_eq!(
        q(&url, format!("SELECT count(*) FROM \"{single}\".meta WHERE k = 'pg_schema'")),
        1,
        "and must leave the single memory's own metadata untouched"
    );
    let mut m = Areev::open_postgres(&url, &single).unwrap();
    assert_eq!(m.recall("ns", "ana", Some("prefers"), 4).unwrap().len(), 1);
    drop(m);

    // Paired memory, then a bare open over it.
    let paired = b.schema_for("was_paired");
    {
        let mut m = b.open_named("was_paired");
        m.add(&fact("ns", "ben", "prefers", "tea")).unwrap();
    }
    let e = Areev::open_postgres(&url, &paired).err().expect("the open must be refused");
    assert_eq!(e.code(), "STO-E011", "{e}");
    assert!(e.to_string().contains("no meta table"), "{e}");
    let sep = if url.contains('?') { '&' } else { '?' };
    let e = Areev::open_postgres(&format!("{url}{sep}provision=never"), &paired).err().expect("the open must be refused");
    assert_eq!(e.code(), "STO-E011", "{e}");
    let e = Areev::open_postgres_with(&url, &paired, AreevOptions { read_only: true, ..Default::default() }).err().expect("the open must be refused");
    assert_eq!(e.code(), "STO-E011", "{e}");
    assert_eq!(
        q(&url, format!(
            "SELECT count(*) FROM information_schema.tables \
             WHERE table_schema = '{paired}' AND table_name IN ({})",
            quoted_list(META_TABLES)
        )),
        0,
        "the refusal must not have bootstrapped a second set of engine metadata"
    );
    let mut m = b.open_named("was_paired");
    assert_eq!(m.recall("ns", "ben", Some("prefers"), 4).unwrap().len(), 1);
}

/// `provision=never`, `--read-only` and `provision --check` on a pair
/// (acceptance 4, "`provision=never` parity"): the stamp is read where the
/// pair keeps it, an absent pair is `STO-E008` naming both schemas and the
/// pair-shaped provision command, a read-only open verifies both schemas and
/// creates neither.
#[test]
fn provision_never_read_only_and_check_work_on_a_pair() {
    let Some(b) = backend() else { return };
    let url = pg_url().unwrap();
    let name = "never_pair";
    let schema = b.schema_for(name);
    let meta = b.meta_schema_for(name).unwrap();
    let never = format!("{}&provision=never", b.url_for(name));

    // Absent pair: refused, nothing created, the operator told the pair-shaped command.
    let e = Areev::open_postgres(&never, &schema).err().expect("the open must be refused");
    assert_eq!(e.code(), "STO-E008", "{e}");
    assert!(e.to_string().contains(&meta) && e.to_string().contains("--meta-schema"), "{e}");
    assert!(!schema_exists(&url, &schema) && !schema_exists(&url, &meta));
    let report = pg::check_provision(&b.url_for(name), &schema, TelemetryMode::Off).unwrap();
    assert_eq!(report.meta_schema.as_deref(), Some(meta.as_str()));
    assert!(!report.exists && !report.is_current());

    // Read-only against the absent pair: STO-E005, and still nothing created.
    let e = Areev::open_postgres_with(&b.url_for(name), &schema, AreevOptions { read_only: true, ..Default::default() }).err().expect("the open must be refused");
    assert_eq!(e.code(), "STO-E005", "{e}");
    assert!(!schema_exists(&url, &schema) && !schema_exists(&url, &meta));

    // Provisioned (an owning open), the SAME never-DSN opens and writes…
    drop(b.open_named(name));
    let mut m = Areev::open_postgres(&never, &schema).unwrap();
    m.add(&fact("ns", "cal", "state", "ok")).unwrap();
    assert_eq!(m.recall("ns", "cal", Some("state"), 4).unwrap().len(), 1);
    drop(m);
    // …the read-only open reads and refuses to write…
    let mut ro = Areev::open_postgres_with(&b.url_for(name), &schema, AreevOptions { read_only: true, ..Default::default() })
        .unwrap();
    assert_eq!(ro.recall("ns", "cal", Some("state"), 4).unwrap().len(), 1);
    let e = ro.add(&fact("ns", "dee", "state", "no")).unwrap_err();
    assert_eq!(e.code(), "STO-E004", "{e}");
    drop(ro);
    // …and the probe reports the pair current, with no version movement.
    let report = pg::check_provision(&b.url_for(name), &schema, TelemetryMode::Off).unwrap();
    assert!(report.exists && report.is_current(), "{report:?}");
    assert_eq!(report.rolling_deploy, "safe");
    assert_eq!(report.meta_schema.as_deref(), Some(meta.as_str()));
}

/// Erasing a pair drops BOTH schemas in one go, after reading the holds from
/// where the pair keeps them (acceptance 2, "retention, export/restore and
/// rollback" route through the metadata schema).
#[test]
fn dropping_a_pair_drops_both_schemas_and_honours_holds_in_the_metadata_schema() {
    let Some(b) = backend() else { return };
    let url = pg_url().unwrap();
    let name = "drop_pair";
    let schema = b.schema_for(name);
    let meta = b.meta_schema_for(name).unwrap();
    {
        let mut m = b.open_named(name);
        m.add(&fact("ns", "ana", "prefers", "quiet rooms")).unwrap();
        m.place_hold("ns", "litigation", "legal-ops", 1).unwrap();
    }
    let e = pg::drop_postgres_schema(&b.url_for(name), &schema).unwrap_err();
    assert_eq!(e.code(), "STO-E009", "a hold in the metadata schema must bind the drop: {e}");
    assert!(schema_exists(&url, &schema) && schema_exists(&url, &meta), "a refused drop drops nothing");
    pg::drop_postgres_schema_with(
        &b.url_for(name),
        &schema,
        &pg::DropOptions {
            override_hold: Some(areev_store::HoldOverride::new("legal-ops", "litigation closed").unwrap()),
        },
    )
    .unwrap();
    assert!(!schema_exists(&url, &schema), "memory schema dropped");
    assert!(!schema_exists(&url, &meta), "metadata schema dropped with it");
}

/// Multiple writers per PAIR: id blocks are claimed from the counters row in
/// the metadata schema, and the serialisation it provides is unchanged.
#[test]
fn concurrent_writers_all_land_on_a_pair() {
    let Some(b) = backend() else { return };
    let name = "mw_pair";
    let schema = b.schema_for(name);
    let pair_url = b.url_for(name);
    drop(b.open_named(name));
    let writer = |tag: &'static str| {
        let pair_url = pair_url.clone();
        let schema = schema.clone();
        std::thread::spawn(move || {
            let mut m = Areev::open_postgres(&pair_url, &schema).unwrap();
            for i in 0..25 {
                m.add(&fact("ns", &format!("{tag}{i}"), "writes", "ok")).unwrap();
            }
        })
    };
    let (t1, t2) = (writer("a"), writer("b"));
    t1.join().unwrap();
    t2.join().unwrap();
    let mut m = b.open_named(name);
    assert_eq!(m.count().unwrap(), 50, "every concurrent write must land");
    let ops = m.changes_since(0, 1000).unwrap();
    assert_eq!(ops.len(), 50);
    for (i, op) in ops.iter().enumerate() {
        assert_eq!(op.op_seq, i as i64 + 1, "op-log must be gapless and ordered");
    }
}

/// Rewrite a DSN's userinfo, so a test can connect as a role it just created
/// (the query string — and with it `meta_schema=` — rides along).
fn as_role(url: &str, user: &str, password: &str) -> Option<String> {
    let (scheme, rest) = url.split_once("://")?;
    let authority_len = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let at = rest[..authority_len].rfind('@')?;
    Some(format!("{scheme}://{user}:{password}@{}", &rest[at + 1..]))
}

/// The runtime role for a pair (acceptance 4): granted on BOTH schemas, no
/// DDL, `provision=never`, it opens, reads and writes; revoked on EITHER
/// schema, it can no longer use the memory — and never gets to create a
/// replacement for what it lost.
#[test]
fn a_least_privilege_role_needs_both_schemas_of_a_pair() {
    let Some(b) = backend() else { return };
    let url = pg_url().unwrap();
    let name = "lowpriv_pair";
    let schema = b.schema_for(name);
    let meta = b.meta_schema_for(name).unwrap();
    {
        let mut m = b.open_named(name);
        m.add(&fact("ns", "ana", "prefers", "quiet rooms")).unwrap();
    }
    let role = format!("areev_lpp_{}", std::process::id());
    let cleanup = format!("DROP OWNED BY {role}; DROP ROLE IF EXISTS {role};");
    let _ = pg::execute_raw(&url, &cleanup);
    if pg::execute_raw(&url, &format!("CREATE ROLE {role} LOGIN PASSWORD 'lowpriv'")).is_err() {
        eprintln!("skipping a_least_privilege_role_needs_both_schemas_of_a_pair: this DSN's role may not CREATE ROLE");
        return;
    }
    let x = |sql: String| pg::execute_raw(&url, &sql).unwrap();
    for s in [&schema, &meta] {
        x(format!("GRANT USAGE ON SCHEMA \"{s}\" TO {role}"));
        x(format!("GRANT SELECT, INSERT, UPDATE, DELETE ON ALL TABLES IN SCHEMA \"{s}\" TO {role}"));
        x(format!("GRANT USAGE ON ALL SEQUENCES IN SCHEMA \"{s}\" TO {role}"));
    }
    let Some(low) = as_role(&format!("{}&provision=never", b.url_for(name)), &role, "lowpriv") else {
        eprintln!("skipping: DATABASE_URL carries no userinfo to rewrite");
        let _ = pg::execute_raw(&url, &cleanup);
        return;
    };

    // Granted on both: a full read-write runtime, no DDL anywhere.
    let mut m = Areev::open_postgres(&low, &schema).expect("a current pair opens read-write for a non-owning role");
    assert_eq!(m.recall("ns", "ana", Some("prefers"), 4).unwrap().len(), 1);
    m.add(&fact("ns", "ben", "prefers", "tea")).unwrap();
    assert_eq!(m.recall("ns", "ben", Some("prefers"), 4).unwrap().len(), 1);
    drop(m);

    // Revoked on the metadata schema: the very first thing an open does —
    // read the stamp — is refused, so the open fails; nothing is created.
    x(format!("REVOKE USAGE ON SCHEMA \"{meta}\" FROM {role}"));
    let e = Areev::open_postgres(&low, &schema).err().expect("without the metadata schema the pair is unusable");
    assert!(e.to_string().contains("42501"), "a permission refusal, not a bootstrap: {e}");
    x(format!("GRANT USAGE ON SCHEMA \"{meta}\" TO {role}"));

    // Revoked on the memory schema: the stamp still reads, but the memory
    // itself does not.
    x(format!("REVOKE USAGE ON SCHEMA \"{schema}\" FROM {role}"));
    let r = Areev::open_postgres(&low, &schema).and_then(|mut m| m.recall("ns", "ana", Some("prefers"), 4));
    let e = r.expect_err("without the memory schema nothing can be read");
    assert!(e.to_string().contains("42501"), "{e}");

    let _ = pg::execute_raw(&url, &cleanup);
}
