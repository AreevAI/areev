//! accuracy — Areev memory accuracy on LoCoMo (LR-1, the Areev self-run).
//!
//! Two legs:
//!
//!   RETRIEVAL HIT-RATE (runs with no LLM / no API key)
//!     Ingest every conversation turn as an Event, then for each question ask
//!     `recall_hybrid` for the top-k turns and check whether a gold-evidence turn
//!     (LoCoMo's `evidence` dia_ids) is in the set — the honest "does the right
//!     memory come back?" number, no LLM-judge machinery. The embedder is
//!     pluggable (`EmbedBackend`): a precomputed real-embedding cache
//!     (`$AREEV_EMBED_CACHE`, produced by scripts/embed_locomo.py) is best; a
//!     local TF-IDF is the no-API fallback.
//!
//!   END-TO-END ANSWER ACCURACY (gated on $AREEV_LLM_CMD)
//!     Each question is answered from the recalled context and an LLM-judge scores
//!     it against gold. Reader and judge are any stdin→stdout command
//!     ($AREEV_LLM_CMD, $AREEV_JUDGE_CMD); scripts/openai_chat.py is a ready
//!     OpenAI adapter. $AREEV_LLM_DEBUG=1 logs every (question, gold, answer,
//!     verdict) tuple. Unset ⇒ this leg is skipped with instructions.
//!
//! RERANKING (optional, retrieval leg only)
//!   `AREEV_DECIDE` (+ `AREEV_DECIDE_CMD`, `AREEV_DECIDE_TIMEOUT_MS`) installs a
//!   `DecisionRerank` over the decision chain those name (the same
//!   `areev_llm::env_chain` MCP and the bindings read) and recalls with
//!   `rerank: true`; the chain's `describe()`, `calibrated`, request/token
//!   counts and failures print beside the table. `--rerank-oracle` (or
//!   `AREEV_RERANK_ORACLE=1`) installs a POSITIVE CONTROL instead: a reranker
//!   scoring the question's gold-evidence turns 1.0 and everything else 0.0.
//!   It must lift hit@k to the pool's own recall — proof the rerank plumbing
//!   reaches the metric before any real reranker's number is quoted.
//!   `AREEV_BENCH_WORKERS` (default 1, max 16) evaluates conversations in
//!   parallel (each on its own memory file; output merges in conversation
//!   order, so it is identical at any worker count); `AREEV_QA_PER_CONV` caps
//!   the questions per conversation for a pilot/cost projection.
//!
//! Usage (best config — raw turns, real embeddings, k=20):
//!   AREEV_EMBED_CACHE=cache.json \
//!   AREEV_LLM_CMD='python3 crates/areev-bench/scripts/openai_chat.py gpt-4o-mini' \
//!   AREEV_JUDGE_CMD='python3 crates/areev-bench/scripts/openai_chat.py gpt-4o' \
//!     cargo run --release -p areev-bench --bin accuracy -- locomo10.json 10

use areev_core::error::Result;
use areev_core::types::Event;
use areev_store::{Areev, AreevOptions, DecisionRerank, DecisionRerankStats, EmbedBackend, RecallTuning, RerankBackend};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::{Arc, Mutex};

// ---------- the rerank positive control ----------
/// Scores a candidate 1.0 when it contains one of the CURRENT question's
/// gold-evidence turns, else 0.0. The harness sets the gold set before each
/// recall. Knows the answer by construction — it proves the plumbing, never
/// a reranker's quality.
struct OracleRerank {
    gold: Arc<Mutex<Vec<String>>>,
}
impl RerankBackend for OracleRerank {
    fn rerank(&self, _query: &str, docs: &[&str]) -> Result<Vec<f32>> {
        let gold = self.gold.lock().unwrap();
        Ok(docs.iter().map(|d| if gold.iter().any(|g| d.contains(g.as_str())) { 1.0 } else { 0.0 }).collect())
    }
    fn model(&self) -> &str {
        "oracle-gold-evidence"
    }
}

#[derive(Clone)]
enum Rerank {
    None,
    Oracle,
    Decision(Arc<dyn areev_core::decide::DecisionBackend>),
}

// ---------- precomputed-embedding backend (real semantic vectors — best) ----------
// Loads a {text -> vector} cache (OpenAI text-embedding-3-small, produced by
// scripts/embed_locomo.py) and looks up by exact text — the harness embeds only
// known turn/question strings, so every lookup hits. Shared via Arc (10s of MB).
struct CachedEmbed {
    map: Arc<HashMap<String, Vec<f32>>>,
    dim: usize,
}
impl EmbedBackend for CachedEmbed {
    fn dim(&self) -> usize {
        self.dim
    }
    fn embed(&self, text: &str) -> Result<Vec<f32>> {
        Ok(self.map.get(text).cloned().unwrap_or_else(|| vec![0.0; self.dim]))
    }
    fn model(&self) -> &str {
        "precomputed-embeddings"
    }
}

// ---------- local TF-IDF (+bigram) embedder (no-API fallback) ----------
struct TfidfEmbed {
    dim: usize,
    idf: HashMap<String, f32>,
    default_idf: f32,
}
impl TfidfEmbed {
    fn build(dim: usize, docs: &[String]) -> Self {
        let n = docs.len().max(1);
        let mut df: HashMap<String, usize> = HashMap::new();
        for d in docs {
            for t in features(d).into_iter().collect::<HashSet<_>>() {
                *df.entry(t).or_insert(0) += 1;
            }
        }
        let idf = df
            .iter()
            .map(|(t, &c)| (t.clone(), ((n as f32 + 1.0) / (c as f32 + 1.0)).ln() + 1.0))
            .collect();
        Self { dim, idf, default_idf: (n as f32 + 1.0).ln() + 1.0 }
    }
}
fn tokenize(s: &str) -> Vec<String> {
    s.chars()
        .map(|c| if c.is_alphanumeric() { c.to_ascii_lowercase() } else { ' ' })
        .collect::<String>()
        .split_whitespace()
        .filter(|w| w.len() > 1)
        .map(|w| w.to_string())
        .collect()
}
/// unigrams + adjacent bigrams
fn features(s: &str) -> Vec<String> {
    let toks = tokenize(s);
    let mut f = toks.clone();
    for w in toks.windows(2) {
        f.push(format!("{}_{}", w[0], w[1]));
    }
    f
}
fn fnv1a(t: &str) -> u64 {
    let mut h = 0xcbf29ce484222325u64;
    for b in t.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}
impl EmbedBackend for TfidfEmbed {
    fn dim(&self) -> usize {
        self.dim
    }
    fn embed(&self, text: &str) -> Result<Vec<f32>> {
        let mut tf: HashMap<String, f32> = HashMap::new();
        for t in features(text) {
            *tf.entry(t).or_insert(0.0) += 1.0;
        }
        let mut v = vec![0f32; self.dim];
        for (t, c) in tf {
            let w = c * self.idf.get(&t).copied().unwrap_or(self.default_idf);
            v[(fnv1a(&t) % self.dim as u64) as usize] += w;
        }
        let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        if norm > 0.0 {
            for x in &mut v {
                *x /= norm;
            }
        }
        Ok(v)
    }
    fn model(&self) -> &str {
        "tfidf-bigram-2048"
    }
}

// ---------- LoCoMo records ----------
struct Turn {
    dia_id: String,
    text: String,
}
struct Qa {
    question: String,
    answer: String,
    evidence: Vec<String>,
    category: i64,
}
struct Conv {
    turns: Vec<Turn>,
    qa: Vec<Qa>,
    dates: HashMap<String, String>, // dia_id -> session date (for temporal resolution)
}

fn parse_locomo(v: &serde_json::Value, limit: usize) -> Vec<Conv> {
    let mut out = Vec::new();
    for sample in v.as_array().unwrap_or(&vec![]).iter().take(limit) {
        let mut turns = Vec::new();
        let mut dates: HashMap<String, String> = HashMap::new();
        if let Some(conv) = sample.get("conversation").and_then(|c| c.as_object()) {
            // keys like session_1, session_2, … (skip session_N_date_time)
            let mut skeys: Vec<&String> = conv
                .keys()
                .filter(|k| k.strip_prefix("session_").map(|r| !r.is_empty() && r.chars().all(|c| c.is_ascii_digit())).unwrap_or(false))
                .collect();
            skeys.sort_by_key(|k| k.strip_prefix("session_").and_then(|r| r.parse::<i64>().ok()).unwrap_or(0));
            for sk in skeys {
                let date = conv
                    .get(&format!("{sk}_date_time"))
                    .and_then(|d| d.as_str())
                    .unwrap_or("")
                    .to_string();
                if let Some(arr) = conv.get(sk).and_then(|s| s.as_array()) {
                    for t in arr {
                        let speaker = t.get("speaker").and_then(|s| s.as_str()).unwrap_or("");
                        let text = t.get("text").and_then(|s| s.as_str()).unwrap_or("");
                        let dia = t.get("dia_id").and_then(|s| s.as_str()).unwrap_or("");
                        if !dia.is_empty() && !text.is_empty() {
                            dates.insert(dia.to_string(), date.clone());
                            turns.push(Turn { dia_id: dia.to_string(), text: format!("{speaker}: {text}") });
                        }
                    }
                }
            }
        }
        let mut qa = Vec::new();
        if let Some(arr) = sample.get("qa").and_then(|q| q.as_array()) {
            for q in arr {
                let evidence: Vec<String> = q
                    .get("evidence")
                    .and_then(|e| e.as_array())
                    .map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_string)).collect())
                    .unwrap_or_default();
                if evidence.is_empty() {
                    continue; // no gold turn to hit — skip (adversarial abstentions)
                }
                let answer = q
                    .get("answer")
                    .and_then(|a| a.as_str())
                    .or_else(|| q.get("adversarial_answer").and_then(|a| a.as_str()))
                    .unwrap_or("")
                    .to_string();
                qa.push(Qa {
                    question: q.get("question").and_then(|s| s.as_str()).unwrap_or("").to_string(),
                    answer,
                    evidence,
                    category: q.get("category").and_then(|c| c.as_i64()).unwrap_or(0),
                });
            }
        }
        out.push(Conv { turns, qa, dates });
    }
    out
}

// one end-to-end task: retrieval is done, only the (parallelizable) LLM
// reader+judge remain.
#[derive(Clone)]
struct Task {
    question: String,
    gold: String,
    category: i64,
    context: String,
}

fn reader_prompt(context: &str, question: &str) -> String {
    format!(
        "Answer a question from a conversation's memory using ONLY the recalled turns \
         below. Each turn is tagged with the date it was said — resolve relative time \
         (yesterday, last week, last month) to an absolute date using those tags. Answer \
         concisely; if the turns don't contain the answer, say so.\n\n\
         Recalled turns:\n{context}\n\nQuestion: {question}\nAnswer:"
    )
}
fn judge_prompt(question: &str, gold: &str, answer: &str) -> String {
    format!(
        "Grade a memory answer for factual correctness against the gold answer. \
         Reply with exactly YES or NO.\n\nQuestion: {question}\nGold answer: {gold}\nModel answer: {answer}\n\nCorrect?"
    )
}

fn run_llm(cmd: &str, prompt: &str) -> Option<String> {
    use std::io::Write;
    use std::process::{Command, Stdio};
    let mut child = Command::new("sh")
        .arg("-c")
        .arg(cmd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    child.stdin.take()?.write_all(prompt.as_bytes()).ok()?;
    let out = child.wait_with_output().ok()?;
    Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// What one conversation contributes to the tables.
#[derive(Default)]
struct ConvResult {
    h1: usize,
    h5: usize,
    h10: usize,
    h_topk: usize,
    mrr: f64,
    evaluated: usize,
    by_cat: BTreeMap<i64, (usize, usize)>,
    tasks: Vec<Task>,
    stats: Option<Arc<DecisionRerankStats>>,
    line: String,
}

#[allow(clippy::too_many_arguments)]
fn eval_conv(
    ci: usize,
    conv: &Conv,
    dir: &std::path::Path,
    embed_cache: &Option<Arc<HashMap<String, Vec<f32>>>>,
    embed_dim: usize,
    topk: usize,
    rerank: &Rerank,
    qa_cap: usize,
    collect_tasks: usize,
) -> ConvResult {
    let mut out = ConvResult::default();
    let db = dir.join(format!("conv{ci}.db"));
    // index_text=false: skip the FTS write tax; the vector leg carries recall.
    let mut m = Areev::open_with(db.to_str().unwrap(), AreevOptions { index_text: false, ..Default::default() }).unwrap();
    match embed_cache {
        Some(map) => m.set_embedder(Box::new(CachedEmbed { map: map.clone(), dim: embed_dim })),
        None => {
            let corpus: Vec<String> = conv.turns.iter().map(|t| t.text.clone()).collect();
            m.set_embedder(Box::new(TfidfEmbed::build(2048, &corpus)));
        }
    }
    let gold_slot = Arc::new(Mutex::new(Vec::<String>::new()));
    match rerank {
        Rerank::None => {}
        Rerank::Oracle => m.set_reranker(Box::new(OracleRerank { gold: gold_slot.clone() })),
        Rerank::Decision(chain) => {
            let r = DecisionRerank::new(chain.clone());
            out.stats = Some(r.stats());
            m.set_reranker(Box::new(r));
        }
    }
    let tuning = RecallTuning { rerank: !matches!(rerank, Rerank::None), ..Default::default() };
    let text_of: HashMap<&str, &str> = conv.turns.iter().map(|t| (t.dia_id.as_str(), t.text.as_str())).collect();
    let ns = format!("conv{ci}");
    for (ti, turn) in conv.turns.iter().enumerate() {
        let mut ev = Event::new(&turn.text).session(turn.dia_id.clone());
        ev.common.namespace = Some(ns.clone());
        ev.common.created_at = Some(1_700_000_000_000 + (ci * 100_000 + ti) as i64);
        m.add(&ev).unwrap();
    }

    for qa in conv.qa.iter().take(qa_cap) {
        *gold_slot.lock().unwrap() =
            qa.evidence.iter().filter_map(|d| text_of.get(d.as_str()).map(|t| t.to_string())).collect();
        let hits = m.recall_hybrid_tuned(&ns, None, None, Some(qa.question.as_str()), topk, None, tuning).unwrap();
        let got: Vec<String> = hits.iter().filter_map(|g| g.get_str("session_id").map(str::to_string)).collect();
        let gold: HashSet<&String> = qa.evidence.iter().collect();
        let rank = got.iter().position(|d| gold.contains(d)); // first hit rank (0-based)
        out.evaluated += 1;
        let e = out.by_cat.entry(qa.category).or_insert((0, 0));
        e.1 += 1;
        if let Some(r) = rank {
            if r < 1 { out.h1 += 1 }
            if r < 5 { out.h5 += 1 }
            if r < 10 { out.h10 += 1; out.mrr += 1.0 / (r as f64 + 1.0); }
            if r < topk { out.h_topk += 1; e.0 += 1; }
        }

        // ---- collect end-to-end task (LLM reader+judge run in parallel below) ----
        if out.tasks.len() < collect_tasks {
            let ctx: String = hits
                .iter()
                .take(topk)
                .filter_map(|g| {
                    let dia = g.get_str("session_id")?;
                    let date = conv.dates.get(dia).map(String::as_str).unwrap_or("");
                    Some(format!("- ({date}) {}", g.get_str("content")?))
                })
                .collect::<Vec<_>>()
                .join("\n");
            out.tasks.push(Task { question: qa.question.clone(), gold: qa.answer.clone(), category: qa.category, context: ctx });
        }
    }
    out.line = format!("  conv{ci}: ingested {} turns, evaluated {} QAs", conv.turns.len(), out.evaluated);
    out
}

fn main() {
    let started = std::time::Instant::now();
    let all_args: Vec<String> = std::env::args().collect();
    let oracle = all_args.iter().any(|a| a == "--rerank-oracle")
        || std::env::var("AREEV_RERANK_ORACLE").map(|v| v == "1").unwrap_or(false);
    let args: Vec<String> = all_args.into_iter().filter(|a| a != "--rerank-oracle").collect();
    let path = args
        .get(1)
        .cloned()
        .or_else(|| std::env::var("AREEV_LOCOMO").ok())
        .unwrap_or_else(|| {
            eprintln!("usage: accuracy <locomo10.json> [conv_limit] [--rerank-oracle]  (or set $AREEV_LOCOMO)");
            std::process::exit(2);
        });
    let limit: usize = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(10);
    let topk: usize = std::env::var("AREEV_TOPK").ok().and_then(|s| s.parse().ok()).unwrap_or(20).clamp(1, 1000);
    let llm_cmd = std::env::var("AREEV_LLM_CMD").ok();
    let judge_cmd = std::env::var("AREEV_JUDGE_CMD").ok().or_else(|| llm_cmd.clone());
    let llm_sample: usize = std::env::var("AREEV_LLM_SAMPLE").ok().and_then(|s| s.parse().ok()).unwrap_or(50);
    let workers: usize = std::env::var("AREEV_BENCH_WORKERS").ok().and_then(|s| s.parse().ok()).unwrap_or(1).clamp(1, 16);
    let qa_cap: usize = std::env::var("AREEV_QA_PER_CONV").ok().and_then(|s| s.parse().ok()).unwrap_or(usize::MAX);
    let decide = areev_llm::env_chain().unwrap_or_else(|e| {
        eprintln!("{e}");
        std::process::exit(2);
    });
    let rerank = match (oracle, decide) {
        (true, Some(_)) => {
            eprintln!("--rerank-oracle and AREEV_DECIDE are exclusive: the oracle is the control, not a chain entry");
            std::process::exit(2);
        }
        (true, None) => Rerank::Oracle,
        (false, Some(chain)) => Rerank::Decision(chain),
        (false, None) => Rerank::None,
    };
    // Real embedder: a precomputed {text: vector} cache, loaded once and shared.
    // Absent ⇒ TF-IDF no-API fallback.
    let embed_cache: Option<Arc<HashMap<String, Vec<f32>>>> = std::env::var("AREEV_EMBED_CACHE")
        .ok()
        .map(|p| {
            let raw = std::fs::read_to_string(&p).expect("read AREEV_EMBED_CACHE");
            let map: HashMap<String, Vec<f32>> = serde_json::from_str(&raw).expect("parse embed cache JSON");
            eprintln!("loaded {} cached embeddings from {p}", map.len());
            Arc::new(map)
        });
    let embed_dim = embed_cache.as_ref().and_then(|m| m.values().next().map(|v| v.len())).unwrap_or(0);

    let raw = std::fs::read_to_string(&path).unwrap_or_else(|e| {
        eprintln!("cannot read {path}: {e}");
        std::process::exit(2);
    });
    let json: serde_json::Value = serde_json::from_str(&raw).expect("valid JSON");
    let convs = parse_locomo(&json, limit);
    let total_turns: usize = convs.iter().map(|c| c.turns.len()).sum();
    let total_qa: usize = convs.iter().map(|c| c.qa.len().min(qa_cap)).sum();
    let embedder_label = match &embed_cache {
        Some(_) => format!("recall_hybrid over precomputed embeddings ({embed_dim}-d)"),
        None => "recall_hybrid over a local TF-IDF+bigram embedder (no API — lexical FLOOR)".to_string(),
    };
    let rerank_label = match &rerank {
        Rerank::None => "none (fusion order)".to_string(),
        Rerank::Oracle => "ORACLE positive control (gold-evidence turns = 1.0)".to_string(),
        Rerank::Decision(c) => format!("DecisionRerank over `{}` (calibrated = {})", c.describe(), c.calibrated()),
    };
    println!(
        "LoCoMo: {} conversation(s), {total_turns} turns, {total_qa} answerable QAs\n\
         retrieval leg: {embedder_label}\n\
         rerank: {rerank_label}\n",
        convs.len()
    );

    let dir = tempfile::TempDir::new().unwrap();
    let collect_tasks = if llm_cmd.is_some() { llm_sample } else { 0 };
    // Conversations are independent memories: evaluate them on `workers`
    // threads, then merge IN CONVERSATION ORDER so the output (and the e2e
    // task sample) is identical at any worker count.
    let next = std::sync::atomic::AtomicUsize::new(0);
    let slots: Vec<Mutex<Option<ConvResult>>> = convs.iter().map(|_| Mutex::new(None)).collect();
    std::thread::scope(|sc| {
        for _ in 0..workers.min(convs.len().max(1)) {
            sc.spawn(|| loop {
                let ci = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let Some(conv) = convs.get(ci) else { break };
                let r = eval_conv(ci, conv, dir.path(), &embed_cache, embed_dim, topk, &rerank, qa_cap, collect_tasks);
                eprintln!("{}", r.line);
                *slots[ci].lock().unwrap() = Some(r);
            });
        }
    });
    // hit-rate accumulators
    let (mut h1, mut h5, mut h10, mut h_topk, mut mrr, mut evaluated) = (0usize, 0usize, 0usize, 0usize, 0.0f64, 0usize);
    let mut by_cat: BTreeMap<i64, (usize, usize)> = Default::default(); // cat -> (hit@topk, n)
    // end-to-end accumulators
    let (mut ans_correct, mut ans_total) = (0usize, 0usize);
    let mut tasks: Vec<Task> = Vec::new(); // e2e tasks collected in pass 1, run in parallel in pass 2
    let mut stats: Vec<Arc<DecisionRerankStats>> = Vec::new();
    for slot in slots {
        let r = slot.into_inner().unwrap().expect("every conversation evaluated");
        println!("{}", r.line);
        h1 += r.h1;
        h5 += r.h5;
        h10 += r.h10;
        h_topk += r.h_topk;
        mrr += r.mrr;
        evaluated += r.evaluated;
        for (cat, (h, n)) in r.by_cat {
            let e = by_cat.entry(cat).or_insert((0, 0));
            e.0 += h;
            e.1 += n;
        }
        for t in r.tasks {
            if tasks.len() < collect_tasks {
                tasks.push(t);
            }
        }
        stats.extend(r.stats);
    }
    // Rule 5: a score over a smaller denominator than the dataset is refused.
    assert_eq!(evaluated, total_qa, "evaluated {evaluated} of {total_qa} QAs — refusing a partial denominator");

    // ---- pass 2: run the collected e2e tasks in parallel (reader + judge) ----
    if let Some(reader) = llm_cmd.clone() {
        if !tasks.is_empty() {
            let judge = judge_cmd.clone().unwrap_or_else(|| reader.clone());
            let debug = std::env::var("AREEV_LLM_DEBUG").is_ok();
            // modest concurrency to stay under provider rate limits; the LLM
            // script retries 429s with backoff. Tune via AREEV_LLM_WORKERS.
            let workers = std::env::var("AREEV_LLM_WORKERS").ok().and_then(|s| s.parse().ok()).unwrap_or(8usize).clamp(1, 64);
            let chunk_size = tasks.len().div_ceil(workers);
            let handles: Vec<_> = tasks
                .chunks(chunk_size)
                .map(|chunk| {
                    let chunk = chunk.to_vec();
                    let (reader, judge) = (reader.clone(), judge.clone());
                    std::thread::spawn(move || {
                        let (mut c, mut t) = (0usize, 0usize);
                        for task in &chunk {
                            let ans = run_llm(&reader, &reader_prompt(&task.context, &task.question)).unwrap_or_default();
                            let verdict = run_llm(&judge, &judge_prompt(&task.question, &task.gold, &ans)).unwrap_or_default();
                            let ok = verdict.to_uppercase().contains("YES");
                            t += 1;
                            if ok {
                                c += 1;
                            }
                            if debug {
                                // one eprintln! = one atomic write, so blocks never interleave
                                eprintln!(
                                    "[{}] cat{} Q: {}\n    gold: {}\n    got:  {}\n    judge: {}\n",
                                    if ok { "✓" } else { "✗" },
                                    task.category,
                                    task.question,
                                    task.gold,
                                    ans.replace('\n', " "),
                                    verdict.replace('\n', " "),
                                );
                            }
                        }
                        (c, t)
                    })
                })
                .collect();
            for h in handles {
                let (c, t) = h.join().unwrap();
                ans_correct += c;
                ans_total += t;
            }
        }
    }

    let pct = |n: usize| n as f64 / evaluated.max(1) as f64 * 100.0;
    let embed_short = match &embed_cache {
        Some(_) => format!("real embeddings {embed_dim}-d (text-embedding-3-small)"),
        None => "TF-IDF+bigram, lexical floor".to_string(),
    };
    println!("\n## Retrieval hit-rate (recall_hybrid vector leg — {embed_short}, k={topk}; rerank: {rerank_label})\n");
    println!("| metric | value |");
    println!("|---|---|");
    println!("| questions evaluated | {evaluated} |");
    println!("| hit@1  | {:.1}% |", pct(h1));
    println!("| hit@5  | {:.1}% |", pct(h5));
    println!("| hit@10 | {:.1}% |", pct(h10));
    if topk != 10 { println!("| hit@{topk} | {:.1}% |", pct(h_topk)); }
    println!("| MRR@10 | {:.3} |", mrr / evaluated.max(1) as f64);
    println!("\n  hit@k = a gold-evidence turn is in the top-k recalled.");
    if embed_cache.is_none() {
        println!("  Lexical FLOOR (TF-IDF + bigrams, no API); a real embedder lifts every row.\n");
    } else {
        println!("  Retrieval rides real semantic embeddings via the EmbedBackend trait.\n");
    }
    println!("  by LoCoMo category (hit@{topk}):");
    for (cat, (hit, n)) in &by_cat {
        let name = match cat {
            1 => "multi-hop",
            2 => "temporal",
            3 => "open-domain",
            4 => "single-hop",
            5 => "adversarial",
            _ => "other",
        };
        println!("    cat {cat} ({name:<11}): {:.1}%  (n={n})", *hit as f64 / (*n).max(1) as f64 * 100.0);
    }

    if let Rerank::Decision(chain) = &rerank {
        let sum = |f: fn(&DecisionRerankStats) -> u64| stats.iter().map(|s| f(s)).sum::<u64>();
        let served = stats.iter().find_map(|s| s.served());
        println!("\n## Decision-backend reranking\n");
        println!("| field | value |");
        println!("|---|---|");
        println!("| chain | `{}` |", chain.describe());
        println!("| calibrated | {} |", chain.calibrated());
        match served {
            Some((provider, model, cal)) => println!("| served | provider `{provider}`, model `{model}`, calibrated {cal} |"),
            None => println!("| served | (no answer received) |"),
        }
        println!("| decision requests | {} |", sum(DecisionRerankStats::requests));
        let mut codes: BTreeMap<String, u64> = BTreeMap::new();
        for s in &stats {
            for (c, n) in s.failure_codes() {
                *codes.entry(c).or_insert(0) += n;
            }
        }
        let codes = codes.iter().map(|(c, n)| format!("{c} ×{n}")).collect::<Vec<_>>().join(", ");
        println!("| failed requests (fell back to fusion) | {} {} |", sum(DecisionRerankStats::failures), if codes.is_empty() { String::new() } else { format!("({codes})") });
        println!("| candidates sent / cache hits | {} / {} |", sum(DecisionRerankStats::candidates_sent), sum(DecisionRerankStats::cache_hits));
        println!("| input / output tokens (provider-reported) | {} / {} |", sum(DecisionRerankStats::input_tokens), sum(DecisionRerankStats::output_tokens));
        println!("| summed request latency | {:.1} s |", sum(DecisionRerankStats::latency_ms) as f64 / 1000.0);
        let samples: Vec<String> = stats.iter().flat_map(|s| s.failure_samples()).take(3).collect();
        for m in samples {
            eprintln!("rerank failure sample: {}", m.chars().take(600).collect::<String>());
        }
    }

    println!("\n## End-to-end answer accuracy (LLM-judged)\n");
    match &llm_cmd {
        Some(_) if ans_total > 0 => {
            println!(
                "  {ans_correct}/{ans_total} correct = {:.1}% (sample of {ans_total}; raise with $AREEV_LLM_SAMPLE)",
                ans_correct as f64 / ans_total as f64 * 100.0
            );
        }
        _ => {
            println!("  SKIPPED — no reader/judge model wired.");
            println!("  Set AREEV_LLM_CMD (+ optional AREEV_JUDGE_CMD) to a stdin→stdout command, e.g.:");
            println!("    AREEV_LLM_CMD='python3 crates/areev-bench/scripts/openai_chat.py gpt-4o-mini' \\");
            println!("      cargo run --release -p areev-bench --bin accuracy -- {path} {limit}");
        }
    }
    println!("\ntotal wall time: {:.1} s", started.elapsed().as_secs_f64());
}
