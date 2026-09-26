//! LoCoMo (snap-research/locomo `data/locomo10.json`) records and the
//! no-API TF-IDF+bigram embedder, shared by the `accuracy` and
//! `min_score_sweep` benches so both read the dataset one way.

use areev_core::error::Result;
use areev_store::EmbedBackend;
use std::collections::{HashMap, HashSet};

// ---------- local TF-IDF (+bigram) embedder (no-API fallback) ----------
pub struct TfidfEmbed {
    dim: usize,
    idf: HashMap<String, f32>,
    default_idf: f32,
}
impl TfidfEmbed {
    pub fn build(dim: usize, docs: &[String]) -> Self {
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
pub struct Turn {
    pub dia_id: String,
    pub text: String,
}
pub struct Qa {
    pub question: String,
    pub answer: String,
    pub evidence: Vec<String>,
    pub category: i64,
}
pub struct Conv {
    pub turns: Vec<Turn>,
    pub qa: Vec<Qa>,
    pub dates: HashMap<String, String>, // dia_id -> session date (for temporal resolution)
}

pub fn parse_locomo(v: &serde_json::Value, limit: usize) -> Vec<Conv> {
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
