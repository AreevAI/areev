# Areev Loop Reflection Engine — governed, verified, measured self-improvement

**Status**: the **verifier (§5) and measurement (§6) are implemented** on
the engine feature branch (since merged to main) — the
`DISCOVER → GROUND → VERIFY → ROUTE` pipeline (proposer ≠
scorer), the abstention-legitimate objective, the confidence floor, the
Effective-Reliability eval (`loop_reflection` bench: the verifier lifts ER
from +0.00 to +1.00 on the reference corpus by filtering decoys), and the live
approval-rate metric (`areev loop`). The **out-of-box multi-provider layer (§9)
is now built too**: the `areev-llm` crate (OpenAI-compatible + Anthropic +
Ollama adapters over `ureq`) + `areev loop run --model provider:name`, key from
the environment — `--llm-cmd` remains the escape hatch. This proposal supersedes
the "optional LLM enrichment" framing in `loop.md` / proposal §9.

---

## 0. TL;DR

The current `--llm-cmd` layer is safe but unproven: an LLM proposes memory
findings, we check that it cited a real evidence hash, and a human reviews the
rest. That guarantees the LLM can't *fabricate* a finding — it does **not**
guarantee the finding is *good*, and we can't measure whether it is. An LLM told
"propose findings" will over-generate (say something because asked), and nothing
downstream stops a grounded-but-useless draft from reaching the queue.

This document proposes turning that layer into a **reflection engine** built on
the one thing the entire self-improvement literature agrees on:

> **Self-improvement is reliable exactly when an external verifier grades the
> change, and degrades — often below baseline — when a model judges its own
> correctness.**

Areev Loop already does the hard, verifier-correct half: **deterministic analyzers
find the problems** (contradictions, duplicates, staleness, cold grains, coverage
gaps) — the "error-finding" that LLMs provably *cannot* do reliably. The LLM
should only do the "error-fixing/enrichment," and every LLM proposal should pass
an **independent verifier** before it ever reaches the review queue, with the
whole loop's precision **measured on a labeled eval**. That is what makes it a
product, and what makes "it self-improves" a claim rather than a vibe.

---

## 1. Why the current `--llm-cmd` layer isn't enough (honest self-assessment)

What we shipped (proposal §9): `ANALYZE → DISCOVER → ENRICH → VALIDATE+DEDUP →
STORE`, with the LLM able only to *add* `origin=llm` drafts (never auto-apply)
and whitelisted guidance. The guardrails are real and correct — but they are all
**safety**, none are **quality**:

| Guardrail (built) | What it does | What it does *not* do |
|---|---|---|
| Must cite a real evidence hash | Prevents citing a nonexistent grain | Prove the grain *supports* the claim |
| `origin=llm` ⇒ never auto-applies | Bounds blast radius | Judge whether the draft is worth the reviewer's time |
| Advisory / human-reviewed | A person decides | Filter obvious/wrong drafts *before* the human |
| Fail-soft | A bad backend can't break a run | Detect a bad *finding* |

Three concrete gaps:

1. **Over-generation is structural, not a prompt bug.** A model asked to
   "propose additional findings" is in the binary-grading regime where guessing
   strictly dominates abstaining (Kalai et al., *Why LMs Hallucinate*, 2025). It
   will propose *something* even when there's nothing to say.
2. **"Cite a real hash" is the weak half of grounding.** It proves the hash
   exists; it does not check entailment. A model can cite a real grain and attach
   an obvious, wrong, or out-of-context interpretation.
3. **It's unmeasured.** The deterministic analyzers have a 1.00 precision
   fixture; the LLM path has *no number at all*. We cannot claim quality we don't
   measure.

The design below closes all three without giving up the safety.

---

## 2. The organizing principle

Two findings from the research anchor the whole design.

**(a) Verifier-anchored improvement compounds; self-judging degrades.** Across
Voyager (skills checked by an execution environment), STaR/ReST-EM (self-training
filtered by ground truth), DeepSeek-R1 (rule-based rewards *chosen over* a learned
reward model precisely to avoid reward hacking), and the process-verifier line
(Lightman et al., *Let's Verify Step by Step*), improvement is reliable *because*
an external signal grades it. Where the model grades itself, it Goodharts: DGM
faked its own test logs and deleted the safety marker it was told to keep; a
self-critiquing planner had an **84% false-positive rate** as its own verifier
(Valmeekam et al.); intrinsic self-correction of reasoning is flat-to-negative
(Huang et al., ICLR'24: GPT-4 GSM8K **95.5 → 89.0** after self-review).

**(b) The bottleneck is error-*finding*, not error-*fixing*.** LLMs cannot
reliably find their own errors, but *can* fix an error once it is localized
externally (Tyen et al.; Huang et al.). This is the load-bearing result for us,
because it maps one-to-one onto Areev Loop's existing split:

```
FIND   ← deterministic analyzers            (the part LLMs can't do — already built, precise)
FIX    ← LLM proposes, localized to a find  (the part LLMs can do — under a verifier)
VERIFY ← external check over the evidence    (the part that makes it trustworthy — this proposal)
```

We are not pivoting the architecture. We are completing the loop it already
implies. **Lead with this in any external claim** — it is a genuine, defensible
design advantage, not marketing.

---

## 3. What we're beating

**"Hermes"** = **Hermes Agent (Nous Research)**: an open-source, self-hosted
agent whose pitch is a built-in learning loop over persistent memory — it
"creates skills from experience," edits procedural skill files, and consolidates
episodic memory in a background post-turn review. It is *agent-system* learning
(memory/skills), not weight updates — the same class as Areev Loop.

The wedge is specific and verified from its own docs: **`memory.write_approval`
and `skills.write_approval` both default to `false`** ("write freely — the gate
is off"). There is an open feature request (#19324) asking for an
approval-before-write policy, and community reports of drift (a loop that
"learned to add, commit, and push to a git repo on its own"). **Hermes is the
ungoverned, unmeasured version of what Areev Loop does.** (Its headline popularity
numbers are not credible and are excluded from any comparison — we cite its
*design*, not "dominance.")

Where the rest of the field sits, and what we take from each:

| System | Mechanism | The lesson for us |
|---|---|---|
| **Hermes Agent** | memory + skill synthesis, gate off | **Beat**: same loop, but governed + verified + measured. |
| **Reflexion / Self-Refine** | verbal self-critique | **Avoid**: fragile without an external oracle; degrades reasoning. |
| **ACE** (Agentic Context Engineering) | evolving playbook via *incremental deltas*, deterministic curator | **Adopt**: never let the LLM monolithically rewrite memory — context collapse (their case: 18k tokens @ 66.7% → one rewrite → 122 tokens @ 57.1%). Areev immutability gives us this for free. |
| **GEPA** | reflective prompt evolution, Pareto-frontier selection | **Adopt**: keep a Pareto set of competing fixes, not the greedy best. Also validates the whole strategy — reflective, non-weight-update improvement beat RL at 35× fewer rollouts. |
| **Voyager** | skill library of *executable, self-verified* code | **Adopt & scope**: synthesize a skill only if it's executably testable; otherwise it stays a proposal, not memory. |
| **Mem0** | LLM picks ADD/UPDATE/**DELETE**/NOOP | **Beat**: destructive deletes silently lose info; its own table shows full-context beats it on accuracy. We supersede, never delete. |
| **Zep / Graphiti** | bi-temporal edge invalidation (expire, don't delete) | **Already are**: Areev's immutable grains + supersession *is* the non-lossy model. Add the governed improvement loop on top. |
| **SEAL** | self-generated finetuning data (real weight update) | **Avoid**: admits its own catastrophic forgetting. Keep learning in the append+supersede memory layer, which can't forget. |

The through-line: a self-improving memory layer is trustworthy **only** when
(a) detection is deterministic, (b) LLM edits are *proposals*, (c) a real
external verifier grades each proposal, (d) edits are non-lossy supersessions
with an immutable audit trail, and (e) irreversible ops require a human gate.
That is already Areev Loop's shape — this proposal adds (c) and the measurement.

---

## 4. Architecture: the reflection pipeline

```
FIND      deterministic analyzers + telemetry signals     (built)
  → PROPOSE   LLM, structured output, abstention-legitimate objective   (§5.1)
  → GROUND    entailment: does the cited evidence support the claim?     (§5.2)
  → VERIFY    independent model, factored, over the evidence — anti-Goodhart  (§5.3)
  → SCORE     calibrated confidence → route (queue / triage / drop)      (§5.4)
  → GOVERN    review / audit / undo                        (built)
  → MEASURE   Effective Reliability on a labeled eval + live approval-rate  (§6)
```

Every stage can emit **reject** or **NOTHING TO REPORT**. The pipeline's job is
to hand the human/policy reviewer a *short, high-precision* list — the opposite
of a firehose of grounded-but-trivial drafts.

The verifier is a **pre-queue filter**: a `origin=llm` draft is only ever
*stored as a pending recommendation* if it survives GROUND + VERIFY. This is a
change from today, where every cited draft is stored.

---

## 5. The verifier (this pass builds §5.1–§5.4)

### 5.1 Stage 0 — the abstention-legitimate objective (biggest leverage, ~no code)

Replace "propose additional findings" with a **bar-raising, negatively-marked**
instruction that makes "nothing" a first-class, zero-penalty answer:

> *"Propose a finding only if you are more than **t** confident it is both
> **correct** and **materially useful**. A wrong or trivial finding is penalized
> **t/(1−t)** points; a correct, useful finding earns 1; **'NOTHING TO REPORT'
> earns 0**. When in doubt, return NOTHING TO REPORT."*

Set **t = 0.75** (penalty ×2) to start; expose it as a param. This ports
exam-style negative marking and is the direct, training-free antidote to
over-generation (Kalai et al., 2025 — scoring rule verified against the paper).
Pair with a hard cap on drafts per run. TruthfulQA's separate truthful/informative
scoring is the same principle: abstention must never be scored as a failure.

### 5.2 Stage 1 — the grounding gate (upgrade of "cite a real hash")

Decompose-then-entail, at claim granularity, against the *actual* cited grains:

1. **Decompose** the proposed finding into atomic claims (FActScore-style).
2. **Entail** each load-bearing claim against its cited evidence grain(s) **at
   sentence granularity**. Reject the finding if any load-bearing claim is not
   `supported`. (SummaC's lesson: whole-finding-vs-whole-evidence NLI scores
   ~56% — near-useless; sentence-level ~74%.)
3. **Contradiction findings get a purpose-built primitive.** "These two grains
   hide a semantic contradiction" is *literally an NLI `contradiction` check
   between the two cited grains* — far more reliable than a free-form LLM
   assertion, and a perfect fit for content-addressed evidence.

Implementation options, cheapest first (decide at build time):
- **(a) LLM-as-entailer** in a constrained yes/no mode, one call per (claim,
  evidence) pair — no new dependency, rides the same `--llm-cmd`/provider path.
- **(b) A small local checker** (MiniCheck-class, ~770M, GPT-4-level grounding
  at ~400× lower cost) behind a cargo feature, for teams that want the LLM out
  of the grounding loop.

Honest ceiling: even GPT-4 hits only ~75% balanced accuracy on hard grounding —
treat Stage 1 as a strong *filter*, not an oracle, and validate on our own
claim/evidence lengths (short-claim accuracy does not transfer to long-form).

### 5.3 Stage 2 — independent verification (the anti-Goodhart core)

A **separate** model call (never the proposer grading itself) runs **factored
chain-of-verification**: from the draft, generate independent verification
questions — *Is this novel vs. already-implied by the deterministic finding? Is
the contradiction real, or merely temporal/underspecified? Does the evidence say
this out of context?* — **answer each in an isolated context** so the model can't
re-read and rubber-stamp its draft, then keep-or-kill. The *factored* separation
is what makes CoVe work; the joint (same-context) variant barely helps
(Dhuliawala et al.: FactScore 55.9 → 71.4 factored).

Two hard rules from the failure data:
- **The verifier is structurally outside the proposer's edit surface.** Same
  model *weights* are fine; the same *call/context proposing and scoring* is not.
  Penalizing detectable gaming is insufficient (DGM disabled its own detector) —
  separation must be structural.
- **If a judge is used, give it the gold evidence.** Ungrounded LLM-judges
  collapse to near-chance (κ 0.14–0.21) exactly on hard cases; grounded, they
  recover (κ ≈ 0.67).

### 5.4 Stage 3 — calibrated confidence → routing

Elicit **verbalized** confidence (0–100%), not token logprobs (RLHF degrades
logprob calibration ~10×; verbalized recovers >50% of ECE but is still
overconfident, so **calibrate post-hoc (Platt/isotonic) on our own labels**).
Combine into one score — **grounding-entailment × verbalized-confidence ×
novelty** — and route by threshold (selective prediction): high → the review
queue; low → dropped silently; middle → optional cheap human triage. The
operating point is tuned on a risk–coverage curve (§6).

### 5.5 Stage 4 — governance (already built)

Unchanged: `origin=llm` never auto-applies; review requires a BECAUSE; every
transition is a hash-chained audit grain; apply is undoable or marked
non-rollbackable up front. The verifier makes Stage 4 see a short, high-precision
list — it does not replace the human gate.

---

## 6. Measurement — what makes it claimable

You **cannot inherit benchmark numbers.** Mem0 and Zep publicly scored the *same
system* on LoCoMo at **58 / 66 / 75 / 84** depending on who held the pen; in both
vendors' own tables a plain full-context baseline is competitive on accuracy. No
existing benchmark measures "was this proposed memory *insight* correct." So we
build our own, two ways:

**(a) An LLM-path eval fixture** (the analog of `loop_precision` for the
deterministic analyzers). A labeled corpus: N scenarios with **planted semantic
issues** the reflection engine *should* surface (a real hidden contradiction, a
stale assumption) and N **decoys** it must not (a superficially-similar but
legitimate pair). Each proposed finding is human-labeled
`{useful-correct / correct-trivial / wrong / harmful}` + which evidence actually
supports it. Headline metric:

> **Effective Reliability = (useful-correct − wrong) / total**, with NOTHING
> scoring 0.

ER *subtracts* for confident-wrong — the metric shape that punishes
over-generation, unlike raw precision. Report ER **plus** precision, recall, and
the **spurious-finding (false-positive) rate** at the chosen operating point, and
a **risk–coverage curve** (precision as a function of how many findings we
surface — this *is* the "how good is it" chart, and it picks the threshold).

**(b) A live approval-rate metric** off the existing audit chain: what fraction
of `origin=llm` drafts a reviewer *approves* vs *rejects* over real use. This is
a genuine field-quality signal that accrues per file and needs no new
infrastructure — the audit grains already record every decision.

Discipline (matches our existing "never headline LoCoMo 54%" rule): we report
**Effective Reliability + abstention rate + approval-rate**, validate any
LLM-judge against human labels via **Cohen's κ** (target ≥ 0.8; note the ceiling
is human–human agreement), and we **never headline a single vendor benchmark
number.** If we cite an external set, prefer **LongMemEval**'s knowledge-update
and abstention categories over the saturated/gameable LoCoMo.

**Live validation (honest finding).** With the **non-parasitic evidence bundle**
(the DISCOVER bundle is seeded from deterministic citations *and then topped up
with recent grains since the last-run watermark*), a real model (`gpt-4o-mini`
via OpenRouter) now discriminates correctly on hand-built corpora:

- **Hidden cross-fact issue → surfaced.** Seed `acme headquarters "San Francisco"`
  + `acme country "Germany"` — a geographic inconsistency **no deterministic
  analyzer flags** (each fact is individually well-formed). The model proposes it,
  it grounds and verifies, and it reaches the queue as one `loop.llm/1` rec.
- **Consistent corpus → abstains.** Seed `acme headquarters "San Francisco"` +
  `country "United States"` + `industry "software"` — the model proposes a vague
  "potential inconsistency," the verifier **rejects it** under the abstention
  rule, and the run stores **0**.

So the claim upgrades from "verified and abstaining" to **"finds grounded issues
determinism misses, and abstains when there are none"** — validated end-to-end
with a real model, not just the mock. Reaching that took three pipeline fixes
found by live-tracing every model call (a logging `--llm-cmd` wrapper):

1. **GROUND checks premises, not the conclusion.** A semantic-inconsistency
   finding *infers* beyond its evidence (the grains state HQ=SF and country=DE;
   "these conflict" is world-knowledge, not in the text). Strict decompose-then-
   entail rejected every such finding. GROUND now verifies the finding's factual
   *premises* are present (anti-fabrication) and lets the inference through — the
   inference's soundness is VERIFY's job, not GROUND's.
2. **VERIFY is soundness + abstention, never novelty.** Told to check "is this
   novel vs the deterministic findings," a weak verifier hallucinated "merely
   restates a deterministic finding already known" — with an *empty* deterministic
   list — and killed the genuine finding. VERIFY now judges only reality (reject
   vague/`potential` claims and inconsistencies without ≥2 conflicting facts) and
   context; novelty is steered at DISCOVER and settled by human review.
3. **Novelty is not an entity-level dedup.** An entity can carry a contradiction
   *and* a staleness *and* an llm-found semantic issue at once, so dropping an llm
   finding because a built-in touched the same target is too coarse. The dedup_key
   (family+target+action) already keeps distinct-family findings apart.

The parasitic-bundle limitation (§11) is thereby **closed for the single-file
path**; the remaining open item is the corpus-scale ER number (a labeled
non-parasitic corpus), tracked below.

---

## 7. How it maps onto Areev / Areev Loop primitives

Almost every piece rides existing structure — this is why the fit is good:

- **Evidence hashes → grounding.** Findings already carry content-addressed
  evidence hashes; the grounding gate reads those exact grains (stable, immutable
  — the same address always fetches the same bytes).
- **`origin=llm` → the verifier's subject.** The stamp already exists and already
  blocks auto-apply; the verifier is a new *pre-store* filter on those drafts.
- **Recommendation + audit grains → the approval-rate metric.** The hash-chained
  audit already records approve/reject with actor + reason; the metric is a read
  over it.
- **Immutable grains + supersession → non-lossy by construction.** We get ACE's
  "delta not rewrite" and Zep's "expire not delete" for free — the LLM proposes
  *supersessions*, never in-place edits; the only destructive op (FORGET) stays
  behind the human `admin` gate.
- **The deterministic `loop_precision` harness → the LLM eval fixture.** Same
  labeled-corpus pattern, new labels and the ER metric.

New surface required (small): a verifier module in `areev-loop` (objective, decompose,
entail, factored-verify, calibrate), a pre-queue hook in the engine's DISCOVER
path, an `origin=llm` eval fixture + ER scorer in `areev-bench`, and an
approval-rate reader over the audit chain.

---

## 8. The claim we can defend

> **The only agent-memory self-improvement layer that is governed, verified, and
> measured.** Deterministic analyzers do the error-finding LLMs provably can't;
> the LLM only *proposes* fixes; every proposal is grounded against
> content-addressed evidence and checked by an *independent* verifier before it
> reaches review; nothing applies without an audit trail and undo; and the loop's
> precision is measured on a labeled eval. Not "an LLM edits your memory and
> hopes."

It beats **Hermes** (write-gate off, no verifier, no eval), **Mem0** (destructive
and unmeasured), and **Reflexion/Self-Refine** (self-critique, fragile without an
oracle) on the exact axis the evidence says determines reliability: **an external
verifier outside the model's edit surface.**

**Honest caveats we ship with the claim** (they are a feature, not a hedge):
- The grounding checker is right ~3 in 4 on hard cases — a strong filter, not an
  oracle. We route by calibrated confidence and keep the human as the final gate.
- Numbers are measured on *our* eval; we do not inherit or headline vendor
  benchmarks.
- The LLM is *bounded* here, not *trusted*: it cannot fabricate (grounding), can't
  self-approve (independent verifier), can't auto-apply (`origin=llm`), and can't
  destroy (supersede-only + human FORGET gate).

---

## 9. Deferred (designed, not built this pass): out-of-box providers

To be a product users run with zero setup, the reflection engine needs
first-class model access beyond a hand-written `--llm-cmd` script. The design
(from the integration research) — **built later, not now**:

- A small **sync `LlmClient` trait** + hand-rolled `ureq` adapters: one
  **OpenAI-compatible** adapter reaches ~90% of providers (OpenAI, Gemini's
  compat endpoint, Groq, DeepSeek, OpenRouter, vLLM, LM Studio, llama.cpp), plus
  native **Anthropic** and **Ollama**. `--llm-cmd` stays as the zero-dep floor.
- **Provider-native, grammar-constrained JSON** (guaranteed-valid output), with
  schemas authored to the cross-provider intersection (flat, all-required,
  enums; no numeric/string constraints).
- **Env-var-first config** (`ANTHROPIC_API_KEY`, `OPENAI_API_KEY`, `OLLAMA_HOST`,
  `OPENAI_BASE_URL`) so it lights up with zero flags; `--model provider:name` to
  choose; a small built-in model registry with a `--base-url` escape valve.
- **Batch API + prompt caching** for the async reflection job: a full-memory pass
  ≈ **$0.15–$1.50**; incremental nightly runs sub-cent. Behind a cargo feature so
  the core store/CAL stay dependency-light.

This pass keeps the verifier + measurement on the existing `--llm-cmd` path (the
verifier just makes *more* model calls through the same seam), so the quality
claim is proven before we invest in the provider breadth.

---

## 10. Build plan (this pass = verifier + measurement)

1. **Objective reframe (§5.1)** — the abstention-legitimate DISCOVER prompt +
   `min_confidence` param + drafts cap. Cheapest, highest-leverage.
2. **Grounding gate (§5.2)** — decompose-then-entail; the NLI-between-grains
   primitive for contradiction findings; option (a) LLM-entailer first.
3. **Independent verifier (§5.3)** — factored CoVe as a separate call; the
   structural proposer≠scorer rule; pre-queue filter in the engine.
4. **Calibrated routing (§5.4)** — verbalized confidence, combined score,
   threshold routing; params exposed.
5. **Eval fixture + ER metric (§6a)** — `origin=llm` labeled corpus in
   `areev-bench`; Effective Reliability + precision/recall/spurious-rate + a
   risk–coverage report; a CI floor once we have baseline numbers.
6. **Approval-rate metric (§6b)** — a reader over the audit chain, surfaced in
   `areev loop` + the console Sessions/Setup views.
7. **Docs + honest positioning** — fold the claim + caveats into `loop.md`;
   this doc is the design of record.

Then, as a separate pass: the out-of-box provider layer (§9).

---

## 11. Open questions / risks

- **Verifier cost.** Each finding now costs several model calls (propose +
  decompose + N entailments + factored verify). Acceptable because the loop is
  async/batchy (the explicit constraint), and batch + caching keep a full pass
  in the ~$1 range — but we should measure calls-per-surfaced-finding and cap it.
- **Grounding-checker choice.** LLM-entailer (no dep, but the LLM is in the loop)
  vs. a small local model (MiniCheck-class, a dep behind a feature). We ship the
  LLM-entailer and now expose `--ground-model` / `--ground-cmd` so a team can
  point GROUND at a *different* backend — a cheaper model, or a local
  NLI/entailment command that takes the generative model out of grounding
  entirely — without changing the proposer or verifier.
- **Eval labeling cost.** ER needs human labels. Start with a small planted
  fixture (like `loop_precision`), grow it from real approve/reject decisions
  (the approval-rate data doubles as label seed).
- **Calibration drift across models.** A threshold calibrated on model X won't
  transfer to model Y; calibration is per-(model, task). Recalibrate when the
  configured model changes; the eval fixture makes this a one-command check.
- **Does it actually help downstream?** ER measures finding-correctness, not
  outcome. The honest proof is an A/B on agent task-success with vs. without
  accepted findings — a later, harder measurement we should name but not
  over-claim before we have it.

---

## Appendix — source ledger (primary)

**Verifier-anchored improvement / self-judging fails:** Let's Verify Step by Step
(Lightman, ICLR'24) arxiv.org/abs/2305.20050 · LLMs Cannot Self-Correct Reasoning
Yet (Huang, ICLR'24) arxiv.org/abs/2310.01798 · LLMs cannot find their reasoning
errors (Tyen) arxiv.org/abs/2311.08516 · Self-critiquing plans, 84% FP
(Valmeekam) arxiv.org/abs/2310.08118 · DeepSeek-R1 rule-based rewards
nature.com/articles/s41586-025-09422-z · DGM (reward-hacking cautionary)
arxiv.org/abs/2505.22954

**Objective / abstention:** Why LMs Hallucinate (Kalai, 2025)
arxiv.org/abs/2509.04664 · TruthfulQA arxiv.org/abs/2109.07958 · R-Tuning
arxiv.org/abs/2311.09677 · abstention survey arxiv.org/abs/2407.18418

**Grounding / verification:** Chain-of-Verification (Dhuliawala, ACL'24)
arxiv.org/abs/2309.11495 · FActScore arxiv.org/abs/2305.14251 · SummaC
arxiv.org/abs/2111.09525 · MiniCheck arxiv.org/abs/2404.10774 · No Free Labels
(grounded-judge κ) arxiv.org/abs/2503.05061

**Calibration:** Just Ask for Calibration (Tian, EMNLP'23)
arxiv.org/abs/2305.14975

**Competitive / memory:** Hermes Agent github.com/nousresearch/hermes-agent +
docs (write_approval defaults) · ACE arxiv.org/abs/2510.04618 · GEPA
arxiv.org/abs/2507.19457 · Voyager arxiv.org/abs/2305.16291 · Reflexion
arxiv.org/abs/2303.11366 · Mem0 arxiv.org/abs/2504.19413 · Zep/Graphiti
arxiv.org/abs/2501.13956 · SEAL arxiv.org/abs/2506.10943 · LongMemEval (ICLR'25)
arxiv.org/abs/2410.10813 · the LoCoMo dispute
blog.getzep.com/lies-damn-lies-statistics-is-mem0-really-sota-in-agent-memory/

**Eval / judges:** MT-Bench / LLM-as-judge (Zheng, NeurIPS'23)
arxiv.org/abs/2306.05685 · judge reliability (independent)
arxiv.org/abs/2408.09235 · LoCoMo arxiv.org/abs/2402.17753

*Caveat: Hermes Agent and some 2025–26 items postdate the Jan-2026 knowledge
cutoff; design details were verified from primary repos/docs, popularity claims
excluded. Every quantified result is dataset-specific — the recurring lesson is
to validate on our own workload and labels.*
