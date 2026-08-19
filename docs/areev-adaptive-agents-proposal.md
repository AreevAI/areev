# Areev for adaptive agents — the runtime, the governance plane, and the distillation path

**Status:** proposal, 2026-08-11. **Partially built:** the runtime and the
governance plane shipped (`areev-run-core`/`areev-run`, the loop's gates and
recommendation lifecycle). The distillation / small-language-model path in §5
onward remains unbuilt. Supersedes and absorbs
[`areev-run-proposal.md`](areev-run-proposal.md) (2026-08-07), whose §4 replay
recommendation survives and whose §2 cost estimate does not. Sits alongside
[`areev-enterprise-proposal.md`](areev-enterprise-proposal.md), whose adoption
ladder and RBAC→TLS→SSO ordering this document keeps unchanged.

Written after a six-track audit of the substrate, the governance layer, the
captured-data corpus, the 2026 SLM-tuning literature, and the existing
strategy documents. Where this document contradicts an earlier one, the
contradiction is called out by name.

---

## 0. The three questions, answered up front

**"Does 'Hermes for enterprise — adaptive agents with governance' make sense?"**
Yes, and it is already the documented position: `areev-enterprise-proposal.md`
opens with *"Areev is Hermes for enterprise."* The premise holds and the audit
made it stronger — Hermes describes *itself* as a "single-tenant personal agent,"
ships `write_approval: false` on both memory **and** skills, and scores zero on
every row of the enterprise checklist. The `MemoryProvider` slot is genuinely
open, needs no PR to Nous, and `prefetch()` really does sit on the synchronous
turn path **with no timeout** — seven of the eight bundled memory providers are
network round-trips there.

**But two corrections.** That document's adoption ladder puts the revenue at
**rung 3 (the enterprise plane), not rung 2 (the runtime)** — still right. What
is now wrong is the **RBAC → TLS → SSO ordering inside rung 3**: those rows were
filled by the market during 2026 (§7). The row nobody has filled is the durable,
verifiable record of what the agent did and on whose authority. **The evidence
plane should be promoted ahead of RBAC, TLS and SSO.** That is a change to a
decision already on file, and it is the single most actionable finding here.

**"Should we allow integrations with SLM tuning?"**
Yes — but not in the form stated, and the difference is the whole proposal.
"We capture more data, so let's use that data to tune models" is contradicted
by the best measurement available. The version the evidence supports is
narrower, harder for a competitor to copy, and is the one position in this
market that nobody occupies.

**"So the models get better and we can reduce the context window."**
Half of this is real and half is a trap. The token-reduction half is measured
and unreliable as a value claim; the model-gets-better half is real but only
under conditions Areev does not meet today. Both are §4.

**The revised thesis, one sentence:**

> Areev should not become a trainer. It should become the **system of record
> for how an agent got better** — the governed corpus that tuning consumes, the
> replay harness that decides whether a tuned model is actually an improvement,
> and the lineage that lets an erasure reach the next model instead of stopping
> at the export.

---

## 1. What the audit changed

Six findings reshaped the plan. Each is a correction to something a current
document asserts.

**1.1 — `areev run` is not "mostly assembly."** The old proposal's §2 table has a
real symbol behind every cell, but five of them are inert. `ExecutorKind`,
`ExecutionStatus`, `FailureCause`, `correlation_id`, `async_mode`,
`expires_at_sec`, `transient_definition_hash` and `actor_execution_environment`
have **no writer and no reader anywhere in the workspace** — they serialize,
round-trip in tests, and are otherwise dead. They are imported scaffolding from
a different system ("HPL Phase 4.1", "Flow-A", a `/triggers/tool-result/callback`
sink that does not exist here). They are a naming convention to honour, not
design work already banked. The claim is accurate about *persistence* and wrong
about *execution*: nothing walks a workflow graph, nothing evaluates a `cond`,
nothing resolves a tool binding, and the three-phase
`definition`/`call`/`result` lifecycle the doc describes does not exist — there
are two kinds (`Definition`, `Execution`) and one grain carries both call and
result.

**1.2 — `areev-llm` cannot do a tool-call round trip, and that is a rewrite.**
`LlmBackend` is `complete(&str) -> String`. The request body is pinned to
exactly two messages, there is no `tools` parameter, no message history, and
`extract` reads `/choices/0/message/content` only — so a native OpenAI tool call
arrives as `content: null` plus `tool_calls[...]` and is **silently discarded**.
`response_format` is forced to JSON. An agent loop needs a messages array in and
structured `tool_calls` out; that is a new trait and three new adapters, not a
Phase-3 wiring job. Budget it as a build.

**1.3 — The Phase-1 determinism gate was measuring the wrong thing, and the real
hazard is worse than the doc says.** Byte-identical replay *is* achievable —
verified empirically: a Tool grain with an explicit `created_at` re-adds to the
identical hash, and re-adding a stored grain is a free no-op that consumes no
sequence number and emits no op-log row. But `created_at` defaults to
`Utc::now()` inside `areev-core`'s serializer and feeds both the payload and
the header, so the clock is not something you must avoid *calling* — it is the
**default on every single write**, and it silently wins unless the runtime
overrides it every time. Meanwhile the gate as written ("a resumed run produces
byte-identical journal grains") passes vacuously under idempotence, or passes
because the runtime re-executed the effects and got lucky. Under a genuine
Temporal model a resumed run produces **no journal grains at all** for the
replayed prefix. The gate that actually pins the claim is effect suppression
(a counting mock executor sees zero calls), decision equivalence (same ordered
node sequence, same accumulated state), and journal immutability (op-log length
unchanged).

**1.4 — Areev is a memory-state store, not a trajectory store.** This is the
finding that reshapes the tuning question. The `Event` grain already models
everything a training example needs — `content_blocks` with typed
`ToolUse`/`ToolResult` parts, `model_id`, `stop_reason`, `token_usage`,
`parent_message_id`, `run_id`. **Every one of those fields has zero producers
outside tests.** The single write path, `Areev::capture`, sets four fields:
namespace, session_id, role, run_id. `capture-stop` keeps only the *last* user
and *last* assistant message per session, flattened to one string with tool
calls truncated at 500 chars and results at 800. `record_tool_call` — the only
shipped tool-recording API on any binding — **has no `input` parameter**, so
tool-call arguments are unreachable from every surface. `bind_tool` does not
exist, so the tool catalogue cannot be populated at all outside Rust. And reads
leave no trace by design.

**1.5 — The assembled context is never persisted, and that one is architectural.**
`ASSEMBLE` classifies as `Read`, returns an in-memory payload, and writes
nothing. `areev-context` has no dependency on `areev-store` and is structurally
incapable of persisting. `FormattedContext` carries no hash and no
`included_hashes`. Repo-wide greps for `context_hash`, `included_hashes`,
`prompt_hash` return zero. The entire durable side effect of an assembly is one
boolean incrementing a global counter. **You cannot distill "the model behaved
well given context C" when C is unrecoverable the moment the call returns.**

**1.6 — There is no reward signal attached to any trajectory.** The loop's audit
chain is the best label in the system — every approve/reject carries a mandatory
`BECAUSE`, a named actor, and a hash link to the previous record. But its unit is
"should this memory edit be made?", not "was this run good?", and **no run,
session, or thread id appears in `Recommendation`, `AuditRecord`,
`OutcomeResult`, or `AppliedRecord`.** No analyzer reads Events at all. Outcome
measurement knows exactly two metrics, its baseline is a hardcoded `0.0` planted
at proposal time rather than a pre-apply measurement, its verdict vocabulary is
`held | regressed` with no `improved`, and `Proposal::Data` — the shape every
LLM-originated finding and every revert takes — **applies as a no-op**.
`revert_of` has zero consumers in the repo. The Verify gate measures and queues
a human prompt; nothing is ever automatically rolled back.

---

## 2. The evidence on tuning from agent data

This section is the load-bearing one, because it inverts the intuition.

### 2.1 The measurement that decides it

Microsoft's Online Experiential Learning work (arXiv 2603.16856, Mar/Jun 2026)
ran the exact ablation this proposal turns on — consolidating agent experience
into weights, raw versus extracted:

| Sokoban, Qwen3-4B-Instruct | In context | Consolidated into weights |
|---|---|---|
| No experience | 7.5% | — |
| **Raw trajectories** | 10.9% | **7.8%** |
| **Extracted knowledge** | 18.2% | **21.4%** |

Raw traces into weights is worth **nothing** — 7.8% against a 7.5% baseline.
Extracted knowledge into weights is worth **2.9×**. The authors' stated critical
condition is on-policy consistency between the knowledge source and the policy
model. *(Caveat, stated plainly: 3×3 and 6×6 grid games, small scale, no
comparison against alternative online-learning methods. It is the most direct
evidence available, not a settled result.)*

Corroboration from the opposite direction: Agent Memory Distillation
(arXiv 2608.07169, 7 Aug 2026) gets +27.2 pp on AppWorld and +11.2 pp on BFCL V3
for 4B–8B students using hierarchical teacher memory — and it is **training-free**.
Structured memory injected at inference delivers most of the win before any
weight moves.

**The consequence for us is direct.** "We capture more data, so let's train on
it" describes the arm that measured zero. An engine that already extracts,
deduplicates, supersedes and tombstones *structured* knowledge from raw agent
activity is producing the artifact that trains well. The raw trace is the
artifact that doesn't — which is fortunate, because §1.4 says we don't have the
raw trace either.

So the pitch is not *your traces become your tuned model*. It is:

> **Your memory becomes your tuned model.**

### 2.2 The context-reduction claim, honestly

The "tune it so you need less context" half is real, measured, and a trap as a
value claim.

**Real:** tool-schema internalization on Gemma 4 E4B and Qwen3-4B with ~1,700
examples cut input length **82.6%** while *raising* tool-F1 from 0.47 to 0.65
(arXiv 2605.17774). A deployed NL2SQL system at Dream11 went from 17,000 tokens
to under 100 — **>99%** — at 98.4% execution accuracy, beating Gemini 2.0 Flash
(IAAI 2026 Deployed Applications track).

**Three reasons not to lead with it:**

1. **Token cuts are not cost cuts.** The most rigorous study in the area
   (arXiv 2607.12161 — 2,908 provider-billed Claude Code runs, 103 tasks,
   hash-frozen and pre-specified) removed **38% of raw tool-output tokens and
   increased end-to-end billed cost by 6.8%**, 95% CI +2.8% to +11.3%.
   Per-task token reduction correlated with cost saving at r = 0.15.
   **Prompt-cache traffic was ~87% of reconstructed cost.** Aggressive
   compression also dropped patch application from 27/40 to 15/40.
2. **The tool-schema problem was solved at the protocol layer without touching
   weights.** Anthropic moved Tool Search and Programmatic Tool Calling to GA in
   February 2026: 85% reduction in tool-definition tokens, one code-execution
   pattern going 150,000 → 2,000, and tool-selection accuracy rising 49% → 74%.
   Progressive disclosure beats internalization on every axis except raw latency
   and needs no retraining.
3. **Mixing tuned-in context with in-context memory is a documented failure
   regime.** Reintroducing privileged context to a distilled student causes harm
   rates up to **15.7%** and accuracy drops up to 9.5 pp (arXiv 2606.11627). An
   agent that *sometimes* has memory in context and *sometimes* leans on the
   adapter is living in exactly that regime.

**Lead instead with latency, egress, sovereignty, offline operation, and
success-adjusted cost.** Those are true, they are what an embedded engine is
already for, and they do not require a claim the literature will falsify.

### 2.3 Volumes, so the plan is scoped to reality

The one real scaling curve for agent trajectories (SWE-Dev, arXiv 2506.07636):
574 trajectories → 13.0%; 16,639 → 22.8%; log-linear, no saturation. The
practical knee is **2,000–5,000 curated successes**. RL needs far fewer *tasks*
than SFT needs *trajectories* (MUA-RL reached competitive τ²-bench scores from
165 tasks), which is why rank-1 LoRA suffices there.

Two numbers to hold onto. **Don't train on failures raw**: naive mixing of
resolved and unresolved runs scored 28.5%, *worse* than resolved-only at 30.9%;
step-level masking of the harmful steps reached 32.2%, and a manual audit found
**only up to 24% of steps in a failed run are actually harmful** (JetBrains,
2026-06-16). And **the local loop is genuinely small**: ~500 examples produce a
working LoRA adapter in 8–16 minutes on a free 16 GB T4 under 8 GB VRAM, with
adapters weighing 8–84 MB. vLLM and SGLang hot-swap them at runtime.

Against Areev's existing corpus, the honest reading is that we are far below
every one of these thresholds — which matches `loop-proposal.md` §17's own
pre-committed gate of ~50–100 labeled decisions per analyzer, and its warning
that building ahead of it "would be learning theater on n=5."

### 2.4 The market gap

Verified across the landscape: **nobody owns "agent memory + tuning."** Letta
declines weight training on the record (context is "an appreciating asset",
weights "a depreciating asset") while conceding distillation of token-space
memories as an unbuilt roadmap item. Mem0 has no training path at all.
Databricks shipped both an agent-memory service on Lakebase and a trainer and
has **not wired them together**. Rubrik acquired Predibase, kept the governance,
and deleted the tuning product. The two genuine both-ends players — Inference.net
AutoTrainer and Distil Labs — are per-task distillers whose own stated scope is
"narrow, high-volume, recurring," explicitly not open-ended agents.

Four gaps follow, in order of relevance to us:

1. **Nobody treats the memory store as both the durable asset and the training
   corpus.** Trace stores are telemetry with retention windows; memory stores are
   retrieval indexes with no training path.
2. **The reward bridge is missing.** Every eval platform accumulates human review
   scores, judge outputs and thumbs, and **none can emit a reward function.**
3. **Nobody documents what happens to a dataset row or a trained adapter when the
   source trace is deleted for GDPR.** Not one platform.
4. **Everything is a hosted control plane.** Every credible capture→tune player
   must see your traffic. **No embedded or local option exists.**

**The memory layer itself is the unoccupied ground, and a survey of nineteen
enterprise agent platforms confirms it.** Across AWS AgentCore, Google Memory
Bank, Microsoft Foundry Memory, Anthropic memory stores, Glean, LangGraph Store,
Salesforce, Snowflake, Databricks, Sierra, Cohere and OpenAI:

> **Memory is being built as a *retrieval* feature, not a *records* feature.
> Nobody documents subject-level erasure, DSAR export, or content-addressed
> immutability in their memory layer.**

The closest anyone comes: Anthropic ships immutable per-mutation versions with
actor attribution and a `redact` endpoint explicitly for "leaked secrets, PII,
or user deletion requests" — the one design converging on ours, and still public
beta. AWS is the only vendor enforcing memory scoping in IAM. The best retention
policies anyone publishes are Copilot Studio's 28-day inactivity delete and
Glean's 30-day extracted-memory TTL — both **time-based, not subject-based**.
Temporal, the durable-execution primitive, has **no memory or retention
semantics at all**; its payloads are opaque blobs.

Two commercial signals worth noting. Memory is becoming a **billed dimension** —
Glean prices agent runs partly on "how much memory it maintains," and Vercel,
Cloudflare and Google (from Sep 2026) all meter memory storage directly. And
Anthropic prints the warning nobody else does: memory attaches read-write by
default, so *"a successful prompt injection could write malicious content into
the store. Later sessions then read that content as trusted memory."* That is an
argument for supersession-with-provenance over mutable stores, and it is ours to
make.

### Two claims that must be narrowed before someone narrows them for us

**"The agent runtime whose execution history *is* queryable memory" is no longer
unoccupied.** `areev-run-proposal.md` §1 asserted the niche was empty as of the
August 2026 survey. It is now **partially occupied**, and by a GA product:

- **AWS AgentCore Episodic Memory (GA).** Stores whole chains of tool calls,
  decisions and outcomes; decomposes each episode into
  situation/intent/assessment/justification/reflection; reflections span episodes
  ("which tool combinations consistently lead to successful outcomes"); and the
  agent retrieves them **at inference time** via `retrieve_exemplars` and
  `retrieve_reflections`. Benchmarked: **τ²-bench retail +11.4% Pass¹** (77.2% vs
  65.8%). AWS even warns about cross-actor privacy within one memory resource.
- **Anthropic Dreams (research preview)** takes a memory store plus past session
  transcripts and emits a **new, separate output store** — *"the input store is
  never modified."* That is supersession semantics, arrived at independently, in a
  shipping lab product.

Concede both and pivot to what they are not. The dimensions that remain open:
**raw and lossless rather than LLM-summarized · content-addressed and
tamper-evident · queryable by a language rather than by embedding · erasable by
subject · one substrate serving both audit and recall · embedded, with no server
on the recall path.** The closest published match to that combination is a
**solo-author preprint** (arXiv 2605.11032, May 2026 — content-addressable entries
in a Merkle-DAG provenance graph), not a product.

**"Governance in the memory substrate" is no longer an empty phrase, because Zep
is already saying it.** Their homepage sells a "Context Lake" of *"governed
context graphs"* with the tagline **"authorization, retention, and audit live in
the substrate, not bolted on"** — backed by GA ABAC and RBAC, audit logs, SOC 2
Type II, HIPAA BAA, BYOK via KMS and BYOC into your VPC.

That is our sentence, from a funded competitor, in production. **The wedge is
where they are silent:** Zep's governance documentation describes **no retention
policy, no legal hold, no deletion or erasure procedure, and no DSAR flow** —
despite the homepage claiming legal hold. Article 17 erasure that reaches
replicas, and a DSAR sharing **one selector** with that erasure, remain unclaimed
by the category leader. Position there specifically, and stop using "governed
memory" as though it were differentiating on its own.

There is also a dated wedge. OpenAI stops accepting new fine-tuning jobs
**2027-01-06** and kills Evals **2026-11-30**; Azure stored completions retire
**2026-10-15 with no export path** ("can't be exported or migrated"); Databricks
FMFT was removed **2026-08-14**; Anthropic has no first-party tuning at all.
Teams that captured their agent history inside a vendor's log product are about
to lose it. Teams that captured it as durable, portable, provenance-bearing
memory keep it. That sentence is true today and stops being novel within a year.

### 2.5 Why the compliance angle is the moat, not the garnish

The survey literature already argues Areev's case in Areev's language.
arXiv 2603.07670 §4.6 on parametric memory: *"Hard to audit… hard to delete from
(machine unlearning is still immature), and expensive to update… For these
reasons, most deployed agents favor non-parametric, inspectable stores."*

And the erasure problem is genuinely unsolved by everyone else:

- Post-hoc unlearning **suppresses rather than removes**. Unlearned models retain
  21% of "forgotten" knowledge at full precision and **83% after 4-bit
  quantization** (arXiv 2410.16454, ICLR 2025; extended May 2026). The artefact
  you certify and the artefact your customer deploys can differ in exactly the
  property you certified.
- A 2026 audit of 10 unlearning methods across 6 datasets found de-optimization
  methods fail outright and Fisher/Hessian methods fail **despite formal
  certifications**.
- **CNIL's default remedy for Article 17 against a model is retraining**, not
  unlearning — and erasure must be communicated to downstream model recipients.
- **The only construction in the literature producing provable deletion**
  (arXiv 2508.12220) is a deterministic training program over an **append-only,
  per-record training ledger** with logged ordering, seeds, and step counters.

**One timing caution that changes the emphasis, not the argument.** Regulation
(EU) 2026/1744 — the Digital Omnibus on AI, in force 27 July 2026 — **pushed
Annex III standalone high-risk from 2 Aug 2026 to 2 Dec 2027**, and Annex I
embedded to 2 Aug 2028. The deadline pressure this proposal would naturally lean
on is sixteen months further out than it looks. Article 50 transparency and
**Article 12 logging are untouched**, GPAI enforcement switched on 2 Aug 2026, and
Colorado SB 26-189 (three-year record retention, adverse-outcome disclosure within
30 days) is effective now. So the compliance argument survives — but **lead with
recall quality and blast-radius control, and use compliance as the closer**, not
the opener.

That last bullet is the point. **Areev is already that ledger.** Content
addressing, immutability, the op-log, `REPORT SUBJECT` and erasure sharing one
selector, and the fingerprint-not-identity audit rule are the exact primitives
that construction requires. The property that makes Areev awkward for
"delete from the weights" is the property that makes lawful **re-derivation**
possible — and re-derivation is what the regulator actually asks for.

**But it only holds if the corpus stays inside the lineage.** A JSONL export is
not a replica: it never replays the op-log, so tombstones never reach it, and
nothing in Areev today records that an export happened or where it went. There
is no equivalent of Article 19's duty to communicate an erasure to each
recipient. A corpus also dissolves the very selectors erasure depends on — it
flattens indexed triples into prose, which `docs/gdpr.md` already warns is
neither reportable nor erasable. **The export is a data-controller boundary
crossing, and it is the thing this proposal must govern rather than merely
enable.**

---

## 3. The collision with a public claim, and how to resolve it

`docs/loop-explainer.md:416` — a row in the competitor table used in the pitch:

| SEAL | self-generated finetuning | weight updates admit catastrophic forgetting; learn in the memory layer |

`loop-reflection.md:134` is harder: *"**Avoid**: admits its own catastrophic
forgetting. Keep learning in the append+supersede memory layer, which can't
forget."* And the adjacent GEPA row cites reflective non-weight improvement as
validating the whole strategy.

This is a public, argued position that weight-level learning is the *wrong
mechanism*, used as a differentiator. It is also **correct, and recently
re-confirmed**: Thinking Machines measured naive midtraining on 100% internal
documents dropping IF-eval from **85% to 45%** (79% at a 70/30 mix), recovered
to 83% only via on-policy distillation.

**The resolution is not to reverse it. It is to keep it and draw the boundary
one layer out.**

- Areev still learns in the memory layer. That claim stands unmodified.
- What is new is that the memory layer's *output* — extracted, governed,
  provenance-bearing knowledge — is the corpus a **host** may tune with, outside
  the engine, exactly as extraction and embeddings are host-supplied today.
- Areev never trains, never ships a trainer, and takes no training dependency.
  It emits a governed corpus, and it **grades the result**.

This keeps every existing public promise intact:

| Promise | Status under this proposal |
|---|---|
| "It takes no LLM dependency: it does not run models for you" (`FAQ.md:369`) | Unchanged. A training job needs a process, a credential and a GPU — by CAL's own boundary rule, *"if it needs a filesystem path, a credential, or a process to exist, it's a host verb."* |
| "Areev Loop improves the agent's memory, not its outputs" | Unchanged. The tuning path is explicitly a host activity that Areev supplies data to and measures. |
| The SEAL row | Unchanged, and now better supported by 2026 evidence. Add one clause: *and when you do tune, the corpus and the evaluation had better be governed.* |
| "Substrate not framework" | Unchanged, and reinforced — we are the substrate for someone else's trainer. |
| "no unbenched accuracy number" | Binding. Nothing in §5 permits a capability claim before the harness in Phase B exists. |

**The one thing that must change on the record** is the framing around
`loop-explainer.md` §14, which currently reads as "weight updates are bad." The
honest 2026 form is: *weight updates are the last step after context and harness
are exhausted, they demand an evaluation harness to be safe, and the reason we
can offer one is that our memory is immutable and content-addressed.* That is a
strengthening of the existing argument, not a retreat from it.

---

## 4. The unification: replay is the keystone

Three separate documents already converge on the same next capability, and none
of them noticed the others:

- `loop-proposal.md` §17 names **counterfactual replay** the flagship of its
  escalation ladder — *"explore in the past, not in production"* — and calls it
  "unique to content-addressed immutable memory."
- `loop-explainer.md` §16 roadmap item 1 is **pre-apply replay validation** —
  *"will this change make my agent worse?"*
- `areev-run-proposal.md` §4 chose **deterministic replay (model B)** as the
  runtime's differentiator.

Add the fourth, from this audit: **replay is the only honest way to make a
capability claim about a tuned model**, and the repo's own claim discipline
(`loop-proposal.md` §18, `loop-reflection.md` §278–283 — Effective Reliability,
abstention rate, Cohen's κ ≥ 0.8 against human labels, LongMemEval over LoCoMo,
no invented precision) requires one before anything can be said out loud.

So the plan does **not** start with the runtime, and it does **not** start with
an exporter. It starts with replay, because replay is simultaneously the loop's
off-policy evaluator, the runtime's determinism gate, and the tuning path's
promotion gate. One capability, three payoffs, no public claim reversed.

---

## 5. The revised plan

Five phases. A and B are the spine and are independently valuable even if the
tuning direction is never pursued. C is the differentiated artifact. D and E are
optional on top and should not be started before B's gate is green.

### Phase A — make the trajectory real

The corpus audit's encouraging finding: four of the five gaps are plumbing into
write paths that already exist, and only one needs a design decision.

| Work | Shape | Note |
|---|---|---|
| `input` parameter on `record_tool_call` | Additive across facade + CLI + MCP + both bindings | Tool-call arguments are currently unreachable from every surface |
| `bind_tool` writing `ToolKind::Definition` | New; needs a `json_build.rs` arm so it is reachable outside Rust | Without a tool catalogue there is no "which tools could the model have called" |
| `capture-stop` walks the **whole** transcript into per-turn Events | Replaces the last-two-messages `HashMap`; populates `content_blocks`, `model_id`, `stop_reason`, `token_usage`, `parent_message_id` | Every field already serializes and round-trips; none has a producer |
| `run_id` on `RecallEvent`; `related_to` reachable from JSON | Small | Telemetry is currently unjoinable to any trajectory |
| **The run manifest** | New grain, existing carrier | See below |
| **The assembly manifest** | **A design decision, not a patch** | See below |

**The run manifest — what machine consumed the context.** Grep confirms **zero
sampling parameters exist anywhere in the grain types**: no `temperature`,
`top_p`, `top_k`, `seed` or `max_tokens`. `model_id` (`mdl`), `observer_model`
(`omdl`) and `TokenUsage` exist and have no producer. Without this, replay is not
reproducible and every corpus row is mislabeled — you cannot separate "the model
improved" from "someone changed a sampling parameter," which is the exact question
Phase B exists to answer. Reproducibility needs **context *and* harness.**

It also closes a second gap: today a run has **no grain of its own** — `run_id` is
an opaque shared string with no start, end, status or metadata. So write one
manifest grain per run carrying provider, model id and build string, sampling
parameters, seed, tool-catalogue hash, system-prompt hash and harness version.
Because it is content-addressed, identical configurations **collapse to a single
grain** and every run points at it, which turns "every run on config X" into a
query rather than a log grep.

**The model field must be a tuple, not a string:** base model + build, adapter
hash, quantization, serving runtime — pinned as one unit, because an adapter
trained against an NF4 base and served on bf16 or GGUF drifts. That is also what
closes the lineage chain end to end — `subject → grains → corpus export → adapter
→ runs served by it` — and it is what lets an erasure report which adapters are
now stale and which runs they contaminated.

**The assembly manifest is the one real decision.** To record what was in the
model's window you need a durable artifact — the query, the included hashes in
order, a digest of the rendered text, the budget, and what was dropped — written
where the budget boolean is written today. That is a new grain shape, which
under invariant I2 (canonical serialization frozen) is an OMS-spec-level
decision routed through `docs/oms-1.6-amendments.md`, the mechanism that already
exists and was exercised for A1–A3.

**Recommendation: ride existing carriers first.** A manifest can be an
`Observation` in a reserved namespace with the hash list in `context` and
`derived_from` pointing at the assembled grains — zero format change, exactly
the move D2 made for grants. Take the format change only if that proves
insufficient. Note the cost either way: assemblies are frequent, so this needs a
sampling policy and it must not touch the microsecond recall path.

**Phase A gate:** a full turn — system prompt, tool catalogue, ordered messages
with typed content blocks, tool calls with arguments, tool results, final output,
and the assembled context that produced it — round-trips out of a memory file and
back. Cross-surface parity per the `areev-add-operation` playbook.

### Phase B — replay and the evaluation harness

Deterministic, offline, no weights, no new dependency.

1. **Explicit `created_at` on every write from the replaying host**, plus a
   tripwire asserting no grain reaches `add()` with `created_at == None`. This
   is the real hazard (§1.3) and it belongs in a runtime write helper, not in
   the store.
2. **Counterfactual replay** — re-run analyzers and configurations against the
   immutable past. This is `loop-proposal.md` §17 rung 1, already sanctioned.
3. **Session replay** — re-run recorded sessions against a changed backend
   (a memory edit, a prompt edit, later a different model) and score the delta.
   This is the public roadmap item.
4. **The measurement bar, inherited not invented:** Effective Reliability with
   abstention, approval rate, Cohen's κ ≥ 0.8 for any LLM judge validated
   against human labels, LongMemEval over LoCoMo, no invented precision figures,
   and no analyzer or metric default-on without a fixture run.

**Phase B gate:** replaying a recorded session invokes the executor **zero**
times for the replayed prefix, visits the same ordered node sequence, reaches
the same accumulated state, and leaves the op-log length unchanged. In CI, not
as a one-off.

While here, fix two things the governance audit found, because they are cheap
and they are correctness bugs rather than features: `Proposal::Data` and
`Proposal::Edit` currently apply as **no-ops** while flipping status to
`Applied`, and `revert_of` has no consumers — so the Verify gate's regression
path is a queued human prompt wearing the clothes of automated remediation.

### Phase C — the governed corpus

A new host verb, `areev corpus`, that emits a training corpus **and the lineage
that makes it lawful**. This is the differentiated artifact and the thing no
competitor documents.

- **Output format:** OpenAI chat-completions JSONL with a top-level `tools`
  sibling. It has no schema, no version field and no governance body, and its
  author is leaving the business — but it is what every trainer accepts.
- **Emit the fields the ecosystem is missing.** No format anywhere represents
  loss intent portably (six mutually incompatible mechanisms, none round-trips),
  step segmentation, step quality labels, observation elision, or the binding of
  a row to (trace, agent version, policy version, data subject). Areev
  structurally knows all of them. Emitting them is a small spec opportunity —
  ADP is academic with no vendor adopters, OTel GenAI semconv has never shipped a
  versioned release and was removed from the core repo at v1.43.0, and the only
  shipped trace→dataset product (Microsoft Foundry) flattens trajectories to
  question-answer pairs.
- **Step-level masking, not run-level filtering** — the 28.5% vs 32.2% result.
  Supersessions, forks, `verification_status`, tool `is_error` and rejected
  recommendations are the negative signals already present; masking is what makes
  them an asset instead of a liability.
- **An export registry.** Every corpus export writes an immutable record: the
  selector, the grain hashes included, the subject fingerprints touched, the
  destination, and a timestamp. This is what turns an untracked boundary crossing
  into Article 19 machinery.
- **Erasure propagation.** `FORGET SUBJECT` consults the export registry and
  reports which corpora — and therefore which adapters — are now stale. It cannot
  reach into a checkpoint; it can tell you precisely which checkpoints must be
  retired or re-derived. Adopt a stated retrain window analogous to the
  documented `--retain 30d` archive guarantee.
- **A cheap standards-alignment move worth taking now.** OpenTelemetry's GenAI
  semantic conventions moved to their own repo in June 2026 and added **seven
  memory operations** to `gen_ai.operation.name` — `create_memory_store`,
  `create_memory`, `update_memory`, `upsert_memory`, `search_memory`,
  `delete_memory`, `delete_memory_store` — plus `gen_ai.memory.*` attributes.
  Everything is Development status with no doc page, which is exactly when
  emitting them is cheap and being early is defensible. Note the direction of the
  arrow, because it is the whole opportunity: **OTel models memory as a thing that
  emits telemetry. Nobody models telemetry as memory.**
- **The honest claim ladder** (never rung 3 or 4):
  1. ✅ *"We can prove what went in, exclude a subject, and re-derive the model."*
  2. ✅ *"We filter the subject out of outputs and can evidence the filter"* — if
     stated as suppression, not removal.
  3. ❌ *"We ran an unlearning algorithm."*
  4. ❌ *"The subject is gone from the weights."*

Note the sequencing constraint: `areev corpus` needs Phase A's fields to have
anything worth exporting, and Phase B's harness to make any claim about what the
corpus produced.

### Phase D — the runtime, re-justified

`areev run` survives, with two changes to its rationale and a corrected estimate.

**Its old justification** — that the execution/memory seam is unoccupied — was
explicitly caveated in its own text: the niche is unoccupied because the split is
*an annoyance, not a blocker.* That is still true.

**Its new justification is stronger and specific:** it is the only way to produce
on-policy trajectories with real rewards, and on-policy consistency is the stated
critical condition in the one measurement that supports the whole tuning
direction (§2.1). It is also where a run acquires an identity, a budget and an
approval boundary — the things the governance audit found missing.

**Corrected scope**, from §1.1–1.3: the graph walk, the condition evaluator, the
three-tier tool resolver, the `ExecutorKind::Client` envelope, a tool-calling LLM
seam, and deterministic write ordering are all **from scratch**. What genuinely
composes is persistence, the run↔memory reads, ASSEMBLE, and concurrency
(`Arc<AreevFacade>` with the store behind its mutex — no dedicated writer thread
needed, but **write *order* must be canonical graph order, not completion order**,
or replay becomes schedule-dependent; that is the real risk §6.3 was reaching
for, and it is not the one it named).

Keep the architectural rule unchanged: a host over Areev, peer to `areev-mcp`,
nothing under `areev-*` depends on it. Keep the name `areev-run` / `areev run`.

### Phase E — the tuning seam

Host-supplied, opt-in, keys from the environment, exactly the posture
`--llm-cmd` and `--embed-cmd` already established.

- `areev tune --cmd ...` hands a corpus to a host-supplied trainer and takes back
  an adapter reference. Areev never trains and takes no training dependency.
- **Initiating a tune is a host verb; governing it needs no new CAL syntax.**
  CAL's own boundary rule settles the first half — *"if it needs a filesystem
  path, a credential, or a process to exist, it's a host verb"* — and a tune needs
  a GPU, a credential, a trainer process and hours of wall clock. But the loop
  lifecycle already entered CAL in 1.3 and is parity-tested: `RUN LOOP`,
  `APPROVE`, `REJECT`, `APPLY`, `ROLLBACK … BECAUSE` and
  `DESCRIBE LOOP/ANALYZERS/OUTCOMES/POLICY` all classify as `Control` and gate on
  `loop.run`/`loop.review`/`loop.apply`. **So model a tuning candidate as a
  recommendation**, and promotion becomes `APPROVE <hash> BECAUSE "…"` then
  `APPLY <hash>` — inheriting separation of duties, the mandatory written reason,
  the hash-chained audit and multi-horizon outcome measurement for free. Reads
  come free too: `DERIVED FROM` walks adapter → corpus → source grains, and
  `RUNS TOUCHING` finds everything a stale adapter contaminated.
- **Dependency:** this only works once `Proposal::Data`/`Proposal::Edit` stop
  applying as no-ops (Phase B). That fix moves from cheap-cleanup to critical path.
- **The adapter registry is grains.** An adapter record naming the base model,
  the quantization, the corpus export hash, the trainer command, and the Phase-B
  evaluation result. Pin base + adapter + quantization as one unit — an adapter
  trained against an NF4 base and served on bf16 or GGUF drifts, and QLoRA is
  already breaking on 2026 architectures (Unsloth explicitly discourages 4-bit
  QLoRA training for the Qwen3.5 family).
- **Promotion is a gated apply.** A tuned adapter enters service only through the
  same four gates a memory edit passes: evidence-cited, reviewed with a mandatory
  reason, audited, and measured against Phase B. This is the sentence the whole
  proposal exists to earn.
- **Target narrow, high-frequency subtasks first** — routing, extraction,
  classification, single-turn tool calls, and memory operations. BFCL V4's memory
  track shows even frontier models score 53–68% on agentic memory management;
  that is a tunable narrow subtask and it is *our* subtask. Long-horizon agentic
  work is not the beachhead: success on >4-human-hour tasks is below 10% across
  the board.

---

## 6. What we will not do

- **Ship a trainer, or take a training dependency.** Two dependency exceptions
  (rustls, a JWT crate) are already pending and unanswered; a third arriving
  first is precisely the "the next ask will cite these as precedent" risk.
- **Claim erasure from weights.** §2.5's ladder, rungs 3 and 4.
- **Lead with token reduction or context-window savings.** §2.2.
- **Make any capability claim before Phase B's harness exists.** Including
  "self-improving agent" in a sense that implies improved *outputs*.
- **Run anything unattended.** No daemon, no scheduler, no nightly tuning job.
  The established pattern is a cheap idempotent command with watermark gates
  riding the user's hooks, cron or CI.
- **Let measured performance unlock automatic behaviour.** `loop-proposal.md`
  §6.1 rejected "earned autonomy" as reward-hackable and statistically
  meaningless at single-digit n. That objection applies to a tuning loop at
  least as strongly. Autonomy stays an explicit host grant.
- **Let the corpus become self-generated.** Recursive training on synthetic-only
  output collapses and is not recoverable; accumulate real data alongside
  synthetic, never replace it.
- **Become a hosted control plane.** The embedded position is the unoccupied one.

---

## 7. The governance work this exposes

The audit found a set of gaps that an enterprise agent story needs regardless of
the tuning direction. Listed because they are the actual content of "with all
governance in place," and one of them is a live security bug.

**Fix first — a principal holding `loop.run` and nothing else can read every
namespace in the file.** The loop's substrate calls `facade.with_store(...)`,
which bypasses `check_verb` entirely; `RUN LOOP` is gated once, and after that
gate the run reads every namespace except two, quotes their content into
recommendation summaries and evidence, and with an LLM attached ships up to 64
grain briefs to the model. This is a cross-namespace read escalation and it
should be closed before anything else here is built.

**Also open, in rough priority order:**

- **Actor is self-asserted on three of five surfaces.** `areev loop` hardcodes
  `ScopeSet::all()` and takes a free-text `--actor`; `--as` is not honored there
  at all. The server takes `actor` from the request body when no credential map
  is configured. Both bindings hardcode `ObserverType::Human`, so an `agent:`
  actor is stamped human — violating the CAL 1.3 rule that a statement must never
  be able to claim humanity.
- **Ordinary grains carry no actor.** `user_id`, `author_did` and `origin_did`
  are never populated from the session principal by any write path, and
  `author_did` is not queryable. "Which agent recorded this failing tool call" is
  unanswerable.
- **Grants are cached at bind.** A `REVOKE` does not take effect on an
  already-bound session.
- **`GRANT`/`REVOKE` write no Tier-2 audit record**, so `areev audit export` does
  not surface privilege changes beside destructions. For Article 30 that is a gap.
- **Three things the model cannot express**, all of which a runtime needs: "this
  agent may call tool X but not tool Y" (no resource axis below namespace), "this
  run may spend at most N tokens" (no spend accounting anywhere — `areev-llm`
  calls capture no usage), and "writes from this run need approval before they
  are recallable" (no draft state; `verification_status` exists but the recall
  path does not filter on it).

These are also the concrete answer to "what does the enterprise plane sell,"
which `areev-enterprise-proposal.md` correctly identifies as rung 3 and the
revenue story. Nothing in the tuning direction should be started before the first
item is closed.

**Three of these stopped being research topics in 2026, so the bar is now
public.** A landscape survey puts hard numbers on what an enterprise buyer will
compare us against:

- **Non-human identity shipped.** Microsoft **Entra Agent ID went GA in April
  2026** — agent identities with owners and sponsors, Conditional Access
  templates, cascade cleanup, and federation to AWS and GCP. AWS AgentCore
  Identity and Google's Agent Registry are GA; **Snowflake added a first-class
  `SERVICE_AGENT` user type in July 2026**. "Is there a principal for the agent"
  is now a checklist row with five vendors answering yes, and our answer is a
  naming convention inferred from a label prefix.
- **Budgets are enforced, not observed.** AWS's managed harness exposes
  `maxIterations` (default 75), `maxTokens`, `timeoutSeconds`, session lifetime
  caps and truncation strategy — per agent or per invocation. Snowflake ships
  per-agent monthly credit ceilings; Copilot Studio disables agents tenant-wide
  at 125% of prepaid capacity. This is exactly the "at most N tokens" we cannot
  express, and it is table stakes rather than a differentiator.
- **Policy-as-code has a benchmark.** AWS AgentCore **Policy went GA in March
  2026**: policies in Cedar and Dogwood, authored in natural language, with
  automated reasoning that rejects overly permissive or unsatisfiable policies
  *before* enforcement, evaluated at the gateway boundary outside agent code, and
  session-aware ("require that an approval was granted before a transfer").

None of this changes the plan — but it does mean the governance items above are
catch-up on a published bar, not invention, and they should be described that way
internally. What none of those vendors has is the records layer (§2.4), which is
where the differentiation actually lives.

**So re-sequence rung 3.** Agent identity, spend caps and MCP admission control
were all filled in 2026 — integrate with Entra and Okta rather than out-building
them. What every 2026 authority demands and nobody delivers is **a durable,
verifiable record of what the agent did and on whose authority**: simultaneously
an AI Act Art. 12 duty, a GDPR Art. 17/30 duty, a Colorado three-year retention
duty, and a separation-of-duties control. The Five Eyes joint guidance on agentic
AI (Apr 2026) reads like a specification for it in prose — treat each agent as a
distinct identity, minimum access only for the period required, visibility into
tool use and **privilege changes**, and a pre-declared answer to *who owns, who
approves, who monitors, and who may suspend*. And CSA's March 2026 survey found
**more than two-thirds of organisations cannot distinguish AI-agent actions from
human actions.**

That is the evidence plane, and it should go ahead of RBAC, TLS and SSO. A
related gap worth claiming while it is open: **Entra retires an agent's
credential at offboarding; nothing retires its memory.**

### Two corrections to how we describe Hermes

The enterprise proposal's characterization checked out on every material point —
the 2,200/1,375-character caps, replace-loses-history, write-approval off by
default, no erasure or roles or actor identity, `prefetch()` on the synchronous
path. Two claims need softening for accuracy:

- **Say "no *policy* retention," not "no retention story."** Hermes has a session
  pruner (`sessions.retention_days: 90`, `auto_prune: false`). What it lacks is
  subject-scoped or policy-scoped retention.
- **Hermes does let an agent query its own execution history.** The
  `session_search` tool runs FTS5 over a `messages` table whose triggers index
  tool calls, at zero LLM cost. It is keyword search over a flat mutable table,
  not a memory model — so the honest contrast is **structure and governance, not
  existence.**

One standing rule applies to the framing itself: `areev-enterprise-proposal.md`
already requires that **the Hermes facts be re-verified per release** — Hermes
postdates the knowledge cutoff and moves fast (the audited checkout was already
two months stale), and a stale claim repeated after they fix it would burn
credibility with precisely this audience.

---

## 8. Risks

1. **The corpus does not exist yet, and the volume gate is far away.** §2.3
   against §1.4. `loop-proposal.md` §17 pre-committed to ~50–100 labeled
   decisions per analyzer and named building ahead of it "learning theater."
   Phase A is the honest answer; do not skip to Phase E.
2. **The OEL result is thin.** Grid games, small scale, one paper. It is the best
   available evidence and it is not a settled result. Phase B exists partly so
   that we measure this ourselves rather than inherit it — and the repo's own
   rule is that we never inherit vendor or benchmark numbers.
3. **The SEAL row.** §3 resolves it, but it must be resolved *explicitly and in
   writing* before any external mention of tuning. A quiet reversal of a
   published competitive argument is worse than the argument.
4. **Scope.** Five phases is more than the repo has ever taken on at once, and
   E0 leftovers from the enterprise proposal (`GET /api/graph/*`, `changes_since`
   in both bindings) are still unbuilt. Phases A and B stand alone; treat C, D
   and E as separately-approved.
5. **Determinism rots silently.** The clock is the default on every write. The
   Phase-B gate must be CI, not a check.
6. **Reward hacking is structural, not a scale artifact.** Nine gridworlds,
   observed reward ~22 with hidden safety reward ~0, and every mitigation tried
   failed — scaling, credit assignment, exploration prompts, entropy
   regularization. Any reward signal we emit will be gamed if it is ever used to
   close a loop without a human.
7. **The dated wedge decays.** The vendor-exit window (§2.4) is real for roughly
   12 months.
8. **Two competitors are one product decision away.** LangChain has every piece —
   it shipped **SmithDB** as a purpose-built trace database and **Context Hub** as
   the agent-readable memory layer in the same release, and **deliberately kept
   them apart**. If they merge them, the seam argument evaporates. Zep already
   sells governed memory and would need only to add erasure. Hindsight
   (Vectorize.io) is MIT-licensed, self-hostable, has an ACL 2026 demo paper — and
   is **already bundled inside Hermes as a rival memory provider**.
9. **The regulatory deadline moved 16 months out.** Do not build a plan whose
   urgency rests on 2 Aug 2026.

---

## 9. Decisions needed

Ordered. The first three are cheap and unblock everything.

1. **Close the `loop.run` cross-namespace read.** Not a decision so much as a
   confirmation to treat it as a bug and fix it first.
2. **Ratify the §3 boundary**: Areev emits a governed corpus and grades the
   result; it never trains. Everything downstream depends on this sentence.
3. **Assembly manifest: ride an `Observation` with zero format change, or take an
   OMS 1.6 amendment?** Recommendation: ride the Observation, sampled, and revisit.
4. **Approve Phase A + B only**, and defer C/D/E to a second decision after B's
   gate is green. Recommendation: yes — A and B are worth building even if the
   tuning direction is abandoned.
5. **Adopt the corrected Phase-1 determinism gate** (effect suppression +
   decision equivalence + journal immutability), replacing the byte-identical
   formulation in `areev-run-proposal.md` §7.
6. **Confirm `areev run`'s revised justification and cost.** The old estimate is
   materially low; a tool-calling LLM seam alone is a rewrite of `areev-llm`'s
   public surface.
7. **Re-sequence rung 3: evidence plane before RBAC/TLS/SSO.** This overturns an
   ordering already argued in `areev-enterprise-proposal.md` §3, on the grounds
   that the market filled those rows during 2026 while the evidence row stayed
   empty. Recommendation: yes, and it also happens to be the cheapest of the
   three, since the audit trail and op-log already exist.
8. **Narrow two claims in writing before they appear anywhere external** — the
   execution-history niche (partially occupied by AWS AgentCore Episodic Memory,
   GA and benchmarked) and "governed memory" (Zep's homepage copy). Both narrow
   to defensible ground; neither survives unqualified.

**Already queued elsewhere and still unanswered**, listed so they are not lost:
the two dependency exceptions (rustls, JWT); firing the CAL 1.3 headline, whose
D10 gate is satisfied; cutting 1.2.0; and the D9 roles design note.

---

## 10. Maintenance debt this proposal will trip over

Flagged because a reader will follow these citations and be misled.

- **`ARCHITECTURE.md` §7 describes rendering as "progressive disclosure… full
  form to summary to omitted (at tuned thresholds)."** `docs/facts/context-assembly.md`
  falsifies this in detail: the `Summary` tier is never emitted, no 95% threshold
  exists, and the `ASSEMBLE` path hardcodes `progressive: false`. The fact sheet
  is right and the architecture doc is wrong.
- **`ARCHITECTURE.md` still asserts the only destructive verb is a single-grain
  `FORGET`** with "no bulk-erasure primitive to reach for" — false since
  `FORGET SUBJECT` and `PURGE OLDER THAN` shipped. It also still says eight MCP
  tools (14), ~27 CLI verbs (~29), 11 analyzers (12), and has **no §10 entry for
  authorization at all** — the largest subsystem shipped in the last fortnight is
  absent from the decision log.
- **`areev-enterprise-proposal.md` §4.2 and §4.3 are contradicted by shipped
  code** ("nothing enters the CAL grammar"; "security config is never persisted
  in a memory file" — both reversed by CAL 1.3's `GRANT` and in-file
  `mg:permits`). Its §8.3 is a dead decision.
- **`docs/tmp/` is gitignored, entirely pre-rename, and both its "superseded by"
  pointers are broken links.** Nothing in it should be cited or shared.
- Two committed files still say "Waiser": `video/README.md` and
  `video/narration/script.json` — the explainer video's narration is publicly
  wrong on the product name.
