# Areev governed agents — the parity program

**Status:** proposal, design of record for the runtime + interop program.
Written 2026-08-14 from a five-track research pass (substrate audit against
`main` e0617cc; LangGraph 1.2.11 and CrewAI 1.15.16 verified against live
docs/source; an August-2026 competitive map; a 10-framework
harness/graph/loop parity scorecard). **Rev 3, same day: adversarially
reviewed and corrected.** Five independent review passes (correctness,
security threat-model, codebase fit, ecosystem fit, governance red-team) ran
against Rev 2 plus the executable-tools additions; ~70 findings were
adjudicated. The material corrections are marked inline as **[R3]**; the
biggest: the journal key gained `task_path`/`effect_seq`, the edge state
machine gained generations (Rev 2's could not iterate), result grains must
re-carry their identity (supersession hides the intent's links), fork
membership is manifest-resolved (not "shared grains"), redaction is
referential (not lossy), code-execution trust is host-rooted (in-file
provenance is fabricable), and every absolute claim now carries its honest
boundary in the same paragraph. Supersedes the *phasing* of
[`areev-run-proposal.md`](areev-run-proposal.md) §7 and absorbs Phase D of
[`areev-adaptive-agents-proposal.md`](areev-adaptive-agents-proposal.md); the
architectural rules of both survive unchanged.

**Decisions.** D1–D4 were made 2026-08-14 and are settled. D5–D10 are queued
in §13 with one-line recommendations.

| # | Decision | Choice |
|---|---|---|
| D1 | Runtime completeness bar | **LangGraph-parity**, not spine-only |
| D2 | Ecosystem depth | **Full persistence backend** (LangGraph Store + Checkpointer, CrewAI StorageBackend) — deliberately reverses areev-run-proposal §8 |
| D3 | Plan scope | **OSS + enterprise plane, OSS-first** |
| D4 | The Hermes bet | **Bet B** — `areev run` replaces Hermes in our own stack after Wave 2 |

---

## 1. Thesis

The August-2026 survey confirms the seam is **still unoccupied**: nobody
joins execution history + semantic memory + improvement governance in one
queryable, portable, auditable substrate. Every competitor governs either
the runtime (Microsoft Foundry, Temporal, guardrails vendors), the telemetry
(LangSmith, Braintrust), or the memory (Anthropic Managed Agents, Letta,
Mem0, Zep) — no one governs the *learning*, and no one can answer:

> *"Show me every run that touched what this agent believes, prove who
> authorized the change, and erase this person from all of it"* — **each a
> single command against one portable file.** [R3: "one query" → "each a
> single command"; erasure reach and the archive window are exactly as
> documented in `docs/gdpr.md` §4 and `docs/erasure.md` — the claim is the
> documented reach, stated precisely, which is still the strongest sentence
> in the market.]

That sentence is the headline. Orchestration features are never the
headline.

**Why now (three 2026 validations):**

1. **Anthropic shipped the walled-garden version.** Managed Agents memory:
   immutable versions, per-write audit, rollback, session attribution,
   automated curation — validating the thesis while lacking exactly what we
   have: portability, a query language, execution↔memory joins, *gated*
   improvement, self-hosting. Letta shipped git-backed versioned memory;
   LangSmith Engine ships trace-mined propose-fix-for-review.
2. **EU AI Act high-risk obligations became enforceable 2026-08-02** —
   Art. 12 lifecycle logging (agent tool-call layers in scope), Art. 14
   designed-in human oversight, ≥6-month retention. The EDPB's Feb-2026
   erasure action produced the consensus **"no provenance, no deletion"** —
   content-addressed grains with subject selectors is the only architecture
   in the competitive map that answers it.
3. **The July-2026 Hugging Face agent intrusion** (17k+ autonomous actions)
   made "replay every decision, joined to what the agent knew" a
   board-level forensics requirement.

**The scorecard narrative.** The 10-framework scorecard's hardest
differentiators — deterministic replay with a stated contract, versioned
plan artifacts, in-flight run migration, eval-gated promotion, on-policy
outcome measurement, shadow evaluation, and a governed
propose→verify→approve→apply→measure→rollback lifecycle ("the emptiest cell
in the scorecard") — are the cells Areev already owns or gets nearly free
from content addressing. What we lack — the harness and graph columns — is
table stakes with known shapes. **The field built harnesses and graphs
without governance; we built governance without a harness or a graph. This
program finishes the triangle, and only our column is a moat.**

Honest caveats carried forward: the seam is unoccupied because the split is
an annoyance, not a blocker (adoption is pulled by compliance and
forensics); Gartner projects >40% of agentic projects canceled by 2027 — we
sell into a trust crisis, which is the point.

---

## 2. Where Areev stands today (audit + review, main @ e0617cc)

**Composes as-is:** Workflow 0x04 storage on every surface; CAL `ASSEMBLE` +
areev-context per-step context assembly; the run↔memory reads
(`run_trace`/`run_yield`/`runs_touching`/`step_actions`/
`record_run_manifest`) on all six surfaces; tool-schema rendering to 9
provider formats; the Areev Loop engine (12 analyzers, four gates,
hash-chained audit lifecycle, DISCOVER→GROUND→VERIFY, outcome measurement);
authz grants + destructive cap; DSAR/erasure symmetry; the graded corpus
(Phases A–C). Load-bearing verified facts [R3]: the Tool grain's
async-supersession convention (`tool.rs:52`); `json_schema_subset` performs
real instance validation (type/const/enum/required/size, recursive);
`put_blob`/`get_blob` with HKDF-subkey blob encryption
(`areev.blobs.v1`); `AreevFacade` is `Sync` (one mutex; safe to share
across a thread pool); supersession is index-layer-only and
double-supersede returns `SupersessionConflict`; the auto-apply grant is
hard-coded to `memory|query` targets, so code targets are auto-apply-
ineligible **by construction**; `LOP` is the precedent for a new error
domain in a runtime-adjacent crate.

**Missing entirely (from scratch):** graph walk, condition evaluation,
`max_cycles`/`retries`/`trigger` enforcement, host tool dispatch, the
`ExecutorKind::Client` envelope (a doc-comment phantom at
`tool.rs:220-225`), an LLM tool-calling seam (zero `tools`/`tool_calls`
in areev-llm — verified), run identity/budget/approval.

**Exists but gapped — the Wave-0 list (expanded by review [R3]):**

1. **`related_to` silently dropped** by `add()` on every non-Rust surface
   (in `COMMON_KNOWN_FIELDS`, no `apply_common!` arm) — `mg:step_action`
   links are Rust-only writes. A live bug, not just a gap.
2. `record_tool_call` carries no workflow/node link, no
   status/failure_cause/executor_kind.
3. Top-level `run_id` is mechanically indexed on any grain type but is an
   untested, undocumented contract.
4. **[R3] Tool *Definition* fields are unreachable from the JSON build
   path** — `type_known_fields("tool")` covers execution fields only;
   `input_schema`, `executor_uri`, `locked_params`, `executor_kind`,
   `strict`, `tool_description`, `annotations` fall to verbatim extras that
   typed readers never see. Blocks CAL supersession of Definitions (§7's
   apply path) until fixed.
5. **[R3] No keyed or paginated journal read exists.** `run_idx` is
   `(ns, run, seq)`; `run_trace` and `step_actions` hard-cap at 1024 with
   no cursor — a run past ~340 supersteps would silently truncate its own
   journal on resume. A correctness hole, not a perf issue.
6. **[R3] No vector-in search API exists anywhere.** `nearest_semantic` is
   text-in and re-embeds; CrewAI hands the adapter a pre-computed
   `query_embedding`. Needed: `nearest_vector(ns, vec, k)` and
   embedding-on-add, dim-checked against file meta.
7. **[R3] The facade's principal binding is one process-wide slot** with a
   documented rebind race — per-principal write attribution (runs,
   responders) needs a principal-scoped write variant on the facade (or an
   interim runtime lock spanning bind+write).
8. Corrections of record: Tool grain has two phases; MCP exposes 16 tools;
   a complete Hermes `MemoryProvider` exists unpublished at
   `examples/hermes/areev/`.

---

## 3. The parity floor and the moats

The 15-item LangGraph table-stakes floor and our status (unchanged from
Rev 2 except the [R3] wording fixes):

| Floor item | Status |
|---|---|
| Durable threads, don't-rerun resume | Build (Wave 1) — journaled effects, §5 |
| interrupt/resume HITL | Build (Wave 1). [R3] **Durable HITL pause — the pause is a grain in the file: it survives process death by construction, and it survives host loss exactly as far as the file's own backup/sync reaches (`areev stream`, hub replication, Postgres DR). Run durability is file durability; we state that rather than imply replication we don't ship.** |
| Streaming (tokens + state) | Build (Wave 2) |
| Time-travel / fork | Build (Wave 2) — forks are manifest-resolved and verifiable (§5.4) |
| Subgraphs + Send fan-out | Build (Wave 2) |
| Typed state with reducers | Build (Wave 2) |
| Postgres + pluggable backends | **Have** |
| Cross-thread store, semantic search, namespacing | **Have** — with one honest footnote [R3]: LangGraph `SearchOp` operator filters (`$gt` etc.) exceed our structural filters; the adapter post-filters over an enlarged candidate pool (§8 Wave 3) |
| Provider-agnostic LLM/tool calling | Build the trait (Wave 0, §6.11) |
| Tracing (LangSmith or OTel) | Partial; OTel export Wave 5 |
| MCP / Agent Protocol | **Have** MCP (16 tools); run tools Wave 5 |
| Local dev server + debugger | **Have** console; run viewer Wave 5 |
| Python + JS | Store surface yes; runtime Python-first, JS Wave 5 (stated deviation) |
| Migration path | Build (Wave 3) |
| Semver discipline | Policy commitment |

**The moats** (unchanged in substance): G3 deterministic replay (LangGraph
publicly broken at crash boundaries — issue #8039, arXiv 2608.03836); G8
versioned plans (content-addressed by construction); G9 in-flight migration
(fork-onto-new-plan-hash); L5/L6/L9 (Areev Loop ships eval-gated promotion,
outcome measurement, and the governed lifecycle today); L8 (graded corpus,
shipped). Cheap differentiators: budgets on six axes (§6.7 [R3] — steps,
tokens, USD, wall, storage, and Wasm fuel), durable HITL (bounded as
above), auto-retry structured output, run forking, prompt-cache-stable
assembly.

**Lane picks** (settled): journaled-effects replay *plus* time-travel
forking; agents-as-tools subagents; governed context/memory evolution over
weights, with the corpus path as the bridge.

---

## 4. Architecture

```
areev-core ← areev-store ← areev-cal ← areev-context
      ↑                                        ↑
areev-loop ← areev-loop-adapter          areev-llm (+ ToolCallLlm trait)
                                       ↑
        areev-run-core   (NEW — pure, sans-IO scheduler)   [R3]
                                       ↑
        areev-run        (NEW — the host/driver, peer of areev-mcp)
                                       ↑
   areev-mcp · areev-server · areev CLI (`areev run`) · areev-py · areev-js
   areev-sandbox (NEW leaf — Wasm runner; workspace membership = D7)
   ─────────────────────────────────────────────────────────────────────────
   Python packages (outside the workspace): areev-langgraph, areev-crewai
   Hermes: examples/hermes/areev → published plugin
```

Rules preserved: execution never enters the engine (hosts execute; the
engine stores); no new CAL syntax; no core format changes (every mechanism
in §5–§7 uses existing fields plus extra fields — verified feasible [R3]);
dependency-light (streaming hand-rolled, bounded thread pool, no workspace
async runtime; percent-encoding hand-rolled ~20 lines).

**[R3] The pure core, corrected.** Rev 2's "RunCtx handle" contradicted
purity — a handle that reaches the journal *is* IO. `areev-run-core` is
**sans-IO**: it exports
`fn step(SchedulerState, &[EventIn]) -> (Vec<Command>, SchedulerState)`
plus pure types — no handle, no IO-bearing trait object, no areev-store
dependency (areev-core is format-only and permitted). The driver in
`areev-run` owns the facade, materializes journal lookups into `EventIn`s,
and executes `Command`s (`WriteIntent`, `Dispatch`, `Checkpoint`, …). This
shape is also what makes the simulation harness (§9) trivial.

**[R3] Purity enforcement, mechanically honest.** `cargo deny` cannot ban
`std::time` (std is not a dependency). Enforcement is: (i) a CI step
failing if `cargo tree -p areev-run-core -e normal` matches
`rand|getrandom|chrono|time|tokio|ureq|reqwest` (the crate takes zero such
deps); (ii) a crate-local `clippy.toml` with `disallowed-methods`
(`SystemTime::now`, `Instant::now`) plus a CI grep rejecting
`allow(clippy::disallowed_methods)` under that crate; (iii) `#![no_std] +
alloc` is the only way to make "cannot name it" literally true — optional
hardening, decided at implementation, not assumed. No
`HashMap`/`HashSet` in scheduler state (BTreeMap; hash-iteration order is
the classic silent divergence source); `serde_json` stays on its default
BTreeMap map (never `preserve_order`).

**Concurrency (verified [R3]):** one shared `Arc<AreevFacade>` (the
open-path registry forbids a second handle); all writes serialize on its
mutex — adequate for journaling. Standing discipline: **never dispatch an
executor while holding `with_store`.** Tier B/C sandbox subprocesses never
open the memory file — the parent journals on their behalf, and the
cross-process OS lock backstops the rule.

---

## 5. The replay contract

**The claim, stated honestly at the top [R3]:** an Areev run is a pure
function of `(plan grain, run manifest, input, journal)`. **The journal is
exactly-once**: a result-journaled effect is never re-executed and never
double-counted — across interrupts and across crashes. **The external
effect is at-most-once per attempt in normal operation and at-least-once
across the dispatch-to-result crash window**: a dangling intent is
re-delivered with the same idempotency key, and every redelivery is
journaled as an Observation — recorded, never silent. Hosts that cannot
deduplicate on the key set `on_dangling = fail`. *Glossary rule for every
document and demo: "exactly-once" may only ever modify "journaling";
effects are described only as above.*

### 5.1 The journal mechanism

- **Intent** = a Tool Execution grain, `status = Pending`, written before
  dispatch (core's documented async convention). **[R3] Mandatory content
  fields:** `run_id`, `task_path`, `node`, `attempt`, `effect_seq`,
  `superstep`, `tool_call_id`, the `mg:step_action:<node>` link, and the
  input payload.
- **[R3] The journal key** is
  `(run_id, task_path, node, attempt, effect_seq, kind)`:
  - `task_path` — the deterministic spawn path (`""` for the static graph;
    `parent_path/spawn_ordinal` per Send, ordinal = position in the
    journaled spawn decision). Without it, N Send-spawned tasks at one node
    collide.
  - `effect_seq` — the 0-based ordinal of the effect within a node attempt
    (an abstract node is an LLM *loop*: turn 0, its tool calls 1..k in
    provider order, the follow-up turn k+1, schema re-prompts continuing
    the sequence). Within-attempt re-prompts consume `effect_seq`, never
    node attempts.
  - **`tool_call_id` = the digest of the journal key** — unique per
    occurrence by construction *and* reproducible under scheduler
    permutation (a random id would make the permutation gate impossible).
- **Result** = a supersession of the intent, `status = Completed | Failed`
  (+ `failure_cause`), output, and — for LLM effects — mandatory `usage`.
  **[R3, blocker fix] The result grain re-states `run_id`,
  `tool_call_id`, `task_path`, `node`, `attempt`, `effect_seq`,
  `superstep`, and the `mg:step_action` link.** Supersession flips the old
  grain's link rows to non-current and propagates nothing — without
  re-statement, the node's execution record vanishes from `step_actions`
  and the result never appears in `run_trace`. (The existing
  `repeated_attempts…` test never supersedes; a supersession case is added
  to it.) `run_trace` returns both intent and result forever (run-index
  rows don't participate in supersession); consumers fold chains via
  `supersession_map` — stated so no surface double-counts.
- **Idempotency key** =
  `sha256(run_id ‖ task_path ‖ node ‖ attempt ‖ effect_seq ‖ canonical(input))`,
  handed to the executor with every dispatch and identical on redelivery.
- **Decision record** (part of each checkpoint): every condition outcome,
  every edge transition `(edge, generation, outcome)`, cycle counters,
  spawn decisions, and the superstep's two clock readings (§6.7). Replay
  re-derives and asserts (`RUN-E009`) — this is what catches an impure host
  evaluator or reducer.

### 5.2 Determinism rules

- **Persistence order is free; state-merge order is canonical.** Intents
  write at dispatch; results write at completion (durability first). Replay
  never reads by op-log sequence — only by journal key. Reducers apply in
  canonical order `(superstep, node position, task_path, attempt)`.
  Presentation: the **runtime** sorts `run_trace` canonically (the store
  returns op-log order; the sort is possible precisely because the fields
  of §5.1 are in-content) — stated so cross-surface parity doesn't "pass"
  showing six copies of op-log order.
- **[R3] Runtime-authored grains take `created_at` from the journaled
  clock**, never wall-clock. Uniqueness under identical inputs is carried
  by the key fields (attempt/effect_seq/task_path in content), not by
  timestamps — the release-only sub-ms collapse shape is structurally
  impossible, and a property test (run under `--release`) proves distinct
  hashes for identical-input retries within one superstep.
- One clock *pair* per superstep (§6.7), RNG seed in the manifest, retry
  backoff sleeps decide nothing and are skipped on replay; the scheduler
  never reads env or locale (crate boundary).

### 5.3 Resume algorithm (normative)

1. Load the manifest (plan hash, pinned tool resolutions, config hash, RNG
   seed, budgets, principal, price-table hash, redaction posture, owner
   nonce). Verify plan + pinned tool grains resolve (`RUN-E004`).
2. **[R3] Ownership check:** a run is owned by one memory. If the file's
   op-log head postdates the manifest's last-recorded op and the owner
   nonce doesn't match, resuming requires explicit `--fork` — resuming a
   *copy* is a fork by rule, never a silent continuation. If any journal
   grain of the run has forked supersession tips (two results imported for
   one intent), the run is **`Tainted`** (`RUN-E016`): non-resumable,
   non-verifiable until a human reconciles — the provisional-head tiebreak
   never silently elects a winner for an execution journal.
3. Load the target checkpoint; verify `run_id` + plan hash (`RUN-E010`).
   Rebuild scheduler state from the checkpoint only.
4. **[R3] Dangling-intent scan, bounded and filtered:** intents with
   `status = Pending`, not superseded, `executor_kind ≠ Client`, and
   `superstep ≥` checkpoint's (the field is in-content, so the scan is a
   filtered, **paginated** journal walk — Wave 0 adds the cursor API).
   Policy: re-dispatch with the same idempotency key + journal a
   redelivery Observation keyed
   `(run, task_path, node, attempt, effect_seq, redelivery_ordinal)`,
   written idempotently (lookup-before-write) so crash-loops can't inflate
   the count; or `on_dangling = fail` (`RUN-E008`). No third mode.
   **Client asks are never re-dispatched**: pending ones simply remain
   pending; expired ones are settled by the scheduler at resume
   (superseded to `Failed/Timeout` against the resume superstep's journaled
   reading).
5. Continue superstepping; journal hits return recorded results; decision
   records re-derive and assert.

### 5.4 Checkpoints, forks, migration, erasure

- One State grain per superstep (`context_data` = state after spill,
  `plan` = live nodes, `history` = decision record + journal grain refs),
  `derived_from` its predecessor, ≤1 MiB after spill (`RUN-E015`).
- **[R3] Forks are manifest-resolved, not "shared grains".** Journal
  grains carry the *parent's* `run_id`; the fork's manifest records
  `(base_run, base_checkpoint)` and a `mg:fork_of` Fact indexes the
  relationship. `run_trace(fork)` splices parent traces (truncated at each
  base checkpoint) with the fork's own tail; `runs_touching` consults the
  fork index and reports forks for inherited-prefix grains — without this,
  the compliance headline silently omits every fork. Verifiability = the
  manifest's base-checkpoint hash chain.
- **In-flight migration** = fork onto a new plan hash; full §6.1
  validation re-runs before the first superstep.
- **[R3] Run-aware erasure.** Subject data flows through reducers into
  every later checkpoint and spilled blob — tombstoning the matching
  journal grains alone fails Art. 17 exactly the way the raw-identity
  audit bug did. Rule: when `FORGET SUBJECT` matches journal grains of run
  R at superstep k, the sweep also tombstones R's checkpoints with
  superstep ≥ k and their spilled blobs (walkable via `derived_from`); the
  run becomes non-resumable past k−1 **by design**. The erasure receipt
  records the journal *keys* and truncation superstep (keys are not
  subject data); `verify` distinguishes **tombstoned** (reported as "step
  erased under audit record X", continue) from **absent-without-receipt**
  (`RUN-E004` — integrity failure). Erased runs are exempt from the
  spend-recomputation invariant. Because `forget` deletes run-index rows,
  the receipt — not the index — is the data source for the hole report.
- **[R3] Run-atomic retention with floors.** Run journals live in
  run-scoped namespaces; the retention sweep unit is a whole *terminated*
  run (manifest + journal + checkpoints + blobs), eligible only when
  `terminal_at < horizon` and no fork manifest references it. Namespace
  retention gains **floors** (`--min-days`) and a **legal hold** file-truth
  (`areev hold set --ns … --because …`): `PURGE OLDER THAN`, sweeps, and the
  retention analyzer refuse grains younger than the floor or under hold
  (`RUN-E017`) — otherwise an operator can destroy the very Art. 12 logs
  §1 cites, with a clean audit trail of doing so. Hold-vs-erasure
  precedence is D10.

### 5.5 `areev run verify` — three tiers, labeled output [R3]

Verify is a product surface and states exactly what it proved, per step:

1. **journal-consistent** (all runs): checkpoints and decisions re-derive
   from journaled results; byte-exact same-architecture, decision-exact
   cross-architecture (libm transcendentals differ across platforms — this
   caveat is normative here, not a footnote), and the output labels which
   mode ran. Verify-mode **writes nothing** — it recomputes and
   byte-compares, never calls `add`.
2. **re-execution-proven** (Tier-C capability-zero Wasm tools only):
   outputs re-executed and byte-compared — "proven, not just journaled."
3. **not verified**: the authenticity of journaled Tier-A/B results is
   trusted as recorded until grain signing is enforced on import — an
   operator with file write access can fabricate a plausible journal. Said
   plainly, because diligence teams will find it anyway.

### 5.6 The CI gates

1. **Replay-equivalence** — verify-mode replay from every checkpoint of the
   reference runs: byte-compares checkpoints, asserts journal-set
   identity. A resumed *live* run asserts prefix-set identity plus
   decision-record equality (its tail legitimately mints new grains).
2. **Crash-injection** — deterministic injection points (never sleeps) at
   every superstep boundary and inside the intent→result window; resume;
   assert zero duplicate executions per idempotency key and exactly one
   redelivery Observation per redelivery.
3. **Scheduler-permutation** — adversarial completion orders and pool
   sizes 1/2/N, Send-spawned tasks included; identical checkpoints and
   grain sets (this is the gate `tool_call_id`-as-key-digest exists for).

---

## 6. Runtime semantics (pinned)

### 6.1 Load-time validation (fail fast; re-run at resume and fork)

- **V1** nodes non-empty; IDs unique, ≤256 bytes, no control chars; entry
  = `nodes[0]`.
- **V2** edge referential integrity (re-validated at runtime — OMS files
  arrive from other implementations); self-edges follow V5.
- **V3** bindings/retries keys ∈ nodes; Bound hashes resolve to Tool
  Definitions (`RUN-E004`). [R3] Definitions arriving in foreign files may
  carry out-of-subset schema keywords, which `json_schema_subset`
  *silently skips* — V3 runs the structural subset check on pinned
  definitions and surfaces a warning listing skipped constraints.
- **V4** conds parse (`validate()` hook on the evaluator) (`RUN-E005`).
- **V5** every SCC (Tarjan) contains ≥1 `max_cycles` edge (`RUN-E002`);
  global `max_supersteps` backstop (default 1024, in the manifest).
- **V6** all nodes statically reachable (`RUN-E003`).
- **V7 — resolution freeze.** Named/Abstract resolution happens once at
  run start, pinned into the manifest (Named → resolved Definition hash;
  Abstract → requires a configured ToolCallLlm, `RUN-E006`). Resume never
  re-resolves.
- **V8 [R3] — price-table coverage.** Every model reachable from the
  manifest has a price row; a result reporting an unlisted model is
  `FailureCause::ExecutorError` *before* its output enters state — never
  silently priced zero, never a retroactive failure of the whole run.

### 6.2 The scheduler: generations, joins, dead paths, termination [R3 — rewritten; Rev 2's machine could not iterate]

Rev 2's `Pending → Fired | Dead` edge states are both terminal, so a
back-edge could never fire twice, and joins fed by cycles false-`Stalled`.
Corrected model:

- **Edge state is per-generation:** `(edge, gen): Pending → Fired | Dead`.
  When a back-edge fires into a node of SCC S, every node and edge inside
  S increments generation and resets to `Waiting`/`Pending` for gen g+1;
  edges *leaving* S resolve once per generation.
- **Readiness** = no in-edge Pending **at the current generation** and ≥1
  Fired at it. **Death** is SCC-quotient: on the condensation DAG (already
  computed for V5), an SCC all of whose external in-edges are Dead is Dead
  as a unit; a generation that ends without re-entering its SCC marks that
  generation's intra-SCC pending edges Dead — resolving downstream joins
  that Rev 2 would have deadlocked (a join fed from inside a cycle that
  died) or false-`Stalled`.
- **Out-edge evaluation** on `Done(ok)`, declaration order, unconditional
  always fires, conditional per §6.4, exhausted back-edge = Dead with a
  journaled `cycle_exhausted` outcome. **Cycle counters count
  generations**, restoring §6.3's meaning; counters live in checkpoints
  and are replay-asserted.
- **Failure policy (v1):** retries exhausted → `Done(failed)` → the run
  **drains in-flight tasks, checkpoints, terminates `Failed`** naming the
  node. Explicitly: `Done(failed)` triggers run-drain, *not* dead-path
  propagation — stated so a future per-node-handler addition doesn't
  inherit a join that deadlocks below a handled failure.
- **Termination:** frontier empty, nothing Running/AwaitingClient →
  `Completed` if ≥1 terminal node `Done(ok)`, else **`Stalled`**
  (`RUN-E001`, naming the all-conditions-false node). Termination bound:
  total edge traversals ≤ forward-edges × (1 + Σ max_cycles into each
  SCC), plus the superstep backstop.
- **Superstep:** evaluate frontier in canonical order → dispatch (the only
  parallelism) → collect all results → reduce in canonical order →
  evaluate edges → journal the decision record → checkpoint.

### 6.3 Cycles and retries

`max_cycles = n` = the edge fires in at most n generations per run.
`retries[node] = n` = n *re*-attempts (n+1 executions), attempts numbered
from 1, each attempt its own intent/result pair and step_action record.
Retryability: `Timeout`/`ExecutorError` retryable; `SchemaValidationFailed`
retryable only via the LLM re-prompt path (consuming `effect_seq`, §5.1);
`UserAborted`/budget/cancel never. Backoff: fixed policy, sleeps only,
skipped on replay.

### 6.4 Conditions (frozen v1 grammar)

```
cond := path op literal | path "exists" | ["!"] path
op   := "==" | "!="        path := [A-Za-z0-9_-]+ ("." …)*
literal := JSON string | number | true | false | null
```

Strict JSON equality, no coercion; truthiness: `false`/`null`/missing/
`""`/`0`/`[]`/`{}` false, else true; parse errors at load; evaluation
total; host evaluators pure — enforced by the journaled decision assert,
not trust. No expression language beyond this in v1.

### 6.5 State, reducers, spill

One JSON state object (Send tasks get task-scoped state journaled with the
spawn decision). Host reducers per key: pure, deterministic,
batching-invariant; default LWW in canonical order. Shipped
`check_reducer_laws` property harness + debug-build sampling
(`RUN-E014`); the harness flags transcendental float ops (cross-arch
hazard, §5.5). Spill: values > 256 KiB → CAS blobs (encrypted under the
memory's subkey), `{"$blob": uri}` refs; checkpoint >1 MiB after spill →
`RUN-E015`.

### 6.6 HITL: the `requires_action` envelope

Client-bound dispatch writes the intent (`Pending`, `executor_kind =
Client`, `tool_call_id`, optional `expires_at_sec`), parks the node
`AwaitingClient`, checkpoints, returns:

```json
{ "kind": "requires_action", "run_id": "…", "checkpoint": "<hash>",
  "asks": [ { "tool_call_id": "…", "node": "…", "tool_name": "…",
              "input": { … }, "output_schema": { … } | null,
              "expires_at_sec": … | null, "approval": true|false } ] }
```

- Responses address asks **by `tool_call_id`, never by index** (LangGraph's
  by-index matching is a footgun we don't replicate).
- **[R3] `respond` is itself an effect boundary:** it takes a *fresh*
  clock reading, journals it on the superseding result, and evaluates
  `expires_at_sec` against that reading (there is no "scheduler clock at
  respond time" — the scheduler is parked; nondeterminism enters and is
  recorded, like any effect). Replay uses the journaled value.
- Validation order: run pausable → id names a Pending Client ask
  (`RUN-E011`) → not expired → authorized (§6.8) → result validates
  against `output_schema` (`VAL`, not stored) → supersede, attributing the
  responder.
- **[R3] Losing responses are journaled, not vanished.** A response that
  loses the supersession race, or arrives on a settled/expired ask, is
  recorded as an Observation (responder, tool_call_id, submitted-outcome
  hash, rejection reason) *before* the caller gets `RUN-E011` — "two
  officers approved conflicting outcomes seconds apart" is exactly what an
  auditor needs to see. The same rule covers a live-result vs
  redelivery-result race on Host effects: first-commit wins (the store
  guarantees one winner); the discarded real execution's output is
  journaled as an Observation, because a divergent dropped result is
  forensic evidence.
- **[R3] Approval asks require separation of duties:** for asks flagged
  `approval: true`, responder ≠ triggering principal is **refused, not
  conventioned** — mirroring the loop's self-approval block. Responding
  and resuming stay separate acts.

### 6.7 Budgets and cancel

- Manifest: `max_supersteps`, `max_tokens`, `max_usd`, `max_wall_ms`,
  **[R3] `max_storage` (single-result bytes / cumulative blob bytes /
  journal grain count — the axis an attacker controls most cheaply)**,
  price table by CAS hash, redaction posture, owner nonce. Wasm fuel
  (§7) is the sixth axis where Tier C runs.
- **[R3] Wall-clock charges active time only.** Each superstep journals
  *two* readings (start, end); `wall = Σ(end−start)`. Parked/crashed gaps
  accumulate as `elapsed`, reported but never charged — otherwise a
  three-day approval pause (the §6.6 selling point) kills the run at
  resume. Calendar bounds belong to `expires_at_sec`; a distinct
  `max_elapsed_ms` axis may be added later, never overloaded onto wall.
- **[R3] Enforcement is per-dispatch, not per-superstep.** Before each
  effect dispatch: `spent(journal) + in-superstep accumulator +
  reserve(effect)` must fit, `reserve` = the call's mandatory
  `max_tokens` (required for budgeted runs). Overshoot is bounded by
  parallelism × one call — pre-flight-only checking allowed
  `fan-out × retries × max-output` overshoot inside a single superstep.
- All axes are pure functions of the journal; raising a budget = journaled
  manifest supersession. Exhaustion → checkpoint + `BudgetExhausted(axis)`
  (`RUN-E007`), resumable.
- **Cancel:** cancel Event (`run_id`, principal, reason); checked at
  superstep boundaries and before HITL parks; **[R3] preemptive for tool
  execution** — Wasm via epoch interruption (a watchdog bumps the engine
  epoch; deterministic trap surfacing as `FailureCause::UserAborted`,
  never retried), Tier-B subprocesses via SIGKILL, plus a per-call
  wall-clock ceiling. The Wave-6 kill-switch SLA is claimed **only for
  epoch-interruptible/killable tiers** and measured, not asserted.

### 6.8 Run identity and authorization

- The manifest records the triggering principal; journal writes are
  attributed to it. **[R3] Attribution mechanics:** the facade's
  process-wide principal slot has a documented rebind race — Wave 0/1 adds
  a principal-scoped write path (per-call `AuthzSet`), with a
  runtime-level bind+write lock as the interim. Runs never loosen the
  destructive cap.
- **D5 (queued):** verbs `run.execute`, `run.respond`, `run.cancel` in
  `authz::ALL`. **[R3] Tier mapping pinned in the recommendation:**
  `run.execute` and `run.respond` are Control-tier (starting effectful
  budget-spending computation / exercising approval authority — not plain
  `write`); `run.cancel` is deliberately low-tier and broadly grantable —
  the brake must never be blocked by missing privilege.

### 6.9 Error codes — the `RUN` domain (append-only from day one)

RUN-E001 stalled · E002 unbounded cycle · E003 unreachable node · E004
binding/pinned hash unresolvable (or journal ref absent without erasure
receipt) · E005 condition invalid · E006 abstract node without ToolCallLlm
· E007 budget exhausted (axis) · E008 dangling intent with `on_dangling =
fail` · E009 replay divergence · E010 checkpoint/manifest mismatch · E011
response names no pending ask (loser journaled first) · E012 run operation
unauthorized · E013 canceled · E014 reducer law violation · E015
checkpoint over cap · **[R3] E016 journal tainted (forked supersession
tips / ownership violation) · E017 retention floor or legal hold violation
· E018 code execution refused (§7, names the failed condition)**. Surface
mapping per existing conventions.

### 6.10 Streaming (Wave 2) — observational only

Events (`RunStarted` … `RunFinished`), each stamped `(superstep, node,
task_path, attempt)`; bounded buffer, drop-oldest + counter on
`RunFinished`; never backpressure into the scheduler. Check: identical
journals with no subscriber, a subscriber, and a deliberately slow
subscriber.

### 6.11 The `ToolCallLlm` contract (areev-llm, Wave 0)

Request: model, system, messages, tools (rendered per provider by the
existing 9-format machinery), tool_choice, **mandatory `max_tokens`**,
temperature (default 0). Response: text?, `tool_calls[{id, name,
arguments}]`, stop_reason, **mandatory `usage`** (a backend that can't
report usage can't serve budgeted runs, by construction). Checks:
arguments parse → validate against `input_schema` via `json_schema_subset`
when `strict` → `SchemaValidationFailed` re-prompts with the error
appended (consuming `effect_seq`); unknown tool → one re-prompt then
`ExecutorError`; 429/5xx/timeout retryable, other 4xx terminal.
**[R3] Streaming is sized honestly:** areev-llm today is blocking ureq
with whole-body reads, a 10 MiB cap, `"stream": false`, and a 120-second
body timeout that would kill any long stream — TokenChunk needs a
`stream: true` path, a hand-rolled SSE/NDJSON line parser (no new dep),
three per-provider delta schemas, and the timeout rework. It is the long
pole of Wave 0. Fixture conformance ×3 providers (clean call, malformed
args, schema violation, unknown tool, parallel calls, streamed); opt-in
live smoke.

---

## 7. Executable tools — code as governed grains [R3 — hardened throughout]

**The invariant survives:** "Tool grains are data, never executables" is
violated by the *engine* executing code, not by storing it. Code = CAS
blob + Tool Definition with `executor_uri = "code:…"` — zero core changes.
Hosts (`areev-run`, `areev-sandbox`, an opted-in client) execute, in
sandboxes. What it buys: the self-improvement loop closes over
*capabilities* — tool_failure detects a weak tool, the loop proposes a
revision, gates review it, the evalset executes it, Apply supersedes the
Definition, V7 keeps in-flight runs on the old hash, regression reverts by
pointer flip — every step audited. Nobody ships this.

### 7.1 Trust is host-rooted, never file-derived [R3 — the review's deepest finding, three lenses converged]

Grants are `mg:permits` Facts *in the file*; bundle import verifies
content integrity, not authorship; grain signing is dormant scaffolding.
So a crafted file can carry the code grain, a grant authorizing it, and a
fabricated "Applied recommendation" audit chain — all internally
consistent. Therefore:

- **Execute-authorization consults host-side state only**: a host-held
  **approval registry** of executable code hashes (the `loop-policy.json`
  pattern — host config, never persisted in a memory file), populated
  only by this host's own gate-approved Apply events or explicit operator
  registration. In-file grants may *narrow*, never widen. Imported audit
  chains are **evidence, not authorization**.
- When COSE signing lands (named dependency), the registry upgrades to
  signature verification against host-configured author keys.
- The honest claim until then: *a crafted bundle is defused by the
  default-off cap, the host registry, and the sandbox — not by
  provenance.*

### 7.2 The execution gate (all conditions, default-deny)

1. **The process cap is open** — **[R3, D8] code execution ships
   default-OFF** (`--allow-code-exec` to enable; the destructive cap's
   default-allow pattern is the wrong default for LLM-written code).
2. The code hash is in the **host approval registry** (§7.1).
3. The principal holds the execute grant (file grants narrowing only).
4. **[R3] The capability set is bound into what was approved.** The
   capability manifest (net/fs/clock imports, limits) is a field of the
   Tool Definition — part of the content address — so changing
   capabilities = new hash = new approval + new evalset run, and the
   evalset executes under the *exact* approved capability set. Without
   this, an approved "pure" tool can later be re-dispatched with network
   granted while every gate still reads green.

Refusal = `RUN-E018` naming the failed condition. Storing code never
authorizes executing it; every execution is journaled.

### 7.3 The tiers, honestly named [R3]

- **Tier A — host-registered** (status quo): connectors, credentialed,
  effectful. Never goes away; never a code grain.
- **Tier B — host-delegated execution. No isolation is guaranteed by
  Areev.** (Rev 2 called this a sandbox; it is `--tool-runner-cmd` +
  DB-resident code — if the runner is `python {file}` there is no
  sandbox.) Tier B alone **cannot satisfy gate condition 1's spirit**:
  enabling it requires either a runner isolation self-attestation
  (`docker|gvisor|none`, host policy sets the minimum, default rejects
  `none`) or an explicit acknowledged-unsandboxed flag that is stamped
  into every execution's audit record. SIGKILL on cancel.
- **Tier C — first-party Wasm** (`areev-sandbox`): **[R3] blessed format
  = pure wasm32 core module, no WASI, a tiny frozen areev-defined import
  set** (alloc + linear-memory JSON in/out). Capability imports default
  none. Fuel metering (deterministic compute budgets) + epoch
  interruption (preemptive cancel) + memory caps, **plus the compile-side
  defenses fuel doesn't cover**: module byte cap before decode,
  compile-time/memory limits or offline AOT with the artifact hash
  pinned, `max_wasm_stack`, import namespace pinned to the frozen set,
  and RUSTSEC advisory gating on wasmtime in the existing `cargo deny`
  CI. Pure Tier-C tools are the **re-execution-proven** verify tier.
- **[R3] Client-side execution is opt-in per client with its own cap and
  its own registry** — the envelope's code hash is an offer, not an
  instruction. The console (a browser) **never auto-executes**: a
  code-bearing ask renders as an inert, escaped diff. The blob fetch path
  is authenticated like `/api/segment*` (never the console's default-open
  read posture). "Same hash both sides" is claimed for v1 as *another
  areev process/edge runtime running areev-sandbox*; browser execution is
  a later tier that must resolve import- and fuel-parity explicitly
  (browsers have no WASI and cannot enforce or report fuel).

### 7.4 Governing the loop that writes code [R3]

- **Never auto-applied, stated by name:** code/tool targets are excluded
  from auto-apply categorically. (Verified: `grants_auto_apply`
  hard-codes `memory|query`, so this is structural today — the rule makes
  it a stated invariant with a pinning test, not an accident.)
- **Rule E1 — the gate cannot be gamed by what it gates:** a
  recommendation targets code *or* an evalset, never both; evalset
  changes are their own always-human-approved class with removed/weakened
  cases rendered in the diff; every code recommendation **pins the
  evalset hash** it was gated against (shown at review: "gated against
  `<hash>`, last modified by `<actor>`"); superseding an evalset
  invalidates in-flight code recommendations pinned to it (they re-gate);
  LLM-drafted evalsets inherit the never-auto-apply origin rule.
- **The evalset-run edge is recorded:** Apply of a code recommendation is
  *refused* unless its audit Observation carries refs to (evalset hash,
  gating `run_id`, pass/fail stats). The full chain — code hash → Applied
  recommendation → approver + BECAUSE → evalset run + result →
  `runs_touching` count — is surfaced as **`areev tool provenance <hash>`**
  (+ MCP twin). One-command forensics is a product surface, not an
  implied join.
- **Revert carries blast radius:** every code revert (auto-proposed or
  manual) MUST attach `runs_touching(<reverted hash>)` since Apply, the
  affected runs' traces, and the grains they wrote — reverting code does
  not revert data; the report is what makes the data reviewable.
- **Controls by tier — REQUIRED vs RECOMMENDED:** required for all code:
  no auto-apply, pinned evalset + recorded gating run, an outcome metric
  attached at Apply with the revert proposal wired on regression.
  Required for Tier B additionally: a second human reviewer (code ≠
  config) and a shadow replay of N recent journaled runs (zero effect
  dispatches) before Apply. Recommended (Wave-4 design item, not v1):
  staged/canary promotion — a `candidate` head that becomes the default
  resolution only after M clean runs.
- **The git mirror is one-way by construction:** export-only; no product
  code path reads the repo; a commit never becomes a grain; the mirror
  runs with a read-only Areev principal and a write-only git credential;
  tested by the mirror binary having no import entry point. Code enters
  the substrate only via authored add or an Applied recommendation.
- **Erasure × pins:** erasing or retiring a code grain still pinned by an
  active run manifest is blocked or force-cancels the pinning runs — a
  governed decision, never a mid-resume `RUN-E004` surprise; `verify`
  reports "code erased under record X" for historical runs. Note the
  plaintext-digest oracle: blob addresses are digests of plaintext even
  under encryption — sensitive proprietary tool code may opt into
  keyed-address blobs (documented trade: loses cross-store address
  stability).
- **Journal taint:** every journal grain is stamped with origin
  (`executor_kind`, tool hash, principal); tool-output-derived text is
  `untrusted` by construction, assembly policies can fence or down-weight
  it (rendered as delimited untrusted content, never instructions), and
  the loop's standing rule — attacker-influenced free text disqualifies
  automated action — extends to journal-derived text. This is the
  persisted-prompt-injection defense, designed before the journal exists
  rather than retrofitted.

### 7.5 Secrets and redaction [R3 — Rev 2's lossy redaction was self-contradictory]

Lossy redact-then-journal breaks both replay (the idempotency key becomes
uncomputable from the journal) and redelivery (the re-dispatched input is
`[REDACTED]`). Pinned instead:

- **Referential secrets are the boundary:** hosts pass secrets as stable
  references — `{"$secret": "<key>"}` — resolved from host config at
  dispatch (identically on redelivery). The journal, the idempotency key,
  and replay all see the reference form. Erasing a secret = deleting the
  vault entry; redelivery then fails *loudly* (`ExecutorError`), never
  silently replays holes. Credentials belong in `locked_params`/executor
  config and never in state — stated at the `areev run` surface, not just
  here.
- A host-registered **lossy** redactor is permitted on *result* grains
  only (results are never re-dispatched); it must be deterministic, its
  identity/version is pinned in the manifest (like the price table), and
  it **fails closed** — on redactor error the step fails; raw bytes are
  never journaled.
- **D9:** the manifest carries an explicit `redaction: none | <id>` field
  so every audit sees the posture; v1 ships no built-in redactor
  (default `none`, loudly documented) — the referential-secret rule is
  the primary control, redaction the backstop.

---

## 8. The workstreams

### Wave 0 — substrate closure [R3 — grown by review; still the unblock-everything wave]

1. Fix the `related_to` silent drop (validated entries, `VAL` errors,
   count caps, cross-surface tests).
2. Extend `record_tool_call` (workflow/node link, status, failure_cause,
   executor_kind, correlation_id).
3. Freeze the `run_id` contract (validation 1–128 bytes; tests per grain
   type; conformance case on both backends).
4. **[R3]** Extend `type_known_fields("tool")` to the full Definition
   vocabulary (`input_schema`, `executor_uri`, `locked_params`,
   `executor_kind`, `strict`, `tool_description`, `annotations`, plus
   execution-side `status`/`failure_cause`) — required for §7's apply
   path and faithful adapters.
5. **[R3]** Paginated journal reads: `run_grains(ns, run, after_seq,
   limit)` (or run_trace cursors) at store + facade; lift the 1024 caps
   on `run_trace`/`step_actions` behind pagination.
6. **[R3]** Vector-in APIs: `nearest_vector(ns, vec, k)` (+ hybrid
   variant) and optional `embedding` on add, dim-checked against file
   meta.
7. **[R3]** Principal-scoped write path on the facade (per-call
   `AuthzSet`), retiring the rebind race for attributed writes.
8. `ToolCallLlm` per §6.11 — including the streaming groundwork (SSE
   parser, per-provider deltas, body-timeout rework); the long pole.

**Gate:** step_action written from Python, read back everywhere; fixture
suite green ×3 providers; fuzz of the `related_to` and workflow
validators; pagination proven past 1024 journal grains; vector roundtrip
dim-checked.

### Wave 1 — the deterministic spine

§5 + §6 for Host and Client nodes, on the sans-IO core + driver split;
parallel dispatch from day one (§5.2 makes it safe; retrofitting
parallelism is how canonical-order bugs are born). **[R3]** Includes the
ownership nonce, `Tainted` detection, erasure receipts, retention floors +
`areev hold`, and the **10-minute proof** deliverable: `areev init
--template run-demo` — run → pause on HITL → approve as a second principal
→ resume → `areev run verify` → provenance — no LLM key required. Buyers
don't read §5; they run this and then try to break §5's claim.

**Gate:** the three §5.6 CI gates; paused run resumes from the file alone
in a fresh process; redelivery = one Observation, zero duplicate
executions; principal without `write` fails the first journal write;
approval ask self-response refused; erased-run verify reports holes via
receipts; `--release` retry-hash property test.

### Wave 2 — agent-grade + LangGraph parity

Abstract nodes (the LLM loop journaled per §5.1's `effect_seq`),
structured-output auto-retry, streaming, subgraph nodes (child `run_id`,
`parent_task_id` linkage), Send fan-out (journaled spawns, `task_path`),
typed reducers, time-travel forks + fork-onto-new-plan (§5.4's
manifest-resolved mechanics), prompt-cache-stable assembly.

**Gate:** the parity demo (diamond, bounded cycle via **generations**,
map-reduce, subgraph, interrupt + fork, streamed); all three replay gates
green with parallelism + streaming + Send permutation; fork's `run_trace`
splices correctly and `runs_touching` reports the fork on inherited
grains.

### Wave 3 — ecosystem adapters (Python; parallel from Wave 0)

**Standing rule [R3]:** each adapter implements and enumerates the
**complete** abstract surface of its upstream interface — the saver's
`get_tuple/list/put/put_writes/delete_thread/delete_for_runs/copy_thread/
prune/get_delta_channel_history` + async twins; the store's
`batch/abatch`; CrewAI's full `StorageBackend` protocol + async mirrors.
(Four of the review's findings were children of listing only the methods
we had answers for. Unimplemented abstract methods fail at instantiation.)

**`areev-langgraph`:**

1. `AreevStore(BaseStore)` — reversible percent-encoded namespace mapping
   (property-tested); put→add, overwrite→supersession, delete→tombstone;
   **[R3]** operator filters (`$gt` …) post-filtered adapter-side over an
   enlarged candidate pool (documented cap); `refresh_ttl` args
   accepted-and-ignored under `supports_ttl=False`; batches execute
   in-order with read-your-writes; `created_at` **denormalized forward**
   on supersede (O(1) gets, chain walk only for legacy files); provenance
   is recorded and visible on Areev surfaces — their `Item` has no field
   to carry it, so the claim is scoped to our surfaces.
2. `AreevCheckpointSaver` — **[R3] identity =
   `(thread_id, checkpoint_ns, checkpoint_id)`** (`checkpoint_ns` is live
   subgraph namespacing Rev 2 missed entirely); **checkpoints form a
   tree, not a chain** — one grain identity per checkpoint id,
   supersession *only* for re-put of the same id, `parent_checkpoint_id`
   stored so `parent_config` reconstructs, `list()` = heads in the thread
   ns, never chain-collapsed (chain-collapsing would have destroyed the
   time-travel we claim to strengthen). `put_writes` upserts keyed
   `(thread, ns, checkpoint_id, task_id, idx)` — retries with different
   bytes supersede, never append. `get_next_version` = parse int prefix,
   increment, re-pad to 32 digits + suffix (lexicographic == numeric,
   property-tested). Checkpoint metadata `run_id` lifted to top-level at
   put (`delete_for_runs` via `run_idx`; free upside: `runs_touching`
   joins LangGraph runs out of the box). `copy_thread` specified
   (cross-file copy; payload blobs dedup). DeltaChannel:
   `get_delta_channel_history` implemented, delta/snapshot grains keyed
   `(thread, ns, channel, version)` with a **ranged batch fetch** (one
   facade call per reconstruction), and the O(snapshot_frequency) read
   bound stated, not hidden. Deletes emit erasure receipts. Pin
   `langgraph-checkpoint >=4.2,<5`, upstream suite in CI, weekly canary,
   compatibility-matrix across last N minors.
   **[R3] Deployment mapping pinned:** one LangGraph thread = one Areev
   memory file (their thread = our isolation unit — invariant #5 landing
   exactly where it should; `delete_thread` = erase one file, the
   cleanest possible erasure demo). Engineering the costs explicitly:
   process-global LRU handle cache with close-on-evict against the
   open-path registry; the cross-thread `AreevStore` is its own shared
   file; bindings must release the GIL during facade calls (areev-py
   does — stated as a requirement); single-file mode remains the
   documented dev/single-user configuration; Pg mode documented as
   schema-serialized.
3. `AreevTraceMirror` — **[R3] surface pinned:** a sync
   `BaseCallbackHandler` attached via config, enqueue-only on the app's
   thread, one worker thread owning the Areev handle (satisfying
   single-writer); callback-shape churn added to the compatibility
   matrix. **Mode matrix [R3]:** `best-effort` (bounded queue,
   drop-oldest, drop counter — labeled *observability*, dev default) vs
   **`guaranteed`** (backpressure or spill-to-local-WAL drained by the
   worker — never drops). **The Art. 12 / forensics positioning may cite
   only guaranteed mode** — a mirror that sheds audit events under load
   cannot back a lifecycle-logging pitch, counter or no counter.
4. `areev migrate --from langgraph` — decodes json/msgpack-typed payloads;
   **pickled payloads are skip-and-report** (per-thread counts, source
   untouched) — pickle is not portably readable and we say so up front.

**`areev-crewai`:**

1. `AreevStorageBackend` — update→supersession, delete→tombstone,
   `crw:id:<uuid>` Facts mapping record ids to heads; `search()` consumes
   CrewAI's pre-computed embedding via the new `nearest_vector` (re-
   embedding with a different model would be silently wrong similarity —
   the Rev-2 design would have done exactly that); dim recorded in meta,
   mismatch = `VAL`; `source` → subject identity (FORGET SUBJECT over
   CrewAI memories, with receipt); `private` enforced store-side.
   **[R3] Heads-only read semantics pinned for every read method**
   (`search`/`list_records`/`count`/`get_record`/`get_scope_info`/
   `list_scopes`) — ConsolidationFlow LLM-rewrites records on save, so
   chains deepen continuously; growth routes to retention/archive.
   **Predicate deletes** (`delete(filter…)`, **and `reset(scope)` — the
   review caught that scope is a predicate too**): enumerate heads,
   tombstone each by hash under a `delete` grant, one audit Observation
   per sweep — CAL's invariant untouched; the cost stated honestly
   (reset of 100k records = 100k tombstones, minutes not ms, and the
   file does not shrink; per-memory crypto-erasure cannot substitute
   because the key is per-memory, not per-scope; sharding top scopes
   into files is the documented alternative).
2. `AreevAuditListener` — **[R3] schema-agnostic by construction**:
   unknown event types persist as generic Observations (their
   twice-weekly releases structurally cannot break the sidecar); same
   best-effort/guaranteed mode matrix as the mirror.
3. `AreevKnowledgeSource` (read-side) + grant-backed
   `PRE_TOOL_CALL`/`PRE_MODEL_CALL` hooks (denials journaled).
   **[R3] The ≤1.9.x legacy `Storage` shim is cut** — building for the
   deleted API of a vendor that removed its entire memory system in a
   minor release is adapter surface without a buyer.

**Hermes:** publish `examples/hermes/areev`; CI smoke via its
`test_provider.py`.

**[R3] Wave-3 gate additions:** the **design-partner deployment profile**
is published (token auth + TLS-terminating proxy + the multi-principal
credential map + Postgres backend for multi-tenant — sufficient for
design-partner security reviews; OIDC pulls forward only when a signed
partner requires it); encoding/id-mapping property tests; upstream suites
at pins; the FORGET-SUBJECT demo; mirror chaos test (kill host
mid-stream: best-effort shows bounded loss with an accurate counter,
guaranteed shows zero loss).

### Wave 4 — loop upgrades + the code pipeline

Run-outcome analyzer; execution-verified skills; shadow evaluation over
journals (zero effect dispatches, asserted); cost attribution feeding
budget_pressure; `areev eval`; **[R3] the §7.4 code pipeline**, including
the `OmsSubstrate` blob seam (capability-gated `put_blob`/`get_blob`
methods — the loop can't carry or dereference candidate code today), a
new code-revision `ActionKind` (append-only enum), Rule E1 enforcement,
`areev tool provenance`, and the canary-promotion design doc.

**Gate:** end-to-end governed code change — proposed by the loop, human-
reviewed with diff, evalset-gated (edge recorded), applied, outcome-
measured, regressed, auto-revert-proposed **with blast-radius report** —
demoable. Shadow evaluator provably dispatches zero effects.

### Wave 5 — surfaces & platform parity

`areev run` CLI verbs (`run/resume/respond/fork/cancel/list/inspect/
verify`), MCP run tools, Python/Node runtime bindings (JS closes its
deviation; binding-level streaming names the `set_embedder` callback seam
as its precedent), console run viewer + time-travel UI + **HITL approval
queue — which requires the multi-principal credential map; shared-token
approvals are refused for `run.respond` and recommendation review**
[R3 — a shared token voids the approver-identity edge of the provenance
chain], OTel export, **[R3] `areev run oversight-report [--plan <hash>]`**
— Client-gated nodes, authorized responders, expiry/budget config,
measured kill-switch time; the Art. 14 row of `docs/eu-ai-act.md`,
converting a regulatory obligation into a demo. **[R3] Hub triggers/crons
struck from this wave** — open question resolved in favor of CLI/cron-
first; a hub scheduler needs its own design doc and contradicts the
no-background-sweepers exclusion.

**Gate:** cross-surface parity; one workflow authored once — run from
CLI, resumed from Python, approved in the console by a second principal,
traced over MCP, verified.

### Wave 6 — the enterprise plane

RBAC roles → TLS → SSO in the enterprise proposal's order; SCIM tracked;
kill-switch drill measured against the <5-minute clause **for
epoch-interruptible tiers**; audit export + hash-chain story in
procurement language; re-acceptance harness (`areev eval` after model
swap, tolerance bands); `docs/eu-ai-act.md` (Art. 12 retention floors +
legal hold are Wave-1 features it documents; the eu-ai-act deployment
profile declares run namespaces with a ≥6-month floor by default).
Certifications remain a hosted-offering decision outside this plan.

**Gate:** the procurement table, every row green or explicitly
N/A-with-rationale.

---

## 9. The check matrix

Unchanged rows from Rev 2 (workflow-validation fuzz "never panics, never
silently accepts"; condition fuzz; scheduler transition tests; golden
journal fixtures with bless; clippy/BTreeMap lints; reducer laws; journal
key-uniqueness property — **[R3] now generating Sends and multi-call
turns, or it proves the wrong key**; budget recompute == live; HITL
by-id; authz; error-code registry; streaming with/without/slow
subscriber; ToolCallLlm fixtures; adapter property tests + upstream
suites + chaos; cross-surface playbook; conformance on both backends;
`--release` CI for timing-adjacent tests; per-superstep perf gate).
Additions [R3]:

| Layer | Check |
|---|---|
| Simulation | `areev-run-sim`: seed-reproducible DST harness (virtual clock, adversarial scheduler, programmable executor latency/failure/crash) exploring thousands of schedules per CI run against the §5 invariants; failures replay from seed |
| Formal | TLA+/Quint model of the **intent/result/crash/redelivery/checkpoint/resume core only** (where the marketing claim lives), model-checked **in parallel, non-blocking** — the DST harness is the gate; the spec publishes when green. Graph semantics (generations, SCC death, joins) verified instead by exhaustive enumeration over all ≤5-node graphs in a deterministic Rust test — a full-scheduler TLA+ model would lag the code and rot |
| Purity | `cargo tree -p areev-run-core` CI ban-list; crate-local clippy.toml; grep-reject `allow(disallowed_methods)` |
| Supersession | step_action/run_trace tests WITH supersession (the shape the existing test explicitly avoids); intent+result fold via supersession_map on every surface |
| Pagination | resume correctness past 1024 journal grains |
| Erasure | run-aware truncation; receipt-driven hole reports; tombstoned-vs-absent distinction; erased-run spend exemption |
| Retention | floor + hold refusal tests (`RUN-E017`); run-atomic sweep; fork-refcount blocks sweep |
| Ownership | copy-resume requires `--fork`; forked journal tips → `Tainted` |
| Code gate | all four §7.2 conditions individually refusable (`RUN-E018` names which); capability-superset dispatch refused; registry never populated from file content; auto-apply exclusion pinning test |
| Wasm | module-size/compile-limit/stack-cap tests; epoch-interrupt cancel under a spinning module; RUSTSEC advisory gate on wasmtime |
| Secrets | `$secret` refs journal-stable; deleted secret → loud redelivery failure; lossy redactor on results only, version-pinned, fail-closed |

Recurring repo bug shapes, checked by name: silent field drops;
CI-green-but-wrong authz; release-only timing collapse; occurrence-vs-
value confusion; **[R3] the supersession-hides-the-link shape** (this
review's blocker — now a named test family); assert mechanism, never
count.

---

## 10. Positioning discipline

- Headline per §1, with its stated reach. Never orchestration breadth.
- The glossary rule (§5) binds all collateral: "exactly-once" modifies
  only "journaling."
- Self-improvement is stated with its gates — and now with Rule E1, which
  is itself a differentiator: *the gate cannot be weakened by the thing it
  gates* is a sentence no other self-improvement vendor can say.
- Verify is pitched by its three labeled tiers — "journal-consistent /
  re-execution-proven / not-verified" — because a claim that grades
  itself honestly survives hostile probing, and the market finding is
  that buyers are trained to catch anything else.
- Against Anthropic's managed memory: same guarantees — embedded,
  portable, model-agnostic, queryable, auditable by you. Against
  LangChain+Temporal+LangSmith: the seam between their three products is
  our product. OpenAI's Agent Builder/Evals shutdown remains the hosted-
  platform cautionary tale.

## 11. Deliberate deviations & exclusions

JS runtime bindings → Wave 5 (stated). No distributed execution. No
expression language beyond §6.4. No process sandboxing outside Tier C
(Tier B is *named* unsandboxed). No background sweepers in v1 (HITL
expiry lazy; hub crons deferred with their own design doc). No TTL
emulation in the LangGraph store (`supports_ttl=False`, honest no). No
per-node error handlers in v1 (fail-fast only; `Done(failed)` = drain).
**[R3] No CrewAI ≤1.9 legacy shim. No browser code execution in v1
(client = areev process/edge runtime). No two-way git mirror, by
construction. No auto-apply for code, ever.** No hosted control plane in
this program. No new CAL syntax; no core format changes.

## 12. Risks

1. **Scope** — waves ship independently; Wave 3 parallel from Wave 0;
   pre-agreed de-scope order (hub scheduler first — already cut, subgraph
   streaming granularity second); Wave-1 internal midpoint (spine on
   linear graphs) to measure velocity.
2. **Checkpointer treadmill** — pins + upstream suite + weekly canary +
   compatibility matrix + the shallow latest-only tier as a fallback;
   optionally contribute a conformance suite upstream and become the
   reference implementation.
3. **The window** — the trace mirror + audit listener ship first and
   occupy the seam publicly in weeks; engage the Portable-Agent-Memory
   authors on OMS alignment; design partners on Wave 3. Portability +
   self-hosting + model-agnosticism survive even if Anthropic ships
   queries.
4. **Determinism** — enforced six ways (sans-IO core, DST simulator,
   three CI gates, lints, journaled decisions, reducer laws); audited
   never, enforced always.
5. **CrewAI churn** — thin adapter, pinned mappings, schema-agnostic
   listener, compatibility matrix.
6. **Code execution is a new attack surface** — default-off (D8),
   host-rooted trust (§7.1), capability-bound approvals (§7.2), honest
   tier naming (§7.3), Rule E1 (§7.4). The residual risk is stated: until
   import signing, journal authenticity is trust-as-recorded (§5.5 tier
   3).
7. **Estimate honesty** — the runtime is from-scratch; ToolCallLlm
   streaming is the Wave-0 long pole; the §5–§7 pinning converts
   exploratory work to specified work, and the DST/spec work runs
   parallel, not blocking.

## 13. Decisions — **all six ACCEPTED 2026-08-14** (D7 confirmed: standalone)

- **D5 — run verbs.** Add `run.execute` / `run.respond` / `run.cancel` to
  `authz::ALL` before Wave 1; execute + respond at Control tier, cancel
  deliberately low-tier. *Recommend: accept now — verb vocabulary is
  append-only; retrofitting attribution is worse.*
- **D6 — executable tool grains.** Ship §7 (tier table, four-condition
  gate, Wasm as the blessed portable format). *Recommend: accept.*
- **D7 — sandbox placement.** `areev-sandbox` as a **standalone
  package** (the areev-js pattern) invoked as a subprocess — keeps
  wasmtime's ~200-crate tree out of workspace MSRV/deny/test time; an
  in-process feature can come later. *Recommend: standalone subprocess.*
- **D8 — code execution default.** Default-OFF as shipped
  (`--allow-code-exec` opt-in, never enabled by file content).
  *Recommend: accept.*
- **D9 — redaction posture.** v1: referential secrets as the primary
  control; no built-in redactor; manifest carries explicit
  `redaction: none | <id>`; configured redactors are results-only,
  version-pinned, fail-closed. *Recommend: accept.*
- **D10 — legal hold vs erasure.** `FORGET SUBJECT` against a held
  namespace proceeds only with `--override-hold`, the hold named in the
  audit Observation (Art. 17(3) makes precedence the controller's call —
  explicit, never silent). *Recommend: accept.*

Open questions: adapter package names on PyPI (`areev-langgraph`,
`areev-crewai`) — confirm before reserving; hosted offering +
certifications — revisit after Wave-3 partner signal; OMS amendment for
the runtime conventions (checkpoint/journal/step_action/condition
grammar) — propose after Wave 2, when the semantics have survived the
permutation gate and the published TLA+ spec can accompany it.
