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
pattern in a few files. Canonical references:
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
  `{"blob": {"read": true}}`. A tool reaches at most what it declared.
- **`locked_params`** — arguments frozen in the definition; the model or
  caller cannot override them.
- **`runtime` + `runtime_limits`** — sandbox selection and its ceilings
  (fuel, memory pages, call count, response bytes), frozen into the run
  manifest at start.

**Host-side only (deliberately never persisted in the file):**

- `--allow-executor <hex,…>` — the pin that authorizes code-carrying blobs;
  per platform for native blobs.
- `--credential NAME=…` + `--allow-host` — the broker: tokens never enter
  tool processes; the outbound allowlist is the host's, and a tool's
  declaration can only *narrow* it.
- `--tool-cmd` / `--sandbox-cmd` / `$AREEV_RUN_TOOL_CMD` — which local
  programs may execute at all (server-bound for MCP: a client cannot grant
  it to itself).
- `--no-destructive-ops` — a process-wide cap over any grant in the file.
- Console/hub auth (`--token-env`, per-principal credentials for
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
   irreversible effect (payments, sends, deletes).
3. **Abstract nodes** — leave a node unbound: its label becomes the LLM
   instruction, executed as a journaled tool-calling loop over the run's
   pinned tools. Agentic behavior inside a governed slot — budgeted,
   replayable, verifiable.
4. **`$send` fan-out** — a node's result spawns tasks at runtime: dynamic
   width the plan didn't enumerate, joined by declared reducers.
5. **Dynamic planning** — the agent authors the Workflow grain itself. See §7.

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

Authoring surface note: generate plans via **JSON `add`** (bindings/MCP),
not CAL `ADD workflow`, whenever they need bounded cycles (`max_cycles`) or
reducers — CAL's graph syntax cannot express those yet, and its `* N` means
per-node *retries*, not a cycle bound.

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

## 12. Do and don't

**Do**

- Start fully bound; escalate autonomy per node, only as needed (§6).
- Seed tool code as grains (`executor_uri`, preferably `wasm32-areev-io`)
  so the loop can govern its revisions — the pin (`--allow-executor`) stays
  host-side, per platform for native blobs.
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

## 13. The development workspace vs. the agent

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

Deploy = ship the bundle, pin the executor addresses on the host, arm the
trigger. The repo never executes in production and the mirror back to it is
export-only — which is exactly what lets the loop, not the repo, be where
the agent's tools improve. `smoke.sh` + fixtures remain non-negotiable:
an agent whose happy path CI can't execute will rot exactly when someone
needs it.

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
