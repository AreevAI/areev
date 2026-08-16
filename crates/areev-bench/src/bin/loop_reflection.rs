//! loop_reflection — the offline **Effective Reliability** eval for the LLM
//! reflection path (design `docs/loop-reflection.md` §6). It is the LLM-path
//! analog of `loop_precision`: a labeled corpus of planted issues + decoys,
//! run through the *real* GROUND→VERIFY→ROUTE pipeline, scored by a metric that
//! **subtracts for confident-wrong** — the shape that punishes over-generation:
//!
//!   Effective Reliability = (useful-correct − wrong) / total_scenarios
//!
//! Two things to be honest about:
//!
//! 1. The built-in `RefReviewer` is a **deterministic stand-in** for a real
//!    model — it over-generates at PROPOSE (a finding for every seeded subject,
//!    decoys included) and its VERIFY reads the cited evidence and keeps a
//!    finding only if the grains *genuinely* conflict. That makes this run a
//!    *machinery reference* (does the pipeline route planted issues through and
//!    filter decoys, and does the scorer compute ER correctly?) — **not** a
//!    model-quality claim. The field number comes from running the same corpus
//!    through a real model via `--llm-cmd` (`CommandLlm`).
//! 2. It also prints the ER the pipeline would get **without** the verifier
//!    (accept every grounded draft) — the delta is the verifier's value.
//!
//! Run: `cargo run --release -p areev-bench --bin loop_reflection`

use serde_json::{json, Map, Value};
use areev_loop::{Engine, GrainRecord, LlmBackend, ReferenceSubstrate, RunOptions};
use areev_bench::{effective_reliability as score, Reliability, Verdict};

// Kept small so the over-generated drafts (one per subject, pos+neg) fit under
// the engine's per-run MAX_LLM_DRAFTS cap without truncation.
const N: usize = 3; // planted positives (and decoys)
const NOW: i64 = 2_000_000_000_000;

/// A deterministic reference reviewer (a stand-in for a real model; see the
/// module docs). Over-generates at DISCOVER; grounds everything that cites real
/// grains; and VERIFIES by actually reading the cited evidence — keeping a
/// finding only when its two grains genuinely conflict (same subject+relation,
/// different object). It never reads the pos_/neg_ labels — only the evidence.
struct RefReviewer;

impl LlmBackend for RefReviewer {
    fn model(&self) -> &str {
        "reference-reviewer"
    }
    fn complete(&self, request: &str) -> areev_loop::Result<String> {
        let req: Value = serde_json::from_str(request).unwrap_or(Value::Null);
        let op = req.get("op").and_then(|v| v.as_str()).unwrap_or("");
        match op {
            "discover" => Ok(discover_over_generate(&req)),
            "ground" => Ok(ground_all_supported(&req)),
            "verify" => Ok(verify_by_reading_evidence(&req)),
            _ => Ok("{}".into()), // enrich etc.
        }
    }
}

/// One draft per distinct subject seen in the evidence bundle (decoys included).
fn discover_over_generate(req: &Value) -> String {
    let mut by_subject: std::collections::BTreeMap<String, Vec<String>> = Default::default();
    if let Some(ev) = req.get("evidence").and_then(|v| v.as_array()) {
        for e in ev {
            let hash = e.get("hash").and_then(|v| v.as_str()).unwrap_or("");
            let text = e.get("text").and_then(|v| v.as_str()).unwrap_or("");
            if let Some(subject) = text.split_whitespace().next() {
                by_subject.entry(subject.to_string()).or_default().push(hash.to_string());
            }
        }
    }
    let recs: Vec<Value> = by_subject
        .into_iter()
        .map(|(subject, hashes)| {
            json!({
                "summary": format!("possible semantic issue with {subject}"),
                "target": format!("entity:bench/{subject}"),
                "evidence": hashes,
                "confidence": 0.9,
            })
        })
        .collect();
    json!({ "recommendations": recs }).to_string()
}

fn ground_all_supported(req: &Value) -> String {
    let ids = req.get("claims").and_then(|v| v.as_array()).cloned().unwrap_or_default();
    let results: Vec<Value> = ids
        .iter()
        .filter_map(|c| c.get("id").and_then(|v| v.as_u64()))
        .map(|id| json!({ "id": id, "supported": true }))
        .collect();
    json!({ "results": results }).to_string()
}

/// Keep a finding iff its cited grains genuinely conflict: ≥2 grains that share
/// subject+relation but differ on object. Reads the evidence, not the labels.
fn verify_by_reading_evidence(req: &Value) -> String {
    let items = req.get("findings").and_then(|v| v.as_array()).cloned().unwrap_or_default();
    let results: Vec<Value> = items
        .iter()
        .filter_map(|it| {
            let id = it.get("id").and_then(|v| v.as_u64())?;
            let ev = it.get("evidence").and_then(|v| v.as_array())?;
            // Parse "subject relation object" out of each cited grain.
            let mut objs_by_sr: std::collections::BTreeMap<(String, String), std::collections::BTreeSet<String>> =
                Default::default();
            for e in ev {
                let text = e.get("text").and_then(|v| v.as_str()).unwrap_or("");
                let parts: Vec<&str> = text.splitn(3, ' ').collect();
                if parts.len() == 3 {
                    objs_by_sr
                        .entry((parts[0].to_string(), parts[1].to_string()))
                        .or_default()
                        .insert(parts[2].to_string());
                }
            }
            let conflicts = objs_by_sr.values().any(|objs| objs.len() >= 2);
            Some(json!({ "id": id, "keep": conflicts, "confidence": if conflicts { 0.9 } else { 0.2 } }))
        })
        .collect();
    json!({ "results": results }).to_string()
}

fn main() {
    let mut sub = ReferenceSubstrate::new();
    let mut clock = 0i64;
    let mut mk = |sub: &mut ReferenceSubstrate, subject: &str, relation: &str, object: &str| {
        clock += 1;
        let mut fields = Map::new();
        fields.insert("subject".into(), json!(subject));
        fields.insert("relation".into(), json!(relation));
        fields.insert("object".into(), json!(object));
        fields.insert("namespace".into(), json!("bench"));
        sub.insert(GrainRecord {
            hash: String::new(),
            grain_type: "fact".into(),
            namespace: "bench".into(),
            created_at_ms: NOW - 3_600_000 + clock,
            valid_to_ms: None,
            superseded_by: None,
            fields,
        });
    };
    for i in 0..N {
        // positive: a real contradiction (two live values under a functional
        // relation). The deterministic sweep fires AND the finding is genuine,
        // so the reviewer's over-generated draft should survive VERIFY.
        mk(&mut sub, &format!("pos_con_{i}"), "deploy_target", "us-east-1");
        mk(&mut sub, &format!("pos_con_{i}"), "deploy_target", "eu-west-1");
        // decoy: an exact duplicate (two identical facts). The deterministic
        // sweep fires (seeding the bundle), the reviewer over-generates a
        // spurious "semantic issue", but VERIFY reads the evidence and sees no
        // conflict → kills it. A finding that survives here is a false positive.
        mk(&mut sub, &format!("neg_dup_{i}"), "tier", "gold");
        mk(&mut sub, &format!("neg_dup_{i}"), "tier", "gold");
    }

    // Default: the deterministic reference reviewer (a machinery reference).
    // `AREEV_LOOP_EVAL_MODEL=openrouter:openai/gpt-4o-mini` runs the corpus through
    // a real model for the field number.
    let real = std::env::var("AREEV_LOOP_EVAL_MODEL").ok().filter(|s| !s.is_empty());
    let engine = match &real {
        Some(spec) => {
            eprintln!("# reviewer: real model `{spec}` (via areev-llm)");
            Engine::with_builtins()
                .with_llm(areev_llm::resolve(spec, None, None).expect("resolve AREEV_LOOP_EVAL_MODEL"))
        }
        None => Engine::with_builtins().with_llm(Box::new(RefReviewer)),
    };
    engine.run(&mut sub, &RunOptions::default(), NOW).expect("run");
    let recs = engine.recommendations(&sub, None).expect("list");

    // Classify only the llm-origin (surfaced-by-reflection) recs.
    let mut verdicts = Vec::new();
    for r in &recs {
        if !format!("{:?}", r.origin).contains("Llm") {
            continue;
        }
        let s = r.summary.render();
        if s.contains("pos_") {
            verdicts.push(Verdict::UsefulCorrect);
        } else if s.contains("neg_") {
            verdicts.push(Verdict::Wrong);
        }
    }
    let rep = score(&verdicts, N);

    // The counterfactual: without the verifier, every over-generated draft
    // (pos AND neg) would surface — N useful, N wrong.
    let raw = score(&[Verdict::UsefulCorrect, Verdict::Wrong].repeat(N), N);

    println!("# loop_reflection — Effective-Reliability (machinery reference; real number via --llm-cmd)\n");
    println!("planted positives: {N}   decoys: {N}\n");
    println!("| pipeline | surfaced | useful | wrong | ER | precision | recall | spurious |");
    println!("|---|---|---|---|---|---|---|---|");
    let row = |name: &str, r: &Reliability| {
        println!(
            "| {name} | {} | {} | {} | {:+.2} | {:.2} | {:.2} | {:.2} |",
            r.surfaced, r.useful_correct, r.wrong, r.er, r.precision, r.recall, r.spurious_rate
        );
    };
    row("no verifier (accept grounded)", &raw);
    row("with verifier", &rep);
    println!(
        "\n_Effective Reliability = (useful-correct − wrong) / positives; it subtracts for \
         confident-wrong, so over-generation lowers it. The verifier lifts ER from {:+.2} to \
         {:+.2} on this corpus by filtering the decoys._",
        raw.er, rep.er
    );

    // CI guard applies only to the deterministic reference reviewer; a real
    // model produces an exploratory number, not a gate.
    if real.is_none() && (rep.spurious_rate > 0.0 || rep.recall < 0.9) {
        eprintln!(
            "\nREGRESSION: spurious_rate {:.2} (want 0), recall {:.2} (want ≥0.9)",
            rep.spurious_rate, rep.recall
        );
        std::process::exit(1);
    }
}
