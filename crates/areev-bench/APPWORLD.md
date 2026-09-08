# Governed self-improvement on an interactive coding agent — AppWorld

**Status: complete, 2026-09-08. Four arms, 168 held-out tasks each, $5.94.**

**The result is a null, on every pre-registered metric.** Neither raw
experience nor governed rules moved the score, and the paired token
comparison does not survive either. What survives is the governance ledger:
the gate refused five rules that would have been actively wrong.

Does a governed memory loop make an agent measurably better at work whose
success is checked against a database rather than a grader's opinion?

[`RECEIPTS.md`](RECEIPTS.md) answered yes on extraction — one grain type (a
lesson `Fact`), learned from one kind of evidence (a person's correction),
on one kind of task. The two tracks that tried to widen it each stopped for
a different reason, and both reasons are about the benchmark rather than the
loop:

- **τ²-bench retail** ([`tau2/README.md`](tau2/README.md)) — the withheld
  policy clauses changed what the agent *did* (42 tool errors against 23,
  two runs killed by error floods) and nothing the reward could see
  (p = 0.69). **The reward could not see the manipulation.**
- **PAST-Bench** ([`PERSIST.md`](PERSIST.md)) — all three arms
  indistinguishable, per-family noise floor 0.09–0.13 wider than every
  between-arm gap, and a gate audit in which **15 of 28 approvals were
  over-generalisations from a single instance**. **Underpowered, and the
  gate admits rules that do not pay.**

AppWorld is chosen because it answers both objections structurally, before
anyone runs anything.

## The result

Every arm ran all 168 `test_normal` tasks with zero worker failures.

| arm | solved | TGC | SGC | tokens/ep | $/ep |
|---|---:|---:|---:|---:|---:|
| **A** no memory | 49 | 29.2 | 14.3 | 147,087 | 0.0073 |
| **B** passive | 52 | 31.0 | 16.1 | 128,872 | 0.0065 |
| **B2** passive, repeat | 55 | 32.7 | 16.1 | 135,423 | 0.0067 |
| **C** governed | 49 | 29.2 | 10.7 | 157,205 | 0.0078 |

**The noise floor is +1.8 points.** Two identical passes of the same arm
(B2 against B) differ by 3 solved tasks and **31 discordant tasks** —
17 won by one pass, 14 by the other, p = 0.72. Nothing below clears it.

| comparison | delta | discordant | McNemar p |
|---|---:|---|---:|
| **B2 vs B** — the floor | +1.8 pts | 17 / 14 | 0.7201 |
| B vs A — does memory help? | +1.8 pts | 20 / 17 | 0.7428 |
| C vs B — does governing it help? | −1.8 pts | 13 / 16 | 0.7111 |
| C vs A | 0.0 pts | 20 / 20 | 1.0000 |

**C vs A is an exact tie**: 49 solved each, 20 tasks won by each side. The
governed arm changed which tasks it solved and not how many.

### The cost claim does not survive pairing either

Read off the aggregates, B looked ~12% cheaper per episode than A. Paired by
task it is not:

| comparison | median tokens | cheaper / dearer | Wilcoxon p |
|---|---:|---|---:|
| B2 vs B — the floor | +2.0% | 87 / 80 | 0.3649 |
| B vs A | −7.4% | **76 / 92** | 0.6846 |
| C vs B | +10.7% | 80 / 88 | 0.1449 |

B's median episode is 7.4% cheaper, yet **more tasks got dearer than
cheaper** (92 against 76). The aggregate saving came from a handful of
runaway episodes in arm A, not from the typical task. An aggregate would have
supported a cost claim that pairing refuses — the same distinction
[`PERSIST.md`](PERSIST.md) drew, landing the other way here.

### What this is, and what it is not

**It is a well-powered null.** 168 held-out tasks paired by id, a reward that
is unit tests over database state, and a noise floor measured rather than
assumed. It replicates PAST-Bench's finding — the governed loop adds nothing
over the plain store, which adds nothing over no store — on a benchmark
chosen specifically because it answered that track's two objections.

**It is not evidence that the rules were bad.** They were four sensible
rules about authentication and parameter validity, drawn from failures that
genuinely dominate this agent's transcript. They were read on every episode
and they did not convert into solved tasks. The most likely reading, and it
is a reading rather than a finding: for this model, knowing the protocol was
never the binding constraint. `phone.login` fails on its own 13 times in the
passive block — an agent that cannot authenticate is not helped by being told
to authenticate first.

**What survives independent of the score** is the gate's ledger: 100 rules
proposed, 4 approved, 44 refused with reasons, and **five of those refusals
were rules that would have been actively wrong** — four inventing AppWorld
endpoints that do not exist, one contradicting the Spotify API. In an
ungoverned skill library those five reach the prompt. That is measurable, it
did not depend on the outcome, and it is the only claim this run supports.

## The benchmark

**AppWorld** (Trivedi et al., ACL 2024 Best Resource Paper,
[arXiv:2407.18901](https://arxiv.org/abs/2407.18901), Apache 2.0): 9
day-to-day apps, 457 APIs, ~106 simulated people, and **750 tasks over 250
scenarios** — three task variations per scenario. The agent writes Python
that calls the APIs in a REPL and drives the world to a goal state.

Splits, counted from the installed dataset (`load_task_ids`), not from a
secondary source: **train 90 · dev 57 · test_normal 168 · test_challenge
417**. A task id is `<scenario>_<variation>`, so the scenario grouping SGC
scores is readable straight off the id. The challenge split is the tasks
needing an API from the two held-out apps, plus a heavier concentration of
the hardest difficulty level; the rest are split at random.

**The pin.** AppWorld is installed from its git tree at commit `42b5bcf`
(version `0.2.0.dev0`, data version `0.2.0`), not from the PyPI release
`0.1.3.post1`. The released package ships only the paper's original agents
behind a model-graph runner; the tree ships the framework-free "simplified"
scaffolds the project now recommends, which are what a memory-assembling
subclass can be built on without forking the benchmark. The cost of the
choice is stated rather than hidden: reproducing this needs a git commit
rather than a version number, so the commit is quoted everywhere the result
is.

It is not vendored. It installs beside this repo into its own environment,
and `appworld/` here is a bridge — what this directory owns is the agent
scaffold, the Areev memory the prompt is assembled from, and the arms.

**Three properties are why it is the right benchmark for this question:**

1. **The reward sees everything.** Scoring is unit tests over database
   state plus explicit no-collateral-damage checks. A rule that changes what
   the agent does changes the database. That is precisely what τ² lacked —
   there, authentication was read-only and the once-only modify bit a slice
   of tasks too thin to show.
2. **It has power.** 168 held-out tasks paired by `task_id`, against
   PAST-Bench's 26 families.
3. **Transfer is in the dataset, not in our hope.** Three tasks per
   scenario, and 457 APIs reused across the whole set, so a learned
   procedure has somewhere to transfer to by construction. The benchmark's
   own **SGC** metric scores the scenario rather than the task, which makes
   within-scenario transfer directly measurable instead of inferred.

## Stage 1 — the positive control passed

Before any model was asked to score anything, every ground-truth solution in
`train` and `dev` — **147 tasks** — was driven through the harness and
evaluated. All of them pass:

| split | tasks | TGC | SGC |
|---|---:|---:|---:|
| train | 90 | 100.0 | 100.0 |
| dev | 57 | 100.0 | 100.0 |

100.0 at difficulty 1, 2 and 3 alike. The scoring path can score, so a zero
from an agent later is a fact about the agent. This is the check whose
absence cost this program a published conclusion once, and it is now the
first thing that runs.

*A note on what `dev` can and cannot support:* its 57 tasks are 30 at
difficulty 1, 24 at difficulty 2, and **3 at difficulty 3 — all three
variations of a single scenario**. The ceiling probe's difficulty-3 bucket
is therefore one scenario wide and cannot speak for hard tasks in general.
`make_probe_set.py` prints this rather than quietly filling the quota from
easier tasks.

## What is already known here, including by other people

Quoted from the sources, not measured by us:

- The paper reports **GPT-4o solving ~49% of normal and ~30% of challenge
  tasks**, other models at least 16 points lower.
- **ASSAY** ([arXiv:2606.15390](https://arxiv.org/abs/2606.15390)) is the
  current state of the art on the challenge split — **DeepSeek-V3 at 69.3%
  TGC, a 47.4% relative improvement** — by doing the *ungoverned* version of
  this loop: generate skills from experience, then curate them by their
  measured per-task causal effect.

**We will not be state of the art here, and nothing from this track may
claim it.** Two things follow, and both are stated before the first episode
rather than after the result:

- The agent model is pinned to `qwen3-30b-a3b-instruct-2507` — the same
  model as every other track in this program, so results read against our
  own line of work. It is not the model the leaderboard is set with, and any
  comparison to a leaderboard number is a category error.
- ASSAY's central finding — that skills help some task types and hurt
  others, that the effects cancel in aggregate, and that global curation
  therefore cannot see them — is an **independent replication of what our
  own gate audit found** on PAST-Bench. It is prior art that agrees with us
  about the problem and answers it empirically. Our claim, if any survives,
  is about governance: approval with a stated reason, supersession,
  measured revert, and an audit trail a person can read.

## Stage 2 — the ceiling probe: reachable, and only just

The shipped ReAct code agent, unmodified, on the 11-task stratified `dev`
slice. Nothing of ours in the loop. 1300s wall clock, **$0.0906 metered**.

| | TGC | SGC |
|---|---:|---:|
| **aggregate (11 tasks)** | **18.2** | **22.2** |
| difficulty 1 (4 tasks) | 50.0 | 50.0 |
| difficulty 2 (4 tasks) | 0.0 | 0.0 |
| difficulty 3 (3 tasks, one scenario) | 0.0 | 0.0 |

**The gate passes: 18.2 is not zero, so the domain is reachable and a later
zero would be a fact about the agent rather than about the harness.** That is
all it establishes. At n=11 the interval around 18.2% is far too wide to
carry anything else, and the honest reading is narrower still: this agent
solves some difficulty-1 tasks and, in this sample, none above it. The
measurable band is thin.

**What it also bought, which is the other half of why the probe runs first:**
$0.0082 and ~118s per episode, measured rather than guessed. A four-arm
sweep of `test_normal` is 672 episodes — roughly **$5.50 of agent leg** and,
at one process, about 22 hours. The budget is comfortable; the wall clock is
what has to be engineered, and `solve_tasks(num_processes=...)` is the
handle.

Two of eleven episodes ended at `max_steps` (50) rather than finishing;
those are reported separately from wrong answers, as pre-declared.

## Stage 3 — the lever: authentication discipline

The failures are not diffuse. Across the probe: **46 HTTP 401s in 6 of the
11 tasks**, 13 HTTP 422s in 3, and five distinct cases of the agent inventing
a parameter name (`venmo.search_users(phone_number)` where the API takes
`query`).

The 401s are one failure with two halves, and neither is ignorance —
`login` is called in 10 of the 11 tasks:

| what happened | tasks |
|---|---:|
| called an app API **before** authenticating | 4 |
| authenticated, then still 401 — the `access_token` was not carried into later calls | 2 |
| authenticated cleanly, no 401 | 4 |
| never touched an app that needed auth | 1 |

So the agent knows the protocol exists and applies it unreliably. That is
rule-shaped — *authenticate before the first call to an app, and pass the
token to every subsequent call* — repeated, and grounded in evidence the
environment states outright (the 401 body names the cause). Most importantly
it is **reward-visible in a way τ²'s clauses were not**: an unauthenticated
agent cannot change the database, and the database is what the unit tests
read. This is the manipulation the reward can see, which is the whole reason
this benchmark was chosen.

**Three cautions, recorded now rather than after the arms run:**

1. **The lever may be too easy.** One rule plausibly addresses six of eleven
   tasks. If a single "log in first" lesson does all the work, that is the
   result and it will be reported as exactly that — a weak model taught one
   API protocol — not dressed up as general self-improvement.
2. **Authentication is necessary, not sufficient.** Three tasks
   authenticated cleanly and failed anyway. The ceiling on what this lever
   can move is well below 100%.
3. **This is a weak-model failure.** The ground-truth solutions authenticate
   as a matter of course, and the paper's GPT-4o reaches ~49% on
   `test_normal`. What a positive result here would show is that the loop can
   teach *this* model *this* protocol from its own failures. That is a
   narrower claim than "governed memory makes agents better", and the
   narrower claim is the one that may be made.

## Stage 4 — the bridge, and what it cost to trust it

`appworld/` subclasses the shipped ReAct agent and changes two things: a
block is appended to the rendered prompt, and the episode's failures are
recorded at its close (with a loop pass, in the governed arm). Everything
else stays the benchmark's and byte-identical across arms.

**The manipulation is asserted, not assumed.** `selftest.py` builds all
three arms against a real task world and checks that each arm's prompt is
arm A's prompt *plus its own block, byte for byte*. It runs without a key,
so CI can hold it.

**Held-out arms are frozen by the store, not by intention.** They open with
`read_only=True` (every write refused, `STO-E004`) and record nothing. B and
B2 must read the same memory or the noise floor measures the memory drifting
rather than the model.

**Parallelism is safe and, checked against the serial run, exact.** The same
11 probe tasks under three worker processes scored **TGC 18.2 / SGC 22.2 —
identical to one process**, per difficulty level as well, in 319s against
1300s. Each worker takes its own copy of a frozen memory, because one file
admits one handle even for readers (`STO-E002`).

**Six defects were found before any arm ran**, five of them by the
self-test or by reading a smoke's own output. The three worth naming here,
because each would have produced a number rather than an error:

1. The store projects a Tool grain's body as `tool_content`; the writer used
   `content`. The passive block was assembling as a list of bare endpoint
   names with **every error message empty** — an arm that would have run,
   scored, and measured nothing.
2. API errors were attributed to the **last call in a code chunk** rather
   than the one that failed. A chunk routinely calls several APIs, and the
   smoke's own block blamed `venmo.create_payment_request` for an error whose
   text named `search_users`. Attribution now takes the endpoint the message
   names, then the line echoed in the traceback, and only then the last call.
3. A worker died on `response.get("usage", {})` returning `None` — the same
   absent-versus-null confusion as `reasoning_content`, in a second place —
   and `sweep.sh` reported success because POSIX `wait` with no operands
   always returns 0. **The first parallel run scored 9 of 11 tasks and called
   it a result.** The sweep now waits per PID and refuses to evaluate when
   any worker died: no score is better than a wrong one. A response with no
   usage block normalises to empty *and* prints `AREEV-WARN usage-null`,
   because those tokens are genuinely uncounted and a cost meter that
   under-reports silently is worse than one that fails.

## A design mistake, recorded before the result lands

**The blocks are assembled with `RECALL` plus host-side Python, where
`ASSEMBLE` is the statement that exists for exactly this.** That is a
mistake, not a trade-off, and it is written here while the arms are still
running so it cannot later look like hindsight.

One statement reproduces the governed block, checked against the real
memory:

```sql
ASSEMBLE "operating rules" FOR "the coding agent" FROM
  rules: (RECALL facts WHERE relation = "lesson")
BUDGET 300 tokens
FORMAT markdown
WITH dedup(object)
```

It returns the same four rules, with their subjects and confidences, in one
query — against `memory.py`'s RECALL, filter, sort, join. What the hand-rolled
path forfeits:

- **the token budget.** `experience_block` caps the passive arm at *12 lines*,
  which is a line count, not a token count. `BUDGET n tokens` is what makes
  two arms comparable on cost by construction rather than by coincidence —
  and budgeted assembly was the **one measured win** in
  [`PERSIST.md`](PERSIST.md) (a fifth cheaper per episode, Wilcoxon
  p = 0.005). This track currently forfeits the thing that last won.
- **progressive disclosure** (Full→Summary→Omit) and `PRIORITY`/`PIN` from
  `areev-context`, none of which the Python reaches.
- **the shared renderer.** Every other Areev surface renders through
  `areev_cal::render`; this one hand-formats markdown, so the benchmark
  demonstrates less of the product than it should.

The one thing CAL does not do is the passive arm's **frequency count** — the
`(16x)` that makes a raw error list informative rather than a flat set;
`dedup` drops duplicates rather than tallying them. That is an argument for
counting host-side and *still* assembling through `ASSEMBLE`, not for
bypassing it.

**It is not being changed mid-run.** The block is read on every learn episode,
so switching now would invalidate the completed learn phase as well as the
arms in flight, and "never change a harness after seeing its result" is the
rule this program keeps. The comparison as built is internally valid — the
arms differ only in their block, whatever assembled it. What is damaged is
the demonstration, not the measurement.

### The rest of the surface this harness does not touch

The hand-rolled assembly is one instance of a wider gap. Written out so run 2
has a design rather than a regret:

**Saved queries.** The two reads are Python string literals in `memory.py`.
CAL registers them in the file itself:

```sql
DEFINE QUERY "agent_block"($ns) AS {
  ASSEMBLE "operating rules" FROM rules: (RECALL facts WHERE relation = "lesson")
  BUDGET 300 tokens FORMAT markdown WITH dedup(object)
}
-- then, from any host: RUN "agent_block"($ns = "appworld")
```

Saved queries live as `qry:<name>` meta rows, **travel with the memory file**
and **replicate through bundles**. As built, a memory handed to someone else
does not carry how to read it; the knowledge is stranded in this harness.

**Output templates and formats.** `DEFINE TEMPLATE` registers a reusable
renderer as a `tpl:<name>` row; `FORMAT` offers `sml`, `markdown`, `text`,
`toon` and `json`. This harness asks for `FORMAT json` and rebuilds markdown
by hand in Python. `toon` — compact tabular blocks — is the obvious fit for a
block of repeated `(endpoint, error, count)` rows and is plausibly the
cheapest of the five in tokens, which is precisely the axis the hand-rolled
path already forfeits.

**The Tool grain's call/result lifecycle.** The binding has
`record_tool_call(name, result, is_error, call_id, input, status,
failure_cause, correlation_id, …)`. This harness calls plain
`add("tool", {tool_name, is_error, content})` and throws the rest away — the
code that produced the failure, the call/result correlation, the status and
the failure cause. A Tool grain is a call *and* its result; ours is a flat
error string.

**Namespace prefix scoping.** Two namespaces are used: `appworld` for the
agent's own memory and `agent:harness` for eval-run journals, and that split
is load-bearing — it is what keeps the score out of the agent's reach. But
the `"org.*"` prefix scope is unused, where the shape fits exactly:
per-app child namespaces (`appworld.phone`, `appworld.spotify`, …) would let
a task recall the app it is about, or `"appworld.*"` for everything. That is
not cosmetic here: the gate approved a rule scoped to `phone.*` which then
sits in an undifferentiated pile, and per-app namespaces are the mechanism
that would have made its scope mean something at recall time.

**Not used at all, and correctly so:** tool-schema rendering and the
memory-tool adapter. AppWorld's agent writes Python into a REPL; there are no
tool definitions and no tool-call protocol in this benchmark, so a Tool grain
here is evidence of a failure, never a callable the model is offered.

**Pre-registered for run 2:** both blocks assembled by a **saved query** whose
body is an `ASSEMBLE` under an explicit token budget, rendered through a
registered template, with per-app namespaces under `appworld.*` and failures
recorded through `record_tool_call` with their input and status. Report
tokens per episode against this run. That turns the mistake into the question
PAST-Bench answered once on a different benchmark — does budgeted assembly
cost less for the same work — asked here where the reward can see the
difference.

## Design, fixed in advance

**Arms.** Four, all on the same held-out tasks, paired by `task_id`:

| | |
|---|---|
| **A** | the shipped scaffold, no memory — the anchor |
| **B** | Areev memory, passive: every episode recorded, assembled into the prompt, no loop |
| **B2** | a second identical pass of B — **the noise floor** |
| **C** | governed: B plus a loop pass at episode close (propose → GROUND → verify → rubric review with a BECAUSE → apply under supersession → measured revert) |

**Split usage.** Learn on `train` (90 tasks, full ground truth). Held out is
`test_normal` (168). `dev` (57) is the pilot and tuning split and is **not
touched again once the arms start** — no harness change after seeing a
result, which is the rule this program learned the hard way.

**Primary metric.** TGC on `test_normal`, B vs A and C vs B, paired by task,
McNemar's exact test, reported beside the **B vs B2 noise floor**. An effect
that does not clear that floor is not an effect.

**Secondary, pre-declared.** SGC (the scenario-level metric — where
within-scenario transfer would show if it exists); prompt tokens per episode
(the one real win PAST-Bench found was cost); API-error counts; termination
reasons; the spend.

**Legs, all pinned, temperature 0.** Agent `qwen3-30b-a3b-instruct-2507` via
OpenRouter. GROUND and the rubric reviewer call **OpenAI directly**
(`scripts/openai_chat.py`), not through OpenRouter. Every leg and seed lives
in `appworld/env.sh`; every call is metered.

## Stopping rules, written before they can be inconvenient

1. **A positive control comes first.** Before any model is asked to score,
   a train-split ground-truth solution is driven through the harness and
   must score TGC 1.0. "Failed" and "never ran" look identical in a reward
   column, and this repo has published a wrong conclusion for want of this
   check exactly once.
2. **A ceiling probe comes second.** The shipped scaffold, unmodified, on
   ~12 `dev` tasks with the pinned agent model. If it is at or near zero,
   the domain is out of reach for this model, that is the result, and **no
   learning run follows** — a loop cannot teach an agent a rule it could not
   have used. τ² is the precedent: two passes instead of six, asked first.
3. **A lever check comes third.** The probe's failures are classified into
   repeated, memorable mistakes an approved rule could fix, versus one-off
   reasoning failures. The classification is written here *before* arm money
   is spent. τ² died at this step, and it died after paying.
4. **The pilot gates the sweep.** A scenario-stratified ~40-task slice runs
   first. If B − A does not clear B − B2 there, the full sweep would be
   measuring noise and does not run.
5. **A null publishes as a null**, with the ledger, the gate's reasons, and
   the spend.
6. **No figure appears in this file that nobody measured** — cost, accuracy
   or otherwise. Estimates used for planning stay out of the record.

**Budget: $25**, metered, hard stop, and the cap is raised only by asking.
The wallet is shared with the PAST-Bench run in flight on the office box;
this track runs entirely on the laptop and never opens a socket on that
machine.

## Layout

| file | role |
|---|---|
| `appworld/agent.py` | the scaffold: assembled memory in, code blocks out, with `tool_calls_returned` counted beside `malformed_tool_calls` |
| `appworld/memory.py` | the Areev bridge: record → loop → review → apply/rollback → lesson assembly, plus the reviewer's fixed rubric |
| `appworld/episode.py` | one task: build the world, run it, score it, return the record the loop reflects over |
| `appworld/run.py` | the learn pass over `train` |
| `appworld/evaluate.py` | the paired held-out arms |
| `appworld/selftest.py` | the keyless floor CI runs, so the harness cannot rot between runs |
| `appworld/env.sh` | every model leg, pinned and seeded |

Evidence — per-task records, governance ledgers, meters, statistical
workings — goes to
[`AreevAI/areev-benchmark`](https://github.com/AreevAI/areev-benchmark),
never here. Only counts travel.

## The harness moved onto Areev's own surfaces (2026-09-08)

Recorded under rule 4 of `CLAUDE.md` — never change a harness after seeing its
result, and when you must, say what changed and which runs precede it.
**Run 1, the pre-registered null reported above, precedes all of it.** Nothing
below revises a number; the counts, the ledger and the gate's reasons stand
exactly as published.

What changed, and why:

- **Nine per-app namespaces.** Errors are now written to `appworld.<app>` and
  every read scopes `"appworld.*"`. This is the defect this document already
  records: the gate approved a rule that scoped itself to `phone.*`, and it
  then sat in an undifferentiated pile where the scope it named meant nothing
  at recall time. A prefix scope selects the base namespace **and** its
  descendants, so run 1's flat memories still read correctly through the new
  query — verified against them directly, block-for-block.
- **The run-1 memories were migrated and swapped**, and the flat originals are
  kept. `appworld/migrate_ns.py --swap` writes a NEW memory (a namespace is
  part of the content address, so re-namespacing mints new addresses; there is
  no in-place move), then gives it the source's name and moves the
  pre-migration memory to `<name>.flat.db`. Nothing is deleted and the swap is
  reversible by hand; every sibling moves together — the `-wal`, the `.blobs/`
  and the `.telemetry.db` — since a memory whose WAL was left behind has a torn
  tail. The migrated memory carries a `migrated_from` record naming the source
  and the per-namespace counts, so a copy is never mistaken for an original.

  Both forms were checked to render **identical prompt blocks**, governed and
  passive, which is what a prefix scope buys: `"appworld.*"` selects the base
  namespace and its descendants alike. The recommendation grains do NOT
  migrate — they are engine-authored and query-only — so a migrated memory
  starts with an empty review queue, and run 1's ledger of 100 proposals and
  44 refusals lives in the `.flat.db` beside it.
- **API errors are recorded as calls** (`record_tool_call`), keeping the
  arguments, the status and the failure cause the flattened `add("tool", …)`
  discarded.
- **The governed arm's prompt block is a saved `ASSEMBLE`** registered in the
  memory file. Byte-identical to the renderer it replaced, gated by
  `scripts/parity_check.py appworld`.
- **The passive arm's block is not.** It ranks by frequency, and CAL's
  `GROUP BY` reorders rows without projecting a per-group count a template
  could render. CAL selects; the harness tallies. `appworld/AREEV.md` says so,
  and any token or truncation figure quoted here means the harness's cap.

  That selection is a saved **`RECALL`**, not an `ASSEMBLE`, and the
  distinction cost a bug to learn. `ASSEMBLE` applies a token budget whether or
  not one is asked for — the default is 4000 — and a budget that binds drops
  grains **silently**: no warning, and `total_available` reports the
  post-budget count, so a caller cannot tell a full answer from a truncated
  one. Wrapping this pure selection in an `ASSEMBLE` returned **79 of 229**
  error grains on run 1's own memory. Every prompt section in the crate now
  states its budget explicitly, and `scripts/parity_check.py` seeds 200 grains
  past the default so the omission cannot come back.

A run 2 under this harness would not be comparable to run 1 on prompt bytes for
the passive arm alone; the governed arm's bytes are unchanged.
