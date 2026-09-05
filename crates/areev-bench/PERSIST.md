# Governed self-improvement across sessions — PAST-Bench and HorizonBench

**Status: proposal, 2026-09-06. Not pre-registered, nothing run, no money
spent.** This becomes a pre-registration when its open questions (§11) are
answered and the pilots (§9) have replaced every estimate with a meter
reading. Until then every dollar and hour in here is an estimate and says so.

## 1. The question, stated precisely

[`RECEIPTS.md`](RECEIPTS.md), [`ADBUY.md`](ADBUY.md), [`DRIFT.md`](DRIFT.md)
and [`CURVE.md`](CURVE.md) all ask one shape of question: an agent fills a
row from a document, a person corrects it, and the loop turns corrections
into governed rules. That is one grain type (a `lesson` Fact) learned from
one kind of evidence (a correction) on one kind of task (extraction).

The paper's claim is wider than that. Areev agents are supposed to improve
**facts, procedures, tools, events and skills** — every grain type the agent
runs on — under the same governance, and then hand what they learned to a
small model. This experiment measures the wide claim on two public
benchmarks that were built to measure exactly it, and adds the tuning leg
on top of each:

> Does an agent whose memory is governed — proposed, reviewed, applied,
> measured, reverted — improve more across sessions than the same agent with
> a plain memory, or none; does the improvement follow the intended pathway
> (save → retrieve → update) rather than a shortcut; and can a 1.7B model
> tuned on that governed memory carry the improvement with a fraction of
> the context, on hardware that costs a fraction as much?

Three benchmarks were named for this. Two fit; one does not, and §4 says why.

## 2. The benchmarks

### PAST-Bench (arXiv 2608.04003, Aug 2026, Apache-2.0)

[github.com/Gen-Verse/PAST-Bench](https://github.com/Gen-Verse/PAST-Bench).
Built to attribute cross-session improvement to retained experience: 26
task families, 204 episodes, four capabilities, every family run twice
under matched conditions — persistence **on** (`w/-evolve`) and **denied**
(`w/o-evolve`) — with the volatile context wiped between episodes so any
gain must have travelled through the persistent substrate. Episodes have
roles: *cold* (first contact), *learn/update* (deposit the target state),
*evaluation* (a fresh session, new surface, trigger wording removed) and
*control* (shortcut, surface-memorisation, stale-reuse and wrong-substrate
checks that cap what a gain may be credited as).

| capability | families / episodes | what it tests | the Areev grain it lands on |
|---|---|---|---|
| Memory | 5 / 41 | a declarative clause is retained and applied later, unprompted | **Fact** (`fact` / `lesson` proposals) |
| Procedural reuse | 8 / 64 | a multi-step workflow is re-executed in order, with the right tools | **Workflow** + **Skill** grains; `plan_revision`; `skill_stall` |
| Information gathering | 6 / 48 | the agent consults what it stored *before* answering, not a planted default | recall / `ASSEMBLE`; `query_revision`; `coverage_gap` |
| Update | 7 / 51 | a second authoritative write overrides the first without the old state leaking — corrections, rule migration, temporary-exception expiry, SOP patching | **supersession**; `valid_to` + `staleness`; `contradiction_sweep`; `outcome_review` |

Two published metrics, both reported here unchanged: the **self-evolution
gap** Δ = score(on) − score(off), per family, macro-averaged per
capability, credited only when it clears the family's control episodes; and
the **mechanism-evidence score** — did the gain use the intended pathway,
judged from saved artifacts and runtime telemetry (writes, reads, skill
patches, artifact diffs). Grading is an LLM judge, MiniMax-M2.7 by
default. Published Hermes Δ across seven models: +0.13 to +0.24;
Hermes+ (five interventions) on MiniMax-M2.7: Δ +0.15, mechanism 0.73.

The four capabilities are the four things the paper claims Areev governs,
and the benchmark already controls for the objection a reviewer would
raise first (surface memorisation). That is why it is the primary track.

### HorizonBench (arXiv 2604.17283, Apr 2026, Apache-2.0)

[github.com/stellalisy/HorizonBench](https://github.com/stellalisy/HorizonBench),
data at `stellalisy/HorizonBench` on Hugging Face. 360 simulated users,
each with a six-month history of ~4,300 turns (~163K tokens), generated
from a mental-state graph that records **the provenance of every preference
change**. 4,245 five-option questions, 2,135 of them about a preference
that *evolved* after a life event, 2,110 about one that stayed put. The
pre-evolution value is always among the options as the hard negative.
Standard protocol: the entire history in context. Best of 25 frontier
models: 52.8% overall; **28.1% on evolved items against 56.4% on static** —
models retrieve the old preference and fail to integrate the event that
changed it. The toolkit provides `parse_conversations()` for retrieval
methods and an OpenAI-compatible path for local models.

This is [`DRIFT.md`](DRIFT.md)'s question at scale — a convention that is
*replaced*, where improving means retracting — measured on a corpus with
ground-truth provenance, which is what supersession exists to hold. It is
also the cleanest possible test of the context-window claim: 163K tokens
of history against a few thousand tokens of assembled memory.

### EdgeBench (arXiv 2607.05155, Jul 2026, CC-BY-4.0 tasks) — not run

[github.com/ByteDance-Seed/EdgeBench](https://github.com/ByteDance-Seed/EdgeBench).
134 tasks, 51 released; each is **12+ hours of continuous agent operation**
in a two-container sandbox with graded feedback, and the finding is that
performance follows a log-sigmoid of interaction time (R² 0.998). The
README states that one 12-hour task with a frontier model costs "hundreds
to over a thousand USD" and a full run is "a five-figure spend".

Two reasons not to run it, either sufficient. It measures *within-task*
iteration — one agent, one task, hours of resubmission — not what carries
across sessions; a governed memory has nothing to do there that a scratch
file does not. And a bounded pilot (two Knowledge-category tasks, two-hour
checkpoints, the 30B agent) would cost tens of dollars to produce a number
about which nothing in this paper is claimed. What EdgeBench contributes is
its **analysis**: the log-sigmoid fit of learning against interaction.
§7 applies that fit to every learning curve this programme produces —
documents on the receipts corpora, episodes here, months of history on
HorizonBench — and reports the fit quality rather than assuming a plateau.

For the record, the reference agent's own evaluation tracks (TBLite,
YC-Bench, Terminal-Bench, all through Atropos) measure coding and
long-horizon operation with no persistence ablation, so they are not
substitutes either.

## 3. Track A — PAST-Bench

### 3.1 One scaffold, five memories

The design of [`FOURWAY.md`](FOURWAY.md): hold the agent constant and swap
only what carries forward. A single tool-calling agent (`persist/agent.py`,
the harness's JSON-on-stdio contract, `qwen3-30b-a3b-instruct-2507`
pinned, temperature 0, seeded) runs every episode under PAST-Bench's own
runner, sandbox and graders. The arms:

| arm | what carries forward between episodes | who decides what is kept |
|---|---|---|
| **none** | nothing — the benchmark's `w/o-evolve` condition | — |
| **mem0** | what mem0 extracts from each session, retrieved by similarity | mem0's extractor (three modes as in FOURWAY: installed, domain hint, raw) |
| **Areev passive** | every turn, tool call and artifact as grains; a budgeted `ASSEMBLE` at session start; **no loop** | the agent's own writes |
| **Areev governed** | passive **plus** a loop pass at every episode close: DISCOVER → GROUND → VERIFY, a rubric reviewer approves or refuses with a BECAUSE, apply; the outcome gate measures the next evaluation episode and reverts a regression | the loop, then a reviewer |
| **reference agent** | PAST-Bench's shipped Hermes adapter, unmodified, on the same model and judge | its own files and skills |

A sixth, if the vendored Hermes version accepts it: **reference agent +
Areev provider** — the `MemoryProvider` plugin in `examples/hermes/`
(verified against Hermes 0.16.0), which isolates the substrate from the
scaffold in the other direction.

The governed arm is where the wide claim is tested, and the grain mapping
is not decorative — each capability exercises a different proposal kind and
a different analyzer (table in §2). Two places the engine is expected to
fall short are named now so they are findings, not surprises: a
`plan_revision` can edit conditions, cycle bounds and retries but not
topology, so an SOP patch that adds a step must be proposed as a `lesson`
and applied by the harness as a `SUPERSEDE workflow` on approval; and a
temporary exception's expiry relies on the proposer setting `valid_to`,
which no earlier run required. The receipts programme found six engine
defects this way; the ledger records any found here the same way.

### 3.2 What is measured

- **Δ and mechanism score**, exactly as PAST-Bench defines and computes
  them, per family, per capability, overall — for every arm. Its
  `sequence_comparison.json` is the artifact; nothing is recomputed by
  hand.
- **Paired contrasts across arms** on the same episodes: governed vs
  passive (what the loop adds), governed vs mem0, governed vs none, each by
  paired exact test on episode scores (McNemar on the judge's pass/fail
  where it is binary, a paired sign test on the [0,1] score otherwise), and
  the reference agent beside them. Three runs per arm; the within-run
  pairing is the claim, cross-run only reported.
- **The governance ledger**: proposals per kind, approved / refused /
  applied / reverted, with the reviewer's BECAUSE, per capability. The
  Update capability's ledger is the one to read: it is where a refusal or a
  revert is the *correct* outcome.
- **Noise floor**: two identical `none` runs (A0R) per model, reported
  before any Δ is interpreted; an effect inside the floor is not an effect.
- **Prompt tokens per episode** per arm: the whole-file paste the reference
  agent does, the budgeted assembly Areev does, nothing for `none`.
- **Cost**: every model call metered into `usage.jsonl` (the harness
  adapters do; mem0's SDK calls are wrapped as in `mem0_arm.py`; PAST-Bench's
  judge calls are metered from its trace) and priced by `cost.py`'s pinned
  table — $ per 100 episodes, split read / learn / judge.

### 3.3 The tuning leg

After a governed run, the memory of each family's learn episodes is a
corpus (`slm_corpus.py` generalised: system = the day-one instruction, user
= the learn episode's task, assistant = the trajectory that succeeded,
with the approved artifacts as they stood). `areev tune --cmd` hands it to
a CUDA trainer (§8) and registers the adapter; `areev eval run` grades it
against the pinned evalset; the loop's `adapter_intake` proposes it and a
reviewer promotes it — the product path, end to end, which the receipts
harness could not exercise.

Two splits, mirroring seen / unseen vendors in [`FOURWAY.md`](FOURWAY.md):

| split | trained on | evaluated on | reads as |
|---|---|---|---|
| **seen families** | learn episodes of all 26 families | their evaluation episodes | memory distilled into weights for one deployment |
| **unseen families** | learn episodes of 20 families | the 6 held-out families' evaluation episodes | did it learn *how to use what it learned*, or memorise 20 families |

The tuned 1.7B is evaluated as the agent three ways: with **no persistence
read at all** (the memory is in the weights; the benchmark's `w/o-evolve`
condition for a model that was tuned), with a 1K-token assembly, and with
the full assembly the 30B gets. The untuned 1.7B with the full assembly is
the control that separates "a small model with the memory in its prompt"
from "a small model tuned on it". PAST-Bench's own control episodes are
scored for the tuned model exactly as for every arm — the surface-
memorisation and stale-reuse caps apply to a model that memorised, too.

### 3.4 What a pilot must establish before the real run

Per-episode token cost on this model is not published; the pilot is one
family per capability, both conditions, `none` and `governed` only, fully
metered. It fixes the budget (§10), confirms the runner accepts an
external agent adapter (the README documents none; the fallback is to
drive our agent against the family files and call the graders as a
library), and reveals the vendored Hermes version.

## 4. Track B — HorizonBench

### 4.1 Protocol

The history is a stream. For each user it is ingested **in date order** as
Events, and the governed arm runs a loop pass every simulated week (~26 per
user): DISCOVER proposes preference **Facts** under a relation the model
names, citing the turns; `contradiction_sweep` and DISCOVER together flag a
live value a later event contradicts; the reviewer (a rubric judge that
sees the cited turns and **never** the mental-state graph) approves the
supersession or refuses it with a BECAUSE. At question time the agent gets
an `ASSEMBLE` of at most 4K tokens — the live preference facts for the
question's domain with their supersession chains, plus the most recent
turns — and answers the five-option question.

| arm | context at question time | tokens (est.) |
|---|---|---|
| **full context** | the whole history, the paper's protocol, our model | ~163K |
| **RAG** | `parse_conversations()` chunks, top-k by embedding | ≤4K |
| **mem0** | turns `add()`ed in order, `search()` at question time | ≤4K |
| **Areev passive** | every turn a grain, recall by domain, no loop, no supersession | ≤4K |
| **Areev governed** | approved preference facts with supersession chains + recent turns | ≤4K |

Scored as the benchmark scores: accuracy overall, on **evolved** and on
**static** items (the Evo–Static gap), and the rate at which a wrong answer
is the **pre-evolution distractor** — the belief-update failure the paper
diagnoses, and the number supersession exists to move.

### 4.2 The tuning leg, and the context-window claim

Users are the entity: **300 train / 60 held-out**, the same split rule as
registrants in CURVE and vendors in FOURWAY, so no held-out user's history
was ever in a corpus. Two corpora, pre-registered with different roles:

- **memory-distilled** (the headline for the tune claim): windows of a
  training user's turns → the governed facts as they stood after that
  window, supersessions included. No question, no option letter — the
  model learns the *update step* from what the loop and reviewer produced.
- **label-supervised** (the upper bound, disclosed as such): the training
  users' benchmark items with their assembled 4K context → the correct
  letter. The benchmark ships no train split; carving one from its users
  is legitimate transfer and is stated as what it is.

Evaluated on the 60 held-out users: tuned 1.7B at 4K, tuned 1.7B at 1K,
untuned 1.7B at 4K (control), the 30B at 4K, the 30B at 163K. The
context-window claim is then a table of accuracy against prompt tokens,
and the capital claim is which of those rows runs on an 8 GB card.

## 5. Overfitting — the record, not a hope

Every control the running CURVE benchmark keeps, kept here, plus the two
the benchmarks add:

| control | where |
|---|---|
| entity split — held-out **families** / **users**, never just held-out items | §3.3, §4.2 |
| validation-selected checkpoint, iterations scaled to corpus size, full loss curve in the manifest | trainer (§8) |
| seen vs unseen, paired separately; the difference between halves is memorisation | `slm_overfit.py`, generalised |
| memorisation probe: exact-string recall of training rows from the tuned model | new, `persist/overfit.py` |
| A0 cross-arm drift check: all arms ignorant at cold — a gap there is provider drift | §3.2 |
| PAST-Bench control episodes (shortcut, surface, stale, wrong substrate) scored for every arm including the tuned one | §3.3 |
| HorizonBench distractor rate as the stale-belief diagnostic | §4.1 |
| `OVERFIT.json` beside every result, as in `results/fourway-2026-09-05/` | §9 |

## 6. Cost and capital — what is captured

Everything comes from meters; nothing from a card statement or a guess.

| metric | source |
|---|---|
| $ per 100 episodes / per 1,000 questions — **read**, **learn** (loop + review), **judge**, per arm | `usage.jsonl` → `cost.py`, pinned prices |
| prompt tokens per episode / question, per arm | the same ledger |
| tuned-model tokens at zero marginal **and** at a shadow hosted small-model rate | `cost.py`'s two readings |
| GPU-hours and peak VRAM per adapter; wall time per training run | trainer manifest (`nvidia-smi` sampled during the run, never after) |
| local inference latency and throughput of the 1.7B on the RTX 4060 | `slm_serve.py` meter |
| hardware class each row *requires* — the 1.7B on an 8 GB consumer card; the 30B-A3B on a ≥24 GB card quantised or ≥60 GB in bf16 — with dated list prices | stated, labelled as list prices |
| electricity | **not measured** — no meter on the box; said so rather than estimated |

## 7. The learning curve, fitted

EdgeBench's contribution, borrowed: for every curve this programme has —
CURVE's checkpoints at 20/40/80/160/320 documents, DocILE's to 1,280, the
learn-episode sequences here, the weekly passes on HorizonBench — fit the
log-sigmoid of score against interaction and report the fit (R²) beside
the plateau. Then the combined report can say whether governed learning
has *one* curve shape across corpora, or whether it does not, which is
also a result.

## 8. Infrastructure — the office box

`swe@192.168.1.2`: Ubuntu 24.04, 16 cores / 32 threads, 31 GB RAM, RTX
4060 **8 GB**, 671 GB free, driver 595, Docker 29, Python 3.12, no CUDA
toolkit, no Rust, no torch. Currently idle. Nothing is installed yet; this
is the list.

- **Rust**: rustup stable → `areev` binary and `areev-py` (maturin) from the
  `bench/persist` worktree, synced by `rsync` (never a push).
- **`~/mg/local/persist-venv`** (3.12): torch cu12x wheels (no nvcc needed),
  transformers, peft, trl, vllm (serves the 1.7B base + LoRA behind an
  OpenAI-compatible endpoint; `slm_serve.py` already speaks that), datasets,
  mem0ai, openai; `ollama` for mem0's embedder (`mxbai-embed-large`, as
  `mem0_arm.py` pins).
- **PAST-Bench** in its own uv-managed 3.11 venv per its README, sandbox
  image built; Hermes and Hermes+ as vendored.
- **Trainer**: `persist/slm_train_cuda.sh` — the CUDA twin of
  `slm_train.sh`: `Qwen/Qwen3-1.7B` in bf16, LoRA rank/layers as on the
  laptop, `max_seq 3072` with gradient checkpointing (fits 8 GB at batch 1–2),
  epochs scaled to rows, validation every 25 steps, lowest-val checkpoint
  kept, loss curve in the manifest. The laptop trains
  `mlx-community/Qwen3-1.7B-4bit`; same family, different precision — a
  disclosed difference in the combined table, not a hidden one.
- **GPU is serialised**: training and vLLM serving never overlap
  (`gpu_yield.sh`'s rule); PAST-Bench sandboxes and mem0 use the CPU.
- **Runs live in `~/mg/local/areev-runs/persist/` on the box**, under
  `tmux`; only counts, manifests and checksums travel back into
  `crates/areev-bench/results/persist-<date>/`, as every result here does.
- **Keys**: not on the box yet — §11.

## 9. Order of work and gates

1. **Box setup** (½ day). Toolchains, PAST-Bench installed and its sample
   family running on the reference agent, HorizonBench `--config sample`
   running, keyless dry-run of our agent with the mock judge — the
   keyless floor every harness here has.
2. **PAST-Bench pilot** (1 day, metered): one family per capability,
   `none` + `governed`, both conditions. Output: $ per episode, wall time
   per episode, adapter viability, Hermes version. **Gate**: the
   extrapolated full budget is under the cap in §10, or the arms are cut and
   the cut is written here.
3. **HorizonBench pilot** (½ day): 10 users end to end through every arm.
   Same outputs.
4. **Pre-registration commit**: this file amended with the meters, the
   stated interpretations (§12) untouched, before any full run.
5. **Full runs**: PAST-Bench five (or six) arms × 3 runs; HorizonBench five
   arms × the 60 held-out users, full-context arm once (it is
   deterministic at temperature 0 and costs the most).
6. **Tuning legs**: adapters per §3.3 and §4.2, `OVERFIT.json`, memorisation
   probes, cost manifests.
7. **Stats and verify**: every published number recomputed from raw trials
   by `persist/verify.py --check`, checksums in a `MANIFEST.md`.
8. **Combined report** — after the laptop's CURVE and DocILE runs land —
   `crates/areev-bench/PROGRAM.md`: one table of every corpus × (governed
   effect, mem0, passive, tuned before/after, seen/unseen, prompt tokens,
   $/1k, GPU-hours, hardware class); the grain-coverage matrix (which grain
   type each corpus exercised, and the ledger counts per kind); the fitted
   curves; the governance ledgers; the engine defects found. Charts by a
   script from the JSON, never by hand.

Nothing in 5–8 starts before 4 is committed. The worktree is
`bench/persist`; nothing is pushed or merged without approval.

## 10. Budget — estimates, to be replaced by the pilots

| item | estimate | basis |
|---|---|---|
| PAST-Bench, per arm, 3 runs | ~$5 agent + ~$5 judge | 1,224 episode-runs × ~40K tokens at $0.048/M; judge ~10K/episode at $0.30/M — **per-episode tokens are a guess until step 2** |
| HorizonBench full-context arm, once | ~$35 | 4,245 × 163K prompt tokens at $0.048/M |
| HorizonBench governed ingestion, 360 users | ~$50 | ~9,400 weekly loop passes; DISCOVER on the 30B, GROUND on gpt-4o-mini, review on gpt-4o dominates |
| HorizonBench mem0 ingestion | ~$60–100 | ~775K `add()` calls with extraction on the 30B |
| tuning | $0 marginal | the box; GPU-hours reported |
| **total** | **under $500** | cap proposed in §11 |

## 11. Open questions (asked in chat; answers amend this file)

1. "Horizon" — HorizonBench (evolving preferences) is assumed; confirm it
   is not Long-Horizon-Terminal-Bench.
2. EdgeBench — accept §2's recommendation (analysis borrowed, no run), or
   fund a bounded pilot that will not bear on the claim.
3. Budget cap for this track.
4. Judge — MiniMax-M2.7 via OpenRouter ($0.30/$1.20 per M, one key) so
   the reference-agent numbers are comparable to the paper's, assumed.
5. Keys on the box — a separate OpenRouter key with its own spend limit is
   recommended over copying `dev-areev.env`.
6. SLM base — `Qwen/Qwen3-1.7B` bf16 on CUDA beside the laptop's MLX
   4-bit, difference disclosed; or move both to one base.

## 12. Stated in advance

Written before any run; the pilots may amend the budget, never these.

- **Governed > passive > none on Δ, with mechanism scores rising in the
  same order, and the Update ledger showing refusals or reverts:** the
  headline — the loop, not the store, is what improves the agent, and it
  improves the four capabilities through four different grain types.
- **Governed ≈ passive:** the store carries the benefit and governance
  costs the same for no gain here; published as that. The ledger still
  says what governance *refused*, which is a safety result, not a
  performance one.
- **Governed < passive:** approved artefacts trade breadth for precision
  (the earlier arm result) replicates on a new workload — strengthens
  rather than retracts it; the per-capability table says where.
- **Areev arms < reference agent:** the scaffold, not the substrate, is
  the difference; the provider arm (reference agent + Areev), if it runs,
  isolates that. Published either way.
- **Tuned 1.7B with no persistence read ≥ 30B with full assembly on
  seen families, and above the untuned control on unseen:** the memory
  moved into the weights and the model learned to use it. On unseen
  families only the second half is claimed.
- **Tuned 1.7B ≤ untuned control:** the corpus is too small at 204
  episodes; published as a bound on the method at this scale, with the
  loss curves.
- **HorizonBench: governed at 4K closes the Evo–Static gap relative to
  passive and RAG at 4K, and the distractor rate falls:** supersession
  does what it is for. **The 30B at 163K stays ahead:** a fair reading
  that full context is still better at this size, and the cost table says
  what the difference buys.
- **Any arm inside the A0R noise floor:** not interpreted.
