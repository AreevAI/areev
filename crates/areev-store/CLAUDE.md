# areev-store

The store: backend-agnostic store logic (src/lib.rs) over an internal sync
`Db` seam (src/db.rs — `execute/query/query_hot/begin/commit/rollback`, plus
the `prefers_batched_reads`/`ensure_embeddings` capability hooks). Two
transports implement it:

- **`TursoDb`** (default; embedded): one memory = one Turso database file.
  Owns a tokio current-thread `Runtime`, a single `Connection`, and the
  SQL-keyed prepared-statement cache (the `_hot` calls). Point reads are
  µs-class; `prefers_batched_reads = false` because a parameterized `IN` on
  the PK is a table scan on this engine — measured ~8x on the voice frame.
- **`PgDb`** (src/pg.rs, `feature = "postgres"`): one memory = one Postgres
  schema, **multiple concurrent writers allowed**. Write txns claim id
  blocks from the in-schema `counters` row via the `Db::reserve_write` hook
  (which also serializes concurrent write txns, keeping op-log order equal
  to commit order); the term dictionary and BM25 collection stats are
  DB-authoritative on cache miss (`intern_term`/`lookup_term*`/
  `collection_stats` hooks); in-txn rechecks use `Db::for_update`. An
  explicit statement translator handles the divergent dialect (per-table
  `ON CONFLICT` upserts, pgvector `<=>`/casts, `?N`→`$N`) and FAILS FAST on
  anything unmapped, the `vector(dim)` column is added at the first
  `set_embedder` (dim mismatch = hard refusal), CAS blobs live in an
  in-schema table, and `prefers_batched_reads = true`. Page cipher and the
  telemetry sidecar are file-backend-only and rejected at open.
  **Every session opens through `pgtls::connect`** (src/pgtls.rs) — the one
  place a Postgres socket is made, for `open`, `reconnect`, and
  `drop_postgres_schema` alike. It splits `sslmode`/`sslrootcert` out of the
  DSN (the driver rejects `sslrootcert` as an unknown option and knows only
  three of libpq's five `sslmode` rungs), maps the mode onto a driver mode
  plus a rustls certificate verifier, and spawns the connection future. The
  verification semantics are **libpq's**: `require` encrypts without
  validating, only `verify-ca`/`verify-full` check the chain — because RDS
  signs with private roots and a stricter `require` would break every stock
  RDS DSN. Behind `feature = "postgres-tls"`; without it a DSN asking for
  `require` or above is refused (**`STO-E003`**), never downgraded to
  plaintext, and `disable`/`prefer` behave exactly as before. Pinned by a
  real handshake against a throwaway rcgen CA (`pgtls::handshake_tests`),
  which is what proves `verify-full` actually verifies.

In-memory counters (`next_seq/next_op/next_term/hlc_last`, BM25 stats)
loaded on open are authoritative only on the embedded backend
(**single-writer-per-FILE**); on Postgres they are a fallback the
multi-writer hooks override. Because those counters are per-handle, a second
handle on one file would drift and then collide — so `open_internal` claims the
absolute path in a process-wide `OPEN_FILES` registry and a second open fails
with **`STO-E002`** (`OpenFileGuard`, released on drop; `None` on Postgres,
which is genuinely multi-writer). The cross-process OS lock never caught this
because within one process that lock is already held. Node has no
deterministic drop, so its binding exposes `close()`.

`add` is **idempotent on the content address**: `add_batch_inner` probes `has`
before reserving counters and skips grains already stored (and duplicates
within the batch), returning the existing hash. Byte-identical grains are one
grain; the old behaviour raised `UNIQUE constraint failed: grains.hash` for two
identical events in the same millisecond. Cross-backend parity is pinned by
`crates/areev-conformance` — the same case list runs against both (plus
Pg-only multi-writer race cases); extend it whenever store semantics change.

## Schema (SCHEMA const, lib.rs ~160)

- `terms(id, term)` — the dictionary; S/R/O strings become fixed-width ids
  (`term_id` cached forward map; `term_str` is an O(n) reverse scan). On
  Postgres the row is `terms(id, term, term_hash)` and **uniqueness is on
  `term_hash` (SHA-256 of the UTF-8 bytes), never on `term`** (#160): a
  btree entry caps at ~2704 bytes after pglz, so `text UNIQUE` refused any
  value that did not compress under it — random/base64/JSON at 2.7 KB while
  8 KB of `x` passed — and the loop's ledger crossed it by itself. Every
  S/R/O string is interned, so the dictionary must not bound a value. The
  open-time migration in `PG_SEED` adds and backfills the column
  (`sha256(convert_to(term,'UTF8'))` must equal Rust's
  `Sha256::digest(term.as_bytes())` — pinned by the pg conformance test),
  creates the unique index, and drops the old text constraint; a read-only
  open refuses a schema without the column (`STO-E005`). The scrub path
  re-keys the row through the `Db::scrub_term` seam. Conformance:
  `incompressible_values_of_any_size_are_stored` (both backends).
- `grains` — `seq` PK, `hash` (content address), ns/gtype/created_at,
  s/p/o dict ids, `vf/vt` (world-time validity), `svf/svt` (knowledge-time /
  supersession), `superseded_by/supersedes`, `text` (FTS source), and the
  **immutable serialized blob**.
- "2½ permutations": `triples` with `idx_spo` + `idx_pos` (mandatory) plus a
  separate `osp` table — the "½" — written **only** when the relation is in
  `AreevOptions::entity_relations`. Reverse traversal (`Direction::In/Both`)
  silently finds nothing for relations outside that set. **Exception**:
  `related_to` cross-links always get `osp` rows, because a link's object is a
  grain hash and therefore always an entity. `grains_by_object` is the
  object-anchored mirror of an anchored `recall_hybrid` ("what points at X"),
  and is what makes `WITH multi_hop` follow reverse edges — so it inherits the
  same entity-relations restriction.
- **Subject anchors**: a grain carrying a `subject` but NOT a full `(s,p,o)`
  triple (an Event about a message id, an Observation about an entity) still
  gets one `triples` row — `(ns, s, NULL, NULL, seq, cur)`. Relation and object
  are NULL because the grain asserts neither, which also makes the row inert to
  every relation-bound query by construction; like links it never reaches
  `heads`/`entity_latest` (a log entry about a subject has no "current value").
  Requiring all three used to drop these grains from the index entirely, which
  cost two things, the second far worse than the first: `recall(ns, subject, …)`
  answered empty for a grain that plainly carried the subject, and — because
  `forget_subject`/`subject_report` select through `triples` — the identity's
  own grain was invisible to **erasure and DSAR disclosure**. Existing files are
  healed by the `link_index` stamp bump (v3) on open; the rebuild replays the
  rows and reconstructs `cur` from supersession state, so a reindex neither
  duplicates a grain nor resurrects a superseded one.
- **Cross-grain links**: `GrainCommon.related_to` entries index as triples
  subject-ed on the linking grain's *own* hash — `(own_hash, relation_type,
  target_hash)` — so `related()`/`path()` traverse them like any edge. They are
  written to `triples`/`osp` **only**, never `heads`/`entity_latest`: OMS §15.3
  is normative that such a link is an annotation and MUST NOT alter the target's
  supersession state. `step_actions()` reads the OMS §8.4 execution-record
  family `mg:step_action:<node_id>` (Tool grain → the Workflow node it ran);
  that relation is parameterized, so its predicates are found by dictionary
  prefix scan rather than a static vocabulary. Files written before this
  indexing existed need `areev reindex`.
- `entity_latest` PK(ns,s,p) — the µs point read. `heads` PK(ns,s,p,seq) —
  fork tips. `oplog(op_seq, hlc, op, hash)` — OP_ADD/OP_SUPERSEDE/OP_FORGET.
  `thread_idx` — session transcripts. `embeddings(seq, vec)`.
- `idx_grains_ns_s` on `grains(ns, s, p)` — **the vector legs' filter index.**
  They all read `FROM embeddings e JOIN grains g ON g.seq = e.seq WHERE
  g.ns = ? [AND g.s = ?] [AND g.p = ?] ORDER BY <distance> LIMIT k`, and with
  only `idx_grains_hash` to work with the planner scanned every grain and
  scored every vector — so a *scoped* k-NN cost exactly what an unscoped one
  did, and namespace/subject filtering bought a constant factor instead of a
  proportional one. Measured at 100k grains: 39.8 ms → 0.20 ms, with the
  unfiltered and BM25 legs unmoved (RESULTS.md §8b). Column order is load
  bearing and was measured: `(ns, s, p)` gives seekable prefixes for all three
  scoped arms, and putting `svt` ahead of `s` to also absorb `svt IS NULL`
  **destroys the win** — the null test is not an equality constraint, so it
  truncates the seek. Present in both backends' schemas; `CREATE INDEX IF NOT
  EXISTS` on every open is the migration, so an existing file pays the build
  once at its next open.
- BM25 leg: `fts_vocab(id, term)` + `fts_post(term, seq, ns, tf)` +
  `fts_doc(seq, len)` — our own inverted index. Written on add, dropped on
  `forget`, rebuilt by `rebuild_text_index`. **Meant to be deleted** if
  tursodatabase/turso#8170 is fixed — see `docs/facts/bm25-index.md`.
- **The join** (`prov_idx`, `run_idx`): `prov_idx(ns, parent BLOB, seq)` is
  reverse provenance — parent content address to the grains derived from it;
  `run_idx(ns, run, seq)` maps `run_id` to the grains recorded during a run.
  Deliberately narrow tables rather than triple rows: `derived_from` sits on
  *every* grain, so indexing it as triples would inflate the index recall
  scans. `run_trace`/`run_yield`/`runs_touching` are built on them —
  `run_yield` crosses from execution history into semantic memory (what a run
  *produced*, not what it recorded). `grains_derived_from` is served by
  `prov_idx`; it used to scan and deserialize every grain in the store.
  `run_id` is written through `Capture` (so `remember`/`capture` set it on
  every surface, not just Rust).
  `rebuild_link_indexes()` backfills all three (plus `related_to` links) and is
  wired into `areev reindex`, `reindex_links()` and `reindexLinks()` — but
  **open() heals automatically**: the `link_index` meta row is the file-truth,
  and a missing or stale version triggers a rebuild plus an `open_warnings()`
  note. Emptiness is not the signal (a file may legitimately have no links);
  the stamp is. **`forget` must delete from `prov_idx`/`run_idx` like every
  other index** — `seq` is re-derived as `MAX(seq)+1` on open, so a surviving
  row gets inherited by the next write.
- `ns_reg(ns, n)` — the **namespace registry**: one row per namespace with at
  least one `grains` row, `n` = that count. What makes prefix scoping
  (`"org.*"`) resolvable without scanning grains, and what the fail-closed
  authz sweep over an expansion enumerates. Count-maintained at every
  grain-row insert/delete choke point (`insert_prepped`, `insert_blob`,
  `forget`, `erase_where` — replication therefore maintains it by
  construction) and DELETEd at zero, so an emptied namespace stops matching
  scopes and stops demanding grants. Self-healed from `grains` on open when
  the `ns_registry` meta stamp is missing/stale (the `link_index` pattern).
  Read via `namespaces()` / `namespaces_in_scope(&NsScope)`.
- `meta(k, v)` — **file-carried declarations**:
  `text_index` ("1"/"0"), `entity_relations` (sorted JSON array),
  `embedding_model`/`embedding_dim` (provenance, stamped by the first
  `set_embedder`), `min_reader_version` (stamped when a grain newer than
  `0x0B` is written — `deserialize_blob` errors on an unknown type byte rather
  than skipping it, so such a file is unreadable, not partially readable, to an
  older build). Bare `open()` honors these; `open_with()` re-stamps and
  records changes in `open_warnings()`; a different-dim embedder warns
  instead of mixing vector spaces. Host config is never persisted here —
  the file describes itself, the host supplies capabilities.
  `tests/meta_tests.rs` covers persistence/reconciliation.
- CAS blob sidecar at `"{path}.blobs"`, git-style `hex[..2]/hex[2..]` fan-out:
  `put_blob` (idempotent, tmp+rename), `get_blob` (re-verifies sha256),
  `gc_blobs` (ref-count from live grains' `content_refs`). Free fn
  **`read_blob_offline(db_path, uri)`** reads one blob WITHOUT opening the
  database — the file lock is exclusive, so while a run holds a memory a second
  process is refused even for a read, which would strand an attachment out of
  reach of the `--tool-cmd` subprocess meant to process it. Safe without a lock
  because a blob is immutable, lives beside the file, and its address is its
  checksum (re-verified here too). `Ok(None)` = sealed, so the caller must open
  with the key. Surfaces: `areev blob put|get` (get is served pre-open),
  `put_blob`/`get_blob` in both bindings — **bytes in, bytes out**, the one
  documented exception to the JSON-strings-out FFI convention.

## Core invariants

- **Blobs are immutable.** `supersede` and `forget` mutate the index layer
  only (`svt`, `superseded_by`, head recompute); stored blobs never change.
- Double-supersede of the same head → `SupersessionConflict` error locally;
  the same event arriving via import becomes a **fork** instead.
- Unknown terms short-circuit to empty results, never errors.
- HLC = `now_ms() << 16`, monotone, restored from `MAX(hlc)` on open.

## Forks / heads / merge (the "grains as git" model)

- Local add collapses the head (DELETE+INSERT into `heads`); **import UNIONs**
  (`insert_blob`), which is what creates forks.
- `apply_supersede_flip`: old grain already superseded by a *different* grain
  → keep both tips as heads. Deterministic provisional head everywhere =
  max `(created_at, hash)` tuple — zero coordination, same answer on every
  node. `heads()` orders provisional-first.
- `merge_heads` requires ≥2 tips, records all `merge_parents` in `context`
  (inside the blob, so it replicates), supersedes every open tip.

## Namespace scoping (`"org.*"`)

Every **plural read** (`recall`, `recall_hybrid*`, `search_text*`,
`search_vector*`, `nearest_vector`/`nearest_semantic`, `recent*`) accepts a
prefix scope in its namespace parameter: `"org.*"` = `org` + its `.`-descendants (`org.sales`, never
`organization`; parse rules in `areev_core::ns::NsScope`). Expansion resolves
against `ns_reg`; the legs then run over a namespace-id **set** — per-ns
probes merged on the file-global seq (which IS recency order) for the
structural/recent legs, `ns IN (…)`-scoped postings/vector scans for the
others, one RRF fusion (`recall_hybrid_ids`). A single exact namespace keeps
every cached statement untouched — the voice path never pays for this.
`recall_hybrid_scoped`/`recent_scoped` take an already-resolved LIST (the CAL
facade's path after per-namespace authz). Under a multi-namespace scope the
egress hint is `None`, so each grain resolves its own anon policy, and
telemetry records the pattern as typed.

Everything else **refuses patterns** via `require_exact_ns` (VAL-E001):
writes (`prep_from_blob(new_write=true)` — `insert_blob` stays permissive so
pre-reservation files import), point reads (`latest`, `thread_tail`, `heads`),
graph/run reads, destruction (`forget_subject`, `forget_older_than`,
`subject_report`/`subject_bundle` — the DSAR pair refuses identically to the
erasure it mirrors), and the policy setters (retention/floor/hold/anon).
Conformance: `cases/ns_scope.rs`, both backends; store tests:
`tests/ns_scope_tests.rs`.

The two nearest-neighbour reads were the **last plural reads still refusing a
pattern**, and the exception was expensive rather than merely inconsistent: a
corpus-wide semantic search had no way to spell itself except by falling
through to prefix-scoped `recall_hybrid`, which runs a BM25 leg and a
structural leg and fuses them to answer a question that is purely about
vectors — 605ms against 150ms for the scan it actually needed (RESULTS.md
§8c). Both now route through one private `nearest_scoped`, which keeps the
single-namespace path parameterized (`g.ns = ?1`, cached plan intact — the
voice path pays nothing) and inlines the id set only for a real multi-namespace
scope. Because cost is now proportional to the scope rather than the corpus,
the hierarchy has to be IN the namespace to be queryable: `deal.<sector>.<id>`
can express "this sector", `deal.<id>` cannot.

## Session-scoped and ordered reads

`recent_in_session[_scoped](ns, session, gtype, limit, live_only)` reads
`idx_thread(ns, session, seq)` directly — newest-first, `live_only`-aware,
optionally type-narrowed. It exists because `recent`/`recent_live` can only
scan a namespace by insertion order, so a session filter layered on top is a
post-filter over whatever page the scan returned: on a busy namespace the tail
of one conversation can be entirely outside that window, and the query answers
"nothing" instead of "the last N turns". The CAL facade routes
`WHERE session_id = "…"` here (issue #49). `thread_tail` is the same index in
transcript order (oldest→newest, all types, heads and superseded alike).

`recent_ordered[_scoped](…, RecentOrder)` serves an ORDER BY from the scan.
`RecentOrder::CreatedAt{Desc,Asc}` is the ONLY non-seq ordering, and
deliberately so: `created_at` is the only sort key the `grains` table carries
as a column. Every other field a caller might order by lives inside the
immutable blob, so SQL cannot reach it without materializing a column per
field — CAL ranks those in the executor over a widened scan instead
(`CAL-W015`). The two orders differ whenever grains are backdated or imported
out of order, which is exactly when a caller asks for `created_at` explicitly.

## Anonymization key material

Three keys are HKDF-derived from ONE root, with their own domain-separation
strings: the session key (`areev.anon.session.v1`), the memory key
(`areev.anon.memory.v1`, value-derived tokens) and the vault key
(`areev.vault.v1`, sealed mapping rows).

The root is `AreevOptions::anon_key` when the host supplies one, else the page
key. Before the `anon_key` seam existed the root could ONLY be the page key,
which the Postgres backend refuses outright (it is a page-cipher capability) —
so the backend built for stateless hosts could not use the egress control built
for untrusted egress, and value-derived tokens were unavailable on plaintext
files too. The vault itself was never the problem: its rows live in `meta`
under `VAULT_PREFIX`, which both backends have, and `forget_subject` scrubs
them through the same path on both.

The host key is never persisted — not in `meta`, not in a bundle, not in the
file. Rotating it makes existing vault rows permanently unreadable and changes
every derived token; that is a crypto-erasure lever as much as an operational
hazard. Conformance: `host_anon_key_unlocks_the_vault`, both backends.

## Hybrid recall

`recall_hybrid` = structural (`recall_seqs`) + BM25 (`search_text`, our own
inverted index over `fts_vocab`/`fts_post`/`fts_doc`, only when `index_text` —
**not** Turso's `USING fts`, and `docs/facts/bm25-index.md` says why plus how to
go back) + vector (`search_vector`, brute-force
`vector_distance_cos`) fused with RRF (k0=60). **Deadline-bounded fail-open**:
legs past the budget are skipped and partial results returned — never errors.
Embeddings come from the host via the `EmbedBackend` trait (`dim`/`embed`,
installed with `set_embedder`); there is no built-in model.

**Vector search is exact unless someone opts out.** The scan is linear in the
vectors a query cannot rule out, so the first lever is always to make it rule
more out — scope by namespace or subject and `idx_grains_ns_s` turns the scan
into a seek. Only a genuinely corpus-wide query has nothing to filter on, and
for that `ensure_vector_index(m, ef_construction, ef_search)` builds a pgvector
HNSW index: **Postgres only** (the embedded engine answers `STO-E007` rather
than no-op'ing, because a silent no-op leaves a host believing its corpus was
indexed while every query still scans it), 24 ms → 1.0 ms at 100k grains.
`drop_vector_index` is the way back to exact; `vector_index()` answers "are my
results exact right now?". It is deliberately NOT in `PG_SCHEMA`: an ANN index
changes recall from exact to approximate, and nobody should acquire that by
upgrading a binary. `ef_search` is session-scoped rather than a file truth —
the accuracy/latency trade belongs to the caller, not to the data. `CommandEmbed`
shells out to a host command per embed (text on stdin → JSON array on stdout;
CLI `--embed-cmd`, py `set_embedder_command`, js `setEmbedderCommand`) — fine
for turn-level recall, not the voice frame path.

The FTS/embed text projection is `projected_text` (lib.rs): the grain's
`embedding_text` override when present (import pipelines + memory_tool set
it), else "s r o" + top-level `content`. The write path, the reranker's
`candidate_text`, and the `rebuild_text_index` backfill all share it — keep
them in lockstep.

**Bulk loads**: `defer_text_index()` suspends posting writes (the `text` column
keeps populating), and `rebuild_text_index()` backfills NULL `text` from blobs
and re-tokenizes the whole corpus into `fts_post`/`fts_doc`. Deferral is process
state, not file state, so a process that dies mid-load reopens with an
incomplete index and open's self-heal rebuilds it. Postings are per-token, so a
bulk load no longer *needs* this the way it did when the leg was Turso's FTS —
it is still cheaper than writing postings per row.
`tests/text_index_tests.rs` pins the flow.

`recall_hybrid` delegates to `recall_hybrid_tuned(.., RecallTuning)`, which
adds the opt-in post-fusion refinements (all default off, all fail-open,
pool-capped at `REFINE_POOL`=64):
- **query expansion** (Tier-1): rule-based query variants → extra BM25 legs,
  RRF-fused. `QueryExpander` trait; built-in `EnglishExpander` (synonyms +
  naive stemming, English-only) when none installed via `set_query_expander`.
- **rerank** (Tier-2): a host-installed `RerankBackend` (`set_reranker` —
  same seam shape as `EmbedBackend`, no in-engine ML dep) re-scores the
  candidate pool's text; takes precedence over MMR.
- **diversity** (Tier-1): greedy MMR (`lambda·rel − (1−lambda)·max_sim`) over
  embedded candidates, using `vector_distance_cos` for both query-relevance
  and pairwise similarity; needs an embedder, silently skipped otherwise.
- **include_superseded**: widen *all three* legs from the heads to the whole
  supersession chain — structural drops `cur=1` (its own cached statements,
  `st_probe_*_all`), BM25 skips the `live_seqs` filter (`search_text_all`), the
  vector leg drops `svt IS NULL` (`search_vector_all`). Heads-only is the right
  default — stale values in a model's context are the failure mode — so this is
  strictly opt-in, for callers asking *about the past*. Forgotten grains cannot
  return: `forget` DELETEs the index rows, it does not flag them. Callers pair
  it with `supersession_map(&[Hash])` to label which results are stale;
  returning history unmarked is worse than not returning it.

CAL reaches these via the already-ported `WITH diversity|rerank|
query_expansion|superseded` options (executor → `RecallParams` →
`AreevFacade` → `RecallTuning`). Covered by `tests/recall_tuning_tests.rs`
(store) and areev-cal's `tests/recall_tuning_cal_tests.rs` (end-to-end).

## Bundles / sync

`BUNDLE_MAGIC = b"MGB1"`. `bundle_since(cursor)` exports op-log records
(`op·hlc·hash·len·blob`; forgotten grains have len 0). `import_bundle_until`
replays idempotently in op order; its `max_hlc` filter is point-in-time
restore. `changes_since` is the follow/pull cursor primitive. Streaming
("generations", `areev stream/restore/follow`) is CLI-level orchestration of
these same calls — there is no separate segment abstraction in this crate.

**Registry meta segment (`MGB2`)**: when the file carries replicable meta
rows (`REPLICABLE_META_PREFIXES` = `qry:`/`tpl:`/`retention:`/
`retention_floor:`/`anon:`), the bundle is v2 — magic `MGB2`, then
`meta_len(u32)·meta_json`, then the op records; registry-free bundles stay
byte-identical MGB1 (older builds refuse MGB2 loudly at the magic check).
Export strips `last_run_at` (usage never replicates); import merges
latest-wins on `updated_at` for `qry:`/`tpl:` (preserving local
`last_run_at`), write-if-absent for retention and anon rows (sync never swaps a live
policy; an applied `anon:` row re-arms the live handle's egress gate), applies nothing outside the allowlist (a crafted bundle cannot
touch `text_index`/`min_reader_version`), and skips the segment entirely on
a PITR import (meta rows have no HLC). Counted in
`ImportStats::meta_applied/meta_skipped`. Conformance:
`cases/meta_registry.rs`, both backends.

## Trigger state (`trg:` meta rows) + `meta_cas`

`meta_cas(key, expected, new) -> bool` is the store's one named conditional
write: `expected=None` claims a row only if absent (`INSERT OR IGNORE`),
`Some(prev)` swaps only if the row still holds exactly `prev` (`UPDATE … WHERE
v = prev`). Rows-affected is the answer. Both shapes already translate for
Postgres — `meta` has an upsert conflict target and `INSERT OR IGNORE` is
handled generically — so **no new dialect entry is needed**, which is the reason
this is a `meta` row rather than a `trg_state` table.

It compares the **whole value**, so a contended row must have exactly one
writer. That is the intent for a lease row; do not use it where fields are
independently owned.

`TriggerState` (next_due_at, cursor, op_cursor, claimed_by, lease_until, fence,
paused, consecutive_failures, last_error) rides `trg:<trigger-hash>`.

- **`trg:` is deliberately NOT in `REPLICABLE_META_PREFIXES`.** The declaration
  is a grain and replicates; this is per-host usage. Replicating it is wrong
  twice: two synced hosts ping-pong on each other's watermark, and a dev memory
  restored from prod inherits prod's cursor and silently skips real work while
  reporting success. Same rule as a saved query's `last_run_at`.
- **The fence lives inside the compared value**, which is what makes a lease
  fence without a token column: a holder whose lease expired carries a stale
  `v`, so its release matches no row and is refused. Kleppmann's argument, free,
  because the lock and the data are the same row.
- **An unparseable state row is an error, never "never fired."** Reading it as
  absent would re-fire everything — for a polling trigger, the whole backlog.
  Same fail-closed posture as an unreadable retention policy.

Conformance (both backends): `trigger_state_never_replicates`,
`meta_cas_admits_one_claimer_and_fences_the_loser`.

**`Areev::supersession_chain(&Hash) -> Result<Vec<Hash>>`** walks a grain's
`supersedes` column backward from the given hash to the first grain in its
edit history (no existing accessor exposed a single grain's `supersedes`
field outside `history()`'s own raw SQL, so this is the one place that does —
`history()`'s multi-grain walk stays separate since it also needs `blob`/
`superseded_by` and serves a different contract). Returns every hash visited,
**head first, root last**; a never-superseded grain's chain is itself alone.
Bounded at `MAX_SUPERSESSION_CHAIN_HOPS` (64, `pub const`) — exceeding it
returns `AreevError::SupersessionChainTooDeep` (`STO-E006`) rather than
looping forever, since a cyclic `supersedes` graph is corrupt data, not slow
data. A hash missing from the index (forgotten, or unknown) stops the walk
where it is, the same "tolerate the missing link" posture `history()` takes.

This is a general store primitive, not trigger-specific, but its motivating
caller is `areev-trigger`'s evaluator (#128): a `Trigger` grain re-pointed by
`SUPERSEDE` mints a new head for the same standing rule, and keying `trg:`
state (and derived run ids) on the head alone orphans the cursor and dedup
fence on every applied recommendation — see `docs/triggers.md` "Superseding
a trigger keeps its cursor". Conformance (both backends):
`supersession_chain_walks_to_the_first_grain`.

## memory_tool.rs

Anthropic memory-tool backend: `view/create/str_replace/insert/delete/rename`
over a `/memories/...` path space. Each file = a supersession chain of Fact
grains (`relation="memory_file"`, body in `context.content` so the term
dictionary never stores file bodies; body also mirrored into `embedding_text`
so files reach the BM25/vector legs). Every edit is a supersession; delete
forgets the whole chain; path traversal is rejected.

## migrate.rs

File-based importers from other memory systems (mem0 incl. history→
supersession replay, langgraph/langmem, letta + letta-archival, zep/graphiti
with bi-temporal validity, basic-memory notes → `memory_file` chains, generic
jsonl). Conventions: original timestamps in `created_at`, `source_type =
"import"`, provenance in `context.import`, prose in `context.content` +
capped `embedding_text`; re-runs skip what's already there (content-address
probe / chain-existence check). `migrate_payload` is the bindings' string
dispatcher and wraps the load in defer/rebuild_text_index; the CLI dispatcher
(`run_migrate` in areev) adds the basic-memory vault walk.
`tests/migrate_tests.rs` + areev `tests/migrate_smoke.rs` gate it.

## Read-only opens (`AreevOptions::read_only`, issue #127)

Backend-agnostic contract: `read_only: bool` (default `false`) makes a handle
refuse every write, on EITHER backend, with the same coded error
(**`STO-E004`**) — what lets one conformance case (`cases/read_only.rs`)
cover both. The bar it exists to clear: on postgres, opening an existing,
fully-migrated memory should not require handing the caller write authority,
because Postgres checks `CREATE` on the database before it checks whether a
schema exists, and table OWNERSHIP before it checks whether an index exists —
so even fully idempotent `IF NOT EXISTS` DDL 42501s a least-privilege
SELECT-only role. `docs/deployment-profile.md` has the grant recipe;
`areev ui --read-only` is the motivating consumer (paired with #124, so a
read-only console never needs a writable DSN).

- **Postgres open (`pg::PgDb::open`)**: `read_only` skips the bootstrap
  advisory lock, `CREATE SCHEMA`, `PG_SCHEMA`'s DDL and `PG_SEED`'s upserts
  entirely — none of it runs. It still issues `SET search_path` (a session
  command any role may issue), then VERIFIES with SELECT-only probes
  (`PgDb::verify_read_only`): schema existence via
  `information_schema.schemata`, then `to_regclass` on the five tables
  `finish_open` unconditionally reads right after open (`meta`, `terms`,
  `grains`, `oplog`, `fts_doc` — not the full `PG_SCHEMA` list). Failure is
  **`STO-E005`**, worded to distinguish "schema absent" (wrong name, or the
  memory was never created) from "schema present but not fully initialized"
  (an owning role needs to open it read-write once to finish bootstrap) —
  those need different operator actions, so the message names which one it
  is rather than surfacing a raw `42501`.
- **Embedded open**: proceeds exactly as read-write for an EXISTING file —
  there is no privilege system to fail against, and this process already
  owns the file — so a stale index still self-heals and file declarations
  still get read normally. But `read_only` must never bring a memory into
  existence — the postgres analogue of "schema absent" — so `open_internal`
  checks the main file's presence with `std::path::Path::exists` BEFORE
  anything touches the filesystem (before `TursoDb::open`, which would
  otherwise create the `.db` file and, once anything reads/writes through
  it, a `-wal`; before `finish_open`'s `create_dir_all` for `.blobs`).
  Absent, it refuses with **`STO-E005`** naming the path, mirroring
  postgres's wording — creating nothing. Only the MAIN file's absence
  refuses: an existing file with no `-wal`/`.blobs` yet (freshly
  checkpointed, or never blobbed) opens normally. Once past that check,
  `read_only` only starts mattering at the first write call.
- **`finish_open`'s own writes** (both backends: the `text_index`/
  `entity_relations` meta stamps, the legacy `idx_fts` drop, and the three
  self-heal rebuilds — text index, link indexes, namespace registry) are
  skipped under `read_only` rather than attempted-then-blocked, so a
  read-only open of a memory that would otherwise self-heal still succeeds;
  it pushes an `open_warnings()` note instead ("needs a one-time rebuild...
  read-only open, which never writes"). An explicit `index_text`/
  `entity_relations` that disagrees with the file's declaration is refused
  up front (`STO-E004`) rather than silently ignored, since honoring it
  would need the same write.
- **The telemetry sidecar is never attached under `read_only`**, on either
  backend, even if the caller asked for it — its flush is itself a write
  (on close and on explicit flush), and on postgres `Telemetry::open_pg`
  bootstraps `telem_*` tables in the SAME schema via its own `PgDb::open`,
  which a least-privilege role can no more do than the main schema's
  tables. The store only warns (`open_warnings()`) when the CALLER passed a
  non-`Off` `telemetry_mode` — the CLI's own default (`--telemetry`
  unspecified resolves telemetry straight to `Off` once `--read-only` is
  set, rather than asking for `Aggregate` and having the store downgrade
  it) never triggers this, so `areev … --read-only` is silent on a plain
  invocation; an explicit `--telemetry aggregate|full --read-only` still
  gets the warning, since that combination genuinely is a caller asking for
  something read-only cannot provide. The store's warning text names no
  specific API (no Rust field, no CLI flag) since it serves every binding.
- **A missing memory + a real one behave IDENTICALLY across backends**:
  `read_only_open_of_missing_memory_refuses_and_creates_nothing`
  (conformance, both backends via `Backend::try_open_named_with`, which
  returns the `Result` `open_named_with` panics on) asserts `STO-E005` and
  that a subsequent normal open still finds a genuinely empty memory; the
  embedded-only twin in `tests/read_only_tests.rs` additionally reads the
  directory before/after, since "creates nothing" is a filesystem claim
  postgres has no analogue for.
- **The write gate**: `Areev::check_writable` is the single function every
  mutating entry point calls first — not `Db::reserve_write` alone, because
  on postgres `reserve_write`'s `UPDATE … RETURNING` runs through the
  driver's `query` path, not `execute`, so a check placed only inside it
  would miss `intern_term`'s `INSERT … RETURNING` and the raw `blobs`/`meta`
  writes that never allocate an id block at all. It is called from: every
  grain-write entry point (`add`/`add_if_novel`, `supersede`, `forget`,
  `forget_subject`/`forget_older_than`, `merge_heads`, bundle import,
  `rebuild_text_index`/`rebuild_link_indexes`/`rebuild_ns_registry` i.e.
  `areev reindex` — NOT their internal self-heal calls from `finish_open`,
  which are skipped outright as above); `meta_put`/`meta_delete`/`meta_cas`
  (the one choke point behind retention/anon policies, saved queries and
  templates, trigger state, and the embedding/min-reader-version provenance
  stamps — audited by grepping every direct `self.db.execute` for `INSERT`/
  `UPDATE`/`DELETE` in `lib.rs`); `put_blob`/`gc_blobs`/`encrypt_blobs`
  (CAS — `get_blob` is a read and stays open); and `set_embedder`'s call
  into `ensure_embeddings` (postgres's lazy `vector(dim)` column DDL — the
  embedder still installs when the memory already declares matching
  provenance, since that path touches no database).
- Reads are untouched: `recall`/`recall_hybrid*`/`search_*`/CAL `SELECT`
  never call `check_writable`.

## Turso gotchas (documented in-code)

- `experimental_index_method(true)` is required at open.
- FTS costs ~150ms per write txn once the index exists — even for NULL text.
  Voice/edge profile runs `AreevOptions { index_text: false }` (see
  `examples/voice_loop.rs`).
- `PRAGMA integrity_check` miscounts experimental FTS internals; `verify()`
  classifies `__turso_internal_fts` lines as benign `fts_notes`. The real
  tamper check is the per-blob content-address re-hash.

## Tests & benches

`cargo test -p areev-store`. All tests use `tempfile::TempDir`.
- `store_tests.rs` — add/recall/supersede/forget, graph ops, `entity_at`
  both axes, reopen persistence.
- `fork_merge_tests.rs` — fork → provisional head → merge (uses **fixed**
  `created_at` values to make the tiebreak deterministic — copy that pattern).
- `fts_hybrid_tests.rs` — RRF ranking, zero-deadline fail-open.
- `multilingual_vector_tests.rs` — `TrigramEmbed` test backend, EN/AR/ZH.
- `bundle_blob_tests.rs` — CAS + bundle replication.
- `memtool_remember_tests.rs` — memory-tool cookbook flows, `remember()`.
- `read_only_tests.rs` — `AreevOptions::read_only`: succeeds against an
  existing memory, refuses every write family with `STO-E004`, reads
  unaffected, conflicting explicit `index_text` refused up front, a missing
  memory refuses with `STO-E005` and creates NOTHING on disk (directory
  read before/after), and an existing file with no `-wal`/`.blobs` sidecars
  still opens. The cross-backend twins are `areev-conformance`'s
  `read_only_open_succeeds_and_refuses_every_write` and
  `read_only_open_of_missing_memory_refuses_and_creates_nothing`.

Benchmarks: `cargo run --release -p areev-store --example bench` (latency
gates: recall p50 < 200µs, latest < 100µs) and `--example voice_loop`
(50ms frame cadence; spin-waits rather than sleeps).
