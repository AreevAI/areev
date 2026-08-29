//! Reproducibility pins for the published `selfimprove_aba` runs.
//!
//! A published A/B/A/B number is only meaningful if a later re-run is measuring
//! the SAME experiment. Model output is not deterministic, so nothing here
//! pins a score — what it pins is everything upstream of the model that a
//! re-run must reproduce exactly:
//!
//!   1. the generated task sets (prompt AND hidden ground truth) per seed
//!   2. the tool schemas the model is handed
//!   3. the governed pipeline's learned output — which lesson the analyzers
//!      propose over a fixed experience, and the exact LESSONS prompt bytes
//!      that lesson renders into across apply → rollback → re-apply
//!   4. each passive arm's rendered context
//!
//! If any of it drifts, the committed runs under `results/` stop being
//! comparable to anything produced afterwards, and the drift is invisible in
//! the numbers themselves — a re-run just lands somewhere else, with no way to
//! tell a changed loop from a changed task set. So it fails here instead.
//!
//! Bless after an INTENTIONAL change:
//!   GOLDEN_BLESS=1 cargo test -p areev-bench --test reproducibility
//! then review the diff and say in the commit message which published runs it
//! invalidates. That review is the whole point — a blessed diff is a decision,
//! not a formality.

use areev_bench::selfimprove::context::{
    AllProvider, ContextProvider, ExperienceGrain, LlmProvider, SteelProvider,
};
use areev_bench::selfimprove::memory::Memory;
use areev_bench::selfimprove::{agent, env};
use std::fmt::Write as _;
use std::path::PathBuf;

/// Engine phase clocks, pinned exactly as `selfimprove_aba` pins them.
const BASE_MS: i64 = 1_700_000_000_000;
const HOUR_MS: i64 = 3_600_000;

/// A short stable digest of a whole text stream.
///
/// `subject_fingerprint` is SHA-256 truncated to 64 bits. Truncation is fine
/// for the job here — this detects accidental drift in our own generator, not
/// an adversary constructing a collision — and reusing it keeps the bench off
/// a crypto dependency of its own (workspace policy: think twice before adding
/// a dependency).
fn digest(s: &str) -> String {
    areev_core::authz::subject_fingerprint(s)
}

fn golden_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/golden/reproducibility.txt")
}

fn assert_golden(actual: &str) {
    let path = golden_path();
    if std::env::var("GOLDEN_BLESS").is_ok() {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, actual).unwrap();
        eprintln!("blessed {}", path.display());
        return;
    }
    let expected = std::fs::read_to_string(&path).unwrap_or_else(|_| {
        panic!("missing {} — bless with GOLDEN_BLESS=1", path.display())
    });
    assert_eq!(
        actual,
        expected,
        "\nThe selfimprove inputs drifted from {}.\n\
         Every run under crates/areev-bench/results/ was produced against the \
         pinned values; a re-run after this change is NOT comparable to them.\n\
         If the change is intended: GOLDEN_BLESS=1 cargo test -p areev-bench \
         --test reproducibility, review the diff, and name the invalidated runs \
         in the commit message.\n",
        path.display()
    );
}

/// The task-set configurations the committed runs were produced at.
/// (`gen_tasks` is a sequential stream, so a larger n contains the smaller as
/// a prefix — both are listed anyway, because the published configs are what a
/// reader will re-run and a silent prefix relationship is not a guarantee.)
const PINNED_SETS: &[(u64, env::Split, usize, &str)] = &[
    // The 3-seed publication: results/selfimprove-3seed-qwen3-30b-2026-08-26
    (1, env::Split::Experience, 300, "3seed"),
    (1, env::Split::Eval, 100, "3seed"),
    (2, env::Split::Experience, 300, "3seed"),
    (2, env::Split::Eval, 100, "3seed"),
    (3, env::Split::Experience, 300, "3seed"),
    (3, env::Split::Eval, 100, "3seed"),
    // The original single-seed pilot (SELFIMPROVE.md "Live pilot").
    (1, env::Split::Experience, 150, "pilot"),
    (1, env::Split::Eval, 60, "pilot"),
];

fn split_name(s: env::Split) -> &'static str {
    match s {
        env::Split::Experience => "experience",
        env::Split::Eval => "eval",
    }
}

/// Section 1 — the task sets, prompt and hidden ground truth alike.
fn section_task_sets(out: &mut String) {
    out.push_str("## TASK SETS\n");
    out.push_str(
        "# digest covers every task's full fingerprint (prompt + hidden spec);\n\
         # task[0] is shown verbatim for orientation.\n",
    );
    for &(seed, split, n, run) in PINNED_SETS {
        let tasks = env::gen_tasks(seed, split, n);
        assert_eq!(tasks.len(), n);
        let stream: String =
            tasks.iter().map(|t| format!("{}\n", t.fingerprint())).collect();
        let _ = writeln!(
            out,
            "seed={seed} split={} n={n} run={run} digest={}",
            split_name(split),
            digest(&stream)
        );
        let _ = writeln!(out, "  task[0] {}", tasks[0].fingerprint());
    }
    out.push('\n');
}

/// Section 2 — the tool surface handed to the model.
fn section_tool_schemas(out: &mut String) {
    let schemas = env::tool_schemas();
    let compact = serde_json::to_string(&schemas).expect("schemas serialize");
    out.push_str("## TOOL SCHEMAS\n");
    let _ = writeln!(out, "digest={}", digest(&compact));
    let _ = writeln!(out, "{compact}");
    out.push('\n');
}

/// Drive the mock agent over the first `n` experience tasks of `seed` and
/// record every call into a fresh memory — the real capture path, not a
/// synthetic stand-in, so the pinned lesson is the one the pipeline actually
/// learns.
fn capture_experience(mem: &Memory, seed: u64, n: usize) {
    for task in env::gen_tasks(seed, env::Split::Experience, n) {
        let mut e = env::Env::new(&task);
        let mut backend = agent::MockBackend;
        let rec = agent::run_task(
            &mut backend,
            &mut e,
            &task.id,
            &task.prompt,
            "",
            None,
            24,
            |_| {},
        );
        mem.record_task(&rec).expect("record");
    }
}

/// The ledger as the pin cares about it: what was learned and how it was
/// dispositioned. Recommendation HASHES are deliberately excluded — they
/// commit to `created_at`, which is wall-clock at capture time, so pinning
/// them would make this test fail on the clock rather than on a change.
fn ledger_lines(ledger: &areev_bench::selfimprove::Ledger) -> Vec<String> {
    let mut lines: Vec<String> = ledger
        .entries
        .iter()
        .map(|e| format!("{} :: {} :: {}", e.disposition, e.source, e.summary))
        .collect();
    lines.sort();
    lines
}

/// Section 3 — the governed pipeline end to end: capture → analyze → apply →
/// rollback → re-apply, pinning what is learned and the prompt bytes it makes.
fn section_governed_pipeline(out: &mut String) {
    let dir = tempfile::TempDir::new().expect("tempdir");
    let mem = Memory::create(dir.path()).expect("memory");
    capture_experience(&mem, 1, 40);

    out.push_str("## GOVERNED PIPELINE (mock agent, seed 1, 40 experience tasks)\n");

    // A0 — captured, nothing applied.
    let a0 = mem.lessons_markdown().expect("lessons");
    let _ = writeln!(out, "A0.lessons_empty={}", a0.is_empty());

    // LEARN 1 → B.
    let (ledger1, applied1) = mem.learn(None, None, BASE_MS + HOUR_MS).expect("learn 1");
    let _ = writeln!(out, "learn1.applied_count={}", applied1.len());
    for line in ledger_lines(&ledger1) {
        let _ = writeln!(out, "learn1.ledger {line}");
    }
    let b = mem.lessons_markdown().expect("lessons");
    out.push_str("B.lessons:\n");
    out.push_str(&b);

    // ROLLBACK → A1: the file changes, so the prompt must empty.
    mem.rollback(&applied1, BASE_MS + 2 * HOUR_MS).expect("rollback");
    let a1 = mem.lessons_markdown().expect("lessons");
    let _ = writeln!(out, "A1.lessons_empty={}", a1.is_empty());

    // LEARN 2 → B2: a rolled-back hash is terminal, so this is a fresh
    // governed proposal — and it must restore the same prompt bytes.
    let (ledger2, applied2) = mem.learn(None, None, BASE_MS + 3 * HOUR_MS).expect("learn 2");
    let _ = writeln!(out, "learn2.applied_count={}", applied2.len());
    for line in ledger_lines(&ledger2) {
        let _ = writeln!(out, "learn2.ledger {line}");
    }
    let b2 = mem.lessons_markdown().expect("lessons");
    let _ = writeln!(out, "B2.lessons_identical_to_B={}", b2 == b);
    out.push('\n');
}

/// A fixed experience set for the arm renderers. Built by hand rather than
/// read back from the store: `experience_grains` orders by `created_at_ms`
/// then hash, and `created_at` is wall-clock, so store order is not a stable
/// thing to pin. What IS stable — and what the arms are — is the rendering.
fn arm_grains() -> Vec<ExperienceGrain> {
    let mk = |task: &str, tool: &str, code: Option<&str>, body: &str| {
        let is_error = code.is_some();
        ExperienceGrain {
            task_id: task.to_string(),
            tool: tool.to_string(),
            input_json: r#"{"customer_id":"cus_4bd720bc","amount":250}"#.to_string(),
            output_json: body.to_string(),
            is_error,
            code: code.map(str::to_string),
            rendered: areev_bench::selfimprove::context::render_line(
                tool, is_error, code, body,
            ),
        }
    };
    vec![
        mk("exp-0000", "refund", Some("rate_limited"),
           r#"{"error":{"code":"rate_limited","message":"too many requests","retry_after_s":4}}"#),
        mk("exp-0000", "refund", None, r#"{"refund_id":"rf_1"}"#),
        mk("exp-0001", "refund", Some("approval_required"),
           r#"{"error":{"code":"approval_required","message":"refunds over $100 require a valid manager_auth token"}}"#),
        mk("exp-0002", "log_case", Some("invalid_timestamp"),
           r#"{"error":{"code":"invalid_timestamp","message":"timestamp must be utc iso-8601"}}"#),
    ]
}

/// Section 4 — what each passive arm actually puts in front of the model.
fn section_passive_arms(out: &mut String) {
    let grains = arm_grains();
    let task_prompt = "Nadia Fournier <nadia.fournier@meridianco.example> reports a \
                       duplicate charge. Please put $250 back on their account.";

    out.push_str("## PASSIVE ARM CONTEXT\n");

    let steel = SteelProvider::build(grains.clone());
    let _ = writeln!(out, "m-steel.label={}", steel.label());
    let _ = writeln!(
        out,
        "m-steel.task_start={:?}",
        steel.task_start(task_prompt).expect("steel task_start")
    );
    let hook = steel
        .on_tool_error(
            task_prompt,
            "refund",
            "rate_limited",
            r#"{"error":{"code":"rate_limited","message":"too many requests","retry_after_s":4}}"#,
        )
        .expect("steel on_tool_error");
    let _ = writeln!(out, "m-steel.on_tool_error:\n{hook}");

    let all = AllProvider::build(grains.clone());
    let _ = writeln!(out, "m-all.label={}", all.label());
    let _ = writeln!(
        out,
        "m-all.task_start:\n{}",
        all.task_start(task_prompt).expect("all task_start")
    );

    let llm = LlmProvider::build_mock(grains);
    let _ = writeln!(out, "m-llm.label={}", llm.label());
    let _ = writeln!(
        out,
        "m-llm.task_start:\n{}",
        llm.task_start(task_prompt).expect("llm task_start")
    );
    out.push('\n');
}

/// One golden covering every input a published run depends on. Deliberately
/// ONE test and ONE file: these values are only meaningful together (a task
/// set is comparable to a published run only if the prompts and the learned
/// lesson are too), and a single diff is what a reviewer should be looking at
/// when deciding whether a change invalidates `results/`.
#[test]
fn published_run_inputs_are_pinned() {
    let mut out = String::new();
    out.push_str(
        "# selfimprove_aba reproducibility pins — see tests/reproducibility.rs.\n\
         # Drift here means the committed runs under results/ are no longer\n\
         # comparable to a fresh run. Bless deliberately, never reflexively.\n\n",
    );
    section_task_sets(&mut out);
    section_tool_schemas(&mut out);
    section_governed_pipeline(&mut out);
    section_passive_arms(&mut out);
    assert_golden(&out);
}

/// KNOWN DEFECT, pinned deliberately so a fix cannot land quietly.
///
/// `gen_tasks` derives its RNG state as `(seed ^ salt·K) | 1`. The `| 1`
/// keeps xorshift off its zero fixed point, but it also forces bit 0 — so
/// seeds that differ ONLY in bit 0 produce byte-identical task streams. Every
/// even/odd pair collides: 0≡1, 2≡3, 4≡5, …
///
/// That is why `results/selfimprove-3seed-qwen3-30b-2026-08-26` is three RUNS
/// over two distinct task sets: seeds 2 and 3 ran the same 100 held-out tasks
/// (verifiable in the committed transcripts — their `task_outcome` streams
/// match template-for-template). The seed-2 and seed-3 columns are therefore a
/// repeat measurement of one task set under a non-deterministic model, not two
/// independent replications, and "independently significant in every seed"
/// rests on two task sets rather than three.
///
/// This test asserts the CURRENT behavior on purpose. Fixing the derivation
/// changes seed 1's stream too, which invalidates every committed run — so the
/// fix is a publication decision, not a cleanup. When it is made, this test
/// fails, and that failure IS the prompt to re-run and re-publish.
#[test]
fn known_defect_even_odd_seed_pairs_collide() {
    let a = env::gen_tasks(2, env::Split::Eval, 50);
    let b = env::gen_tasks(3, env::Split::Eval, 50);
    let fp = |t: &[env::Task]| -> String { t.iter().map(|x| x.fingerprint()).collect() };
    assert_eq!(
        fp(&a),
        fp(&b),
        "seeds 2 and 3 no longer collide — the derivation was fixed. Every run \
         under results/ was produced with the old streams and must be re-run \
         before its numbers are quoted again."
    );
    // The pair that does NOT share bit 0 is genuinely distinct, which is what
    // makes this a bit-0 defect rather than a dead seed parameter.
    assert_ne!(fp(&env::gen_tasks(1, env::Split::Eval, 50)), fp(&a));
}

/// The restoration invariant, asserted rather than eyeballed in the golden:
/// B2 must put back exactly the prompt B had. It holds for a non-obvious
/// reason — a rolled-back recommendation hash is TERMINAL, so B2 is a fresh
/// proposal through the full gate — and if it ever stopped holding, the
/// A/B/A/B design would be measuring two different lessons.
#[test]
fn re_apply_restores_the_same_prompt_bytes() {
    let dir = tempfile::TempDir::new().expect("tempdir");
    let mem = Memory::create(dir.path()).expect("memory");
    capture_experience(&mem, 1, 40);

    let (_, applied1) = mem.learn(None, None, BASE_MS + HOUR_MS).expect("learn 1");
    assert!(!applied1.is_empty(), "the pinned experience must produce a lesson");
    let b = mem.lessons_markdown().expect("lessons");
    assert!(!b.is_empty(), "B must have lessons");

    mem.rollback(&applied1, BASE_MS + 2 * HOUR_MS).expect("rollback");
    assert_eq!(mem.lessons_markdown().expect("lessons"), "", "rollback must empty");

    let (_, applied2) = mem.learn(None, None, BASE_MS + 3 * HOUR_MS).expect("learn 2");
    assert!(!applied2.is_empty(), "restoration must re-apply");
    assert!(
        applied2.iter().all(|h| !applied1.contains(h)),
        "a rolled-back hash is terminal: restoration mints new recommendations"
    );
    assert_eq!(
        mem.lessons_markdown().expect("lessons"),
        b,
        "B2 must restore exactly B's prompt bytes — otherwise A/B/A/B compares two lessons"
    );
}
