//! #181, the pool: N memories in one process hold at most `?pool=` connections.
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

/// This process's own connections carry a unique `application_name`, so the
/// count is not polluted by the other test binaries cargo runs in parallel
/// against the same server (each is its own process and its own pool).
fn app_name() -> String {
    format!("areev_pool_{}", std::process::id())
}

/// Every connection this process holds, counted from a probe that names
/// itself differently (and pools separately).
fn live(url: &str) -> i64 {
    let sep = if url.contains('?') { "&" } else { "?" };
    let probe = format!("{url}{sep}application_name=areev-probe&pool=1");
    areev_store::pg::query_raw_i64(
        &probe,
        &format!(
            "SELECT count(*) FROM pg_stat_activity \
              WHERE application_name = '{}' AND datname = current_database()",
            app_name()
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
    let pooled = with_param(&url, &format!("pool=3&application_name={}", app_name()));
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
    assert!(live(&url) <= 3, "the cap is the cap: {} live", live(&url));

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
        peak = peak.max(live(&url));
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
