# Governed self-improvement across sessions — PAST-Bench and Horizon

**Status: PAST-Bench complete; run 4 (2026-09-08) re-measured it after the
Verify gate was found never to have fired.** Three seeds × three arms ×
26 families, plus mem0 on one seed, plus the tuning leg and a gate audit.
Evidence: [`results/persist-2026-09-07/`](https://github.com/AreevAI/areev-benchmark/tree/main/results/persist-2026-09-07/).
The Horizon track ran on its three public tasks only (§4). Budget: **$50**
cap ($30 approved 2026-09-06, raised the same day after run 1); about $25
of it spent.

**The three findings, in the order they should be read:**

1. **All three arms are statistically indistinguishable on the benchmark's
   own metric.** Areev's passive arm has the highest point estimate
   (+0.291 against Hermes's +0.246) and wins 16 of 26 families, at
   p = 0.21. Not a win. The per-family noise floor (0.09–0.13 across
   seeds) is larger than every between-arm gap.
2. **Areev answers for less.** Fewer prompt tokens per episode in 18 of 26
   families, Wilcoxon p = 0.005 — a fifth cheaper on the typical family, a
   third across the workload, because Hermes has a tail (121K tokens per
   episode on one family) and the budgeted assembly does not.
3. **The governed loop, with a working gate, still adds nothing over the
   plain store** (−0.025, p = 0.78). The gate itself is sound — 82%
   agreement with an independent second judge — but 15 of its 28 approvals
   are over-generalisations from a single instance, which is a plausible
   and testable reason the rules it admits do not pay.

The tuning leg is a **null with a scope**: 32 rows drawn from 26 unlike
families did not move the memory into the weights, where 34 rows from one
document type did in [`FOURWAY.md`](FOURWAY.md).

## 1. The question, stated precisely

[`RECEIPTS.md`](RECEIPTS.md), [`ADBUY.md`](ADBUY.md), [`DRIFT.md`](DRIFT.md)
and [`CURVE.md`](CURVE.md) all ask one shape of question: an agent fills a
row from a document, a person corrects it, and the loop turns corrections
into governed rules — one grain type (a `lesson` Fact) learned from one kind
of evidence (a correction) on one kind of task (extraction).

The paper's claim is wider. Areev agents are supposed to improve
**facts, procedures, tools, events and skills** — every grain type the agent
runs on — under the same governance, and then hand what they learned to a
small model. This track measures the wide claim on two public benchmarks
built for cross-session learning, and adds the tuning leg on top:

> Does an agent whose memory is governed — proposed, reviewed, applied,
> measured, reverted — improve more across sessions than the same agent with
> a plain memory, or none; does the improvement follow the intended pathway
> (save → retrieve → update) rather than a shortcut; and can a small model
> tuned on that governed memory carry the improvement with a fraction of
> the context, on hardware that costs a fraction as much?

## 2. The benchmarks

### PAST-Bench (arXiv 2608.04003, Aug 2026, Apache-2.0) — the primary track

[github.com/Gen-Verse/PAST-Bench](https://github.com/Gen-Verse/PAST-Bench),
pinned at commit `f822351` (2026-08-05). Built to *attribute* cross-session
improvement to retained experience: 26 task families, 204 episodes, four
capabilities, every family run twice under matched conditions —
persistence **on** (`with_persistence`) and **denied**
(`without_persistence`) — with the context wiped between episodes so any
gain must have travelled through the persistent substrate. Episodes have
roles: *cold*, *learn/update* (deposit the target state), *evaluation* (a
fresh session, new surface, trigger wording removed) and *control*
(shortcut, surface-memorisation, stale-reuse and wrong-substrate checks that
cap what a gain may be credited as). Tasks act on mock services over HTTP
tools; grading is deterministic checks on the service state plus an LLM
judge for communication.

| capability | families / episodes | what it tests | the Areev grain it lands on |
|---|---|---|---|
| Memory | 5 / 41 | a declarative clause is retained and applied later, unprompted | **Fact** (`note` written by the agent; `lesson`/`fact` proposed by the loop) |
| Procedural reuse | 8 / 64 | a multi-step workflow is re-executed in order, with the right tools | **Skill** grain (name, description, instructions); `skill_stall` |
| Information gathering | 6 / 48 | the agent consults what it stored *before* answering, not a planted default | recall at session start; `session_search` over prior **Events**; `coverage_gap` |
| Update | 7 / 51 | a second authoritative write overrides the first without the old state leaking — corrections, rule migration, temporary-exception expiry, SOP patching | **supersession** (replace = supersede, never edit); `contradiction_sweep`; `outcome_review` |

Two published metrics, reported here unchanged from the benchmark's own
`sequence_comparison.json`: the **self-evolution gap** Δ = score(on) −
score(off), per family, macro-averaged per capability, credited only when it
clears the family's control episodes; and the **mechanism-evidence score** —
did the gain use the intended pathway, judged from the persisted artifacts
(entries, skill docs, keyword hits, artifact diffs) and the persistence tool
calls (writes, reads, skill patches, session searches). Published reference
numbers: Hermes Δ +0.13 to +0.24 across seven frontier models; Hermes+
(five interventions) on MiniMax-M2.7: Δ +0.15, mechanism 0.73.

### Horizon (Orin Labs, MIT) — the second track

[github.com/orinlabs/horizon](https://github.com/orinlabs/horizon), packaged
as Harbor tasks. An agent receives a long first-person trace of an earlier
deployment (`/workdir/trace.jsonl`: reasoning, tool calls, outputs over
weeks) at environment build time, may ingest it however it likes, and is
then given a live task whose answer depends on something buried in that
history — the reward is deterministic on the final environment state. The
private set is **195 tasks** with traces averaging ~30M tokens; the public
repo carries **three example tasks** with small traces (~48 KB), and the
leaderboard is populated by Orin Labs running submitted agents (a PR with
`agents/<name>/` plus an email) on the private set. Reference agents:
`trace_rag` (chunk by day, embed, search), `trace_rlm`, `openclaw_lcm`,
`hermes` (seeds the trace into Hermes's session DB), and two integrity
baselines — `perfect_context` (the ceiling) and `tools_only` (the floor).

What it contributes that PAST-Bench cannot: the learning source is a
*trace* of tool calls and outcomes, not a conversation — i.e. Tool and
Event grains — and the private set is large enough for a leaderboard number.
What it cannot contribute here: three public tasks are a smoke, not a
statistic. The plan treats it accordingly (§4).

### EdgeBench — skipped

Named in the brief; skipped on 2026-09-06. Each task is 12+ hours of
within-task iteration at hundreds of dollars per task with a frontier model,
and it measures nothing that carries across sessions.

## 3. Track A — PAST-Bench

### 3.1 One scaffold, five memories

The design of [`FOURWAY.md`](FOURWAY.md): hold the agent constant and swap
only what carries forward. The scaffold is the benchmark's **own**
OpenAI-compatible chat loop (its `openai_compat_chat` adapter, unchanged in
how it calls the model and dispatches task tools); the arms differ only in
the persistence layer behind it.

| arm | what carries forward between episodes | who decides what is kept |
|---|---|---|
| **none** | nothing — the benchmark's `without_persistence` condition, run paired with every arm below | — |
| **areev-passive** | every turn, task tool call and saved entry is a grain in one Areev file; the live notes, profile and skills are rendered into the system prompt at session start; the model can add/replace/remove entries and manage skills through tools named as Hermes names them (`memory`, `skill_manage`, `skills_list`, `skill_view`, `session_search`) so the benchmark's mechanism counters see them; a *replace* is a supersession, never an edit. **No loop.** | the model's own writes |
| **areev-governed** | the passive arm **plus** a loop pass at every episode close: ANALYZE → DISCOVER → GROUND → VERIFY, then a fixed-rubric reviewer (`persist/reviewer.py`, written before any run) approves or refuses each proposal with a BECAUSE; approved changes apply under supersession; the previous episode's graded score is journaled as an evalset run under the family, so `outcome_review` can propose a revert of an applied change that hurt — which the harness applies, because a measured regression is the gate's own verdict | the loop, then the reviewer |
| **mem0** | `add()` after each session, `search()` at session start, results in the prompt — mem0's extractor decides (three modes as in FOURWAY if budget allows; installed-default first) | mem0 |
| **hermes** | PAST-Bench's shipped Hermes adapter (vendored **0.4.0**), unmodified, on the same model and judge | Hermes's files and skills |

Every arm is `--compare-no-persistence`, so `none` is measured beside each
one on the same run. The mem0 arm is written after the Areev arms have a
metered pilot; if the budget does not reach it, that is recorded rather than
a mode dropped silently.

**What the benchmark needed from us.** The adapter and backend register at
import time (`persist/pastbench/run.py`); the benchmark source has **one
edit**, applied by `patch_pastbench.py`: its `evolve` command builds a
persistence backend only for agent names it knows (`hermes*`, `nanobot`,
`zeroclaw`), and `areev*` is added to that line. Nothing else changes. The
Areev file is rendered into the Hermes-shaped tree the benchmark snapshots
(`memories/MEMORY.md` entries, `memories/USER.md`, `skills/<name>/SKILL.md`,
`session_current.json`), so **one scorer grades every arm**. Memory
*injection* is inferred by the benchmark from "a memory file existed before
the episode", which is why an empty memory renders no file.

Two places the engine was expected to fall short are named now so they are
findings, not surprises: a `plan_revision` can edit conditions, cycle bounds
and retries but not topology, so an SOP patch the loop proposes lands as a
`lesson` and the agent's own `skill_manage patch` is the topology path; and
a temporary exception's expiry relies on the proposer setting `valid_to`,
which no earlier run required. Engine defects found the way the receipts
programme found six are recorded in the ledger and fixed in the tree.

### 3.2 What is measured

- **Δ and the mechanism score**, exactly as the benchmark computes them
  (`sequence_comparison.json` → `past-bench v2-report`), per family, per
  capability, overall, for every arm. Nothing recomputed by hand.
- **Paired contrasts across arms** on the same episodes: governed vs
  passive (what the loop adds), governed vs mem0, governed vs none, and
  hermes beside them — paired exact test on episode pass/fail (McNemar) and
  a paired sign test on the [0,1] score. Within-run pairing is the claim;
  cross-run only reported.
- **The governance ledger** (`areev_ledger.json` per episode): proposals per
  analyzer and kind, approved / refused / applied / reverted, the reviewer's
  BECAUSE, the funnel. The Update capability's ledger is the one to read: it
  is where a refusal or a revert is the *correct* outcome.
- **Noise floor**: two identical `none` runs (A0R) per model before any Δ is
  interpreted; an effect inside the floor is not an effect.
- **Prompt tokens per episode** per arm, from the benchmark's own trace
  (`[end] turns=… tokens=…in/…out`), and the injected memory's size from the
  ledger.
- **Cost**: the agent's tokens from the benchmark trace; the loop and review
  legs metered by the bench adapters into `usage.jsonl`; judge tokens from
  the benchmark's judge trace where it records them (to be confirmed in the
  pilot — if it does not, judge cost is bounded from the key's ledger and
  labelled as such). Priced by `cost.py`'s pinned table, extended for
  MiniMax-M2.7 and the fp8 endpoint.

### 3.3 The tuning leg

After a governed run, the memory of each family's learn episodes is a corpus
(`slm_corpus.py` generalised: system = the day-one instruction, user = the
learn episode's task, assistant = the trajectory that succeeded, with the
approved artifacts as they stood). `areev tune --cmd` hands it to a CUDA
trainer and registers the adapter; `areev eval run` grades it against the
pinned evalset; the loop's `adapter_intake` proposes it and a reviewer
promotes it — the product path end to end.

| split | trained on | evaluated on | reads as |
|---|---|---|---|
| **seen families** | learn episodes of all 26 families | their evaluation episodes | memory distilled into weights for one deployment |
| **unseen families** | learn episodes of 20 families | the 6 held-out families' evaluation episodes | did it learn *how to use what it learned*, or memorise 20 families |

The tuned model is evaluated as the agent three ways: with **no persistence
read at all** (the memory is in the weights — the `without_persistence`
condition for a tuned model), with a 1K-token assembly, and with the full
assembly the 30B gets. The untuned small model with the full assembly is the
control that separates "a small model with the memory in its prompt" from
"a small model tuned on it". PAST-Bench's control episodes are scored for
the tuned model as for every arm.

**As run (2026-09-07, from run 1's governed memories):** the seen corpus
holds the learn and cold episodes of all 26 families that scored ≥ 0.6 —
38 rows, 32 kept after the 2K-token window (the rest were trajectories too
long to keep an assistant turn inside it); the unseen corpus holds 20
families' rows (26 kept) with six families held out by `random.Random(1)`
over the sorted names: `EP03_recall_then_modify`, `PC01_sop_bootstrap_01`,
`PC01_sop_bootstrap_02`, `PC01_sop_bootstrap_06`,
`PG03_oncall_handoff_lookup`, `PG06_kappa_integration_review`. Adapters:
seen 76 steps / 187 s / best val 0.037; unseen 64 steps / 169 s / best val
0.090; both nf4 QLoRA at 7.1 GB peak. The evaluation runs the
**`areev-passive`** scaffold (no loop) against the locally served model
under the same paired `--compare-no-persistence` protocol, so each family
yields the tuned model **with** the memory in its prompt and **with none**
in one run: tuned-seen on all 26 families, the untuned base on all 26 (the
control), tuned-unseen on the 6 held-out. The 1K-assembly condition is
deferred to after the sighted runs; the corpus-size caveat (§10, next-to-
last bullet) applies to everything here.

**SLM base: `Qwen/Qwen3-1.7B` in bf16** on the RTX 4060 — the same model
family the laptop's CURVE run tunes as an MLX 4-bit build, so the combined
table compares like with like and discloses the precision difference in one
row. Decided 2026-09-06 ("default can be the same, so we can compare").

### 3.4 The pilot, and what it must establish

One family per capability (`SM01_preference_adoption`, `PC01_sop_bootstrap_01`,
`PG02_ops_exception_desk`, `SM04_rule_migration`) × (`areev-governed`,
`areev-passive`, `hermes`), each with `--compare-no-persistence`, fully
metered. It fixes the budget (§8), confirms the adapters under the real
runner, sandbox and judge, and reveals what the judge trace records (it
records nothing — the judge's cost lives only in the key-usage bound).

**The pilot's table (2026-09-06, StreamLake, one run each; the benchmark's
own Δ on the evaluation episodes and its mechanism score; key-usage bound
per family-run, judge included):**

| family (capability) | governed Δ / mech | passive Δ / mech | hermes Δ / mech | $ per family-run (gov / pass / hermes) |
|---|---|---|---|---|
| SM01 preference adoption (memory) | **+0.304** / 0.30 | rerun after a benchmark crash, see §11 | +0.304 / 0.30 | 0.026 / — / 0.032 |
| PC01 SOP bootstrap (procedural) | **+0.263** / 0.18 | +0.145 / 0.24 | **−0.084** / 0.12 | 0.057 / 0.048 / 0.097 |
| PG02 ops exception desk (information gathering) | **+0.403** / 0.14 | +0.406 / 0.14 | +0.385 / 0.15 | 0.040 / 0.029 / 0.156 |
| SM04 rule migration (update) | **+0.640** / 0.60 | +0.344 / 0.30 | +0.320 / 0.07 | 0.031 / 0.027 / 0.027 |

Twelve family-runs, **$0.58** on the key, 7–8 min per Areev family-run and
7–18 min per Hermes one. The mem0 arm smoked afterwards on SM01 (two
harness fixes first — §11 #11): Δ **+0.164** / mech 0.22, evaluation 0.504
vs 0.340, 14 entries after the learn episodes, 2K chars injected, **$0.11**
per family-run — four times an Areev family-run, mem0's extraction calls
being the difference. What it kept is the FOURWAY finding again: episodic
narrations ("User requested a review of today's note…", "User saved the
TSV-formatted action items in a file…") with the rule inside one of them,
where the Areev arms kept the rule. What the ledgers say, family by family:

- Every Δ above came from what the **agent itself saved** — a note in the
  memory families, a skill in the procedural one (only after the
  session-end nudge, §11 #8), the pre-seeded skill and prior sessions in the
  information-gathering one. The loop **proposed** in every governed
  family-run (one to four drafts, all cited, most grounded) and the
  reviewer **refused every one**: as a restatement of the entry the agent
  had already saved (correct — dedup), or as an inference the evidence did
  not state (the rubric's "supported" clause). No loop proposal has been
  applied yet, so the governed and passive arms differ only by the loop's
  cost and by run-to-run variation; the SM04 gap (1.0 vs 0.704) is the
  quality of two different notes the same model wrote, not governance.
- The governance ledger is therefore clean but empty of applies. The full
  runs will say whether that holds across 26 families; if it does, the
  honest headline for PAST-Bench is that the substrate carries the gain and
  the gate admits nothing it should not — §10's second bullet, stated in
  advance.
- Hermes 0.4.0 on the same model and judge: level with Areev on the memory
  family, behind on procedural (negative Δ) and update, ahead on nothing;
  two to three times the cost per family-run on the two families where it
  runs long.

**Decisions for the full runs (the pre-registration, committed 2026-09-06
before any full run):** all 26 families; arms `areev-governed`,
`areev-passive`, `hermes`, `mem0` (installed default); three runs of each
Areev arm and of Hermes (seed = run number), one of mem0 unless the budget
allows more; four streams side by side with port offsets 0/100/200/300;
`SEED` per run; the reviewer rubric, the nudge, the pin and every
harness change frozen at commit `0454df1` of `bench/persist`; the
benchmark at `f822351` plus the two patches in `patch_pastbench.py`.
Projected from the pilot's meters: ~$6 per run-set with mem0, **~$17–19
for the programme** including the tuned-model evaluations' judge calls,
inside the $30 cap. Wall: the first hour of run 1 measured ~25 min per
family-run with four streams on the key — StreamLake rate-limits and the
retries' backoff, not the box, set the pace — so a run-set is ~12 h, not
the 5 h first estimated; the mem0 arm is queued behind the Areev arms
rather than added as a fifth stream, because a rate-limited key gains
nothing from more streams.

## Run 4 (2026-09-08) — the gate fires; the scores stay inside the noise

**Evidence:** [`results/persist-run4-2026-09-08/`](https://github.com/AreevAI/areev-benchmark/tree/main/results/persist-run4-2026-09-08/).
Three seeds, 26 families, the two Areev arms. Hermes's rows are runs 1–3's,
unchanged code and no rerun — stated wherever they are used. Spend $16.23
against a cap raised twice (§10). Predictions were fixed in §10 before any
run-4 episode; every one is reported below, met or not.

**The result that is not about scores.** Runs 1–3 recorded **zero** measured
outcomes across 78 governed arm-seeds and were read as "governance costs and
gains nothing". That was never a measurement of governance. Seven independent
defects sat between the harness holding a number and the gate being able to
use it (§11 #20, #24–#28), each sufficient on its own to produce exactly the
silence observed. Run 4 records **87 verdicts — 51 held, 28 regressed, 8
drifted — and applies 16 reverts.** The prior null is retracted as
uninterpretable, not overturned by a better number.

| arm | mean Δ | s1 | s2 | s3 | mechanism | prompt tok / episode |
|---|---:|---:|---:|---:|---:|---:|
| areev-governed | **+0.293** | +0.300 | +0.289 | +0.320 | 0.205 | 18,660 |
| areev-governed-blindrevert | +0.279 | +0.279 | +0.320 | +0.265 | 0.198 | — |
| areev-passive | +0.268 | +0.249 | +0.298 | +0.248 | 0.206 | 19,108 |
| hermes 0.4.0 *(runs 1–3)* | +0.246 | +0.252 | +0.265 | +0.221 | 0.166 | 28,269 |

Noise floor, same arm and family across seeds: **0.078–0.104**. Every gap in
that table is inside it.

### Against the predictions, in order

| stated in §10 | outcome | |
|---|---|---|
| procedural Δ rises from +0.144/+0.112 toward Hermes's +0.169 | governed **+0.112 → +0.158**; passive +0.144 → +0.153 | partly |
| `sop_bootstrap_04`/`_06` stop scoring below their own no-memory floor | `_04` fixed (Δ +0.137, 0.487 vs 0.350); **`_06` still below** (0.280 vs 0.350) | partly |
| governed ≥ passive once reverts fire | **−0.025 (p 0.78) → +0.040 (p 0.11)**, 13/10 | direction, not significance |
| passive − hermes at **p < 0.10** | **+0.016, p 0.84**, 12/12 | **not met** |
| prompt tokens ≤ +10% | passive +2.5%, governed −3.9% | met |

**The headline contrast moved the wrong way and the bar was missed.** Passive
fell +0.291 → +0.268 while governed rose +0.266 → +0.293; both moves are
inside the noise floor, but the paired passive−hermes contrast went from
+0.045 (p 0.21) to +0.016 (p 0.84). Run 4 does not support "level with the
leading agent framework" any more strongly than runs 1–3 did; it supports it
less. The cost claim is unchanged and is the one that still clears its bar:
19,108 tokens per episode against Hermes's 28,269, a third less.

### What reverts buy, isolated

`areev-governed-blindrevert` is the same code with one variable disabled —
§11 #28 made every revert unreachable — run on the same seeds. It is the
cleanest contrast this track has produced, and it is small:

| contrast | mean difference | W/L | Wilcoxon p |
|---|---:|---|---:|
| governed − blindrevert (all 26) | +0.014 | 11/13 | 0.94 |
| governed − blindrevert (procedural, 8) | **+0.063** | — | no test at n=8 |

Overall, applying reverts is unreadable against the noise. On procedural
families — where a harmful rule is a wrong step rather than a wrong fact —
the fixed arm scores +0.158 against the blind arm's +0.095, and
`sop_bootstrap_04` separates cleanly: blind **−0.025, below its own
no-memory floor**; fixed **+0.137**. That is the shape the design predicts,
at a sample size that cannot carry a claim.

### What run 4 cannot say

- **Nothing about executable plans.** Every `plan` proposal degraded to a
  skill: the proposer writes edge conditions like `tickets.length > 0`,
  outside the runtime's frozen v1 grammar, so the plan validator refuses
  them (§11 #27). Procedure *capture* is tested — 395 of 526 evaluation
  episodes rendered a skill — the plan grain is not.
- **Nothing about provider drift.** Hermes was not rerun; its rows are
  eleven days older than the arms they are paired against.
- `PC01_sop_bootstrap_06` is missing from run 4's passive arm (it scored
  −0.005 in runs 1–3, so its absence does not explain the passive decline).

### What to do next, in order

1. **Teach the proposer the condition grammar by example, not by
   description.** It is the single blocker on the plan grain, and the fix is
   a prompt change plus the existing validator.
2. **Rerun Hermes** before any comparative claim is published. ~$7.
3. `min_evidence: 2` is pre-registered and untested — the audit's
   over-generalisation finding still has no measurement.
4. `sop_bootstrap_06` has now scored below its no-memory floor in six
   consecutive arm-runs. It is a reproducible harm, and nobody has read a
   transcript of it.

## Results so far — read against the noise floor

**Three seeds, all 26 families, all three arms, paired by family
(`persist/pastbench/stats.py`; every Δ read from the benchmark's own
`sequence_comparison.json`, nothing re-scored). Seeds are `run1-sighted`,
`run2`, `run3` — all three post-fix; run 1's blind-gate readings are
archived, not pooled:**

| arm | mean Δ | s1 | s2 | s3 | mechanism | prompt tok / episode |
|---|---:|---:|---:|---:|---:|---:|
| areev-governed | +0.266 | +0.237 | +0.286 | +0.273 | 0.201 | 19,416 |
| areev-passive | +0.291 | +0.306 | +0.273 | +0.294 | 0.235 | 18,650 |
| hermes 0.4.0 | +0.246 | +0.252 | +0.265 | +0.221 | 0.166 | 28,269 |

**The noise floor makes the score differences unreadable.** The same arm on
the same family, across seeds, differs by **0.09–0.13** on average — larger
than any gap between the arms:

| contrast | mean difference | wins/losses | Wilcoxon p | sign p |
|---|---:|---|---:|---:|
| governed − passive | −0.025 | 14/12 | 0.780 | 0.845 |
| governed − hermes | +0.020 | 12/13 | 0.420 | 1.000 |
| passive − hermes | +0.045 | 16/10 | 0.213 | 0.327 |

**All three arms are statistically indistinguishable on the benchmark's own
metric.** Areev's passive arm has the highest point estimate and wins 16 of
26 families against Hermes, and that is all the data supports; the governed
arm against Hermes is a coin flip at 12–13. It is **not** "Areev beats
Hermes". Detecting a +0.045 difference against a per-family spread of ~0.16
needs roughly 70 families; PAST-Bench has 26, and a fourth seed would not
change it because family-to-family variation, not seed noise, dominates.

**The cost of the answer is where the difference is real.** Areev reaches
the same score on fewer prompt tokens per episode, in 18 of 26 families,
**Wilcoxon p = 0.005** (passive) and **0.010** (governed, which also pays
for its loop passes). Two honest readings of the size, both stated:

- **typical family**: Areev uses **80%** of Hermes's tokens (median ratio;
  mean ratio 0.90) — a fifth less, not a third;
- **aggregate**: **66%** — 18.7K against 28.3K — because Hermes blows up on
  a few families (121K per episode on `PC01_sop_bootstrap_05`, 51K on
  `PC01_sop_bootstrap_04`) where Areev's budgeted assembly does not. Six
  families see Hermes use more than twice Areev's tokens; on eight, Areev
  uses more.

The bill follows the aggregate; the typical episode follows the median. The
defensible sentence is "a fifth cheaper per episode and a third cheaper
across a workload, because it has no tail". Mechanism evidence favours
Areev (0.20–0.24 against 0.17) but not significantly (p = 0.42–0.56).

### The tuning leg — the memory did not move into the weights at this scale

The 1.7B model, served locally at its own 16K context, evaluated as the
agent through the same `areev-passive` scaffold and the same paired
protocol as every other arm (25 of 26 families; the sixth is the SOP family
that needs port offset 0):

| agent | with the memory in its prompt | with NO persistence | Δ | prompt tokens / episode |
|---|---:|---:|---:|---:|
| 30B + Areev memory (runs 2–3) | **0.62** | 0.34 | +0.28 | 18.0K |
| 1.7B **tuned** on the governed corpus | 0.468 | 0.317 | +0.151 | 15.7K |
| 1.7B **untuned** (the control) | 0.483 | 0.312 | +0.172 | 13.9K |

**The tuned model is not better than the untuned base** — 0.468 against
0.483 with the memory, 0.317 against 0.312 without it. If the corpus had
moved the memory into the weights, the tuned model would beat the control
*without persistence*; it does not, by 0.005 on 25 families, which is
nothing. This is §10's stated-in-advance outcome ("Tuned 1.7B ≤ untuned
control: published as a bound on the method at this scale, with the loss
curves"), and the bound is the honest headline: **32 training rows drawn
from 26 unlike task families did not transfer.**

Read beside [`FOURWAY.md`](FOURWAY.md), where the same recipe on the same
model size took a receipts agent from 97 to 571 of 720 on a 34-row corpus,
this sharpens rather than contradicts the tuning claim: that corpus was
**one** document type, one ledger, one set of conventions repeated 40
times; this one is 26 families with different services, tools and rules and
one or two rows each. The claim that survives is the one FOURWAY's own
sources state — distillation pays on narrow, high-volume, recurring work —
and PAST-Bench is the counter-example that says where it stops. Neither
small-model configuration reaches the 30B's 0.62 either.

**The held-out split says nothing either way, and says so at n=5.** The
second adapter trained on 20 families and was evaluated on the 6 it never
saw (5 completed). On those families:

| agent on the 5 held-out families | with memory | no persistence |
|---|---:|---:|
| 1.7B tuned on the **other 20** families | 0.496 | 0.324 |
| 1.7B tuned on **all 26** (saw these) | 0.483 | 0.299 |
| 1.7B untuned | 0.466 | 0.288 |

Both adapters edge the base here, and the one that never saw these families
edges the one that did — which is the ordering noise produces, not
generalisation. Five families against a per-family spread of ~0.1 supports
no claim at all, and it is reported as a null of measurement rather than a
null of method.

### The gate, audited by a second judge

`persist/pastbench/audit_reviewer.py` re-judges every decision of run 2's
governed arm against the evidence the reviewer was shown, using a
**different model** (MiniMax-M2.7, the benchmark's judge; the reviewer is
gpt-4o) and a narrower question: *from this evidence alone, is the entry a
durable general rule the evidence states or implies?* 88 decisions with
their evidence:

| the reviewer | auditor VALID | auditor INVALID |
|---|---:|---:|
| approved (28) | 13 | **15** |
| refused as a duplicate of a live entry (44) | 28 | 16 |
| refused as unsupported (5) | 0 | 5 |
| refused, other reasons (11) | 1 | 10 |

**Raw agreement is 50%.** Counting the dedup refusals as correct — the
auditor sees only one proposal and its evidence, never what the memory
already holds, so it cannot judge a duplicate — agreement is **82%**, and
the 5 "unsupported" refusals are unanimous.

The finding that matters is the other diagonal: **15 of 28 approvals are
over-generalisations the auditor rejects**, and the shape repeats — the
evidence records one instance ("log this vendor's decision", "check the
prior decision for Vendor X") and the approved rule quantifies it over all
future cases. That is a plausible mechanism for the null in the row below:
the gate is admitting rules that are true of one episode and inert in the
next. A rubric clause about quantifying beyond the evidence is the obvious
next change, and it is **not** being made mid-programme — the rubric was
frozen before the runs and stays frozen; this is recorded as the finding
and as the first candidate for a follow-up round.

**The governed loop still adds nothing measurable over the plain store**
(−0.004, p = 0.879), now with a *sighted* reviewer that approves 22–28
proposals per run and refuses 52–61 with reasons. That is §10's second
bullet, stated in advance, and it is the finding this track most has to
report honestly: the substrate carries the gain; the gate demonstrably
refuses what it should and admits what it should, but on these 26 families
the rules it admits do not move the score beyond what the agent saves for
itself.

**Run 1, all 26 families, after defects 12–17 (2026-09-07; the benchmark's
Δ on evaluation episodes, macro-averaged; its mechanism score; the agent's
mean prompt tokens per episode with memory; one run, no noise floor yet):**

| arm | Δ all (26) | memory (5) | procedural (8) | info-gathering (6) | update (7) | mechanism | prompt tok / ep |
|---|---:|---:|---:|---:|---:|---:|---:|
| areev-governed | **+0.236** | +0.212 | +0.180 | +0.389 | +0.186 | 0.216 | 18.6K |
| areev-passive | **+0.237** | +0.122 | +0.191 | +0.363 | +0.262 | 0.219 | 19.0K |
| hermes 0.4.0 | **+0.244** | +0.252 | +0.194 | +0.418 | +0.145 | 0.161 | 34.0K |
| mem0 (12 of 26 so far) | +0.138 | +0.307 | — | +0.017 | — | 0.082 | 15.2K |

Read with the noise in mind (§10's last bullet; one run per arm): the three
full arms sit within 0.01 of each other overall; Hermes leads on memory and
information gathering, the Areev arms lead on update and on mechanism
evidence, at roughly half of Hermes's prompt tokens. Governed and passive
are the same substrate in run 1 by construction: the loop **proposed 75
times and the reviewer refused every proposal** — because it was never
shown the cited evidence (§11 #18, found after run 1: the binding's
recommendation listing carries no evidence field; the stored record does).
Run 1's governed arm is therefore the **blind-gate** reading and is kept as
such; run 2 is the first run with a sighted reviewer, and the
governed-vs-passive contrast the design exists for is measured there and
in run 3.

**Amendment, 2026-09-06 (after run 1; the user's decisions):** the cap
rises from **$30 to $50**; runs are parallelised — run 2 and run 3 of the
three main arms start together on six port-offset streams, plus a
**sighted rerun of run 1's governed arm** (the blind-gate reading stays
archived), plus mem0's remaining families and the tuned evaluations; every
model leg now pins an OpenRouter provider **order**, `streamlake,alibaba`
(StreamLake first, Alibaba only when it throttles; the Hugging Face router
lists no enabled provider for this model on the available token), which is
disclosed as the pin; Hermes gets its third run. One quality change to the
Areev substrate goes in before these runs, with evidence from run 1 (§11
#19): `session_search` returns the whole thread of each matching session
instead of single matching turns. No arm's scoring, judge, families or
reviewer rubric change.

**First valid family-run (SM01, governed, StreamLake, 2026-09-06):**
Δ on the evaluation episodes **+0.304** (0.704 with persistence, 0.400
without), mechanism 0.3, memory injected in both evaluation episodes; the
agent saved one note in learn-A that carried the family; the loop proposed
one lesson at learn-A (dropped at GROUND/VERIFY) and one in a control
episode, which the reviewer refused as unsupported — a correct refusal, the
rule there lived in a fixture, not in a person's instruction. This is one
family on one run and is quoted here as the pilot's first meter reading,
not as a result. The two earlier SiliconFlow smokes are not comparable
(§7).

## 4. Track B — Horizon

### 4.1 The Areev agent

`agents/areev/` in the Horizon layout (a `harbor.agents.base.BaseAgent`
subclass, async throughout, host-side): ingest the trace into one Areev file
— each `function_call`/`function_call_output` pair a **Tool grain** (name,
input, result, error, timestamp), each `reasoning` and `message` an
**Event** in a session keyed by UTC day — then run the governed loop over
it **before the task starts**: `tool_failure` clusters the breakages
(the `curl`-is-broken shape is exactly its input), DISCOVER proposes the
lessons the trace implies, the fixed-rubric reviewer approves or refuses,
approved lessons apply. The task then runs with an assembled memory
(lessons + `session_search` over the trace's Events) and the task's own
tools from `/.horizon/tools/tools.json`, like `trace_rag` does. Two
variants, one flag: **passive** (ingest + search, no loop) and **governed**.

### 4.2 What it can and cannot show

- On the **three public tasks**: a smoke and a per-task cost/latency/token
  reading beside `trace_rag` and `tools_only` on the same model — plumbing,
  never a claim. Measured 2026-09-06, qwen3-30b on StreamLake, one trial per
  task (the reward is deterministic; the model at temperature 0 is not
  quite):

  | agent | tasks solved | prompt tokens / task | chat $ / task | what carries the answer |
  |---|---:|---:|---:|---|
  | `trace_rag` (reference) | **3 / 3** | 21–27K | 0.0015–0.0021 | whole-day chunks by embedding similarity |
  | `areev-governed` | 1 / 3 | 12–16K | 0.0006–0.0008 | Tool/Event grains, keyword + hybrid search, best-day digest; loop found nothing to learn (no human turns, no tool errors in these traces) |
  | `areev-passive` | 1 / 3 | 12–16K | 0.0006–0.0008 | the same memory without the loop |
  | `tools_only` (floor) | 0 / 3 | 4–6K | 0.0003 | nothing |

  The two Areev misses are the same shape: the memory returned the right
  records (the vendor's quote with its rate and contact; the requester's
  reading group with its paper and format) and the model's reply carried a
  different detail than the judge's regex requires — the approval date
  instead of the rate or contact; and, under two other reading groups in
  the same search results, the *distractor* group's paper and format
  instead of the requester's. Retrieval got the model there at half the
  tokens; the reply did not say the words, and once it followed a
  distractor the memory had faithfully kept. Six
  iterations of generic fixes went into the agent (Tool grains are not
  returned by hybrid search and are keyword-scanned; the day around the best
  hit is reconstructed; human turns are Observations; rate limits retry) and
  it stops here — anything further would be tuning to three examples. The
  private set is the test that counts, and the number to beat there is the
  leaderboard's, not `trace_rag`'s on three tasks.
- On the **private 195**: the number that matters, obtainable only by
  submitting the agent (PR + email to Orin Labs). Submission is a decision
  for the user (§9); the agent is written to the repo's contract either way.
- The tuning leg does not apply here: one trace per task, no held-out
  entity, no corpus. Horizon contributes Tool/Event-grain learning and a
  leaderboard, not a tune.

**Harness notes (local only, disclosed).** Harbor 0.22 rejects non-numeric
keys in `reward.json`; the three public judges write the reply text there,
so a local patch moves free text to `reply.json` and flattens the metrics to
0/1 (`horizon_judgepatch.py`; task digests in `dataset.toml` no longer match
the local copies, which is irrelevant to the private set). The reference
agents mint a per-trial USD-capped sub-key from an OpenRouter *management*
key; the office env file provides one (`OPENROUTER_MANAGEMENT_API_KEY`,
exported as the `OPENROUTER_MANAGEMENT_KEY` the agents read), and a local
fallback to the plain key exists for when it is absent. The environment
image's default trace URL (`orinlabs/horizon-example-traces`) returns 404
even with a token; the public set is `orinlabs/horizon-1-example-traces`,
and the base image is re-tagged locally with that URL.

## 5. Overfitting — the record, not a hope

| control | where |
|---|---|
| entity split — held-out **families**, never just held-out episodes | §3.3 |
| validation-selected checkpoint, iterations scaled to corpus size, full loss curve in the manifest | the CUDA trainer (§7) |
| seen vs unseen, paired separately; the difference between halves is memorisation | `slm_overfit.py`, generalised |
| memorisation probe: exact-string recall of training rows from the tuned model | `persist/overfit.py` |
| A0 cross-arm drift check: all arms ignorant at cold — a gap there is provider drift | §3.2 |
| PAST-Bench control episodes (shortcut, surface, stale, wrong substrate) scored for every arm including the tuned one | §3.3 |
| `OVERFIT.json` beside every result, as in `results/fourway-2026-09-05/` | §9 |

## 6. Cost and capital — what is captured

| metric | source |
|---|---|
| $ per family and per 100 episodes — **agent**, **learn** (loop + review), **judge**, per arm | benchmark trace + `usage.jsonl` → `cost.py`, pinned prices |
| prompt tokens per episode per arm; injected memory chars | benchmark trace; `areev_ledger.json` |
| Horizon: $ / tokens / wall time per task per agent | Harbor `result.json` and the agent's ATIF trajectory |
| tuned-model tokens at zero marginal **and** at a shadow hosted small-model rate | `cost.py`'s two readings |
| GPU-hours and peak VRAM per adapter; wall time per training run | trainer manifest (`nvidia-smi` sampled during the run) |
| local inference latency and throughput of the 1.7B on the RTX 4060 | `slm_serve.py` meter against vLLM |
| hardware class each row *requires*, with dated list prices | stated, labelled as list prices |
| electricity | **not measured** — said so rather than estimated |

## 7. Infrastructure — the office box (as built, 2026-09-06)

`swe@192.168.1.2`: Ubuntu 24.04, 32 threads, 31 GB RAM, RTX 4060 8 GB,
driver 595, 660 GB free. Installed today: rustup/cargo; `areev` 1.7.2
release binary and the `areev` Python binding (maturin, into the PAST-Bench
venv); `~/mg/local/persist-venv` with torch (CUDA wheels), vllm 0.28,
transformers, peft, trl, datasets, mem0ai, qdrant-client; ollama with
`mxbai-embed-large` (mem0's embedder); PAST-Bench at `f822351` in its own
uv-managed Python 3.11 venv with Hermes 0.4.0 and Hermes+ installed and the
sandbox image built; Harbor 0.22 with the Horizon reference agents; the
Horizon environment image. The `bench/persist` worktree is rsynced to
`~/mg/products/areev-persist` (never pushed). Runs live in
`~/mg/local/areev-runs/persist/`; only counts, manifests and checksums
travel back into `results/persist-<date>/` in
[AreevAI/areev-benchmark](https://github.com/AreevAI/areev-benchmark).

Model legs, all through OpenRouter on the one key in `dev-areev.env`:
agent and DISCOVER/VERIFY `qwen/qwen3-30b-a3b-instruct-2507` pinned to
**`streamlake`**; GROUND `openai/gpt-4o-mini`; review `openai/gpt-4o`; judge
`minimax/minimax-m2.7` ($0.30/$1.20 per M) — the paper's judge, so the
Hermes reference numbers are comparable. Every leg seeded; temperature 0.

**The tuning leg, as built.** `persist/tune/slm_train_cuda.sh` +
`train_lora.py` — the CUDA twin of `receipts/slm_train.sh` (transformers +
peft): the same corpus format, LoRA rank 8 on the last 8 layers as on the
laptop, batch 1 with the iteration count scaled as the MLX trainer scales
it, validation every 25 steps, lowest-val checkpoint kept, loss curve, peak
VRAM and wall time in the manifest. **Measured 2026-09-06:** the bf16 base
does not fit the 8 GB card — the first backward pass of a 2K-token
trajectory OOMs on the fp32 logits — so the base loads as **nf4 (QLoRA)**
with gradient checkpointing; 40 iterations take 105 s at 7.1 GB peak. The
adapter is served on the bf16 base (vLLM, LoRA applied, Hermes-style
tool-call parsing, the torch sampler because there is no CUDA toolkit to
JIT FlashInfer's) — the usual QLoRA practice and a disclosed mismatch. The
laptop trains `mlx-community/Qwen3-1.7B-4bit`: same model, both on a 4-bit
base, one row of the combined table. Rows whose assistant turns fall past
the 2K window are dropped and counted (2 of 6 on the smoke corpus, with
tool results already capped at 1,200 chars); non-finite steps are skipped
and counted (0 after the cap). `pastbench/corpus.py` builds the corpus from
a governed run's learn and cold episodes with the injected memory section
stripped from the system prompt and the persistence tool calls removed
from the trajectory — the benchmark does not log its composed system
prompt or tool schemas, so the adapter writes both into its ledger.

**The pin, and why it moved twice.** The receipts programme pinned
`coreweave/bf16`; on 2026-09-06 OpenRouter listed no CoreWeave endpoint for
this model (StreamLake, SiliconFlow fp8, Nebius fp8 (down), Alibaba
remained). `siliconflow/fp8` was chosen first for its declared quantization
and 262K context, and **rejected after two smokes**: on the benchmark's
opening request it returns `finish=stop`, empty content, and exactly the 28
output tokens of the tool call the other endpoints return — its tool-call
parsing swallows the call, so whole episodes ended at turn 1 with a 0.2
score. Replayed with `pinprobe2.py`, StreamLake and Alibaba both return
`notes_list`, streaming or not. StreamLake is the cheapest of the two and
carries the price the cost table already pinned for this model
($0.048/$0.193 per M); its quantization is undeclared, which is recorded
rather than assumed. Both SiliconFlow smokes are kept on the box as
`*-siliconflow*` and are not results.

## 8. Budget — $30 cap; meters replace estimates as they land

| item | reading / estimate | basis |
|---|---|---|
| Horizon smoke, `trace_rag`, one task | **$0.0011, 22.3K tokens, 25 s** (meter) | Harbor trajectory `extra.cost_usd` |
| PAST-Bench, one episode, governed arm | **~10–12K prompt tokens with memory, ~8K without; 4–5 turns; ~20 s** (meter, SM01 on StreamLake) | benchmark trace |
| PAST-Bench, one family-run (8 episodes × 2 conditions), governed, everything included | **$0.020 key-usage bound, 353 s wall** (meter, SM01, 2026-09-06) | `pilot/spend.jsonl` |
| PAST-Bench, one full arm-run (26 families) incl. loop + review + judge | ~$0.55 (extrapolated from the meter) | pilot |
| five arms × 3 runs + A0R floor | ~$8–10 (extrapolated) | pilot |
| tuning | $0 marginal | the box; GPU-hours reported |

The key's account balance at start (2026-09-06): **$47.5 of $185
remaining**, shared with the laptop's running CURVE benchmark; this track
stops and asks at $30 of its own metered spend, or earlier if the account
runs low.

## 9. Order of work and gates

1. ~~Box setup~~ — done 2026-09-06 (§7).
2. ~~Keyless self-test of the adapter~~ — passes (`selftest.py`: fixture
   import, injection, all five tools, rendering, counters, diff, retrieval
   signals, governed close with deterministic analyzers, anchors, the
   no-persistence condition).
3. **SM01 smoke** (running): governed arm, both conditions, real judge.
   Gate: Δ and mechanism computed by the benchmark, ledger non-empty,
   metered cost. Then the same family on `areev-passive` and `hermes`.
4. **Pilot**: one family per capability × the three arms. Output: $/family
   per arm, wall time, judge trace contents. Gate: the extrapolated full
   budget fits §8 or the arms are cut here, in writing.
5. **Pre-registration commit**: this file with the meters; §10 untouched.
6. **Full runs**: 26 families × (governed, passive, hermes) × 3 runs, plus
   A0R; mem0 if the budget reaches it.
7. **Horizon**: the `areev` agent on the three public tasks beside
   `trace_rag` and `tools_only`; submission if approved.
8. **Tuning legs, overfit probes, cost manifests, `verify.py --check`,
   `MANIFEST.md`.**
9. **Combined report** — after the laptop's CURVE and DocILE land —
   `crates/areev-bench/PROGRAM.md`: every corpus × (governed effect, mem0,
   passive, tuned before/after, seen/unseen, prompt tokens, $/1k,
   GPU-hours, hardware class); the grain-coverage matrix; the governance
   ledgers; the engine defects found.

Nothing in 6–9 starts before 5 is committed. Nothing is pushed or merged
without approval.

## 10. Stated in advance

Written before any scored run; the pilot may amend the budget, never these.

- **Governed > passive > none on Δ, mechanism rising in the same order, and
  the Update ledger showing refusals or reverts:** the headline — the loop,
  not the store, is what improves the agent, and it improves the four
  capabilities through four different grain types.
- **Governed ≈ passive:** the store carries the benefit and governance costs
  the same for no gain here; published as that. The ledger still says what
  governance *refused*, which is a safety result, not a performance one.
- **Governed < passive:** approved artefacts trade breadth for precision
  (the earlier arm result) replicates on a new workload — strengthens rather
  than retracts it; the per-capability table says where.
- **Areev arms < hermes:** the scaffold, not the substrate, is the
  difference; published either way, with the prompt-token column beside it.
- **Tuned 1.7B with no persistence read ≥ 30B with full assembly on seen
  families, and above the untuned control on unseen:** the memory moved into
  the weights and the model learned to use it. On unseen families only the
  second half is claimed.
- **Tuned 1.7B ≤ untuned control:** the corpus is too small at 204
  episodes; published as a bound on the method at this scale, with the loss
  curves.
- **Horizon public tasks:** plumbing only; no claim from three tasks.
- **Any arm inside the A0R noise floor:** not interpreted.

### Stated in advance — run 4 (written 2026-09-07, after the three-seed forensics, before any run-4 episode)

Run 4 is the same design — three seeds, the three main arms, all 26
families, the same model and provider pin, Hermes untouched — with four
changes to the Areev arms, each aimed at a mechanism the ledger names
(§11 #20–#22, #23 deliberately left open) and each a policy that defaults to
the run-3 behaviour:

1. **The Verify gate is live**: `checkpoints: [{"after_runs": 1}]` — a
   lesson is measured at the next graded episode after its apply, and the
   gate's second question, `premise_drift`, is on (#20, #22).
2. **The proposer may author a Skill or a Plan** from a trajectory that
   succeeded; successful tool calls reach the evidence bundle (#21).
3. **The agent may give a note an `expires`**; an expired note is not
   rendered and the `staleness` analyzer proposes its tombstone.
4. `min_evidence` stays at 1, so run 4 measures the writer and the gate,
   not the evidence floor. `AREEV_LOOP_POLICY_EXTRA={"min_evidence": 2}` is
   the pre-registered arm for a run 5 if governed still trails passive.

Predictions, in order of confidence:

- **Procedural (8 families):** the empty-store arm-seeds (9 of 60 in runs
  1–3) have a writer. Prediction: Areev's procedural Δ rises from
  +0.144/+0.112 toward Hermes's +0.169; `sop_bootstrap_04` and `_06` stop
  scoring below their own no-memory floor. If procedural does not move, the
  writer is not the cause and the proposer's skill/plan drafts are being
  refused — the funnel says where.
- **Governed vs passive** (tested by the governed RE-RUN only; §11 #28 made
  the first governed leg a blind-revert reading, kept as
  `areev-governed-blindrevert`): runs 1–3 read −0.025 (p 0.78) with zero reverts.
  Prediction: governed ≥ passive once harmful lessons are measured at the
  next episode and reverted, and lessons whose premise was superseded are
  reverted (`rule_migration`, −0.324 in run 3, is the named case). If
  governed still trails passive with reverts now firing, the lessons
  themselves are the harm and run 5's `min_evidence: 2` is the test.
- **Passive vs Hermes (the headline):** +0.045 (p 0.21) as measured. The
  counterfactual said parity on `sop_bootstrap_04`, `failure_to_rule_01` and
  `constraint_retention` reaches p ≈ 0.016. Run 4 addresses the first two;
  #23 is deliberately unchanged, so the bar for run 4 is **p < 0.10 on the
  paired Wilcoxon**, and a significant win is not claimed from it. If passive
  − Hermes is unchanged or worse, published as that, beside the funnel.
- **Cost:** no more than +10% prompt tokens per episode on the Areev arms —
  plans and skills render into the prompt; the bundle's reserved shares are
  unchanged. Above that, the cost claim is re-stated with the new number.
- **Any per-family swing inside the noise floor (0.09–0.13):** not
  interpreted.

Cap for run 4: **$20** (raised twice, both after the run began and both recorded here: $12 → $15 to pay for the governed re-run §11 #28 forced, → $20 with the run explicitly not to be stopped for budget). Measured against runs 1–3's key deltas — roughly
$5–6 per full three-arm seed, Hermes the costliest arm at ~$0.35 per family
against ~$0.25 for either Areev arm — three seeds of all three arms is
~$15–18, above the cap. Hermes's code is untouched, so run 4 runs the **two
Areev arms** for three seeds (~$8–10) and pairs them against runs 1–3's
Hermes rows, stated as such; a Hermes rerun (~$7) is a provider-drift
control that can be added to the same roots at any time, and is recorded
here if it is. The three-seed statistics script and the audit run
unchanged; the harness's configuration is written beside every ledger as
`loop-policy.json`. Run 4 was preceded by a one-family paid smoke
(`PC01_sop_bootstrap_04`, both Areev arms, $0.14), which found #24.

## 11. The defect ledger — what building the harness found (running)

Recorded as the receipts programme recorded its six, because each one
changed a number before it was caught.

| # | found | where | what | fix |
|---|---|---|---|---|
| 1 | 2026-09-06, first smoke | engine + harness | the loop's all-namespace evidence scan skips every `agent:*` namespace as governance metadata, so a memory living in `agent:persist` was invisible to DISCOVER — every pass reported `evidence: 0` while the file held facts, events and tool calls | harness memory moved to `desk:persist`; the engine behaviour is by design and now documented at the call site |
| 2 | 2026-09-06, first smoke | harness | text extraction handled dict blocks only; the benchmark's pydantic `TextBlock`s yielded "" — the user's instructions were never recorded as Observations, so DISCOVER had no human evidence | `_text_of` reads pydantic blocks and nested tool results |
| 3 | 2026-09-06, second smoke | provider | SiliconFlow fp8 swallows this model's tool calls on some requests (28 tokens, empty content, `finish=stop`); episodes ended at turn 1 | pin moved to StreamLake; both smokes archived as invalid |
| 4 | 2026-09-06, Horizon smoke | harness | the Horizon agent package was named `areev` and shadowed the `areev` binding on `PYTHONPATH=agents` | package renamed `areev_agent` |
| 5 | 2026-09-06, Horizon smoke | harness | `session_search` returned narration only — reasoning Events outnumber and out-rank Tool records, so the vendor quote in an `inbox_read` output never surfaced and the task scored 0 | wider candidate set, Tool and Event hits interleaved; human `message` turns also recorded as Observations so DISCOVER has evidence |
| 6 | 2026-09-06, Horizon | harness (upstream) | Harbor 0.22 rejects the public judges' `reward.json` (free text and a nested dict where numbers are required) | local judge patch, numeric keys only; digests no longer match, irrelevant to the private set |
| 7 | 2026-09-06, pilot PC01 | harness | the reviewer never saw the cited evidence — the lookup used `RECALL grains WHERE hash = …`, not a CAL noun, failed silently, and every proposal was refused as unsupported, three correct SOP lessons among them | cited hashes resolved from an index of the namespace's observations, facts, tools and events |
| 8 | 2026-09-06, pilot PC01 | harness | the agent finished learn episodes with the procedure demonstrated and nothing saved (`skills_list` calls only); Hermes gets a memory-flush and skill-creation nudge from the benchmark, our adapter had none | one session-end nudge, the save step asked for once, for both Areev arms |
| 9 | 2026-09-06, pilot SM01 passive | benchmark | `summarize_reflection_episode` reads `content[0].text` of the last message; in the no-persistence reflection the model has no memory tools, spends all 20 turns on task tools, the trace ends on a tool result, and the family-run dies with AttributeError | second patch: the last text block, or "" |
| 10 | 2026-09-06, Horizon | engine | hybrid `search()` returns Event grains only on this file; the 13 Tool grains naming the vendor never surfaced, and a tool's output lives in `tool_content` | harness keyword pass over Tool grains; to raise as an engine question (should Tool grains be text-indexed for recall?) |
| 11 | 2026-09-06, mem0 smoke | harness | mem0's embedder needs the `ollama` client library, absent from the venv: every `add()` failed, the ledger recorded it, and the arm scored Δ 0.0 as if it were a result; then mem0 2.x's read calls refused `user_id=` (`filters=` and `top_k` instead) | library installed; a failed write now fails the family-run; 2.x call shapes with a 1.x fallback |
| 12 | 2026-09-07, full run 1 | benchmark | the model provider retries five times with a flat 2–4 s wait (~15 s); under StreamLake's rate-limit windows with three streams on the key, 11 of 29 governed and 15 of 29 passive family-runs died on `RateLimitError` at "attempt 5/5" (three of each were the `_shared` fixture directories the first driver listed as families) — $3.9 of partial runs on the key | third patch: twelve attempts, doubling wait capped at 60 s; the driver lists real families only; the failed families resumed, the completed ones kept |
| 13 | 2026-09-07, run 1 audit | harness | the benchmark pre-seeds prior sessions as one `session_seed.json` (`{"sessions": [{id, title, messages}]}`); the importer read a `sessions/` directory only, so the six information-gathering families — expected signal `session_search` — ran with no prior session to search. Their run-1 rows (Areev Δ +0.00 to +0.49, Hermes +0.32 to +0.50) are kept under `pre-seed-fix/` as the unseeded reading | each seeded session imported as its own thread of Events, title on the first; the six families rerun for both Areev arms inside run 1 |
| 14 | 2026-09-07, run 1 audit | harness | the reviewer refused all 67 loop proposals of run 1 ("lacks evidence" in 50) and the decision record did not carry the evidence shown, so whether the gate or the drafts were at fault could not be audited from the ledger; the anchors' memory copies hold no loop records either (open question) | the evidence text travels with every decision from here on; run 1's refusals are audited post hoc by re-judging the drafts against their cited grains, labelled as such |
| 15 | 2026-09-07, run 1 | harness | first read as "`x_mock` ignores `--port-offset`"; the real cause (found when run 3 lost a family in its first minute): families bind service ports 3200, 3300 and 9105–9210, so streams offset by 100 collide — one stream's 9105+100 is another's 9205 — and whichever binds second exits at once. Every early "did not become ready / exited immediately" was this | offsets spaced 10,000 apart (0, 10000, …, 55000; ports stay under 65535); `fixup.sh` still reruns an arm's missing families alone at offset 0 |
| 16 | 2026-09-07, seeded rerun | harness | with the sessions imported (#13) the three `session_search` families still scored Δ ≈ 0: the agent never called `session_search` in any evaluation episode. The prompt said only that earlier sessions were "searchable"; Hermes's tool description says when to use it and a no-query call lists recent sessions' titles for free | the injected section lists the earlier sessions' titles (most recent first, up to 30) and the tool description says when to search — parity with what Hermes exposes, not a task hint; the six families rerun once more inside run 1, the unseeded and the untitled readings both kept |
| 17 | 2026-09-07, titled rerun | harness | `PG01_release_decision_followup`'s seeded sessions plus the family's own turns passed the 500-grain scan cap the harness inherited from the receipts rule ("a truncated prompt is a wrong prompt"), the scan raised, the exception's traceback kept the file handle alive, and the next open failed with STO-E002 for both Areev arms | event scans are bounded at CAL's 1000 and never raise (a truncated title list is a bounded answer, not a wrong one); `with_memory` drops the traceback before releasing the handle; PG01 rerun |
| 18 | 2026-09-07, run 1 audit | harness | the ROOT CAUSE of #7 and #14: the binding's `recommendations()` JSON has no `evidence` field at all (analyzer, summary, target_ref, status, hash, severity, …), so every reviewer call since the pilot was handed "(none)" and the gate refused 75 of 75 proposals in run 1 as unsupported — the governed arm was the passive arm plus the loop's cost, by construction. The cited hashes live in the stored recommendation Fact (`areev-loop` namespace, relation `loop_recommendation`, the record in `object`) | the reviewer reads the evidence from the stored record; run 1's governed arm is the *blind-gate* reading and stays; run 2 is the first sighted run and is labelled so |
| 19 | 2026-09-07, run 1 audit | harness | `session_search` returned single matching Events out of context: on the three session-search families Hermes (session summaries) beat the Areev arms by 0.13–0.15 even after the titles fix, and the returned turns named the session that mattered without carrying what it said | the top matching sessions are returned as whole threads, in order, bounded — lossless where Hermes summarises |
| 20 | 2026-09-07, three-seed forensics | engine | the Verify gate never fired: 81 lessons applied across the 78 governed arm-seeds, **0 verdicts, 0 reverts**. The adapter journaled every episode's score as an evalset run and set `outcome_evalset`, but no schedule, so the loop's default applied — the first checkpoint a day after the apply — on families whose median wall time is 6.8 minutes (max 12). The half of governance the receipts harness had just proved end to end was structurally unable to come due; every harmful lesson in #22 stayed live | the loop takes its schedule in the deployment's unit — `checkpoints: [{"after_runs": 1}]` measures at the next graded episode — and the harness sets exactly that; `cadence` is policy too, for the same reason in the other direction. **Necessary, not sufficient**: the run-4 smoke found a second cause (#24) that would have kept every verdict at zero regardless |
| 21 | 2026-09-07, three-seed forensics | engine | on procedural families 9 of 60 Areev arm-seeds had an EMPTY store at evaluation (Δ −0.033, below no memory); the other 51 scored +0.169 — Hermes's procedural score to the third decimal. In all nine the agent performed the procedure correctly in the learn episode (0.856 on `sop_bootstrap_04`, identical to Hermes) and then answered the session-end nudge "nothing to save". Every skill in the memory depended on the model volunteering one mid-task; Hermes runs a separate review pass that writes for it. Where Areev did write, the skill was thin (`artifact_rule_quality` 0.25 vs 1.0 on `failure_to_rule_01`): five terse lines against trigger / steps / verification / pitfalls | DISCOVER may author a Skill (`skills` policy, default on): description, `when_to_use`, ordered steps, named by the target, superseding a live skill of that name; successful tool calls join the evidence bundle after the failures, and the Tool brief carries the call's input. Gate-governed like every draft; never auto-applied |
| 22 | 2026-09-07, three-seed forensics | harness + engine | of the 81 applied lessons only 41 came from learn episodes — 25 from control and 15 from evaluation episodes; 61 carry an absolute quantifier; 11 tell the agent to stop or escalate. One, "if a method is not found after three attempts, escalate to human immediately instead of retrying", applied at `eval_near`, took `sop_bootstrap_05` from passive +0.730 to governed −0.010; on `rule_migration` the loop encoded the OLD regime's flag as standing policy (−0.324). The audit's finding — 15 of 28 approvals generalised one instance — has a mechanism | `min_evidence` in the policy (default 1, today's behaviour; the audit argues 2); with #20 fixed, a harmful lesson is now measured at the next episode and reverted. The harness leaves `min_evidence` at its default for comparability and exposes `AREEV_LOOP_POLICY_EXTRA` to set it per run |
| 24 | 2026-09-07, run-4 smoke | harness | with the schedule fixed, one family of the governed arm still recorded **no verdict** — every stored recommendation carried `metric: null`. The harness journaled each episode's score as `{"task_score": 0.234, "passed": false}`: a **boolean** `passed` and no `failed`. The loop's eval reader is fail-closed by design — a summary missing its counts is dropped, never defaulted (an absent `failed` must never read as zero failures) — so every run ever journaled here was unreadable, no metric ever attached, and the Verify gate had nothing to measure in runs 1–3 for THIS reason as much as for #20. The receipts harness writes integer counts, which is why the same gate works there | the harness writes `passed`/`failed` as integer counts beside `task_score`, through one helper the keyless selftest pins; the run is re-smoked before launch |
| 23 | 2026-09-07, three-seed forensics | adapter | `constraint_retention`: both systems store the boundary with artifact quality 1.0 and retrieval 1.0; Areev scores a flat 0.664 on every seed, Hermes 0.904 on `eval_far`. Hermes routed it to `USER.md` (a behavioural expectation, always in context); Areev's agent used the profile slot in **0 of 156** arm-seeds. Where the governed loop restated the constraint as a procedure ("verify each recipient is an internal employee before sharing") `eval_far` rose 0.664 → 0.864 on two of three seeds: the same knowledge as a directive is obeyed, as a fact it is not | open — render constraints as in-scope directives and route behavioural rules to the profile section (the paper's E2); not changed before run 4 so the three fixes above can be attributed |
| 25 | 2026-09-07, run-4 smoke | harness | with the journal readable (#24) the first learn episode still journaled nothing, so the family's first applied lesson was proposed with no run behind it and no metric attached. The cold baseline runs once per family in a shared pre-pass and its directory is back-filled into each variant BEFORE that grading lands: the search found the variant's `01_…`, read no score, and stopped. Looking in one place was the bug, not the place it looked | the search continues until a SCORE is found — variant first, then `shared_cold/` — and the keyless selftest pins both halves. Verified live: `evalset:PC01_sop_bootstrap_04:task_score`, baseline 0.246, checkpoints `[{"after_runs": 1}]` on a stored recommendation |
| 26 | 2026-09-07, run-4 smoke | harness | a `held` verdict proposes nothing, and only the post-learn ANCHOR memory is archived — so a verdict recorded in an evaluation episode's working memory left no trace anywhere. Runs 1–3's "0 verdicts" was read off the ledgers, which counted reverts and never carried the verdict series at all | `govern()` writes the outcome series and a held/regressed/drifted tally into every ledger, so "did the gate fire" is answerable from the archive |
| 27 | 2026-09-07, run-4 probe | engine | over ~35 loop passes across five smokes the proposer authored no skill and no plan. Probed directly against a clean successful trajectory it authored a PLAN every time — the benchmark family it was smoked on simply had no successful procedure to capture. But the plan was discarded twice over: its edge conditions (`tickets.length > 0`) are outside the runtime's frozen v1 grammar, and one step of four named `tool` as its tool — a reasoning step that calls nothing, which the grounding rule read as a fabrication and rejected the whole draft for | a graph the runtime refuses is recorded as a SKILL rather than dropped (the procedure is what is worth keeping; the model's own guard survives as prose); a step whose tool the evidence never shows keeps its instruction and loses the attribution, with at least one grounded step still required. Verified live: the same draft now lands as `record skill: "resolve_shared_incident" (4 steps)` |
| 28 | 2026-09-08, run 4 in flight | harness | the gate fired — 26 measured regressions, 12 revert proposals reaching the reviewer — and the reviewer refused **every one** with "advisory only — asks for no change". Its approve-a-revert branch tests `analyzer == "outcome_review"` against the real id `loop.outcome_review/1`; the comparison is always false, so a revert fell through to the generic path, classified as advisory, and was dismissed. Detection worked and correction never fired, which makes run 4's governed leg a **blind-revert** reading exactly as run 1 was a blind-gate one | matched by analyzer FAMILY, version-insensitive, pinned by the keyless selftest; run 4's governed arm is archived as `areev-governed-blindrevert` and re-run with the fix (cap raised to $15 for it). The passive arm has no loop and is unaffected |

## The harness moved onto Areev's own surfaces (2026-09-08)

Recorded under rule 4 of `CLAUDE.md`. **Runs 1–4 precede it** and no number in
this document is revised.

PAST-Bench turned out to have the least CAL-expressible prompt of the five
tracks, and that is the useful finding. Of the four injected sections, one
moved to a saved `ASSEMBLE` query under a real token budget; three cannot, for
reasons that are specific and worth knowing before the next bench tries:

- **Notes** need a `valid_to` filter, which is not a queryable field on facts
  (`CAL-E060`) and not a template variable (`CAL-E042`).
- **Skills** mark retirement by writing `description: "retired"`, and
  `description` is not filterable on skills (`CAL-E060`). Filtering on `object`
  instead does not work *and does not warn*.
- **Earlier sessions** need one row per session with a regex-extracted title.
  `GROUP BY` reorders rows rather than projecting one per group, and CAL has no
  text extraction.

`persist/pastbench/AREEV.md` records all three. The injected block itself moved
to `persist/pastbench/prompt.py`, which imports no PAST-Bench, so it loads and
is parity-checked without the benchmark installed.

### Which budget produced the token figures

`CLAUDE.md` requires naming this wherever a token effect is reported, and this
document reports one. The honest statement:

- the **user-profile** section is bounded by CAL's `BUDGET 1200 tokens`,
  counted by the engine's own estimator;
- **everything else** is bounded by `MAX_INJECT_CHARS = 12_000`, a **character**
  cap applied to the composed block in `_inject_memory`.

A cap counted in characters is not a token budget. Every token claim above
means the second one unless it says otherwise.
