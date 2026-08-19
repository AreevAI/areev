# `areev run` — an agent runtime whose execution history is memory

**Status:** proposal, **partially superseded — and now BUILT**. Written
2026-08-07, after the graph-engineering audit (PR #56) landed the substrate this
would sit on. **2026-08-19:** the runtime shipped — `areev-run-core` (the pure
scheduler) and `areev-run` (the driver) are in the workspace, with the CLI
`areev run …` verbs, the six MCP `areev_run_*` tools, and the console Runs tab.
`docs/run.md` is the reference; this file is kept for the design rationale, not
as a statement of what exists. **2026-08-14:** the phasing here (§7) and the two open decisions
(§10) are superseded by
[`areev-governed-agents-proposal.md`](areev-governed-agents-proposal.md), which
records the decided answers (LangGraph-parity bar; Bet B on Hermes; the §8
checkpointer exclusion deliberately reversed) — the architectural rules (§3,
§4, §6) survive unchanged there.

> **Read [`areev-adaptive-agents-proposal.md`](areev-adaptive-agents-proposal.md)
> (2026-08-11) first.** A substrate audit against the code corrected three things
> here: §2's "mostly assembly" holds for persistence but not for execution (five
> of its cells are inert scaffolding with no callers anywhere in the workspace);
> §6.2's "`areev-llm` plugs in here" is wrong, since that crate cannot carry a
> `tools` array or parse `tool_calls` and needs a new trait; and §7's Phase-1
> gate as worded passes vacuously — the replaced gate is effect suppression plus
> decision equivalence plus journal immutability. §4's recommendation of
> deterministic replay survives and is now load-bearing for more than this
> document. The runtime also moves later in the plan: replay, not the runtime,
> is the keystone.

---

## 1. The thesis

Not "another orchestrator." There are a dozen good ones — LangGraph, Temporal,
Restate, Inngest, Hatchet, DBOS, Cloudflare Workflows — and competing on
orchestration features is a losing frame.

The claim worth making is narrower and, as far as the August 2026 survey found,
unoccupied:

> **The agent runtime whose execution history *is* queryable memory.** The plan,
> the runs, the checkpoints and the facts the runs produced live in one portable
> file, and you can query across them.

Every other stack splits execution state from semantic memory — a checkpointer
holds in-thread state, a memory store holds cross-thread facts — and nothing
queries across the seam. `run_trace` / `run_yield` / `runs_touching` (PR #56)
already cross it. `areev run` is the thing that produces the data worth crossing.

**Honest caveat, carried from the research:** the niche is unoccupied because the
split is an *annoyance*, not a blocker. Teams hand-roll around it. This is a
differentiator, not an urgent pain — plan adoption accordingly.

---

## 2. Why this is now mostly assembly

PR #56 landed the substrate. What a runtime needs to persist already exists:

| Need | What exists | Where |
|---|---|---|
| The plan | Workflow `0x04` — real DAG: nodes, typed edges with `cond` + `max_cycles`, per-node tool `bindings`, `retries`, `trigger` | `areev-core/src/types/workflow.rs` |
| Graph semantics | OMS §8.4: entry = first node; fan-out = multiple unconditional out-edges; **AND-join** = multiple in-edges; decision point = multiple conditional out-edges; terminal = no out-edges | spec, normative |
| Tool binding | Three-tier resolution: **Bound** (hash) → **Named** (by `tool_name`) → **Abstract** (label is an instruction an LLM interprets) | spec §8.4 |
| The call lifecycle | Tool grain: `definition` / `call` / `result` phases, `ExecutionStatus`, `FailureCause`, `tool_call_id`, `call_batch_id`, `correlation_id` | `types/tool.rs` |
| Who executes | `ExecutorKind::{Host, Client}` | `types/executor_kind.rs` |
| Run ↔ plan link | `mg:step_action:<node_id>` execution records — runs point at the plan, never mutate it | PR #56 |
| The checkpoint | State `0x03` with `context` + `plan` + `history` | PR #56 |
| Correlation | `run_id`, queryable and indexed | PR #56 |
| The join | `run_trace` / `run_yield` / `runs_touching` | PR #56 |
| LLM providers | OpenAI-compatible / Anthropic / Ollama over a blocking client | `areev-llm` |
| Tools → model | Tool definitions rendered to **9 provider formats** | `areev-core/format/tool_schema/` |
| Context per step | CAL + `ASSEMBLE` with budgets and priorities | `areev-cal`, `areev-context` |

**There is even a designed-but-unimplemented hook.** `types/tool.rs:223` says
`ExecutorKind::Client` bindings *"cause the Flow-A loop to pause and return a
`requires_action` envelope to the caller."* That loop exists nowhere in the
repo. **`areev run` is that loop.**

---

## 3. The architectural rule (non-negotiable)

`areev run` is a **separate crate that is a host over Areev**. Execution never
moves into the engine.

`ARCHITECTURE.md` states it: *"Tool grains are data, never executables… execution
is always the host's job."* That rule is why Areev is embeddable and why its
security story is simple. Breaking it to save a layer would be a bad trade.

```
areev-core ← areev-store ← areev-cal ← areev-context
                                   ↑
                             areev-run  (new — a host, like areev-mcp)
                                   ↑
                              areev run   (CLI verb)
```

Nothing under `areev-*` may depend on it. It composes existing crates the same
way `areev-mcp` and `areev-server` do.

---

## 4. The one fundamental choice: the replay model

**This decides everything else, so decide it first.**

| | **A — resume-from-snapshot** | **B — deterministic replay** |
|---|---|---|
| Model | Restore State, re-execute forward | Journal every effect's *result*; replay returns recorded results |
| Precedent | LangGraph | Temporal |
| On resume | LLM/API calls fire **again**, may differ | Results are returned from the journal; no re-execution |
| Cost | Cheap | The loop must be a pure function of `(input, journal)` — no wall-clock, no randomness outside journaled steps |

### Recommendation: **B**, and it is the differentiator

The hard part of Temporal's model is a durable, ordered, immutable journal of
every effect's result. **We already have it**: every tool result is an immutable,
content-addressed Tool grain, ordered by the op-log, linked to the plan by
`mg:step_action` and to the run by `run_id`.

LangGraph explicitly does *not* offer this — its "time travel" is
resume-from-snapshot and re-executes forward non-deterministically. Temporal
offers it but deliberately keeps the transcript *out* of event history (50 MB /
51,200-event cap), so the semantic payload lives somewhere else and can't be
queried with the run.

Doing both — deterministic replay **and** the transcript as queryable memory —
is the position nobody holds.

**What it costs:** discipline, enforced by tests. The orchestrator may not read
the clock or generate randomness outside a journaled step. That needs a
replay-equivalence gate (§7) or the claim rots.

---

## 5. Scope

### v1 MUST — the spine

1. Load a Workflow grain; resolve the entry node (first element of `nodes`).
2. Walk edges per §8.4: sequential, parallel fan-out, AND-join, conditional
   branch, bounded cycles.
3. Resolve each node's tool: Bound → Named → Abstract.
4. Dispatch: `Host` executes via a pluggable executor; `Client` pauses and
   returns a `requires_action` envelope.
5. Journal: a Tool grain per call and result, linked by `mg:step_action:<node>`,
   stamped with `run_id`.
6. Checkpoint: a State grain per superstep (`context` = accumulated state,
   `plan` = remaining nodes, `history` = prior steps).
7. Resume: from a `run_id` or a checkpoint hash, replaying journaled results.
8. Terminate: terminal node, unrecoverable error, or budget exhaustion.

### v1 MUST NOT

- Distributed execution, workers, queues — single process.
- Streaming, sub-workflows, dynamic graph mutation.
- **No new CAL syntax** (OMS conformance decision).
- **No changes to `areev-core`.** If v1 needs a format change, the design is wrong.
- No `BaseCheckpointSaver` implementation (see §8).

---

## 6. Design decisions, with recommendations

**6.1 Condition evaluation.** `WorkflowEdge.cond` is an *opaque* string per spec
— no grammar, deliberately host-defined. Recommend a `ConditionEvaluator` trait
with a deliberately small built-in over the accumulated State (`key == value`,
`key exists`, truthiness) plus a host escape hatch. **Do not invent an expression
language in v1** — that is a spec-shaped decision and a permanent maintenance
surface.

**6.2 Abstract nodes are the agent-ness.** A node with no binding is "a
human-readable instruction; executor (LLM or agent) interprets it." That is where
`areev-llm` plugs in and where `tool_schema`'s nine provider renderings earn
their keep: present the workflow's bound tools to the model in its native
format, let it choose, journal the choice as a Tool grain. A workflow of only
bound nodes is a pipeline; one with abstract nodes is an agent. Same runtime.

**6.3 Concurrency vs. single-writer.** Areev is **single-writer-per-file**.
Recommend: parallel *tool execution* on a bounded pool, but **one owning thread
for all writes**. Fan-out is concurrent; journaling is serialized. This is a real
correctness risk and should be designed explicitly, not discovered.

**6.4 Retries and cycles.** `Workflow.retries` (node → max attempts) and
`WorkflowEdge.max_cycles` (back-edge bound) both already parse and round-trip.
Honor both. Each attempt is its own `step_action` record — already tested
(`repeated_attempts_at_one_node_all_survive`). Note `max_cycles` is currently
reachable only through the JSON path, not CAL's `* N`.

**6.5 Human-in-the-loop is already modeled.** `ExecutorKind::Client` = pause and
ask. No new concept needed — implement the envelope `tool.rs` already describes.

**6.6 Governance seam (not v1).** Areev Loop's four gates could gate a run's writes,
and `runs_touching` already makes "what did this run change" answerable. Design
the seam; build it later.

---

## 7. Phasing

| Phase | Deliverable | Gate |
|---|---|---|
| **0 — Decide** | Replay model, Hermes bet, name. No code. | This doc, agreed |
| **1 — Spine** | Linear workflow, host tools, journal + checkpoint + resume | **Replay-equivalence test**: a run resumed from a checkpoint produces byte-identical journal grains to the original |
| **2 — Graph** | Fan-out, AND-join, conditionals, retries, bounded cycles | A diamond and a bounded cycle execute correctly; concurrency does not violate single-writer |
| **3 — Agent** | Abstract nodes via `areev-llm` + `tool_schema` | An abstract node picks and calls a bound tool; the choice is journaled |
| **4 — HITL** | `ExecutorKind::Client` pause/resume envelope | A paused run resumes days later from the file alone |
| **5 — Surfaces** | `areev run` CLI, MCP tool, Python/Node | Cross-surface parity per the `areev-add-operation` playbook |

Phase 1's gate is the important one: it is what makes the determinism claim
true, and it must be a CI gate, not a one-off check.

---

## 8. What we deliberately will not do

- **No execution inside `areev-core`.** §3.
- **No `BaseCheckpointSaver` implementation.** Its real surface is 12 sync + 10
  async methods (the published docs say four), it is actively moving
  (`DeltaChannel` is beta with an explicit "custom implementations must be
  DeltaChannel-aware" warning), and `prune(strategy="delete")` /
  `delete_for_runs` / `delete_thread` require genuine deletion, which fights an
  immutable content-addressed store. Being a LangGraph backend means competing
  with Postgres on Postgres's terms.
- **Not a distributed workflow engine.** Temporal exists and is very good. One
  process, one file.

---

## 9. Risks

1. **Crowded space.** Mitigate by never competing on orchestration features —
   the memory-native angle is the whole pitch.
2. **Determinism is a strong claim and easy to break.** One `SystemTime::now()`
   in the loop silently invalidates it. Mitigate with the Phase 1 CI gate.
3. **Scope creep.** An agent runtime's surface is unbounded (cancellation,
   timeouts, budgets, streaming, sub-graphs, observability). The v1 MUST-NOT list
   is the defense.
4. **Concurrency × single-writer.** §6.3. Design first.
5. **Slow adoption.** The differentiator is an annoyance, not a blocker.

---

## 10. Two decisions needed before Phase 1

**10.1 The Hermes bet — this sets the bar for v1.**

- **Bet A — Areev stays the memory layer *under* Hermes.** `areev run` is small
  and aimed at people not already on a framework. Low risk, low ceiling, and the
  existing `MemoryProvider` integration keeps paying.
- **Bet B — `areev run` replaces Hermes in our own stack.** Bigger build, but we
  own the loop end-to-end and the memory-native story becomes demonstrable
  rather than architectural.

These imply different v1 completeness bars. Decide before building.

**10.2 The name.**

- **`areev run`** (CLI verb) + crate **`areev-run`** — consistent with the
  workspace's `areev-*` convention and with `areev hub` / `areev serve`.
  Describes what it does: runs a defined workflow. **Recommended.**
- **`areev-agent`** — overpromises autonomy for v1, and collides conceptually
  with Areev Loop, which is already the self-improving-agent story. Reads like a
  second agent product.

Recommendation: crate `areev-run`, CLI `areev run`. Describe the *product* as an
agent runtime; keep the *artifact* named for what it does.
