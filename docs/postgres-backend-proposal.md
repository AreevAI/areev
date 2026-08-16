# Postgres Storage Backend — Proposal

**Status: proposal (2026-08-07). Decision pending.**

Driver: enterprise deployments — concretely Atmatic (healthcare front-desk,
GCP Cloud Run + Cloud SQL Postgres 16) — need Areev's memory model without a
local file. The question asked: *does it make sense to replace Turso with
Postgres?* This document answers it for both tiers (edge and hub/server),
quantifies the latency impact, and lays out options.

---

## 1. The direct answer

**Replace: no. Add: yes.**

- **Edge tier (voice loops, Pi/NUC appliances, laptops, in-process agents):
  keep Turso.** The embedded file is not an implementation detail here — it
  is the product. Microsecond recall, "no server in the recall path",
  one-memory-one-file erasure/portability, crypto-erasure, and the entire
  §6 edge benchmark story all depend on it. Any networked backend breaks the
  voice-loop gate by construction (see §3).
- **Server/hub tier (stateless containers, multi-instance apps, enterprise
  HA): add a Postgres backend** as a second implementation behind the
  existing `CalStoreFacade` seam, feature-flagged, never the default. This
  is not primarily an HA play — it is an *existence* play: the flagship
  enterprise prospect literally has nowhere to put a file (§5).

The honest enterprise pitch this enables: *the same memory model and CAL
surface from a $35 Pi to a regionally-HA Cloud SQL instance* — edge memories
sync into the Postgres tier via the existing bundle/oplog format.

---

## 2. What the exploration found (evidence summary)

Four parallel investigations (store coupling, hub/HA, latency evidence,
Atmatic deployment target). Full details live in the subsections below;
headline facts:

**Coupling (areev-store):**
- SQL is confined to `lib.rs` + `telemetry.rs`, but there is **no storage
  trait**: ~180 inlined SQL statements across ~65 public methods, 62
  hand-rolled `rt.block_on` sites, one hardcoded `Builder::new_local(path)`
  (`lib.rs:1183`), 7 backend-typed cached `turso::Statement` slots.
- The one real seam is `CalStoreFacade` (31 methods, object-safe, DTO-only)
  — and there is prior art: `areev-cal/src/lib.rs:5` records that a
  Postgres facade + recursive-SQL graph module existed in the ancestor
  codebase and was "intentionally not ported".
- The seam leaks: `AreevFacade::with_store(|m: &mut Areev| …)` has **87
  production call sites** outside CAL (py 26, js 23, areev-loop 17, server 10,
  mcp 8, cli 3), and the CLI additionally makes ~31 direct `Areev` calls
  that bypass the facade entirely. Every one binds a surface to the concrete
  Turso-backed type. (Corrected from an earlier ~107/67-method estimate —
  see `postgres-backend-plan.md` §0.)
- **Single-writer is baked into RAM, not SQL**: `next_seq`/`next_op`/
  `next_term`/`hlc_last`/BM25 collection stats are process-local counters
  seeded from `MAX()+1` at open; the whole `terms` dictionary is an
  in-memory HashMap. Two writers on one shared DB corrupt on the first
  concurrent write. There is also **no Areev-level file lock** — a second
  process opening the same file is undefined behavior today, not an error.
- SQL dialect issues are real but mechanical: `vector32()`/
  `vector_distance_cos` (→ pgvector), 14× `INSERT OR REPLACE` (→
  `ON CONFLICT`, and `triples`/`osp` have no unique constraint to name),
  `?N` → `$N`, rowid PKs, `PRAGMA integrity_check`, loose type affinity,
  ASCII-case-insensitive `LIKE` on the `meta` prefix scans.
- No Postgres analogue for: Turso's page-level AES-256-GCM (crypto-erasure),
  the `.blobs` / `.kdf` / `.telemetry.db` filesystem sidecars.

**Hub/HA (areev-server):**
- The hub is a **singleton by construction**: one process = one memory file,
  single-threaded accept loop (one request at a time, `Connection: close`),
  `Mutex<Areev>` inside. No leader election, no fencing, no health probe
  that isn't also an open data endpoint.
- **Push is real replication; pull is not.** `POST /api/segment` applies
  MGB1 bundles; `GET /api/segment` re-serves archived files by name. There
  is no `bundle_since` export endpoint — a hub-native write can never reach
  an edge. The hub is a write sink with no outbound change feed.
- Durability gaps: no fsync policy anywhere (Turso defaults), segment push
  is bare `fs::write` (no fsync, no tmp+rename, silent 1 MiB truncation
  with a 200 response), `verify()` cannot detect WAL-rollback loss,
  non-atomic cursor files, plaintext HTTP (TLS is roadmap).
- The **CRDT-ish core is sound** and worth preserving: content-addressed
  immutable grains, adds are pure set-union (no conflict class), idempotent
  imports, deterministic provisional-head election on forks, HLC-based PITR
  (`areev restore --until-hlc`). The 2-hop supersession replication fix
  (`lib.rs:5211-5219`) is regression-tested.
- `ARCHITECTURE.md` §11's "directory of memory files, shards by key hash"
  hub is **aspirational — none of it is implemented**.

**Latency evidence (areev-bench):**
- Published numbers (M4 Max, 10–13k grains): structural recall p50
  **30–33µs**, `entity_latest` **9.2µs**, voice-loop frame recall **79µs
  p50 / 152µs p99** under the 50ms-cadence gate, add (FTS off) **117µs
  p50 / 136µs amortized**. Pi 3: recall 361µs, flat in corpus size.
- Two gates have teeth (`exit 1`): voice-loop frame recall p50 < 200µs
  (`voice_loop.rs:79`), honesty M3 write cost < 1000µs
  (`honesty_metrics.rs:170`). Neither runs in CI; both are manual
  pre-commit steps.
- **The hot paths are N+1 by design** (free in-process, lethal over a
  wire): `recall` = 1 triple query + **one blob query per hit** (1+k round
  trips; k=16 → 17), `thread_tail` = 1+n, hybrid recall ≈ 16 statements
  (per-token BM25 loop + per-hit blob loop), a plain `add` ≈ 16 statements
  (8 re-prepares + 8 inserts), FTS-on add ≈ 45–50, `related()` = one query
  per (node × relation × direction) per BFS level, `entity_at` walks the
  supersession chain one round trip per link.

**Atmatic (the deployment target):**
- Already evaluated Areev — memo at
  `atmatic/docs/work/research/areev-vs-postgres-migration.md` (2026-07-15):
  rejected as system-of-record, recommended as an additive agent-memory/RAG
  pilot (beachheads: the recommendations/continuous-learning path, and
  `search_history`, today an `ILIKE` substring scan).
- **Stateless Cloud Run, no persistent disk** (tmpfs counts against a 512Mi
  limit), worker autoscales 1→3 instances, one Node process serves all
  tenants via `AsyncLocalStorage` + `search_path` per request. An embedded
  file has nowhere durable to live and would diverge per instance.
- Everything already runs on **one Cloud SQL Postgres 16** (`pg` driver, no
  ORM, pgvector installed in an `ext` schema, HNSW in production, nightly
  backups + 7-day PITR). Tenancy is **schema-per-org**, chosen explicitly
  because cross-org reads become structurally impossible and
  `pg_dump -n org_N` / `DROP SCHEMA` are the export/erasure primitives.
- Compliance regime is UAE (DHA/ADHICS + PDPL) + Singapore PDPA, not HIPAA.
  Hard gates: **real erasure** (their memo verbatim: immutability "is a
  compliance *problem* for patient data, not a feature"), nightly retention
  sweeps, data residency (storage must sit in-region), append-only audit
  enforced by a DB trigger, never store clinical PHI in memory.
- HA today: none (no REGIONAL flag on Cloud SQL) — but zone-redundant HA is
  already a named pre-condition for real-patient go-live. If Areev stores
  into that instance, it **inherits** regional HA, PITR, and their drilled
  DR runbook for free. That is the entire enterprise-storage argument for
  this customer.

---

## 3. Latency impact — quantified

The engine's design assumption is ~1–2µs per SQL statement (in-process).
Substituting a per-statement network round trip:

| Operation | Round trips | Today (Turso) | Pg unix socket ~0.1ms | Pg same-VPC ~0.5ms | Pg cross-AZ ~1ms |
|---|---|---|---|---|---|
| `recall` k=16 | 17 | 30µs | 1.7ms | 8.5ms | 17ms |
| hybrid recall, voice shape k=8 | ~9 | 79µs | 0.9ms | 4.5ms | 9ms |
| hybrid recall, 5-token, all legs | ~16 | — | 1.6ms | 8ms | 16ms |
| `entity_latest` | 2 | 9.2µs | 0.2ms | 1ms | 2ms |
| `thread_tail` n=20 | 21 | 125µs | 2.1ms | 10.5ms | 21ms |
| `add` (FTS off) | ~16 | 117µs | 1.6ms | 8ms | 16ms |
| `add` (FTS on, 10 tokens) | ~45 | fsync-bound | 4.5ms | 22ms | 45ms |

Consequences:

1. **The voice gate cannot survive any network.** The 200µs hard gate
   breaks at ~22µs of per-statement latency; even a same-box Unix-socket
   Postgres (~50–80µs RTT) blows it 4–5×. Voice stays Turso, full stop.
2. **Unbatched, Postgres recall lands at 1–20ms — the same band as the
   hosted memory services our own frame chart uses as the foil** (Zep's
   "retrieval under 200ms"). The N+1 loops must be rewritten set-based for
   the Pg backend: blob fetch joined into the triple query or
   `= ANY($1::bigint[])`, the BM25 token loop as one query, `related()` /
   `entity_at` as recursive CTEs. Batched, a recall is 2–3 round trips →
   **~0.3–1ms same-VPC** — irrelevant inside a chat/agent turn that spends
   seconds in LLM inference, and honestly competitive for a server-tier
   memory.
3. **Claims must be scoped, not deleted.** "No server in the recall path",
   "recall in microseconds", the frame chart, and the Pi story remain true
   *of the embedded backend* and must be labeled as such in README/
   ARCHITECTURE/FAQ. The Pg backend gets its own published numbers and its
   own gates (proposed: batched recall p50 < 2ms same-VPC; add p50 < 5ms).
   Never present Turso numbers for a Postgres deployment.

---

## 4. Options

### Option A — Status quo + hub hardening (no Postgres)
Harden what exists: fsync/atomic segment writes, an outbound
`GET /api/bundle?since=` feed, file locking with a clean `STO-Ennn` error,
TLS, concurrent server. Enterprises get active/passive hub on a durable
volume; edges keep everything.
- **For:** smallest effort; no claim erosion; fixes real gaps worth fixing
  anyway.
- **Against:** does not solve Atmatic at all — Cloud Run has no volume;
  a singleton hub with a bearer token is not an enterprise HA story; every
  serious buyer will ask "why can't it use our Postgres?" and the answer
  stays no.

### Option B — Postgres as a second backend behind `CalStoreFacade` (recommended)
`PgStore` implements the existing 67-method facade trait; Turso remains the
embedded backend and the default. Feature-flagged (`postgres`, default off)
so edge builds stay dependency-light. Details in §5.
- **For:** solves the actual blocker (stateless deployments); inherits the
  customer's existing HA/PITR/DR/residency; preserves every edge claim;
  prior art exists (the ancestor codebase's Pg facade); the oplog/bundle
  format becomes the edge↔cloud interchange, which is a coherent story.
- **Against:** large effort (§6) — a second write path whose fork/head
  semantics must stay behaviorally identical, a set-based rewrite of the
  hot loops, a Pg test harness alongside ~135 tempdir-based store tests,
  and two backends to maintain forever. The multi-writer version requires
  moving RAM counters into DB sequences — a genuine redesign of the write
  path and BM25 stats.

### Option C — Replace Turso with Postgres everywhere
- **Against, decisively:** kills the microsecond recall claim, design goal
  #1 ("a real-time voice loop that cannot pay a network round trip"), the
  one-memory-one-file invariant (unit of erasure/sync/portability), Turso
  page-level crypto-erasure, the standalone edge/Pi story, the trust-suite
  artifacts (kill −9, tamper, WAL-scan — all file-level evidence), and the
  frame-chart argument that transport-not-storage is the latency budget.
  It would refute our own published benchmarks. **Rejected.**

### Option D — Postgres only as hub durability (segments/oplog in Pg, no query engine)
Hub archives MGB1 segments + oplog into Postgres tables; recall still needs
a Turso file materialized somewhere.
- **For:** small; makes the hub's record-of-truth HA.
- **Against:** doesn't serve recall from stateless containers — Atmatic's
  actual need — and adds a rehydration step nobody asked for. Only worth it
  as an incidental byproduct of B. **Rejected as a standalone option.**

---

## 5. Recommended design (Option B)

### 5.1 Tier placement
- **Edge (unchanged):** in-process Turso file. Voice, appliances, CLI,
  local agents. All current claims and gates apply here only.
- **Server tier (new):** app processes embed Areev via the existing
  bindings (Node/Python) with a Postgres-backed store. No new deployable —
  for Atmatic this means the napi binding inside channels-worker pointed at
  the already-attached Cloud SQL socket, not an Areev sidecar service.
- **Hub:** on the Pg backend, "hub" stops being a special singleton — any
  app instance can accept segment pushes and apply them into Postgres via
  the same idempotent `import_bundle` semantics. Edge↔cloud sync keeps the
  MGB1 bundle/oplog wire format unchanged.

### 5.2 One memory = one schema
The file invariant maps to **one Postgres schema per memory**
(`areev_<memory>` or, for Atmatic, inside the org's existing schema
resolution). This preserves the invariant's *semantics*:
- **Unit of erasure:** `DROP SCHEMA … CASCADE` (mirrors their
  `DROP SCHEMA org_N` primitive).
- **Unit of portability:** `pg_dump -n <schema>`, plus `areev bundle` export
  works from any backend since the oplog is a table.
- **Isolation:** cross-memory reads are structurally impossible to express
  by accident — the exact property Atmatic chose schema-per-org for. A
  single table set with a `memory_id` column is explicitly **not**
  acceptable to this buyer; schema scoping is a requirement.
- **Mounts/ASSEMBLE:** a mount becomes a second `PgStore` handle with a
  different schema (and optionally a read-only role). ASSEMBLE is already
  backend-agnostic; read-only-by-construction must become read-only-by-role
  — a new, small security surface to document.

### 5.3 Concurrency in two stages
- **Stage 1 — single writer per memory, enforced.** Take
  `pg_advisory_lock(hash(schema))` at open; a second writer gets a clean
  new `STO-Ennn` error instead of today's undefined behavior. RAM counters
  and the in-memory dictionary keep working unchanged. This alone unlocks
  Cloud Run *if* writes for a given memory route to one instance — which is
  restrictive with 3 autoscaled instances, hence:
- **Stage 2 — true multi-writer.** `seq`/`op_seq`/`term_id` become
  `IDENTITY`/sequences with `RETURNING`; dictionary interning becomes
  `INSERT … ON CONFLICT DO NOTHING` + select (cache remains as a
  write-through cache, never authoritative); HLC allocation moves behind
  the lock or a `hlc` table; BM25 `N`/`avgdl` are computed per query from
  `fts_doc` aggregates instead of RAM counters. Fork semantics carry over
  cleanly: local add collapses heads, import UNIONs — both expressible as
  single statements under `READ COMMITTED` with row locks on
  `heads(ns,s,p)`.

### 5.4 Storage mapping
- **Blobs:** `bytea` column (grains are small; the `.blobs` CAS sidecar
  maps to a `blobs(hash bytea PK, body bytea)` table; S3 offload is a
  later option). All `fs`-path–derived sidecar logic is backend-internal.
- **Vectors:** pgvector `vector(N)` + `<=>` cosine; optional HNSW. The
  runtime-supplied embedder dim becomes DDL-fixed at first
  `set_embedder` — dim mismatch becomes a hard, early `VAL` error instead
  of today's warning (an improvement for this tier).
- **BM25:** scoring is already Rust; port the postings pull as one batched
  query per search. Do **not** switch to `tsvector` — ranking parity with
  the embedded backend matters more than native FTS.
- **Encryption/erasure:** Turso's page cipher has no analogue. For this
  tier the compliance-correct story is the opposite one anyway: **real,
  gated deletion**. Proposal: a schema-level erase (memory drop) plus a
  subject-scoped hard-delete for right-to-erasure, gated the same way
  `FORGET` is (`allow_destructive_ops`) *and* — mirroring Atmatic's own
  audit-trigger pattern — a transaction-local GUC the caller must set.
  **This widens the destructive surface and per CLAUDE.md invariant #3
  requires an explicit design + OMS-conformance decision before build.**
  Optional preservation of crypto-erasure: envelope-encrypt grain blobs
  with a per-memory data key (destroy key = erase blobs), documented as
  weaker than the file cipher (index columns remain plaintext).
- **Telemetry sidecar:** tables in the same schema; the in-memory buffered
  capture path is untouched (it does no SQL on the hot path).

### 5.5 Code shape (build order)
1. **Exec-wrapper refactor** in `areev-store`: a `Db { execute, query,
   prepare, txn }` module; route all 62 `block_on` sites through it. Pure
   refactor, green under the existing ~950-test suite. Highest-leverage
   single step; valuable even if Postgres never ships.
2. **Close the `with_store` hole**: promote the ~87 concrete-type escape
   hatches (py/js/areev-loop/server/mcp/cli, plus the CLI's direct calls) into
   `CalStoreFacade` methods.
   Independently valuable — it makes the trait the crate already claims to
   have real. Note `areev-js` is outside the workspace; its 23 sites need
   the napi build + `node --test` run by hand.
3. **Batch the N+1 hot loops** (blob fetch `IN`-join, BM25 token batch,
   recursive CTE for graph/chain walks) behind the wrapper — benefits the
   Turso path marginally, is the difference between 17ms and 1ms for Pg.
4. **`PgStore` stage 1** (tokio-postgres behind `feature = "postgres"`,
   schema-per-memory, advisory-locked single writer, pgvector, batched
   reads; testcontainers-based CI job, gated so default CI is unchanged).
5. **Bindings + CLI surface**: `Areev.open("postgres://…?schema=…")` in
   py/js; `areev --db postgres://…` where it makes sense.
6. **Stage 2 multi-writer** + the gated erasure design + Pg-tier perf gates
   and published RESULTS.md section.
7. **Docs pass**: scope every "no server / microseconds / one file" claim
   to the embedded backend; add the two-tier deployment story to
   ARCHITECTURE.md §11 (replacing the aspirational hub text with the real
   design).

### 5.6 Dependency policy
`tokio-postgres` (fits the existing private-runtime wrapper; the store API
stays sync) behind a non-default cargo feature. Edge builds compile exactly
as today. This is a deliberate, contained exception to the
dependency-light rule, confined to one crate and one feature flag. Cloud
SQL via the Cloud Run attachment is a Unix socket (no TLS stack needed);
rustls only if/when TCP+TLS connections are demanded.

---

## 6. Effort and risk (honest sizing)

| Step | Size | Risk |
|---|---|---|
| 1. Exec wrapper | ~1 week | Low — mechanical, suite-guarded |
| 2. Close `with_store` (~87 sites + CLI routing pass, 6 crates) | 2–3 weeks | Medium — js is outside the workspace test net |
| 3. Batch N+1 loops | ~1 week | Medium — ranking/order parity must be golden-tested |
| 4. PgStore stage 1 | 3–4 weeks | High — dialect + behavioral parity of two write paths (local collapse vs import union); needs a conformance suite run against both backends |
| 5. Bindings/CLI | ~1 week | Low |
| 6. Stage 2 multi-writer + gated erasure | 3–4 weeks | High — write-path redesign + an OMS-level destruction decision |
| 7. Docs/claims/benches | ~1 week | Low, but mandatory — unscoped claims become false the day Pg ships |

Total: roughly **2.5–3.5 months** of focused work to stage-2 quality; a
usable stage-1 pilot for Atmatic (single-writer, recommendations/
`search_history` beachhead, no clinical PHI) after steps 1–5, ~7–9 weeks.
Steps 1–3 are no-regret and improve the existing product regardless of the
Postgres decision.

Biggest risks: (a) behavioral parity of fork/head semantics across two
write paths — mitigate with a backend-parameterized conformance test suite
run against both; (b) silent `areev-js` breakage — its 23 `with_store`
sites are invisible to `cargo test --workspace`; (c) scope creep toward
"distributed Areev" — stage 2 is multi-writer on one Postgres, not
multi-region consensus.

---

## 7. Open decisions (need explicit sign-off)

1. **Widening the destructive surface** (subject-scoped hard delete +
   schema drop) — CLAUDE.md invariant #3 requires a design +
   OMS-conformance decision. Healthcare buyers make this unavoidable;
   the gating pattern (config flag + txn-local GUC + audit grain) is the
   proposal.
2. **Claim scoping** in public docs/benches — accept that README/
   ARCHITECTURE get "on the embedded backend" qualifiers.
3. **Feature-flag vs separate crate** for `PgStore` (`areev-store`
   feature vs `areev-store-pg`) — proposal: feature flag first, split
   only if the dependency tree demands it.
4. **Stage 2 timing** — ship the stage-1 pilot to Atmatic first and let
   real usage decide whether multi-writer is needed before the write-path
   redesign.
