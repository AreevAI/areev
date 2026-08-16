//! Real-store latency check against latency targets (run --release).

use areev_core::types::{Event, Fact, Grain};
use areev_store::{AddableDyn, Areev};
use std::time::Instant;

fn pct(mut ns: Vec<u128>, name: &str, target_us: f64) {
    ns.sort_unstable();
    let n = ns.len().max(1);
    let pick = |q: f64| ns[((n as f64 * q) as usize).min(n - 1)] as f64 / 1000.0;
    let (p50, p95, p99) = (pick(0.5), pick(0.95), pick(0.99));
    let verdict = if p50 <= target_us { "PASS" } else { "FAIL" };
    println!("| {name} | {p50:.1} | {p95:.1} | {p99:.1} | {target_us:.0} | {verdict} |");
}

fn main() {
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("bench.db");
    let mut m = Areev::open(path.to_str().unwrap()).unwrap();

    // load 10k facts over 800 subjects + 3k events over 150 sessions
    let mut x: u64 = 42;
    let mut rng = move || {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        x
    };
    let t0 = Instant::now();
    let rels = ["prefers", "lives_in", "speaks", "allergic_to", "reports_to", "status"];
    let mut batch: Vec<Fact> = Vec::new();
    for _ in 0..10_000 {
        let s = format!("caller:{:04}", rng() % 800);
        let r = rels[(rng() % rels.len() as u64) as usize];
        let o = format!("value-{:05}", rng() % 5000);
        let mut f = Fact::new(&s, r, &o).confidence(0.9);
        f.common.namespace = Some("main".to_string());
        batch.push(f);
        if batch.len() == 500 {
            let refs: Vec<&dyn AddableDyn> = batch.iter().map(|f| f as &dyn AddableDyn).collect();
            m.add_batch(&refs).unwrap();
            batch.clear();
        }
    }
    let mut ev_batch: Vec<Event> = Vec::new();
    for i in 0..3_000u64 {
        let mut e = Event::new(&format!("utterance number {i} about things"));
        e.session_id = Some(format!("call-{:03}", rng() % 150));
        e.common.namespace = Some("main".to_string());
        ev_batch.push(e);
        if ev_batch.len() == 500 {
            let refs: Vec<&dyn AddableDyn> = ev_batch.iter().map(|e| e as &dyn AddableDyn).collect();
            m.add_batch(&refs).unwrap();
            ev_batch.clear();
        }
    }
    println!("loaded 13k grains in {:.2}s", t0.elapsed().as_secs_f64());
    println!("| bench (real store API) | p50 µs | p95 µs | p99 µs | target | verdict |");
    println!("|---|---|---|---|---|---|");

    let mut v = Vec::new();
    for _ in 0..2000 {
        let s = format!("caller:{:04}", rng() % 800);
        let t = Instant::now();
        let r = m.recall("main", &s, None, 16).unwrap();
        v.push(t.elapsed().as_nanos());
        std::hint::black_box(r);
    }
    pct(v, "recall about subject (k<=16, deserialize)", 200.0);

    let mut v = Vec::new();
    for _ in 0..5000 {
        let s = format!("caller:{:04}", rng() % 800);
        let t = Instant::now();
        let r = m.latest("main", &s, "prefers").unwrap();
        v.push(t.elapsed().as_nanos());
        std::hint::black_box(r);
    }
    pct(v, "entity_latest head (full grain)", 100.0);

    let mut v = Vec::new();
    for _ in 0..1000 {
        let sess = format!("call-{:03}", rng() % 150);
        let t = Instant::now();
        let r = m.thread_tail("main", &sess, 20).unwrap();
        v.push(t.elapsed().as_nanos());
        std::hint::black_box(r);
    }
    pct(v, "thread_tail 20 events (deserialize)", 2000.0);

    let mut v = Vec::new();
    for i in 0..500u64 {
        let mut f = Fact::new(&format!("caller:{:04}", i % 800), "note", &format!("n{i}"));
        f.common.namespace = Some("main".to_string());
        let t = Instant::now();
        m.add(&f).unwrap();
        v.push(t.elapsed().as_nanos());
    }
    pct(v, "add single grain (full txn)", 1000.0);
}
