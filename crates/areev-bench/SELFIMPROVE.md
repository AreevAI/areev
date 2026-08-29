# The self-improvement benches — does the loop actually make an agent better?

`loop_precision` proves the analyzers *detect* seeded problems; `loop_reflection`
proves LLM findings *survive a skeptic*; the LoCoMo run proves *retrieval*.
None of them proves the claim that matters: **iteration N+1 succeeded because
of what the loop learned from iteration N.** The `selfimprove_*` bins close
that gap with causal, reproducible, LLM-driven experiments over a synthetic
tool environment with **known ground truth**.

Every number these bins emit is designed to be re-run by a stranger for a few
dollars, with every model call transcribed to a committed JSONL — the same
honesty contract as the LoCoMo run.

## The claim under test, stated precisely

> With the model, tools, prompts, and task distribution held fixed, an agent
> whose experience flows through Areev's capture → analyze → recommend →
> govern → apply loop performs better on **held-out tasks** than the same
> agent without the applied lessons — and the improvement is **caused by the
> applied lessons**, shown by removing them (rollback) and restoring them
> (re-apply).

Not under test: model-weight learning (Areev never trains), open-ended
world outcomes (docs/loop.md "the honest boundary").

## The environment — hidden rules, real ground truth

A deterministic, in-process "support desk" API (no network, seeded xorshift
like `bench.rs`). The agent gets tool schemas and tasks; it does **not** get
the operating rules, which are hidden in the tool implementations:

| # | hidden rule | naive failure it causes |
|---|---|---|
| R1 | `search_customers` paginates; results must be exhausted (`has_more`) | acts on the wrong/missing customer |
| R2 | `get_customer`/mutations require the canonical `cus_…` id from search, verbatim | id-guessing errors |
| R3 | refunds over $100 need a `manager_auth` token from `request_approval` first | authorization errors |
| R4 | refund must precede `cancel_subscription` (cancel-before-refund errors) | ordering failures |
| R5 | timestamps must be UTC ISO-8601 (`…Z`) | validation errors |
| R6 | a tool that returns 429 carries `retry_after_s`; only `wait(seconds ≥ retry_after_s)` then retry succeeds — immediate retries keep failing | retry storms |

Tasks are template-generated from a seeded entity pool with a **programmatic
success predicate** over final environment state (ledger entries, subscription
state, no orphaned approvals) — no LLM judge anywhere. Splits are disjoint:
EXPERIENCE tasks (the agent may fail and learn) and a HELD-OUT eval set
(different entities, paraphrased templates), fixed per `--seed`.

The rule list is a parameter of `env.rs`, not a constant — the learning-curve
and adversarial bins extend it without touching the harness.

## What the first live run found (and changed)

The first pilot — 150 tasks on Qwen3-30B via OpenRouter — is the reason the
`loop.tool_failure` denominator changed, and it is worth recording because it
is the failure mode this bench exists to catch.

The run captured 772 tool calls containing **139 genuine failures** (verified
in the stored grains: 139 `is_error=true`, matching 139 error bodies, zero
mismatches — the capture path was sound). The loop then proposed **nothing at
all**: `proposed 0 across 12 analyzer(s)`.

The cause was the rate denominator. Scored against every call to the tool, the
five real failure modes landed between 9% and 30% — under the 40% gate — and
none reached the absolute floor:

| tool | signature | count | tool's calls | old rate | recovered? |
|---|---|---|---|---|---|
| refund | rate_limited | 46 | 152 | 30.3% | never |
| log_case | invalid_timestamp | 33 | 114 | 28.9% | always |
| refund | approval_required | 33 | 152 | 21.7% | always |
| refund | cancel_before_refund | 14 | 152 | 9.2% | never |
| log_case | rate_limited | 13 | 114 | 11.4% | mostly |

The deeper point: the old denominator let **sibling failure modes mask each
other**, so the tools that broke in the most ways were the hardest to learn
from. It also meant the analyzer was effectively blind to a *competent* agent,
whose failures spread across signatures instead of concentrating in one — the
mock only ever fired lessons because it was pathologically naive (retry storms
→ 61% rates). A benchmark that only works against a bad agent proves nothing.

Scoring each mode against its own opportunities (the tool's successes plus that
cluster) has a property worth stating: **it encodes recovery for free**. A
failure the agent recovers from contributes its own compensating success to the
denominator and dilutes itself; one the agent never escapes does not. On this
trace the analyzer now fires on `refund/rate_limited` (46 failures, never
recovered — the agent called `wait()` and then simply stopped, failing the
task) and stays correctly silent on the 66 failures the agent self-corrected.

## Bench 1 (this one): `selfimprove_aba` — the A/B/A/B causal proof

```
1  EXPERIENCE   run N experience tasks; every tool call recorded into the
                memory (`record_tool_call`, errors flagged); no lessons in
                the prompt — the agent is ignorant by construction
2  LEARN        `loop` pass over that history with the LLM stages attached
                (DISCOVER → GROUND → VERIFY via command backends) plus the
                deterministic analyzers; scripted review applies the pending
                lessons under a distinct reviewer actor, recording the full
                ledger (applied / rejected / advisory).
                Honesty note: LLM DISCOVER findings are ADVISORY by design
                today (`Proposal::Data`, apply refuses) — they are counted
                in the ledger and scored for reliability, but the executable
                lessons that drive the causal arm are the analyzer proposals
                (`tool_failure` → `ADD fact`, rollbackable). The engine
                feature that makes verified LLM findings executable is on
                the roadmap below; this bench absorbs it without redesign.
3  EVAL A0      held-out set, prompt assembled from LIVE memory → no lessons
                exist yet at A0-time is *not* how we do it: A0 is measured
                with recommendations still pending (proposed, not applied),
                so the memory state is "experience captured, nothing applied"
4  APPLY → B    apply the lessons; re-run the SAME held-out set
5  ROLLBACK → A1  roll every applied lesson back; re-run
6  RE-APPLY → B2  restore the lessons; re-run. Rollback is TERMINAL for a
                recommendation hash (lifecycle: rolled_back never re-applies)
                — restoration goes through the engine again: a later loop
                pass re-proposes the same dedup_key ("the situation
                returned"), and scripted review approves + applies the fresh
                proposal. B2 therefore exercises the whole governed path a
                second time, which is exactly the claim under test.
```

The load-bearing honesty rule: **the eval prompt is assembled from live
memory on every run** (a CAL read over the file rendering active lessons into
a LESSONS section). Rollback changes the *file*; the file changes the
*prompt*. There is no harness flag that toggles behavior — the only lever is
Areev's own apply/rollback, so the A0→B→A1→B2 deltas are attributable to the
governed state and nothing else.

Fixed across all four evals: model, provider (pinned), temperature 0, tool
schemas, task set and order, max turns, token budget. Reported per state:
task success, per-rule recurrence, tool-error rate, mean steps, tokens, cost.

The recurrence metric counts **mishandling**, not error occurrence: an armed
rate limiter fires once at even a perfect agent, so "≥1 error" would read
100% forever. A task mishandles a rule when the same rule errors ≥2 times
(no adaptation), or errors once and the task fails for a reason attributable
to that rule (gave up on that wall); R1's silent wrong-customer shape is
attributed from final state, since no error ever fires for it. Attribution
is conservative — where counts can't distinguish a handled error from a
quit, the failure is counted against us (`selfimprove::mishandled_rules`).

### What the README gets

```text
held-out task success — same frozen model, same prompts, temp 0
  A0  before lessons          xx%
  B   lessons applied         xx%
  A1  lessons rolled back     xx%   ← the causal proof
  B2  lessons re-applied      xx%
```

plus the per-rule recurrence table (R1–R6 failure-rate before/after) and the
full governance ledger ("14 proposed, 9 applied, 3 rejected in review, 2
advisory-only") — the failures stay in the report; a clean sweep is
marketing, a ledger is evidence.

## Runner protocol — one JSON per line on stdio

The Rust bin owns the agent loop (task prompt, tool execution, turn cap,
transcripts). The model is a subprocess, same shape as `--tool-cmd`
connectors and `$AREEV_LLM_CMD`:

```
stdin : {"op":"chat","model":"…","messages":[…],"tools":[…],"temperature":0}
stdout: {"message":{"role":"assistant","content":"…","tool_calls":[
          {"id":"…","name":"…","arguments":"{…json…}"}]},
         "usage":{"prompt_tokens":N,"completion_tokens":N}}
```

- `scripts/openrouter_toolcall.py MODEL [--provider P]` — stdlib-only
  OpenAI-compatible adapter (key: `$OPENROUTER_API_KEY`, base
  `https://openrouter.ai/api/v1`). `--provider P` pins
  `{"order":[P],"allow_fallbacks":false}`; **without the flag nothing is
  pinned** and OpenRouter routes freely. The adapter returns the serving
  model and provider in a `meta` key on every response — which the harness
  currently discards (see "What the transcripts actually contain").
- `--mock` — a **built-in deterministic agent** (no subprocess, no key): it
  behaves naively unless the system prompt's LESSONS section names a rule it
  recognizes, in which case it complies. Mock mode exists to prove the
  *plumbing* end-to-end in CI (`--assert-shape` requires B > A0, A1 ≈ A0,
  B2 ≈ B) and is labelled as such everywhere — it is never a learning claim.

Loop LLM stages take the same kind of adapter via `--llm-cmd` / `--ground-cmd`
(the engine's `CommandLlm` protocol), so DISCOVER and GROUND can run on
different cheap models — the proposer never grades itself.

## Repo layout

```
crates/areev-bench/
  SELFIMPROVE.md                    this file — design + honesty rules
  src/selfimprove/
    mod.rs                          module root, shared config/types
    env.rs                          hidden rules, tools, task gen, scoring
    agent.rs                        agent loop, adapter subprocess, mock agent
    memory.rs                       record → loop → review → apply/rollback →
                                    lesson assembly (the Areev bridge)
    report.rs                       report.json / report.md / transcripts.jsonl
  src/bin/selfimprove_aba.rs        this bench
  scripts/openrouter_toolcall.py    the live AGENT adapter (chat + tools)
  scripts/openrouter_loop.py        the live LOOP adapter (DISCOVER/GROUND/
                                    VERIFY/ENRICH) — stdlib, forwards every
                                    payload key but `instructions`
  scripts/aba_stats.py              paired McNemar over the per-task rows
  results/                          committed transcripts + reports of runs
                                    quoted anywhere public
```

Two adapters, because the agent and the loop speak different protocols: the
agent adapter answers `op: "chat"` with tool calls, the loop adapter answers
`probe`/`discover`/`ground`/`verify`/`enrich`. Pointing `--llm-cmd` at the
agent adapter (or the reverse) fails at the loop's construction-time probe.

## Bench 2: the passive-memory arms — does the LOOP add value over the store?

The A/B/A/B pilot's baseline is an **unaided** agent, so it proves the applied
lesson caused the gain — not that curation beats retrieval. These arms close
that gap, and they are deliberately framed as **the same store with the loop
OFF**, not as an imitation of any competitor: the delta isolates the loop, and
there is no third-party retrieval implementation for anyone to dispute.

Three built-in variants form a ladder, so nobody can claim the weak one was
chosen. All run over the SAME captured experience, on the SAME held-out set,
after the governed states; the eval prompt carries the provider's context and
never the lesson (lessons live as Facts, providers read only Tool grains, so
nothing leaks between arms):

| arm | what enters the prompt | the objection it answers |
|---|---|---|
| `m-steel` | per-error structured retrieval: when a tool call fails, past grains matching that exact (tool, error code) are injected right at the decision point — a better hook than semantic similarity would get | "retrieval with the perfect hook would have caught it" |
| `m-all` | the full failure history rendered at task start, generous budget — the information upper bound | "you under-retrieved" |
| `m-llm` | the raw history summarized once into operator notes by an LLM after the experience phase (cost recorded), notes into every prompt | the extraction-at-write-time shape of LLM-memory products |

A fourth arm is a **seam, not an implementation**: `--context-cmd 'CMD'` runs
any external context provider over the protocol below, so any vendor can plug
their own memory into the identical experiment. We do not benchmark named
competitors in this repo; we publish the harness and the invitation.

### Pre-registered interpretation (written before the arms ever ran)

- **B > M:** curation beats retrieval on accuracy — and on cost, which the
  per-arm token columns quantify. The maximal claim.
- **B ≈ M:** the loop's value on this workload is cost, determinism, and
  governance, not accuracy: a one-line lesson against a per-turn context tax,
  zero model calls at write time against N, byte-stable re-derivation against
  extraction drift. That is what gets published, with those numbers.
- **B < m-all:** the lesson rendering leaves information on the table — a
  measured argument for the LLM-authored procedural lesson on the roadmap.
  Also published.

Whatever lands, the result ships with per-arm success, per-rule recurrence,
prompt/completion tokens, and model-call counts.

### The context-provider contract (frozen — code against this)

In-process trait (`src/selfimprove/context.rs`):

```rust
pub struct ExperienceGrain {          // one experience-phase tool call
    pub task_id: String,
    pub tool: String,
    pub input_json: String,
    pub output_json: String,
    pub is_error: bool,
    pub code: Option<String>,          // frozen error code when is_error
    pub rendered: String,              // the product renderer's one-line form
}

pub trait ContextProvider: Sync + Send {
    fn label(&self) -> &'static str;   // "m-steel" | "m-all" | "m-llm" | "m-cmd"
    /// Markdown for the system prompt's "## MEMORY (from passive recall)"
    /// section at task start; empty string = no section.
    fn task_start(&self, task_prompt: &str) -> Result<String, String>;
    /// Markdown appended to a failing tool result as
    /// "\n\n[memory] Relevant past experience:\n…"; empty = nothing.
    fn on_tool_error(&self, task_prompt: &str, tool: &str, code: &str, body: &str)
        -> Result<String, String>;
}
```

**Why the arms render their own line, and why that favours them.** The bench
renders each recalled grain as ``- `refund` call failed with `rate_limited`:
{body ≤160 chars}`` rather than calling `areev_cal`'s one-line *summary* form,
which emits `refund [FAIL]` — terse by design, and correct for a summary, but
it carries neither the error code nor the payload the arms exist to inject.
(The product's `sml` and `toon` formats do carry the body; only the summary
line does not.) The bench line mirrors the lesson renderer's shape so both
read alike in a prompt, and at 160 chars it preserves the whole error object
including `retry_after_s` — so an M arm sees strictly **more** raw detail than
the governed lesson's single summarizing line. The comparison is tilted toward
retrieval on purpose; that is what makes a win meaningful.

Ingest happens at construction (providers are read-only afterwards — that is
what makes eval workers safe). Caps, all logged when they truncate (no silent
caps): `m-all` ≤ 24_000 chars; `m-steel` ≤ 4_000 chars per injection, most
recent first; `m-llm` notes ≤ 4_000 chars, summarizer input = the error grains
plus per-tool call counts.

External providers (`--context-cmd`): ONE persistent process per eval pass,
newline-delimited JSON, calls serialized:

```
→ {"selfimprove":1,"op":"ingest","grains":[{…ExperienceGrain fields…}]}
← {"ok":true}
→ {"op":"context","stage":"task_start","task_prompt":"…"}
← {"context":"…markdown or empty…"}
→ {"op":"context","stage":"tool_error","task_prompt":"…","tool":"…","code":"…","body":"…"}
← {"context":"…"}
```

**The provider owns its framing; the caller is idempotent as a guard.**
`task_start` returns a COMPLETE section (heading included) and `on_tool_error`
a COMPLETE block (prefix included), mirroring `memory::lessons_markdown`,
which owns its own `## LESSONS` heading — so all four arms emit structurally
identical prompt bytes, which is what makes them comparable. The caller adds
a marker only when it is absent, because double-framing is merely cosmetic
while no-framing would read as "no memory at all" to the mock's marker scan
instead of failing loudly.

Ordering note: `GrainRecord` exposes no sequence number, so recording order is
`created_at_ms` then `hash`. "Most recent first" therefore holds across tasks;
calls recorded inside one millisecond fall back to hash order — deterministic
and stable across runs, which is what the prompt bytes require.

The mock agent treats codes found in "## MEMORY" sections and "[memory]"
blocks exactly like LESSONS codes — it models "an agent that uses whatever
context it is given", so mock M arms prove plumbing, never a comparison.

## Roadmap — the benches this skeleton is built to grow

- `selfimprove_curve` — the learning curve: task stream with loop checkpoints
  every K tasks, arms `vanilla | passive-memory | loop-deterministic |
  loop+LLM | placebo(shuffled lessons)`, cumulative + held-out curves, 3+
  seeds. Reuses env/agent/memory/report unchanged; adds the arm switch and
  checkpoint schedule.
- `selfimprove_adversarial` — the governance differentiator: a poisoned
  experience stream (misleading failures, planted bad lessons); measures
  lesson precision and held-out damage, gated loop vs a naive
  write-everything memory baseline.
- deterministic-analyzer showcase — same A/B/A shape with `--llm-cmd` absent:
  what do contradiction/duplicate/staleness lessons alone buy? (The engine
  closes with zero model calls; this bench prices that floor.)
- **LLM-authored executable lessons**: **landed end-to-end.** Engine: a
  DISCOVER draft may author a `lesson` (one capped imperative line); a
  lesson-bearing draft that survives GROUND/VERIFY stamps as an applicable,
  rollbackable `ADD fact` proposal (`relation = "lesson"`, namespace from
  the cited evidence) — human-applied only, auto-apply stays structurally
  closed to `origin=llm` (`docs/loop.md`, DISCOVER). Bench: `--llm-lessons`
  is the arm switch (scripted review approves + applies authored lessons;
  `lessons_markdown` renders them as a "Rules learned" block), `--mock-llm`
  is its keyless canned backend, and CI asserts the full authored-lesson
  lifecycle (`apply → render → rollback-empties → re-apply restores`) on
  every push. The default path — no `--llm-lessons` — reproduces the
  published runs' review policy and prompt bytes exactly. Live numbers for
  this arm are not yet published; the roadmap entry for them is the
  `selfimprove_curve` `loop+LLM` arm above.

## Determinism & stats rules (areev-testing applies)

- Everything env-side is seeded; `--seed` reproduces task sets exactly.
- Live model outputs are NOT deterministic even at temp 0 — publishable runs
  use ≥3 seeds and report per-seed numbers plus a paired McNemar test on
  A0-vs-B over the shared task set; single-seed output is labelled `pilot`.
  Every eval transcript carries a `kind: "task_outcome"` row per task (the
  paired unit — aggregates cannot support a paired test, and per-task
  outcomes cannot be reconstructed after the run). Feed the run dirs to
  `scripts/aba_stats.py`, which reports discordant counts and an exact
  two-sided p for A0→B, B→A1 and A1→B2: the causal claim needs all three,
  not just the first.
- `report.json` carries config + git rev; the transcripts carry the run's
  behaviour, with the limits stated below.
- CI is keyless throughout — no live keys, no live numbers asserted. It runs
  `--mock --assert-shape` three times (with the arms, governed-only, and
  `--mock-llm --llm-lessons` for the authored-lesson lifecycle) and
  `verify_run.py` over the committed results; the reproducibility pins run
  with the ordinary test suite.

### What the transcripts actually contain

Precisely, because "we publish the receipts" is the claim this benchmark
trades on and the receipts are narrower than they sound. Every committed
transcript holds exactly two row shapes:

| row | fields |
|---|---|
| one per executed tool call | `task_id`, `turn`, `tool`, `args`, `is_error`, `code` |
| one per task (`kind: task_outcome`) | `state`, `task_id`, `template`, `success`, `failure_reason`, `steps`, `tool_errors`, `rules_exercised`, `mishandled`, `prompt_tokens`, `completion_tokens` |

Plus `error` / `provider_error` rows when a backend or a context provider
fails. That is enough to recompute every published number
(`scripts/verify_run.py` does exactly that) and to run the paired tests.

It is **not** a record of the model calls. Prompts, completions, the serving
model id and the serving provider id are not written — the adapter returns
model and provider in `meta`, and `run_task` drops it. So a committed run
cannot answer "which provider served this task", and with no `--provider`
pin (the published runs pass none) a single run may have been served by
several. Treat that as an uncontrolled variable in any cross-run comparison,
and pass `--provider` when a comparison depends on it.

Recording the meta per turn is a small change and the obvious next
improvement; it is called out here rather than implied away.

## Keeping the published runs reproducible

A published number is only meaningful if a later run is measuring the same
experiment. The model is not deterministic, so nothing pins a score — what is
pinned is everything upstream of the model, plus the evidence downstream of
it. Three keyless gates, all in CI:

| gate | what it protects | fails when |
|---|---|---|
| `tests/reproducibility.rs` → `tests/golden/reproducibility.txt` | the task sets (prompt **and** hidden ground truth) per seed, the tool schemas, the governed pipeline's learned lesson, the LESSONS prompt bytes across apply→rollback→re-apply, and each arm's rendered context | any of it drifts — i.e. the runs under `results/` stop being comparable to a fresh run |
| `--assert-shape` ledger checks | *what* was learned, not just that the rates moved: ≥1 lesson applied, every applied lesson analyzer-origin (LLM findings stay advisory), and every lesson B applied restored in B2 | an analyzer or store change silently changes which lesson fires, or a partial restore makes B2 a third memory state |
| `scripts/verify_run.py` | the committed evidence: every published per-state and per-rule number recomputed from the `task_outcome` rows, identical task ids across states (the paired-test precondition), and `MANIFEST.md` checksums over every file | a published number stops matching its own transcripts, or a published file is renamed/overwritten |

The third exists because it has already happened: the single-seed pilot's
transcripts were renamed and overwritten in place by the three-seed run, and
nothing in review caught it.

**Blessing the golden is a publication decision, not a cleanup.**
`GOLDEN_BLESS=1 cargo test -p areev-bench --test reproducibility` regenerates
it; the commit message must name which runs under `results/` the change
invalidates.

### Known defect: even/odd seed pairs collide

`gen_tasks` derives its RNG state as `(seed ^ salt·K) | 1`. The `| 1` keeps
xorshift off its zero fixed point — and also forces bit 0, so seeds differing
only in bit 0 generate byte-identical task streams. Every pair collides: 0≡1,
2≡3, 4≡5, …

The consequence for the three-seed publication: **seeds 2 and 3 ran the same
100 held-out tasks** (verifiable in the committed transcripts — their
`task_outcome` streams match template-for-template). Those two columns are a
repeat measurement of one task set under a non-deterministic model, not two
independent replications, so the run spans two distinct task sets rather than
three. The pooled statistics and the causal reading survive (A0→B, B→A1 and
A1→B2 are each significant within seed 1 alone); "independently significant in
every seed" is the phrase that overstates what is there.

Fixing the derivation changes seed 1's stream too, which invalidates every
committed run — so the fix is a re-publication, not a patch, and it is pinned
by `known_defect_even_odd_seed_pairs_collide` until then. That test failing IS
the prompt to re-run.

## Flags

| flag | what it does |
|---|---|
| `--workdir PATH` | run directory; refuses a pre-existing `bench.db` (a stale memory would poison A0) |
| `--seed N` `--experience N` `--eval N` | task generation; `--seed` reproduces the task sets exactly |
| `--mock` \| `--agent-cmd 'CMD'` | exactly one: the deterministic keyless agent, or a chat adapter |
| `--llm-cmd` / `--ground-cmd 'CMD'` | the loop's DISCOVER/VERIFY and GROUND backends (**loop** protocol) |
| `--mock-llm` | keyless canned loop-LLM (authors one fixed lesson) in place of `--llm-cmd`; the two are mutually exclusive |
| `--llm-lessons` | the **loop+LLM arm**: the scripted review also approves + applies LLM-authored lessons, and they render into LESSONS. Requires `--llm-cmd` or `--mock-llm`. Off = the published-run review policy, byte-for-byte |
| `--arms LIST` | comma list of `m-steel,m-all,m-llm,m-cmd`; empty = governed states only |
| `--context-cmd 'CMD'` | the external context provider; required by (and only by) `m-cmd` |
| `--mllm-cmd 'CMD'` | chat adapter for the `m-llm` summarizer; defaults to `--agent-cmd`, unused under `--mock` |
| `--workers N` | eval concurrency (default 4). Output is byte-identical at any N: workers buffer, the main thread writes in task order and is the only writer of the memory |
| `--max-turns N` `--assert-shape` | turn cap; the CI shape gate |

**Two adapters, two protocols.** `--agent-cmd`/`--mllm-cmd` speak the chat +
tool-call protocol (`openrouter_toolcall.py`); `--llm-cmd`/`--ground-cmd`
speak the loop's `probe`/`discover`/`ground`/`verify` protocol
(`openrouter_loop.py`). Crossing them fails at the loop's construction-time
probe, which is the intended loud failure.

## Reproduce

Keyless floor (what CI runs — plumbing only, never a learning claim):

```bash
cargo run -p areev-bench --bin selfimprove_aba -- \
  --workdir /tmp/aba --seed 1 --mock --assert-shape \
  --arms m-steel,m-all,m-llm --workers 4
```

Live pilot, governed states only (~$0.35, measured — see "What a re-run costs"):

```bash
export OPENROUTER_API_KEY=…
AGENT='python3 crates/areev-bench/scripts/openrouter_toolcall.py qwen/qwen3-30b-a3b-instruct-2507'
cargo run --release -p areev-bench --bin selfimprove_aba -- \
  --workdir /tmp/aba --seed 1 --experience 150 --eval 60 --agent-cmd "$AGENT" \
  --llm-cmd    'python3 crates/areev-bench/scripts/openrouter_loop.py qwen/qwen3-30b-a3b-instruct-2507' \
  --ground-cmd 'python3 crates/areev-bench/scripts/openrouter_loop.py deepseek/deepseek-chat'
```

Full comparison — governed states **and** the passive-memory arms. Run it at
three seeds and feed all three run dirs to `scripts/aba_stats.py`:

```bash
cargo run --release -p areev-bench --bin selfimprove_aba -- \
  --workdir /tmp/aba-s1 --seed 1 --experience 300 --eval 100 --workers 4 \
  --agent-cmd "$AGENT" --mllm-cmd "$AGENT" \
  --llm-cmd    'python3 crates/areev-bench/scripts/openrouter_loop.py qwen/qwen3-30b-a3b-instruct-2507' \
  --ground-cmd 'python3 crates/areev-bench/scripts/openrouter_loop.py deepseek/deepseek-chat' \
  --arms m-steel,m-all,m-llm
```

Then verify what you produced against what shipped:

```bash
python3 crates/areev-bench/scripts/verify_run.py <run-dir>                    # audit
python3 crates/areev-bench/scripts/verify_run.py <run-dir> --write-manifest   # publish
```

### What a re-run costs

Measured, not estimated — the committed three-seed run spent **$2.30** over
2,800 task-runs. Its eval phases alone account for 31.1M prompt and 0.51M
completion tokens (recomputable: `verify_run.py` sums them from the
transcripts), so re-price it against whatever the provider charges today
rather than trusting a figure in a doc.

| what | scale | cost |
|---|---|---|
| full three-seed replication | 2,800 task-runs, 4 states + 3 arms | **$2.30** |
| one seed, governed states only (150/60) | ~390 task-runs | **~$0.35** |
| keyless floor (`--mock`) | any | **$0** |

The prompt-token total dwarfs completion by 60×, which is the shape of this
benchmark: long tool schemas and a growing message history re-sent every turn.
Arms that inject more context (m-all at 6.2× the prompt tokens) cost
proportionally more, which is exactly the axis the arm comparison prices.
