//! selfimprove_learn — the authoring-rate instrument.
//!
//! The A/B/A/B bench pays for four eval states to answer "did the lessons
//! help?". This one answers the question that has to come first for an
//! LLM-authored learner: "does the model author an applicable lesson AT ALL,
//! and where does it lose the ones it drafts?" — over the same captured
//! experience, K times, for a few cents, before a full run spends on it.
//!
//! Input is a workdir `selfimprove_aba --stop-after experience` left behind.
//! Every pass copies that memory into its own directory (the embedded store
//! is single-writer per file and a learn pass mutates it), runs one governed
//! learn pass through the real engine with the given loop backends, and
//! appends one JSON row: the DISCOVER funnel stage by stage, what was
//! authored, what the scripted review applied, and the wall time. The rows
//! are the evidence; the summary at the end is derived from them.
//!
//! What it does NOT measure: whether a lesson helps. A lesson that survives
//! GROUND + VERIFY here can still be useless or harmful on held-out tasks —
//! that is the A/B/A/B bench's job, and this instrument exists so that bench
//! is run on a learner that reliably produces something to measure.

use areev_bench::selfimprove::memory::{LearnConfig, LearnOutcome, LessonArms, Memory, MockLoopLlm};
use areev_loop::policy::DiscoverObjective;
use areev_loop::{CommandLlm, LlmBackend};
use serde_json::{json, Value};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Instant;

/// The same pinned engine clock `selfimprove_aba` learns at, so a lesson
/// authored here carries the same timestamps a full run would stamp.
const BASE_MS: i64 = 1_700_000_000_000;
const HOUR_MS: i64 = 3_600_000;

struct Args {
    workdir: PathBuf,
    passes: usize,
    llm_cmd: Option<String>,
    ground_cmd: Option<String>,
    mock_llm: bool,
    learner: bool,
    llm_lessons: bool,
    label: String,
    out: Option<PathBuf>,
}

fn usage() -> ! {
    eprintln!(
        "usage: selfimprove_learn --workdir PATH (--llm-cmd 'CMD' | --mock-llm) [--ground-cmd 'CMD']\n\
         \x20                        [--passes N] [--learner] [--llm-lessons]\n\
         \x20                        [--label NAME] [--out FILE.jsonl]\n\
         \n\
         PATH must hold the bench.db that `selfimprove_aba --stop-after experience` captured.\n\
         Each pass learns over a fresh copy of it; rows go to --out (default\n\
         PATH/learn/<label>.jsonl) and a summary to PATH/learn/<label>.summary.json.\n\
         A literal {{pass}} in --llm-cmd / --ground-cmd becomes the pass number (per-pass seeds)."
    );
    std::process::exit(2);
}

fn die(msg: &str) -> ! {
    eprintln!("selfimprove_learn: {msg}");
    std::process::exit(1);
}

fn parse_args() -> Args {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let mut workdir: Option<PathBuf> = None;
    let mut passes: usize = 5;
    let mut llm_cmd: Option<String> = None;
    let mut ground_cmd: Option<String> = None;
    let mut mock_llm = false;
    let mut learner = false;
    let mut llm_lessons = false;
    let mut label: Option<String> = None;
    let mut out: Option<PathBuf> = None;
    let mut i = 0;
    while i < argv.len() {
        match argv[i].as_str() {
            "--mock-llm" => {
                mock_llm = true;
                i += 1;
            }
            "--learner" => {
                learner = true;
                i += 1;
            }
            "--llm-lessons" => {
                llm_lessons = true;
                i += 1;
            }
            flag => {
                let Some(value) = argv.get(i + 1) else { usage() };
                match flag {
                    "--workdir" => workdir = Some(PathBuf::from(value)),
                    "--passes" => passes = value.parse().unwrap_or_else(|_| usage()),
                    "--llm-cmd" => llm_cmd = Some(value.clone()),
                    "--ground-cmd" => ground_cmd = Some(value.clone()),
                    "--label" => label = Some(value.clone()),
                    "--out" => out = Some(PathBuf::from(value)),
                    _ => usage(),
                }
                i += 2;
            }
        }
    }
    let Some(workdir) = workdir else { usage() };
    if passes == 0 {
        usage()
    }
    if mock_llm == llm_cmd.is_some() {
        eprintln!("selfimprove_learn: exactly one of --mock-llm / --llm-cmd is required");
        usage()
    }
    // The label names the configuration in the rows, so a default that
    // encodes the two switches keeps two configurations from sharing a file.
    let label = label.unwrap_or_else(|| {
        format!(
            "{}-{}",
            if learner { "learner" } else { "review_queue" },
            if llm_lessons { "llm-applied" } else { "llm-advisory" }
        )
    });
    Args {
        workdir,
        passes,
        llm_cmd,
        ground_cmd,
        mock_llm,
        learner,
        llm_lessons,
        label,
        out,
    }
}

/// Copy every file of the captured memory (`bench.db`, its WAL, any
/// sidecar) into `dst`, so a pass mutates its own copy and the captured
/// experience stays what every later pass reads.
fn copy_memory(src: &Path, dst: &Path) -> Result<usize, String> {
    std::fs::create_dir_all(dst).map_err(|e| format!("create {}: {e}", dst.display()))?;
    let mut copied = 0;
    let entries = std::fs::read_dir(src).map_err(|e| format!("read {}: {e}", src.display()))?;
    for entry in entries {
        let entry = entry.map_err(|e| format!("read {}: {e}", src.display()))?;
        let name = entry.file_name();
        let Some(name_str) = name.to_str() else { continue };
        if !name_str.starts_with("bench.db") || !entry.path().is_file() {
            continue;
        }
        std::fs::copy(entry.path(), dst.join(&name))
            .map_err(|e| format!("copy {}: {e}", entry.path().display()))?;
        copied += 1;
    }
    if copied == 0 {
        return Err(format!("no bench.db* files under {}", src.display()));
    }
    Ok(copied)
}

/// An optional boxed loop backend — the shape `Memory::learn_with` takes.
type LoopBackend = Option<Box<dyn LlmBackend>>;

/// Backends for one pass. A literal `{pass}` in either command is replaced
/// by the pass number, so an adapter's `--seed {pass}` gives every pass its
/// own request seed: with temperature 0 and one fixed seed, ten passes can
/// be one sample repeated ten times, and a rate measured that way is not a
/// rate. The substitution is the only thing that differs between passes.
fn backends(args: &Args, pass: usize) -> (LoopBackend, LoopBackend) {
    let seeded = |cmd: &str| cmd.replace("{pass}", &pass.to_string());
    let llm: LoopBackend = if args.mock_llm {
        Some(Box::new(MockLoopLlm))
    } else {
        args.llm_cmd.as_deref().map(|cmd| {
            Box::new(
                CommandLlm::new(&seeded(cmd), None)
                    .unwrap_or_else(|e| die(&format!("--llm-cmd: {e}"))),
            ) as Box<dyn LlmBackend>
        })
    };
    let ground: LoopBackend = args.ground_cmd.as_deref().map(|cmd| {
        Box::new(
            CommandLlm::new(&seeded(cmd), None)
                .unwrap_or_else(|e| die(&format!("--ground-cmd: {e}"))),
        ) as Box<dyn LlmBackend>
    });
    (llm, ground)
}

fn main() {
    let args = parse_args();
    if !args.workdir.join("bench.db").exists() {
        die(&format!(
            "{} has no bench.db — run `selfimprove_aba --workdir {} ... --stop-after experience` first",
            args.workdir.display(),
            args.workdir.display()
        ));
    }
    let learn_dir = args.workdir.join("learn");
    std::fs::create_dir_all(&learn_dir).unwrap_or_else(|e| die(&format!("create {}: {e}", learn_dir.display())));
    let out_path = args
        .out
        .clone()
        .unwrap_or_else(|| learn_dir.join(format!("{}.jsonl", args.label)));
    let mut out = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&out_path)
        .unwrap_or_else(|e| die(&format!("open {}: {e}", out_path.display())));
    let cfg = LearnConfig {
        arms: LessonArms { analyzer: true, llm: args.llm_lessons },
        objective: if args.learner {
            DiscoverObjective::Learner
        } else {
            DiscoverObjective::ReviewQueue
        },
    };
    let objective = if args.learner { "learner" } else { "review_queue" };
    eprintln!(
        "learn: {} pass(es) over {} · objective {objective} · llm lessons {}",
        args.passes,
        args.workdir.join("bench.db").display(),
        if args.llm_lessons { "applied" } else { "advisory" }
    );

    let mut rows: Vec<Value> = Vec::new();
    for pass in 1..=args.passes {
        let pass_dir = learn_dir.join(&args.label).join(format!("pass-{pass:02}"));
        if pass_dir.exists() {
            std::fs::remove_dir_all(&pass_dir)
                .unwrap_or_else(|e| die(&format!("clear {}: {e}", pass_dir.display())));
        }
        copy_memory(&args.workdir, &pass_dir).unwrap_or_else(|e| die(&e));
        let started = Instant::now();
        let (llm, ground) = backends(&args, pass);
        let outcome: LearnOutcome = {
            // Scoped so the handle is released before the next pass copies.
            let mem = Memory::open(&pass_dir).unwrap_or_else(|e| die(&e));
            mem.learn_with(llm, ground, cfg, BASE_MS + HOUR_MS)
                .unwrap_or_else(|e| die(&format!("pass {pass}: {e}")))
        };
        let elapsed_ms = started.elapsed().as_millis() as u64;
        let llm_entries: Vec<&_> = outcome
            .ledger
            .entries
            .iter()
            .filter(|e| e.source == "llm")
            .collect();
        let llm_applied = llm_entries.iter().filter(|e| e.disposition == "applied").count();
        let analyzer_applied = outcome
            .ledger
            .entries
            .iter()
            .filter(|e| e.source != "llm" && e.disposition == "applied")
            .count();
        let funnel = outcome
            .funnel
            .as_ref()
            .map(|f| serde_json::to_value(f).unwrap_or(Value::Null))
            .unwrap_or(Value::Null);
        let stored = outcome.funnel.as_ref().map(|f| f.stored).unwrap_or(0);
        let row = json!({
            "label": args.label,
            "pass": pass,
            "objective": objective,
            "llm_lessons": args.llm_lessons,
            "funnel": funnel,
            "llm_stored": stored,
            "llm_applied": llm_applied,
            "analyzer_applied": analyzer_applied,
            "llm_findings": llm_entries.iter().map(|e| json!({
                "disposition": e.disposition,
                "summary": e.summary,
            })).collect::<Vec<_>>(),
            "elapsed_ms": elapsed_ms,
        });
        writeln!(out, "{row}").unwrap_or_else(|e| die(&format!("write {}: {e}", out_path.display())));
        out.flush().ok();
        eprintln!(
            "  pass {pass:02}: stored {stored} llm finding(s), applied {llm_applied} llm + {analyzer_applied} analyzer, {elapsed_ms} ms{}",
            outcome
                .funnel
                .as_ref()
                .map(|f| format!(
                    " [proposed {} → cited {} → grounded {} → kept {}]",
                    f.proposed, f.cited, f.grounded, f.kept
                ))
                .unwrap_or_default()
        );
        rows.push(row);
    }

    // The summary is derived from the rows, never computed separately.
    let n = rows.len() as f64;
    let sum = |key: &str| -> u64 {
        rows.iter()
            .filter_map(|r| r.pointer(&format!("/funnel/{key}")).and_then(Value::as_u64))
            .sum()
    };
    let passes_with_lesson = rows
        .iter()
        .filter(|r| r["llm_stored"].as_u64().unwrap_or(0) > 0)
        .count();
    let summary = json!({
        "label": args.label,
        "objective": objective,
        "llm_lessons": args.llm_lessons,
        "passes": rows.len(),
        "passes_with_llm_finding": passes_with_lesson,
        "authoring_rate": passes_with_lesson as f64 / n,
        "mean_llm_stored_per_pass": sum("stored") as f64 / n,
        "funnel_totals": {
            "evidence": sum("evidence"),
            "proposed": sum("proposed"),
            "cited": sum("cited"),
            "dropped_uncited": sum("dropped_uncited"),
            "dropped_target": sum("dropped_target"),
            "grounded": sum("grounded"),
            "kept": sum("kept"),
            "stored": sum("stored"),
        },
        "mean_elapsed_ms": rows.iter().map(|r| r["elapsed_ms"].as_u64().unwrap_or(0)).sum::<u64>() as f64 / n,
        "rows": out_path.display().to_string(),
    });
    let summary_path = learn_dir.join(format!("{}.summary.json", args.label));
    std::fs::write(&summary_path, serde_json::to_string_pretty(&summary).unwrap())
        .unwrap_or_else(|e| die(&format!("write {}: {e}", summary_path.display())));
    eprintln!(
        "authoring rate {}/{} passes · mean stored {:.2}/pass · funnel proposed {} → cited {} → grounded {} → kept {} → stored {}",
        passes_with_lesson,
        rows.len(),
        sum("stored") as f64 / n,
        sum("proposed"),
        sum("cited"),
        sum("grounded"),
        sum("kept"),
        sum("stored")
    );
    println!("{}", serde_json::to_string_pretty(&summary).unwrap());
}
