//! areev-bench — adoption benchmark harnesses.
//!
//! Shared helpers: deterministic dataset generation and percentile
//! reporting. The binaries are the benchmarks:
//!   frame_chart — recall latency across real surfaces vs the 50ms
//!                 audio-frame budget (in-process / HTTP / MCP stdio)
//!   trust_suite — durability + integrity artifacts (kill -9 recovery,
//!                 tamper detection, deletion-remnant scan, PITR restore)

use areev_core::types::{Fact, Grain};
use areev_store::{AddableDyn, Areev};

pub mod selfimprove;

/// Human/reference verdict for one surfaced finding. Abstention is represented
/// by the absence of a verdict, so it scores zero rather than as an error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    UsefulCorrect,
    Wrong,
}

/// Effective-Reliability report shared by offline harnesses.
#[derive(Debug, Clone, PartialEq)]
pub struct Reliability {
    pub surfaced: usize,
    pub useful_correct: usize,
    pub wrong: usize,
    pub er: f64,
    pub precision: f64,
    pub recall: f64,
    pub spurious_rate: f64,
}

/// Score surfaced findings against the number of planted/known positives.
/// Wrong confident findings subtract; abstention contributes zero.
pub fn effective_reliability(verdicts: &[Verdict], positives: usize) -> Reliability {
    let surfaced = verdicts.len();
    let useful_correct = verdicts.iter().filter(|v| **v == Verdict::UsefulCorrect).count();
    let wrong = verdicts.iter().filter(|v| **v == Verdict::Wrong).count();
    let denominator = positives.max(1) as f64;
    let er = (useful_correct as f64 - wrong as f64) / denominator;
    let precision = if surfaced > 0 {
        useful_correct as f64 / surfaced as f64
    } else {
        1.0
    };
    let recall = useful_correct as f64 / denominator;
    let spurious_rate = if surfaced > 0 {
        wrong as f64 / surfaced as f64
    } else {
        0.0
    };
    Reliability {
        surfaced,
        useful_correct,
        wrong,
        er,
        precision,
        recall,
        spurious_rate,
    }
}

pub const RELS: [&str; 6] = ["prefers", "lives_in", "speaks", "allergic_to", "reports_to", "status"];

/// Deterministic xorshift so every engine/surface sees the identical
/// dataset and query workload.
pub struct Xorshift(pub u64);
impl Xorshift {
    #[allow(clippy::should_implement_trait)] // deterministic RNG step, not an Iterator
    pub fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
}

/// The bench.rs dataset shape: `n` facts over `subjects` callers.
pub fn gen_facts(rng: &mut Xorshift, n: usize, subjects: u64) -> Vec<(String, &'static str, String)> {
    (0..n)
        .map(|_| {
            let s = format!("caller:{:04}", rng.next() % subjects);
            let r = RELS[(rng.next() % RELS.len() as u64) as usize];
            let o = format!("value-{:05}", rng.next() % 5000);
            (s, r, o)
        })
        .collect()
}

/// Load facts into a store in 500-grain batches under namespace `main`.
pub fn load_facts(m: &mut Areev, facts: &[(String, &'static str, String)]) {
    let mut batch: Vec<Fact> = Vec::new();
    for (s, r, o) in facts {
        let mut f = Fact::new(s, r, o).confidence(0.9);
        f.common.namespace = Some("main".to_string());
        batch.push(f);
        if batch.len() == 500 {
            let refs: Vec<&dyn AddableDyn> = batch.iter().map(|f| f as &dyn AddableDyn).collect();
            m.add_batch(&refs).unwrap();
            batch.clear();
        }
    }
    if !batch.is_empty() {
        let refs: Vec<&dyn AddableDyn> = batch.iter().map(|f| f as &dyn AddableDyn).collect();
        m.add_batch(&refs).unwrap();
    }
}

pub struct Pcts {
    pub p50: f64,
    pub p95: f64,
    pub p99: f64,
}

/// Sort + report p50/p95/p99 in µs; prints a markdown row.
pub fn pct(mut ns: Vec<u128>, name: &str) -> Pcts {
    ns.sort_unstable();
    let n = ns.len().max(1);
    let pick = |q: f64| ns[((n as f64 * q) as usize).min(n - 1)] as f64 / 1000.0;
    let p = Pcts { p50: pick(0.5), p95: pick(0.95), p99: pick(0.99) };
    println!("| {name} | {:.1} | {:.1} | {:.1} |", p.p50, p.p95, p.p99);
    p
}

/// Locate the release `areev` binary for surface benches that drive the
/// real CLI (honors CARGO_TARGET_DIR, falls back to the workspace target).
pub fn areev_bin() -> std::path::PathBuf {
    let target = std::env::var("CARGO_TARGET_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| {
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target")
        });
    target.join("release/areev")
}

#[cfg(test)]
mod reliability_tests {
    use super::*;

    #[test]
    fn er_subtracts_for_wrong() {
        let verdicts = [
            Verdict::UsefulCorrect,
            Verdict::UsefulCorrect,
            Verdict::UsefulCorrect,
            Verdict::UsefulCorrect,
            Verdict::Wrong,
        ];
        let report = effective_reliability(&verdicts, 5);
        assert!((report.er - 0.6).abs() < 1e-9);
        assert!((report.precision - 0.8).abs() < 1e-9);
        assert!((report.recall - 0.8).abs() < 1e-9);
        assert!((report.spurious_rate - 0.2).abs() < 1e-9);
    }

    #[test]
    fn er_can_go_negative_when_mostly_wrong() {
        let verdicts = [
            Verdict::UsefulCorrect,
            Verdict::Wrong,
            Verdict::Wrong,
            Verdict::Wrong,
        ];
        let report = effective_reliability(&verdicts, 4);
        assert!((report.er + 0.5).abs() < 1e-9);
    }

    #[test]
    fn perfect_abstention_scores_zero_not_negative() {
        let report = effective_reliability(&[], 3);
        assert_eq!(report.er, 0.0);
        assert_eq!(report.surfaced, 0);
    }
}
