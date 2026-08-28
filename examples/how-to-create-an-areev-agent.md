# How to create an Areev agent

An Areev agent is **a memory file plus a harness** — not a framework object.
The memory file holds everything the agent is (its tools *and their code*, its
plans, its knowledge, its history, its improvement queue) as immutable,
content-addressed grains; the harness is whatever drives `areev run` /
`areev trigger run` on a schedule or an event. This guide is the assembly
manual: the recommended architecture, which grain to reach for when, where
tool code should live, how security is split between the file and the host,
namespace design, CAL as the agent's query *and* presentation layer, the
autonomy spectrum from fixed pipeline to dynamic planning, and the do/don't
list learned from building the [vertical agents](agents/) in this repo.

Working example to keep open beside this:
[`agents/invoice-to-accounting/`](agents/invoice-to-accounting/) — the full
pattern in one file per language (Python, TypeScript, Rust — same plan,
same content address), with corrections taken by email reply and the loop
scheduled. Then [`agents/`](agents/) has nine more, and each one exists to
work a different part of this guide end to end: §4 (code as a grain) is
[`sanctions-screening/`](agents/sanctions-screening/), §6's fan-out is
[`rcm-optimization/`](agents/rcm-optimization/), §12's oversight report is
[`hiring-screening/`](agents/hiring-screening/), and erasure is
[`data-subject-requests/`](agents/data-subject-requests/). When a passage
here says something works, one of those asserts it on every release.
Shared how-to material for every agent example — email providers,
deployment, testing — lives in [`agents/docs/`](agents/docs/). Canonical references:
[`docs/run.md`](../docs/run.md) (the runtime),
[`docs/triggers.md`](../docs/triggers.md) (activation),
[`docs/loop.md`](../docs/loop.md) (self-improvement),
[`docs/cal-reference.md`](../docs/cal-reference.md) (the query language),
[`docs/security-model.md`](../docs/security-model.md) (threat model).

---

## 1. The architecture — six layers, one file

| Layer | Made of | Authored with |
|---|---|---|
| **Capabilities** | Tool Definition grains: schema, executor kind, locked params, capability declarations — and, for code-carrying tools, `executor_uri` naming the code blob by content address | `ADD tool` / `db.add("tool", …)` |
| **Procedure** | Workflow grains: nodes, edges (`cond`, `max_cycles`), bindings, retries, reducers | `ADD workflow` (CAL) or JSON `add` |
| **Activation** | Trigger grains pointing **at** a plan by hash — cron, watch, webhook, composite; no daemon, evaluation is a command | `areev trigger add --workflow <hash>` |
| **Execution** | `areev run` — journaled, budgeted, HITL-gated, verifiable | CLI / MCP `areev_run_*` / bindings |
| **Knowledge** | Fact / Event / State grains the tools read and write | `record_tool_call`, `remember`, `ADD` |
| **Oversight** | Grants (`run.execute` / `run.respond` / `run.cancel`), client-gate nodes, Areev Loop | CAL DCL, `areev loop` |

**One memory file per agent** (or per business domain). The file is the unit
of isolation, erasure, sync, and portability — an agent you can copy, back
up, or hand to an auditor as a single artifact. Because CAS blobs travel in
bundles, that artifact includes the tool code itself. Cross-agent reads go
through `ASSEMBLE` facade mounts, never shared connections.

---

## 2. Surfaces — the CLI in this guide has SDK twins

The `areev …` commands below are the walkthrough surface; **programmers
embed the same engine in-process** — every verb in this guide has a binding
twin (`run_start`/`runStart`, `run_respond`, `record_tool_call`, `remember`,
`loop_run`, …), and the whole of CAL is one string call away:

| Language | Package | Shape |
|---|---|---|
| Rust | [`areev` on crates.io](https://crates.io/crates/areev) (library crates + the binary) | `AreevFacade` directly — what every other surface wraps |
| Python | [`areev` on PyPI](https://pypi.org/project/areev/) | `areev.Areev("agent.db", ns="…")` — scalars in, JSON strings out; releases on drop |
| Node | [`@areev/areev` on npm](https://www.npmjs.com/package/@areev/areev) | same convention, promise-based; call `close()` when done |
| MCP | `areev serve --mcp` | for LLM-driven agents (Claude Code, any MCP client) |

`db.cal("…")` executes any statement in this guide; `cal_prepare` /
`calPrepare` parses and validates it at startup, turning a bad query into a
startup error instead of a first-turn error. Full embedding walkthrough:
[`docs/quickstart.md`](../docs/quickstart.md).

---

## 3. When to use which grain

| Grain | Use it for | Don't use it for |
|---|---|---|
| **Fact** | Durable knowledge as subject–relation–object: preferences, config, entity attributes. The unit recall ranks best | Free prose (that's Event), telemetry (Observation) |
| **Event** | Things that happened at a moment: a message, a decision, an episode. `remember` / `capture` land here, thread-indexed | Current-state lookups — recall the Fact, not the transcript |
| **State** | A mutable-by-supersession value with history: counters, checkpoints, run manifests. `ACCUMULATE` targets these | Anything another type models better; State is the escape hatch, not the default |
| **Workflow** | The plan: a directed graph of steps. Immutable — every edit is a supersession minting a **new hash** | Run state or trigger schedules (both point *at* the plan; it stores neither) |
| **Tool** | Two roles by `kind`: a **definition** (what a tool is — schema, executor, locked params, and its code via `executor_uri`) and an **execution record** (one call that happened, written via `record_tool_call`) | Hand-writing execution records with raw `ADD tool` — identical retries dedup into one grain and starve the loop's evidence |
| **Trigger** | A standing rule that starts a plan: cron, source-watch, composite gates with correlation windows | Anything the plan itself should decide — a trigger fires runs, it doesn't branch |
| **Goal** | The task's intent: description, criteria, open/closed state. Write one per task and `related_to`-link the plan to it | Progress logging (Events) or metrics (Observations) |
| **Observation** | Telemetry and audit: run outcomes, tool errors, authz decisions, loop transitions. What the analyzers read | Knowledge you'd recall into a prompt |
| **Skill** | Competence the agent has demonstrated, proficiency = confidence; `skill_stall` watches these | A tool catalog (that's Tool definitions) |
| **Reasoning** | A recorded chain of reasoning worth citing later | Routine step logging — the run journal already captures execution |
| **Consensus / Consent** | Multi-party agreement; a subject's recorded permission (GDPR trail) | — |
| **Recommendation** | Written by `areev loop`, never by hand — the governed improvement queue | Ad-hoc TODOs |

Rules of thumb: **if it's true, Fact; if it happened, Event; if it's measured,
Observation; if it's intended, Goal; if it's procedure, Workflow; if it's
capability, Tool.** When two fit, pick the one an analyzer or a recall query
will consume later — grains are written to be read.

**Two field traps worth knowing before you design around a grain.** First,
**unknown fields are accepted silently** — a misspelled key is copied into
`extra_fields` and does nothing, so a typo never errors, it just quietly
fails to work. Worse, a few *recognized* names have no builder behind them
and vanish entirely: on `goal` that includes `criteria`, `priority`,
`goal_state` and `progress` (only `description`, `subject` and `object`
survive), and there are similar gaps on `observation`, `reasoning` and
`consent`. Check what actually round-trips before you build a query on a
field. Second, `tool` takes **`input_schema` / `output_schema`** — there is
no bare `schema` — and workflow edges are **`src` / `dst`**, never
`from` / `to`.

**`add` and `supersede` mean different things in time, and the as-of reads
can tell.** A grain carries two clocks: `valid_from`/`valid_to` (when it was
true in the world) and the system clock (when you came to know it), and
`entity_at(..., axis="world"|"knowledge")` reads them separately. The rule
that falls out is sharp:

- a **variation** — a new state that coexists with the old one in its own
  time window — is an **`add`**. The world axis picks among *live* grains by
  their validity window, so both windows must stay live.
- a **restatement** — you were wrong, or you learned late — is a
  **`supersede`**. The knowledge axis walks the supersession chain, so a
  correction has to be linked to what it corrects.

Get it backwards and the reads go quiet rather than wrong: superseding a
still-valid window hides it from the world axis forever, and adding a
correction as a fresh grain leaves the knowledge axis unable to find it.
`system_valid_from` is not settable — the store copies `created_at` into it.
[`agents/insurance-documents/`](agents/insurance-documents/) turns this into
the difference between telling an insured they are covered and telling them
they are 112,000 short.

---

## 4. Where tool code lives — this decides what the loop can improve

A tool's *code* has two possible homes, and the choice is not cosmetic: **the
loop can only govern code that is a grain.**

**Code-carrying tools (the default to aim for).** The Definition names its
executor by content address — `executor_uri: "cas://sha256:…"` — and the code
is an ordinary CAS blob: it travels in bundles, its digest is verified on
every read, and nothing runs unless the **host** pinned the address at run
start (`--allow-executor` — the pin is the authorization, deliberately not
stored in the file, because code arriving in a bundle must not carry its own
permission). Three runtime shapes:

| `runtime` | What it buys | Trade |
|---|---|---|
| `"wasm32-areev"` | Sandboxed pure compute: fuel + memory ceilings, no I/O, one `.wasm` runs everywhere | No network, no SDK deps |
| `"wasm32-areev-io"` | Sandboxed **with declared capabilities**: `areev::fetch` through the credential broker (host allowlist ∩ the tool's declared hosts/methods/paths), blob reads by content address — the persistable I/O tool | Must compile to pure wasm32 |
| native (absent) | Any executable blob — a script with a shebang works. Persisted, provenance-chained, pinned | **Not sandboxed** (runs as you) and platform-specific; its deps must exist on the host |

**Host scripts (`--tool-cmd`).** A JSON-on-stdio process resolved by name at
dispatch. Right for the keyless mock floor, for one-off local glue, and for
SDK-heavy legs you genuinely cannot ship as a blob. Wrong as the resting
state of a production tool, because of what follows.

**Why this decides self-improvement.** The loop's `code_revision` class
works **only on grain code**: a `tool:` target must be a content address, the
revision pins the evalset it was gated against (Rule E1), apply is refused
without the recorded gating run, `areev tool provenance <hash>` chains code →
recommendation → approver → evalset result → the runs that executed it, and a
revert attaches its blast radius (`runs_touching` since apply). A file-based
script gets none of this: the loop can still *flag* it (`tool_failure`
clusters over execution records), but the fix happens in your editor —
ungoverned, unpinned, invisible to provenance. Store tools as files and the
agent's tools are permanently outside its own improvement loop.

You still develop in git — that costs nothing: the git mirror is export-only
by construction, and code enters the substrate only through an authored add
or an applied recommendation. Your build step compiles/copies the source and
seeds it as a blob; the grain is the deployed artifact, the repo is the
workshop.

**Running code-carrying tools — what the driver owes them.** Three practical
rules, learned by taking a production agent from `--tool-cmd` to grains:

- **Derive the pin from the files you ship.** `put_blob` returns
  `cas://sha256:` of exactly the bytes, so `--allow-executor` is computable
  from the workshop without opening the memory: hash the same files the
  seeder seeds, and the host authorizes precisely the code in its own
  checkout. The corollary is a feature, not friction: a `code_revision` the
  loop applies lands at an address the checkout does *not* contain, so runs
  refuse it (`RUN-E018`) until the operator syncs the revised source or pins
  the new address explicitly — the moment the agent's code moves ahead of
  the human's copy is loud, never silent.
- **A native blob's environment is the host's job.** A
  `#!/usr/bin/env python3` (or node, or sh) blob resolves its interpreter
  and imports from whatever environment the driver gave it — that is the
  documented native trade ("its deps must exist on the host"), so the
  driver must arrange it deliberately: put the right interpreter first on
  `PATH`, the shared library on the import path, before every
  `run_start`/`run_resume`/`trigger_run`. Split the code accordingly:
  the **improvable logic lives in the blob** (it is the unit the loop can
  revise, and what provenance chains), stable plumbing lives in a
  host-installed library the blob imports. A blob that is only a shim
  around a host library has put the logic back where the loop cannot
  reach it.
- **A stale pin stalls; it must not be left stalled.** Since 1.6.4 a
  refused run start **holds the cursor**: the firing records the RUN-E018,
  increments `consecutive_failures`, backs off and retries, and the item
  survives until pin and memory agree ([#129](https://github.com/AreevAI/areev/issues/129);
  before that fix the item was silently *consumed*, so pre-1.6.4 drivers
  must pre-check their pins cover every non-client `executor_uri` and
  refuse the whole tick). The remaining operational duty is noticing:
  watch `consecutive_failures`/`last_error` on `trigger status` — a desk
  refusing every start is safe but doing nothing. The one-step cure is a
  converging re-seed (§14), which updates definitions and pins together.

**A revision is a chain, not a supersession.** When the loop's advice is
"change the code", moving it takes four steps, because every layer names its
input by content address:

```
new bytes  -> a new blob address                       (put_blob)
           -> supersede the Tool definition to name it
           -> supersede the Workflow: bindings name tools BY HASH, so the
              plan must move too -- which mints a NEW PLAN HASH
           -> re-point the Trigger: triggers do NOT follow supersession heads
```

Stop after step two — the intuitive place to stop — and nothing happens: the
old plan still binds the old definition, which still names the old blob, and
the agent goes on running the rule you believe you replaced, silently. The
one visible symptom is that the pin you expected to break did not.
[`agents/sanctions-screening/`](agents/sanctions-screening/) walks the whole
chain and asserts each link.

**Inbound: do you need a connector at all?** The trigger path cannot yet
execute a connector from a grain (resolving connectors as capability tools
by content address is a documented not-yet), but often you don't need one:

1. **Push sources** → a `webhook`/`manual` trigger + `areev trigger
   deliver`, **no connector**. The host already terminates TLS and
   authenticates the sender; everything after delivery is plan nodes —
   grain-stored capability tools.
2. **Pull sources with simple item semantics** → a cron trigger, **no
   connector**, with a grain-stored `wasm32-areev-io` tool as the plan's
   entry node polling through the broker. The fetch code is a grain, so
   the loop governs it. Cost: you manage the cursor yourself (a State
   grain via `ACCUMULATE`) and one run processes a batch.
3. **Pull sources that need the item machinery** — per-item dedup (one
   run per item, twice-delivered = one run), cursor/catch-up/backlog,
   backoff, attachment filing into the CAS — → keep a connector script,
   and keep it a **dumb pipe** (fetch, normalize, cursor) so the logic
   worth improving lives behind the seam, in grains. Connectors are
   already shaped like capability tools (pure stdio, no memory access,
   brokered credentials), so expect this file to become a grain when the
   trigger path's egress plumbing unifies with the run path's.

---

## 5. Security restrictions — what lives in the file, what stays with the host

The security model is a deliberate split. **Declarations travel with the
memory; authorizations stay with the host** — so importing someone's bundle
imports their tools and code but never the permission to run them, and
nothing in a stolen file widens reach on your machine. Effective permission
is always an intersection: *declared ∩ granted ∩ host-configured*.

**In the file (travels, replicates, audited):**

- **Grants** — `mg:permits` Facts written via CAL DCL: `read` / `write` /
  `supersede` / `delete` / `erase` per namespace, plus the runtime verbs
  `run.execute` / `run.respond` / `run.cancel`. `GRANT … ON` takes exact
  namespace names (or `*` alone) — never prefix patterns.
- **Capability declarations on Tool grains** — the `capabilities` field:
  allowed hosts/methods/path-prefixes, named credentials, extra headers,
  `{"blob": {"read": true}}`. A tool reaches at most what it declared. Write
  **one `http` block per service**: a call must be admitted by a single block
  in full, so a tool holding two services' credentials cannot send one
  service's secret to the other.
- **`locked_params`** — arguments frozen in the definition; the model or
  caller cannot override them.
- **`runtime` + `runtime_limits`** — sandbox selection (`native`,
  `wasm32-areev`, `wasm32-areev-io`) and its ceilings (fuel, memory pages,
  call count, response bytes), frozen into the run manifest at start. The
  ceilings are validated for shape but **not clamped**, so a plan may declare
  more than you meant to allow — review them like any other declaration.
- **An anonymization policy per namespace** — `set_anon_policy(ns, …)`. The
  modes are exactly `off`, `audit` (detect and record, rewrite nothing),
  `egress` (reads leave pseudonymized), `ingress` (writes land
  pseudonymized), and `both`. There is **no "rewrite" mode**, though the
  behaviour of `egress` is often described that way. Start at `audit` and
  measure before you turn on rewriting — and never policy the operational
  namespace, because a rewriter that turns 64-char hashes and dates into
  `[PERSON_1]` will happily mangle your plans and bindings. Note `ingress`
  rewrites fields *before* hashing, so it is incompatible with the pinned
  `created_at` trick that keeps plan hashes reproducible.

**Host-side only (deliberately never persisted in the file):**

- `--allow-executor <hex,…>` — the pin that authorizes code-carrying blobs;
  per platform for native blobs.
- `--credential NAME=…` + `--allow-host` + `--tool-egress` — the broker:
  tokens never enter tool processes; the outbound allowlist is the host's,
  and a tool's declaration can only *narrow* it. **Reachable from the CLI,
  MCP (via `$AREEV_RUN_*`), and `trigger run` only** — the Python and Node
  `run_start` take no credential arguments, so an agent that brokers
  outbound calls is driven by the CLI or a heartbeat, not by a binding.
- `--tool-cmd` / `--sandbox-cmd` / `$AREEV_RUN_TOOL_CMD` — which local
  programs may execute at all (server-bound for MCP: a client cannot grant
  it to itself).
- `--no-destructive-ops` — a process-wide cap over any grant in the file.
- Console auth (`--token-env`, per-principal credentials for
  `run.respond`) and TLS posture — see
  [`docs/deployment-profile.md`](../docs/deployment-profile.md).

**The loop policy file** — [`examples/policy/`](policy/) ships three
`loop-policy.json` variants (solo / team / locked-down prod). It is the
**only** place auto-apply is granted, it rejects unknown keys, and it can
name only built-in action classes — so a committed or stolen policy file is
inert (it cannot register an executable). `areev loop policy` prints the
effective policy. Keep it in version control next to your seeder; review it
like code.

---

## 6. The autonomy spectrum — decide per node, not per framework

How much the LLM decides is expressed in the plan, node by node:

1. **Fully bound** — every node bound to a Tool definition. A deterministic
   pipeline; the model appears only inside tools, if at all. Start here.
2. **Client gates** — bind a node to a definition with
   `executor_kind: "client"`: the run parks until a named person `respond`s.
   The approver's identity is the audit record. Put one before every
   irreversible effect (payments, sends, deletes). Gates are not only for
   *external* effects: a decision that **changes the agent itself** — "may I
   remember this?", a knowledge update, a policy override — deserves the
   same treatment. A two-node plan (send the proposal → client gate) turns
   it into a parked run, and `run_respond`'s separation of duties (the
   responder must differ from the principal that started the run) then
   guards it structurally, instead of by an allowlist check in your harness
   that someone can forget to write. Two shapes worth knowing: a client
   node may be **terminal** (park → respond → the run completes — no
   downstream node needed), and on the embedded backend the *driver* writes
   the resulting grains after `run_resume` returns, because a tool process
   must never open the memory the runtime is holding.
3. **Abstract nodes** — leave a node unbound: its label becomes the LLM
   instruction, executed as a journaled tool-calling loop over the run's
   pinned tools. Agentic behavior inside a governed slot — budgeted,
   replayable, verifiable. Two practical limits: the tool-calling backend is
   an **HTTP provider** (`--model provider:name`), so `--llm-cmd` does *not*
   reach this path and the only local option is `ollama:<model>` against a
   real server — which is why no example here puts an abstract node on the
   keyless floor; and an attempt is capped at **16 effects**, so a node that
   needs more must be split.
4. **`$send` fan-out** — a node's result spawns tasks at runtime: dynamic
   width the plan didn't enumerate, joined by declared reducers. A reducer's
   value is a **bare string** — `lww` (the default), `append`, `sum`, `max`,
   `min` — and it is read at *run start*, never validated on the write path,
   so a mistyped reducer name stores cleanly and then refuses every run.
5. **Subgraph bindings** — bind a node to another **Workflow** hash and it
   runs inline as a child with its own journal. Compose vetted sub-plans
   rather than raw tools. **But a child that parks on a client gate fails
   the parent node** — v1 does not bubble asks through subgraphs, so
   subgraphs and human gates do not compose. Keep every gate in the parent
   and every child fully automated. There is also no depth limit and no
   self-reference guard: a self-binding plan recurses until the stack ends.
6. **Dynamic planning** — the agent authors the Workflow grain itself. See §7.

**One structural rule underneath all of them: a plan needs a dead end.**
Terminal nodes are the ones with **no outgoing edges**, and a run ends by
reaching one. A graph where every node has an out-edge — the natural shape
when you write a polling or retry loop — never completes; it terminates as
`Stalled` (`RUN-E001`). Give every cycle an explicit exit node.

Escalate along this spectrum only when the previous level can't express the
job. Every step up trades static checkability for flexibility — and the
governance (budgets, gates, journal, verify) is what makes the top of the
spectrum tolerable at all.

---

## 7. Dynamic workflows — the planner pattern

Nothing privileges human-authored plans. An agent holding `write` +
`run.execute` grants can plan per task:

```
task arrives
  → write a Goal grain (intent, criteria)
  → recall similar past plans + their measured outcomes        (§9)
  → author a Workflow grain for this task, related_to → Goal
  → run it: run_start(plan_hash, input)
```

Five properties make this safe rather than scary:

1. **Validation is a gate.** The generated graph is checked before the first
   journal write — unbounded cycles (`RUN-E002`), unreachable nodes
   (`RUN-E003`), malformed conditions (`RUN-E005`), shape errors
   (`RUN-E019`). A hallucinated graph is *refused*, with a code the planner
   can react to.
2. **Content addressing turns generation into a library.** Re-generating the
   same plan for a similar task returns the **existing** hash — so run
   history, failure clusters, and cost accumulate per plan across tasks
   automatically.
3. **The resolution freeze**: tools (and, for code-carrying tools, the exact
   code address and runtime) are pinned into the run manifest at start;
   nothing moves under a running run, and the manifest is the
   reproducibility record.
4. **Governance is orthogonal to authorship.** Budgets, the credential
   broker and outbound allowlist, client gates, executor pins, and grants
   apply identically whether a person or the planner wrote the plan. The
   planner does **not** hold `run.respond` — approvals stay with named
   people.
5. **Subgraph bindings compose vetted pieces.** The strongest form: the
   planner wires **approved sub-plans** (a binding to another Workflow grain
   runs it as an inline child), not raw tools. Dynamic topology,
   pre-governed building blocks.

Authoring surface note, and it is stronger than it looks. CAL authors plans
fine — `ADD workflow "name" … BIND …` has its own graph syntax — but **`ADD`
cannot create a Tool grain at all**: the types its `SET` form accepts are
exactly `fact`, `goal`, `observation` and `skill`. Every Tool definition
(host tools, `client` gates, `executor_uri` carriers) therefore needs the
generic `add` from a binding, MCP's `areev_add`, or the console. Since
bindings point at Tool *hashes*, a plan authored in CAL has nothing to bind
to until something else has created those grains — so **CAL and the CLI alone
cannot author a working agent**; a binding or MCP is required somewhere in
the pipeline.

On top of that, CAL's graph syntax cannot express bounded cycles
(`max_cycles`) or reducers, and its `* N` means per-node *retries*, not a
cycle bound. Generate plans that need either via JSON `add`.

---

## 8. Organizing the memory — namespaces are a hierarchy, and reads scope by prefix

Namespaces are dot-separated hierarchies, and the read side understands
them: a trailing `*` after a separator selects the base **plus its
descendants** — `"org.*"` matches `org`, `org.sales`, `org.sales.emea`
(never `organization`; a different separator like `org:x` needs `"org:*"`).
The same convention works everywhere plural reads happen: CAL `WHERE
namespace = "org.*"`, `namespace IN (…)` sets, the MCP `namespace` argument,
`areev recall --ns`, ASSEMBLE sources. **Design your namespace tree for
this** — the hierarchy is your scoping mechanism:

```
acme.support.tickets      acme.support.kb        acme.billing
plans.approved            plans.generated        agent:planner
```

- One query spans a whole subtree (`"acme.support.*"`) or the whole org
  (`"acme.*"`) without enumerating namespaces.
- Under a bound principal the expansion **fails closed**: every namespace a
  prefix covers must be within the session's read grants, or the whole
  query refuses (naming the pattern you typed, never a discovered
  namespace).
- Scopes select **reads only**. Writes, destruction (`FORGET SUBJECT`,
  `PURGE`), grants, and retention policy all take exact namespaces and
  refuse patterns loudly — a wildcard never widens a destructive surface.
  `*` is therefore reserved: you cannot name a namespace with one.

That asymmetry is what makes namespaces **promotion lanes**:

- `plans.approved` — human-promoted plans; the planner's building blocks.
- `plans.generated` — planner output awaiting evidence.
- `agent:<name>` or a per-domain namespace — the agent's working knowledge.
- `agent:harness`, `agent:authz` — written by the runtime and CAL (journals,
  manifests, audit); read them, don't write them by hand.

The planner *reads* `plans.*` in one query but can only *write* the exact
namespace its grant names; "promotion" is a governed supersede into
`plans.approved`, enforced by who holds `write` there.

Link at write time. `related_to` from plan → Goal, `run` on remembered
Events, `record_tool_call` naming `run_id` + node — these links are cheap at
write time and are exactly what retrieval and the loop consume later. A
grain nothing links to is a grain nothing will find.

---

## 9. Retrieval — rank by evidence, not just similarity

A Workflow's recall text is its node labels (`"fetch -> approve -> refund"`),
so plans participate in hybrid recall (BM25 + vectors when an embedder is
configured) like any grain:

```sql
RECALL workflows WHERE namespace = "plans.approved" "reconcile invoices"
```

Then join each candidate hash to its evidence:

- run-terminal Observations (`observation_kind = "run_outcome"`, keyed by
  `plan_hash`) → completion rate and spend;
- `areev runs-touching --hash <grain>` / `areev run-trace <run-id>` →
  concrete precedents in both directions;
- CAL `HISTORY sha256:<plan-hash>` → how the plan evolved and *why* (every
  supersession carries a reason).

The planner picks the plan with the best **track record** for this kind of
goal — retrieval ranked by measured outcomes, possible only because plans
and telemetry live in the same file. `ASSEMBLE` renders the selected context
(goal, candidates, outcome stats) into the planner's prompt under a token
budget.

---

## 10. CAL as configuration — saved queries and templates travel with the file

`RECALL` answers *what* comes back — grains from memory. **How it reads in
the prompt is a separate, versionable decision**: the `FORMAT` clause
renders the result set (`sml`, `markdown`, `toon`, `json`, …), and a
**template** is a format you define yourself:

```sql
-- Shorthand: one line per grain ({{variable}} syntax, filters via |).
DEFINE TEMPLATE ticket_line AS "- {{subject}} {{relation}} {{object}} ({{confidence}})"

-- Sectioned (OMS CAL §10.6): budget-aware, with summary/omission fallbacks.
DEFINE TEMPLATE briefing EXTENDS readable
  HEADER  { "## Context ({{assembly.grain_count}} items)" }
  ELEMENT { "- {{content}}" }
  ELEMENT_SUMMARY { "- {{summary}}" }
```

Use it anywhere a format goes: `RECALL … FORMAT TEMPLATE briefing` (bare
name = registered template; a *quoted* argument is always an inline body).
Under a budget, `ELEMENT_SUMMARY` replaces `ELEMENT` when disclosure
shrinks, and `ELEMENT_OMIT` accounts for what was dropped — formatting
degrades gracefully instead of truncating.

Pair templates with **saved queries** so the whole retrieval-and-presentation
recipe is named, parameterized, and hot-swappable without redeploying the
agent:

```sql
DEFINE QUERY "session_prompt"($user, $session)
  DESCRIPTION "standard session bootstrap"
AS {
  ASSEMBLE "session" FROM
    profile: (RECALL facts  WHERE subject = $user),
    recent:  (RECALL events WHERE session_id = $session RECENT 10)
  BUDGET 1200 FORMAT sml
}

RUN "session_prompt"($user = "john", $session = "call-42")
```

Why this matters for agents: saved queries and templates persist as
**`qry:`/`tpl:` meta rows in the memory file itself** — they travel with the
`.db`, replicate in bundles, and are visible identically from the CLI, MCP,
the console, and both bindings. Your prompt-assembly logic ships *inside the
agent artifact*, not scattered across client code. `DEFINE` parses the body
at write time (a broken query is refused where it's written, not at 3 a.m.
by an unattended agent), `RUN` caches the compiled plan per argument set,
and the loop's definition-rewrite class can even propose governed revisions
to a saved query. Tune the retrieval recipe with `DROP QUERY` + `DEFINE` —
no agent redeploy.

The same travels-with-the-file rule extends to a tool's **instructions**.
A prompt hard-coded in tool source is invisible to the memory: nobody can
read what the agent was told to do without reading code, and changing it is
a redeploy. Put the working instructions in a **Skill grain** instead,
include `RECALL skills` in the context query the trigger declares, and have
the tool read its contract from the assembled context (keep the constant in
code only as the fallback for a run that carried no context). Now the prompt
is versioned where the agent lives — superseding the grain retunes every
future run, `HISTORY` records why the wording changed, and the change needs
no deploy at all.

---

## 11. Self-improvement — the loop closes at three levels

**Level 1 — the plan improves.** Runs feed the loop's analyzers:
`run_outcome` (failure clusters and cost per plan hash), `tool_failure`
(per-tool error clustering), recall telemetry. `areev loop run` turns these
into Recommendations with the run grains as evidence; a person reviews
(`approve` / `reject --because`, separation of duties enforced); the fix is
a `SUPERSEDE workflow` → **new hash** → re-point the triggers (they do *not*
follow supersession heads); `outcome_review` re-measures after the review
window and **proposes a revert on regression**. Plan changes are never
auto-applied — deliberately.

> **The re-point keeps its state since 1.6.4** — cursor and dedup fence are
> keyed on the **root of the trigger's supersession chain**, so superseding
> a trigger to follow an improved plan neither re-seeds the source nor
> resets item dedup, and `areev trigger show` prints the cursor
> ([#128](https://github.com/AreevAI/areev/issues/128)). On older runtimes
> the superseded head started blank — a live source silently skipped
> everything since the last poll, and re-delivered items minted fresh run
> ids. Two defensive habits stay worth keeping regardless of version: a
> **connector that remembers the last cursor it returned** resumes instead
> of seeding when handed a genuinely fresh chain (a re-declared trigger,
> a restored host), and every costly effect carries its **own idempotency
> key** checked at the effect — the fence you control survives anything,
> and it is the one you want anyway.

**Driving the lifecycle from a binding, without tripping over it.** Three
things surprised every agent example built against this surface:

- **`apply` subsumes `approve`.** Calling `approve_recommendation` and then
  `apply_recommendation` fails with `LOP-E020 illegal lifecycle transition:
  approved -> approved`. Use `approve` when a human is signing off and
  nothing executes, or `apply` alone when the change should land — not both.
- **Whether `apply` is refused depends on the analyzer, not on your code.**
  Some recommendation families are advisory and refuse `apply` outright
  (`LOP-E011`); others accept it and are held back instead by the auto-apply
  ceiling in their manifest, so the engine reports `auto_applied: 0` even
  under a policy that grants the family auto-apply. Assert the behaviour of
  the analyzer you actually have rather than assuming a universal rule.
- **Analyzer thresholds are tuned to volume, and tuning is itself recorded.**
  The stock ratios assume a busy system; a desk doing a handful of runs a
  week will never trip them. `set_analyzer_config("loop.run_outcome/1", True,
  '{"min_failure_ratio": 0.3}')` is a legitimate act of configuration, not a
  fork. Note the ids carry a version suffix (`/1`) — read them from
  `loop_analyzers()` rather than guessing. And note what the denominator is:
  `tool_failure` divides by *that tool's* opportunities, so telemetry you
  record under a busy tool's name gets diluted below the firing threshold.

**Level 2 — the tool code improves (grain code only, §4).** A
`code_revision` recommendation targets a tool's content address, pins the
evalset it was gated against (Rule E1), and applies only with the recorded
gating run: `areev eval run --evalset <hash> …` then
`areev loop apply <rec> --because "…" --gating-run <eval-run-id>`. The whole
chain is one command — `areev tool provenance <code-hash>` — and a revert
carries its blast radius (the runs that executed the reverted code and the
grains they wrote, because reverting code does not revert data). None of
this exists for tools stored as files.

**Level 3 — the plan *generator* improves.** The loop's flagged plan hashes
and evidence live in the same memory the planner recalls from, so the next
task's retrieval surfaces "plans shaped like this failed most of their runs"
*before* the planner regenerates the shape. Improving the generator =
curating what it retrieves: promote proven plans into `plans.approved`, let
flagged ones sink. No fine-tuning, no prompt surgery — the feedback channel
is the memory itself.

---

## 12. Proving oversight — the record an auditor actually asks for

Agents that touch regulated work eventually have to answer a question that
is not "is the model good?" but **"who decided this, and could they have
said no?"** That distinction is worth internalising, because it is the one
the law actually draws.

Machines doing the work is not the regulated part. UETA §14 says a contract
may be formed by the interaction of electronic agents "even if no individual
was aware of or reviewed the electronic agents' actions", and E-SIGN
(15 U.S.C. §7001(h)) upholds it so long as the agent's action is "legally
attributable to the person to be bound". **What is gated is attribution.**

And the regimes that gate it hardest converge on the same four demands — a
named individual, their capacity, what they saw, and evidence they could
have refused:

- **EDPB/WP29 WP251rev.01**: a controller "cannot avoid the Article 22
  provisions by fabricating human involvement"; oversight must be
  "meaningful, rather than just a token gesture", carried out by someone
  "who has the authority and competence to change the decision".
- **Colorado C.R.S. §6-1-1701(15)** (as enacted by SB 26-189, 2026 — the
  earlier SB 24-205 was repealed and reenacted before it ever took effect):
  the reviewer must be trained, consider primary evidence, have authority to
  override, and "not default to the system output".
- **ITAR 22 C.F.R. §120.67(a)(4)(iii)**: the approver must be able to refuse
  to sign "without prejudice or other adverse recourse".
- **21 C.F.R. §11.50(a)**: the record must carry the name, the timestamp,
  *and* "the meaning (such as review, approval, responsibility, or
  authorship)" of the signature.
- **ERISA 29 C.F.R. §2560.503-1(h)(3)** and **42 C.F.R. §422.590(h)(1)**:
  the reviewer must be someone "who was not involved" in the original
  determination — and, in ERISA's case, not their subordinate either.

Read that list as a specification and the mapping is direct:

| What the rule demands | What the agent does |
|---|---|
| a named individual | `run_respond(..., responder="user:mo")` — the responder is required, never inferred |
| who was not the decider | separation of duties: the runtime refuses the principal that started the run |
| a recorded reason | the `--because` on every governed decision; a blank one is refused |
| what they saw | the ask's `input` is journaled, and `run_verify` re-derives the chain byte-for-byte |
| ability to refuse | reject and cancel are first-class outcomes, not error paths |
| retention | the memory is append-only and content-addressed; erasure is explicit and audited (§ below) |

**Ask the runtime for the report rather than writing one.** For a run or a
plan, `run_oversight_report(run_id=…)` / `areev run oversight-report`
produces the EU AI Act Article 14 picture **measured from the journal**:

```json
{"human_gates": {"client_gated_nodes": [{"node": "recruiter_review", ...}],
                 "every_client_ask_is_an_approval": true,
                 "separation_of_duties": "responder != triggering principal, refused structurally"},
 "authorized_responders": {"principals_granted_run_respond": ["user:ines", "user:mo"]},
 "budgets": {"max_tokens": 200000, "max_usd_micros": 1500000, ...},
 "kill_switch": {"verb": "run.cancel", "measured_cancel_to_drain_ms": [2]}}
```

Every field is derived, not asserted — the gated nodes come from the plan,
the responders from the file's grants, the ceilings from the manifest, and
the kill-switch latency is **timed** from the journaled cancel to the
terminal checkpoint. A policy document claims oversight; this measures it.

Two honest caveats. Human involvement does **not** change whether a system
is high-risk — the Commission's Article 6(5) guidelines (19 May 2026) say so
explicitly — so a gate is a control, never an exemption. And most US regimes
in this space are *disclosure* regimes rather than *decide* regimes: NYC
LL144, for instance, states outright that nothing in it "requires an
employer... to provide an alternative selection process" (6 RCNY §5-304(a)).
Build the gate because it is the right control and because your EU exposure
(GDPR Art. 22) or your sector rule demands it — not because a US audit
statute implies it.

**Derive state from the journal, never from your own side file.** It is
tempting to answer "what has this run done so far?" by reading the ledger
your tools appended to — it is right there and it is easy. It is also a
*different* record: it can miss effects that were journaled but not yet
written, double-count across a fork (a fork inherits the base's context
verbatim, so ledger rows attributed by run id disagree with reality), and
drift silently whenever a tool fails between doing the work and recording
it. The runtime's own answer is `run_inspect` for the summary,
`run_grains`/`run_trace` for the entries, and the last checkpoint's context
for the merged state. Keep your ledger as the record of **effects**, which
is exactly what makes a `run_shadow` assertion meaningful: replay the run
and the ledger must not grow.

**Erasure has a blast radius your namespace scope does not cover.** Two
things reintroduce an identity you just removed, and both were found the
hard way building the example below:

- **The run journal sits outside every erasure scope.** Journal grains live
  in `agent:harness`, so a namespace-scoped `forget_subject` never touches
  them — and if you passed the subject's name into `run_start`'s input, it
  is still there afterwards. Pass a **fingerprint** instead
  (`sha256(id)[:16]`, matching `authz::subject_fingerprint`), which is the
  same reason the audit records erasure by fingerprint rather than by name.
- **Recall telemetry writes identities back.** With the default
  `telemetry="aggregate"`, the desk's own post-erasure verification searches
  leave the erased name in the sidecar — and the loop then proposes
  *"recurring question with no matching memory: <name>"*, minting a fresh
  recommendation grain containing the identity you were asked to erase. Open
  a privacy-facing memory with `telemetry="off"`.

The general rule: **erasure is scoped to what you named, and the agent's own
machinery is not in that scope unless you put it there.** Sweep the journal,
the telemetry sidecar, any stream/bundle archives (`--retain` bounds them),
and your own logs.

These citations are load-bearing for the *design*, not legal advice, and
this area moves fast — several widely-repeated sources went stale in
2025-26 (the CFPB's Circular 2023-03 was withdrawn; the EEOC's AI guidance
was taken down; SR 11-7 was superseded and its replacement puts generative
and agentic AI out of scope). Check the primary source before you rely on
one, and prefer the regulation itself over guidance about it.

Worked examples: [`agents/hiring-screening/`](agents/hiring-screening/) (the
Article 14 report), [`agents/data-subject-requests/`](agents/data-subject-requests/)
(erasure and DSAR), [`agents/sanctions-screening/`](agents/sanctions-screening/)
(which code version decided).

---

## 13. Do and don't

**Do**

- Start fully bound; escalate autonomy per node, only as needed (§6).
- Seed tool code as grains (`executor_uri`, preferably `wasm32-areev-io`)
  so the loop can govern its revisions — the pin (`--allow-executor`) stays
  host-side, per platform for native blobs.
- Make the seeder converge: recall first on a stable identity, keep what is
  unchanged, supersede what is edited, in dependency order (blobs →
  definitions → workflow → trigger), and report what changed (§14).
- Derive the executor pin from the same files the seeder ships (sha256 of
  the bytes IS the blob address), and treat a run of RUN-E018 refusals as
  "re-seed now", not as noise (§4).
- Keep a tool's working instructions in a Skill grain recalled via the
  declared context query, with the code constant only as fallback (§10) —
  a prompt in the memory is versioned, auditable, and retunable without a
  redeploy.
- Put a client gate before every irreversible effect, and keep
  `run.respond` with named people — never the agent's own principal.
- Design the namespace tree for prefix reads (§8); grant writes on exact
  namespaces only.
- Ship retrieval + presentation as saved queries and templates in the file
  (§10), not as strings in client code.
- Version-control the loop policy file and the seeder; review both like
  code.
- Write a Goal per task and link the plan to it.
- Record every tool call with `record_tool_call` (it stamps per-call
  identity so retries stay distinct).
- Set budgets on every run; treat `BudgetExhausted` as a resumable state
  (fork under raised budgets), not an error.
- Keep the dev floor keyless and deterministic against fixtures; make the
  live path opt-in behind an env var.
- Bundle the whole agent (`plan.mgb`-style): plans, tool definitions, tool
  code blobs, saved queries, templates — one content-addressed artifact.
- Sweep references after superseding a plan: triggers first, then subgraph
  bindings in other plans, then anything quoting the hash.

**Don't**

- Don't leave production tool logic in `--tool-cmd` scripts — it puts the
  agent's tools permanently outside its own improvement loop (§4). Scripts
  are for mocks, local glue, and SDK legs that truly can't ship as blobs.
- Don't store run state or schedules on the Workflow grain — it's
  content-addressed; runs and triggers point at it, never the reverse.
- Don't rely on the runtime's cursor as your only fence. A re-point keeps
  its state since 1.6.4 ([#128](https://github.com/AreevAI/areev/issues/128)),
  but a genuinely new chain still seeds, and effects are where duplication
  costs money — keep idempotency keys at the effects (§11).
- Don't gate only external effects — a decision that changes the agent
  itself (remembering a lesson, overriding policy) goes through a client
  gate too, so the engine's separation of duties covers it (§6).
- Don't use raw `ADD tool` for execution records (dedup eats your
  evidence), and don't hand-write Recommendation grains (the loop owns
  them).
- Don't give the planner principal approval or destructive grants; a
  planner that can approve its own plans is not a governed planner.
- Don't expect wildcards anywhere destructive — erasure, grants, retention
  take exact namespaces by design; that's a guarantee, not a limitation.
- Don't reach for CAL `REVERT` — it parses but is not executable. Revert =
  supersede back to the prior graph (read it from `HISTORY`).
- Don't let an LLM compose raw-tool graphs when approved subgraphs exist —
  compose vetted sub-plans instead.
- Don't ship a vendor SDK into the agent's core — connectors and host tools
  are JSON-on-stdio processes at the seams; SDK weight lives in your
  scripts, not the memory.
- Don't share one memory file between unrelated agents to "simplify ops" —
  the file is the isolation and erasure unit; use mounts for cross-reads.

---

## 14. The development workspace vs. the agent

The **agent** is the memory file (or its bundle): tool definitions, code
blobs, plans, saved queries, templates, triggers — everything
content-addressed, with its history. What lives in your repo is the
**workshop** that produces it:

```
my-agent/
  src/               # tool code — compiled/copied and seeded as CAS blobs
  seed.py            # authors Tool definitions (executor_uri → blob),
                     # the Workflow grain, saved queries + templates;
                     # emits plan.mgb
  loop-policy.json   # auto-apply policy (start from examples/policy/)
  trigger.sh         # areev trigger add --workflow <hash> ...
  connectors/        # ONLY if the source needs the item machinery (§4.3) —
                     #   dumb-pipe stdio scripts (mock + real, env-gated)
  tools-mock/        # --tool-cmd mocks for the keyless floor ONLY
  fixtures/          # committed sample inputs
  smoke.sh           # keyless end-to-end: seed → trigger run → assert
  improve.sh         # areev loop run → review one rec with judgment
```

**The seeder must converge, not fork.** Grains are immutable and
content-addressed, so a seeder that re-`add`s on every deploy grows sibling
heads: two workflows with the same name, tool definitions nobody binds, a
`RECALL` that returns last month's plan next to this month's. Identity is a
**stable field** (`tool_name`, the workflow's `name`), never the hash — so
seed by *recall first, then add / keep / supersede*:

```python
def ensure(db, grain_type, fields, identity_key, ns):
    heads = json.loads(db.cal(f"RECALL {grain_type}s LIMIT 50 FORMAT json"))["grains"]
    head = next((g for g in heads
                 if g["fields"].get(identity_key) == fields[identity_key]), None)
    payload = json.dumps(fields, sort_keys=True)
    if head is None:
        return db.add(grain_type, payload, ns=ns)            # first seed
    if all(head["fields"].get(k) == v for k, v in fields.items()):
        return head["hash"]                                  # unchanged: converge
    return db.supersede(head["hash"], grain_type, payload, ns=ns)  # edited: evolve
```

Seed in dependency order — **code blobs → tool definitions → workflow →
trigger** — because each layer's fields embed the previous layer's
addresses: an edited blob changes its definition, a changed definition
changes the plan's bindings, a changed plan re-points the trigger (mind the
re-point cliff, §11). Emit what changed (`added` / `unchanged` /
`superseded` per grain): a deploy whose seed reports *nothing* changed is
verifiably a no-op, and one that reports supersessions names exactly what
evolved. This is also your **upgrade path**: pointing the same seeder at a
memory authored under an older shape migrates it in place — old definitions
supersede to gain what they lacked, new grains add, and nothing forks.

**One memory, one driver — and the guard is thinner than it looks.** The
embedded backend is single-writer, enforced two ways: an in-process
open-path registry (`STO-E002`) and an OS file lock while a handle is open
(`STO-E001`). Neither protects you from the case that actually happens,
because a well-behaved driver **opens per invocation and releases**: two
drivers on one memory rarely collide on a lock, they simply *interleave
between invocations*. One answers an ask the other was about to read, and
the second fails somewhere unrelated with an assertion about business logic.
This is a real failure mode for parallel CI, for a heartbeat that overlaps a
manual command, and for two operators on one box. Serialise deliberately —
an atomic `mkdir` lock beside the memory is enough for a harness, and a
scheduler lease is the production answer. Do not rely on the single-writer
guard to notice.

Deploy = ship the bundle, pin the executor addresses on the host, arm the
trigger. The repo never executes in production and the mirror back to it is
export-only — which is exactly what lets the loop, not the repo, be where
the agent's tools improve. `smoke.sh` + fixtures remain non-negotiable:
an agent whose happy path CI can't execute will rot exactly when someone
needs it.

---

## 15. Deploying it — alone or as a fleet

The repo's Docker image packages the deployment roles — console, trigger
heartbeat — as one container ([`docs/docker.md`](../docs/docker.md)),
and running several agents on one box changes nothing in §1's rule: **one
memory per agent**. On the embedded backend each agent owns its file and its
heartbeat process — the exclusive file lock makes sharing impossible rather
than merely inadvisable. On the Postgres backend each agent owns a schema
and every role runs concurrently. Either way, agents share infrastructure,
an image, and a database cluster — never a writable memory — so they cannot
race each other's heads, poison each other's recall, or block each other's
erasure. Cross-agent knowledge moves the governed ways only: read-only
mounts or bundle subscriptions.

---

## Missing something?

If your agent needs a capability this guide says is a not-yet (connectors
as grains, CAL syntax for `max_cycles`/reducers, CAL `REVERT`, …) — or one
it doesn't mention at all — **file an issue**:
[github.com/AreevAI/areev/issues](https://github.com/AreevAI/areev/issues).
Real agent-builder use cases are what prioritize the roadmap; describe the
workflow you're building, not just the feature.

And if Areev is useful to you, a ⭐ on
[github.com/AreevAI/areev](https://github.com/AreevAI/areev) genuinely
helps others find it.
