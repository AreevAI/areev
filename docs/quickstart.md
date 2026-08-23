# Getting started with Areev

Everything on this page is copy-pasteable. It covers installing Areev on each
registry, the three-command CLI quickstart, wiring the MCP server into Claude
Code, embedding the store from Rust / Python / Node, the PostgreSQL backend,
encryption at rest, and durability/fleet sync. Task-oriented recipes beyond
these live in the [cookbook](cookbook.md).

## Install

Areev ships on all three registries — install the surface you need:

```bash
cargo install areev          # the `areev` CLI
pip install areev            # Python bindings
npm install @areev/areev     # Node bindings (unscoped `areev` is pending an npm exception)
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

## The CLI in three commands

Store a fact, recall it, hand it to a model — no ceremony (`--db` is optional;
it falls back to `$AREEV_DB`, then `~/.areev/default.db`):

```bash
areev add    john prefers "window seat"     # subject relation object
areev recall john                           # → the stored fact, one JSON grain per line
areev recall john --render sml              # → "john prefers window seat" as a model-ready block
```

Point it at a specific file with `-d mem.db` (or `export AREEV_DB=mem.db`).
Then explore: `areev cal '<QUERY>'` runs the query language
([reference](cal-reference.md)), `areev ui` opens the web console
(http://127.0.0.1:7437), and `areev repl` is an interactive CAL shell. The
global `--embed-cmd 'my-embedder'` flag installs your embedder for vector
recall on any verb (text on stdin, JSON vector on stdout).

## Give Claude Code (or any MCP client) persistent memory

```bash
claude mcp add areev -- areev serve --mcp --db ~/.areev/code.db --ns claude-code
```

`areev serve --mcp` speaks newline-delimited JSON-RPC 2.0 on stdio and works
with any MCP client — 25 tools, with a 12-tool `--profile memory` subset for
hosts that only want chat memory. See [`mcp-reference.md`](mcp-reference.md).

## Run a governed workflow (`areev run`)

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
  server](mcp-reference.md) (`areev_run_*` — host tools execute only
  via `$AREEV_RUN_TOOL_CMD`), the Python/Node bindings (`db.run_start(…)` /
  `await m.runStart(…)`), and the web console's Runs tab, which is the human
  approval queue (shared-token and anonymous callers are refused for
  approvals — the approver's identity *is* the audit record).

Because the plan, the journal, and the memory share one file, the run/memory
join comes free: an agent's tool call cites the run that made it, and a
fact's provenance names the runs that touched it. Full guide:
[`run.md`](run.md) · standing rules that start runs on a schedule or an
event: [`triggers.md`](triggers.md) · compliance maps:
[`eu-ai-act.md`](eu-ai-act.md), [`procurement.md`](procurement.md).

## Keep your LangGraph or CrewAI stack — govern its state

You don't have to adopt the runtime to get the governance. Two pip adapters —
[`areev-langgraph`](https://pypi.org/project/areev-langgraph/) and
[`areev-crewai`](https://pypi.org/project/areev-crewai/) — put Areev
underneath the framework you already run:

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

Both adapters live outside this repo and are **not under active
development** — 1.0.0 is on PyPI and works against current Areev, but new
releases wait on demand. If you want either moved forward, say so in an
[issue](https://github.com/AreevAI/areev/issues) and we will un-park them.

## Rust

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

## Python

```python
import areev, json
m = areev.Areev("john.db", ns="caller")
m.add_fact("john", "prefers", "tea", confidence=0.95)
m.recall("john")                     # JSON string, newest-first — needs a subject
m.search("tea", k=5)                 # free text, when you don't have a subject.
                                     # BM25-only out of the box, so it matches
                                     # words that are present; install an
                                     # embedder for semantic hits like
                                     # "hot drinks".
m.cal('RECALL facts WHERE subject = "john"')
m.memory_tool(json.dumps({"command": "view", "path": "/memories"}))  # Anthropic memory-tool backend
```

`Areev(..., index_text=False)` turns the BM25 index off for this file (a
deliberate re-stamp, reported by `open_warnings()`). That trades `search()`'s
text leg — keep it working by installing an embedder — for write latency that
stays flat as the file grows. `add_batch(...)` writes many grains in one
transaction; to load another system's export, prefer `migrate()`
([migration guide](migrate.md)).

## Node

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

## PostgreSQL backend (server tier)

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
  [erasure.md](erasure.md) for the scope contract and the documented OMS
  deviation.
- **HA is inherited**: run it on a regionally-replicated Postgres and the
  memory inherits the failover, PITR, and backup story your ops team already
  drilled.

Deployment modes, auth, and what each mode may claim:
[`deployment-profile.md`](deployment-profile.md).

## Encryption at rest

```bash
export AREEV_KEY="correct horse battery staple"
areev add --db secret.db --ns caller --subject john --relation prefers \
  --object "window seat" --passphrase-env AREEV_KEY   # AES-256-GCM, Argon2id key
```

The passphrase-derived key covers the database and its CAS attachment sidecar;
destroying the key is crypto-erasure of the memory. Threat model and caveats:
[`security-model.md`](security-model.md).

## Durability & fleets

```bash
areev stream  --db john.db --to  s3-mounted/john/     # continuous op-log shipping (~Litestream, grain-level)
areev restore --db new.db  --from s3-mounted/john/ [--until-hlc T]   # incl. point-in-time
areev follow  --db org-replica.db --from org-pub/     # subscribe: org knowledge → every edge
areev verify  --db john.db                            # integrity + full content-address recheck
```

One memory = one file: the unit of erasure (crypto-erase = key destruction),
sync, portability, and write parallelism. Partition by user, org, category, or
conversation — your call.

## Where to next

- **Build an agent that learns — and can unlearn, by hand**:
  [cookbook §10](cookbook.md#10-build-an-agent-that-learns-and-can-unlearn--by-hand)
  (experience log → distilled lessons → proficiency chain → rollback).
- **Turn on governed self-improvement**: `areev init --db demo.db
  --template demo`, then `areev loop run` — the full guide is
  [`loop.md`](loop.md).
- **Import an existing corpus** from mem0, Zep/Graphiti, Letta, LangMem, or
  JSONL, with its edit history: [`migrate.md`](migrate.md).
- **See a real agent end to end**: the
  [invoice-to-accounting example](../examples/agents/invoice-to-accounting/)
  runs keyless in CI, both chapters.
