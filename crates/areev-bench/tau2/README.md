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

## Result — the domain is out of reach for this agent model

**τ²-bench's own shipped agent, on the same tasks with the same model,
scores 0.0000.** That is the control that matters, and it was run before
any conclusion was drawn about this bridge:

```
tau2 run --domain retail --task-ids 20 21 22 --max-steps 50   --agent-llm openrouter/qwen/qwen3-30b-a3b-instruct-2507   --user-llm  openrouter/qwen/qwen3-30b-a3b-instruct-2507
```

| task | reward | DB check | NL assertions | ended |
|---|:---:|:---:|:---:|---|
| 20 | 0.0 | 0.0 | 1.0 | user_stop |
| 22 | 0.0 | 0.0 | 1.0 | user_stop |
| 21 | 0.0 | — | — | max_steps |

The agent converses acceptably — the natural-language assertions pass, and
its partial-action scores are 43% and 80% — but it never reaches the exact
gold database state τ²'s reward requires. Our own FULL arm (whole policy,
whole tool descriptions, no lessons) matched it at 0 over its first
episodes, which is what stopped the run.

**So no learning claim can be made in this domain at this model, and none
is.** With the ceiling at zero there is no headroom for a governed loop to
close, and any B-vs-A number here would be measuring the agent's ceiling
rather than the loop. The bridge, the redaction and the audit all work —
the smoke ran the whole chain end to end — and they are committed for a
run against a model that can actually clear the bar. What is *not* here is
a number dressed up as a finding.

Evidence, including the native control's raw results:
[`../results/tau2-ceiling-2026-09-04/`](../results/tau2-ceiling-2026-09-04/).

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
