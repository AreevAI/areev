//! decide_calibrate — how well a decision backend's probabilities are
//! calibrated, on a labeled JSONL set (`docs/decision-model-proposal.md`
//! phase 2). One row per line:
//!
//! ```json
//! {"state": "…" | {…} | […], "question": {<one wire question>}, "label": <gold>}
//! ```
//!
//! `label` is the correct option key for a `choice`, `0`/`1` (or
//! `false`/`true`) for a `noul`, and the level index for a `score`. Each row
//! is one decision request (question id `q`) to the chain `AREEV_DECIDE` /
//! `AREEV_DECIDE_CMD` / `AREEV_DECIDE_TIMEOUT_MS` names (`areev_llm::env_chain`).
//!
//! Prints, per provider that answered (a chain may answer from several
//! entries) and per question type: n, accuracy (argmax), ECE (10 equal-width
//! bins over the top-label confidence), and Brier (summed over classes). For
//! `noul` rows it also prints the p(yes) threshold that maximizes F1 and the
//! two-band suggestion at 5% error tolerance — DROP below `t_lo` (at most 5%
//! of the dropped rows were actually yes), KEEP at or above `t_hi` (at most 5%
//! of the kept rows were actually no), defer to the deterministic rule in
//! between. Bands are only usable for OMISSION when the backend is
//! calibrated (proposal §2 rule 2); the output says so.
//!
//! Deterministic: rows run sequentially in file order; the only wall-clock
//! figure is the final total-latency line. A row whose request fails is
//! reported and the run exits non-zero — a calibration over a smaller
//! denominator than the file is refused, not averaged.
//!
//!   AREEV_DECIDE=openrouter:jev-latest \
//!     cargo run --release -p areev-bench --bin decide_calibrate -- \
//!     crates/areev-bench/data/decide_calibrate_sample.jsonl

use std::collections::BTreeMap;

use areev_core::decide::{Answer, DecideRequest, Question};
use serde_json::Value;

/// One scored row: the class distribution, the gold class, and (noul only)
/// p(yes) with its 0/1 label.
struct Scored {
    kind: &'static str,
    probs: Vec<f32>,
    gold: usize,
    noul: Option<(f32, bool)>,
}

fn argmax(p: &[f32]) -> usize {
    let mut best = 0;
    for (i, x) in p.iter().enumerate() {
        if *x > p[best] {
            best = i;
        }
    }
    best
}

/// Top-label ECE over 10 equal-width confidence bins.
fn ece(rows: &[&Scored]) -> f64 {
    let mut bins = [(0.0f64, 0.0f64, 0usize); 10]; // (sum conf, sum correct, n)
    for r in rows {
        let a = argmax(&r.probs);
        let conf = r.probs[a] as f64;
        let b = ((conf * 10.0) as usize).min(9);
        bins[b].0 += conf;
        bins[b].1 += (a == r.gold) as u8 as f64;
        bins[b].2 += 1;
    }
    let n = rows.len().max(1) as f64;
    bins.iter()
        .filter(|b| b.2 > 0)
        .map(|b| (b.0 / b.2 as f64 - b.1 / b.2 as f64).abs() * b.2 as f64 / n)
        .sum()
}

/// Multi-class Brier: mean over rows of Σ_k (p_k − 1[k = gold])².
fn brier(rows: &[&Scored]) -> f64 {
    let n = rows.len().max(1) as f64;
    rows.iter()
        .map(|r| {
            r.probs
                .iter()
                .enumerate()
                .map(|(k, p)| {
                    let y = (k == r.gold) as u8 as f64;
                    (*p as f64 - y).powi(2)
                })
                .sum::<f64>()
        })
        .sum::<f64>()
        / n
}

/// The p(yes) threshold (predict yes when p ≥ t) maximizing F1 on the yes
/// class; ties go to the higher threshold. `None` when no row is yes.
fn best_f1(rows: &[(f32, bool)]) -> Option<(f32, f64)> {
    if !rows.iter().any(|r| r.1) {
        return None;
    }
    let mut ts: Vec<f32> = rows.iter().map(|r| r.0).collect();
    ts.sort_by(|a, b| a.partial_cmp(b).unwrap());
    ts.dedup();
    let mut best: Option<(f32, f64)> = None;
    for t in ts {
        let tp = rows.iter().filter(|r| r.0 >= t && r.1).count() as f64;
        let fp = rows.iter().filter(|r| r.0 >= t && !r.1).count() as f64;
        let fneg = rows.iter().filter(|r| r.0 < t && r.1).count() as f64;
        let f1 = if tp == 0.0 { 0.0 } else { 2.0 * tp / (2.0 * tp + fp + fneg) };
        if best.is_none_or(|b| f1 >= b.1) {
            best = Some((t, f1));
        }
    }
    best
}

/// The two bands at `tol` error: the HIGHEST `t_lo` such that rows with
/// p < t_lo are at most `tol` yes, and the LOWEST `t_hi` such that rows with
/// p ≥ t_hi are at most `tol` no. Candidate thresholds are the observed p
/// values (plus 1.01 for "keep nothing"). Returns (t_lo, dropped, t_hi, kept).
fn two_bands(rows: &[(f32, bool)], tol: f64) -> (f32, usize, f32, usize) {
    let mut ts: Vec<f32> = rows.iter().map(|r| r.0).collect();
    ts.push(1.01);
    ts.sort_by(|a, b| a.partial_cmp(b).unwrap());
    ts.dedup();
    let (mut lo, mut dropped) = (0.0f32, 0usize);
    for &t in &ts {
        let band: Vec<_> = rows.iter().filter(|r| r.0 < t).collect();
        let err = band.iter().filter(|r| r.1).count() as f64 / band.len().max(1) as f64;
        if err <= tol {
            (lo, dropped) = (t, band.len());
        }
    }
    let (mut hi, mut kept) = (1.01f32, 0usize);
    for &t in ts.iter().rev() {
        let band: Vec<_> = rows.iter().filter(|r| r.0 >= t).collect();
        let err = band.iter().filter(|r| !r.1).count() as f64 / band.len().max(1) as f64;
        if err <= tol {
            (hi, kept) = (t, band.len());
        }
    }
    (lo, dropped, hi, kept)
}

/// Turn one answer into a distribution over classes in a fixed order
/// (noul: [no, yes]; choice: option keys sorted; score: level index).
fn score_row(q: &Question, a: &Answer, label: &Value) -> Result<Scored, String> {
    match (q, a) {
        (Question::Noul { .. }, Answer::Noul { p }) => {
            let y = match label {
                Value::Bool(b) => *b,
                v => match v.as_u64() {
                    Some(0) => false,
                    Some(1) => true,
                    _ => return Err(format!("noul label must be 0/1 or a bool, got {v}")),
                },
            };
            Ok(Scored { kind: "noul", probs: vec![1.0 - p, *p], gold: y as usize, noul: Some((*p, y)) })
        }
        (Question::Choice { criteria, .. }, Answer::Choice { probabilities, .. }) => {
            let keys: Vec<&String> = criteria.keys().collect();
            let want = label.as_str().ok_or_else(|| format!("choice label must be an option key, got {label}"))?;
            let gold = keys.iter().position(|k| *k == want).ok_or_else(|| format!("label {want:?} is not an option"))?;
            let probs = keys.iter().map(|k| probabilities.get(*k).copied().unwrap_or(0.0)).collect();
            Ok(Scored { kind: "choice", probs, gold, noul: None })
        }
        (Question::Score { levels, .. }, Answer::Score { probabilities, .. }) => {
            let gold = label.as_u64().map(|g| g as usize).filter(|g| *g < levels.len())
                .ok_or_else(|| format!("score label must be a level index < {}, got {label}", levels.len()))?;
            let probs = (0..levels.len()).map(|i| probabilities.get(&i.to_string()).copied().unwrap_or(0.0)).collect();
            Ok(Scored { kind: "score", probs, gold, noul: None })
        }
        _ => Err("answer type does not match the question".into()),
    }
}

fn main() {
    let path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "crates/areev-bench/data/decide_calibrate_sample.jsonl".to_string());
    let chain = match areev_llm::env_chain() {
        Ok(Some(c)) => c,
        Ok(None) => {
            eprintln!("decide_calibrate: set AREEV_DECIDE (and/or AREEV_DECIDE_CMD) to the chain to calibrate");
            std::process::exit(2);
        }
        Err(e) => {
            eprintln!("decide_calibrate: {e}");
            std::process::exit(2);
        }
    };
    let raw = std::fs::read_to_string(&path).unwrap_or_else(|e| {
        eprintln!("cannot read {path}: {e}");
        std::process::exit(2);
    });
    let started = std::time::Instant::now();
    // provider -> (model, calibrated, rows)
    let mut by_provider: BTreeMap<String, (String, bool, Vec<Scored>)> = BTreeMap::new();
    let (mut total, mut failed) = (0usize, 0usize);
    let (mut tok_in, mut tok_out) = (0u64, 0u64);
    for (ln, line) in raw.lines().enumerate().filter(|(_, l)| !l.trim().is_empty()) {
        total += 1;
        let fail = |m: String| {
            println!("  row {}: FAILED — {m}", ln + 1);
        };
        let row: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(e) => {
                fail(format!("not JSON: {e}"));
                failed += 1;
                continue;
            }
        };
        let q = match Question::from_wire("q", &row["question"]) {
            Ok(q) => q,
            Err(e) => {
                fail(e.to_string());
                failed += 1;
                continue;
            }
        };
        let req = DecideRequest::new(row["state"].clone(), [("q".to_string(), q.clone())].into_iter().collect());
        let d = match chain.decide(&req) {
            Ok(d) => d,
            Err(e) => {
                fail(e.to_string());
                failed += 1;
                continue;
            }
        };
        tok_in += d.input_tokens.unwrap_or(0);
        tok_out += d.output_tokens.unwrap_or(0);
        match score_row(&q, &d.answers["q"], &row["label"]) {
            Ok(s) => by_provider
                .entry(d.provider.clone())
                .or_insert_with(|| (d.model.clone(), d.calibrated, Vec::new()))
                .2
                .push(s),
            Err(e) => {
                fail(e);
                failed += 1;
            }
        }
    }

    println!("decide_calibrate: {total} rows from {path}");
    println!("chain: `{}` (calibrated = {})\n", chain.describe(), chain.calibrated());
    println!("| provider | model | calibrated | type | n | accuracy | ECE@10 | Brier |");
    println!("|---|---|---|---|---:|---:|---:|---:|");
    for (provider, (model, cal, rows)) in &by_provider {
        let mut kinds: Vec<&str> = rows.iter().map(|r| r.kind).collect();
        kinds.sort();
        kinds.dedup();
        let mut groups: Vec<(String, Vec<&Scored>)> =
            kinds.iter().map(|k| (k.to_string(), rows.iter().filter(|r| r.kind == *k).collect())).collect();
        groups.push(("all".into(), rows.iter().collect()));
        for (kind, g) in groups {
            let acc = g.iter().filter(|r| argmax(&r.probs) == r.gold).count() as f64 / g.len().max(1) as f64;
            println!(
                "| {provider} | {model} | {cal} | {kind} | {} | {:.3} | {:.3} | {:.3} |",
                g.len(),
                acc,
                ece(&g),
                brier(&g)
            );
        }
    }
    for (provider, (_, cal, rows)) in &by_provider {
        let noul: Vec<(f32, bool)> = rows.iter().filter_map(|r| r.noul).collect();
        if noul.is_empty() {
            continue;
        }
        println!("\nnoul thresholds — {provider} (n = {}):", noul.len());
        match best_f1(&noul) {
            Some((t, f1)) => println!("  best F1: predict yes at p ≥ {t:.3} (F1 = {f1:.3})"),
            None => println!("  best F1: undefined (no yes rows)"),
        }
        let (lo, dropped, hi, kept) = two_bands(&noul, 0.05);
        let between = noul.iter().filter(|r| r.0 >= lo && r.0 < hi).count();
        println!(
            "  two bands at 5% error: DROP p < {lo:.3} ({dropped} rows), KEEP p ≥ {hi:.3} ({kept} rows), \
             defer the {between} in between to the rule"
        );
        if lo > hi {
            println!("  (the bands overlap: the 5% tolerance is met on both sides of the overlap; use the KEEP band first)");
        }
        if !cal {
            println!("  NOTE: uncalibrated backend — these bands may REORDER but never OMIT (proposal §2 rule 2)");
        }
    }
    println!("\nprovider-reported tokens: {tok_in} in / {tok_out} out");
    println!("total latency: {:.2} s", started.elapsed().as_secs_f64());
    if failed > 0 {
        println!("\nREFUSED: {failed} of {total} rows failed — a calibration over a partial denominator is not reported");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(p: &[f32], gold: usize) -> Scored {
        Scored { kind: "choice", probs: p.to_vec(), gold, noul: None }
    }

    #[test]
    fn perfect_confident_answers_have_zero_ece_and_brier() {
        let a = s(&[1.0, 0.0], 0);
        let b = s(&[0.0, 1.0], 1);
        let rows = vec![&a, &b];
        assert_eq!(ece(&rows), 0.0);
        assert_eq!(brier(&rows), 0.0);
    }

    #[test]
    fn overconfident_wrong_answers_score_badly() {
        let a = s(&[0.9, 0.1], 1);
        let rows = vec![&a];
        assert!((ece(&rows) - 0.9).abs() < 1e-6);
        assert!((brier(&rows) - (0.81 + 0.81)).abs() < 1e-5);
    }

    #[test]
    fn f1_threshold_and_bands_on_a_separable_set() {
        let rows = [(0.05, false), (0.1, false), (0.2, false), (0.8, true), (0.9, true), (0.95, true)];
        let (t, f1) = best_f1(&rows).unwrap();
        assert_eq!((t, f1), (0.8, 1.0));
        let (lo, dropped, hi, kept) = two_bands(&rows, 0.05);
        assert_eq!((lo, dropped), (0.8, 3), "everything below the first yes drops cleanly");
        assert_eq!((hi, kept), (0.8, 3));
    }

    #[test]
    fn bands_widen_the_middle_when_classes_overlap() {
        let rows = [(0.1, false), (0.4, true), (0.5, false), (0.9, true)];
        let (lo, dropped, hi, kept) = two_bands(&rows, 0.05);
        assert_eq!((lo, dropped), (0.4, 1));
        assert_eq!((hi, kept), (0.9, 1));
    }
}
