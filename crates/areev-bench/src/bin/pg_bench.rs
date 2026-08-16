//! Server-tier (Postgres backend) latency measurement — the honest
//! counterpart to the embedded `bench` example, same dataset shape (10k
//! facts / 800 subjects, seeded xorshift) so the two tables are comparable.
//!
//! Run: DATABASE_URL=postgres://… cargo run --release -p areev-bench \
//!        --features postgres --bin pg_bench
//!
//! The numbers depend on the network between the process and the server —
//! report the topology with them (localhost container ≠ same-VPC ≠
//! cross-AZ). The only hard gate is a sanity bound (recall p50 < 5ms),
//! deliberately loose: this backend's contract is millisecond-class
//! turn-level recall, not the embedded microsecond class.

#[cfg(feature = "postgres")]
fn main() {
    use areev_bench::Xorshift;
    use areev_core::types::{Fact, Grain};
    use areev_store::{AddableDyn, Areev};
    use std::time::Instant;

    let url = std::env::var("AREEV_PG_URL")
        .or_else(|_| std::env::var("DATABASE_URL"))
        .expect("set AREEV_PG_URL or DATABASE_URL");
    // NOT `pg_…` — Postgres reserves schema names with that prefix (42939).
    let schema = format!("bench_pg_{}", std::process::id());
    let mut m = Areev::open_postgres(&url, &schema).expect("open postgres store");

    let rels = ["prefers", "lives_in", "speaks", "allergic_to", "reports_to", "status"];
    let mut rng = Xorshift(42);
    let t0 = Instant::now();
    let mut batch: Vec<Fact> = Vec::with_capacity(500);
    let mut loaded = 0usize;
    while loaded < 10_000 {
        batch.clear();
        for _ in 0..500 {
            let s = format!("person{}", rng.next() % 800);
            let r = rels[(rng.next() % rels.len() as u64) as usize];
            let o = format!("value {} for the record", rng.next() % 4096);
            let mut f = Fact::new(&s, r, &o).confidence(0.9);
            f.common.namespace = Some("bench".into());
            batch.push(f);
        }
        let refs: Vec<&dyn AddableDyn> = batch.iter().map(|f| f as &dyn AddableDyn).collect();
        m.add_batch(&refs).expect("batch load");
        loaded += 500;
    }
    println!("loaded 10k facts in {:.1}s (schema {schema})", t0.elapsed().as_secs_f64());

    fn pct(mut ns: Vec<u128>, name: &str, target_us: f64) -> bool {
        ns.sort_unstable();
        let n = ns.len().max(1);
        let pick = |q: f64| ns[((n as f64 * q) as usize).min(n - 1)] as f64 / 1000.0;
        let (p50, p95, p99) = (pick(0.5), pick(0.95), pick(0.99));
        let ok = p50 <= target_us;
        let verdict = if ok { "PASS" } else { "FAIL" };
        println!("| {name} | {p50:.0} | {p95:.0} | {p99:.0} | {target_us:.0} | {verdict} |");
        ok
    }

    println!("| pg_bench (postgres backend) | p50 µs | p95 µs | p99 µs | target | verdict |");
    println!("|---|---|---|---|---|---|");
    let mut all_ok = true;

    // structural recall, k<=16 (probe+blob join = one round trip)
    let mut lat = Vec::with_capacity(2000);
    for i in 0..2300u64 {
        let s = format!("person{}", rng.next() % 800);
        let t = Instant::now();
        let out = m.recall("bench", &s, None, 16).expect("recall");
        if i >= 300 {
            lat.push(t.elapsed().as_nanos());
        }
        std::hint::black_box(out);
    }
    all_ok &= pct(lat, "recall about subject (k<=16, deserialize)", 5000.0);

    // entity_latest point read (2 round trips)
    let mut lat = Vec::with_capacity(2000);
    for i in 0..2300u64 {
        let s = format!("person{}", rng.next() % 800);
        let t = Instant::now();
        let out = m.latest("bench", &s, "prefers").expect("latest");
        if i >= 300 {
            lat.push(t.elapsed().as_nanos());
        }
        std::hint::black_box(out);
    }
    all_ok &= pct(lat, "entity_latest head (full grain)", 3000.0);

    // hybrid free-text recall (BM25 leg + batched candidate fetch)
    let mut lat = Vec::with_capacity(500);
    for i in 0..600u64 {
        let q = format!("value {}", rng.next() % 4096);
        let t = Instant::now();
        let out = m.recall_hybrid("bench", None, None, Some(&q), 8, None).expect("hybrid");
        if i >= 100 {
            lat.push(t.elapsed().as_nanos());
        }
        std::hint::black_box(out);
    }
    all_ok &= pct(lat, "hybrid free-text recall (k=8)", 20000.0);

    // single-grain add (full txn incl. reserve_write serialization)
    let mut lat = Vec::with_capacity(200);
    for i in 0..200u64 {
        let mut f = Fact::new(&format!("writer{}", rng.next()), "notes", "a fresh single add");
        f.common.namespace = Some("bench".into());
        let t = Instant::now();
        m.add(&f).expect("add");
        lat.push(t.elapsed().as_nanos());
        let _ = i;
    }
    all_ok &= pct(lat, "add single grain (full txn)", 20000.0);

    drop(m);
    areev_store::pg::drop_postgres_schema(&url, &schema).expect("drop bench schema");
    std::process::exit(if all_ok { 0 } else { 1 });
}

#[cfg(not(feature = "postgres"))]
fn main() {
    eprintln!("pg_bench needs the postgres feature: cargo run -p areev-bench --features postgres --bin pg_bench");
    std::process::exit(2);
}
