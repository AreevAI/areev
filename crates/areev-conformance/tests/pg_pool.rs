//! #181, the pool: N memories in one process hold at most `?pool=`
//! connections — and #229, the reaper: a pool that has gone quiet gives them
//! back, and then goes itself.
#![cfg(feature = "postgres")]

use areev_conformance::{fact, Backend, PgBackend};
use areev_store::Areev;

fn pg_url() -> Option<String> {
    std::env::var("AREEV_PG_URL")
        .or_else(|_| std::env::var("DATABASE_URL"))
        .ok()
        .filter(|u| u.starts_with("postgres"))
}

fn with_param(url: &str, p: &str) -> String {
    if url.contains('?') { format!("{url}&{p}") } else { format!("{url}?{p}") }
}

/// This test's own connections carry a unique `application_name`, so the
/// count is not polluted by the other test binaries cargo runs in parallel
/// against the same server (each is its own process and its own pool) — nor
/// by the other test in THIS binary, which cargo runs on a parallel thread
/// against the same registry. `application_name` is not one of the store's
/// parameters, so a DSN naming one is its own pool.
fn app_name(test: &str) -> String {
    format!("areev_pool_{test}_{}", std::process::id())
}

/// Every connection this process holds under `test`'s name — or under any
/// name extending it, which is how the reaping test spells one pool per
/// tenant. Counted from a probe that names itself differently (and pools
/// separately).
fn live(url: &str, test: &str) -> i64 {
    let sep = if url.contains('?') { "&" } else { "?" };
    let probe = format!("{url}{sep}application_name=areev-probe&pool=1");
    areev_store::pg::query_raw_i64(
        &probe,
        &format!(
            "SELECT count(*) FROM pg_stat_activity \
              WHERE application_name LIKE '{}%' AND datname = current_database()",
            app_name(test)
        ),
    )
    .unwrap()
}

#[test]
fn n_memories_hold_at_most_p_connections() {
    let Some(url) = pg_url() else {
        eprintln!("skipping: no DATABASE_URL/AREEV_PG_URL");
        return;
    };
    // `pool_idle_secs=0`: this test is about the cap, so nothing may be
    // reaped under it (#229).
    let pooled = with_param(
        &url,
        &format!("pool=3&pool_idle_secs=0&application_name={}", app_name("cap")),
    );
    let b = PgBackend::new(&pooled);

    // Twelve handles, telemetry on — twenty-four connections before #181.
    let mut handles: Vec<Areev> = (0..12)
        .map(|i| {
            let schema = b.schema_for(&format!("pool_{i}"));
            Areev::open_postgres_with_telemetry(&pooled, &schema, areev_store::TelemetryMode::Aggregate)
                .unwrap()
        })
        .collect();
    for (i, m) in handles.iter_mut().enumerate() {
        m.add(&fact("ns", &format!("s{i}"), "r", "o")).unwrap();
        assert_eq!(m.count().unwrap(), 1);
    }
    assert!(live(&url, "cap") <= 3, "the cap is the cap: {} live", live(&url, "cap"));

    // Overlapping transactions from more threads than the cap: everything
    // lands, nothing crosses, the count never exceeds the cap.
    let writers: Vec<_> = (0..6)
        .map(|t| {
            let schema = b.schema_for(&format!("pool_w{t}"));
            let url = pooled.clone();
            std::thread::spawn(move || {
                let mut m = Areev::open_postgres(&url, &schema).unwrap();
                for i in 0..20 {
                    m.add(&fact("ns", &format!("w{t}_{i}"), "writes", &format!("t{t}"))).unwrap();
                }
                m.count().unwrap()
            })
        })
        .collect();
    let mut peak = 0;
    for _ in 0..20 {
        peak = peak.max(live(&url, "cap"));
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    for w in writers {
        assert_eq!(w.join().unwrap(), 20);
    }
    assert!(peak <= 3, "peak {peak} connections under a cap of 3");
    for t in 0..6 {
        let mut m = b.open_named(&format!("pool_w{t}"));
        assert_eq!(m.count().unwrap(), 20);
        assert_eq!(
            m.latest("ns", &format!("w{t}_7"), "writes").unwrap().unwrap().get_str("object"),
            Some(format!("t{t}").as_str())
        );
    }
    drop(handles);

    // A later open asking for another size is told, not obeyed.
    let m = Areev::open_postgres(&with_param(&pooled, "pool=9"), &b.schema_for("pool_late")).unwrap();
    assert!(
        m.open_warnings().iter().any(|w| w.contains("sized at 3")),
        "{:?}",
        m.open_warnings()
    );
}

/// #229: the pool gives its connections back, and then goes itself.
///
/// In the topology the issue was measured on, `?pool=` bounds nothing: a
/// host that gives every tenant its own ROLE has one POOL per tenant, and
/// eight memories opened and closed left eight connections held whatever
/// `?pool=` said. This reproduces that shape — the pool key is the DSN, so
/// eight DSNs differing in a non-store parameter are eight pools, exactly as
/// eight roles would be — and then asserts the property the issue asks for:
/// with no handle open, an idle memory holds nothing.
#[test]
fn idle_connections_are_reaped_and_the_pool_goes_with_them() {
    let Some(url) = pg_url() else {
        eprintln!("skipping: no DATABASE_URL/AREEV_PG_URL");
        return;
    };
    let ttl = std::time::Duration::from_secs(3);
    // One DSN — and therefore one pool — per memory.
    let dsn = |i: usize| {
        with_param(
            &url,
            &format!("pool=8&pool_idle_secs=3&application_name={}_{i}", app_name("reap")),
        )
    };
    let b = PgBackend::new(&url);

    let mut handles: Vec<Areev> = (0..8)
        .map(|i| Areev::open_postgres(&dsn(i), &b.schema_for(&format!("reap_{i}"))).unwrap())
        .collect();
    for (i, m) in handles.iter_mut().enumerate() {
        m.add(&fact("ns", &format!("s{i}"), "r", "o")).unwrap();
        assert_eq!(m.count().unwrap(), 1);
    }
    // One cheap read each, so all eight connections are freshly idle and the
    // count below is not racing the TTL of whichever memory was opened first.
    for m in handles.iter_mut() {
        m.count().unwrap();
    }
    assert_eq!(live(&url, "reap"), 8, "one pool per tenant means one connection per tenant");
    drop(handles);

    // Still held right after close — the pool keeps them for the TTL, which
    // is the whole point of pooling. This is the state the issue measured:
    // eight memories closed, eight connections retained.
    assert_eq!(
        live(&url, "reap"),
        8,
        "a closed handle returns its connection to the pool, it does not close it"
    );

    // …and given back after it. Generously bounded: the reaper looks every
    // half-TTL, and a loaded CI box is allowed to be slow.
    let deadline = std::time::Instant::now() + ttl * 10;
    while live(&url, "reap") > 0 && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    assert_eq!(
        live(&url, "reap"),
        0,
        "an idle memory must hold NO connection once the TTL has passed"
    );
    // The pools themselves went with them, so the process is not
    // accumulating one runtime (and its worker threads) per role either.
    for i in 0..8 {
        assert!(
            !areev_store::pg::pool_is_registered(&dsn(i)),
            "a pool holding nothing, that nothing holds, must not outlive the sweep"
        );
    }

    // Reopening after the reaping is an ordinary open: it works, it sees
    // what was written, and it says nothing about any of this.
    let mut m = Areev::open_postgres(&dsn(3), &b.schema_for("reap_3")).unwrap();
    assert_eq!(m.count().unwrap(), 1);
    assert_eq!(
        m.latest("ns", "s3", "r").unwrap().unwrap().get_str("object"),
        Some("o"),
        "the memory is unchanged by its pool's lifetime"
    );
    assert!(m.open_warnings().is_empty(), "{:?}", m.open_warnings());
}
