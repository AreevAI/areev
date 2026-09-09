//! The store's own recovery from a prepared statement the backend no longer
//! knows (#181), in its own binary because the chaos form it uses
//! (`DEALLOCATE ALL` before every statement outside a transaction) is process-
//! wide and would break any concurrent test's transaction.
//!
//! Outside a transaction a `26000` is retried after re-preparing. Inside one
//! it cannot be — the failed Bind aborted the transaction — and the error
//! must say what a deployment needs (a pooler that tracks prepared
//! statements, or session mode) rather than read like a driver bug.
#![cfg(feature = "postgres")]

use areev_conformance::{Backend, PgBackend};

fn pg_url() -> Option<String> {
    std::env::var("AREEV_PG_URL")
        .or_else(|_| std::env::var("DATABASE_URL"))
        .ok()
        .filter(|u| u.starts_with("postgres"))
}

#[test]
fn stale_statements_are_retried_outside_a_transaction_and_explained_inside_one() {
    let Some(url) = pg_url() else {
        if std::env::var("CI").as_deref() == Ok("true") {
            panic!("CI=true but no DATABASE_URL — the postgres stale-statement job must not skip");
        }
        eprintln!("skipping: no DATABASE_URL/AREEV_PG_URL");
        return;
    };
    // Open and write with no chaos: the hot statements get prepared and
    // cached, and nothing has deallocated them yet.
    std::env::remove_var("AREEV_PG_SESSION_CHAOS");
    let b = PgBackend::new(&url);
    let mut m = b.open_named("stale");
    for i in 0..5 {
        m.add(&areev_conformance::fact("ns", &format!("s{i}"), "is", "here")).unwrap();
    }
    // Reads outside a transaction: every statement is preceded by
    // DEALLOCATE ALL, so every cached hot statement is stale on arrival and
    // must be re-prepared — invisibly.
    std::env::set_var("AREEV_PG_SESSION_CHAOS", "deallocate");
    for i in 0..5 {
        assert_eq!(m.recall("ns", &format!("s{i}"), Some("is"), 4).unwrap().len(), 1);
        assert!(m.latest("ns", &format!("s{i}"), "is").unwrap().is_some());
    }
    assert_eq!(m.count().unwrap(), 5);
    // The backend-switch shape: the statements go away AT transaction start.
    // A write whose first statement runs outside the transaction (an `add`'s
    // existence probe) heals the cache and lands; one whose first cached
    // statement is inside the transaction trips, and the error must name the
    // remedy. Which of the two a given write is depends on its statement
    // order, so the contract pinned here is: every outcome is one of those,
    // and the handle is never wedged.
    std::env::set_var("AREEV_PG_SESSION_CHAOS", "deallocate-txn");
    let mut landed = 0usize;
    for i in 0..6 {
        match m.add(&areev_conformance::fact("ns", &format!("late{i}"), "is", "here")) {
            Ok(_) => landed += 1,
            Err(e) => {
                let msg = e.to_string();
                assert!(msg.contains("26000"), "{msg}");
                assert!(msg.contains("max_prepared_statements") && msg.contains("session mode"), "{msg}");
            }
        }
    }
    std::env::remove_var("AREEV_PG_SESSION_CHAOS");
    // Not wedged: the next write, unchaosed, lands, and the count reflects
    // exactly what landed under chaos.
    m.add(&areev_conformance::fact("ns", "after", "is", "here")).unwrap();
    assert_eq!(m.count().unwrap(), 5 + landed + 1);
}
