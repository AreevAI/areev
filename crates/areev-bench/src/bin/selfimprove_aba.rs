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
//! `--arms` adds the passive-memory ladder (SELFIMPROVE.md "Bench 2") after
//! B2: one extra eval pass per arm over the SAME held-out tasks with the
//! LESSONS section EMPTY and a `ContextProvider` in its place. The arms are
//! the same store with the loop OFF — the delta isolates the loop, and it is
//! attributable because providers read only Tool grains, so the applied
//! lesson Facts living in the very same memory file cannot leak into an M
//! prompt.
//!
//! `--mock` is the keyless deterministic plumbing reference (CI runs
//! `--mock --assert-shape`); it is never a learning claim, and under `--mock`
//! an M arm proves the provider PLUMBING only — the mock obeys whatever
//! context it is handed, so it can never rank curation against retrieval.
//! Live numbers come from `--agent-cmd` (see SELFIMPROVE.md "Reproduce").
//!
//! `--workers N` parallelizes the eval and experience passes. It is an
//! optimization and never a variable: workers buffer their own transcript
//! rows, and the main thread writes rows (and the experience phase's
//! `record_task`) strictly in task-index order, so output is byte-identical
//! at any worker count and the single-writer store is only ever written from
//! one thread.

use areev_bench::selfimprove::context::{ContextProvider, ExperienceGrain};
use areev_bench::selfimprove::memory::Memory;
use areev_bench::selfimprove::{agent, context, env, report::Reporter};
use areev_bench::selfimprove::{EvalSummary, Ledger, RuleId, RuleStat, TaskRunRecord, Usage};
use serde_json::{json, Value};
use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::thread;

/// Engine phase clocks: fixed base (determinism rule — never the wall clock
/// when the value decides behavior), +1h per phase.
const BASE_MS: i64 = 1_700_000_000_000;
const HOUR_MS: i64 = 3_600_000;

/// The passive-memory arms, in ladder order (SELFIMPROVE.md "Bench 2").
const KNOWN_ARMS: [&str; 4] = ["m-steel", "m-all", "m-llm", "m-cmd"];

struct Args {
    workdir: PathBuf,
    seed: u64,
    experience: usize,
    eval: usize,
    mock: bool,
    agent_cmd: Option<String>,
    llm_cmd: Option<String>,
    ground_cmd: Option<String>,
    /// Requested passive arms, deduped, in the order given (empty = the
    /// A/B/A/B states only, i.e. exactly the pre-arms behavior).
    arms: Vec<String>,
    context_cmd: Option<String>,
    mllm_cmd: Option<String>,
    workers: usize,
    max_turns: u32,
    assert_shape: bool,
}

impl Args {
    fn wants(&self, arm: &str) -> bool {
        self.arms.iter().any(|a| a == arm)
    }

    /// The chat adapter the m-llm summarizer runs on: its own flag, else the
    /// agent's (same protocol, and one model is the honest default).
    fn mllm_cmd(&self) -> Option<&str> {
        self.mllm_cmd.as_deref().or(self.agent_cmd.as_deref())
    }
}

/// Arm → the `EvalSummary.state` label. `aba_stats.py` keys the passive arms
/// off the leading "M", so these strings are a cross-tool contract.
fn arm_state(arm: &str) -> &'static str {
    match arm {
        "m-steel" => "M-steel",
        "m-all" => "M-all",
        "m-llm" => "M-llm",
        "m-cmd" => "M-cmd",
        other => die(&format!("unknown arm {other}")),
    }
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
         \x20                       [--arms m-steel,m-all,m-llm,m-cmd]\n\
         \x20                       [--context-cmd 'CMD'] [--mllm-cmd 'CMD']\n\
         \x20                       [--workers N] [--max-turns N] [--assert-shape]"
    );
    std::process::exit(2);
}

/// `--arms` value → validated, deduped arm list preserving the given order.
/// An unknown name is a hard error: silently dropping it would publish an arm
/// table missing the arm someone asked for.
fn parse_arms(value: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for raw in value.split(',') {
        let arm = raw.trim();
        if arm.is_empty() {
            continue;
        }
        if !KNOWN_ARMS.contains(&arm) {
            eprintln!(
                "selfimprove_aba: unknown arm {arm:?} (known: {})",
                KNOWN_ARMS.join(", ")
            );
            usage()
        }
        if !out.iter().any(|a| a == arm) {
            out.push(arm.to_string());
        }
    }
    out
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
    let mut arms: Vec<String> = Vec::new();
    let mut context_cmd: Option<String> = None;
    let mut mllm_cmd: Option<String> = None;
    let mut workers: usize = 4;
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
                    "--arms" => arms = parse_arms(value),
                    "--context-cmd" => context_cmd = Some(value.clone()),
                    "--mllm-cmd" => mllm_cmd = Some(value.clone()),
                    // A worker count below 1 would run nothing at all.
                    "--workers" => {
                        workers = value.parse::<usize>().unwrap_or_else(|_| usage()).max(1)
                    }
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
    // The external-provider seam is required by, and only meaningful for, the
    // m-cmd arm — a --context-cmd nobody runs is a silently ignored flag.
    if arms.iter().any(|a| a == "m-cmd") != context_cmd.is_some() {
        eprintln!(
            "selfimprove_aba: --context-cmd is required for --arms m-cmd, and meaningless without it"
        );
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
        arms,
        context_cmd,
        mllm_cmd,
        workers,
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

/// One finished task, tagged with its index in the task list.
type PooledTask = (usize, TaskRunRecord, Vec<Value>);

/// Run `tasks` across `args.workers` threads and return one
/// `(record, transcript rows)` per task **in task order**.
///
/// The determinism argument, which is the whole design: a worker claims the
/// next index off an atomic counter, builds its OWN backend and its OWN `Env`
/// (both per task already), and buffers its transcript rows locally instead
/// of writing them. Nothing is written from a worker. The main thread sorts
/// by task index before it writes anything, so `--workers 4` emits exactly
/// the bytes `--workers 1` does, and the single-writer memory file is only
/// ever touched from one thread (the experience phase's `record_task`).
///
/// `run_task` takes only per-worker (`backend`), per-task (`Env`, the row
/// buffer) or `Sync` borrows (`Args`, `lessons`, and `ContextProvider`'s own
/// `Sync + Send` bound), which is what makes this safe to fan out at all.
fn run_pool(
    tasks: &[env::Task],
    args: &Args,
    lessons: &str,
    context: Option<&dyn ContextProvider>,
) -> Vec<(TaskRunRecord, Vec<Value>)> {
    let next = AtomicUsize::new(0);
    let (tx, rx) = mpsc::channel::<PooledTask>();
    let workers = args.workers.clamp(1, tasks.len().max(1));
    thread::scope(|scope| {
        for _ in 0..workers {
            let tx = tx.clone();
            let next = &next;
            scope.spawn(move || loop {
                let i = next.fetch_add(1, Ordering::Relaxed);
                let Some(task) = tasks.get(i) else { break };
                let mut e = env::Env::new(task);
                let mut backend = make_backend(args);
                let mut rows: Vec<Value> = Vec::new();
                let rec = agent::run_task(
                    backend.as_mut(),
                    &mut e,
                    &task.id,
                    &task.prompt,
                    lessons,
                    context,
                    args.max_turns,
                    |ev| rows.push(ev.clone()),
                );
                // A closed receiver means the main thread is already gone.
                if tx.send((i, rec, rows)).is_err() {
                    break;
                }
            });
        }
        // The workers hold the only remaining senders, so `rx` ends when the
        // last one finishes.
        drop(tx);
        let mut out: Vec<PooledTask> = rx.iter().collect();
        out.sort_by_key(|(i, _, _)| *i);
        // The index is dropped here, so every caller downstream pairs results
        // with tasks BY POSITION. That is only sound if this vector is exactly
        // one result per task, in order — verify it rather than assume it.
        //
        // A gap would not lose one task, it would silently misattribute every
        // task after the gap: `run_eval` builds each `task_outcome` row from
        // both sides (`task_id` and `success` off the record, `template` and
        // `rules_exercised` off the task), so a one-off shift would publish
        // per-rule denominators and paired-test rows against the wrong tasks —
        // wrong numbers that still look completely well-formed.
        if out.len() != tasks.len() {
            die(&format!(
                "worker pool returned {} results for {} tasks — results are paired \
                 with tasks by position, so a gap silently misattributes every \
                 task after it",
                out.len(),
                tasks.len()
            ));
        }
        for (slot, (i, _, _)) in out.iter().enumerate() {
            if slot != *i {
                die(&format!(
                    "worker pool result {slot} carries task index {i} — the \
                     position pairing downstream would be wrong"
                ));
            }
        }
        out.into_iter().map(|(_, rec, rows)| (rec, rows)).collect()
    })
}

/// One eval pass: the SAME held-out tasks in the SAME order, fresh Env per
/// task, `lessons` fetched once by the caller (empty on the passive arms,
/// which pass a `context` provider instead — never both). Eval tool calls are
/// NEVER recorded into memory: the held-out set must not leak into learning,
/// and the providers ingest only the experience phase for the same reason.
fn run_eval(
    state: &str,
    tasks: &[env::Task],
    lessons: &str,
    context: Option<&dyn ContextProvider>,
    args: &Args,
    reporter: &Reporter,
) -> EvalSummary {
    let mut tx = reporter
        .transcript(&format!("transcripts-eval-{state}.jsonl"))
        .unwrap_or_else(|e| die(&format!("open eval-{state} transcript: {e}")));
    let finished = run_pool(tasks, args, lessons, context);
    let mut records = Vec::with_capacity(tasks.len());
    for (task, (rec, rows)) in tasks.iter().zip(finished) {
        // Belt to `run_pool`'s braces: the row below draws half its fields
        // from `task` and half from `rec`, so they must be the same task.
        if rec.task_id != task.id {
            die(&format!(
                "eval-{state}: result for {} paired with task {} — the \
                 task_outcome row would mix two tasks",
                rec.task_id, task.id
            ));
        }
        for row in &rows {
            tx.row(row);
        }
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

/// Build one passive-memory provider over the captured experience.
///
/// This is the ONLY place coupled to `context.rs`'s constructors; everything
/// else in the bin talks to the frozen `ContextProvider` trait. The second
/// return is the summarizer's token cost — `Some` only for m-llm, whose model
/// calls at write time are precisely the thing its column has to price.
fn build_provider(
    arm: &str,
    grains: &[ExperienceGrain],
    args: &Args,
) -> (Box<dyn ContextProvider>, Option<Usage>) {
    match arm {
        "m-steel" => (Box::new(context::SteelProvider::build(grains.to_vec())), None),
        "m-all" => (Box::new(context::AllProvider::build(grains.to_vec())), None),
        "m-llm" => {
            // Mock mode summarizes deterministically and keylessly: CI runs
            // this arm, and it must not need a model or a key.
            let provider = if args.mock {
                context::LlmProvider::build_mock(grains.to_vec())
            } else {
                let cmd = args
                    .mllm_cmd()
                    .unwrap_or_else(|| die("m-llm needs --mllm-cmd or --agent-cmd"));
                context::LlmProvider::build(cmd, grains.to_vec())
                    .unwrap_or_else(|e| die(&format!("m-llm summarizer: {e}")))
            };
            let usage = provider.usage();
            (Box::new(provider), Some(usage))
        }
        "m-cmd" => {
            let cmd = args.context_cmd.as_deref().expect("validated at parse");
            let provider = context::CmdProvider::spawn(cmd, grains)
                .unwrap_or_else(|e| die(&format!("--context-cmd: {e}")));
            (Box::new(provider), None)
        }
        other => die(&format!("unknown arm {other}")),
    }
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
            // The passive ladder: no lessons in the prompt, a provider instead.
            "M-steel" => "M-steel per-error recall",
            "M-all" => "M-all  whole failure history",
            "M-llm" => "M-llm  summarized notes",
            "M-cmd" => "M-cmd  external provider",
            other => other,
        }
    }
    for e in evals {
        let mean_steps = if e.n == 0 { 0.0 } else { e.total_steps as f64 / e.n as f64 };
        // A mock M row is plumbing evidence, and the table outlives the
        // caveat under it the moment someone screenshots the rows — so the
        // row carries its own warning. The mock complies with any context it
        // is handed, which makes "more context" win by construction.
        let label = if args.mock && e.state.starts_with('M') {
            format!("{} ⚠ plumbing only", name(&e.state))
        } else {
            name(&e.state).to_string()
        };
        println!(
            "| {} | {}/{} | {:.1}% | {} | {:.1} |",
            label,
            e.successes,
            e.n,
            e.success_rate() * 100.0,
            e.tool_errors,
            mean_steps
        );
    }

    if !args.arms.is_empty() {
        println!(
            "\nM-* are the passive-memory arms: the same store with the loop OFF — no lessons in \
             the prompt, a context provider instead."
        );
        if args.mock {
            println!(
                "Under --mock they prove the provider plumbing only: the deterministic agent \
                 complies with any context it is handed, so these rows never rank curation \
                 against retrieval."
            );
        }
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

/// The applied lessons of one learn pass, identified by what they SAY rather
/// than by hash. Hashes cannot be compared across passes by design — a
/// rolled-back recommendation is terminal, so the restoring pass always mints
/// new ones — but the lesson content must match, and that is the thing worth
/// asserting.
fn applied_signatures(ledger: &Ledger) -> Vec<String> {
    let mut sigs: Vec<String> = ledger
        .entries
        .iter()
        .filter(|e| e.disposition == "applied")
        .map(|e| format!("{} :: {}", e.source, e.summary))
        .collect();
    sigs.sort();
    sigs
}

/// The CI gate for `--mock`: the shape of causality, not a magnitude claim.
/// (The A1 lessons-empty check is hard-asserted in the pipeline itself.)
///
/// `evals` is `[A0, B, A1, B2]` followed by one row per requested arm. The
/// arm checks are PLUMBING assertions and nothing more: the mock complies
/// with whatever context it is given, so "M-all beats A0" says the provider
/// reached the prompt, never that retrieval works.
///
/// The ledger checks are seed-independent and hold for a live run too: they
/// assert WHAT was learned, which the success-rate checks cannot see. A store
/// or analyzer change that quietly stopped one of two lessons from firing
/// would still clear every rate threshold above while making the run
/// incomparable to the published ones — `tests/reproducibility.rs` pins the
/// exact lesson text at a fixed seed; these pin the invariants at any seed.
fn check_shape(evals: &[EvalSummary], eval_n: u32, applied1: &[String], applied2: &[String]) {
    let (a0, b, a1, b2) = (&evals[0], &evals[1], &evals[2], &evals[3]);
    let mut fails = Vec::new();

    if applied1.is_empty() {
        fails.push(
            "the learn pass applied no lesson — B measures the same memory state as A0"
                .to_string(),
        );
    }
    // Restoration must be COMPLETE: B2 is only a re-apply of B if every lesson
    // B carried came back. A partial restore would silently make B2 a third
    // memory state that the report still labels as B's.
    for sig in applied1 {
        if !applied2.contains(sig) {
            fails.push(format!(
                "re-apply did not restore {sig} — B2 is not the same memory state as B"
            ));
        }
    }
    // LLM findings are advisory by design (`stamp_llm` emits `Proposal::Data`,
    // apply refuses). If one ever applies, the causal arm silently starts
    // measuring an LLM-authored lesson and SELFIMPROVE.md's honesty note goes
    // stale in the same moment.
    for sig in applied1.iter().chain(applied2) {
        if !sig.starts_with("loop.") {
            fails.push(format!(
                "applied a non-analyzer lesson ({sig}) — LLM findings are advisory by design"
            ));
        }
    }
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
    let arms = &evals[4..];
    for s in arms {
        if s.n != eval_n {
            fails.push(format!(
                "{} ran {} of {eval_n} held-out tasks — an arm must see the SAME set",
                s.state, s.n
            ));
        }
    }
    if let Some(m_all) = arms.iter().find(|s| s.state == "M-all") {
        if m_all.success_rate() <= a0.success_rate() + 0.05 {
            fails.push(format!(
                "M-all ({:.3}) must exceed A0 ({:.3}) by more than 0.05 — the mock uses any \
                 context it is given, so this failing means the provider never reached the prompt",
                m_all.success_rate(),
                a0.success_rate()
            ));
        }
    }
    if fails.is_empty() {
        println!("\nassert-shape: PASS (B > A0 + 0.10, |A1−A0| ≤ 0.05, |B2−B| ≤ 0.05, A1 lessons empty)");
        println!(
            "assert-shape: PASS ledger ({} lesson(s) applied, all analyzer-origin, all restored in B2)",
            applied1.len()
        );
        if !arms.is_empty() {
            let labels: Vec<&str> = arms.iter().map(|s| s.state.as_str()).collect();
            println!(
                "assert-shape: PASS arms {} (n = {eval_n} each; plumbing only under --mock)",
                labels.join(", ")
            );
        }
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
        // Workers run the tasks; the main thread alone writes — both the
        // transcript and the memory, in task-index order. The store is
        // single-writer per file, and the grain order must not depend on how
        // many threads happened to be free.
        for (rec, rows) in run_pool(&exp_tasks, &args, "", None) {
            for row in &rows {
                tx.row(row);
            }
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
    let a0 = run_eval("A0", &eval_tasks, &lessons, None, &args, &reporter);

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
    let applied_sig1 = applied_signatures(&ledger1);
    let lessons = mem.lessons_markdown().unwrap_or_else(|e| die(&e));
    let b = run_eval("B", &eval_tasks, &lessons, None, &args, &reporter);

    // 4 ROLLBACK → eval A1. The load-bearing honesty check: rollback changes
    // the FILE, and the file must change the PROMPT to empty.
    mem.rollback(&applied1, t_rollback).unwrap_or_else(|e| die(&e));
    let lessons = mem.lessons_markdown().unwrap_or_else(|e| die(&e));
    if !lessons.is_empty() {
        die("rollback did not empty the LESSONS section — the causal lever is broken");
    }
    let a1 = run_eval("A1", &eval_tasks, &lessons, None, &args, &reporter);

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
    let applied_sig2 = applied_signatures(&ledger2);
    let lessons = mem.lessons_markdown().unwrap_or_else(|e| die(&e));
    let b2 = run_eval("B2", &eval_tasks, &lessons, None, &args, &reporter);
    let mut evals = vec![a0, b, a1, b2];

    // 6 PASSIVE ARMS (--arms) — the same store with the loop OFF, run last so
    // the governed states are already measured and nothing about them moves.
    //
    // Two properties make an arm's number attributable. (a) The prompt: the
    // LESSONS section is EMPTY here and a `ContextProvider` supplies the
    // context instead — never both, asserted in `run_task`. (b) The source:
    // providers ingest `experience_grains()`, which reads only Tool grains
    // from the experience phase, so the applied lesson Facts sitting in this
    // very same memory file cannot leak into an M prompt, and neither can the
    // held-out set. One ingest per arm, before its pass — providers are
    // read-only afterwards, which is what makes the eval workers safe.
    if !args.arms.is_empty() {
        let grains = mem.experience_grains().unwrap_or_else(|e| die(&e));
        eprintln!("arms: {} experience grains ingested per provider", grains.len());
        for arm in &args.arms {
            let state = arm_state(arm);
            let (provider, summarizer) = build_provider(arm, &grains, &args);
            eprintln!("eval {state}: {} tasks", eval_tasks.len());
            let mut summary =
                run_eval(state, &eval_tasks, "", Some(provider.as_ref()), &args, &reporter);
            // The summarizer's model calls are part of what this arm COST —
            // an extraction-at-write-time memory that reported only its read
            // tokens would be priced dishonestly.
            if let Some(u) = summarizer {
                println!(
                    "m-llm summarizer: {} prompt + {} completion tokens",
                    u.prompt_tokens, u.completion_tokens
                );
                summary.usage.prompt_tokens += u.prompt_tokens;
                summary.usage.completion_tokens += u.completion_tokens;
            }
            evals.push(summary);
        }
    }

    // 7 Report: config (every flag + git rev), the merged ledger verbatim
    // (failures are evidence), the four governed summaries + any arms.
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
        "arms": args.arms,
        "context_cmd": args.context_cmd,
        "mllm_cmd": args.wants("m-llm").then(|| args.mllm_cmd()).flatten(),
        "workers": args.workers,
        "max_turns": args.max_turns,
        "assert_shape": args.assert_shape,
        "runner_actor": mem.runner_actor(),
        "reviewer_actor": mem.reviewer_actor(),
        "phase_base_ms": BASE_MS,
        "git_rev": git_rev(),
    });
    reporter
        .write_report(&config, &ledger, &evals)
        .unwrap_or_else(|e| die(&format!("write report: {e}")));

    print_results(&args, &evals, &ledger, applied1.len(), applied2.len());

    if args.assert_shape {
        check_shape(&evals, args.eval as u32, &applied_sig1, &applied_sig2);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use areev_bench::selfimprove::LedgerEntry;

    fn mock_args(workers: usize) -> Args {
        Args {
            workdir: PathBuf::new(),
            seed: 7,
            experience: 0,
            eval: 6,
            mock: true,
            agent_cmd: None,
            llm_cmd: None,
            ground_cmd: None,
            arms: vec![],
            context_cmd: None,
            mllm_cmd: None,
            workers,
            max_turns: 24,
            assert_shape: false,
        }
    }

    fn rows_of(finished: &[(TaskRunRecord, Vec<Value>)]) -> Vec<Value> {
        finished.iter().flat_map(|(_, rows)| rows.iter().cloned()).collect()
    }

    fn outcomes_of(finished: &[(TaskRunRecord, Vec<Value>)]) -> Vec<(String, bool, u32, String)> {
        finished
            .iter()
            .map(|(r, _)| (r.task_id.clone(), r.success, r.steps, r.failure_reason.clone()))
            .collect()
    }

    /// The pool is an optimization, never a variable. Transcript bytes and the
    /// memory write order both derive from task-INDEX order, so four workers
    /// must produce exactly what one does — otherwise `--workers` would
    /// quietly become a parameter of the published numbers.
    #[test]
    fn the_pool_is_deterministic_across_worker_counts() {
        let tasks = env::gen_tasks(7, env::Split::Eval, 6);
        let one = run_pool(&tasks, &mock_args(1), "", None);
        let four = run_pool(&tasks, &mock_args(4), "", None);

        assert_eq!(one.len(), tasks.len());
        // Task order, not completion order.
        let ids: Vec<String> = tasks.iter().map(|t| t.id.clone()).collect();
        assert_eq!(outcomes_of(&one).iter().map(|o| o.0.clone()).collect::<Vec<_>>(), ids);
        assert_eq!(outcomes_of(&one), outcomes_of(&four));
        // The rows are the transcript: identical sequence, identical bytes.
        let (a, b) = (rows_of(&one), rows_of(&four));
        assert!(!a.is_empty(), "the mock must actually call tools");
        assert_eq!(a, b);
        assert_eq!(
            serde_json::to_string(&a).unwrap(),
            serde_json::to_string(&b).unwrap()
        );
    }

    /// More workers than tasks must not spin up idle threads that then race
    /// for an empty queue — and an empty task list must not hang.
    #[test]
    fn the_pool_handles_more_workers_than_tasks_and_no_tasks() {
        let tasks = env::gen_tasks(7, env::Split::Eval, 2);
        let many = run_pool(&tasks, &mock_args(16), "", None);
        assert_eq!(many.len(), 2);
        assert!(run_pool(&[], &mock_args(4), "", None).is_empty());
    }

    /// Canned provider: proves `Option<&dyn ContextProvider>` really does
    /// cross into the workers (the trait's `Sync + Send` bound is what allows
    /// it), and that a shared provider stays deterministic under fan-out.
    struct ConstProvider;

    impl ContextProvider for ConstProvider {
        fn label(&self) -> &'static str {
            "m-const"
        }
        fn task_start(&self, _task_prompt: &str) -> Result<String, String> {
            Ok("- past runs hit rate_limited and invalid_timestamp\n".to_string())
        }
        fn on_tool_error(
            &self,
            _task_prompt: &str,
            _tool: &str,
            _code: &str,
            _body: &str,
        ) -> Result<String, String> {
            Ok(String::new())
        }
    }

    #[test]
    fn a_shared_provider_fans_out_and_stays_deterministic() {
        let tasks = env::gen_tasks(7, env::Split::Eval, 6);
        let provider = ConstProvider;
        let one = run_pool(&tasks, &mock_args(1), "", Some(&provider));
        let four = run_pool(&tasks, &mock_args(4), "", Some(&provider));
        assert_eq!(outcomes_of(&one), outcomes_of(&four));
        assert_eq!(rows_of(&one), rows_of(&four));
        // The context reached the agent: with the codes above the mock waits
        // out a 429 instead of hot-retrying it, which the bare pool does not.
        let bare = run_pool(&tasks, &mock_args(1), "", None);
        assert_ne!(
            outcomes_of(&one),
            outcomes_of(&bare),
            "a provider that names frozen codes must change the mock's behavior"
        );
    }

    #[test]
    fn parse_arms_dedupes_and_keeps_the_given_order() {
        assert_eq!(parse_arms(""), Vec::<String>::new());
        assert_eq!(parse_arms("m-all"), vec!["m-all"]);
        assert_eq!(
            parse_arms(" m-llm , m-steel ,m-all, m-llm ,"),
            vec!["m-llm", "m-steel", "m-all"]
        );
    }

    /// The arm labels are a cross-tool contract (`aba_stats.py` keys the
    /// passive arms off the leading "M"), so every known arm must map, and
    /// map to exactly that shape.
    #[test]
    fn every_known_arm_maps_to_an_m_prefixed_state() {
        for arm in KNOWN_ARMS {
            let state = arm_state(arm);
            assert!(state.starts_with("M-"), "{arm} → {state}");
            assert_eq!(state.to_ascii_lowercase(), arm);
        }
    }

    fn summary(state: &str, n: u32, successes: u32) -> EvalSummary {
        EvalSummary {
            state: state.to_string(),
            n,
            successes,
            tool_errors: 0,
            total_steps: n,
            per_rule: vec![],
            usage: Usage::default(),
        }
    }

    /// `check_shape` exits the process on failure, so the tests here pin the
    /// PASSING shape: the four governed states plus arms that ran the whole
    /// held-out set, with M-all clear of A0, and a ledger whose lessons are
    /// analyzer-origin and fully restored in the re-apply pass.
    #[test]
    fn check_shape_accepts_the_governed_states_with_arms() {
        let evals = vec![
            summary("A0", 10, 3),
            summary("B", 10, 9),
            summary("A1", 10, 3),
            summary("B2", 10, 9),
            summary("M-steel", 10, 6),
            summary("M-all", 10, 8),
        ];
        let applied = vec!["loop.tool_failure/1 :: Tool \"refund\" failed".to_string()];
        check_shape(&evals, 10, &applied, &applied);
    }

    /// The signature list is what makes the restoration check possible across
    /// passes: hashes differ by design (rolled-back is terminal), so identity
    /// has to come from the lesson's content, and order must not matter.
    #[test]
    fn applied_signatures_are_content_keyed_sorted_and_applied_only() {
        fn row(disposition: &str, source: &str, summary: &str) -> LedgerEntry {
            LedgerEntry {
                hash: "deadbeef".to_string(),
                source: source.to_string(),
                summary: summary.to_string(),
                disposition: disposition.to_string(),
                because: String::new(),
            }
        }
        let ledger = Ledger {
            entries: vec![
                row("applied", "loop.tool_failure/1", "refund rate_limited"),
                row("rejected", "loop.staleness/1", "FORGET something"),
                row("applied", "loop.tool_failure/1", "log_case invalid_timestamp"),
                row("advisory", "llm", "a finding"),
            ],
        };
        assert_eq!(
            applied_signatures(&ledger),
            vec![
                "loop.tool_failure/1 :: log_case invalid_timestamp".to_string(),
                "loop.tool_failure/1 :: refund rate_limited".to_string(),
            ],
            "applied rows only, content-keyed, sorted so pass order cannot matter"
        );
        // Two passes that learned the same thing in a different order must
        // compare equal — otherwise the restoration check would fire on noise.
        let reordered = Ledger { entries: ledger.entries.into_iter().rev().collect() };
        assert_eq!(applied_signatures(&reordered).len(), 2);
    }
}
