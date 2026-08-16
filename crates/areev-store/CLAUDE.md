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
  (`term_id` cached forward map; `term_str` is an O(n) reverse scan).
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
  `gc_blobs` (ref-count from live grains' `content_refs`).

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

## Hybrid recall

`recall_hybrid` = structural (`recall_seqs`) + BM25 (`search_text`, our own
inverted index over `fts_vocab`/`fts_post`/`fts_doc`, only when `index_text` —
**not** Turso's `USING fts`, and `docs/facts/bm25-index.md` says why plus how to
go back) + vector (`search_vector`, brute-force
`vector_distance_cos`) fused with RRF (k0=60). **Deadline-bounded fail-open**:
legs past the budget are skipped and partial results returned — never errors.
Embeddings come from the host via the `EmbedBackend` trait (`dim`/`embed`,
installed with `set_embedder`); there is no built-in model. `CommandEmbed`
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
`retention_floor:`), the bundle is v2 — magic `MGB2`, then
`meta_len(u32)·meta_json`, then the op records; registry-free bundles stay
byte-identical MGB1 (older builds refuse MGB2 loudly at the magic check).
Export strips `last_run_at` (usage never replicates); import merges
latest-wins on `updated_at` for `qry:`/`tpl:` (preserving local
`last_run_at`), write-if-absent for retention rows (sync never swaps a live
policy), applies nothing outside the allowlist (a crafted bundle cannot
touch `text_index`/`min_reader_version`), and skips the segment entirely on
a PITR import (meta rows have no HLC). Counted in
`ImportStats::meta_applied/meta_skipped`. Conformance:
`cases/meta_registry.rs`, both backends.

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

Benchmarks: `cargo run --release -p areev-store --example bench` (latency
gates: recall p50 < 200µs, latest < 100µs) and `--example voice_loop`
(50ms frame cadence; spin-waits rather than sleeps).
