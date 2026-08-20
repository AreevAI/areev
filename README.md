# Areev

**The substrate for adaptive agents** — agents whose behaviour changes on
evidence, under human authority, in steps you can inspect, undo, and re-measure.

[![CI](https://github.com/AreevAI/areev/actions/workflows/ci.yml/badge.svg)](https://github.com/AreevAI/areev/actions/workflows/ci.yml)
[![License: MIT OR Apache-2.0](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#license)
[![MSRV](https://img.shields.io/badge/rustc-1.90%2B-blue.svg)](#install)

<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="demo/screens/graph-dark.png">
    <img src="demo/screens/graph-light.png" width="900"
         alt="The Areev console: an agent's memory as a provenance graph, with a rewind scrubber along the bottom">
  </picture>
</p>

<p align="center">
  <em>Every memory an agent holds, as a graph you can walk — and rewind.<br>
  This is a real console over the real <a href="data/demo.db"><code>demo.db</code></a> in this repo. Nothing here is a mockup.</em>
</p>

Areev is a memory engine you **embed**. It holds an agent's knowledge *and* its
execution history in one content-addressed file — raw and lossless, never an LLM
summary of itself — so a single substrate answers both questions a serious
deployment asks: **what should this agent recall right now?** and **what did it
do, on whose authority, and can we take it back?**

Recall is structural and queryable — [CAL](docs/cal-reference.md), a real query
language, not a similarity search you hope lands. It runs in-process in
**microseconds**, with no server anywhere near the recall path: fast enough
inside a real-time voice agent's turn, where a network call cannot go.

---

## The problem

Agent memory today is a vector store plus an extraction pipeline. Audited
deployments keep finding the same three failures, and none of them announce
themselves — the agent just gets quietly worse.

| What usually happens | What happens here |
|---|---|
| **The store fills with near-duplicates.** The same fact, written eleven slightly different ways, all ranked, all in the prompt. | Memories are content-addressed. A byte-identical rewrite collapses to **one** grain — not a duplicate, not an error. |
| **Stale values out-rank current ones.** The agent learned the price twice and cites the old number, and nothing in the store says which is which. | An edit is a **supersession**. Recall returns 1 current value and 0 stale ones; the old value stays in history, retrievable on purpose. |
| **Nobody can say where a belief came from.** "Why does it think that?" ends in a log grep, if the log still exists. | **100%** of grains trace to when and how they entered — and to the run that produced them. |
| **A crash pays the invoice twice.** Retry logic hopes the vendor deduplicates. | Intent is journaled **before** the effect. A crash-window redelivery reuses the same idempotency key and is journaled as a redelivery. |
| **"Delete this person's data" is a project.** | `FORGET SUBJECT` is one operation, it replicates as ordinary tombstones so replicas delete too, and a disclosure shares **one selector** with it. |

None of that is a promise you have to take on faith. It is a deterministic
benchmark with no LLM in the loop:
`cargo run -p areev-bench --bin honesty_metrics`.

---

## Three systems, one file

<p align="center">
  <img src="docs/assets/arch-three-systems.png" width="900"
       alt="Areev's architecture: Graph (the record — traverse, replay, assemble), Loop (the learning — detect, verify, measure) and Governance (the authority — intent-first, separation of duties, audit trail), all converging on one file that is immutable, content-addressed, versioned and tombstoned">
</p>

Designed *against* each other, not merely alongside: the graph is shaped so an
erasure can reach every copy of a fact, the loop is shaped so it cannot apply
its own advice, and governance is shaped so neither of them can be talked out
of it. That is why the record can be erased, the turn can be budgeted, and the
change can be undone.

> **git for your agent's memory**: log, diff, time-travel, forks with explicit
> merges, and encrypted incremental sync — built into the data model, because
> grains *are* content-addressed immutable objects.

---

## Sixty seconds

An accounts-payable agent. Invoices arrive, get extracted, and land in the
expense sheet — except the ones a person has to look at first, which park until
somebody says yes.

```bash
cargo install areev
git clone https://github.com/AreevAI/areev && cd areev/examples/agents/invoice-to-accounting
./smoke.sh
```

No credentials. No network. No model key. Seconds later:

```
3a. an invoice under the threshold posts itself
{"finished":"Completed","run_id":"small"}

3b. one over the threshold parks for a person
   waiting on ask e65803012896e8dc…
   the starter cannot approve its own run (refused, as designed)
{"finished":"Completed","run_id":"large"}

3c. a scanned page fails loudly rather than posting a blank row
{"finished":"Failed { node: \"parse_attachments\", detail: \"pdftotext produced 0 characters\" }"}

OK — 2 posted, 1 refused, 1 approval recorded against a named person.
```

That third one is the one worth staring at. A pipeline that "handles" an
unreadable attachment by extracting nothing writes a row of nulls into your
books. This one stops, and says why.

**[→ the full walkthrough](examples/agents/invoice-to-accounting/)** — four files
and three fixtures, one of which you'd replace to make it real.

---

## What a governed run actually is

Every agent framework executes graphs. Almost none can *prove* an execution
afterwards. Here the plan is a grain, the run is a journal in the same file, and
the person who has to say yes is a node in the graph — not a Slack message
somebody remembered to send.

An effect is written down **before** it is allowed to happen: intent is
journaled, the tool runs, the result supersedes the intent. That ordering is
what makes a crash-window effect redeliverable under the same idempotency key
instead of paid twice, and what lets `areev run verify` re-drive the whole run
from its journal and byte-compare every checkpoint.

<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="demo/screens/workflow-dark.png">
    <img src="demo/screens/workflow-light.png" width="900"
         alt="The invoice workflow in the Areev console: eight steps, conditional edges, and a human-approval node picked out in orange">
  </picture>
</p>

<p align="center"><em>The plan itself — conditional edges in a frozen grammar, and the one node that requires a person.</em></p>

<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="demo/screens/runs-dark.png">
    <img src="demo/screens/runs-light.png" width="900"
         alt="The Runs page: eight governed runs — six completed, one failed, one waiting on a human with Approve and Refuse buttons">
  </picture>
</p>

<p align="center"><em>…and eight real runs of it. One waiting on a person, one that failed honestly, six posted.<br>
Approving requires <strong>your own</strong> sign-in: a shared console token is refused, because the approver's identity <em>is</em> the audit record.</em></p>

---

## The loop that closes it

[**Areev Loop**](#areev-loop--governed-self-improvement-built-in) reads the
agent's own history back as evidence and proposes changes — *"this tool failed
40% of its calls"*, *"these two facts contradict"* — each citing the grains it
was computed from. Thirteen deterministic analyzers, **zero model calls
required**.

<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="demo/screens/suggestions-dark.png">
    <img src="demo/screens/suggestions-light.png" width="900"
         alt="The review queue: a contradiction and a fork surfaced by the loop, each with the conflicting values and an Apply or Dismiss decision">
  </picture>
</p>

<p align="center"><em>Thirteen findings from the demo memory, in plain language, each undoable.<br>
Nothing here applies itself.</em></p>

Every recommendation goes through **propose → review → apply → verify**, carries
a written reason and a stored inverse, and is **re-measured after apply**. Attach
an LLM and its findings are grounded against the evidence and independently
verified before a human ever sees them.

**The honest scope, stated plainly**: Areev Loop improves the agent's
**memory** — not the model's weights, and not by itself the model's outputs.
Areev never trains, ships no trainer, and takes no training dependency. One
property is worth the whole design: **the gate cannot be weakened by the thing
it gates** — a `code_revision` recommendation is pinned to the evalset it was
judged against, and can be applied only through the gating edge that judged it
(Rule E1).

---

## Erasure is a real operation

Because the record is content-addressed and subject-indexed rather than baked
into weights, removing a person is an operation rather than a project.
`FORGET SUBJECT` removes them from the live store and replicates as ordinary
tombstones, which delete on replicas too — and the data-subject report shares
**one selector** with it, so a disclosure describes exactly what an erasure
removes. The reach, and the archive-retention window it does *not* cover, are
stated plainly in [`docs/gdpr.md`](docs/gdpr.md) and
[`docs/erasure.md`](docs/erasure.md).

That is the property model-side memory cannot offer, and it is where blast
radius is actually controlled.

---

## What "adaptive" means here

An adaptive agent is **not** one that quietly retrains itself. It is one whose
behaviour changes *deliberately* — on evidence you can inspect, under authority
you granted, in a step you can undo and then measure again.

Nothing here runs unattended. There is **no daemon and no scheduler**: analysis
is a cheap idempotent command you put on a hook, a cron, or CI. Autonomy is
never earned by a metric; it stays an explicit grant from the host.

### 1. The record — and the turn

Every grain is an immutable, content-addressed `(subject, relation, object)`
assertion with a reverse index, so the store is a **provenance graph**, not a
pile of embeddings — and the same system decides what reaches the model:

- **Traverse it**: `areev related` walks edges, `WITH multi_hop` follows them
  in reverse — *what points at this?*, not just *what does this point to?*
- **Time-travel it**: `areev entity-at` reconstructs what the agent believed at
  a timestamp. Updates are supersessions, so history is never overwritten and
  the current value is unambiguous.
- **Assemble it into the prompt**: retrieval returns candidates; a turn needs a
  *budget-shaped* context. Grains render Full up to ~70% of the token budget,
  degrade to **Summary** to ~95%, then **Omit** — so the tail of a large result
  set costs a summary rather than the whole grain or nothing at all. A per-type
  priority table (consent > state > goal > fact) with a reserved minimum per
  type means one loud grain type cannot crowd out the consent record or the
  open goal. ([`areev-context`](crates/areev-context/), not a `top_k` and a prayer.)
- **Reproduce it**: same inputs, same context — allocation is deterministic,
  which is what makes a replay comparable to the original run. One renderer
  backs both this and CAL's `FORMAT`, pinned by a byte-parity test, so the two
  surfaces cannot drift.
- **Annotations stay annotations**: a cross-link between grains never alters its
  target's supersession state (OMS §15.3), so enriching the graph cannot
  silently rewrite what the agent currently believes.
- **Join execution to memory**: `areev run-trace` gives a run's transcript and
  the durable knowledge it produced; `areev runs-touching` answers the blast-radius
  question — *this fact is wrong, which runs produced or refined it?* Stated with
  its limit: a run that merely **read** a grain leaves no grain behind, so an
  append-only store cannot attest to it.
- **Concurrent edits become branches**, surfaced with a deterministic
  provisional head and merged explicitly — never silently lost.

### 2. The learning — Areev Loop

Thirteen deterministic analyzers over the agent's own history, four gates, and
an outcome measurement afterwards. Covered [above](#the-loop-that-closes-it);
the full guide is [`docs/loop.md`](docs/loop.md).

### 3. The authority — governance

Governance here is not access control bolted onto a store. It is the **shape of
the operations**:

- **Destruction takes a hash, an identity, or an age — never a predicate.**
  `DELETE` is not a token in the grammar. The three destructive statements each
  require an authorization grant plus a recorded reason, write an audit record,
  and can be capped off entirely per process.
- **The audit names a fingerprint, not the person.** An immutable, replicating
  grain naming the erased subject would undo the erasure it records.
- **One selector, two directions**: `REPORT SUBJECT` (disclose) and
  `FORGET SUBJECT` (erase) share it, so a DSAR cannot describe more or less than
  the erasure removes.
- **Effects are journaled before they happen.** `areev run` writes intent before
  every dispatch; a crash-window effect is redelivered under the **same
  idempotency key** — journaled as a redelivery — never minted as a duplicate.
  `areev run verify` re-drives the run with every effect answered from the
  journal, writing nothing, and byte-compares every checkpoint — a
  **journal-consistent** replay, which is what it is called rather than a bare
  "verified".
- **Separation of duties is structural**: the principal who triggered an
  approval gate cannot approve it.

---

## The rest of the case

- **Fast where it matters** (measured, Apple M4 Max): structural recall **~30µs**,
  `entity_latest` **~9µs**, 50ms-cadence voice loop with live write-back
  **79µs p50 / 152µs p99** per frame recall.
- **A runtime that can prove what it ran** — [`areev run`](#run-a-governed-workflow-areev-run)
  gives you LangGraph-grade execution (Send fan-out, subgraphs, typed reducers,
  streaming, time-travel forks) with an audit story the frameworks don't have:
  budgets enforced per-superstep, a kill switch whose cancel-to-drain time is
  **measured** into the oversight report rather than asserted
  ([EU AI Act Art. 12/14 map](docs/eu-ai-act.md)).
- **Hybrid recall**: structural + BM25 + vector legs fused with RRF; multilingual
  by construction (Arabic and English ride every leg; unspaced CJK rides the
  vector leg). Bring any embedder: the `EmbedBackend` trait in Rust, a callback
  in Python (`set_embedder`), or a command on every surface
  (`--embed-cmd 'my-embedder'` — text on stdin, JSON vector on stdout).
- **Distributed the git way**: op-log streaming with generations and
  point-in-time restore; pull subscriptions for fleet-wide knowledge
  distribution; concurrent edits become **branches with a deterministic
  provisional head** — surfaced, merged explicitly, never silently lost.
- **Private by design**: local-first, no telemetry; optional **AES-256-GCM
  encryption at rest** with an Argon2id-derived key; deletion is a tombstone or
  **crypto-erasure** (destroy the key, destroy the memory). See [Security](#security--privacy).
- **Model-native**: built-in MCP server, [Anthropic memory-tool backend
  adapter](docs/memory-tool.md), budget-aware context rendering (SML / Markdown /
  TOON / JSON), tool-schema rendering for 9 provider formats, Python and Node
  bindings.
- **Keep your stack**: [LangGraph and CrewAI adapters](#keep-your-langgraph-or-crewai-stack--govern-its-state)
  put your existing agent's state in a memory file you can diff, sync, and erase.
- **Runs where a file cannot**: stateless containers and multi-instance services
  get the same engine over a [PostgreSQL schema](#postgresql-backend-server-tier)
  with the same semantics, pinned by one conformance suite run against both.
- **A format you keep**: the `.mg` format is fully documented and
  [OMS](https://github.com/openmemoryspec/oms)-conformant (byte-exact test
  vectors), so the record outlives this engine — and outlives us. Importers
  exist for the common stores if you are bringing history with you
  ([`docs/migrate.md`](docs/migrate.md)).

*The `.mg` format and CAL are stable and documented, conformant with the Open
Memory Spec (OMS). See [`CHANGELOG.md`](CHANGELOG.md) for the current release.*

---

## See it yourself

The demo memory behind every screenshot above is committed to this repo — 466
grains, 8 governed runs, 13 pending recommendations, one open fork:

```bash
areev ui --db data/demo.db --ns accounting     # → http://127.0.0.1:7437
```

<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="demo/screens/analytics-dark.png">
    <img src="demo/screens/analytics-light.png" width="900"
         alt="The Analytics page: a census across all 12 grain types, a namespace breakdown, and which recall legs are active on this file">
  </picture>
</p>

<p align="center"><em>All twelve grain types this format can hold, and how many of each are in there.<br>
Click one and it writes the CAL that returns it.</em></p>

Rebuild it from scratch with [`scripts/build_demo.sh`](scripts/build_demo.sh):
every run in it is a real journal and every recommendation is a real analyzer
output, not rows written to look convincing.

## Install

Areev ships on all three registries — install the surface you need:

```bash
cargo install areev          # the `areev` CLI
pip install areev            # Python bindings
npm install @areev/areev     # Node bindings
```

No Rust toolchain? Every release also carries prebuilt `areev` binaries for
Linux (x86_64 / aarch64), macOS (Intel / Apple Silicon) and Windows x86_64:

```bash
curl -fsSL https://raw.githubusercontent.com/AreevAI/areev/main/scripts/install.sh | sh
```

It installs to `~/.local/bin` (`/usr/local/bin` as root; override with
`AREEV_INSTALL`), pins with `AREEV_VERSION=v1.0.2`, and verifies the download
against the release's `SHA256SUMS`. Or grab an archive straight from the
[Releases page](https://github.com/AreevAI/areev/releases) — handy in a
notebook, where the wheel covers the memory and the loop but `areev ui` (the
web console, including the review queue) lives in the binary.

Embedding the store in a Rust project? Add the library crates instead of the CLI:

```bash
cargo add areev-store areev-core
```

Or build from source (Rust 1.90+):

```bash
git clone https://github.com/AreevAI/areev
cd areev
cargo build --release                       # builds the `areev` binary
./target/release/areev --help
# Python bindings (maturin):  maturin develop -m crates/areev-py/Cargo.toml
# Node bindings (napi-rs):    cd crates/areev-js && npm ci && npm run build
```

## Quickstart (CLI)

Store a fact, recall it, hand it to a model — three commands, no ceremony
(`--db` is optional; it falls back to `$AREEV_DB`, then `~/.areev/default.db`):

```bash
areev add    john prefers "window seat"     # subject relation object
areev recall john                           # → the stored fact, one JSON grain per line
areev recall john --render sml              # → "john prefers window seat" as a model-ready block
```

Point it at a specific file with `-d mem.db` (or `export AREEV_DB=mem.db`).
Then explore: `areev cal '<QUERY>'` runs the query language, `areev ui` opens the
web console (http://127.0.0.1:7437), and `areev repl` is an interactive CAL shell.

### Give Claude Code (or any MCP client) persistent memory

```bash
claude mcp add areev -- areev serve --mcp --db ~/.areev/code.db --ns claude-code
```

`areev serve --mcp` speaks newline-delimited JSON-RPC 2.0 on stdio and works
with any MCP client — see [`docs/mcp-reference.md`](docs/mcp-reference.md).

### Run a governed workflow (`areev run`)

Memory is half the story. The other half is *executing* agents so that what
they did is provable afterwards — journaled, resumable, replayable, and gated
by humans where it matters. The 10-minute proof needs no LLM key:

```bash
areev run demo --db runs.db     # seeds a 2-node plan: host tool → human approval
                               # (prints the workflow hash — a content-addressed grain)
areev run start --db runs.db --workflow <WF_HASH> --run-id demo-1 \
  --input '{"who":"world"}' --tool-cmd 'printf '\''{"greeting":"hello"}'\'''
# → the host tool runs, then the run PARKS on the approval gate:
# {"kind":"requires_action","asks":[{"node":"approve","tool_call_id":"<ASK>",…}],…}
```

Approve it — as a **different** principal, because the principal who started
the run structurally cannot approve their own ask:

```bash
areev run respond --db runs.db --run-id demo-1 --ask <ASK> \
  --result '{"approved":true}' --as user:officer
areev run resume  --db runs.db --run-id demo-1   # → {"finished":"Completed"}
areev run verify  --db runs.db --run-id demo-1   # replays the journal, byte-compares every checkpoint
```

That `verify` is the point: every step wrote an intent grain *before*
dispatch and a result grain that supersedes it, plus a checkpoint per
superstep — so the run can be re-derived from its own journal and compared
byte-for-byte against what was stored. If anyone edited history, verify names
the checkpoint and the differing fields. From there, everything is a query,
not a log grep:

```bash
areev run-trace --run-id demo-1               # the full journal, in order
areev runs-touching --hash <HASH>             # which runs produced/refined this grain (the reverse join)
areev run oversight-report --run-id demo-1    # the EU AI Act Art. 14 answers: gates, budgets,
                                             # responders, MEASURED kill-switch drain time
areev run cancel --run-id demo-1              # the kill switch (lowest-privilege verb)
areev run fork --run-id demo-1 --as-run demo-1b --at 1   # time-travel: branch from superstep 1
areev run shadow --runs demo-1                # re-execute from the journal with ZERO side effects
```

Real plans go further than the demo, with the same guarantees:

- **LLM nodes**: leave a node unbound and it becomes an *abstract* node — a
  journaled tool-calling loop (`--model claude-sonnet`, `openai:gpt-5`,
  `ollama:llama3.1`, or any OpenAI-compatible endpoint; keys from the
  environment). Every model turn and tool call lands in the journal, so
  verify never needs to call the model.
- **Budgets that actually stop the run**: `--max-tokens / --max-usd /
  --max-wall-ms / --max-supersteps`. A budget-exhausted run parks at a checkpoint;
  `areev run fork` re-opens it under raised budgets exactly where it stopped.
- **LangGraph-grade control flow**: conditional edges, bounded cycles, `Send`
  fan-out, subgraphs, typed reducers (append/sum/max/…), streaming events —
  all validated at plan load, all replayable.
- **Every surface**: the same six verbs ride the [MCP
  server](docs/mcp-reference.md) (`areev_run_*` — host tools execute only
  via `$AREEV_RUN_TOOL_CMD`), the Python/Node bindings (`db.run_start(…)` /
  `await m.runStart(…)`), and the web console's Runs tab, which is the human
  approval queue (shared-token and anonymous callers are refused for
  approvals — the approver's identity *is* the audit record).

Because the plan, the journal, and the memory share one file, the run/memory
join comes free: an agent's tool call cites the run that made it, and a
fact's provenance names the runs that touched it. Full guide:
[`docs/run.md`](docs/run.md) · compliance maps:
[`docs/eu-ai-act.md`](docs/eu-ai-act.md),
[`docs/procurement.md`](docs/procurement.md).

### Keep your LangGraph or CrewAI stack — govern its state

You don't have to adopt the runtime to get the governance. Two pip adapters
(in [`adapters/`](adapters/)) put Areev underneath the framework you already
run:

```python
# LangGraph: a checkpointer where one thread = one memory file you can
# diff, sync, and erase; plus a BaseStore and a trace mirror.
from areev_langgraph import AreevCheckpointSaver
graph = builder.compile(checkpointer=AreevCheckpointSaver("./threads"))

# CrewAI: memory storage where every consolidation rewrite is a supersession
# — "what did the agent believe before the LLM rewrote it" stays a query.
from crewai.memory import Memory
from areev_crewai import AreevStorageBackend
memory = Memory(storage=AreevStorageBackend("crew.db"))
```

What that buys you over the in-memory/SQLite defaults: checkpoints form
supersession trees (time-travel and re-put both work, history kept); a
CrewAI record's `source` becomes a partition-keyed subject, so **one
`areev forget-subject "<source>"` erases that user's records, history, and
index rows with a receipt** — the right-to-erasure demo; and the trace/audit
mirrors are honest about loss: `best-effort` mode counts every dropped event,
`guaranteed` mode backpressures and never drops (the only mode a compliance
story may cite).

### Build an agent that learns — and can unlearn

Memory rot *compounds* in a self-improvement loop: an agent that re-learns
duplicates and keeps stale lessons doesn't plateau, it gets worse. Areev's
write path is the safety mechanism for that loop — log raw experience,
distill lessons into facts, track proficiency as a supersession chain:

```bash
areev remember --observer executor --content "Attempt 2: isolated the tempdir per test - PASSED."
areev cal 'ADD fact SET subject = "fix_flaky_tests" SET relation = "lesson"
  SET object = "Shared tempdirs need per-test isolation." REASON "distilled from session 41"'
areev cal 'HISTORY WHERE subject = "fix_flaky_tests" AND relation = "proficiency"'  # the learning curve
areev restore --db rewound.db --from ./checkpoints --until-hlc <T>  # roll back a bad learning episode
```

Distilling the lessons is a model call, and it is yours to own: no model runs
unless you point Areev at one (`--model provider:name` or `--llm-cmd`, key
from the environment). Point `remember` at one and it extracts the facts for
you — stamped `verification_status="unverified"` with the model named on the
grain, after the raw text is already stored, so a hallucinated extraction is
reviewable and never costs you the source
([cookbook §9](docs/cookbook.md#9-ingest-raw-conversation-then-distill-facts)).
What the write path guarantees either way: revised lessons replace instead of
co-ranking, every lesson links back to the experience that taught it
(`derived_from`),
synced/replayed writes can't double-store, and a bad episode rewinds with
point-in-time restore (checkpoint first — the recipe shows the flow). Even a
*paraphrased* re-learning is caught: `areev novelty` reports the nearest existing
lesson so the harness supersedes it instead of adding a near-duplicate
(advise-only — it never drops a write itself). Full loop:
[cookbook §10](docs/cookbook.md#10-build-an-agent-that-learns-and-can-unlearn--by-hand).

### Areev Loop — governed self-improvement, built in

The section above is the loop *by hand*. **Areev Loop** governs it: it turns your
agent's history into recommendations — evidence-cited, reviewable, undoable,
measured — starting with **zero model calls**. The fastest way to see it needs
no agent and no waiting:

```python
import areev, json
db = areev.Areev("proof.db", actor="user:me")
for _ in range(5): db.record_tool_call("stripe_refund", '{"error":"rate_limited"}', is_error=True)
for _ in range(2): db.record_tool_call("stripe_refund", '{"ok":true}', is_error=False)
db.loop_run()                                             # deterministic; never gated when bare
for r in json.loads(db.recommendations('{"status":"pending"}')): print(r["severity"], r["summary"])
# → high  Tool "stripe_refund" failed 5 times (71% of calls): rate_limited
db.apply_recommendation(<hash>, because="retries belong in the client")   # audited, undoable
```

What that buys you:

- **Your agent stops repeating what fails.** Thirteen deterministic analyzers
  (eleven default-on) cluster recurring tool failures into lessons, catch
  duplicate and contradictory facts, flag stale grains, and surface forks —
  computed over typed grains, never raw prose. With the recall-telemetry
  sidecar on, three of them see memory *utility*, not just hygiene: facts
  never recalled (`cold_grains`), questions that keep coming back empty
  (`coverage_gap`), context budgets overflowing (`budget_pressure`). And
  with [`areev run`](#run-a-governed-workflow-areev-run) journaling executions
  into the same file, `run_outcome` reads run terminals per plan — *"this
  workflow failed 4 of 6 runs (last error: …)"*, *"this plan has spent
  $4.10 across 6 runs"* — as analyzer findings with the run grains cited,
  not dashboard archaeology.
  Precision is measured, not asserted: 1.00 on the labeled fixture,
  with a 0.90 failure floor when the fixture runner is invoked
  (`cargo run -p areev-bench --bin loop_precision`). The reusable Effective
  Reliability arithmetic and loop correctness tests run in ordinary CI; the
  fixture binary itself is an explicit evaluation command.
- **Nothing changes behind your back.** Four gates — propose → review →
  apply → verify — with separation of duties, a **mandatory reason** on every
  decision, a hash-chained audit grain per transition, and a stored inverse
  for every apply. Auto-apply is off unless a host policy file explicitly
  grants it, and never for destructive or LLM-originated changes.
- **It proves whether its own advice worked.** A recommendation that carries
  a metric is re-measured after you apply it — at 1d / 7d / 30d checkpoints,
  against what actually happened (did that tool failure recur?); a late
  regression proposes a revert. `areev loop outcomes` is the receipt.
- **Add an LLM for what determinism can't see — verified, never trusted.**
  `areev loop run --model claude-sonnet` (or `openai:gpt-5`,
  `ollama:llama3.1`, any OpenAI-compatible endpoint, or `--llm-cmd 'CMD'`)
  lets a model discover cross-fact issues like a semantic contradiction — but
  every draft must ground against the cited grains and survive an
  **independent verifier** (the proposer never grades itself) before it
  reaches the queue, and `origin = llm` can never auto-apply. "Nothing to
  report" is a first-class answer, so it doesn't invent findings to look busy.

### Reproducible trajectories and governed corpora

The trajectory path keeps the typed evidence needed to replay or train from a
run: `record-tool-call` stores JSON arguments separately from results,
`capture-stop` preserves every ordered chat/content block, `run-manifest`
binds a run to a content-addressed configuration, and sampled ASSEMBLE
manifests record the exact included/dropped hashes plus the rendered digest.
Set `--run-id` to join full-mode recall telemetry to the same trajectory.

`areev corpus --select '<READ CAL>' [--out train.jsonl] [--recipient ID]` reuses CAL as the
authorized selector and streams OpenAI chat JSONL with tool definitions,
step-level loss weights/quality labels, elision records, and trace/model/policy/
subject-fingerprint bindings. Each export writes a replicating manifest grain
whose `related_to` edges name every source hash; `--recipient` records the
downstream trainer/model owner that must act on a stale-export notice. Later identity or retention
erasure reports which exported corpora are stale and must be retired or
re-derived; this is auditable suppression/re-derivation, not a claim that a
subject has been removed from model weights.
- **It runs where you already run things — no daemon.** A cheap, idempotent
  command with watermark gates (`--min-new`, `--if-stale`): a Claude Code
  `SessionEnd` hook, cron, CI (`areev loop list --fail-on high` exits 2 —
  a build gate), or the `areev_loop` MCP tool. And the loop closes *into*
  the agent: `areev recall-hook --with-loop` rides the pending queue into
  the context Claude Code injects, so the agent sees its own recommendations
  without polling. The console (`areev ui`) shows the queue, recall sessions,
  and measured outcomes.

From a fresh install: `areev init --db demo.db --template demo` seeds a demo
corpus, `areev loop run` proposes across analyzers (`areev loop reflect`
sweeps the whole memory), and the Areev Loop tab in `areev ui` is the governed
review queue. Full guide: [docs/loop.md](docs/loop.md) · why the LLM layer
is verified, never trusted: [docs/loop-reflection.md](docs/loop-reflection.md).

## Examples

Runnable material lives in [`examples/`](examples/) — docs-with-files, cloned
rather than installed. Every one models **judgment**: approve a recommendation,
dismiss another with a reason. None of them is a rubber-stamp loop.

| | |
|---|---|
| [`agents/`](examples/agents/) | **Vertical agents**, end to end: a polling trigger wakes a workflow, a human approves what spends money, a system of record gets written once. Every vendor leg is a JSON-on-stdio connector, so they add **no dependencies** to this repo |
| [`colab/`](examples/colab/) | Notebooks: the full self-improving loop, plus five business walkthroughs — wrong-lesson rollback, detect/review/govern, an enterprise architecture. Keyless deterministic floor; the LLM layer is optional |
| [`ci/`](examples/ci/) | A GitHub Actions job that **fails the build** on pending high-severity recommendations — governance as a merge gate |
| [`policy/`](examples/policy/) | Three `loop-policy.json` variants: solo, team, locked-down production |
| [`mcp/`](examples/mcp/) | The multi-agent supervisor pattern — separation of duties enforced over MCP |
| [`import/`](examples/import/) | Tool-call JSONL → Tool grains → tool-failure clustering |
| [`analyzers/`](examples/analyzers/) | Bring your own analyzer over the probe/analyze protocol (advisory-only by construction) |
| [`llm/`](examples/llm/) | Ready-to-run `--llm-cmd` backends, plus the stdin/stdout protocol |

### Rust

Embed the store in-process. Add it to your `Cargo.toml`:

```toml
[dependencies]
areev-store = "1"
areev-core  = "1"
```

Most agent hosts are async (Tokio, axum). Use `AsyncAreev` there — it runs each
operation on the blocking pool and tears the store down off the async worker, so
neither a call nor a drop can panic inside a runtime:

```rust
use areev_store::AsyncAreev;
use areev_core::types::Fact;

let db = AsyncAreev::open("agent.db").await?;
db.add(Fact::new("john", "prefers", "dark mode")).await?;
let latest = db.latest("caller", "john", "prefers").await?;
```

In synchronous code (a CLI, a script, a test) use `Areev` directly:

```rust
use areev_store::Areev;
use areev_core::types::Fact;

let mut db = Areev::open("agent.db")?;
db.add(&Fact::new("john", "prefers", "dark mode"))?;
```

> `Areev` is blocking and drives its own runtime, so it must not be called — or
> dropped — from inside an async runtime. Reach for `AsyncAreev` in async code.

### Python

```python
import areev, json
m = areev.Areev("john.db", ns="caller")
m.add_fact("john", "prefers", "tea", confidence=0.95)
m.recall("john")                     # JSON string, newest-first — needs a subject
m.search("tea", k=5)                 # free text, when you don't have a subject.
                                     # BM25-only out of the box, so it matches
                                     # words that are present; install an
                                     # embedder (below) for semantic hits like
                                     # "hot drinks".
m.cal('RECALL facts WHERE subject = "john"')
m.memory_tool(json.dumps({"command": "view", "path": "/memories"}))  # Anthropic memory-tool backend
```

`Areev(..., index_text=False)` turns the BM25 index off for this file (a
deliberate re-stamp, reported by `open_warnings()`). That trades `search()`'s
text leg — keep it working by installing an embedder — for write latency that
stays flat as the file grows. `add_batch(...)` writes many grains in one
transaction; to load another system's export, prefer `migrate()`.

### Node

```js
const { Areev } = require('@areev/areev')

const mem = new Areev('john.db', 'caller')                  // 3rd arg: passphrase for AES-256 at rest
await mem.addFact('john', 'prefers', 'tea', 0.95)
await mem.recall('john')                                     // JSON string, newest-first
await mem.cal('RECALL facts WHERE subject = "john"')
await mem.memoryTool('{"command": "view", "path": "/memories"}')  // Anthropic memory-tool backend
```

Every method returns a promise — store calls run on libuv's thread pool rather
than blocking the event loop. The constructor is the exception, so opening a
file still fails at the line that opened it. **Await your writes**: promises
settle in completion order, not call order.

### PostgreSQL backend (server tier)

One memory = one file is the edge story. In stateless deployments (Cloud Run,
autoscaled containers) there is no durable disk — so the same store runs over
**one PostgreSQL schema per memory** instead, behind the non-default
`postgres` cargo feature:

```bash
cargo install areev --features postgres
areev add luis prefers window_seat --db 'postgres://user:pass@host/db?schema=memory_luis'
areev recall --db 'postgres://user:pass@host/db?schema=memory_luis' --subject luis
```

The bindings ship with the backend built in — the same class takes a DSN
where it takes a path:

```python
m = areev.Areev("postgres://user:pass@host/db?schema=memory_luis")
areev.drop_postgres_schema(url, "memory_luis")   # memory-level erasure
```

```js
const m = new Areev('postgres://user:pass@host/db?schema=memory_luis')
dropPostgresSchema(url, 'memory_luis')            // memory-level erasure
```

```rust
let mut m = Areev::open_postgres("postgres://user:pass@host/db", "memory_luis")?;
```

Identical semantics by construction — the same store logic (fork election,
supersession, op-log, BM25, hybrid recall) runs over either backend, pinned by
a conformance suite that executes the same case list against both. The
differences are deliberate and explicit:

- **Latency class**: point reads are microseconds embedded, milliseconds over
  a network. The voice frame path stays on the embedded backend by design.
- **Multiple concurrent writers per memory**: any number of app instances can
  hold handles on the same schema. Write transactions claim their id blocks
  from an in-schema counters row, which serializes them briefly — so the
  op-log stays gapless and ordered for followers, racing supersedes of one
  head produce one winner and one clean `SupersessionConflict`, and readers
  never block (MVCC). One instance can likewise hold handles to many
  memories (the schema-per-tenant shape).
- **Vectors** use [pgvector](https://github.com/pgvector/pgvector); the
  `vector(dim)` column is created when the first embedder is installed, and a
  dimension mismatch is a hard refusal rather than a degraded leg.
- **Erasure and portability** map to schema operations: `pg_dump -n <schema>`
  exports a memory, `DROP SCHEMA … CASCADE` erases one (exposed as
  `drop_postgres_schema`). Recall telemetry rides the memory's schema too.
  Page-level crypto-erasure remains a file-backend capability; encrypt at
  the deployment layer (TDE/pgcrypto) instead.
- **Right to erasure and retention** (both backends): `forget_subject`
  erases every structured reference to one identity — full history, object
  references, thread events, the dictionary entry itself — with replicating
  tombstones; `forget_older_than` is the age-based retention sweep. Both
  are host-level operations, deliberately not reachable from CAL; see
  [docs/erasure.md](docs/erasure.md) for the scope contract and the
  documented OMS deviation.
- **HA is inherited**: run it on a regionally-replicated Postgres and the
  memory inherits the failover, PITR, and backup story your ops team already
  drilled.

### Encryption at rest

```bash
export AREEV_KEY="correct horse battery staple"
areev add --db secret.db --ns caller --subject john --relation prefers \
  --object "window seat" --passphrase-env AREEV_KEY   # AES-256-GCM, Argon2id key
```

### Durability & fleets

```bash
areev stream  --db john.db --to  s3-mounted/john/     # continuous op-log shipping (~Litestream, grain-level)
areev restore --db new.db  --from s3-mounted/john/ [--until-hlc T]   # incl. point-in-time
areev follow  --db org-replica.db --from org-pub/     # subscribe: org knowledge → every edge
areev verify  --db john.db                            # integrity + full content-address recheck
```

One memory = one file: the unit of erasure (crypto-erase = key destruction),
sync, portability, and write parallelism. Partition by user, org, category, or
conversation — your call.

## Benchmarks

Reproducible harnesses in `crates/areev-bench` (accuracy, honesty, transport)
and `crates/areev-store/examples` (`bench`, `voice_loop` — the in-process
latency gates) — full methodology and raw data in
[`RESULTS.md`](crates/areev-bench/RESULTS.md); committed transcripts in
[`results/`](crates/areev-bench/results).

**Memory quality — [LoCoMo](https://github.com/snap-research/locomo)** (10
conversations, 5,882 turns, 1,982 QAs), a plain retrieve-then-read pipeline with
no task-specific tuning:

| retrieval leg | Areev |
|---|---|
| hit@10 / hit@20 — OpenAI `text-embedding-3-small` | **74.5% / 81.6%** |

End-to-end answer accuracy is **54.2%** across all 1,982 QAs (gpt-4o-mini reader,
gpt-4o judge, k=20) — a cheap, untuned reader over that retrieval, where the
reader (not recall) is the ceiling; a stronger reader lifts it. Bring your own
models (`$AREEV_LLM_CMD` / `$AREEV_JUDGE_CMD`) and embedder (the `EmbedBackend`
trait; the no-API TF-IDF floor still scores 40.7% hit@10). Every answer and judge
verdict is committed for audit — the category has a history of unreproducible
claims, so we publish the receipts:
[transcripts](crates/areev-bench/results/locomo-gpt-4o-mini-k20-2026-07-07.transcripts.jsonl)
([summary](crates/areev-bench/results/locomo-gpt-4o-mini-k20-2026-07-07.summary.json)).

**Memory integrity — honesty metrics** (structural, deterministic, no LLM):
byte-identical writes settle to **one grain** (idempotent import, sync replay,
and retries — paraphrase dedup is host-side); after 20 updates recall returns
**1 current value, 0 stale** with full history kept; writes cost **~136µs and
0 LLM calls** (text index off or deferred; a live FTS index adds ~140ms/write
— RESULTS.md finding #1); **100%** of grains trace to when/how they entered.
`cargo run -p areev-bench --bin honesty_metrics`.

**Latency** (Apple M4 Max) — the microseconds that make an embedded engine a
different shape from a memory *service*:

| recall operation | p50 | p99 |
|---|---|---|
| `entity_latest` (in-process) | **~9 µs** | — |
| structural recall (in-process) | **~30 µs** | — |
| inside a 50 ms voice frame, live write-back | **79 µs** | 152 µs |
| same recall via localhost HTTP sidecar | 158 µs | 264 µs |
| same recall via MCP stdio (agent host) | 129 µs | 205 µs |

Every surface above fits inside 0.6% of a 50 ms audio frame; the two transport
rows show the cost is the network hop, not the store — the whole argument for
embedding it.

**On edge hardware** — benchmarked on the devices themselves, not extrapolated.
A **$35 Raspberry Pi 3 B from 2016** (1 GB RAM, 1.2 GHz Cortex-A53, consumer
microSD) serves recall at **~361 µs, flat from 500 to 8,000 grains**; an
**Intel NUC8i3BEH from 2018** (i3-8109U, NVMe) does the same in **~30 µs** —
matching the M4 Max figure above, through the Python binding's FFI. Both install
with `pip install areev` in 16 seconds, no compiler. 16× the corpus, same
latency: a device can accumulate memory for months and answer as fast on day 200
as on day 1. The write path is the one thing to design for (bulk-load at
0.4–4 ms/grain vs 24–201 ms with a live FTS index). Clock-certified per phase,
with a projection for current Pi hardware:
[RESULTS.md §6](crates/areev-bench/RESULTS.md).

## Quality

An engine you embed runs inside your process, holds the memory your agent is
trusted to act on, and — through `FORGET SUBJECT` — destroys data on request.
That is a lot to ask of a dependency, so the engineering is measured in the
open and regenerated from the tree on every CI run:

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="docs/assets/repo-stats-dark.svg">
  <img src="docs/assets/repo-stats-light.svg" width="760"
       alt="Areev repository quality metrics — source and test line counts, test count, and stable error codes, generated from the tree">
</picture>

- **Tests are about a third of the codebase**, and roughly half of that is
  *integration* testing — the CLI and MCP suites drive the real binary over
  real stdio, not mocks. `cargo test --workspace` runs the lot in under a minute.
- **Every user-facing error has a stable code.** `DOMAIN-Ennn`, **append-only**
  — a code is never renumbered or reused, so an error you handle today keeps
  its meaning across upgrades ([`ERROR_CODES.md`](ERROR_CODES.md)).
- **Both storage backends run the same conformance suite.** One case list —
  forks, replication, tombstones, PITR, BM25, vectors, CAS, CAL — executed
  against embedded Turso *and* PostgreSQL, so backend choice cannot quietly
  change semantics.
- **CI is the gate, not a formality.** Tests on Linux, macOS and Windows;
  `clippy -D warnings`; a pinned MSRV build; `cargo doc`; coverage;
  `cargo deny` for advisories and licences; and the Python and Node bindings
  built and tested on every commit.
- **The docs are executable.** The CAL examples in
  [`cal-reference.md`](docs/cal-reference.md) are parsed by a test that fails
  CI on a stale one — the reference cannot drift from the language.

Full per-crate breakdown: **[docs/repo-stats.md](docs/repo-stats.md)** (also
emitted as [`repo-stats.html`](docs/repo-stats.html) and
[`repo-stats.json`](docs/repo-stats.json)). All of it is produced by
[`scripts/repo_stats.py`](scripts/repo_stats.py) and regenerated on every CI
run, which fails the build if the published figures drift from the tree — these
numbers cannot go stale without turning the build red.

## Documentation

| Doc | For |
|---|---|
| [`ARCHITECTURE.md`](ARCHITECTURE.md) | How Areev works: grains, `.mg` format, CAL, recall, sync |
| [`docs/loop.md`](docs/loop.md) | Areev Loop — governed self-improvement (analyzers, four gates, policy, CLI/bindings/MCP/API) |
| [`docs/loop-reflection.md`](docs/loop-reflection.md) | The reflection engine — how LLM proposals are grounded, verified, and measured |
| [`docs/run.md`](docs/run.md) | `areev run` — the governed runtime guide: authoring plans, the journal, verify, HITL, budgets, forks, every surface |
| [`docs/eu-ai-act.md`](docs/eu-ai-act.md) · [`docs/procurement.md`](docs/procurement.md) | EU AI Act article→capability→command map, and the procurement/security questionnaire answers |
| [`docs/deployment-profile.md`](docs/deployment-profile.md) | Deploying the runtime + adapters: modes, auth, SSO, what each mode may claim |
| [`docs/cal-reference.md`](docs/cal-reference.md) | The CAL query language reference |
| [`docs/mcp-reference.md`](docs/mcp-reference.md) | The MCP server + its 23 tools |
| [`docs/migrate.md`](docs/migrate.md) | Importing an existing corpus, with its edit history, from other stores or JSONL |
| [`docs/memory-tool.md`](docs/memory-tool.md) | The Anthropic memory-tool backend (Python / Node / CLI) |
| [`docs/cookbook.md`](docs/cookbook.md) | Task-oriented recipes |
| [`FAQ.md`](FAQ.md) | Questions & answers (also LLM-friendly) |
| [`SECURITY.md`](SECURITY.md) · [`docs/security-model.md`](docs/security-model.md) | Security policy & threat model |
| [`docs/gdpr.md`](docs/gdpr.md) · [`docs/erasure.md`](docs/erasure.md) | GDPR obligations → capabilities (for a DPIA), and the erasure requirement record |
| [`AGENTS.md`](AGENTS.md) · [`llms.txt`](llms.txt) | For AI agents working in / with this repo |
| [`CONTRIBUTING.md`](CONTRIBUTING.md) | How to contribute (DCO sign-off) |

## Security & privacy

Areev is local-first and collects no telemetry. Optional **AES-256-GCM
encryption at rest** protects the database and its CAS attachment sidecar (key
derived from a passphrase via Argon2id); deleting a memory is a tombstone or
**crypto-erasure**. The web console binds loopback with no auth by design and
refuses to expose itself to the network without an explicit opt-in.

Read the honest [threat model](docs/security-model.md) before deploying beyond a
local machine, and report vulnerabilities per our [security policy](SECURITY.md)
— **please don't open public issues for them**.

### Handling a data-subject request

Software can't *be* GDPR-compliant — a deployment is. What Areev gives you is
the mechanism, and the evidence:

```bash
areev subject-report "pat" --db memory.db --ns caller --out pat.jsonl --bundle pat.mgb
areev forget-subject  "pat" --db memory.db --ns caller --yes --because "Art. 17 request #42"
areev audit export --db memory.db --out evidence.jsonl
```

The report and the erasure run **one selector**, so what an access request
discloses is exactly what an erasure removes — including partition keys
(`pat#visit1`) and the full supersession history, and optionally prose
mentions. The `.mgb` bundle is the Art. 20 portability artifact. The audit
record names a *fingerprint* of the identity, not the identity: verifiable by
recomputation, unusable for enumeration — because an immutable, replicating
audit grain that named the subject would undo the erasure it records.

[`docs/gdpr.md`](docs/gdpr.md) is the article→capability map to lift into a
DPIA, including the deployment requirements (one hub per trust domain, TLS
proxy off-loopback, a documented archive-retention window) and the limits
stated honestly.

## Workspace

| Crate | What |
|---|---|
| `areev-core` | `.mg` format, canonical serialization, content addressing, 12 grain types, tool-schema rendering |
| `areev-store` | Turso-backed store: dictionary-encoded triples, hybrid recall, heads/forks, blobs (CAS), bundles/streaming, memory-tool adapter |
| `areev-cal` | CAL lexer/parser/executor, multi-source ASSEMBLE, saved queries, `AreevFacade` (+ read-only mounts) |
| `areev-context` | Budget-aware provider-optimal rendering (SML/TOON/Markdown/JSON) |
| `areev-loop` | The self-improvement engine — substrate-agnostic: analyzers, four gates, recommendation lifecycle, LLM verifier (no Areev deps) |
| `areev-loop-adapter` | Areev substrate adapter for Areev Loop + the recall-telemetry sidecar |
| `areev-llm` | Out-of-box LLM backends: Areev Loop reflection, `remember` extraction, and the runtime's tool-calling seam (OpenAI-compatible / Anthropic / Ollama) |
| `areev-run-core` | The pure `areev run` scheduler — sans-IO BSP step function, plan validation, frozen condition grammar; no clock/rand/IO in its dependency tree (CI-enforced) |
| `areev-run` | The `areev run` driver — journal, checkpoints, crash-resume, HITL respond, budgets, cancel, journal-consistent `verify`, shadow eval, OTel export |
| `areev-mcp` | Stdio MCP server — 23 tools: memory (`areev_recall/add/…`), the loop pair, DSAR/provenance reads, and the runtime six (`areev_run_*`) |
| `areev-server` | Local web console (memories / graph / query / Areev Loop queue / Runs approval queue / sessions) + areevd hub mode; per-principal auth, optional TLS (`tls` feature) and SSO trusted-header |
| `areev` | The `areev` binary |
| `areev-py` | Python bindings (`import areev`) |
| `areev-js` | Node bindings (napi-rs native addon, `require('@areev/areev')`) |
| `adapters/` | pip packages `areev-langgraph` (checkpointer, store, trace mirror) and `areev-crewai` (memory backend, audit listener) |

Built on [Turso Database](https://github.com/tursodatabase/turso) (MIT) — see
`THIRD-PARTY-NOTICES.md`.

## Contributing

Contributions are welcome under the [DCO](https://developercertificate.org/) — see
[CONTRIBUTING.md](CONTRIBUTING.md) and our [Code of Conduct](CODE_OF_CONDUCT.md).
Questions and ideas: [GitHub Discussions](https://github.com/AreevAI/areev/discussions).

## License

Licensed under either of [Apache License 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT) at your option. Unless you explicitly state otherwise,
any contribution you intentionally submit for inclusion is dual-licensed as
above, with no additional terms. The OMS specification itself is CC0.
