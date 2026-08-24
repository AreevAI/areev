---
name: areev-agent-workflows
description: Playbook for the agent-workflow graph lifecycle — how a Workflow plan grain becomes the execution graph `areev run` walks, and the full author → run → history → improve → apply/revert cycle around it (CAL graph syntax, run journal + step_action history, loop findings per plan hash, supersession-based plan evolution). Use when building or operating agents on Areev's workflow runtime, authoring/editing plan grains, wiring run history queries, or evolving a plan through the loop's propose→approve→apply→revert gates — the plan is content-addressed, so "editing" it mints a new hash and everything that points at the old one (triggers above all) must be walked forward deliberately.
---

# Agent workflows — the plan graph and its lifecycle

A plan is a **Workflow grain** (`areev-core/src/types/workflow.rs`), and the
whole runtime treats it as immutable input: the execution graph is *derived*
from it at run start, run history *points back at it* via links, and every
"edit" is a supersession that mints a **new content address**. Keep those
three facts in view and the rest of the lifecycle follows.

## 1. The data model — what the grain carries

```
nodes:    Vec<String>              // ids double as labels; nodes[0] IS the entry (OMS §8.4)
edges:    Vec<WorkflowEdge>        // { src, dst, cond: Option<String>, max_cycles: Option<u32> }
bindings: HashMap<node, hash>      // node → Tool Definition (or another Workflow = subgraph)
retries:  HashMap<node, u32>       // n = re-attempts AFTER the first failure
+ extra fields: name (human label), reducers {state_key: lww|append|sum|max|min}
```

Semantics that are easy to miss:

- **Edge declaration order is load-bearing** — it is the canonical
  evaluation order (§6.2). Reordering edges in an otherwise-identical plan
  changes behavior *and* the content address.
- **`max_cycles` is the loop primitive**: cycles are legal, but every cycle
  must carry at least one bounded edge or the plan is refused (`RUN-E002`).
  The re-entry point can be any node in the loop, not just the entry.
- The grain has **no trigger field and no run state**. Triggers are their own
  grains pointing AT the plan (`areev trigger add --workflow <hash>`); runs
  point at it via `mg:step_action:<node>` links. Both directions are forced
  by content addressing — a plan that accumulated either would change hash.

## 2. Grain → execution graph (two stages, pure then store-bound)

**Stage 1 — `PlanGraph::build`** (`areev-run-core/src/plan.rs`, pure):
validates V1–V6 and indexes. Node ids collapse to positions; every
per-node/per-edge fact becomes a position-indexed `Vec` (never a HashMap —
replay determinism). One Tarjan pass computes SCCs *and* classifies DFS
back-edges (`cycle_edge`), which generation-0 AND-joins exclude so cycles
can re-enter mid-graph. Validation is errors, never warnings:

| Refusal | Code |
|---|---|
| shape: empty/dup/oversized node ids, edge → unknown node, bindings/retries naming unknown nodes | `RUN-E019` |
| unbounded cycle (an SCC with no `max_cycles` edge inside it) | `RUN-E002` |
| node unreachable from `nodes[0]` | `RUN-E003` |
| condition fails the frozen v1 grammar (`==`/`!=`/`exists`/truthy, no coercion) | `RUN-E005` |

**Stage 2 — `RunManifest::resolve`** (`areev-run/src/manifest.rs`, V3/V7):
freezes each node into a `PinnedTool` at run start — the **resolution
freeze**, so a plan (or a tool head) superseded mid-run cannot change a
running run. Four executor shapes fall out of what a node binds:

- binding → Tool Definition ⇒ **host** (via `--tool-cmd`, or code-carrying:
  `executor_uri: "cas://sha256:…"` names the code blob by content address,
  runnable only under a host-side `--allow-executor` pin, optionally
  sandboxed via `runtime: "wasm32-areev[-io]"` — grain code is the ONLY
  form the loop's `code_revision`/Rule E1 pipeline can govern) or
  **client** (`executor_kind: "client"` — a human gate, the run parks for
  `respond`);
- binding → another Workflow grain ⇒ **subgraph** (inline child run,
  deterministic child id);
- no binding but a Definition named like the node ⇒ that definition;
- neither ⇒ **abstract** — the node label itself is the LLM instruction
  (needs a configured backend, else `RUN-E006`).

`reducers` is validated here too: an unknown reducer name fails at run
start, not at first merge.

## 3. Authoring surfaces — and what each one can express

**CAL** (the paved road for DAG-shaped plans):

```sql
ADD workflow "refund pipeline"
    fetch -> approve WHEN "order.found == true"
    approve -> refund WHEN "approved == true" * 2
    BIND fetch = sha256:<tool-hash>
    BIND approve = sha256:<human-gate-tool-hash>
    REASON "governed refund flow"
```

Grammar: arrow chains, `(a, b)` groups for fan-out/fan-in, `WHEN "<cond>"`
per edge, `* N` on an edge = **retries for the destination node** (largest
wins when several edges share a destination) — **not** `max_cycles`.
`SUPERSEDE sha256:<plan-hash> <graph...> BIND ... REASON "..."` is the
matching evolve form. `ON "..."` was removed in 1.3 — declare a Trigger
grain instead.

**Generic add (JSON)** — bindings' `db.add("workflow", json)`, MCP
`areev_add`, Rust builders — is the only surface that authors **everything**:
`max_cycles` (bounded loops), `reducers`, `retries` directly. Canonical
example: `docs/run.md` "Authoring a plan".

**Console Workflows tab** — node/edge editing over `/api/cal`; it draws
triggers and run status but structurally cannot save them into the plan.
Plans with bounded cycles or retries open **view-only** there (no surface
syntax for either), and closing a cycle in an editable plan is refused
rather than saved unbounded.

## 4. Running

Same runtime on every surface — CLI `areev run
start/resume/respond/cancel/list/inspect/verify/fork/shadow/oversight-report`,
the MCP `areev_run_*` tools, bindings `run_start(...)` etc. Three grant
verbs: `run.execute`, `run.respond` (per-principal — the approver's identity
is the audit record), `run.cancel`. `verify` re-drives the run from the
manifest writing nothing and byte-compares checkpoints; `fork` is time
travel (same plan) or migration (new plan hash, full re-validation, graph
restarts at entry with inherited context).

## 5. History — everything points at the plan

- **Journal**: intent = Tool grain (`status=Pending`) written before
  dispatch; result = its **supersession re-stating every identity field**
  (gotcha: a Tool's content deserializes as `tool_content`, not `content`).
  Each carries the `mg:step_action:<node>` link → the plan hash.
- **Plan version chain**: CAL `HISTORY sha256:<plan-hash>` — supersession
  keeps every prior graph readable.
- **Joins**: `areev run-trace <run-id>` (run → memory it touched),
  `areev runs-touching --hash <grain>` (grain → runs that produced it),
  `areev step-actions` (node → its execution grains), `areev tool
  provenance <hash>` (governed code → the loop recs and runs around it).
- **Checkpoints**: State grains chaining scheduler state + decision records
  — what `verify` compares against.

## 6. Improving the graph — the governed cycle

The loop **flags**, a human **decides**, supersession **applies**,
`outcome_review` **measures and proposes the revert**:

1. `areev loop run` — the `run_outcome` analyzer reads run-terminal
   Observations, aggregates per `plan_hash`, and emits **`Flag`
   recommendations** (failure clusters, cost attribution) with the run
   grains as evidence. Changing a workflow is deliberately never
   auto-applied — it is always a human/host decision.
2. Review: `areev loop list / show <hash>`, then `areev loop approve|reject
   <hash> --because "..."`. Separation of duties applies (the writer
   can't approve their own), and every decision writes a reasoned audit
   Observation.
3. Apply the graph change itself with **`SUPERSEDE workflow`** (CAL) or a
   JSON supersede — this mints a **new plan hash**. Then walk the
   references forward:
   - **Triggers point at the old hash and do not follow heads** — the
     evaluator starts exactly the hash in the trigger's `workflow` field.
     Supersede each trigger to the new hash or the old plan keeps running.
   - In-flight runs are untouched (manifest pinned the old hash) — that's
     correct, let them drain.
   - Re-run affected saved queries/dashboards that filter on the hash.
4. `outcome_review` re-measures the stored metric after `review_after` and
   **proposes a revert on regression**. For code-carrying changes
   (`code_revision`), Rule E1 applies: the rec pins its evalset and `areev
   loop apply <rec> --gating-run <eval-run-id>` is the only admitted path.

## 7. Reverting

- CAL `REVERT` **parses but is not executable** (the executor returns
  `Unsupported`) — do not reach for it. The real revert of a plan change is
  another `SUPERSEDE workflow` restating the prior graph (read it back with
  `HISTORY`); nothing is lost either way, supersession is append-only.
- Loop-applied recommendations record their inverse at apply; regression
  reverts arrive as new recommendations through the same review gate.

## Gotchas checklist

- A plan edit = new content address. Sweep: triggers (re-point), docs/
  examples quoting the hash, any `bindings` in OTHER plans that subgraph
  this one.
- Edge order and `retries`-vs-`max_cycles` confusion are the two silent
  behavior changes; `* N` in CAL is retries.
- `to_workflow()` deliberately **skips** malformed edges from foreign OMS
  files instead of failing — a plan that "lost" edges on import validates
  differently, it doesn't error.
- Record tool calls with `record_tool_call`, never raw `ADD tool` —
  content addressing collapses identical retries into one grain and
  starves the loop's evidence.
- Console + `areev run` read the same grain but build different graphs:
  the scheduler treats `max_cycles`/edge-order as semantics, the canvas
  ranks only forward edges for layout. Don't "fix" one against the other.
