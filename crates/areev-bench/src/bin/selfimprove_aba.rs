//! selfimprove_aba — the A/B/A/B causal proof (crates/areev-bench/SELFIMPROVE.md).
//!
//! Held-out task success under four memory states, where the ONLY lever is
//! Areev's own governed apply/rollback (the eval prompt is assembled from
//! live memory on every run — no harness flag):
//!
//!   A0  experience captured, nothing applied
//!   B   lessons applied
//!   A1  lessons rolled back        ← the causal proof
//!   B2  lessons re-applied (a fresh governed pass: rolled_back is terminal)
//!
//! Phase order, and one documented deviation from SELFIMPROVE.md step 3's
//! sketch: A0 is evaluated BEFORE the learn pass runs at all, not "after
//! propose, before apply". The prompt-visible state is identical — lessons
//! exist only after apply, so pending-vs-unproposed renders the same empty
//! LESSONS section — and this order keeps each phase a single lever pull:
//!   (a) eval A0 → (b) learn → (c) eval B → (d) rollback → (e) eval A1
//!   (hard-asserts the LESSONS section is empty) → (f) learn again →
//!   (g) eval B2.
//! Phases run at strictly increasing engine clocks (base + n·1h, total span
//! far under the 1-day outcome_review horizon, so no revert fires mid-bench).
//!
//! `--mock` is the keyless deterministic plumbing reference (CI runs
//! `--mock --assert-shape`); it is never a learning claim. Live numbers come
//! from `--agent-cmd` (see SELFIMPROVE.md "Reproduce").

use areev_bench::selfimprove::memory::Memory;
use areev_bench::selfimprove::{agent, env, report::Reporter};
use areev_bench::selfimprove::{EvalSummary, Ledger, RuleId, RuleStat, TaskRunRecord, Usage};
use serde_json::json;
use std::collections::BTreeSet;
use std::path::PathBuf;

/// Engine phase clocks: fixed base (determinism rule — never the wall clock
/// when the value decides behavior), +1h per phase.
const BASE_MS: i64 = 1_700_000_000_000;
const HOUR_MS: i64 = 3_600_000;

struct Args {
    workdir: PathBuf,
    seed: u64,
    experience: usize,
    eval: usize,
    mock: bool,
    agent_cmd: Option<String>,
    llm_cmd: Option<String>,
    ground_cmd: Option<String>,
    max_turns: u32,
    assert_shape: bool,
}

fn die(msg: &str) -> ! {
    eprintln!("selfimprove_aba: {msg}");
    std::process::exit(2);
}

fn usage() -> ! {
    eprintln!(
        "usage: selfimprove_aba --workdir PATH (--mock | --agent-cmd 'CMD')\n\
         \x20                       [--seed N] [--experience N] [--eval N]\n\
         \x20                       [--llm-cmd 'CMD'] [--ground-cmd 'CMD']\n\
         \x20                       [--max-turns N] [--assert-shape]"
    );
    std::process::exit(2);
}

fn parse_args() -> Args {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let mut workdir: Option<PathBuf> = None;
    let mut seed: u64 = 1;
    let mut experience: usize = 150;
    let mut eval: usize = 60;
    let mut mock = false;
    let mut agent_cmd: Option<String> = None;
    let mut llm_cmd: Option<String> = None;
    let mut ground_cmd: Option<String> = None;
    let mut max_turns: u32 = 24;
    let mut assert_shape = false;

    let mut i = 0;
    while i < argv.len() {
        match argv[i].as_str() {
            "--mock" => {
                mock = true;
                i += 1;
            }
            "--assert-shape" => {
                assert_shape = true;
                i += 1;
            }
            flag => {
                let Some(value) = argv.get(i + 1) else { usage() };
                match flag {
                    "--workdir" => workdir = Some(PathBuf::from(value)),
                    "--seed" => seed = value.parse().unwrap_or_else(|_| usage()),
                    "--experience" => experience = value.parse().unwrap_or_else(|_| usage()),
                    "--eval" => eval = value.parse().unwrap_or_else(|_| usage()),
                    "--agent-cmd" => agent_cmd = Some(value.clone()),
                    "--llm-cmd" => llm_cmd = Some(value.clone()),
                    "--ground-cmd" => ground_cmd = Some(value.clone()),
                    "--max-turns" => max_turns = value.parse().unwrap_or_else(|_| usage()),
                    _ => usage(),
                }
                i += 2;
            }
        }
    }

    let Some(workdir) = workdir else {
        eprintln!("selfimprove_aba: --workdir is required");
        usage()
    };
    // Exactly one agent backend: the mock proves plumbing, the command
    // produces numbers — mixing them would blur which claim a run makes.
    if mock == agent_cmd.is_some() {
        eprintln!("selfimprove_aba: exactly one of --mock / --agent-cmd is required");
        usage()
    }
    Args {
        workdir,
        seed,
        experience,
        eval,
        mock,
        agent_cmd,
        llm_cmd,
        ground_cmd,
        max_turns,
        assert_shape,
    }
}

/// Fresh backend per task: the mock must not leak state across tasks, and a
/// command backend is a per-call subprocess anyway.
fn make_backend(args: &Args) -> Box<dyn agent::ChatBackend> {
    if args.mock {
        Box::new(agent::MockBackend)
    } else {
        Box::new(agent::CmdBackend {
            cmd: args.agent_cmd.clone().expect("validated at parse"),
            model: String::new(),
        })
    }
}

/// Fold one eval pass into the report shape. Per-rule: opportunities = tasks
/// exercising the rule at all; failures = tasks that MISHANDLED it (see
/// `selfimprove::mishandled_rules` — an unavoidable first 429 that gets
/// handled is not a failure, giving up on it is).
fn summarize(state: &str, tasks: &[env::Task], records: &[TaskRunRecord]) -> EvalSummary {
    let mut successes = 0u32;
    let mut tool_errors = 0u32;
    let mut total_steps = 0u32;
    let mut usage = Usage::default();
    for r in records {
        if r.success {
            successes += 1;
        }
        tool_errors += r.tool_errors;
        total_steps += r.steps;
        usage.prompt_tokens += r.usage.prompt_tokens;
        usage.completion_tokens += r.usage.completion_tokens;
    }
    let rules: BTreeSet<RuleId> = tasks
        .iter()
        .flat_map(|t| t.rules_exercised.iter().copied())
        .collect();
    let per_rule = rules
        .iter()
        .map(|&rule| RuleStat {
            rule,
            opportunities: tasks
                .iter()
                .filter(|t| t.rules_exercised.contains(&rule))
                .count() as u32,
            failures: records
                .iter()
                .filter(|r| areev_bench::selfimprove::mishandled_rules(r).contains(&rule))
                .count() as u32,
        })
        .collect();
    EvalSummary {
        state: state.to_string(),
        n: records.len() as u32,
        successes,
        tool_errors,
        total_steps,
        per_rule,
        usage,
    }
}

/// One eval pass: the SAME held-out tasks in the SAME order, fresh Env per
/// task, `lessons` fetched once by the caller. Eval tool calls are NEVER
/// recorded into memory — the held-out set must not leak into learning.
fn run_eval(
    state: &str,
    tasks: &[env::Task],
    lessons: &str,
    args: &Args,
    reporter: &Reporter,
) -> EvalSummary {
    let mut tx = reporter
        .transcript(&format!("transcripts-eval-{state}.jsonl"))
        .unwrap_or_else(|e| die(&format!("open eval-{state} transcript: {e}")));
    let mut records = Vec::with_capacity(tasks.len());
    for task in tasks {
        let mut e = env::Env::new(task);
        let mut backend = make_backend(args);
        let rec = agent::run_task(
            backend.as_mut(),
            &mut e,
            &task.id,
            &task.prompt,
            lessons,
            args.max_turns,
            |ev| tx.row(ev),
        );
        // Per-task outcome row: the paired unit. Aggregates alone cannot
        // support the paired test the stats rule requires (same instances
        // across states), and per-task outcomes cannot be reconstructed
        // afterwards — so they are persisted, not just summed.
        tx.row(&json!({
            "kind": "task_outcome",
            "state": state,
            "task_id": rec.task_id,
            "template": task.template,
            "success": rec.success,
            "failure_reason": rec.failure_reason,
            "steps": rec.steps,
            "tool_errors": rec.tool_errors,
            "rules_exercised": task.rules_exercised,
            "mishandled": areev_bench::selfimprove::mishandled_rules(&rec),
            "prompt_tokens": rec.usage.prompt_tokens,
            "completion_tokens": rec.usage.completion_tokens,
        }));
        records.push(rec);
    }
    summarize(state, tasks, &records)
}

fn git_rev() -> String {
    std::process::Command::new("git")
        .args(["rev-parse", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

fn print_results(args: &Args, evals: &[EvalSummary], ledger: &Ledger, applied1: usize, applied2: usize) {
    let label = if args.mock {
        " (mock — deterministic plumbing reference, not a learning claim)"
    } else {
        ""
    };
    println!("# selfimprove_aba — A/B/A/B causal proof{label}\n");
    println!(
        "held-out task success — same tasks, same order, seed {} ({} tasks):\n",
        args.seed, args.eval
    );
    println!("| state | success | rate | tool errors | mean steps |");
    println!("|---|---|---|---|---|");
    fn name(state: &str) -> &str {
        match state {
            "A0" => "A0 before lessons",
            "B" => "B  lessons applied",
            "A1" => "A1 lessons rolled back",
            "B2" => "B2 lessons re-applied",
            other => other,
        }
    }
    for e in evals {
        let mean_steps = if e.n == 0 { 0.0 } else { e.total_steps as f64 / e.n as f64 };
        println!(
            "| {} | {}/{} | {:.1}% | {} | {:.1} |",
            name(&e.state),
            e.successes,
            e.n,
            e.success_rate() * 100.0,
            e.tool_errors,
            mean_steps
        );
    }

    // Per-rule recurrence: tasks mishandling / tasks exercising, per state.
    let rules: BTreeSet<&str> = evals
        .iter()
        .flat_map(|e| e.per_rule.iter().map(|s| s.rule))
        .collect();
    if !rules.is_empty() {
        println!("\nper-rule mishandling recurrence (tasks mishandling / tasks exercising):\n");
        print!("| rule |");
        for e in evals {
            print!(" {} |", e.state);
        }
        println!();
        print!("|---|");
        for _ in evals {
            print!("---|");
        }
        println!();
        for rule in &rules {
            print!("| {rule} |");
            for e in evals {
                match e.per_rule.iter().find(|s| s.rule == *rule) {
                    Some(s) => print!(" {}/{} |", s.failures, s.opportunities),
                    None => print!(" - |"),
                }
            }
            println!();
        }
    }

    println!(
        "\ngovernance ledger: {} rows — {} applied, {} rejected, {} advisory, {} apply_failed",
        ledger.entries.len(),
        ledger.count("applied"),
        ledger.count("rejected"),
        ledger.count("advisory"),
        ledger.count("apply_failed"),
    );
    println!(
        "  (first pass applied {applied1}, re-apply pass applied {applied2} — a rolled-back \
         hash is terminal, so restoration is a fresh governed proposal)"
    );
}

/// The CI gate for `--mock`: the shape of causality, not a magnitude claim.
/// (The A1 lessons-empty check is hard-asserted in the pipeline itself.)
fn check_shape(a0: &EvalSummary, b: &EvalSummary, a1: &EvalSummary, b2: &EvalSummary) {
    let mut fails = Vec::new();
    if b.success_rate() <= a0.success_rate() + 0.1 {
        fails.push(format!(
            "B ({:.3}) must exceed A0 ({:.3}) by more than 0.10 — applied lessons had no effect",
            b.success_rate(),
            a0.success_rate()
        ));
    }
    if (a1.success_rate() - a0.success_rate()).abs() > 0.05 {
        fails.push(format!(
            "A1 ({:.3}) must return to A0 ({:.3}) within 0.05 — rollback did not remove the effect",
            a1.success_rate(),
            a0.success_rate()
        ));
    }
    if (b2.success_rate() - b.success_rate()).abs() > 0.05 {
        fails.push(format!(
            "B2 ({:.3}) must match B ({:.3}) within 0.05 — re-apply did not restore the effect",
            b2.success_rate(),
            b.success_rate()
        ));
    }
    if fails.is_empty() {
        println!("\nassert-shape: PASS (B > A0 + 0.10, |A1−A0| ≤ 0.05, |B2−B| ≤ 0.05, A1 lessons empty)");
    } else {
        for f in &fails {
            eprintln!("assert-shape FAIL: {f}");
        }
        std::process::exit(1);
    }
}

fn main() {
    let args = parse_args();
    let mem = Memory::create(&args.workdir).unwrap_or_else(|e| die(&e));
    let reporter = Reporter::new(&args.workdir)
        .unwrap_or_else(|e| die(&format!("reporter: {e}")));

    let t_learn1 = BASE_MS + HOUR_MS;
    let t_rollback = BASE_MS + 2 * HOUR_MS;
    let t_learn2 = BASE_MS + 3 * HOUR_MS;

    // 1 EXPERIENCE — the agent is ignorant by construction (empty lessons);
    // every tool call is recorded into the memory, errors flagged.
    let exp_tasks = env::gen_tasks(args.seed, env::Split::Experience, args.experience);
    eprintln!("experience: {} tasks", exp_tasks.len());
    {
        let mut tx = reporter
            .transcript("transcripts-experience.jsonl")
            .unwrap_or_else(|e| die(&format!("open experience transcript: {e}")));
        for task in &exp_tasks {
            let mut e = env::Env::new(task);
            let mut backend = make_backend(&args);
            let rec = agent::run_task(
                backend.as_mut(),
                &mut e,
                &task.id,
                &task.prompt,
                "",
                args.max_turns,
                |ev| tx.row(ev),
            );
            mem.record_task(&rec).unwrap_or_else(|e| die(&e));
        }
    }

    // The held-out set: disjoint from experience, fixed per seed, evaluated
    // in the same order under all four states.
    let eval_tasks = env::gen_tasks(args.seed, env::Split::Eval, args.eval);

    // 2 A0 — experience captured, nothing applied.
    let lessons = mem.lessons_markdown().unwrap_or_else(|e| die(&e));
    if !lessons.is_empty() {
        die("A0 precondition broken: LESSONS section non-empty before any apply");
    }
    eprintln!("eval A0: {} tasks", eval_tasks.len());
    let a0 = run_eval("A0", &eval_tasks, &lessons, &args, &reporter);

    // 3 LEARN → apply (scripted review: executable non-destructive approved
    // + applied, destructive rejected, advisory ledgered) → eval B.
    let (ledger1, applied1) = mem
        .learn(args.llm_cmd.as_deref(), args.ground_cmd.as_deref(), t_learn1)
        .unwrap_or_else(|e| die(&e));
    eprintln!(
        "learn 1: {} ledger rows, {} applied",
        ledger1.entries.len(),
        applied1.len()
    );
    let lessons = mem.lessons_markdown().unwrap_or_else(|e| die(&e));
    let b = run_eval("B", &eval_tasks, &lessons, &args, &reporter);

    // 4 ROLLBACK → eval A1. The load-bearing honesty check: rollback changes
    // the FILE, and the file must change the PROMPT to empty.
    mem.rollback(&applied1, t_rollback).unwrap_or_else(|e| die(&e));
    let lessons = mem.lessons_markdown().unwrap_or_else(|e| die(&e));
    if !lessons.is_empty() {
        die("rollback did not empty the LESSONS section — the causal lever is broken");
    }
    let a1 = run_eval("A1", &eval_tasks, &lessons, &args, &reporter);

    // 5 RE-APPLY → eval B2. RolledBack is terminal per hash, so this is the
    // whole governed path a second time: re-propose → approve → apply.
    let (ledger2, applied2) = mem
        .learn(args.llm_cmd.as_deref(), args.ground_cmd.as_deref(), t_learn2)
        .unwrap_or_else(|e| die(&e));
    eprintln!(
        "learn 2: {} ledger rows, {} applied",
        ledger2.entries.len(),
        applied2.len()
    );
    if applied2.is_empty() {
        die("re-apply pass applied nothing — B2 would silently equal A1");
    }
    let lessons = mem.lessons_markdown().unwrap_or_else(|e| die(&e));
    let b2 = run_eval("B2", &eval_tasks, &lessons, &args, &reporter);

    // 6 Report: config (every flag + git rev), the merged ledger verbatim
    // (failures are evidence), the four summaries.
    let mut ledger = Ledger::default();
    ledger.entries.extend(ledger1.entries);
    ledger.entries.extend(ledger2.entries);
    let config = json!({
        "bench": "selfimprove_aba",
        "workdir": args.workdir.display().to_string(),
        "db": mem.db_path().display().to_string(),
        "seed": args.seed,
        "experience": args.experience,
        "eval": args.eval,
        "mock": args.mock,
        "agent_cmd": args.agent_cmd,
        "llm_cmd": args.llm_cmd,
        "ground_cmd": args.ground_cmd,
        "max_turns": args.max_turns,
        "assert_shape": args.assert_shape,
        "runner_actor": mem.runner_actor(),
        "reviewer_actor": mem.reviewer_actor(),
        "phase_base_ms": BASE_MS,
        "git_rev": git_rev(),
    });
    let evals = [a0, b, a1, b2];
    reporter
        .write_report(&config, &ledger, &evals)
        .unwrap_or_else(|e| die(&format!("write report: {e}")));

    print_results(&args, &evals, &ledger, applied1.len(), applied2.len());

    if args.assert_shape {
        check_shape(&evals[0], &evals[1], &evals[2], &evals[3]);
    }
}
