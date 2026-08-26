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
  `https://openrouter.ai/api/v1`, provider pinned + fallbacks off, model +
  provider ids echoed into every transcript row).
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

## Roadmap — the benches this skeleton is built to grow

- **`passive memory` arm — the decisive comparison, and the next run.** A0
  here is an agent with *nothing* in its prompt, so the pilot proves the
  applied lesson caused the gain but not that curation beats retrieval. The
  arm to add renders raw recalled history (the failure grains themselves)
  into the prompt instead of the loop's lesson, on the same held-out set,
  with a deliberately *generous* budget so the comparison steelmans
  retrieval. `B > M` is the result that answers "a plain memory store does
  this too"; `B ≈ M` would mean the loop's value is cost and governance
  rather than accuracy, and that is the honest thing to report if it
  happens. Cheap: one extra eval pass, ~60 task runs.
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
- **LLM-authored executable lessons** (engine feature, not a bench): today
  `stamp_llm` hardcodes advisory `Flag`/`Data`, so a verified DISCOVER
  finding can never be applied — the governed path for an LLM to *author* a
  lesson grain (evidence-cited, GROUND/VERIFY-survived, human-applied,
  rollbackable) is the missing last mile. When it lands, this bench's
  causal arm extends to LLM-authored lessons with zero harness changes.

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
- Transcripts (`transcripts.jsonl`) carry every request/response, model id,
  provider id, and per-call usage; `report.json` carries config + git rev.
- CI runs `--mock --assert-shape` only (keyless-deterministic floor); no live
  keys in CI, no live numbers asserted.

## Reproduce (live pilot, ~$3–5)

```bash
export OPENROUTER_API_KEY=…
cargo run --release -p areev-bench --bin selfimprove_aba -- \
  --workdir /tmp/aba --seed 1 --experience 150 --eval 60 \
  --agent-cmd 'python3 crates/areev-bench/scripts/openrouter_toolcall.py qwen/qwen3-30b-a3b-instruct-2507' \
  --llm-cmd   'python3 crates/areev-bench/scripts/openrouter_toolcall.py qwen/qwen3-30b-a3b-instruct-2507' \
  --ground-cmd 'python3 crates/areev-bench/scripts/openrouter_toolcall.py deepseek/deepseek-chat'
```
