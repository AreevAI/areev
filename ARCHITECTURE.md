# Areev Architecture

Areev is an embedded memory engine for AI agents and the reference
implementation of the **Open Memory Spec (OMS)** — the open standard for
portable, provenance-verified agent memory. It stores memories as immutable,
content-addressed *grains* in per-file [Turso](https://github.com/tursodatabase/turso)
databases, queries them with **CAL** (the Context Assembly Language), and
renders the results into model-ready context in-process. There is no server in
the recall path.

This document describes the system for developers who want to understand,
embed, or contribute to Areev. It covers the data model, the storage layer,
the query language, recall, versioning, context rendering, the crate layout,
and the design decisions that shape all of it.

Related references:

- [CAL query language reference](docs/cal-reference.md)
- [MCP server reference](docs/mcp-reference.md)
- [Areev Loop: governed self-improvement](docs/loop.md)
- [Security model & threat model](docs/security-model.md)
- [Vulnerability reporting](SECURITY.md)

---

## System at a glance

Areev is one loop: memory is **assembled** into a prompt on the read path,
and every action feeds back into memory on the write path — which **Areev Loop**
then governs, verifies, and improves.

```
        D E J A D B   —   in-process recall · governed, verified self-improvement

   recall path (no server, no network) ─────────▶   the assembled context   ──▶
                                                     (one budgeted token block)
   ┌──────────────────┐     ╔══════════════╗                            ╔═══════════╗
   │  S T O R A G E   │read▶║ CAL·ASSEMBLE ║ ───────────────────────────▶║    LLM    ║
   │  per-file Turso  │     ╚══════════════╝                            ╚═════╤═════╝
   ├──────────────────┤      hybrid recall  structural · BM25 · vector · RRF   │ tool
   │ FACTS   EVENTS   │      per-source  BUDGET + PRIORITY · dedup             ▼ calls
   │ SKILLS  TOOLS    │      FORMAT sml│toon│markdown│json                ╔═══════════╗
   │ WORKFLOWS STATE  │◀── results · events · facts captured (ADD) ────────╢  ACTIONS  ║
   │ GOALS   …   (12) │    immutable · content-addressed grains            ║ host runs ║
   └──┬────────────▲──┘                                                    ╚═══════════╝
      │            │
   op-log +   governed writes back — SUPERSEDE · ADD · FORGET
  telemetry        ▲   (only through the four gates)
      │            │
      ▼            │
   ╔═════════════════════════════════════════════════════════════════════════════╗
   ║  W A I S E R  —  governed · verified · measured self-improvement     [built] ║
   ║   ANALYZE    11 deterministic analyzers over typed grains — 0 LLM calls      ║
   ║   DISCOVER   optional LLM: propose → GROUND → VERIFY  (proposer ≠ scorer)    ║
   ║   RECOMMEND  recommendation grain + cited evidence + severity                ║
   ║   GOVERN     four gates — propose · review (BECAUSE) · apply · verify        ║
   ║   MEASURE    re-run the metric at 1d / 7d / 30d · revert on regression       ║
   ╚═════════════════════════════════════════════════════════════════════════════╝

   recall — in-process, p50 ~33µs · no server in the recall path
   write  — append-only · immutable history · full provenance · forks surface contradictions
   areev-loop — deterministic core needs no model · the LLM only proposes, never gates or applies ·
            every change is evidence-cited, reviewed, undoable, and re-measured · no daemon
```

**Reading the diagram — one clockwise cycle with an inner governed loop:**

1. **Storage (left).** The memory itself — immutable, content-addressed grains
   in per-file Turso DBs (§2–§3). The labeled streams are the grain types that
   actually reach a model; `(12)` marks the rest.
2. **Recall (`CAL · ASSEMBLE`, §5–§7).** Reads storage and joins the streams
   under per-source token budgets and priorities, in-process — no server, no
   network. What travels the arrow is not "a prompt string" but one
   token-accounted budgeted block; that block *is* the prompt.
3. **Actions (write path).** The model calls tools; execution is always the
   **host's** job (Areev stores tool grains, never runs them). Results and new
   events are **captured back** as fresh grains.
4. **Areev Loop (inner loop, §8).** Tails the op-log and recall telemetry, runs
   deterministic analyzers (optionally an *independently verified* LLM), and
   writes vetted improvements back — but only through four governance gates.
   Unlike an autonomous consolidation daemon, it has no scheduler: a run is a
   cheap, idempotent command a host triggers on a hook, cron, CI, or MCP call.

---

## 1. Design goals

Areev is shaped by three constraints, in priority order:

1. **In-process, microsecond recall.** The flagship consumer is a real-time
   voice loop that cannot pay a network round trip. The primary interface is a
   Rust handle (`Areev::open(path)`); MCP, HTTP, and language bindings are
   thin layers over the same engine.
2. **Portable, verifiable memory.** Every memory is a file the user owns.
   Grains are content-addressed and immutable, so memory can be exported,
   backed up, synced, and audited without trusting any single service.
3. **Safe-by-default for agents.** The query surface's only destructive verb is
   a single-grain `FORGET`, gated by a per-process switch (default on) and
   backed by no bulk-erasure primitive — enforced by the grammar and type
   system, and fully disable-able for untrusted input.

Everything below follows from these constraints.

---

## 2. The core model: immutable content-addressed grains

A **grain** is the atomic unit of memory: one fact, one event, one state
snapshot, one tool call. Grains are:

- **Immutable.** A stored grain is never edited in place. Every "update" is a
  new grain that *supersedes* the old one; every "removal" is a tombstone or a
  cryptographic erasure. Store code mutates only the *index layer* that points
  at grains — never the grain blobs themselves.
- **Content-addressed.** A grain's identity is the SHA-256 hash of its entire
  serialized blob (header included). The address *is* the content: two
  byte-identical grains collapse to one address, and any change to a grain
  produces a different address. This is what makes memory tamper-evident and
  deduplicated by construction.

### 2.1 The `.mg` blob format

Each grain serializes to a `.mg` blob:

```
blob = 9-byte header  ++  canonical MessagePack payload
address = SHA-256(entire blob, header included)
```

The 9-byte header is fixed-width and self-describing:

| Bytes | Field | Meaning |
|---|---|---|
| 0 | `version` | Format version (currently `0x01`) |
| 1 | `flags` | Bit flags (see below) |
| 2 | `grain_type` | The grain type byte (`0x01`–`0x0C`) |
| 3–4 | `ns_hash` | First 2 bytes of SHA-256(namespace), big-endian |
| 5–8 | `created_at_sec` | Creation time, epoch **seconds**, big-endian u32 |

Flag bits: `signed` `0x01`, `encrypted` `0x02`, `compressed` `0x04`,
`has_content_refs` `0x08`, `has_embedding_refs` `0x10`, `ai_generated` `0x20`,
and bits 6–7 encode a sensitivity level. The payload carries full timestamps in
epoch **milliseconds**; the header's second-resolution timestamp is a coarse
sort/filter key.

### 2.2 Canonical serialization

Because the content address is computed over the serialized bytes, the
serialization must be **canonical** — the same logical grain must always
produce the same bytes on every machine. Areev freezes these rules:

- **NFC normalization.** Every string is Unicode-NFC-normalized before hashing,
  so composition variants of the same text collapse to one address.
- **Sorted map keys.** Maps are emitted in sorted key order (built as
  `BTreeMap`).
- **Compact keys.** Field names serialize to short canonical forms (a fixed
  long↔short table). A handful of fields stay uncompacted by design.
- **Omit-when-default.** `None`/empty fields and default enum values are
  omitted from the payload entirely.

These rules are a conformance contract: changing any of them would silently
change the content address of every grain ever written and break OMS test-vector
conformance. They are treated as frozen unless the spec itself moves.

### 2.3 The 12 grain types

OMS 1.5 defines 12 grain types, each with a stable header byte. The type byte, the
canonical name, and the fields are part of the format contract.

Fields in **bold** are *required* — the write path rejects a grain without them
(`VAL-E001`). The rest are the type's other characteristic fields.

| Byte | Type | Purpose | Key fields |
|---|---|---|---|
| `0x01` | **Fact** | A subject–relation–object triple: durable structured knowledge | **`subject`**, **`relation`**, **`object`**, `confidence` |
| `0x02` | **Event** | A conversational or system event; the transcript unit | **`content`**, `role`, `session_id`, `created_at` |
| `0x03` | **State** | An agent state snapshot / checkpoint | `context`, `plan`, `history` |
| `0x04` | **Workflow** | A DAG of steps bound to tool definitions | `nodes`, `edges`, `bindings`, `retries`, `trigger` |
| `0x05` | **Tool** | A tool definition, call, or result across its lifecycle | **`tool_name`**, `tool_phase`, `input`, `is_error` |
| `0x06` | **Observation** | A raw observation from a sensor or observer | **`content`**, `observer_id`, `observer_type`, `value`, `unit` |
| `0x07` | **Goal** | A goal or task with state and dependencies | **`description`** (or `object`), `goal_state`, `deadline`, `depends_on` |
| `0x08` | **Reasoning** | A recorded inference (premises → conclusion) | `reasoning_type`, `premises`, `conclusion` |
| `0x09` | **Consensus** | An agreement across multiple observers | `threshold`, `agreement_count`, `participating_observers` |
| `0x0A` | **Consent** | A consent / authorization record (DID-scoped) | **`subject_did`**, **`user_id`**, `consent_action`, `purpose`, `grantor_did`, `grantee_did` |
| `0x0B` | **Skill** | A packaged, reusable agent capability with learned proficiency | **`name`**, **`description`**, `domain`, `proficiency`, `transferable` |
| `0x0C` | **Recommendation** | A governed, auditable proposal to change memory or agent config | `target_ref`, `analyzer`, `summary`, `dedup_key`, one `proposal_*` |

All 12 types share a common envelope (`namespace`, timestamps, provenance,
supersession links, optional content/embedding references). The type-specific
fields above are what each type adds on top.

`State`, `Workflow`, `Reasoning` and `Consensus` deliberately require nothing:
they are container types whose payload shape is the host's, so an empty one is
a legal (if useless) grain rather than a validation error. Everything else has
at least one required field, and `Recommendation` is engine-emitted — there is
no `ADD recommendation`.

The required set is not only prose: `DESCRIBE <type>` reports it as
`required_fields`, so a client can ask the engine instead of reading this
table, and a test pins the two together.

> **Tool grains are data, never executables.** Areev stores, correlates, and
> renders tool definitions/calls/results — it never runs them. A Tool grain's
> `tool_phase` distinguishes a `definition` (name + input/output schema) from a
> `call` (input + correlation id) from a `result` (output + `is_error`). The
> engine can render stored definitions to nine provider tool-schema formats
> (OpenAI, Anthropic, Gemini, MCP, and text variants) for tool-RAG, but
> execution is always the host's job.

> **A plan never accumulates its runs.** A Workflow grain is immutable and
> content-addressed, so run state cannot be written into it. Execution records
> point the other way: a Tool grain carries a `related_to` link of type
> `mg:step_action:<node_id>` targeting the Workflow's hash (OMS §8.4), read back
> via `step_actions()`. Retries and parallel branches are simply several records
> against the same node — none supersedes another. Per OMS §15.3 the link is an
> annotation: it is indexed for retrieval and never alters the plan's
> supersession state. With `Event.run_id` for correlation and a State grain as
> the resumable checkpoint, a run is persisted entirely in existing types.

---

## 3. Storage: one memory = one file

Each memory is a single Turso (SQLite-lineage, embedded, MIT-licensed) database
file. This is the load-bearing decision that makes the rest coherent:

> **One file is simultaneously the unit of erasure, sync, portability, write
> parallelism, and retention.**

- **Erasure** is file-granular: crypto-erase a memory by destroying its key.
- **Sync/backup** operates on a file's grain stream.
- **Write parallelism** is one writer queue per file; there is no cross-file
  transaction to coordinate.
- **Portability**: a memory is one file you can copy, hand to a user, or import
  into any OMS implementation.

Applications partition memory into files along whatever boundary their domain
needs — per user, per organization, per category, per conversation. Within a
file, hot queries partition further by namespace, session, and thread. When a
session needs to span several files, it does so through
[ASSEMBLE with facade mounts](#55-assemble-and-facade-mounts), not through
shared connections.

### 3.1 The index layer

Grains are opaque immutable blobs; everything queryable is a *derived index*
maintained on write. The store keeps, among others:

- **Dictionary-encoded triple indexes.** Fact subject/relation/object strings
  are mapped through a terms dictionary to fixed-width integer ids, and stored
  as narrow permutation indexes (SPO + POS, with a selective OSP permutation
  for entity-valued objects). This is the "hexastore-equivalent" the spec
  permits — the permutations CAL's bounded traversal actually needs, rather
  than the full six.
  A grain that names a `subject` but asserts no relation or object is indexed
  too, as a subject-anchored row with both other positions NULL. Requiring a
  complete triple dropped such grains from the index entirely, and because
  erasure and DSAR disclosure select through these same indexes, the cost was
  not only a silent empty recall but an identity's own Event surviving
  `forget_subject` — see `docs/erasure.md`.
- **`entity_latest`** — the current head(s) per `(subject, relation)`, so
  "current value of X" is a point read.
- **A full-text index** (BM25) and a **vector index** for hybrid recall.
- **A thread index** `(namespace, session_id, seq)` for transcript-tail and
  session-directory queries.
- **An op-log** with a hybrid logical clock (HLC) and tombstones — the ordered,
  replayable record that powers sync and point-in-time restore.

Because user strings are dictionary-encoded to integer term-ids before they
reach the triple queries, and all store access uses parameterized SQL, there is
no SQL-injection surface.

### 3.2 Content-addressed blob sidecar (CAS)

OMS keeps grains small (~100-byte class) and references media by URI. Areev
implements the reference target: a per-memory content-addressed `blobs/`
sidecar. Media is stored once, addressed by `cas://sha256:...`, deduplicated by
construction, garbage-collected by ref-count from live grains, and read back
hash-verified. Recall never scans bytes — searchability comes from *derived
text* (transcripts, extractions) stored in grain content and from embedding
references. See the [security model](docs/security-model.md) for the current
plaintext-sidecar limitation.

Because the payloads live *beside* the file rather than in it, a blob can be
read **without opening the memory** (`read_blob_offline`, `areev blob get`).
That is not a convenience: the embedded backend's file lock is exclusive, so
while a run holds a memory even a reader is refused — which would put an
attachment out of reach of the very `--tool-cmd` subprocess the run spawned to
process it. No read-only open mode is needed to fix that, because a blob has
nothing to be consistent *with*: it is immutable, and its address is its
checksum, which the read re-verifies. An encrypted memory is the exception —
decrypting the sidecar needs the derived key, so that path still opens.

---

## 4. Versioning: heads, forks, supersession, tombstones

Because grains are immutable, "change" and "delete" are modeled as new state in
the index layer, never as edits.

- **Supersession.** To evolve a memory, write a new grain whose `derived_from`
  points at the old one. The store sets the old grain's index-layer
  `superseded_by` pointer and system-valid-to timestamp. The old blob is
  untouched and fully recoverable — supersession builds an append-only version
  history, and `HISTORY OF <hash>` walks it.
- **Heads.** `entity_latest` is a *heads set* per `(subject, relation)`, not a
  single row. In the common single-writer case there is exactly one head.
- **Forks.** When two writers concurrently supersede the same head (v1 → v2a
  and v1 → v2b), immutability means **both tips survive** — the conflict
  structurally cannot destroy either version. Reads never block: recall serves
  a **provisional head** that every node computes identically (HLC, then hash
  tiebreak — zero coordination). Resolution is an explicit **merge
  supersession** that records both parents and closes the fork — auditable
  forever. For an agent, cross-channel disagreement is context, not an error.
  *Surfacing:* `areev forks` enumerates every open fork and `areev merge
  --subject S --relation R --object O` closes one. Recall itself does **not**
  stamp a contested marker by default — that would add a per-hit head probe to
  the microsecond hot path — so surfacing is opt-in rather than a recall-time
  cost. Two CAL surfaces make it reachable by the agent itself, not just an
  operator: `RECALL … CONTRADICTIONS` returns **only** contested grains
  (optionally scoped by a `OF (sub-query)` tail), and `WITH
  contradiction_detection` returns the normal result set with disputed grains
  stamped. Both fill `contested_by` on each grain with the other live tips, so
  a model sees *what* disagrees, not merely that something does. One
  `GROUP BY … HAVING COUNT(*) > 1` over `heads` per query, applied after every
  other filter, and fail-open like the rest of the recall path.
- **Tombstones and erasure.** Removal is never an in-place delete (which would
  leave recoverable data in free pages and the WAL). `forget` writes a
  tombstone to the op-log and drops the grain from the hot index. The strong
  erasure path is cryptographic: encrypt the memory with a per-file key and
  destroy the key.

The grain set is a grow-only structure: **adds are pure set union and have no
conflict class at all.** The only semantic conflict — concurrent supersession
of one head — resolves deterministically and surfaces as a first-class fork.

---

## 5. CAL: the Context Assembly Language

CAL is the query language and the primary API surface — it is what makes Areev
a database rather than a library. A CAL statement runs a pipeline:

```
text → length check → bidi rejection → NFC normalize → lex → parse
     → CalQuery (AST) → execute → pipeline stages → format → result
```

Full syntax, statement types, and safety limits are in the
[CAL reference](docs/cal-reference.md). The architectural essentials:

### 5.1 Read and write tiers

- **Read tier**: `RECALL`, `ASSEMBLE`, `EXISTS`, `HISTORY`, `DESCRIBE`,
  `COALESCE`, set operations, and a post-statement pipeline (`| SELECT`,
  `| ORDER BY`, `| LIMIT`, `| COUNT`, …).
- **Write tier**: `ADD` and `SUPERSEDE` (append-only). Every write requires a
  `REASON`/`BECAUSE` clause, so the provenance of a change is captured in the
  change itself.

### 5.2 The narrow, gated destructive surface

CAL's destructive surface is deliberately tiny and defense-in-depth gated. The
**only** destructive statement is `FORGET <hash>` — a single-grain tombstone
(`Areev::forget`). Everything larger is kept out, and even FORGET is gated:

1. **Lexer.** A destructive-keyword blocklist (`DELETE`, `ERASE`, `TRUNCATE`,
   `INSERT`, `CREATE`, `GRANT`, …) has no statement tokens in the grammar —
   the words only ever lex as inert identifiers, hard-rejected by the parser
   before any dispatch. The deletion verb is `FORGET`.
2. **Parser.** Those identifiers are fast-rejected with a dedicated error.
   `FORGET <hash>` parses; the bulk/scope forms (`FORGET USER/SCOPE`, `PURGE`)
   exist in the AST but the text parser still refuses them, and `DROP` accepts
   only `TEMPLATE`/`QUERY`. Saved-query bodies are re-checked read-only.
3. **Execution gate.** FORGET/DROP/PURGE execute only when
   `CalExecutorConfig::allow_destructive_ops` is set. It defaults to **on**, but
   any host can flip it off per-process (`areev serve --mcp --no-destructive-ops`,
   likewise `areev ui` / `areev cal`), yielding a read-only session in which every
   destructive statement returns `Unsupported`. On the server path, FORGET
   additionally requires the `admin` capability scope.

The same capability backs both surfaces: the Rust API, the MCP `areev_forget`
tool, and CAL `FORGET` all reduce to `Areev::forget(hash)`. Bulk erasure by
user or scope is intentionally **not** implemented — there is no store primitive
for it — so a single query cannot wipe a namespace. A CAL session can be
pinned to a namespace via `CalExecutorConfig::namespace_override` (enforced on
the server path; not yet wired to the MCP/CLI surfaces, where the caller picks
its namespace). Sensitivity is recorded per grain in the header; recall-time
enforcement of a sensitivity ceiling is host-side today. Against untrusted
input the operator can disable deletion entirely with one flag.

### 5.3 Safety limits

The parser and executor enforce hard bounds so a hostile or runaway query
cannot exhaust resources: max query length (64 KiB), max nesting depth (8), max
result limit (1000), max pipeline stages (5), max `LET` bindings (5) with a
1000-grain cap per binding, and more. The full table is in the
[CAL reference](docs/cal-reference.md#safety-limits). Two Unicode invariants run
before tokenization: bidirectional-override rejection (defeats visual spoofing)
and NFC normalization.

### 5.4 New syntax is a spec decision, not a product decision

CAL syntax is an OMS conformance contract: a query that parses here must parse
the same way in any other conforming implementation, or the contract is worth
nothing. So the rule is that Areev does not *invent* CAL syntax — it
implements what the spec already defines, and anything Areev genuinely needs
first goes to the spec before it goes in the parser.

Everything added to the surface since 1.0.3 is on the implements-the-spec side,
and each carries the section it comes from: sectioned templates and inheritance
(§10.6–§10.7), the `{{grain.*}}` / `{{assembly.*}}` / `{{source.*}}` /
`{{budget.*}}` namespaces and content projection (§10.2–§10.5), the template
limits (§10.8), the semantic presets `structured` / `readable` / `compact` /
`data` (§10.1), `FORMAT TEMPLATE` in its three forms (§10.6, §10.6.1),
`AS <format>` (§7 `as_clause`), and `RECALL *` (§4). `CONTRADICTIONS` was
already in the grammar and unimplemented; wiring it changed no syntax.

The corollary is that the extension points are deliberately *not* syntax.
Saved queries, custom templates and presets let a deployment shape its own
output vocabulary without touching the grammar — which is why they persist in
the memory file rather than in a client, and why `data` cannot be extended
(it is a renderer, not a template).

### 5.5 ASSEMBLE and facade mounts

`ASSEMBLE` is CAL's context-composition statement: it draws from multiple
labeled sources, applies per-source token budgets and priorities, deduplicates,
and renders a single budgeted block ready for a model prompt.

Cross-file recall goes through **facade mounts**, not shared connections. A
`AreevFacade` wraps one writable session store and any number of *read-only*
mounted stores:

```rust
facade.mount("org", org_replica);   // read-only
// CAL reaches the mount via the `alias.inner` namespace inside a source:
//   ASSEMBLE "prompt" FROM
//     policies: (RECALL facts  WHERE namespace = "org.policies" RECENT 10),
//     profile:  (RECALL facts  WHERE subject = "john"),
//     session:  (RECALL events WHERE session_id = "call-42" RECENT 10)
//   BUDGET 1500 tokens
//   PRIORITY profile: 0.5, session: 0.3, policies: 0.2
//   FORMAT sml
```

A namespace of the form `alias.inner` routes to the mounted store; writes only
ever hit the session store, so mounts are read-only *by construction*. This is
how a voice edge attaches local organization/category replicas and assembles a
whole prompt in one in-process statement.

---

## 6. Recall: hybrid retrieval with RRF fusion

Recall has three independent legs, fused in the engine:

1. **Structural** — indexed triple lookups (`subject`/`relation`/`object`,
   `entity_latest`, thread tail). This is the microsecond hot path and needs no
   model.
2. **Lexical (BM25)** — full-text search over grain content.
3. **Vector** — semantic similarity over embeddings.

The lexical and vector legs are combined with **Reciprocal Rank Fusion (RRF)**
in Rust, then optionally reranked. The design is deliberately degradable: with
no embedding backend installed, recall runs on structural + BM25 alone — enough
for profile and booking-style workloads, and the default for constrained
"edge" deployments where every millisecond of prefill is compute-bound.

**Embedders and rerankers are traits** (`EmbedBackend`, `RerankBackend`). Areev
ships no mandatory external service: bring a remote HTTP embedder, a local
model, or nothing at all. Because a memory file records its embedding provenance
(model + dimension) in its `meta` table, a mismatched embedder warns rather than
silently mixing vector spaces.

Bounded graph reads sit on the same indexes: 1-hop neighborhoods, relation-filtered
k-hop traversal, bounded shortest paths (for "why does the agent believe X"
provenance walks), and as-of temporal reads — all indexed reads at recall
latency with depth/frontier/deadline caps. This is *temporal graph reads without
a graph database*; unbounded traversal and graph analytics are deliberately out
of scope.

---

## 7. Context rendering: budget-aware, provider-optimal, one renderer

The last step in the recall path is turning grains into model-ready text under
a token budget. The context layer renders to **SML, TOON, Markdown, and JSON**,
with provider presets (e.g. SML for Claude-class, Markdown for GPT-class) and
grain-type diversity floors so a budget doesn't collapse to a single type.

There is exactly **one per-grain rendering implementation** —
`areev_cal::render` — shared by CAL's `FORMAT` arms and `areev-context`'s
assembler, so a grain renders to the same bytes on every surface (pinned by
`crates/areev-context/tests/render_parity.rs`); envelopes (grouping,
sections, budget modes) remain per-surface policy. The same module owns the
one `chars/4` token estimator every budget consumer shares.

Rendering uses **progressive disclosure**: as the budget fills, individual
grains degrade from full form to summary (70% threshold) to omitted (95%)
rather than the whole block being truncated at a byte boundary — in prose
formats; JSON and TOON stay whole-entry, because a prose summary inside a
structured dump would corrupt it. `ASSEMBLE`'s `BUDGET` clause drives this
directly (template renders pick their disclosure tier from tokens-per-grain,
so `ELEMENT_SUMMARY`/`ELEMENT_OMIT` fire under pressure), and
prompt-assembly logic can live in named, versioned saved CAL queries —
hot-swappable without redeploying the agent, and replicated with the file's
bundles.

---

## 8. Areev Loop: governed self-improvement

Recall (§5–§7) makes memory *useful* on the read path. **Areev Loop** is the layer
that makes memory *get better* on the write path — an agent learning from its
own history — without the failure mode that keeps most teams from shipping it:
an agent that edits its own memory and prompt is an unreviewed production deploy
that runs continuously. Areev Loop's stance is that **self-improvement is a
governance problem before it is an intelligence problem**. Every change to the
backend is a first-class object — evidence-cited, reviewable, undoable, measured.

Two properties shape everything below:

- **Deterministic core; LLM optional.** Because memories are typed grains, not
  text blobs, analyzers compute over declared semantics (`Event.is_error`, a
  Fact's subject/relation/object, supersession chains, `valid_to`) and produce
  useful recommendations with **zero model calls**. An LLM is strictly additive
  enrichment — it can never gate, approve, or apply anything.
- **Governance is native.** Every change passes four gates and lands as
  hash-chained audit grains. There is no daemon and no scheduler anywhere; a
  loop run is a cheap, idempotent command a host triggers however it already
  triggers things (a hook, cron, CI, an MCP call).

### 8.1 The loop and the four gates

```
capture  (tool calls, facts, events)   — record_tool_call / add / import
  → ANALYZE   deterministic, typed       — 12 analyzers over grain semantics
  → RECOMMEND recommendation + evidence  — dedup'd, template-rendered, cited
  → GOVERN    review / policy auto-apply — the four gates, audit grains
  → APPLY     undoable supersession      — scope-checked at execution
  → MEASURE   outcome review             — re-run the metric, revert on regression
```

1. **Propose** — only recommendation objects enter the queue, each carrying a
   versioned analyzer id + params, a deterministic template-rendered summary
   (analyzers cannot emit free prose), bounded evidence hashes, a severity, and
   a reproducible metric snapshot.
2. **Review** — separation of duties (`write` grants neither `review` nor
   `apply`), a mandatory `BECAUSE` reason on every decision, and self-approval
   blocked against the recommendation's creating actor.
3. **Apply** — requires the `apply` scope plus every scope the payload itself
   needs, evaluated at execution time (no privilege amplification); every apply
   records its inverse, or is marked non-rollbackable up front.
4. **Verify** — after a review window the stored metric re-runs and the outcome
   is recorded; regressions propose a revert (§8.4).

Auto-apply is **off by default** and, where a host policy file grants it, is
restricted to structural, engine-verified, non-destructive curation on
memory/query targets only — never prompts, never destruction, never LLM-drafted
text. The rule is one sentence: **the file selects and restricts; only the host
grants** — so a synced or hostile memory file can never arrive pre-armed. Areev Loop
inherits Areev's standing invariant that the only destructive verb is
single-grain `FORGET` (§5.2), so a staleness sweep proposes tombstones a human
must approve under `admin` + `allow_destructive_ops`. The audit trail is grains:
one immutable Observation per transition, hash-chained per recommendation,
carrying the actor label and the reason — it syncs with the file and is
queryable in CAL.

### 8.2 Deterministic analyzers and recall telemetry

Twelve built-in analyzers (ten default-on) read typed grains, never prose:
tool-failure clustering, duplicate/near-duplicate consolidation, contradiction
resolution under functional relations, fork surfacing, staleness, skill-stall,
goal-stagnation, retention sweep (opt-in — declared storage limitation routed
through the review queue), and three **telemetry-fed** analyzers — cold grains,
coverage gaps, and budget pressure — that move Areev Loop from *hygiene* (is
memory internally correct?) to *utility* (is memory used, and does it help?). Precision
is measured, not asserted: the `loop_precision` bench scores each analyzer
against a labeled fixture and fails below its floor when explicitly run. It is
not a CI workflow step; the reusable metric arithmetic and loop/golden tests
are covered by `cargo test --workspace`. Teams extend the set without recompiling
via `--analyzer-cmd`: a subprocess that reads a live-grain snapshot and returns
advisory findings, at trust class `command` (auto-apply `never`) — it surfaces,
never mutates. `areev loop reflect` re-runs every analyzer over the whole
memory (ignoring the incremental watermark) for a first pass or a periodic deep
sweep; dedup keeps it from re-proposing what is already queued.

The utility signal comes from a disposable `<file>.telemetry.db` **sidecar**
that records what recall actually surfaced — which grains were retrieved, which
questions came back empty. It is host-only (a bare library `open()` records
nothing), encrypted under the main file's key (crypto-erasure covers it), never
syncs, and is rebuildable. Capture on the recall path is buffered and
non-blocking, so it never touches the microsecond recall / 50 ms voice budgets
(voice-loop recall p50 stays ~82µs with telemetry on).

### 8.3 The optional, verified LLM path

Attach a model (`areev loop run --model claude-sonnet`, or `--llm-cmd` for a
subprocess backend) and the pipeline gains strictly additive stages that are the
identity when no backend is set:

```
ANALYZE → DISCOVER → GROUND → VERIFY → ENRICH → VALIDATE+DEDUP → STORE
```

The design follows the one result the self-improvement literature agrees on:
**improvement is reliable when an external verifier grades the change, and
degrades when a model judges its own correctness.** Deterministic analyzers do
the error-*finding* LLMs provably can't; the LLM only proposes fixes localized
to a finding, under an **abstention-legitimate objective** ("nothing to report"
is a zero-penalty answer, so it isn't pushed to over-generate). Every draft is
then **grounded** (are the finding's factual *premises* present in the cited
evidence? — a fabrication guard that still allows a genuine inference) and
**independently verified** (a separate call — the proposer never grades itself —
judging soundness and abstention, not novelty) before it can reach the queue,
stamped with a calibrated confidence and `origin = llm` so it can **never
auto-apply**. Grounding can run on a separate backend (`--ground-model` /
`--ground-cmd`) to take the generative model out of the entailment check. A bad
or slow backend drops its contribution, never the run. Quality is measured
(§8.4), not asserted.

Providers ship out of the box in the `areev-llm` crate (OpenAI-compatible,
Anthropic, Ollama over a small blocking HTTP client, keys read from the
environment), isolated there so the core crates stay dependency-light.

### 8.4 Measurement: the Verify gate

The honest test of self-improvement is not "did it make a change" but "did the
change help." When an applied recommendation carries a metric, the engine
re-measures it on a **schedule of checkpoints** (1d / 7d / 30d) — a typed read
over subsequent history, no LLM — and records each outcome as a file-truth
(`held` / `regressed`); a late regression proposes a revert. The LLM path is
measured too: the `loop_reflection` bench scores **Effective Reliability** (it
subtracts for confident-wrong, unlike raw precision), and `areev loop` reports
the live approval-rate of LLM drafts.

The boundary is deliberate. This works for **internal, bounded, attributable**
outcomes — did this tool fail again, does this duplicate still exist — the facts
Areev Loop owns. It does **not** claim open-ended, world-facing outcomes (was a
generated post good, is a patient happier); those surface as a monitored trend a
human judges, never a machine verdict. Areev Loop improves the agent's *memory*, not
its *outputs*.

### 8.5 Where it lives

Areev Loop is three crates (§9). The **`areev-loop`** engine is substrate-agnostic — it
depends on no Areev crate and runs against any OMS-shaped store through the
`OmsSubstrate` trait — so the governance model is not Areev-specific.
**`areev-loop-adapter`** implements that trait over `AreevFacade`; **`areev-llm`**
implements the `LlmBackend` trait with out-of-box providers. The whole user
surface — the `areev loop` verb family and `areev init`, two MCP tools, the
`/api/loop/*` routes, the Areev Loop console tab, and the Python/Node bindings —
reduces to that engine. The OMS `0x0C` **Recommendation** type is now realized
in `areev-core` (§2.3), but Areev Loop still writes its recommendation and audit
grains as content-addressed Fact grains in an `areev-loop` namespace: migrating the
existing queue to the native type is a data decision, not a format one, and is
deliberately separate from landing the type. Full design:
[`loop.md`](docs/loop.md) and
[`loop-reflection.md`](docs/loop-reflection.md).

---

## 9. Crate layout

Areev is a Rust workspace of 12 crates (plus `areev-js`, a standalone napi
package built outside the workspace). Two foundations converge on the leaf
crates — the memory stack, and the Areev Loop self-improvement engine:

```
  the memory stack
    areev-core ─▶ areev-store ─▶ areev-cal ─▶ areev-context

  the self-improvement engine
    areev-loop ─▶ areev-loop-adapter   (areev_loop::OmsSubstrate over AreevFacade)
         └──▶ areev-llm      (areev_loop::LlmBackend — OpenAI / Anthropic / Ollama)

  both feed the leaf crates
    areev-mcp · areev-server · areev-py · areev (binary) · areev-bench (harness)
```

| Crate | Depends on | What it does |
|---|---|---|
| **areev-core** | — | The `.mg` format, canonical serialization, content addressing, the 12 grain types, and tool-schema rendering. Storage-agnostic; everything depends on it. |
| **areev-store** | core | The Turso store: dictionary-encoded triple indexes, `entity_latest` heads/forks, hybrid recall + RRF, bounded graph ops, the op-log + HLC + tombstones, the CAS blob sidecar, bundles/streaming, and the memory-tool adapter. |
| **areev-cal** | core, store | CAL lexer, parser, AST, executor, multi-source ASSEMBLE, templates, saved queries, and the `AreevFacade` (with read-only mounts) that binds CAL to the store. |
| **areev-context** | cal, core | Budget-aware rendering (SML/TOON/Markdown/JSON), progressive disclosure, provider presets, and tool-schema formats. |
| **areev-loop** | — (substrate-agnostic) | The Areev Loop self-improvement engine: the `OmsSubstrate` / `LlmBackend` / `Analyzer` traits, the 11 deterministic analyzers, the recommendation lifecycle + four gates, the LLM DISCOVER → GROUND → VERIFY verifier, and outcome measurement. Depends on no Areev crate — it runs against any OMS-shaped store. |
| **areev-loop-adapter** | areev-loop, cal, store, core | The Areev substrate adapter: implements `areev_loop::OmsSubstrate` over `AreevFacade` so `areev loop` runs against real `.mg`/Turso files, plus the recall-telemetry sidecar. |
| **areev-llm** | areev-loop | Out-of-box LLM provider backends (OpenAI-compatible, Anthropic, Ollama) implementing `areev_loop::LlmBackend` over a small blocking HTTP client — isolates the HTTP surface so the core crates stay dependency-light. |
| **areev-mcp** | cal, core, store, areev-loop, areev-loop-adapter | The stdio MCP server — eight memory- and improvement-semantic tools (six memory + `areev_loop` / `areev_recommendations`) over newline-delimited JSON-RPC 2.0. See the [MCP reference](docs/mcp-reference.md). |
| **areev-server** | cal, context, core, store, areev-loop, areev-loop-adapter | A dependency-light HTTP/1.1 web console (loopback, read-only without a token) plus the `/api/loop/*` routes and Areev Loop console tab, and an optional sync-hub mode with bearer-token auth. |
| **areev** | all of the above | The `areev` binary: ~27 verbs (`add`, `recall`, `cal`, `history`, `log`, `bundle`, `import`, `migrate`, `reindex`, `verify`, `serve --mcp`, `ui`, `repl`, `remember`, `init`, `loop`, …). |
| **areev-py** | cal, context, core, store, areev-loop, areev-loop-adapter | Python bindings (`import areev`); scalars in, JSON strings out. |
| **areev-bench** | most of the stack | Reproducible accuracy and latency benchmark harnesses (latency, honesty, LoCoMo accuracy, `loop_precision`, `loop_reflection`). |

---

## 10. Key design decisions and trade-offs

These are the decisions that most shape the system, and what they buy.

### Dependency-light by policy

Areev avoids heavy dependencies on principle: no CLI-args framework (arguments
are hand-parsed), no HTTP framework (the server is std `TcpListener`), no MCP
SDK (JSON-RPC is hand-rolled), and no workspace-wide async runtime (the store
wraps a private current-thread runtime behind a synchronous API). Point reads in
the microsecond class cannot afford executor hops, and a small dependency
surface is a smaller attack surface and a smaller thing to keep building for
years. Think twice before adding a dependency.

**Recorded exception — rustls (non-default `tls` feature).** Native TLS for
`areev ui`/`areev hub` exists for deployments with nowhere to put the
documented TLS-terminating proxy (edge boxes, appliances). Hand-rolling TLS
is the one thing nobody should ever do, so this is a deliberate exception to
the policy above, scoped three ways: rustls only (the boring, auditable
industry default), behind a **non-default cargo feature** (the published
edge binary stays dependency-light), and with **no ACME or certificate
lifecycle management** (PEM paths in, rotation is the operator's or proxy's
job). The proxy pattern remains the documented default forever. Same
decision shape as `docs/erasure.md`'s role for the erasure requirements.

### Single writer per file

Each memory file has exactly one writer queue. There are no cross-file
transactions, so scaling out is *adding files/shards*, and the audio thread on a
voice edge never blocks on a lock. Multi-writer conflict is handled honestly by
the [heads/forks model](#4-versioning-heads-forks-supersession-tombstones)
rather than by hidden last-writer-wins.


**Adding a grain that is already stored is a no-op.** A content address *is*
the content, so two byte-identical grains are one grain; re-adding returns the
existing address rather than failing on the `grains.hash` unique index. A
skipped duplicate consumes no sequence number and writes no op-log row —
nothing changed, so nothing replicates.

**…but an occurrence is not a value, and carries its own identity.** That
dedup rule is right for the things a memory *knows* — a fact restated is the
same fact — and wrong for the things it *witnesses*. `created_at` has
millisecond resolution and is part of the content address, so two identical
tool calls inside one millisecond would otherwise collapse into a single
grain. A tool that failed five times is a different state of the world from
one that failed once, and that count is the entire input to the
`loop.tool_failure` analyzer: an agent retrying a failing call with identical
arguments is precisely the workload it exists to catch, so collapsing those
retries deletes the signal exactly where it matters.

The recording API therefore gives each call an identity.
`record_tool_call(…, call_id=…)` takes the provider's own `tool_call_id` —
stored on the grain, queryable, and the link from a recommendation's evidence
back to the transcript that produced it. Absent one, a synthetic `auto:` id is
stamped so occurrences never merge. Recording is append-only: replaying a tool
log twice records it twice, and the host owns that. The raw `add("tool", …)`
path is unchanged and keeps ordinary value semantics — the distinction is
between the two APIs, not two kinds of grain.

**One handle per file, shared across threads.** The rule is enforced in both
directions: across processes by an OS file lock, and within a process by a
registry of open paths — a second `open()` on a file this process already holds
fails at open with `STO-E002`. It has to be enforced, not merely documented,
because a handle caches its own `next_seq`/`next_term` allocators and BM25
statistics; two handles drift apart silently and then collide on a write, which
surfaces as a bare `UNIQUE constraint failed` attributed to whichever handle
wrote next rather than to the one that should not exist. Sharing one handle
across threads is fully supported, so opening per request or per agent turn —
the natural move if you think of the file as a database connection — is never
necessary. Rust and Python release the claim on drop; Node has no
deterministic drop, so its binding exposes `close()` — otherwise a handle that
had gone out of JS scope would keep the file locked until GC got to it.

### Host config is never persisted in the file

A memory file declares *what it physically is* — its text-index and
entity-relation settings and its embedding provenance live in a `meta` table, so
the same file behaves identically on any machine and needs no external registry
to travel. Everything else — which embedder the host can run, executor limits,
mounts, write quotas — is *host capability and policy*, supplied per process
(CLI flags, env, MCP args) and never written into the file or read from global
config by the library. Embedded behavior must be machine-independent.
Reconciliation between a file's declarations and a host's config is *loud, not
fatal*: a bare `open()` honors the file; an explicit `open_with()` re-stamps and
reports every change through open warnings.

### CAL's destructive surface is narrow and gated

The [gated destructive surface](#52-the-narrow-gated-destructive-surface) is a
first-class feature, not a footnote. In a landscape where agents have wiped
production databases, an agent-facing query language whose *only* destructive
verb is a single-grain `FORGET` — with no bulk-erasure primitive to reach for,
and a one-flag switch to make a session fully read-only for untrusted input — is
a safety property you can rely on.

### Namespace prefix scopes widen reads only, and fail closed

Namespaces are opaque strings, but hosts name them hierarchically in practice
(`org.sales.emea`, `agent:authz`). A **prefix scope** — a namespace value
ending in `*`, e.g. `"org.*"` — makes that hierarchy queryable: it selects
the base namespace plus every descendant through the separator the caller
wrote (parent + descendants; `organization` never matches `org.*`, and the
forms `org*` / bare `*` refuse rather than guessing). One convention on every
read surface: CAL `WHERE namespace`, `namespace IN (…)` sets (whose members
may themselves be patterns), the MCP `namespace` argument, `--ns`, and
ASSEMBLE sources.

The mechanics: a count-maintained **namespace registry** (`ns_reg`, one row
per grain-bearing namespace, self-healed from `grains` on open via a meta
stamp) makes expansion O(distinct namespaces), and the recall legs
generalize to a namespace-id set — per-namespace probes merged on the
file-global seq for the structural/recent legs, set-scoped postings and
vector scans for the other two, one RRF fusion. The single-exact-namespace
case — the voice hot path — keeps its cached statements untouched.

Three deliberate boundaries: scopes are **read-only** (`*` is reserved, so
writes refuse wildcard namespaces; destruction, grants, and policy take
exact names — a wildcard must never widen a destructive surface); the
expansion **fails closed** under a bound principal (every covered namespace
must be granted, and the refusal names the pattern, never a discovered
namespace); and one `RECALL`'s scope set cannot span mounts (that is
ASSEMBLE's job).

### Self-improvement is governed, not autonomous

Most agent-memory products treat self-improvement as an intelligence problem —
let an LLM rewrite memory and hope. [Areev Loop](#8-loop-governed-self-improvement)
treats it as a governance problem first: a deterministic core that needs no
model, an LLM that can only *propose* under an independent verifier, four gates
on every change, an undo for every apply, and a re-measured outcome for every
metric. Just as deliberately, there is **no daemon and no scheduler** — an areev-loop
run is a command a host triggers, so improvement never runs unattended. That is
the difference between "an LLM edits your memory" and self-improvement you can
put in production.

### Areev supplies the corpus and grades the result; it never trains

The pressure to turn a memory engine into a trainer is obvious — we hold the
trajectories, so why not tune on them? The boundary is deliberate, and it is the
same one CAL applies to every host verb: if a thing needs a filesystem path, a
credential, or a process to exist, it belongs to the host. A training job needs a
GPU, a credential, a trainer process and hours of wall clock, so Areev ships no
trainer and takes no training dependency — the posture `--embed-cmd` and
`--llm-cmd` already established.

What Areev does own is the half nobody else documents. `areev corpus` emits the
training set through an **authorized CAL selector**, with step-level quality
labels and loss weights — masking the harmful steps of a failed run instead of
discarding the run — and writes an immutable export manifest naming every source
hash, the model/policy bindings, the subject fingerprints touched, and the
recipient. `areev run shadow` replays recorded runs with zero effect dispatches
and `areev eval` pins an evalset as a gating edge, which together are the only
honest way to say a candidate is an improvement. And because an export is a
grain, a later `FORGET SUBJECT` reports which corpora — and therefore which
downstream checkpoints — are now stale and must be retired or re-derived.

The claim ladder is bounded on purpose: we can prove what went into a corpus,
exclude a subject from it, and re-derive. We do **not** claim a subject has been
removed from anyone's weights. The seam itself completes the loop: `areev tune
--cmd` hands the corpus to the host's trainer and registers the returned
adapter as an `mg:adapter` Fact — base model + adapter + quantization pinned as
one tuple, `derived_from` naming the corpus manifest — and promotion is a gated
apply through the loop's existing lifecycle (the `adapter_intake` analyzer
proposes, `areev eval run --model` records the gating edge against any
OpenAI-compatible serving endpoint, `APPROVE`/`APPLY --gating-run` admits,
rollback retracts the `mg:adapter_promotion` Fact hosts serve from). One
candidate per served model; auto-apply is impossible by class. Design of
record: [`docs/areev-adaptive-agents-proposal.md`](docs/areev-adaptive-agents-proposal.md)
§5 — and still no capability claim before the harness has measured one.

### The anonymization boundary is the model, not the tool

An `egress` anonymization policy covers what leaves for a model provider, and
that is a narrower claim than "what leaves the process". A host tool posting an
invoice must receive real values — a pseudonymized supplier name writes a
corrupt record — so pseudonymizing the tool seam would break the workflows the
feature exists to serve.

So the run driver pseudonymizes an abstract node's prompt and rehydrates the
model's tool-call arguments before dispatch. Rehydration is for dispatch only:
the journal keeps the pseudonymized form and the idempotency key derives from
it, so `verify` replays byte-identically whether or not a policy is live. An
unresolvable placeholder fails the node rather than sending itself to a vendor.

This required the policy to be `scope: memory` (value-derived tokens, hence an
encrypted memory) and refuses anything else at start with `RUN-E023`, because
appearance-order tokens pseudonymize differently on replay and would surface as
a journal integrity failure rather than the configuration error it is.

The gap it closes is worth recording as its own lesson: the gate was an egress
boundary on *store reads*, and the run path never read. A control whose
coverage does not match the belief it creates is worse than no control, and the
belief `egress` created was that model-facing data was pseudonymized.

### A declaration replicates; an authorization never does

A `Tool` Definition may name its executor by content address
(`executor_uri: "cas://sha256:..."`), so connectors can travel with a memory
the way every other grain does — content-addressed, verifiable, and reachable
by `tool provenance` and the loop's Rule E1 `code_revision` gating. Bundles
carry blobs, so this is real distribution, not a convention.

The decision is what does *not* travel. Executing a blob requires the host to
have pinned its address (`--allow-executor`), which is process configuration
and never a grain. There is deliberately no CAL grant: `mg:permits` Facts live
in the file and replicate, and a permission arriving in the same bundle as the
code it authorizes is not a permission. Unpinned is refused at run start
(`RUN-E018`), before a lease exists, naming the address.

This generalizes two earlier decisions that were made independently — trigger
evaluation state stays in non-replicating `trg:` rows, and host config
(embedder, executor limits, the destructive-ops cap) is per-process. All three
are the same rule: **what a memory says replicates; what a host is permitted to
do does not.** The alternative on offer was a sandbox, which does not constrain
the failure that actually occurs — a connector legitimately holding a
credential and misusing it — so provenance, not isolation, is where the control
was put.

### A capability is declared in the memory and granted by the host; the guest never gets a socket

Tier C (`areev-sandbox`) was correct for pure compute and, for two releases,
that made it half a promise. It is the **only** tier producing a persistable,
content-addressed tool, and it forbade all I/O — so the tools every real agent
needs could not be grains. The two options for an I/O tool were a native code
grain (persisted, but *not sandboxed — it runs as you*, and platform-specific)
and a host `--tool-cmd` script (sandboxed by nothing, and outside the memory
entirely). "Tier C can never touch the network" was a real design statement,
and 1.6.0 revisits it deliberately and narrowly.

The tier now has two runtimes, because there are two determinism stories:
`wasm32-areev` is pure and re-execution-provable; `wasm32-areev-io` adds
exactly one import, `areev::fetch`, and is deterministic *modulo journaled
effects*. Naming them separately is the point — a flag on the first would have
made a provable property silently conditional.

**The isolation claim is strengthened, not weakened.** The guest still has no
socket, no credential, no clock, no environment. It has an unforgeable
capability to *ask the host*, and the host enforces policy and performs the
call. Three trust levels: untrusted guest → trusted sandbox binary, which holds
a revocable broker token → engine broker, which holds credentials. This is the
model proxy-wasm and Cloudflare Workers use ("the platform performs the fetch,
the guest never gets a socket") and the one Spin's runtime-config and Extism
use for secrets ("the guest holds a label, the host resolves it"). It needed no
new IPC: the engine already injected the broker's address and token into the
sandbox process for uniformity, inert only because the *guest* could not reach
them.

The authorization split is the decision above, applied one level down. The
Tool grain's `capabilities` field **declares** — hosts, methods, path prefixes,
credential names — and replicates with the bundle. The host **grants**, with
`--allow-host` / `--credential` / `--tool-egress`, and that never replicates.
The effective set is their intersection, evaluated on every call, so a
declaration can only ever narrow. A grain that authorized its own egress would
be, once again, a permission arriving in the same bundle as the code it
authorizes.

Two things follow that are worth stating because they are not symmetric. The
declaration may pin **path prefixes**, which the host-side allowlist
deliberately cannot: `--allow-host` allowlists hosts because path-level
authorization there would imply a model it does not have, whereas a capability
tool's code is pinned by content address, so it can afford to be narrower —
and the exfiltration case worth closing is a malicious tool POSTing stolen
context to an *allowed* host's upload endpoint. And the declaration is
**frozen into the run manifest** beside the pinned runtime, so a supersession
mid-run cannot widen reach, and resume and verify read the set the run started
with.

The audit trail grew a second half to match. The broker already journaled
refusals; it now journals successful calls too, as `egress_call` Observations
carrying method, final URL, status and body **digests** — never bodies, since
a grain is immutable and replicates, and never a credential value, since the
broker only ever received a label. Neither kind is a journal entry, so `verify`
stays byte-identical whether or not a broker was configured: they are evidence
*about* a run, not steps *of* one.

Two prerequisites had to be closed first, and both were live bugs rather than
theoretical ones — a capability system resting on an allowlist a redirect can
walk through, or a broker whose credential leaks into the child environment,
grants strictly more than it declares. The HTTP client had been following
redirects with the allowlist checked only on the initial URL, and the
`--credential` variable had never been added to the withhold list every other
secret flag is on.

### Cadence is data; evaluation is a command

A trigger is a standing rule that starts a workflow, declared as a `Trigger`
grain (0x0D) and evaluated by `areev trigger run`. This **extends** the
no-daemon decision above rather than retracting it: there is still no resident
process, no timer thread, no `tokio::time`. What changes is that the *cadence*
lives in the memory instead of in someone's crontab, so a synced memory can say
what it was supposed to be doing, and changing an interval is a grain write
rather than a redeploy.

The OS scheduler stays a dumb heartbeat and the memory decides what is due —
the pattern `areev loop run --if-stale` already established, and the one anacron
and systemd's `Persistent=true` take.

Three decisions inside it are worth stating because each has a losing
alternative that looks reasonable:

- **Trigger → plan, never the reverse.** A Workflow is content-addressed and a
  run's manifest pins its hash, so a plan carrying a list of triggers would
  change address every time one was added and orphan its own run history.
- **Correctness rests on occurrence identity, not on the lease.** The run id is
  derived from `(trigger, connector, dedup value)` and the runtime already
  refuses a duplicate run id, so two evaluators racing produce one run and one
  recorded skip — no lease duration to guess, no fencing token, no clock-skew
  window. The lease only prevents duplicate *connector calls*, so losing it
  costs an API call rather than correctness.
- **The declaration replicates; the evaluation state does not.** Cursor, lease
  and watermark are per-host usage. Replicating them would have two synced
  hosts ping-pong on each other's watermark, and would let a dev memory restored
  from prod inherit prod's cursor and silently skip real work.

`Workflow.trigger` was removed in the same change. It was a free-text field that
nothing read — not the scheduler, not the driver — so it described an activation
condition that could not activate anything, and the console offered to set it.

### Anonymization is a store-boundary transform, gated by file policy

Egress pseudonymization runs at the **store read boundary** — below even the
facade — because the CLI and both bindings call store-level reads directly;
any higher hook leaves shipped surfaces raw. One `anon:<ns>` policy row per
namespace is the gate: a file-truth that replicates write-if-absent and
fails reads **closed** when unreadable. Detectors are host capabilities
(embedder-style seams: built-in Tier-0, `--anonymize-cmd` NER,
`--anonymize-llm-cmd` grounded LLM), and a policy demanding an uninstalled
detector fails closed too. The placeholder→value mapping never leaves the
process — payloads carry mapping ids only; reverse lookup is admin-gated
and fingerprint-audited. Value-derived tokens (ingress, `memory` scope, the
sealed vault) key from the page cipher and refuse without one. Three reads
are exempt with the reason stated in code: `subject_report` (the DSAR must
disclose what is stored), `run_grains` (the runtime's machine replay), and
the authz engine's grant recall (a pseudonymized principal would fail every
check). The vocabulary is *pseudonymize* — no anonymity claim, anywhere.
The free-text APIs (`scan_text`/`anonymize_text`) sit one layer up, on the
facade, and stay pure-text-in/JSON-out — no store *writes* — but now read
the store's known-identity table for the facade's default namespace, the
same propagation table the grain-egress boundary builds: a subject already
interned by an intake step is detectable in prose passed to these APIs too,
not only in grain reads (issue #32). `AnonPolicy.known` is the
caller-supplied complement — identities a host holds but never interned as
a grain subject (a CRM row, an email header), each with its own category.

### A cycle's back-edge gates a node's own first activation, not the entry's

A bounded cycle's re-entry point can be **any** node, not only the plan's
entry — `PlanGraph::build` classifies every edge as a DFS back-edge (via
the entry-rooted Tarjan traversal already computing `scc_of`) or not, and
`refresh_readiness` excludes an in-edge from a node's *generation-0*
AND-join exactly when it's a back-edge: such an edge closes a cycle through
that very node, so it cannot possibly have resolved before the node's own
first run. Node 0 needed no such rule — it bootstraps unconditionally
(`apply_event`'s `Start` handler) — but nothing generalized the idea to a
non-entry cycle head, so a plan like `a -> g -> c -> g` (back-edge to `g`,
not `a`) validated cleanly and then deadlocked at superstep 1 (issue #33).
Removing exactly the DFS back-edges of an entry-rooted traversal always
leaves an acyclic graph — a standard theorem — which is what makes the
generalization safe for arbitrary cycle shapes, not just the single-node
self-loop or entry-targeted back-edge the original scheduler handled.

### Pinned literal sections in `ASSEMBLE` (`LITERAL` + `PIN`)

**Decision (2026-08-19, issue #42):** `ASSEMBLE` sources may be host-supplied
literal text (`label: LITERAL "…"`) and may be marked non-degradable
(`label: PIN …`). This is **new CAL syntax ahead of the OMS spec**, recorded
here deliberately — the same posture `DEFINE QUERY` already has.

Why it had to be syntax rather than a host wrapper: a production system prompt
is not "some grains", it is grains **interleaved with fixed text at fixed
positions**, and some of that text is legally or contractually mandatory. Two
things were expressible nowhere. First, a literal: to include a fixed
instruction the host had to write it into memory as a grain, which turns a
compliance-critical string into a mutable row rather than code. Second,
non-degradability: `PRIORITY` only *weights* the budget split, and the
progressive-disclosure allocator walks every source Full→Summary→Omit — so a
long conversation could silently summarise away the one section that had to
survive verbatim. Neither is expressible by weighting, because both are about
a source's **kind**, not its share.

Mechanism: pinned sources are costed in full and reserved off the top; the
remaining budget is shared by `PRIORITY` as before; the trim loop never
touches a pin. If the pins alone exceed `BUDGET` the statement fails with
`CAL-E122` rather than degrading them — for a guarantee of verbatim
disclosure there is no honest partial answer, and a quiet one is the unsafe
outcome. Render order is FROM-clause order and is now a documented contract
with a test, explicitly independent of `PRIORITY`.

Related, same change: **out-of-order `ASSEMBLE` clauses are a parse error.**
They used to detach silently, so `… FORMAT markdown BUDGET 900` ran at the
4000-token default — the guard you wrote was not the guard that ran.

### One anonymization root, separate from encryption at rest

**Decision (2026-08-19, issue #46):** the HKDF root for the anonymization
subkeys (session / memory / vault) is `AreevOptions::anon_key` when the host
supplies one, else the page-cipher key.

Reversible pseudonymisation — replace identities before an LLM egress,
rehydrate the reply — was keyed *only* from the page cipher, which the Postgres
backend refuses outright because it is a page-cipher capability. So the backend
built for stateless hosts could not use the egress control built for untrusted
egress, and deterministic value-derived tokens were unavailable on plaintext
files too. Making the root a first-class, host-supplied capability fixes both
at once and lets a memory be encrypted at rest under one key and pseudonymised
under another — which is what separating those two roles is for. The key is
never persisted; rotating it is a crypto-erasure of the mapping table. Trust
model in [`docs/security-model.md`](docs/security-model.md).

The root reaches every host surface, not just the Rust API: `--anon-key-env VAR`
on the CLI and `anon_key=`/`anonKey` on the Python and Node constructors. That
matters more than it sounds — a capability whose whole purpose is serving
stateless Postgres deployments is worthless if it is settable only from the one
surface those deployments do not use.

### Credentials are minted per request, not read once

**Decision (2026-08-19, issue #45):** LLM backends hold a
`areev_llm::cred::Credential`, not a `String`.

A static key read once from an environment variable excludes every
cloud-native auth model — Vertex AI under Application Default Credentials,
Azure managed identity, AWS SigV4 — which exist precisely so that no key sits
on disk. Those are not a larger version of "read a key from the environment";
many organizations forbid creating such keys at all. The seam is deliberately
narrow (mint the auth value, refresh it) so the endpoint and header stay with
the backend. The Vertex adapter reuses the OpenAI-compatible client against the
**regional** `aiplatform` host — the region is never defaulted and `global` is
refused, because under a residency obligation the region is the entire point.
Providers are individually feature-gated, and a third-party model router
(OpenRouter) is off by default: it is an extra jurisdiction in the transfer
path, and a regulated build should be able to state that its artifact cannot
reach one.

### LIMIT bounds the answer, not the search for it

**Decision (2026-08-19, issue #43):** any post-retrieval pipeline stage that
ranks, filters or counts widens its scan to `max_limit` and re-applies the
caller's bound afterwards, warning (`CAL-W015`) when even the widened scan
fills.

`ORDER BY` sorts what the statement already returned, which is a
`default_limit` page — so `ORDER BY priority DESC | LIMIT 5` returned the top
5 *of the newest 50* and was indistinguishable from the top 5 overall.
`CONTRADICTIONS` had already solved this for itself, with a comment explaining
why; this generalizes that fix to every stage with the same shape rather than
leaving it a one-off.

True sort pushdown is possible for exactly one field. `created_at` is a column
on `grains`, so it is pushed into the scan and is exact at any corpus size.
Every other sort key callers actually use — `priority`, `status`,
`confidence`, every type-specific field — lives **inside the immutable,
content-addressed blob**, where SQL cannot reach it without materializing a
column per field. That is a consequence of content addressing, not an
oversight, and the honest response is to widen and then say so rather than to
imply an exactness the storage model cannot provide.

### WHERE fails closed: pushed down, evaluated per grain, or refused

**Decision (2026-08-22, issue #91):** every `WHERE` condition on the recall
family (`RECALL`, `EXISTS`, `HISTORY … WHERE`, ASSEMBLE's post-filter) is
either consumed by the engine push-down, evaluated per grain by the one
authoritative boolean evaluator (`grain_matches_condition_tree`), or refused
before the scan — it is never dropped.

The failure this retires was the worst shape a memory engine can have: a
*narrowing* clause that silently *widened*. A common field outside a type's
queryable set passed validation, fell out of push-down, and returned
everything with only a stderr warning — so `WHERE status = "failed"`
returned the successes, in the right shape and order, indistinguishable from
a correct answer without knowing the field tables by heart. `NOT x = v` was
flattened to `x = v` (precisely the excluded set), and `a OR b` pushed only
`a`.

The mechanism is a planning pass (`plan_residual_where`) that splits the
condition tree: leaves the push-down consumes (test-pinned truth table)
become engine parameters; everything else — type-specific fields, `NOT`/`OR`
subtrees, unsupported comparators, `IS NULL` — survives as a *residual tree*
evaluated per grain after the (widened) scan. Validation runs before the
scan: a field the target type cannot carry is `CAL-E060`; an engine-level
field (`query`, `time`, `entity`, `contradicted`, `scope`, `tags`) in a
position it cannot be honoured (under `NOT`/`OR`, wrong comparator) is
`CAL-E061`, because those narrow the scan and have no per-grain value.
Leniency was deliberately NOT kept behind an opt-in: a filter that cannot be
honoured has no honest lenient reading, and the safe direction must be the
default direction.

### Portability and provenance over lock-in

Grains are content-addressed, immutable, and hash-linked; the format reserves
a signing flag (COSE envelope — designed, not yet implemented).
Memory exports to `.mg` and imports into any OMS implementation. `areev bundle
--since <hash>` produces incremental, resumable, tamper-evident backups to any
dumb remote (directory, rsync, S3) — end-to-end encrypted when grains and blobs
are encrypted, so the remote never reads the memory. This is *git for agent
memory*: log, diff, time-travel, forks with explicit merges, and encrypted
sync, built into the data model because grains already are content-addressed
immutable objects.

---

## 11. Deployment topology

Areev has no platform dependency. Three tiers cover a multi-channel fleet:

1. **Embedded** — voice and interactive edges run Areev in-process for
   microsecond recall, with per-caller working files and the op-log streaming
   out.
2. **Server tier (`feature = "postgres"`)** — the same store logic over one
   PostgreSQL schema per memory, for stateless deployments (autoscaled
   containers with no durable disk) and for inheriting an existing Postgres
   HA/PITR/backup story. The backend plugs in at an internal `Db` transport
   seam inside the store, so fork/head/oplog semantics are identical by
   construction and pinned by a two-backend conformance suite. Point reads
   are millisecond-class over a network — the voice frame path stays
   embedded by design. Unlike the single-writer file model, this tier admits
   MULTIPLE CONCURRENT WRITERS per memory: write transactions claim id
   blocks from an in-schema counters row (briefly serializing them, so
   op-log order equals commit order and the fork/head semantics match the
   single-writer model exactly), the dictionary is DB-authoritative on
   cache miss, and reads never block. Erasure and export map to
   `DROP SCHEMA … CASCADE` and `pg_dump -n`. The op-log/bundle wire format
   is backend-independent, so edge files sync into a Postgres-backed memory
   with the same `MGB1` bundles.
3. **Hub (`areevd`)** — an optional self-hosted daemon that owns a directory of
   memory files (one writer queue each), serves HTTP/MCP recall/add for
   latency-tolerant channels, serves subscriptions, and handles bundle
   push/pull. It shards by hashing the memory key; with no cross-file
   transactions, scaling is adding shards.
4. **Object storage** — the segment archive and restore source.

Organization/category knowledge fans out read-only to every edge via pull
subscriptions, which is what keeps a session's `ASSEMBLE` local: a session opens
the user file and attaches local org replicas as read-only mounts. See the
[security model](docs/security-model.md) for the trust boundaries of the console
and hub, and [SECURITY.md](SECURITY.md) to report a vulnerability.
</content>
</invoke>
