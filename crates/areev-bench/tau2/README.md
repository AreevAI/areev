# tau2 — governed self-improvement on τ²-bench retail

An agent deployed on an **incomplete understanding of its own domain**,
which then meets work the missing rules govern, learns them from what goes
wrong, and has a person approve them before they take effect. The scenario
in one line: *the assumptions it was built on turn out to be wrong, and the
loop is how it finds out.*

The environment, the customer, the orchestrator and the reward are
[τ²-bench](https://github.com/sierra-research/tau2-bench)'s (MIT), unchanged
and not vendored — this directory is a bridge. What it owns is the agent's
system prompt, the redaction, and the Areev memory the prompt is assembled
from.

| file | role |
|---|---|
| `redact.py` | which clauses are withheld, removed from **both** the policy and the tool descriptions, with an audit that fails if one is still readable |
| `agent.py` | a `HalfDuplexAgent` whose knowledge is (redacted policy + applied lessons), calling a model through the bench's JSON-on-stdio adapter |
| `episode.py` | one task: build the env, run τ²'s orchestrator, score DB-only, return the record the loop reflects over |
| `memory.py` | the Areev bridge: record → loop → review → apply/rollback → lesson assembly, plus the supervisor's fixed rubric |
| `run.py` | A0 over the held-out tasks, then the experience phase with a learn pass every N episodes |
| `evaluate.py` | the paired held-out evaluation (arms B, B2, A) |
| `env.sh` | every model leg, pinned and seeded |

## Setup

τ²-bench is not a dependency of this repo. Install it beside it, into its
own Python 3.13 environment, together with the Areev binding built from
this tree:

```bash
git clone --depth 1 https://github.com/sierra-research/tau2-bench ~/mg/local/tau2-bench
python3.13 -m venv ~/mg/local/tau2-venv
~/mg/local/tau2-venv/bin/pip install -e ~/mg/local/tau2-bench audioop-lts maturin
(cd <this repo> && VIRTUAL_ENV=~/mg/local/tau2-venv \
   ~/mg/local/tau2-venv/bin/maturin develop --release -m crates/areev-py/Cargo.toml)
```

`audioop-lts` is needed because τ²'s voice module imports the `audioop`
module Python 3.13 removed, on a path every import crosses.

## Run

```bash
sh -c '. ./env.sh; $PY run.py --workdir runs/s1 --audit'          # what is withheld, and what leaks
export OPENROUTER_API_KEY=…
sh -c '. ./env.sh; $PY run.py --workdir runs/s1 --experience 20 --eval 25 \
        --learn-every 4 --journal-baseline --measure'
sh -c '. ./env.sh; $PY evaluate.py --workdir runs/s1/eval --learned-db runs/s1/retail.db \
        --experience 20 --eval 25 --journal B=eval-b'
```

## Pre-registered design (written before any paid run)

*Committed 2026-09-04, before a single scored episode. Not yet run.*

- **Withheld**: `authenticate` and `modify_once` — removed from the policy
  **and** from every tool description, with `--audit` failing the run if
  either is still readable. Chosen because one is absent from the shipped
  tool schemas entirely and the other is the classic "nobody told me I get
  one shot" failure; both are named in `redact.py` with their reasons.
- **Split**: the benchmark's own `base` order. The first 20 tasks are the
  experience phase, the next 25 are held out — disjoint by construction, no
  reshuffling, so the tasks are the benchmark's and the slice is stated.
  (Reduced from 30/40 before the first run, for wall-clock: each episode is
  a multi-turn conversation between two models, so the held-out set is read
  three times over and the arms dominate the cost. The reduction is recorded
  here rather than made quietly, and it costs power: at n=25 only a large
  effect can clear the noise floor, which is stated with the result.)
- **Legs**, all pinned, temperature 0: agent `qwen3-30b-a3b-instruct-2507`
  (coreweave/bf16); customer τ²'s own simulator on the same model and pin;
  learner `gpt-oss-120b` (deepinfra/bf16); GROUND `gpt-4o-mini` (openai);
  supervisor `gpt-4o` (openai) on the fixed rubric in `memory.py`.
- **Primary**: solved-task wins vs losses, **B vs A**, paired by task
  (McNemar's exact test), reported beside the **B vs B2 noise floor**. With
  a model playing the customer that floor is expected to be non-trivial,
  and an effect that does not clear it is not an effect.
- **Secondary**, pre-declared: per-clause behaviour (did the agent start
  authenticating; did it stop calling modify twice), tool-error counts,
  termination reasons, and the spend.
- **A ceiling control** (`evaluate.py --control`): the same held-out tasks
  under the **full** policy and full tool descriptions, no lessons. Added
  before the first arm was scored, because without it a zero at A is
  uninterpretable — an agent that cannot do the task with the whole policy
  in front of it was never going to be taught the missing clause, and the
  run would be measuring the model's ceiling rather than the loop. If FULL
  is at or near zero, that is the result and no learning claim follows from
  this domain at this model.
- **Stated in advance**: `max_steps` termination scores zero by τ²'s own
  rule regardless of DB state, so terminations are reported alongside
  rewards — "never finished" and "finished wrong" are different failures
  and will not be merged. A null result publishes as one, with the ledger
  and the supervisor's reasons.

## Status

**The full run was started and stopped, deliberately.** Its A0 arm — the
agent as deployed, on the held-out tasks — was solving 0 of its first 7
episodes, and at roughly 2–3 minutes per multi-turn episode the remaining
six arms were several hours away. A zero there is uninterpretable without
knowing what the same agent scores with the *whole* policy in front of it,
so the run was stopped and `ceiling.py` put first: two passes instead of
six, answering whether this domain is measurable with this model at all
before paying to find out. Recorded here because "we ran it and it was
zero" and "we checked first" are different claims, and only one of them is
true.

## Retracted: "the domain is out of reach"

**An earlier revision of this file claimed τ²-bench retail was out of reach
for this agent model, on the strength of a FULL arm scoring 0 of 25. That
claim was wrong and is withdrawn. The zero was a bug in this bridge.**

The bench's chat adapter returns a tool call flattened onto the call object
(`{id, name, arguments}`); OpenAI nests it under `function`. `agent.py` read
only the nested shape, so **every tool call the model made parsed as
nameless and was dropped**. The agent could not take a single action in any
episode. What it did instead was answer with the empty-reply fallback —
658 times across 32 episodes, roughly 20 turns per conversation, which is
the number that gave the bug away.

With the shape accepted, the same agent on the same tasks solves them:
a two-task smoke went from 0 to **1 of 2**.

Two things this cost, both recorded rather than tidied away:

- **A published conclusion was wrong for about forty minutes.** It was
  caught by counting a log line that looked like noise, not by any gate.
  The lesson is the one this repo keeps relearning — a null result needs a
  positive control before it is believed, and "the agent scored zero" and
  "the agent never acted" are different claims that look identical in a
  reward column.
- **The native control was over-read.** τ²-bench's own shipped agent does
  score 0.0000 on retail tasks 20, 21 and 22 with this model
  (`results/tau2-ceiling-2026-09-04/`), and that measurement stands. But
  three tasks is three tasks: it was never enough to carry "out of reach",
  and it was leaned on because it agreed with a broken arm.

`agent.py` now counts `tool_calls_returned` beside `malformed_tool_calls`,
so a run where the two diverge says "parsing bug" rather than "a model that
would not act".

**Nothing else in the repo was affected, and the reason stings.** The
adapter's own docstring documents the flat shape, and the Rust sibling that
consumes it — the synthetic bench's agent — has handled both for as long as
it has existed, in one line: `let f = tc.get("function").unwrap_or(tc);`.
The pattern was already written, in the same crate, against the same
adapter. This bridge just did not copy it. The published A/B/A/B results,
which run through that Rust path, are untouched; so is the receipts
harness, whose agent uses no tools at all.

## Result — the domain is reachable, but these clauses are the wrong lever

The ceiling probe ran in full, 25 held-out tasks under each condition, no
lessons in either. Evidence:
[`../results/tau2-ceiling-2026-09-04/`](https://github.com/AreevAI/areev-benchmark/tree/main/results/tau2-ceiling-2026-09-04/).

| | solved | tool errors | runs killed by too many errors |
|---|:---:|:---:|:---:|
| FULL — the whole policy and tool descriptions | 7/25 | 23 | 0 |
| REDACTED — `authenticate` and `modify_once` withheld | 9/25 | **42** | **2** |

Paired by task: 2 tasks the full policy solved and the redacted one did
not, 4 the other way, **p = 0.69**.

**Two findings, and they point in different directions.**

*The domain is reachable.* 7 of 25 at 28% settles what the retraction
above already conceded: a zero from this bridge was never the model's
ceiling, and there is room here for a governed loop to work in.

*These clauses are not the lever.* Withholding them costs **nothing
measurable in reward**, which is what a learning claim would have to move.
So a full run with this clause set would have spent six hours measuring
noise, and the pre-registered verdict is the one the probe printed: choose
different clauses.

**They are not inert, though, and that is the useful part.** The redacted
agent makes **nearly twice the tool errors** (42 against 23) and two of its
runs die of too many errors where none of the full-policy runs do. The
withheld rules change what the agent *does* and not what it *scores*,
because τ²'s reward is a database-state check and neither clause changes
the database — authentication is read-only, and the once-only modify bites
in a slice of tasks too thin to show at n=25.

**What a working τ² design needs**, stated for whoever runs it next: clauses
whose violation lands in the database. The cancellation-reason enum and the
refund-destination rule are both in `redact.py` already and both qualify;
neither was chosen here because `--audit` showed them restated in the tool
descriptions, and removing them from both surfaces is a bigger redaction
than the one this run made. That is the next design, not a rerun of this
one.

**No τ² learning number is published**, and none should be from this clause
set. The bridge, the redaction, the audit and the ceiling probe are all
committed and working; what is missing is a manipulation the reward can
see.

## Two things to know before reading a number

**The tool descriptions carry most of the policy.** Of the four clauses
`redact.py` names, two are restated in the schemas the agent is handed. So
"withhold a policy clause" is not by itself a manipulation, and every
withheld clause is removed from both surfaces with `--audit` as the check.
That is a fact about τ²-bench worth stating on its own.

**The customer is a language model too.** Two identical passes over the same
held-out tasks disagree, so the B-vs-B2 arm is not a formality here — it is
the floor any claimed effect has to clear. Scoring is DB-only
(`EvaluationType.ENV`): no LLM judge, so nothing in the score depends on a
grader's opinion, and an episode that ends at `max_steps` scores zero by
τ²'s own rule regardless of DB state.

## The harness moved onto Areev's own surfaces (2026-09-08)

Recorded under rule 4 of `../CLAUDE.md`. **No τ² learning number is
published** (see "Result" above), so nothing published moves with this; the
ceiling-probe numbers precede it.

- The LESSONS block is two saved `ASSEMBLE` queries registered in the memory
  file. `tau2/AREEV.md` has the shape.
- **One deliberate difference.** The retired renderer sorted rules and
  conventions into ONE alphabetical list, interleaving "always confirm before
  cancelling" with "refund window: 30 days". CAL orders *within* a section, not
  across sections, so the two shapes are now two runs of lines. The same grains
  reach the model in the same per-section order; only the interleaving changed.
  `scripts/parity_check.py tau2` asserts the line set is identical.
- **A correlation defect was fixed.** `episode.py` paired a `ToolMessage` with
  the **last** call in the turn, so a turn issuing several tool calls
  attributed every result to one of them — and that is the evidence the
  proposer reads. It now joins on the `tool_call_id` the environment echoes
  back, and the calls are written through `record_tool_call`, which keeps the
  arguments, the status and the failure cause.

A re-run of the ceiling probe under this harness should not be compared
byte-for-byte with the numbers above.
