//! min_score_sweep — what a `WITH min_score(t)` floor does to real recall
//! scores, measured on LoCoMo (no LLM, no API key).
//!
//! `SearchHit.score` is rank-normalized RRF: each leg that finds a hit adds
//! `1/(60 + rank)`, and the pool is divided by its best hit, so the top is
//! exactly 1.0. A fixed floor is therefore a RANK cut whose depth depends on
//! how many legs fired: a hit only one of L legs found scores at most 1/L.
//! This bench measures it instead of arguing it. Every turn is ingested as
//! an Event; each question recalls the top-k turns (`recall_hybrid_scored`,
//! default tuning), and for each floor t it reports how many hits survive
//! and whether a gold-evidence turn (LoCoMo `evidence` dia_ids) still does.
//!
//! Two arms, same memory contents:
//!   lexical — BM25 only (the keyless default: one leg)
//!   hybrid  — BM25 + a vector leg: the local TF-IDF+bigram embedder, or
//!             `$AREEV_EMBED_CACHE` (scripts/embed_locomo.py) for a real one
//!
//! Usage (`AREEV_TOPK`, default 20):
//!
//! ```text
//! cargo run --release -p areev-bench --bin min_score_sweep -- locomo10.json [conv_limit]
//! ```

use areev_bench::locomo::{parse_locomo, Conv, TfidfEmbed};
use areev_core::types::Event;
use areev_store::{Areev, AreevOptions, EmbedBackend, RecallTuning};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

const FLOORS: [f32; 12] = [0.0, 0.2, 0.25, 0.3, 0.33, 0.35, 0.4, 0.45, 0.5, 0.55, 0.6, 0.75];

struct CachedEmbed {
    map: Arc<HashMap<String, Vec<f32>>>,
    dim: usize,
}
impl EmbedBackend for CachedEmbed {
    fn dim(&self) -> usize {
        self.dim
    }
    fn embed(&self, text: &str) -> areev_core::error::Result<Vec<f32>> {
        Ok(self.map.get(text).cloned().unwrap_or_else(|| vec![0.0; self.dim]))
    }
    fn model(&self) -> &str {
        "cached"
    }
}

/// Per floor: (questions whose gold survives, hits kept, gold hits kept).
#[derive(Default, Clone)]
struct Tally {
    questions: usize,
    gold_any: Vec<usize>,
    kept: Vec<usize>,
    gold_kept: Vec<usize>,
}

impl Tally {
    fn new() -> Self {
        Tally { questions: 0, gold_any: vec![0; FLOORS.len()], kept: vec![0; FLOORS.len()], gold_kept: vec![0; FLOORS.len()] }
    }
    fn merge(&mut self, o: &Tally) {
        self.questions += o.questions;
        for i in 0..FLOORS.len() {
            self.gold_any[i] += o.gold_any[i];
            self.kept[i] += o.kept[i];
            self.gold_kept[i] += o.gold_kept[i];
        }
    }
}

fn eval(ci: usize, conv: &Conv, dir: &std::path::Path, hybrid: bool, cache: &Option<Arc<HashMap<String, Vec<f32>>>>, topk: usize) -> Tally {
    let db = dir.join(format!("conv{ci}-{}.db", if hybrid { "h" } else { "l" }));
    let mut m = Areev::open_with(db.to_str().unwrap(), AreevOptions { index_text: true, ..Default::default() }).unwrap();
    if hybrid {
        match cache {
            Some(map) => {
                let dim = map.values().next().map(Vec::len).unwrap_or(0);
                m.set_embedder(Box::new(CachedEmbed { map: map.clone(), dim }));
            }
            None => {
                let corpus: Vec<String> = conv.turns.iter().map(|t| t.text.clone()).collect();
                m.set_embedder(Box::new(TfidfEmbed::build(2048, &corpus)));
            }
        }
    }
    let ns = format!("conv{ci}");
    let events: Vec<Event> = conv
        .turns
        .iter()
        .enumerate()
        .map(|(ti, turn)| {
            let mut ev = Event::new(&turn.text).session(turn.dia_id.clone());
            ev.common.namespace = Some(ns.clone());
            ev.common.created_at = Some(1_700_000_000_000 + (ci * 100_000 + ti) as i64);
            ev
        })
        .collect();
    let refs: Vec<&dyn areev_store::AddableDyn> = events.iter().map(|e| e as &dyn areev_store::AddableDyn).collect();
    m.add_batch(&refs).unwrap();

    let mut t = Tally::new();
    for qa in &conv.qa {
        let hits = m
            .recall_hybrid_scored(&ns, None, None, Some(qa.question.as_str()), topk, None, RecallTuning::default())
            .unwrap();
        let gold: HashSet<&str> = qa.evidence.iter().map(String::as_str).collect();
        t.questions += 1;
        for (fi, floor) in FLOORS.iter().enumerate() {
            let kept: Vec<bool> = hits
                .iter()
                .filter(|(_, s)| *s >= *floor)
                .map(|(g, _)| g.get_str("session_id").is_some_and(|d| gold.contains(d)))
                .collect();
            t.kept[fi] += kept.len();
            let g = kept.iter().filter(|b| **b).count();
            t.gold_kept[fi] += g;
            if g > 0 {
                t.gold_any[fi] += 1;
            }
        }
    }
    t
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let path = args.get(1).cloned().unwrap_or_else(|| {
        eprintln!("usage: min_score_sweep <locomo10.json> [conv_limit]");
        std::process::exit(2);
    });
    let limit: usize = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(10);
    let topk: usize = std::env::var("AREEV_TOPK").ok().and_then(|s| s.parse().ok()).unwrap_or(20).clamp(1, 1000);
    let cache: Option<Arc<HashMap<String, Vec<f32>>>> = std::env::var("AREEV_EMBED_CACHE").ok().map(|p| {
        let raw = std::fs::read_to_string(&p).expect("read AREEV_EMBED_CACHE");
        Arc::new(serde_json::from_str(&raw).expect("parse embed cache JSON"))
    });
    let raw = std::fs::read_to_string(&path).expect("read LoCoMo JSON");
    let convs = parse_locomo(&serde_json::from_str(&raw).expect("valid JSON"), limit);
    let dir = tempfile::TempDir::new().unwrap();
    let vector = if cache.is_some() { "precomputed embeddings" } else { "local TF-IDF+bigram" };

    for (label, hybrid) in [("lexical (BM25 only — one leg)", false), ("hybrid (BM25 + vector — two legs)", true)] {
        let results: Vec<Tally> = std::thread::scope(|sc| {
            let handles: Vec<_> = convs
                .iter()
                .enumerate()
                .map(|(ci, c)| {
                    let (d, cache) = (dir.path(), &cache);
                    sc.spawn(move || eval(ci, c, d, hybrid, cache, topk))
                })
                .collect();
            handles.into_iter().map(|h| h.join().unwrap()).collect()
        });
        let mut all = Tally::new();
        for r in &results {
            all.merge(r);
        }
        let q = all.questions.max(1) as f64;
        println!("\n## {label}{}, k={topk}, {} questions\n", if hybrid { format!(" [{vector}]") } else { String::new() }, all.questions);
        println!("| min_score | gold survives | hits kept / question | gold share of kept |");
        println!("|---|---|---|---|");
        for (fi, floor) in FLOORS.iter().enumerate() {
            let share = if all.kept[fi] == 0 { 0.0 } else { all.gold_kept[fi] as f64 / all.kept[fi] as f64 };
            println!(
                "| {floor:.2} | {:.1}% | {:.1} | {:.1}% |",
                100.0 * all.gold_any[fi] as f64 / q,
                all.kept[fi] as f64 / q,
                100.0 * share
            );
        }
    }
}
