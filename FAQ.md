# Areev FAQ

Common questions about Areev, in question-and-answer form. For hands-on
recipes see the [Cookbook](docs/cookbook.md); for the design see
[`ARCHITECTURE.md`](ARCHITECTURE.md).

## Basics

### What is Areev?

Areev is an **embedded memory engine for AI agents**. You embed it in your
process (like SQLite, not like a database server), store memories as immutable
content-addressed **grains** in a per-memory file, query them with **CAL**, and
hand the results straight to your model — no network hop in the recall path. It
is the reference implementation of the [Open Memory Spec (OMS)](https://github.com/openmemoryspec/oms).

### Who is Areev for?

Anyone building an agent, assistant, or LLM app that needs durable, inspectable
memory: chat assistants that should remember users across sessions, voice agents
that can't afford a network round-trip per turn, coding agents that accumulate
project knowledge, and fleets that need to distribute shared knowledge to many
edges.

### What languages / platforms does it run on?

Areev is written in Rust and ships as a Rust library, the `areev` command-line
binary, an MCP server, and Python (`import areev`) and Node
(`require('areev')`) bindings. It runs anywhere Rust and Turso run — Linux,
macOS, and Windows on x86-64, plus **arm64** everywhere (Apple Silicon, arm64
servers, and 64-bit Raspberry Pi OS).

Edge devices are supported and measured, not assumed — Areev is benchmarked on
the hardware itself. On a **$35 Raspberry Pi 3 B from 2016** (1 GB RAM, 1.2 GHz
Cortex-A53, the slowest 64-bit Pi) recall is a flat **~361 µs from 500 to 8,000
grains** in ~50 MiB of RAM; on an **Intel NUC8i3BEH from 2018** it is **~30 µs**,
level with a 2024 M4 Max. Both install from the wheel in 16 seconds with no
compiler ([RESULTS.md §6](crates/areev-bench/RESULTS.md), which also projects a
Pi 5). Two caveats: install the wheel rather than building on-device
(`cargo install` peaks at 2.0 GiB RSS, so it cannot link in 1 GB), and 32-bit ARM
has no published artifact — use the 64-bit image.

### Is it free? What license?

Yes. Areev is open source under **MIT OR Apache-2.0** (your choice). The OMS
specification it implements is CC0 (public domain). See
[`LICENSE-MIT`](LICENSE-MIT) and [`LICENSE-APACHE`](LICENSE-APACHE).

## Concepts

### What is a "grain"?

A grain is the atomic unit of memory — one immutable, content-addressed record.
Its address is the SHA-256 hash of its entire serialized `.mg` blob, so
identical content always yields the same address, and any change produces a new
grain. There are 12 grain types: **Fact, Event, State, Workflow, Tool,
Observation, Goal, Reasoning, Consensus, Consent, Skill, Recommendation**. A `Fact` is a
subject–relation–object triple (e.g. `john · prefers · "window seat"`); an
`Event` is raw conversational content; and so on.

### What does "content-addressed / immutable" mean for edits and deletes?

Nothing ever mutates a stored grain. To **edit** a memory you *supersede* it:
write a new grain that points back to the old one. The old version stays in the
history — recall returns the current value, but the full lineage is queryable.
To **delete**, you use the host-level `forget`, which tombstones the grain in
the change log (and crypto-erasure destroys the key for encrypted data). This is
why Areev behaves like *git for memory*: you get an append-only log, diffable
history, and time-travel for free.

### What is OMS?

The **Open Memory Spec** — an open standard (CC0) for portable,
provenance-verified agent memory: the `.mg` binary format, canonical
serialization, content addressing, and the grain model. Areev is a conformant
reference implementation, verified byte-exact against the spec's test vectors.
Because the format is open, your memories are portable and not locked to Areev.

### What is CAL?

**CAL** (Context Assembly Language) is Areev's query language. Statements
include `RECALL`, `ASSEMBLE`, `EXISTS`, `HISTORY`, `ADD`, `SUPERSEDE`, and
`DESCRIBE`, plus pipeline stages like `| COUNT`. A key property:
**destruction is shaped** — it takes a hash (`FORGET <hash>`), an identity
(`FORGET SUBJECT "<id>"`), or an age (`PURGE OLDER THAN 90d`), and **never a
predicate**. `DELETE` is not a token in the grammar. Each destructive
statement needs the session's grant plus a recorded `BECAUSE`, writes an audit
record, and can be switched off per process with `--no-destructive-ops`. The
read-only mirror `REPORT SUBJECT` answers data-subject access requests over
the same selector the erasure uses (see [`docs/gdpr.md`](docs/gdpr.md)). `ASSEMBLE` can gather memories from several sources into a single
budgeted prompt in one statement. See [`docs/cal-reference.md`](docs/cal-reference.md).

```
RECALL facts WHERE subject = "john"
RECALL facts WHERE subject = "john" | COUNT
ASSEMBLE "prompt" FROM
  policies: (RECALL facts WHERE namespace = "org.policies" AND subject = "refunds"),
  profile:  (RECALL facts WHERE subject = "john")
```

### What is a namespace, and why "one memory = one file"?

Each Areev file is one self-contained memory: the unit of erasure, sync,
portability, and write parallelism (single writer per file). Within a file,
**namespaces** partition grains (e.g. `caller`, `org.policies`). Partition your
files however fits — by user, org, category, or conversation. Cross-file queries
go through `ASSEMBLE` with read-only mounts, not shared connections.

## Comparisons

### How is Areev different from a vector database?

A vector database stores embeddings and does similarity search; that's *one* of
Areev's recall legs, not the whole thing. Areev is a memory *engine* with a
typed grain model, immutability and full history, structural (subject/relation)
lookup in microseconds, BM25 text search, optional vector search, and provenance
— all in one embedded file you own. Vector search is optional: you bring your own
embedder through the `EmbedBackend` trait, and Areev works fine without one.

### How is Areev different from RAG?

RAG is usually "chunk documents, embed them, retrieve top-k passages." Areev
stores *structured, evolving memory* — facts, events, goals, consent — not
document chunks, and it tracks how each memory entered and changed over time.
You can absolutely use Areev as the retrieval layer of a RAG pipeline, but it
adds structure, history, provenance, and immutability that a plain vector index
does not.

### Why not just use SQLite / Postgres with a memory table?

You can build memory on a raw database, and Areev is in fact built on Turso (a
SQLite-compatible engine). What Areev adds is the parts you'd otherwise
hand-roll: an OMS-conformant immutable content-addressed format, hybrid recall
fused with RRF, supersession/history/forks/merge, a non-destructive query
language, budget-aware context rendering, an MCP server, and content addressing
for integrity. It's the memory-specific layer, not a general SQL store.

### Does it need a server or network service?

No. Recall happens in-process against a local file — that's the point. There
*are* optional networked surfaces (a local web console and a sync hub), but the
core read/write/recall path never leaves the process.

## Using it

### I'm on mem0 / Zep / Letta / LangMem — how do I switch?

`areev migrate` imports each system's export from a file (Areev never calls
your old provider's API), preserving original timestamps and provenance:

```bash
areev migrate --from mem0 --file export.json --history history.json --db mine.db
```

mem0's edit history replays as real supersession chains — ADD → add, UPDATE →
supersede, DELETE → forget — so your memory's pre-import evolution stays
queryable with `HISTORY`. Zep/Graphiti edges keep their bi-temporal validity;
Letta core-memory blocks and Basic Memory notes become live memory-tool files
under `/memories`; anything else imports via generic JSONL. Re-running an
import skips what's already there. Per-source export one-liners:
[`docs/migrate.md`](docs/migrate.md).

### How do I add and recall a memory from the CLI?

```bash
areev add    --db john.db --ns caller --subject john --relation prefers --object "window seat"
areev recall --db john.db --ns caller --subject john
areev recall --db john.db --ns caller --subject john --render sml   # model-ready context
```

`--ns` defaults to `shared`. See the [Cookbook](docs/cookbook.md) for more.

### How do I run a CAL query from the CLI?

```bash
areev cal 'RECALL facts WHERE subject = "john" | COUNT' --db john.db --ns caller
areev repl --db john.db          # interactive CAL shell
```

### How do I use Areev from Python?

`pip install areev` (Node: `npm install @areev/areev`), then:

```python
import areev, json
m = areev.Areev("john.db", ns="caller")
m.add_fact("john", "prefers", "tea", confidence=0.95)
print(m.recall("john"))                              # JSON string
print(m.cal('RECALL facts WHERE subject = "john"'))  # JSON string
```

The bindings follow a **scalars-in, JSON-strings-out** convention and are built
with [maturin](https://github.com/PyO3/maturin). Encryption at rest is available
through the constructor in both bindings — pass a `passphrase`
(`areev.Areev("john.db", ns="caller", passphrase="…")` in Python,
`new Areev("john.db", "caller", "…")` in Node); it derives an AES-256 key with
Argon2id, host-supplied and never stored in the file, exactly like the CLI's
`--passphrase-env`.

### How do I use Areev with Claude Code or another MCP client?

Areev ships a built-in MCP server on stdio. For Claude Code:

```bash
claude mcp add areev -- areev serve --mcp --db ~/.areev/code.db --ns claude-code
```

Any MCP client can launch `areev serve --mcp --db <file>` and get 6 tools:
`areev_recall`, `areev_add`, `areev_supersede`, `areev_forget`,
`areev_remember`, and `areev_cal`. See [`docs/mcp-reference.md`](docs/mcp-reference.md).

### Can I build a self-improving (adaptive) agent on Areev?

Yes — Areev is the memory *substrate* for the loop, not the loop itself. A
self-improvement loop is act → log experience → reflect → distill lessons →
recall them next time; the reflection step is a model call, and it is yours to
own — Areev calls no model unless you point it at one (`areev remember --model`
for extraction, `areev loop run --model` for reflection; the key comes from
the environment). The store's job is making that loop safe to run
unattended, because in a learning loop memory rot **compounds** — an agent that
re-learns duplicates or keeps stale lessons gets systematically worse:

- **Replay-idempotent writes** — a synced, imported, or retried write
  collapses to one grain by content address, so distribution can't
  double-store a lesson. For a *paraphrased* re-learning (new bytes), `areev
  novelty` reports the nearest existing lesson so the harness supersedes it
  instead of adding a near-duplicate — advise-only, so a write is never
  silently dropped.
- **Supersession** — a revised lesson *replaces* the old one at recall time
  (no stale co-ranking); proficiency tracked as a supersession chain makes
  `HISTORY` the agent's measured learning curve.
- **Provenance** — every lesson records why it was written (`REASON`) and links
  to the experience that taught it (`derived_from`). `areev provenance
  <source-hash>` lists every lesson distilled from a given observation — credit
  assignment when behavior goes wrong.
- **Rollback** — a single bad lesson is superseded or forgotten by hash; a whole
  bad *episode* is unlearned precisely via `areev provenance` (forget everything
  derived from that session) or, more bluntly, rolled back with point-in-time
  restore (`--until-hlc`).

The full verified loop — experience Events, lesson facts, a proficiency chain,
act-time recall, and the rewind — is
[cookbook §10](docs/cookbook.md#10-build-an-agent-that-learns-and-can-unlearn).

### How does recall work (hybrid RRF)?

`recall_hybrid` runs up to three legs — **structural** (exact subject/relation
lookup), **BM25** full-text (when a text index exists), and **vector** similarity
(when you've installed an embedder) — and fuses their rankings with **Reciprocal
Rank Fusion (RRF)**. It is *deadline-bounded and fail-open*: a leg that misses
its time budget is skipped and partial results are returned rather than erroring.
Plain structural `recall` (no fusion) is the microsecond-class point read.
Optional post-fusion refinements (query expansion, MMR diversity, cross-encoder
rerank) are all off by default and also fail-open.

### Is it multilingual?

Yes. Structural and BM25 legs handle spaced scripts (e.g. English and Arabic);
unspaced scripts like Chinese ride the vector leg. Bring any multilingual
embedder through the `EmbedBackend` trait.

## Operations & security

### Is my data encrypted?

Optionally, at rest. Add `--passphrase-env <VAR>` to any CLI command to derive
an AES-256 key (Argon2id) from the passphrase held in environment variable
`<VAR>`; the non-secret salt is kept in a `<db>.kdf` sidecar (back it up with the
database). Important caveats: the `.blobs` sidecar for large payloads is **not**
encrypted, and encryption-at-rest uses the storage engine's AES-256-GCM, which is
an experimental Turso feature — treat it as defense-in-depth, not a substitute
for full-disk encryption. Read [`SECURITY.md`](SECURITY.md) before relying on it.

### How do I back up or sync memories?

Several ways, all built on the append-only op-log:

- **Bundle** — `areev bundle --out backup.mgb` writes a portable incremental
  backup; `areev import --bundle backup.mgb` fast-forwards another file.
- **Stream** — `areev stream --to <dir>` continuously ships op-log segments
  (Litestream-shaped, with generations); `areev restore --from <dir>` rebuilds,
  including point-in-time restore via `--until-hlc`.
- **Follow** — `areev follow --from <dir>` subscribes and applies new segments
  (fleet-wide knowledge distribution).

See the [Cookbook](docs/cookbook.md) for full recipes.

### What happens on concurrent edits — do I lose data?

No. Concurrent supersedes of the same memory become **branches (heads)** with a
deterministic provisional head, so every node agrees on the current value with
zero coordination, and both tips survive — nothing is silently lost. Find open
forks with `areev forks` and close one with `areev merge --subject S --relation R
--object O` (an explicit supersession that records all parents). Recall does not
stamp a contested marker — that would put a per-hit head probe on the
microsecond hot path — so surfacing is the explicit `areev forks` query. Note
this covers concurrent *supersedes* of one chain: two independently *added*
facts about the same subject are separate grains and both surface as current
(the engine can't structurally tell an intended multi-value from a
contradiction — that's a semantic, host-side, judgment).

### Can I inspect or edit memories in a UI?

Yes. `areev ui --db <file>` serves a local web console (memories, graph, query,
and Areev Loop tabs, light + dark themes) at `http://127.0.0.1:7437`. It binds
loopback. **Token-less, the console is read-only** — it browses everything but
cannot write; pass `--token-env VAR` to unlock writes (review/apply and CAL
writes). Do not expose it to a network without a TLS-terminating reverse proxy.
See [`SECURITY.md`](SECURITY.md).

### Can my agent improve its own memory safely?

Yes — that is [Areev Loop](docs/loop.md), built into Areev. It turns your
agent's history into **recommendations** (deduplicate these facts, resolve this
contradiction, this tool keeps failing, this lesson is stale) with **no model
calls** — the analyzers are deterministic. Every recommendation cites its
evidence, every decision needs a written reason, every apply is audited and
(unless it's a `FORGET`) undoable. Run it with `areev loop run`,
`db.loop_run()`, or the `areev_loop` MCP tool.

### Does Areev Loop edit my agent's memory or prompt on its own?

No, not by default. Everything lands in a review queue you approve. Auto-apply
is off unless you opt in with a host `loop-policy.json`, and even then it is
restricted to structural memory curation (deduplication, fork merges) — it can
**never** auto-apply anything carrying model-derived or tool-derived free text,
touch a prompt/instruction file, or run a destructive `FORGET`. Those always
require a human. See [`docs/security-model.md`](docs/security-model.md).

### What are the latency characteristics?

Recall is in-process and fast. Measured on an Apple M4 Max: structural recall
around **30µs**, the `entity_latest` point read around **9µs**, and a
50ms-cadence voice loop with live write-back recalls at about **79µs p50 /
152µs p99** per frame. Benchmarks and methodology live in `crates/areev-bench`
and the `crates/areev-store/examples` latency gates (`bench`, `voice_loop`).

### How accurate is recall quality?

On the public [LoCoMo](https://github.com/snap-research/locomo) long-conversation
benchmark, a plain retrieve-then-read pipeline scored around 74.5% / 81.6%
retrieval hit@10 / hit@20 (with `text-embedding-3-small`) and ~54.2% end-to-end
answer accuracy at k=20. Bring your own models and embedder. Every answer and
judge verdict is committed for audit under `crates/areev-bench/results/`.

## Project

### Is Areev production-ready?

**It is `1.0.0`** — the `.mg` format, canonical serialization, CAL syntax, and
error codes are stable contracts, and the engine is built and tested (a large
test suite runs locally). Two honest caveats remain, so read
[`SECURITY.md`](SECURITY.md) before deploying beyond a trusted environment:
encryption-at-rest rides an experimental storage-engine (Turso) AES-GCM feature
and the `.blobs` sidecar is plaintext, and the network surfaces are not yet
hardened for hostile multi-tenant deployment. Safe for local and
trusted-environment use today.

### Is Areev stable — can the format change under me?

No. The **on-disk `.mg` format, canonical serialization, CAL syntax, and error
codes are stable as of `1.0.0`** and follow semantic versioning — a breaking
change to any of them would be a major version bump. Changing the format would
break every content address and OMS conformance, so it won't happen silently.
Because the format is an open spec, your data stays portable regardless.

### What does Areev deliberately *not* do?

It takes no LLM dependency: it does not run models for you. Fact extraction is
supplied by the host (you pass your extractor's output), and embeddings come from
a host-installed embedder. LLM-dependent recall options fail loudly with a clear
error rather than silently doing nothing. This keeps the engine small,
predictable, and free of hidden model costs.

### How do I contribute?

Contributions are welcome — bugs, docs, tests, code. Sign off your commits
(`git commit -s`) under the Developer Certificate of Origin, keep the test suite
green, and **don't run blanket `cargo fmt`** (the tree is intentionally not
rustfmt-clean; format only the lines you touch). Full guidelines are in
[`CONTRIBUTING.md`](CONTRIBUTING.md); working *in* the codebase is described in
[`AGENTS.md`](AGENTS.md).

### Where do I report a security issue?

Privately — please do not open a public issue. See [`SECURITY.md`](SECURITY.md)
for the disclosure process.

### Where can I learn more?

- [`README.md`](README.md) — overview and quickstart
- [`ARCHITECTURE.md`](ARCHITECTURE.md) — design and internals
- [`docs/cal-reference.md`](docs/cal-reference.md) — the CAL query language
- [`docs/mcp-reference.md`](docs/mcp-reference.md) — the MCP tools
- [`docs/cookbook.md`](docs/cookbook.md) — task-oriented recipes
- [`ERROR_CODES.md`](ERROR_CODES.md) — the `DOMAIN-Ennn` error registry
