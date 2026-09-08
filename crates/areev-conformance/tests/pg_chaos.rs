//! Postgres runner under session chaos (#181): the IDENTICAL conformance case
//! list, with `RESET ALL` issued by the store itself before every statement
//! outside a transaction. That drops `search_path` and every GUC — what a
//! transaction-mode pooler's backend switch does between transactions — so a
//! green run here is the proof that no statement resolves through the
//! session. (Prepared statements are the pooler's half: the driver names every
//! parameterized statement, so the pooler must track them; PgBouncer 1.21+
//! with `max_prepared_statements > 0` does, and the deployment profile says
//! so. `pg_stale.rs` pins the store's own recovery for the out-of-transaction
//! case.)
//!
//! Same DSN contract as `pg.rs`: skips loudly without `DATABASE_URL`, hard-
//! fails under `CI=true`. The hook only exists on a `conformance` build of
//! `areev-store`, which this crate's `postgres` feature turns on.
#![cfg(feature = "postgres")]

use areev_conformance::{cases, PgBackend};

fn pg_url() -> Option<String> {
    std::env::var("AREEV_PG_URL")
        .or_else(|_| std::env::var("DATABASE_URL"))
        .ok()
        .filter(|u| u.starts_with("postgres"))
}

fn backend() -> Option<PgBackend> {
    // Process-wide, read before every statement; every test in this binary
    // wants it.
    std::env::set_var("AREEV_PG_SESSION_CHAOS", "1");
    match pg_url() {
        Some(url) => Some(PgBackend::new(&url)),
        None => {
            if std::env::var("CI").as_deref() == Ok("true") {
                panic!("CI=true but no DATABASE_URL — the postgres chaos job must not silently skip");
            }
            eprintln!("skipping: no DATABASE_URL/AREEV_PG_URL");
            None
        }
    }
}

macro_rules! pg_chaos_case {
    ($name:ident) => {
        #[test]
        fn $name() {
            let Some(b) = backend() else { return };
            cases::$name(&b);
        }
    };
}

areev_conformance::for_each_conformance_case!(pg_chaos_case);

