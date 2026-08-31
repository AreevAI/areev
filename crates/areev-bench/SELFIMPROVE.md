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

And three **silent** rules (added 2026-08-31), which break every call
returning 200 — there is no error code, no error body, nothing to normalize:

| # | hidden rule | how it fails |
|---|---|---|
| R7 | closing an account must leave a case behind (the closure is auditable) | refund + cancel both succeed; the task is scored `no_closure_case` |
| R8 | an **enterprise**-plan account needs a manager approval token behind ANY refund, not only one over $100 | the refund succeeds; scored `unapproved_enterprise_refund` |
| R9 | a case about the regulated topic (a data export request) must be filed `priority: high` | `log_case` accepts it happily; scored `case_missing_priority` |

Tasks are template-generated from a seeded entity pool with a **programmatic
success predicate** over final environment state (ledger entries, subscription
state, no orphaned approvals) — no LLM judge anywhere. Splits are disjoint:
EXPERIENCE tasks (the agent may fail and learn) and a HELD-OUT eval set
(different entities, paraphrased templates), fixed per `--seed`.

The rule list is a parameter of `env.rs`, not a constant — the learning-curve
and adversarial bins extend it without touching the harness.

## The silent archetypes — why R7-R9 exist

The 2x2 below could not be executed, and its stated bound was that this
workload's rules "are all tool-failure-shaped, which is precisely what
signature clustering detects, so it gives an LLM no headroom by
construction." R7-R9 are that bound, removed.

Each is a shape a signature clusterer **structurally cannot reach**, because
clustering normalizes error bodies and these produce none:

- **R7 is a missing branch** — a whole class of task (closures) needs a step
  the desk never learned to take. The distributional archetype: the work
  arrives, the plan has no branch for it, nothing errors.
- **R8 is a mis-set threshold** — the approval rule is right in shape and
  wrong in its cutoff for one segment. Over $100 the existing R3 already
  forces a token, so R8 only ever bites *below* the threshold: the learnable
  signal is "small refunds for enterprise customers get rejected", which is a
  correlation, not a signature.
- **R9 is a recurring topic with a special handling rule** — the desk's own
  traffic contains the pattern; no tool objects.

Finding any of them means correlating **outcomes** across episodes. That is
why `memory::record_task` now writes one EPISODE fact per finished task next
to the tool calls: a memory holding only tool calls cannot express a silent
rule at all, so "the LLM found nothing" would have been a fact about the
harness rather than about the model. The episode carries the observable shape
of the run (plan, which tools ran, whether approval was requested, whether a
case was filed and at what priority) and whether the outcome was accepted —
and deliberately **not** the scored `failure_reason` or the attributed rule,
which name the answer. Every field but the accept/reject bit is derived from
the agent's own calls.

**What this invalidates.** Adding R7-R9 changes ground truth, so every
`selfimprove` run under `results/` is a measurement of the six-rule
environment and is not comparable to a re-run at this rev. The task PROMPTS
and pools are byte-identical across the change (the RNG stream did not move),
so the committed transcripts are still exactly what those models saw — what
changed is the scorer. Each run's MANIFEST.md now says so, and the runs stay
published. `tests/golden/reproducibility.txt` was re-blessed in the same
commit; its diff is the task rule-surfaces, the `regulated` flag, and
`log_case`'s new optional `priority` — the governed-pipeline and passive-arm
sections are unchanged, which is the check that the deterministic loop itself
did not move.

**What the keyless path covers, exactly.** The `MockLoopLlm` correlates
bundled episode facts and authors the matching lesson through the engine's
`proposal` vocabulary, so the whole silent-rule path runs with no key:
episodes recorded → DISCOVER bundle carries them → correlation → GROUND +
VERIFY → scripted review → applied lesson → rendered into the prompt. R7 is
walked end to end through the real engine by
`silent_rule_lesson_travels_from_episodes_to_the_prompt`.

R8 and R9 are **not** reachable by the deterministic mock agent, and the
reason is worth stating rather than papering over: R8's correlation needs the
plan, which is only observable if the agent looked the customer up, and the
mock's refund path never calls `get_customer`; R9's needs a case that was
filed successfully, and the naive mock's `log_case` fails R5 first. A live
agent does both routinely. Their correlations are therefore pinned directly
by `each_silent_rule_correlates_from_its_own_episode_shape` rather than left
for a paid run to discover on our behalf.

The correlations are canned, exactly like every other branch of that backend.
It proves the path exists; it is never a claim that a model would find these.

**What two live smokes changed (2026-08-31, qwen3-30b, n=20/30 — plumbing,
not measurement).** Both completed the full A/B/A/B cycle, and R7/R8/R9 all
fired against a live agent, which the mock could not reach. Two things came
out of them and are fixed here:

- **The model saw the episodes and wrote about the error text anyway.** All
  four authored lessons in the first run restated clusters `tool_failure`
  had already produced. The DISCOVER instruction never said that outcome
  records exist, that restating a deterministic finding is worthless, or
  that rejected-versus-accepted is a way to find something. With that added
  (generically — it names no rule and no field), the next run authored a
  lesson reasoning explicitly over "rejected episodes".
- **R8 and R9 were unmeasurable.** At the published config they were
  exercised by 9 and 7 of 100 held-out tasks — dose too thin for any
  per-rule claim, which is precisely what killed the 2x2. The account mix
  now carries more enterprise and two of four topics are regulated, giving
  13 and 15; `every_rule_gets_enough_opportunities_to_be_measurable` is the
  permanent guard, and it fails the build rather than letting an
  underpowered table get published. Prompts are byte-identical across that
  change — only the `plan` column and the regulated-topic flag moved.

Neither smoke is evidence about learning: n=10 held-out is noise, and the
success columns are reported here as what they are.

**A silent rule can be masked by an error-shaped one, and that shows up in
the tables.** At A0 the mock fails most refund tasks on R6/R4 and never
reaches the point where R8 or R9 would bite, so both read 0 mishandled; at B,
with the tool-failure lessons applied, the agent gets past those walls and
the silent rules start scoring (R8 0/4 → 2/4, R9 0/3 → 2/3). Read naively
that looks like the loop made things worse. It is the same effect the
`tool_failure` denominator fix was about: **a rule only becomes visible once
the agent is competent enough to reach it**, so a per-rule row moving up
after an apply can mean the agent got further, not that it got worse. Any
published reading of R7-R9 has to say which walls the agent was clearing at
that state.

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
                Which ORIGINS the review admits is the arm (`--llm-lessons`,
                `--no-analyzer-lessons`): by default only analyzer proposals
                (`tool_failure` → `ADD fact`, rollbackable) are applied and
                LLM findings stay advisory — the published runs' policy. A
                gate-surviving LLM draft that authored a lesson is applicable
                too; every finding of either origin is ledgered regardless of
                whether its arm admits it.
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

### Pre-registered interpretation — the loop+LLM arm (written before its three-seed run)

Committed before the runs (2026-08-29): seeds **1, 3, 5** — three *distinct*
task streams (odd seeds; the even/odd collision below makes n and n+1
collide, so 1/2/3 would be two streams again) — 300 experience / 100
held-out per seed, agent + DISCOVER/VERIFY on `qwen3-30b`, GROUND on
`deepseek-chat`, four states per config per seed, **two configs at one git
rev**: governed-only and `--llm-lessons`. Paired exact McNemar, pooled and
per-seed. Context: the seed-1 pilot that motivated this showed the authored
lesson **eliminating its target rule** (R5 6/25 → 0/25 at state B) while R4
went 3/25 → 15/25 — net B_llm − B_det = −11 pts, p = 0.08, single seed.

- **B_llm > B_det (pooled, significant):** authored remedies add accuracy
  over signature lessons. Published as the arm's headline.
- **B_llm ≈ B_det, with the pilot's per-rule split** (target rule
  eliminated, unstated rules regress): the publishable claim is the
  *mechanism* — an authored remedy is locally perfect and globally
  hazardous, because stating the fix suppresses the inference that
  signature lessons force; the loop's post-apply re-measurement
  (`outcome_review`) is what makes such a change safe to try at all.
  Published with the full per-rule tables.
- **B_llm < B_det (significant):** the same claim, stronger — and
  deterministic signature lessons stay the default lesson kind.
- Whatever lands, it publishes **only if the arm's own causal structure
  holds** (A0→B, B→A1, A1→B2 each significant pooled) — otherwise the
  result is "no causal signal", published as that.
- Stated up front: on re-apply the LLM **re-authors** its lesson — unlike
  analyzer lessons, re-derivation is not byte-deterministic, so the arm's
  B2 tests the governed re-proposal path, not byte-identical restoration.

Every outcome ships with per-seed numbers, per-rule recurrence, token
costs, the governance ledgers, and the full transcripts.

**Outcome (2026-08-30): the third branch.** B_llm < B_det, pooled b=26 c=52,
p=0.0043, with the arm's own causal chain intact (the publication condition)
and the A0 baseline null (p=0.1996, so the gap is not drift). Deterministic
signature lessons stay the default. Two things the pre-registration did not
anticipate, both reported: the LLM does not author on every pass, which made
seed 3 a dose-0 internal control and turned the result into a dose-response;
and the B2 caveat proved stronger than written — an authored lesson is not
merely re-authored rather than re-derived, it may not be re-authored at all.
Full write-up: RESULTS.md, "Does an LLM write better lessons?".

### Pre-registered interpretation — the 2x2, does LLM-authored learning self-improve ALONE (written before its runs)

Committed 2026-08-30, after the first arm result and before any run of this
design. The earlier comparison was `(deterministic)` vs
`(deterministic + LLM)`; it never isolated the LLM, so it could not answer
whether LLM-authored lessons self-improve **on their own**. This does.

**Design.** Seeds 1/3/5, 300/100, same model pair, four cells over one axis
each — *which lesson ORIGINS the review gate admits*. The analyzers always
run, in every cell, so discovery is held constant and only application
varies:

| cell | analyzer lessons | LLM lessons | status |
|---|---|---|---|
| control | applied | advisory | **reused** — 2026-08-30 `governed-*` |
| combined | applied | applied | **re-run** at this rev |
| llm-only | advisory | applied | **new** (`--no-analyzer-lessons`) |
| neither | advisory | advisory | **is state A0 by construction** — already measured in every run |

Two runs are new because an engine fix at this rev changes what the LLM can
see: the DISCOVER evidence bundle now seeds recent **Tool error grains**, not
only Facts and Observations. Before it, the LLM saw a tool failure only if a
deterministic finding happened to cite it — it could elaborate on what
clustering caught but never find what clustering missed, which is the one
thing it is there for. Every earlier LLM number was measured under that
handicap and is superseded for comparison. The control is reused rather than
re-run because the fix cannot change which lessons it applies (LLM findings
stay advisory there); the A0 drift check below is what would catch that
assumption being wrong.

**The primary question is the llm-only cell's own causal chain** — A0→B,
B→A1, A1→B2, each pooled-significant, which is the same bar the deterministic
loop had to clear.

- **Chain holds:** LLM-authored lessons self-improve on their own. Published
  as that, with the effect size and the comparison against the deterministic
  cell stated side by side — parity or a loss there does not retract it.
- **Chain does not hold:** they do not, on this workload at this dose.
  Published as that, and NOT as "LLM learning does not work": this workload's
  six hidden rules are all tool-failure-shaped, which is precisely what
  signature clustering detects, so it gives an LLM no headroom by
  construction. That bound ships with the result.
- **Interaction:** if `combined < control` while `llm-only > A0`, the earlier
  finding sharpens from "authored lessons underperform" to **interference** —
  they work alone and degrade the deterministic set they are added to.

**Outcome (2026-08-30): the design could not be executed.** The model
authored an applicable lesson in only 1 of 6 post-fix B states, so cells did
not contain their treatments; `llmonly-s3` died at re-apply because nothing
was re-authored to restore. No completed llm-only run had a lesson at B, so
the primary test measures nothing. The surviving contrast — A1 (zero lessons)
to B2 (one authored lesson) — is null at n=200, p=0.2810. Authoring fell to
0.42 lessons/pass after the evidence fix (from 1.17), which is the blocker
and the next thing to test. Two bounds on method also came out of it: an
LLM-only configuration cannot reliably complete a governed
apply/rollback/re-apply cycle, and cross-run pairing at n=100 has a noise
floor that reached p=0.064 between two runs with identical applied lessons.
Full write-up: RESULTS.md, "Can an LLM-authored lesson self-improve ALONE?".

**Reported for every cell, whatever lands:** the authored-lesson DOSE per seed
per pass (the LLM does not author on every pass; a cell whose B carried zero
authored lessons is a dose-0 observation and evidence about nothing else), the
A0 cross-cell drift check (all four cells are ignorant at A0, so a significant
difference there is provider drift and invalidates that pairing rather than
supporting it), per-rule recurrence, token cost, the governance ledgers, and
the full transcripts.

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
  The B-over-A0 margin is a **visibility** threshold, not a noise one: the
  mock is deterministic, so a real effect has zero variance. It dropped from
  0.10 to 0.05 when the silent rules landed, because this arm applies
  analyzer lessons only and a signature clusterer never produces one for a
  rule that raises no error — R7-R9 are failures it structurally cannot fix,
  so its absolute headroom is smaller by construction.

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
  scripts/aba_stats.py              paired McNemar over the per-task rows —
                                    states WITHIN one run (A0/B/A1/B2 + arms)
  scripts/aba_arm_stats.py          the same state ACROSS two runs, paired by
                                    seed then task: the governed-vs-loop+LLM
                                    comparison, plus per-rule deltas at B
                                    (`--selftest` is the keyless CI check)
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
  published runs' review policy and prompt bytes exactly. **Live numbers are
  published**: three seeds, both configurations, in RESULTS.md "Does an LLM
  write better lessons? — the loop+LLM arm". The pre-registration below
  resolved to its third branch — the control is significantly ahead at B
  (p=0.0043) — so deterministic signature lessons stay the default lesson
  kind, with a dose-response and a per-rule mechanism behind that number.

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
it. Four keyless gates, all in CI:

| gate | what it protects | fails when |
|---|---|---|
| `tests/reproducibility.rs` → `tests/golden/reproducibility.txt` | the task sets (prompt **and** hidden ground truth) per seed, the tool schemas, the governed pipeline's learned lesson, the LESSONS prompt bytes across apply→rollback→re-apply, and each arm's rendered context | any of it drifts — i.e. the runs under `results/` stop being comparable to a fresh run |
| `--assert-shape` ledger checks | *what* was learned, not just that the rates moved: ≥1 lesson applied, every lesson B applied restored in B2, and the origin rule that DEFINES the arm — governed-only fails if an LLM lesson applies, `--llm-lessons` fails if none does | an analyzer or store change silently changes which lesson fires, a partial restore makes B2 a third memory state, or a run measures one arm under the other's label |
| `aba_arm_stats.py --selftest` | the cross-configuration comparison itself: seed pairing (an unidentifiable seed exits rather than pairing by argument order), task pairing, discordant-count orientation, and per-rule deltas | the tool that computes a published arm number breaks silently |
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
| `--no-analyzer-lessons` | suppress APPLYING analyzer lessons; the analyzers still run, so the LLM's evidence is unchanged. With `--llm-lessons` this is the **llm-only** cell of the 2x2. Refused on its own — nothing would apply and B would be a second A0 |
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

The **loop+LLM arm** is the same invocation `+ --llm-lessons`: B/B2 then
measure analyzer lessons *and* LLM-authored lessons together, and the report
labels the arm via `config.llm_lessons`. Compare against a governed-only run
at the same seed to isolate what authored lessons add. (GROUND on a different
model family than the proposer, as above, is the proposer≠grader rule — keep
it for this arm especially, since its lessons are the thing being admitted.)

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

**The loop+LLM comparison is two run sets, not one.** The arm changes what
the governed states themselves contain, so it cannot be a fifth column beside
the passive arms — it needs its own A/B/A/B. Run each seed twice, identical
but for `--llm-lessons`, then pair them:

```bash
# per seed N: /tmp/governed-sN (no flag) and /tmp/llm-sN (--llm-lessons)
python3 crates/areev-bench/scripts/aba_arm_stats.py \
  --control /tmp/governed-s1 /tmp/governed-s3 /tmp/governed-s5 \
  --arm     /tmp/llm-s1      /tmp/llm-s3      /tmp/llm-s5
```

Runs pair by SEED (from `report.json`, else a `-s<N>` suffix), never by
argument order, and each config's own causal chain still goes through
`aba_stats.py`. Read the **A0 row first**: both configs are ignorant there,
so a significant A0 difference is provider drift and invalidates the B
comparison rather than supporting it. Use odd seeds — the even/odd collision
above makes 1/2/3 two task streams, not three.

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
| three-seed headline run (governed only, 300/100) | 2,100 task-runs, 4 states · 9.3M prompt + 0.29M completion tokens | **~$0.70** |
| the loop+LLM arm beside it (same scale) | 2,100 task-runs · 9.4M + 0.29M | **~$0.70** |
| one seed, governed states only (150/60) | ~390 task-runs | **~$0.35** |
| keyless floor (`--mock`) | any | **$0** |

The two 2026-08-30 rows are the only **scaled** figures here: their token
counts are measured (and recomputable from the transcripts), but the dollar
amount is the $2.30 run's billed rate applied to those counts, not an invoice.
A seed of the pilot config (150/60) is the row above it and was billed
directly.

The prompt-token total dwarfs completion by 60×, which is the shape of this
benchmark: long tool schemas and a growing message history re-sent every turn.
Arms that inject more context (m-all at 6.2× the prompt tokens) cost
proportionally more, which is exactly the axis the arm comparison prices.
