//! honesty_metrics — the memory numbers incumbents won't publish (LR-1 / SP-5).
//!
//! M1  idempotency        → byte-identical grains collapse to one content
//!                          address; replay / sync / re-import can't duplicate.
//! M2  staleness-rate     → after K updates, recall surfaces exactly one
//!                          current value (0 stale), full history retained.
//! M3  write-cost         → structural writes cost 0 LLM calls / 0 tokens / $0.
//! M4  provenance         → every grain traces to when + how it entered, and
//!                          derived facts to the Event they came from.
//!
//! All four are structural and deterministic: no LLM, no network, no
//! competitor hosting. That is the whole point — these are cheap to reproduce
//! and impossible to fudge, unlike the LLM-judge accuracy scores the category
//! fights over (LoCoMo's answer key is ~6% wrong; every vendor claims SOTA on
//! a dataset it picked). Contrast rows cite primary GitHub issues by number,
//! not our adjectives.
//!
//! Run: `cargo run --release -p areev-bench --bin honesty_metrics`
//! Exit 0 = every gate holds (CI-gate shaped, like trust_suite).

use areev_core::types::{Fact, Grain};
use areev_store::{Areev, AreevOptions, FactDraft};
use std::collections::HashSet;
use std::time::Instant;

/// index_text=false: honesty metrics are structural; skip the FTS write tax.
fn opts() -> AreevOptions {
    AreevOptions { index_text: false, ..Default::default() }
}

/// A Fact with an explicit created_at, so content addresses are deterministic
/// (created_at is in the .mg header + payload, hence in the hash).
fn mk(ns: &str, s: &str, r: &str, o: &str, ts: i64) -> Fact {
    let mut f = Fact::new(s, r, o).confidence(0.9);
    f.common.namespace = Some(ns.to_string());
    f.common.created_at = Some(ts);
    f
}

/// p50/p99 of a nanosecond sample, in microseconds.
fn pcts_us(mut ns: Vec<u128>) -> (f64, f64) {
    ns.sort_unstable();
    let n = ns.len().max(1);
    let at = |q: f64| ns[((n as f64 * q) as usize).min(n - 1)] as f64 / 1000.0;
    (at(0.5), at(0.99))
}

const BASE_TS: i64 = 1_700_000_000_000; // fixed epoch-ms anchor for determinism

fn main() {
    let dir = tempfile::TempDir::new().unwrap();
    let mut pass = 0usize;
    let mut fail = 0usize;
    let mut verdict = |name: &str, ok: bool, detail: String| {
        println!("{} {name}\n    {detail}\n", if ok { "PASS" } else { "FAIL" });
        if ok {
            pass += 1
        } else {
            fail += 1
        }
    };

    // ===================== M1: idempotency =====================
    // mem0 #4573: a hallucinated fact "User prefers Vim" was re-extracted and
    // stored 808 times (97.8% of a 10k store was junk). Content addressing
    // makes byte-identical re-storage a no-op: the same grain is the same
    // address, and the UNIQUE(hash) index rejects the second write.
    println!("## M1 — idempotency (byte-identical replay cannot duplicate)\n");
    let m1 = dir.path().join("m1.db");
    let mut m = Areev::open_with(m1.to_str().unwrap(), opts()).unwrap();
    let (mut stored, mut rejected) = (0usize, 0usize);
    for _ in 0..808 {
        // identical content AND identical created_at ⇒ identical blob ⇒ one hash
        match m.add(&mk("main", "user:alice", "prefers", "vim", BASE_TS)) {
            Ok(_) => stored += 1,
            Err(_) => rejected += 1, // UNIQUE(hash) constraint
        }
    }
    let grains = m.recall("main", "user:alice", None, 64).unwrap().len();
    verdict(
        "M1 idempotency — 808 byte-identical writes settle to one grain",
        grains == 1,
        format!(
            "808 identical writes → {stored} stored / {rejected} rejected on content address; \
             recall returns {grains} grain. This is what makes bundle import / op-log replay / \
             retried sync idempotent. (Scope: identical content incl. timestamp — NOT a paraphrase \
             deduper; near-duplicate phrasings need the write-time novelty gate, roadmap SP-1.)"
        ),
    );
    drop(m);

    // ===================== M2: staleness-rate =====================
    // mem0 #5330: after an update, the stale value keeps co-ranking in top-k
    // ("still on Postgres six weeks after they migrated to MySQL"). mem0 #4536:
    // an update can delete the old memory and store nothing (empty memory).
    // Areev's supersede() is atomic and flips the old triple out of the
    // current view (cur=0); entity_latest holds exactly one current value.
    println!("## M2 — staleness-rate (updates never co-rank stale)\n");
    let m2 = dir.path().join("m2.db");
    let mut m = Areev::open_with(m2.to_str().unwrap(), opts()).unwrap();

    // correct path: a chain of K supersessions (agent re-learns / migrates).
    let k = 20i64;
    let vals = ["postgres", "mysql", "mysql", "cockroach", "sqlite"];
    let mut head = m.add(&mk("main", "svc:api", "uses", "postgres", BASE_TS)).unwrap();
    let mut latest_present = true;
    for i in 1..=k {
        let v = vals[(i as usize) % vals.len()];
        let mut nf = mk("main", "svc:api", "uses", v, BASE_TS + i);
        head = m.supersede(&head, &mut nf).unwrap();
        latest_present &= m.latest("main", "svc:api", "uses").unwrap().is_some();
    }
    let recall_current = m.recall("main", "svc:api", None, 64).unwrap().len();
    let history_depth = m.history("main", "svc:api", "uses").unwrap().len();

    // naive path (what an append-only vector store does): K+1 plain adds, no
    // supersede — every version stays "current" and co-ranks in recall.
    for i in 0..=k {
        let _ = m.add(&mk("main", "svc:naive", "uses", &format!("db-{i}"), BASE_TS + 1000 + i));
    }
    let recall_naive = m.recall("main", "svc:naive", None, 64).unwrap().len();

    verdict(
        "M2 staleness-rate — supersession keeps recall to one current value",
        recall_current == 1 && history_depth as i64 == k + 1 && latest_present && recall_naive as i64 == k + 1,
        format!(
            "{k} updates via supersede → recall surfaces {recall_current} current value (0 stale), \
             {history_depth}-deep history retained & queryable, latest() always present (the #4536 \
             empty-memory bug is structurally impossible). The same {} versions as naive appends → \
             recall surfaces all {recall_naive} (the #5330 stale-co-ranks failure). Cost of the clean \
             path: an index-layer flip, 0 LLM calls.",
            k + 1
        ),
    );
    drop(m);

    // ===================== M3: write-cost =====================
    // openwalrus teardown: mem0 does two LLM calls per write (extract+decide),
    // ~200 calls / $0.30-0.80 per 100-turn chat; #2813: memory.add() ~20s.
    // An Areev structural write is a deterministic serialize + index txn.
    println!("## M3 — write-cost (structural writes: 0 LLM calls)\n");
    let m3 = dir.path().join("m3.db");
    let mut m = Areev::open_with(m3.to_str().unwrap(), opts()).unwrap();

    // amortized batch throughput (the voice write-back path)
    let n = 10_000usize;
    let facts: Vec<Fact> = (0..n)
        .map(|i| mk("main", &format!("caller:{:04}", i % 500), "note", &format!("v{i}"), BASE_TS + i as i64))
        .collect();
    let t = Instant::now();
    for chunk in facts.chunks(500) {
        let refs: Vec<&dyn areev_store::AddableDyn> = chunk.iter().map(|f| f as &dyn areev_store::AddableDyn).collect();
        m.add_batch(&refs).unwrap();
    }
    let elapsed = t.elapsed();
    let us_per_write = elapsed.as_micros() as f64 / n as f64;
    let per_sec = n as f64 / elapsed.as_secs_f64();

    // single-write latency distribution
    let mut lat = Vec::with_capacity(2000);
    for i in 0..2000i64 {
        let f = mk("main", "caller:single", "tick", &format!("t{i}"), BASE_TS + 2_000_000 + i);
        let t0 = Instant::now();
        m.add(&f).unwrap();
        lat.push(t0.elapsed().as_nanos());
    }
    let (p50, p99) = pcts_us(lat);
    verdict(
        "M3 write-cost — sub-millisecond, zero inference",
        us_per_write < 1000.0 && p50 < 1000.0,
        format!(
            "10k structural writes: {us_per_write:.1}µs/write amortized ({per_sec:.0}/s); single-add \
             p50 {p50:.1}µs / p99 {p99:.1}µs — 0 LLM calls, 0 tokens, $0. A 1000-turn agent session \
             does 0 memory-management LLM calls in structural mode vs ~2000 for extract-on-every-write \
             (mem0: 2 calls/write, ~$0.30-0.80 per 100-turn chat, 20s add #2813)."
        ),
    );
    drop(m);

    // ===================== M4: provenance-completeness =====================
    // mem0 #4573: developers hand-build a `memory_sources` table just to see
    // which conversation produced a suspect memory. In Areev every write lands
    // in the op-log (op type + HLC + content address); derived facts carry
    // derived_from → their source Observation; supersession chains reconstruct.
    println!("## M4 — provenance-completeness (every grain is traceable)\n");
    let m4 = dir.path().join("m4.db");
    let mut m = Areev::open_with(m4.to_str().unwrap(), opts()).unwrap();
    let mut stored_hashes: HashSet<String> = HashSet::new();

    // (a) plain facts
    for i in 0..500i64 {
        let h = m.add(&mk("main", &format!("u{i}"), "prefers", &format!("o{i}"), BASE_TS + i)).unwrap();
        stored_hashes.insert(h.to_hex());
    }
    // (b) derived facts via remember() + a deterministic stub extractor (no LLM)
    let extract = |_c: &str| {
        vec![FactDraft { subject: "user:carol".into(), relation: "role".into(), object: "admin".into(), confidence: 0.9 }]
    };
    let rr = m
        .remember("main", "carol was promoted to admin", "agent:hr", Some(&extract as &dyn Fn(&str) -> Vec<FactDraft>))
        .unwrap();
    let event_hex = rr.event.to_hex();
    stored_hashes.insert(event_hex.clone());
    let (mut derived_ok, derived_total) = (0usize, rr.facts.len());
    for h in &rr.facts {
        stored_hashes.insert(h.to_hex());
        let g = m.get(h).unwrap();
        if g.get_str("derived_from") == Some(event_hex.as_str()) && g.get_str("source_type") == Some("derived") {
            derived_ok += 1;
        }
    }
    // (c) supersession lineage
    let h_trial = m.add(&mk("main", "user:dave", "status", "trial", BASE_TS)).unwrap();
    let mut paid = mk("main", "user:dave", "status", "paid", BASE_TS + 1);
    let h_paid = m.supersede(&h_trial, &mut paid).unwrap();
    stored_hashes.insert(h_trial.to_hex());
    stored_hashes.insert(h_paid.to_hex());
    let lineage = m.history("main", "user:dave", "status").unwrap().len();

    // op-log coverage: every stored grain has an audit record
    let ops = m.changes_since(0, 1_000_000).unwrap();
    let oplog: HashSet<String> = ops.iter().map(|o| o.hash.to_hex()).collect();
    let covered = stored_hashes.iter().filter(|h| oplog.contains(*h)).count();
    let coverage = covered as f64 / stored_hashes.len() as f64 * 100.0;

    verdict(
        "M4 provenance-completeness — traceable by construction",
        (coverage - 100.0).abs() < f64::EPSILON && derived_ok == derived_total && lineage == 2,
        format!(
            "{coverage:.0}% of {} grains carry an op-log record (op type + HLC + content address); \
             {derived_ok}/{derived_total} derived facts trace to their source Observation via \
             derived_from; supersession lineage reconstructs {lineage}-deep. This is the \
             memory_sources table mem0 users hand-build (#4573) — native, complete, free.",
            stored_hashes.len()
        ),
    );
    drop(m);

    println!("honesty_metrics: {pass} passed, {fail} failed");
    if fail > 0 {
        std::process::exit(1);
    }
}
