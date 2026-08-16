//! cal_validate — validate Areev's CAL `RECALL`/`ASSEMBLE ... FORMAT` and the
//! `areev-context` `ContextAssembler` on REAL LoCoMo data. The point is
//! correctness, not a score: for known questions, the assembled context must
//! faithfully contain the turns Areev recalled (and, end-to-end, the
//! gold-evidence turn). Exercises: store → embedder → AreevFacade →
//! CalExecutor(ASSEMBLE/FORMAT) and → ContextAssembler.
//!
//! usage: `cal_validate <locomo10.json> [conv_limit] [questions_per_conv]`
//! exit 0 = every code path produced faithful output on every sampled question.

use areev_cal::executor::CalResultPayload;
use areev_cal::store_types::RecallParams;
use areev_cal::{CalExecutor, CalExecutorConfig, CalStoreFacade, AreevFacade};
use areev_context::{ContextAssembler, FormatPolicy};
use areev_core::error::Result;
use areev_core::types::Event;
use areev_store::{Areev, AreevOptions, EmbedBackend};
use std::collections::{HashMap, HashSet};

// ---------- self-contained TF-IDF+bigram embedder (no external cache) ----------
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
        .map(str::to_string)
        .collect()
}
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
            v[(fnv1a(&t) % self.dim as u64) as usize] += c * self.idf.get(&t).copied().unwrap_or(self.default_idf);
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
        "tfidf-validate"
    }
}

// ---------- minimal LoCoMo loader ----------
struct Turn {
    dia_id: String,
    text: String,
}
struct Qa {
    question: String,
    evidence: Vec<String>,
}
struct Conv {
    turns: Vec<Turn>,
    qa: Vec<Qa>,
}
fn parse(v: &serde_json::Value, limit: usize) -> Vec<Conv> {
    let mut out = Vec::new();
    for sample in v.as_array().unwrap_or(&vec![]).iter().take(limit) {
        let mut turns = Vec::new();
        if let Some(c) = sample.get("conversation").and_then(|c| c.as_object()) {
            let mut sk: Vec<&String> = c
                .keys()
                .filter(|k| k.strip_prefix("session_").map(|r| !r.is_empty() && r.chars().all(|c| c.is_ascii_digit())).unwrap_or(false))
                .collect();
            sk.sort_by_key(|k| k.strip_prefix("session_").and_then(|r| r.parse::<i64>().ok()).unwrap_or(0));
            for k in sk {
                if let Some(arr) = c.get(k).and_then(|s| s.as_array()) {
                    for t in arr {
                        let sp = t.get("speaker").and_then(|s| s.as_str()).unwrap_or("");
                        let tx = t.get("text").and_then(|s| s.as_str()).unwrap_or("");
                        let d = t.get("dia_id").and_then(|s| s.as_str()).unwrap_or("");
                        if !d.is_empty() && !tx.is_empty() {
                            turns.push(Turn { dia_id: d.into(), text: format!("{sp}: {tx}") });
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
                    continue;
                }
                qa.push(Qa { question: q.get("question").and_then(|s| s.as_str()).unwrap_or("").into(), evidence });
            }
        }
        out.push(Conv { turns, qa });
    }
    out
}

/// strip characters that would break a CAL double-quoted string literal
fn sanitize(q: &str) -> String {
    q.chars().filter(|c| *c != '"' && *c != '\\' && !c.is_control()).collect()
}

/// Run a CAL statement, return (produced_text?, note). Handles the payload
/// match + errors so a bad statement is a reported failure, not a panic.
fn cal_text(ex: &CalExecutor, facade: &AreevFacade, cal: &str) -> (Option<String>, String) {
    match ex.execute(cal, facade) {
        Ok(r) => match r.result {
            CalResultPayload::Formatted { text, .. } => (Some(text), "Formatted".into()),
            other => (None, format!("non-Formatted: {other:?}").chars().take(80).collect()),
        },
        Err(e) => (None, format!("CAL error: {e}").chars().take(120).collect()),
    }
}

/// distinctive middle slice of a turn, for a robust substring check
fn slice(t: &str) -> String {
    let cs: Vec<char> = t.chars().collect();
    let start = cs.len().saturating_sub(30) / 2;
    cs[start..(start + 30).min(cs.len())].iter().collect()
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let path = args.get(1).cloned().unwrap_or_else(|| {
        eprintln!("usage: cal_validate <locomo10.json> [conv_limit] [questions_per_conv]");
        std::process::exit(2);
    });
    let conv_limit: usize = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(2);
    let per_conv: usize = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(8);

    let json: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    let convs = parse(&json, conv_limit);
    let dir = tempfile::TempDir::new().unwrap();

    // counters: how many sampled questions each code path handled faithfully
    let (mut n, mut recall_ok, mut recall_faithful, mut assemble_ok, mut assemble_faithful,
         mut ctx_faithful, mut evidence_e2e) = (0usize, 0, 0, 0, 0, 0, 0);
    let mut printed = 0usize;

    for (ci, conv) in convs.iter().enumerate() {
        let db = dir.path().join(format!("c{ci}.db"));
        let mut m = Areev::open_with(db.to_str().unwrap(), AreevOptions { index_text: false, ..Default::default() }).unwrap();
        let corpus: Vec<String> = conv.turns.iter().map(|t| t.text.clone()).collect();
        m.set_embedder(Box::new(TfidfEmbed::build(2048, &corpus)));
        let ns = format!("c{ci}");
        let dia2text: HashMap<&str, &str> = conv.turns.iter().map(|t| (t.dia_id.as_str(), t.text.as_str())).collect();
        for (ti, t) in conv.turns.iter().enumerate() {
            let mut ev = Event::new(&t.text).session(t.dia_id.clone());
            ev.common.namespace = Some(ns.clone());
            ev.common.created_at = Some(1_700_000_000_000 + (ci * 100_000 + ti) as i64);
            m.add(&ev).unwrap();
        }

        let facade = AreevFacade::with_session(m, Some(ns.clone()), None);
        let ex = CalExecutor::new(CalExecutorConfig::default());

        for qa in conv.qa.iter().take(per_conv) {
            n += 1;
            let sq = sanitize(&qa.question);

            // --- facade.recall (the shared retrieval the CAL executor also uses) ---
            let params = RecallParams::new().query(&qa.question).namespace(&ns).limit(10);
            let hits = facade.recall(&params).unwrap_or_default();
            let top = hits.first().and_then(|h| h.grain.get_str("content")).map(str::to_string);
            if !hits.is_empty() {
                recall_ok += 1;
            }

            // --- CAL RECALL ... FORMAT markdown ---
            let (recall_txt, recall_note) = cal_text(&ex, &facade, &format!("RECALL events ABOUT \"{sq}\" LIMIT 10 FORMAT markdown"));
            // --- CAL ASSEMBLE ... FORMAT markdown ---
            let (asm_txt, asm_note) = cal_text(&ex, &facade, &format!("ASSEMBLE FROM ctx: (RECALL events ABOUT \"{sq}\" LIMIT 10) FORMAT markdown"));
            // --- ContextAssembler ---
            let ctx = ContextAssembler::new().format(&hits, &FormatPolicy::gpt4().token_budget(2000));

            // faithfulness: does each rendered context contain the top recalled turn verbatim?
            let has_top = |txt: &Option<String>| matches!((txt, &top), (Some(t), Some(c)) if t.contains(c.as_str()));
            if has_top(&recall_txt) { recall_faithful += 1; }
            if asm_txt.is_some() { assemble_ok += 1; }
            if has_top(&asm_txt) { assemble_faithful += 1; }
            let ctx_has_top = top.as_deref().map(|c| ctx.text.contains(c)).unwrap_or(false);
            if ctx_has_top { ctx_faithful += 1; }

            // end-to-end: does the assembled context contain a gold-evidence turn?
            let gold_present = qa.evidence.iter().any(|d| {
                dia2text.get(d.as_str()).map(|gt| ctx.text.contains(&slice(gt))).unwrap_or(false)
            });
            if gold_present { evidence_e2e += 1; }

            // print the first few as human-checkable evidence
            if printed < 3 {
                printed += 1;
                println!("\n──── example {printed} ────");
                println!("Q: {}", qa.question);
                println!("gold evidence dia_ids: {:?}", qa.evidence);
                println!("facade.recall → {} hits; top turn: {}", hits.len(), top.as_deref().unwrap_or("<none>").chars().take(70).collect::<String>());
                println!("CAL RECALL…FORMAT markdown  [{recall_note}] faithful={}", has_top(&recall_txt));
                println!("CAL ASSEMBLE…FORMAT markdown [{asm_note}] faithful={}", has_top(&asm_txt));
                println!("ContextAssembler (gpt4/SML-md) → {} chars, {} grains, truncated={}, contains-gold={gold_present}", ctx.text.len(), ctx.included_count, ctx.truncated);
                let preview: String = ctx.text.chars().take(220).collect();
                println!("  context preview: {}…", preview.replace('\n', " "));
            }
        }
    }

    let pct = |x: usize| x as f64 / n.max(1) as f64 * 100.0;
    println!("\n════ CAL / context validation over {n} real LoCoMo questions ════");
    println!("facade.recall returned hits            : {recall_ok}/{n} ({:.0}%)", pct(recall_ok));
    println!("CAL RECALL…FORMAT rendered top turn    : {recall_faithful}/{n} ({:.0}%)", pct(recall_faithful));
    println!("CAL ASSEMBLE…FORMAT produced text      : {assemble_ok}/{n} ({:.0}%)", pct(assemble_ok));
    println!("CAL ASSEMBLE…FORMAT rendered top turn  : {assemble_faithful}/{n} ({:.0}%)", pct(assemble_faithful));
    println!("ContextAssembler rendered top turn     : {ctx_faithful}/{n} ({:.0}%)", pct(ctx_faithful));
    println!("assembled context contained gold turn  : {evidence_e2e}/{n} ({:.0}%)  [end-to-end retrieval+assembly]", pct(evidence_e2e));

    // Validation gate: every code path must faithfully render what was recalled.
    let ok = recall_ok == n && recall_faithful == n && assemble_ok == n && assemble_faithful == n && ctx_faithful == n;
    println!("\n{}", if ok { "VALIDATION PASS — CAL + context faithfully assemble recalled grains" } else { "VALIDATION ISSUES — see per-path counts above" });
    if !ok {
        std::process::exit(1);
    }
}
