# `areev run` — the governed workflow runtime

Every agent framework executes graphs. Almost none can *prove* an execution
afterwards: crash recovery duplicates side effects, "who approved this?" is a
chat search, and "what did the agent actually do?" is a log grep. `areev run`
executes a **Workflow grain** as a governed run whose entire history lives in
the same memory file as everything else Areev stores — journaled before it
happens, checkpointed as it happens, replayable after it happens, and gated
by humans where you say so.

The design rests on three moves:

1. **Intent before dispatch.** Before any effect fires, a Pending Tool grain
   is written carrying the effect's full identity. The result *supersedes*
   that intent. A crash between the two is therefore visible on resume, and
   the effect is re-delivered under the **same idempotency key** — journaled
   as a redelivery, never minted as a duplicate. Exactly-once across
   interrupts; at-least-once across crashes, with the key your tool needs to
   deduplicate.
2. **A pure scheduler.** The step function (`areev-run-core`) is sans-IO —
   no clock, no randomness, no store access in its dependency tree
   (CI-enforced). Every decision it makes is a function of the journal, which
   is what makes the third move possible:
3. **Verification, not trust.** `areev run verify` re-derives every checkpoint
   from the journaled events and byte-compares against the stored chain,
   writing nothing. If anyone edited history — or if a nondeterminism bug
   crept in — verify names the checkpoint and the differing fields.

## The 10-minute proof (no LLM, no waiting)

```bash
areev run demo --db runs.db      # seeds a 2-node plan: host tool → human approval
```

The demo prints the workflow hash (plans are content-addressed grains — the
same plan is the same hash, forever). Start a run:

```bash
areev run start --db runs.db --workflow <WF_HASH> --run-id demo-1 \
  --input '{"who":"world"}' --tool-cmd 'printf '\''{"greeting":"hello"}'\'''
```

The host tool executes, then the run **parks** on the approval node and
prints a `requires_action` envelope:

```json
{"kind":"requires_action","run_id":"demo-1",
 "asks":[{"node":"approve","tool_name":"approve","approval":true,
          "tool_call_id":"<ASK>","input":{"greeting":"hello","who":"world"}}],
 "checkpoint":"…"}
```

Answer it — as a **different** principal, because the principal who triggered
an approval ask structurally cannot answer it — then resume and verify:

```bash
areev run respond --db runs.db --run-id demo-1 --ask <ASK> \
  --result '{"approved":true}' --as user:officer
areev run resume  --db runs.db --run-id demo-1    # → {"finished":"Completed"}
areev run verify  --db runs.db --run-id demo-1    # → {"verified": true, "steps": […]}
```

Responding and resuming are deliberately separate acts: recording a human's
answer must not require holding the run's writer handle open while the human
thinks.

## Authoring a plan

A plan is a **Workflow grain**: nodes, edges, and per-node bindings to Tool
grains. Author it from any surface that adds grains — the bindings' generic
`add`, MCP `areev_add`, or the Rust builders:

```python
import areev, json
db = areev.Areev("runs.db", ns="ops")

fetch   = db.add("tool", json.dumps({"tool_name": "fetch", "kind": "definition",
                                     "tool_description": "fetch the order"}), ns="ops")
approve = db.add("tool", json.dumps({"tool_name": "approve", "kind": "definition",
                                     "tool_description": "human approves the refund",
                                     "executor_kind": "client"}), ns="ops")

wf = db.add("workflow", json.dumps({
    "nodes": ["fetch", "retry_fetch", "approve", "refund"],
    "edges": [
        {"src": "fetch",       "dst": "approve", "cond": "order.found == true"},
        {"src": "fetch",       "dst": "retry_fetch", "cond": "!order.found"},
        {"src": "retry_fetch", "dst": "fetch", "max_cycles": 3},
        {"src": "approve",     "dst": "refund", "cond": "approved == true"},
    ],
    "bindings": {"fetch": fetch, "retry_fetch": fetch, "approve": approve},
    "retries": {"fetch": 2},
    "reducers": {"attempts": "sum"},
}), ns="ops")
```

What each piece means:

- **`bindings`** — node → Tool grain hash, pinned at *run start* into the
  run's manifest (resolution freeze: a plan can't change out from under a
  running run). Three executor shapes fall out of what you bind:
  - a **host tool** (the default) executes through your `--tool-cmd`;
  - a Tool with **`executor_kind: "client"`** is a human gate — the run
    parks and waits for `respond`;
  - a binding to another **Workflow grain** is a **subgraph** — it executes
    inline as a child run with its own journal (child run id is
    deterministic: `parent~<tool_call_id prefix>`).
  - an **unbound** node (no binding, no same-named Definition grain) is an
    **abstract node** — a journaled LLM tool-calling loop; see below.
- **`retries`** — `retries: {node: n}` means *n re-attempts* after the
  first failure, per re-entry generation.
- **`max_cycles`** — the loop-authoring primitive. Cycles are legal, but
  every cycle must carry at least one `max_cycles`-bounded edge or the plan
  is refused at load (`RUN-E002` — an unbounded loop is a bug, not a
  feature).
- **`reducers`** — per-state-key merge functions, frozen into the manifest:
  `lww` (the default for undeclared keys), `append`, `sum`, `max`, `min`.
  They are law-tested for batching invariance, which is what makes fan-out
  results order-independent.

Plan validation runs at load: unreachable nodes (`RUN-E003`), unresolvable
bindings (`RUN-E004`), malformed conditions (`RUN-E005`), unbounded cycles
(`RUN-E002`), structural shape (`RUN-E019`).

### The condition grammar (frozen)

Edge conditions evaluate against the run's context (the merged node
results). The v1 grammar is deliberately small and will not grow quietly:

```text
cond    := path op literal | path "exists" | ["!"] path
op      := "==" | "!="
path    := segment ("." segment)*        segment := [A-Za-z0-9_-]+
literal := JSON string | number | true | false | null
```

Strict JSON equality, no coercion (`"1" != 1`). Truthiness: `false`, `null`,
missing, `""`, `0`, `[]`, `{}` are false; everything else is true. Parse
errors are load-time; **evaluation is total** — a missing path is falsey,
never a runtime error.

### The host-tool contract (`--tool-cmd`)

One subprocess seam on every surface (CLI `--tool-cmd`, MCP
`$AREEV_RUN_TOOL_CMD`, bindings `tool_cmd`):

- the command runs via `/bin/sh -c`, once per effect;
- the tool **input JSON arrives on stdin**;
- `AREEV_TOOL_NAME`, `AREEV_TOOL_HASH`, and `AREEV_IDEMPOTENCY_KEY` arrive in
  the environment — the key is what your tool uses to deduplicate a
  crash-window redelivery;
- the **result JSON leaves on stdout**. A non-zero exit or non-JSON stdout
  is a Failed effect (stderr is captured into the failure detail).

Without a tool command configured, host-tool nodes fail loudly rather than
silently — there is no built-in "just run it" executor.

## What a run writes (the journal)

Runs live in the reserved `agent:harness` namespace, as ordinary grains:

| Record | Grain | When |
|---|---|---|
| Intent | Tool grain, `status = pending` | **before** every effect dispatch |
| Result | supersession of the intent, re-stating its identity + usage | when the effect settles |
| Checkpoint | State grain (scheduler state + the superstep's decision record), chained by `derived_from` | every superstep |
| Manifest | the frozen plan resolution, budgets, principal | at start |
| Cancel / audit / redelivery | Facts and Observations | as they happen |

Every journal record carries the full effect identity — run id, task path,
node, attempt, effect sequence, kind — and `tool_call_id` is a digest of that
key, so it is both occurrence-unique and reproducible under replay. Because
results *supersede* intents, "the current state of every effect" is just the
heads, and the full history is one `HISTORY` query away.

Read it back:

```bash
areev run-trace --run-id demo-1           # what the run recorded, and what it produced
areev run inspect --run-id demo-1         # manifest, budgets, phase, spend, pending asks, fork lineage
areev run list                            # recent runs, newest first
areev runs-touching --hash <GRAIN>        # the reverse join: which runs produced/refined this grain
```

## Crash recovery and resume

`areev run resume --run-id ID` picks up from the latest checkpoint. On
resume:

- answered asks settle; expired asks are journaled and fail their node;
- a **dangling intent** (crash between intent and result) is adopted —
  looked up before written, so there is only ever one intent grain per key —
  and re-dispatched under the same idempotency key, with the redelivery
  recorded as an Observation (`on_dangling = fail` turns this into
  `RUN-E008` instead, if your tools can't tolerate redelivery);
- a run whose manifest exists but whose first checkpoint was lost
  reconstructs from the manifest's input.

## Human-in-the-loop, precisely

- Asks are addressed by `tool_call_id`, **never by index** — an index is a
  race with the scheduler.
- An approval ask **structurally refuses** `responder == the principal that
  triggered it`. This is not a policy toggle; it is checked before anything
  else and there is no flag to disable it.
- On governed sessions (grants present in the file), the responder's own
  grants must cover `run.respond`. Grants are ordinary `mg:permits` Facts —
  they live in the file, sync with it, and are granted in CAL:

  ```
  GRANT run.respond ON ops TO "user:officer" WITH because("refund approvals")
  ```

- Rejected and expired responses are **journaled as Observations before the
  error returns** — a losing approval attempt is audit evidence, not a
  silent 4xx.
- `--ask-ttl <sec>` on start bounds how long an ask may sit unanswered.
- Refusing an ask is a first-class answer: `--is-error true` journals the
  refusal and fails the node as user-aborted.

The web console (`areev ui`) surfaces pending asks in its **Runs tab** — and
`run.respond` over HTTP refuses shared-token and anonymous callers outright.
Only a per-principal credential may approve, because the approver's identity
*is* the audit record. Cancel deliberately keeps the low bar.

## Budgets

`--max-tokens`, `--max-usd`, `--max-wall-ms`, `--max-supersteps`. Spend is
accounted from the journal (usage rides every result grain), so a resumed
run's accounting equals a live run's. Exhaustion is a **parked checkpoint**,
not a corrupted run: `areev run fork` re-opens a budget-exhausted terminal
under raised budgets, continuing exactly where it stopped.

## Verify and shadow

```bash
areev run verify --run-id demo-1     # one run: byte-compare replay
areev run shadow --runs a,b,c        # many runs: replay with ZERO effect dispatches
```

`verify` re-drives the run from the **manifest's input** with the clock
scripted from journaled readings and every effect answered from the journal,
writing nothing — then byte-compares each commanded checkpoint against the
stored chain. Divergence names the differing fields (`RUN-E009`). Canceled
runs verify too (the cancel is replayed from checkpoint state), and
checkpoints past the last verifiable point are reported honestly as
unverified rather than skipped.

`shadow` is the same machinery as a batch pre-flight: the replay path holds
no executor, so "re-execute these journaled runs with zero side effects" is
structural, not a promise. It is also how [Areev Loop](loop.md) evaluates
proposed changes against history before anything is applied.

## Time travel and migration (`fork`)

```bash
areev run fork --run-id demo-1 --as-run demo-1b --at 1          # branch from superstep 1
areev run fork --run-id demo-1 --as-run demo-2  --plan <HASH>   # migrate onto a NEW plan
```

Same-plan forks inherit the manifest's pins and the scheduler state at the
chosen (closed) checkpoint; new-plan forks are migrations — the new plan is
fully re-validated and restarts at its entry with the inherited context as
input. The fork's seed checkpoint `derived_from`s the base checkpoint and an
`mg:fork_of` Fact indexes the lineage, so ancestry is a query. A cancel on a
base run is honored across its fork descendants.

## The kill switch and the oversight report

```bash
areev run cancel --run-id demo-1 --because "operator abort"
areev run oversight-report --run-id demo-1     # or --plan <HASH> for the newest run of a plan
```

`cancel` writes a marker Fact — deliberately the **lowest-privilege** run
verb, because a brake must never be blocked by missing privilege. A live
driver drains at its next superstep boundary; `resume` finalizes a parked
one.

`oversight-report` answers the EU AI Act **Article 14** questions as a
command: where a human can intervene (the client-gated nodes), who is
authorized to (the `run.respond` grants in the file), what expires when
(ask TTLs), and how fast the kill switch actually drained — **measured**
from the journaled cancel Fact to the terminal checkpoint close, not
asserted. The article→capability→command map is
[`docs/eu-ai-act.md`](eu-ai-act.md).

## LLM nodes (abstract nodes)

An unbound node becomes an abstract node: a journaled tool-calling loop.

```bash
areev run start --workflow <WF> --run-id r1 --input '{}' \
  --model claude-sonnet --llm-max-tokens 4096 \
  --tool-cmd 'my-tools'          # the model may only call the plan's HOST pins
```

- Providers: `claude-*` (Anthropic), `openai:*`, `ollama:*`, or any
  OpenAI-compatible endpoint; keys come from the environment
  (`ANTHROPIC_API_KEY`, `OPENAI_API_KEY`, …). No model runs unless you
  configure one — an abstract node without an LLM is `RUN-E006` at start,
  not a silent skip.
- The tools *offered* to the model are exactly the manifest's pinned host
  Definitions. Tool arguments are validated against the pinned schemas —
  strictly, with one re-prompt on violation; an unknown tool name gets one
  re-prompt, then fails the node.
- A tool failure inside the loop is **model-visible** (the model can react),
  never scheduler-retried behind its back.
- Every model turn and every tool call is journaled with token usage, so
  budgets recompute from the journal and **verify never calls the model**.
- Per-turn token reservation means an un-dispatched turn survives budget
  exhaustion as a need — the raised-budget fork resumes exactly there.

## Fan-out (`Send`)

A node's result may carry the reserved `$send` key:

```json
{"$send": [
  {"node": "worker", "input": {"v": 1}},
  {"node": "worker", "input": {"v": 2}}
]}
```

Each spawn executes the target node with its own input under a task path
(`parent/0000`, `parent/0001`, …); the batch joins before the target's
downstream edges fire. Validation is all-or-nothing (one malformed spawn
fails the batch, not half of it), and declared reducers (`append`, `sum`, …)
make the merged results order-independent. Spawn targets are host-bound
nodes in v1.

## Watching a run

- `--events` streams structured run events (JSON lines) to stderr while
  stdout stays the machine surface.
- `--otel-endpoint http://collector:4318` exports one OTLP/HTTP trace batch
  per run at completion; resumes join the same trace (the trace id derives
  from the run id).
- Streaming is **observational only**: journals are byte-identical with no
  subscriber, a normal one, or a slow one — pinned by test.

## Surfaces

The same runtime on every surface — one journal, one set of rules:

| Surface | Shape |
|---|---|
| CLI | `areev run start/resume/respond/cancel/list/inspect/verify/fork/shadow/oversight-report/demo`, plus `areev run-trace` / `areev runs-touching` |
| MCP | the six `areev_run_*` tools ([reference](mcp-reference.md)); host tools only via `$AREEV_RUN_TOOL_CMD`; the acting principal is server-bound — `principal`/`responder` are never client-supplied |
| Python | `db.run_start(workflow, run_id, input_json, tool_cmd, …)`, `run_resume`, `run_respond(…, responder=…)`, `run_cancel`, `run_verify`, `run_shadow`, `run_fork`, `run_list`, `changes_since` — JSON strings out |
| Node | `await m.runStart(…)` and the same set (`runRespond`, `runFork`, …) — promises, JSON strings out |
| HTTP / console | `GET /api/run/list`, `GET /api/run/inspect`, `POST /api/run/respond` (per-principal credential required), `POST /api/run/cancel`; the console's Runs tab is the approval queue. The console's **Workflows** tab visualizes and edits plans themselves (not runs of them) — an editable node/edge graph over the same Workflow grains, built entirely on `/api/browse` and `/api/cal` (`ADD workflow`), no dedicated route. A plan with a bounded-cycle edge or a per-node retry count opens view-only: `ADD`/`SUPERSEDE workflow` has no surface syntax yet to author either (`* N` populates `retries`, not `max_cycles`) — and for the same reason, connecting an edge that would close a cycle in an editable plan is refused rather than silently saved as an unbounded one |

Authorization uses three verbs, granted like any other
([CAL DCL](cal-reference.md)): `run.execute` (start/resume), `run.respond`
(answer asks), `run.cancel` (the brake). An unbound local session is the
owner and holds every right — grants matter once a file is shared.

## The run ↔ memory join

Because the journal and the memory share one file, provenance is closed in
both directions with no extra infrastructure:

- an agent's `record_tool_call` can name the run and workflow node that made
  it (`run_id`, `workflow_hash` + `node_id` → the `mg:step_action` link);
- `areev runs-touching --hash <grain>` walks from any fact back to the runs
  that produced or refined it;
- `areev tool provenance <hash>` chains a piece of governed code to the loop
  recommendations that target it and the runs that executed it;
- the [Areev Loop](loop.md) `run_outcome` analyzer reads run terminals and
  proposes findings — *"this workflow failed 4 of 6 runs"*, *"this plan has
  spent $4.10"* — with the run grains cited as evidence.

## Error codes

All runtime errors lead with a stable `RUN-Ennn` code. The ones you'll
actually meet: `RUN-E002` unbounded cycle (add `max_cycles`), `RUN-E004`
unresolvable binding, `RUN-E005` bad condition, `RUN-E006` abstract node
without an LLM, `RUN-E007` budget exhausted (fork to raise), `RUN-E009`
replay divergence (names the differing fields), `RUN-E011` response names no
pending ask, `RUN-E012` missing grant, `RUN-E013` canceled. The full
registry is [`ERROR_CODES.md`](../ERROR_CODES.md).

## Bounds, stated

- Subgraphs run inline on the driver thread; a child that parks on a human
  gate fails its parent node (ask *bubbling* is not in v1), and parallel
  subgraph siblings serialize.
- `Send` targets host-bound nodes only in v1.
- The condition grammar is frozen; there is no expression language beyond
  it, deliberately.
- One memory = one writer: while a driver holds the file, other writers
  (including a second `areev run` on the same file) are refused with
  `STO-E002`. Respond-then-resume across processes works because each verb
  opens, works, and closes.
