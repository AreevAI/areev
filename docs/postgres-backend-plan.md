# Postgres Backend — Implementation Plan (Option B)

**Status: stage 1 BUILT (2026-08-07). Companion to [`postgres-backend-proposal.md`](postgres-backend-proposal.md), which holds the why; this holds the how.**

> **Implementation record.** Phases 0.1–0.4, 2 (stage 1) and 3 landed on main
> 2026-08-07 (commits `c72b076`…): the `Db` seam + `TursoDb`, the
> `areev-conformance` crate (one case list, both backends), the fidelity
> fixes, backend-shaped read batching (`prefers_batched_reads` — a
> parameterized `IN` on the PK is a table scan on the embedded engine, so
> Turso keeps point-read loops and Pg batches), `PgDb` behind
> `feature = "postgres"` with the fail-fast statement translator, advisory-
> lock single-writer (`STO-E002`), lazy `vector(dim)` column, in-schema CAS
> blobs, `areev --db postgres://…?schema=…`, and the `pgvector/pgvector:pg16`
> CI job. Phase 1 (facade trait promotion) is deferred — with the Db-seam
> design every surface works on a Pg-backed `Areev` unchanged.
>
> **Stage 2 (multi-writer) landed the same day** after the deployment
> requirement was confirmed: the exclusive advisory lock is gone; write
> transactions claim id blocks from an in-schema `counters` row via
> `Db::reserve_write` (allocation + brief serialization in one — op-log
> order equals commit order, so followers never miss ops), the dictionary
> and BM25 stats are DB-authoritative on cache miss, and supersede/forget
> recheck under `FOR UPDATE` so races produce single-writer-identical
> outcomes. Pinned by multi-writer conformance cases (concurrent writers
> land gapless; racing supersedes → one winner + one SupersessionConflict
> with the loser's txn rolled back; multi-memory-per-instance isolation).
> Remaining open: gated subject-scoped erasure (OMS destruction decision),
> bindings openers (py/js `open_postgres`), telemetry-on-Pg, Pg perf gates
> in RESULTS.md.

Decision taken: PostgreSQL as a **second** storage backend behind `CalStoreFacade`,
feature-flagged, never the default. Turso remains the embedded backend and keeps
every edge claim. This plan is built from a statement-level audit of
`areev-store`, a call-site-level audit of every concrete-type escape hatch, and
the test/CI inventory.

---

## 0. Verified ground truth (corrections to the proposal)

The proposal was written from a first survey; the deep audit corrected two
numbers and confirmed the rest:

| Proposal said | Verified reality |
|---|---|
| `CalStoreFacade` has 67 methods | **31 methods** (`areev-cal/src/facade.rs:56-307`), 24 with defaults, all typed DTOs (no JSON strings on the trait — that convention is binding-layer only) |
| `with_store` called ~107 times | **87 production call sites** outside areev-cal (py 26, js 23, areev-loop 17, server 10, mcp 8, cli 3) + 8 in areev-cal tests. The 107 double-counted internal/doc uses. |
| — (not known) | The CLI is worse than its 3 `with_store` sites: **31 direct `m.*` calls** bypass the facade entirely (`areev-cli/src/main.rs:571-587` opens a raw `Areev`; only 5 verb arms build a facade). The CLI needs its own routing pass. |
| — | **Prior art is richer than expected**: the ancestor codebase shipped Pg as a `pg-store` cargo **feature** with recall as **one RRF CTE** in `engine/recall_pg.rs` (per-leg top-k `$3`, final `LIMIT $6`), plus a recursive-SQL graph module. `RecallParams.hybrid: Option<HybridParams>` (`store_types.rs:1059`) is **already threaded through CAL and ignored by the Turso facade** — a pre-wired tuning channel `PgStore::recall` can consume with zero parser/executor changes. |
| — | Key facts that make this tractable: SQL confined to 2 files; 62 `block_on` sites of only 6 shapes; row decoding 100% uniform through 3 helpers (zero typed getters); **no error-string matching anywhere** (everything funnels to `STO-E001` via `db_err`); `turso::Statement` owns its connection (no lifetimes); transactions are string `BEGIN/COMMIT`, all within one call. |

---

## 1. Architecture

> **Build-time decision (2026-08-07, during Phase 0.1).** The backend plugs in
> at the `Db` seam **inside `Areev`** — `Areev { db: Box<dyn Db> }` runs the
> identical store logic (index maintenance, fork election, BM25) over either a
> `TursoDb` or a `PgDb` transport. `PgStore` as a *separate* `CalStoreFacade`
> implementation is dropped: it would duplicate the write path, which the
> audits flagged as the #1 parity risk. Consequences:
> - Fork/head/oplog semantics are in parity **by construction** — the same
>   Rust code executes on both backends; only SQL dialect + transport differ.
> - Every existing surface (facade, `with_store` call sites, MemoryTool,
>   migrate, bindings, CLI) works unchanged on a Postgres-backed `Areev`.
>   **Phase 1 (trait promotion) is therefore OFF the Postgres critical path**
>   — it remains worthwhile API hygiene, deferred to post-pilot.
> - The conformance suite is store-level: cases run over a `Backend` opener
>   yielding `Areev` handles (tempdir file vs schema), not over facade
>   objects.
> - The ~30 genuinely divergent statements (upserts, vector fns, IN-lists,
>   PRAGMA) go through a small dialect surface in the `Db` backend; `?N`→`$N`
>   is a mechanical rewrite inside `PgDb::prepare`.

### 1.1 Crate & feature layout

- `crates/areev-store`:
  - new module `src/db.rs` — the backend trait (`Db`, `Value`, `Row`, `Stmt`) + `TursoDb` impl. Internal (`pub(crate)`), not public API.
  - new module `src/pg/` behind `feature = "postgres"` (non-default; the crate gets its first `[features]` section) — `PgStore` + DDL + dialect. Dependency: `tokio-postgres` (fits the existing private-runtime pattern; connect via Unix socket for Cloud SQL, so no TLS stack initially).
  - `[package.metadata.docs.rs] all-features = true` already exists → docs.rs will build the feature the day it lands; CI must lint it (see §7).
- `crates/areev-cal`: trait promotion (§4); a `PgFacade`-shaped impl is NOT needed —
  `PgStore` implements `CalStoreFacade` directly (mirroring `AreevFacade`), or
  `AreevFacade` is generalized to wrap any store. Decide at build time of Phase 2;
  default assumption: **`PgStore` gets its own facade impl** to avoid disturbing
  the Turso path.
- new `crates/areev-conformance` (`publish = false`, dev-only): the
  backend-parameterized conformance suite (§8).
- Backend dispatch: one shared `open_backend(dsn_or_path, opts) -> Box<dyn CalStoreFacade>`
  helper (in areev-cal) so `areev`, py, and js don't each grow a URL parser.
  `postgres://` / `postgresql://` prefix → PgStore; else file path → Turso.

### 1.2 One memory = one schema

`PgStore::connect(dsn, PgOptions)` with:

```rust
PgOptions {
    schema: String,                    // one memory = one Postgres schema
    create_schema: bool,               // DDL-on-open vs attach-only
    entity_relations: HashSet<String>, // mirrors AreevOptions
    index_text: bool,
    telemetry: TelemetryMode,
    advisory_lock: bool,               // stage-1 single-writer enforcement
    statement_timeout_ms: Option<u32>,
}
```

- Erasure = `DROP SCHEMA … CASCADE`; portability = `pg_dump -n <schema>` and
  `areev bundle` (oplog is a table, so bundle export works from any backend).
- Mounts = a second `PgStore` handle on another schema, ideally under a
  read-only role. Read-only-by-construction becomes read-only-by-role — the
  trait grows `capabilities()` (§4, X-8) and `mount()` asserts it.
- Tenancy for Atmatic: schema resolution composes with their existing
  `resolveOrgSchema(oid)`; a single table set with a tenant column is
  explicitly ruled out.

### 1.3 Concurrency stages

- **Stage 1 (this plan):** single writer per memory, *enforced* —
  `pg_advisory_lock(hashtext(schema))` at open; contention → a **new
  `STO-Ennn` error** (reserve the code in `ERROR_CODES.md` before build;
  append-only). RAM counters (`next_seq`/`next_op`/`hlc_last`/dict/BM25 stats)
  keep working unchanged under the lock. Consider back-porting the same
  advisory-lock idea to Turso as a file lock — today two processes on one file
  are undefined behavior, not an error.
- **Stage 2 (Phase 4):** sequences + `RETURNING` for seq/op/term allocation,
  dict as write-through cache over `ON CONFLICT`, BM25 `N`/`avgdl` computed
  per query from `fts_doc` aggregates, HLC allocation behind the lock or a row.
  Gated on pilot feedback; do not start until stage 1 is in production use.

---

## 2. Phase 0 — no-regret refactors (land on main first, independently valuable)

### 0.1 Exec-wrapper `Db` trait (~1 week)

Design (validated against all 69 `block_on` sites):

```rust
pub(crate) enum Value { Null, Integer(i64), Real(f64), Text(String), Blob(Vec<u8>) }  // ≡ turso::Value
pub(crate) struct Row(Vec<Value>);      // strict i64()/blob()/f64()/text() accessors
pub(crate) trait Db: Send {
    fn execute(&self, sql: &str, params: &[Value]) -> Result<u64>;
    fn query(&self, sql: &str, params: &[Value]) -> Result<Vec<Row>>;   // materialized; safe — every site collects anyway
    fn prepare(&self, sql: &'static str) -> Result<Stmt>;               // backend-keyed cache; format!-SQL stays uncached
    fn exec_prepared(&self, st: &Stmt, params: &[Value]) -> Result<u64>;
    fn query_prepared(&self, st: &Stmt, params: &[Value]) -> Result<Vec<Row>>;
    fn txn(&self, f: &mut dyn FnMut(&dyn Db) -> Result<()>) -> Result<()>; // string BEGIN/COMMIT; closure gets the same object
    fn dialect(&self) -> &dyn Dialect;  // upsert forms, vector fns, integrity check, IN-list style
}
```

Decisions locked by the audit:
- `&self` + interior mutability (`RefCell<HashMap<String, Statement>>`) — kills
  the three-way split-borrow dance; `turso::Statement` is `Clone` and owns its
  connection, so no lifetime plumbing.
- `txn` hands the closure the **same** `&dyn Db` (turso: `BEGIN` is just a
  statement; tokio-postgres: use string `BEGIN`/`COMMIT` on the `Client`, NOT
  `Transaction<'a>` — it borrows the client and breaks the object identity).
- `Vec<Row>` materialization everywhere (the only mid-iteration early-exits are
  on `LIMIT 64` queries).
- Param passing: tuple → `&[Value]` slice rewrite at ~165 call sites, mechanical.
- `insert_prepped(conn: &Connection, …)` → `insert_prepped(db: &dyn Db, …)`,
  loses `async`; its 8 per-call prepares become cache hits (a small write-path win).
- `Areev` fields `rt`/`_db`/`conn`/7 `st_*` slots collapse to `db: Box<dyn Db>`;
  `ensure_stmt` and all `block_on` blocks deleted.
- `AsyncAreev` unchanged (the Drop-on-thread trick stays; it's a consequence of
  the private runtime existing, which continues for both backends).
- Telemetry sidecar: `Telemetry` takes its own `Box<dyn Db>` (today it borrows
  `Areev`'s runtime and opens `<path>.telemetry.db` — that runtime-sharing seam
  must be cut here). For Pg, telemetry tables live in the same schema.

Two latent bugs get fixed by construction: `rebuild_link_indexes` has **no
ROLLBACK arm** (`lib.rs:2162-2214`), and `apply_supersede_flip` runs **11
statements in autocommit** (`lib.rs:5004`) — `txn()` wraps both.

Acceptance: `cargo test --workspace` green, `bench` + `voice_loop` gates pass
(the wrapper must not cost the hot path — prepared-statement cache keeps the
probe/fetch statements warm exactly as today).

### 0.2 Conformance crate, Turso-only (~1 week, parallelizable with 0.1)

Build `crates/areev-conformance` **now**, before the trait promotion, so the
promotion lands against a regression net:

- `trait Backend { fn open(&self) -> (Box<dyn CalStoreFacade>, Guard); fn open_pair(...); }`
- Cases as `pub fn case_x(b: &dyn Backend)`; two thin test files emit per-case
  `#[test]`s via a 10-line macro. Turso backend only at first.
- Seed order: `bugfix_regression_tests.rs` (all 11 — fork/head/oplog/2-hop
  invariants; the suite's spine), `fork_merge_tests.rs` (3 — carry the fixed
  `created_at` tiebreak pattern verbatim), then the areev-testing skill's
  combination checklist (supersede×forget, fork×forget×merge,
  add_if_novel×supersession, import idempotency, 2-hop oplog completeness).
- Also parameterize the `setup()` helpers in the four areev-cal test files
  (`cal_integration.rs:11`, `state_workflow_integration.rs:17`,
  `recall_tuning_cal_tests.rs:11`, `assemble_mount_tests.rs`) to return
  `Box<dyn CalStoreFacade>` from an injectable opener — **74 existing
  deterministic tests become backend-generic for ~a day of work.**
- Fold the two duplicate `MockStore`s (facade.rs:428, executor.rs:6256) into one
  shared test-util mock while touching this area.

Determinism rules carried over: schema-per-test = the tempdir rule's analogue;
pinned `created_at` for all election assertions; assert on sorted/keyed output
(Turso and Pg will not agree on unordered scan order).

### 0.3 Upstream fidelity fixes (~2–3 days)

Nine hazards found in the semantics audit; fix the behavior-defining ones on
main **before** Pg parity is measured against them:

1. **Head re-election is plan-ambiguous** (`supersede`:2728, `forget`:2875):
   `heads JOIN triples` ordered only by `(created_at, hash)` — a link-bearing
   grain has multiple triple rows per seq, so the elected `o` is
   engine-dependent. Add a deterministic tiebreak (`t.o DESC`) on main first.
2. `apply_supersede_flip` deletes heads unqualified (`WHERE seq=?` only,
   lib.rs:5125) — qualify like everywhere else.
3. `recall_hybrid`'s final sort uses `partial_cmp().unwrap()` (3870) — switch to
   `unwrap_or(Equal)` like 3516.
4. Document-and-keep (conformance-pin, don't change): import writes **no** BM25
   postings; `entity_latest` local-overwrite vs import-election asymmetry;
   `recent_live` uses `superseded_by IS NULL` while others use `svt IS NULL`
   (four liveness spellings stay distinct); `term_id` writes outside the txn;
   imported `OP_FORGET` mints a fresh HLC.

### 0.4 Batch the N+1 hot loops (~1 week)

Behind the wrapper, rewrite as set-based (goldens must show identical output
order — this is a ranking-parity risk, test first):

- `recall` / `thread_tail` / hybrid's blob fetch: join blobs into the probe or
  one `= ANY($1)` fetch (17 → 1–2 round trips).
- `search_text_inner`: one query for all tokens (`v.term = ANY($1)`), group in
  Rust; BM25 math stays in Rust untouched.
- `supersession_map` / the loop's `get`-per-op scan: `get_many` batch.
- `related` / `path` / `entity_at(Knowledge)` / `history`: acceptable per-level
  batching now; recursive CTEs are the Pg-side design (prior art: the ancestor's
  "recursive-SQL graph module"), can be Pg-only.

On Turso this is a minor win; on Pg it is the difference between 17ms and ~1ms.

---

## 3. Phase 1 — trait promotion (close the escape hatches) (~2–3 weeks)

### 3.1 The delta: 31 → 59 trait methods, ~14 new DTOs

Full per-site absorption table lives in the audit; summary of the 28 additions:

| Group | Methods |
|---|---|
| Read (5) | `entity_latest`, `recent(ns, type, limit, live_only)`, `provenance_children`, `nearest_semantic`, `get_many` (default = loop over `get`; de-N+1s areev-loop/server/facade labelling) |
| Write (5) | `cal_add_if_novel` + `cal_add_batch` (promote existing inherent methods), `capture(CaptureMeta)`, `attach_facts(FactDraftDto, FactAttributionDto)`, **`merge_heads`** (must be on-trait: fork collapse is the #1 parity risk and must be conformance-testable) |
| Graph/time (6) | `related(TraversalDir)`, `entity_at(TimeAxis)`, `step_actions`, `run_trace`, `run_yield`, `runs_touching` (kept separate: hosts conditionally skip the yield leg) |
| Oplog/sync (5) | `changes_since(OpRecordDto)`, `bundle_since`, `import_bundle(path, max_hlc)` (merges `_until`), `verify(VerifyReportDto)`, `stats(StoreStatsDto)` |
| Meta/config (5) | `store_info()` → one DTO folding `index_text_enabled`/`embedder_dim`/`declared_embedding`/`indexed_documents`/`open_warnings` (absorbs 12 calls across 8 sites), `rebuild_text_index`, `rebuild_link_indexes`, promote `meta_warnings` + `mount_aliases` |
| Telemetry (1) | `telemetry_view(ns) -> Option<TelemetryViewDto>` (None = sidecar off; shape ≈ `areev_loop::TelemetryView` so the adapter is a one-line map) |
| Admin (3) | `migrate_payload(MigrateReportDto)`, `subjects_with_relation` (for the MemoryTool refactor), `capabilities() -> BackendCapabilities {crypto_erasure, file_backed, read_only, multi_writer}` |

All new methods: `&self`, DTO-only, defaults (`Err(Internal)`/empty) so mocks
keep compiling. `VerifyReportDto.integrity` contract: "`ok` or backend-specific
diagnostic"; the portable, conformance-tested half is `hash_mismatches`/`undecodable`.

### 3.2 Special cases (the X-list) — resolutions

| Case | Resolution |
|---|---|
| `set_embedder`/`set_reranker` (trait objects, `&mut`) | Move `EmbedBackend`/`RerankBackend` traits to `areev-core`; add `fn set_embedder(&self, Box<dyn EmbedBackend>)` on the facade trait (interior mutability — the Mutex is already there). On Pg, first install creates the `vector(dim)` table + stamps meta; later mismatch = hard `VAL` error (an upgrade over Turso's warning). |
| Bundle file paths | Add `bundle_since_bytes` / `import_bundle_bytes` variants; path variants default-delegate. Also fixes the server's write-body-to-disk-then-import dance and its silent 1 MiB truncation. |
| areev-loop `store_state` (read-then-write in one lock) | Dedicated atomic method `put_state(ns, subject, relation, json) -> Hash` — splitting it would introduce a lost-update race. **Highest-priority hazard of the promotion.** |
| areev-loop `all_grains` (changes_since + get per op, MAX_SCAN=1M) | `get_many`, or better `grains_of_type(gt, ns, live_only)` pushed into the backend. Worst Pg path in the codebase if left N+1. |
| `MemoryTool` (borrows `&mut Areev`) | Refactor to take `&dyn CalStoreFacade` (it uses exactly 6 primitives, 5 already on the trait + `subjects_with_relation`). No `memory_tool` trait method needed. |
| `migrate_payload` (free fn over `&mut Areev`) | Pragmatic first cut: opaque per-backend trait method (G2). Genericize the ~500-line module later. |
| Typed-grain writes (areev-loop `add(&Fact)`, CLI seeds) | Convert to `cal_add` field maps; **verify `created_at` survives the JSON round trip** (the loop stamps it deliberately for deterministic content addressing — the loop goldens will catch drift). |
| `into_inner() -> Areev` | Stays inherent on `AreevFacade` only; delete `AreevSubstrate::into_store`. |
| `mount(alias, Areev)` | Becomes `mount(alias, Box<dyn CalStoreFacade>)`; assert `capabilities().read_only` intent at mount time. |
| Host constructors (`Server::new(AreevFacade)` etc.) | Become `Box<dyn CalStoreFacade>`/`Arc<dyn …>` — **breaking API change for areev-server and areev-mcp** → minor version bump, changelog entry. |
| `SearchHit` cfg'd fields (`rerank`/`llm-rerank` features) | Add `SearchHit::new(grain)` constructor so no impl enumerates 13 fields under two cfgs. |
| `cal_forget_user`/`cal_forget_scope` (stubs on both sides) | **Do not implement as a side effect.** They stay stubs until the Phase 4 erasure design + OMS decision. |
| py `recall`/`search`, js `recall` bypass `recall(&RecallParams)` | Rewrite through the trait, but golden-test binding output first — the facade path adds overfetch/multi-hop/clamping the raw calls don't have. |

### 3.3 Host migration order

1. **loop adapter** (cleanest; `crates/areev-loop` is already substrate-agnostic).
2. **server + mcp** (10 + 8 sites, mostly absorbed by `store_info`/`telemetry_view`/`stats`/graph methods).
3. **py** (26 sites) — run pytest.
4. **js** (23 sites) — outside the workspace: manual `napi build --release` +
   `node --test __test__/smoke.mjs` after every change batch. Note the two
   bindings are already out of lockstep (js lacks `set_embedder` callback,
   `add_batch`, `index_text` open knob) — don't "preserve parity" blindly; log
   gaps, fix separately.
5. **CLI** — the routing pass: replace the raw `Areev` open at `main.rs:571`
   with `open_backend(...) -> Box<dyn CalStoreFacade>` and convert the 31 direct
   `m.*` verb arms to trait calls. Budget this as its own task (~1 week); the
   golden suite (40 tests) is the safety net.

Exit criterion: `grep -rn "with_store" crates/ --include="*.rs" | grep -v areev_facade.rs | grep -v tests` → zero production hits; conformance suite green on Turso.

---

## 4. Phase 2 — `PgStore` stage 1 (~3–4 weeks)

### 4.1 DDL (schema-per-memory; all integers `bigint` — never `int4`, see coercion note)

```sql
CREATE TABLE meta(k text PRIMARY KEY, v text);
CREATE TABLE terms(id bigint PRIMARY KEY, term text UNIQUE NOT NULL);       -- app-assigned id
CREATE TABLE grains(
  seq bigint PRIMARY KEY,            -- app-assigned from next_seq
  hash bytea NOT NULL,
  ns bigint NOT NULL, gtype bigint NOT NULL, created_at bigint NOT NULL,
  s bigint, p bigint, o bigint,      -- NULL when grain lacks full s/p/o
  vf bigint, vt bigint,              -- world-time validity, NULL = unbounded
  svf bigint NOT NULL,               -- always = created_at
  svt bigint,                        -- NULL = live (the liveness predicate)
  superseded_by bytea, supersedes bytea,
  text text,
  blob bytea NOT NULL
);
CREATE UNIQUE INDEX idx_grains_hash ON grains(hash);
-- embeddings table created lazily at first set_embedder (dim is runtime-supplied):
--   CREATE TABLE embeddings(seq bigint PRIMARY KEY, vec vector(<dim>));
CREATE TABLE fts_vocab(id bigint GENERATED BY DEFAULT AS IDENTITY PRIMARY KEY,
                       term text UNIQUE NOT NULL);
CREATE TABLE fts_post(term bigint, seq bigint, ns bigint, tf bigint);        -- no PK: duplicates are reachable state
CREATE INDEX idx_fts_post ON fts_post(term, ns);
CREATE INDEX idx_fts_post_seq ON fts_post(seq);
CREATE TABLE fts_doc(seq bigint PRIMARY KEY, len bigint);
CREATE TABLE triples(ns bigint, s bigint, p bigint, o bigint, seq bigint, cur bigint);
CREATE INDEX idx_spo ON triples(ns,s,p,o,seq);
CREATE INDEX idx_pos ON triples(ns,p,o,s,seq);
CREATE INDEX idx_triples_seq ON triples(seq);
CREATE TABLE osp(ns bigint, o bigint, s bigint, p bigint, seq bigint, cur bigint);
CREATE INDEX idx_osp ON osp(ns,o,s);
CREATE INDEX idx_osp_seq ON osp(seq);
CREATE TABLE entity_latest(ns bigint, s bigint, p bigint, o bigint,
                           seq bigint, hash bytea, PRIMARY KEY(ns,s,p));
CREATE TABLE heads(ns bigint, s bigint, p bigint, seq bigint, hash bytea,
                   created_at bigint, PRIMARY KEY(ns,s,p,seq));
CREATE TABLE oplog(op_seq bigint PRIMARY KEY, hlc bigint, op bigint, hash bytea);
CREATE TABLE thread_idx(ns bigint, session bigint, seq bigint);
CREATE INDEX idx_thread ON thread_idx(ns, session, seq);
CREATE TABLE prov_idx(ns bigint, parent bytea, seq bigint);                  -- parent = raw hash, not a term
CREATE INDEX idx_prov ON prov_idx(ns, parent, seq);
CREATE TABLE run_idx(ns bigint, run bigint, seq bigint);
CREATE INDEX idx_run ON run_idx(ns, run, seq);
-- telemetry (same schema, when mode != Off): recall_log(id IDENTITY), grain_access,
-- query_stat, budget_stat — per telemetry.rs DDL, TEXT hashes as today.
-- CAS blobs: blobs(hash bytea PRIMARY KEY, body bytea)  -- replaces the .blobs sidecar
```

Notes: `bytea` compares bytewise-then-length = SQLite BLOB compare = Rust slice
`Ord`, so every `(created_at, hash)` election tiebreak carries over unchanged.
Do NOT add unique constraints to `triples`/`osp`/`fts_post` — delete-by-tuple
rebuilds and duplicate postings are reachable, semantics-bearing state.

### 4.2 Dialect mapping (from the audit's checklist — every item is enumerated there)

| SQLite/Turso construct | Pg form |
|---|---|
| `?N` params | `$N` (one-for-one; numbered reuse is legal in both) |
| `INSERT OR REPLACE` ×14 | `ON CONFLICT (<pk>) DO UPDATE`. **Trap:** the telemetry upserts read the old row in correlated subqueries (evaluated before SQLite's DELETE) — translate to `SET recall_count = grain_access.recall_count + 1`, not a re-SELECT. |
| `INSERT OR IGNORE` (fts_vocab) | `ON CONFLICT (term) DO NOTHING` + re-SELECT (NOT `DO UPDATE…RETURNING` — burns identity values Turso doesn't) |
| rowid auto PKs (fts_vocab.id, recall_log.id) | `GENERATED BY DEFAULT AS IDENTITY` |
| app-assigned PKs (grains.seq, terms.id, oplog.op_seq) | plain `bigint PRIMARY KEY` — RAM counters own allocation in stage 1 |
| `vector32(json)` / `vector_distance_cos` (7 sites) | `$n::vector` from the same JSON-array text / `<=>` (both are cosine *distance*; keep `ORDER BY dist ASC` and the `1.0 - d` conversions exactly) |
| inline `IN (csv)` (3 sites) | `= ANY($1::bigint[])` |
| `PRAGMA integrity_check` | drop; `verify()` keeps the portable half (full-scan re-hash) |
| `LIKE ?1 ESCAPE '\'` (meta_scan) | keep as-is — the Rust `strip_prefix` backstop already makes case behavior identical (verified; no change needed) |
| `format!`-interpolated fragments (recent_inner, nearest_semantic, telemetry) | static variants or parameterized |

### 4.3 Semantics that must be reproduced bit-for-bit (conformance-pinned)

1. **Four liveness spellings stay distinct per operation**: `cur=1`
   (structural), `svt IS NULL` (vector/world-time), post-filtered `live_seqs`
   (BM25), `superseded_by IS NULL` (`recent_live`).
2. **Election rule everywhere**: `ORDER BY created_at DESC, hash DESC`;
   `heads()[0]` is the provisional head; import elects `entity_latest` on
   `(created_at, hash) >` while local add overwrites unconditionally.
3. **Write-path divergence local vs import**: local add collapses heads
   (DELETE+INSERT); import UNIONs (`ON CONFLICT` upsert into heads) — this
   asymmetry *creates* forks and must not be "fixed".
4. **Import writes no FTS postings** and doesn't touch BM25 counters.
5. **Supersede double-log**: OP_ADD then OP_SUPERSEDE, `op_seq_sup = op_seq_add+1`,
   both naming the new hash; the 2-hop re-log condition (`changed && !inserted`).
6. **Ranking math in Rust, unchanged**: BM25 K1=1.2/B=0.75 with N/avgdl from
   counters (counting superseded docs); RRF `1/(60+rank)`; tiebreaks
   `score DESC, seq DESC` (fusion/BM25) vs `score DESC, seq ASC` (rerank).
7. **Tokenizer byte-identical** (lowercase, non-alphanumeric split, ≤64 chars).
8. **Fail-open contracts**: BM25/vector/expansion legs swallow errors
   (`unwrap_or_default`); pin with a test that a pgvector dim-mismatch errors
   (it does: "different vector dimensions") and degrades the leg, not the call.
9. **HLC**: `now_ms() << 16`, `+1` on collision, seeded from `MAX(hlc)`;
   import preserves incoming HLC except `OP_FORGET` (fresh local — keep).
10. **Term interning outside the caller's txn** (orphan rows on rollback are
    canonical behavior).

### 4.4 Recall shape

Stage-1 recall uses the Phase 0 batched statements (probe+blob join → 1–2 round
trips). Then adopt the ancestor's **single RRF CTE** for `recall_hybrid` on Pg
(BM25 leg + vector leg + fusion in one statement), reading tunables from the
already-threaded `RecallParams.hybrid`. Drop `HybridParamsRange`/tier clamping
(dead weight implying a tier system that doesn't exist). Graph/time walks
(`related`, `path`, `entity_at(Knowledge)`, `history`) become recursive CTEs on
Pg; per-level batching is the fallback if parity proves fiddly.

### 4.5 Sidecars, encryption, erasure

- **CAS blobs** → `blobs(hash bytea PK, body bytea)` table (same put/get/gc
  semantics; gc scans `grains.blob` for `content_refs` exactly as today).
- **`.kdf`/page-cipher**: Turso-only; never on the trait. `capabilities()`
  reports `crypto_erasure: false` for Pg; hosts refuse `--passphrase-env`
  against a DSN with a clear error.
- **Telemetry**: tables in-schema; hot-path capture (in-memory push) untouched.
- **Erasure**: stage 1 ships only what exists today (`FORGET` single-grain
  tombstone → hard DELETE of index rows + grain row, as on Turso — actually
  *stronger* on Pg since there's no WAL-remnant story; plus `DROP SCHEMA` as
  memory-level erase, CLI-gated). Subject-scoped erasure (`cal_forget_user`)
  stays unimplemented pending the Phase 4 OMS decision.

### 4.6 Open/config parity

Open sequence mirrors Turso's: create-or-attach schema → advisory lock →
meta read + reconcile → warnings (`store_info().open_warnings`; new warning
kinds: stamp divergence, lock contention, missing pgvector extension) →
re-stamp `text_index`/`entity_relations` → counter seeding (7 scalar queries) →
dict slurp. `CREATE EXTENSION IF NOT EXISTS vector` in bootstrap (works with
Atmatic's `ext`-schema relocation since vector types resolve via search_path).

---

## 5. Phase 3 — surfaces (~1 week)

- `open_backend` dispatch in CLI (`areev --db postgres://…?schema=…`), py
  (`Areev("postgres://…")`), js (same). Scalars-in/JSON-out convention
  unchanged — the bindings talk to the trait, so most methods light up for free
  after Phase 1.
- `areev hub` on a Pg store: segment POSTs apply via the same idempotent import;
  any number of app instances can accept pushes (the hub singleton constraint
  dissolves for Pg-backed deployments). Edge↔cloud keeps the MGB1 wire format.
- Docs pass: scope "no server in the recall path" / "microseconds" / "one
  memory = one file" claims to the embedded backend (README, ARCHITECTURE §1/§3,
  FAQ, crate descriptions); rewrite ARCHITECTURE §11 with the real two-tier
  topology; SECURITY.md gets the per-backend erasure/encryption matrix.

---

## 6. Phase 4 — stage 2 (multi-writer) + gated erasure (~3–4 weeks, gated on pilot)

- Sequences/`RETURNING` for seq/op/term; dict → write-through cache over
  `ON CONFLICT`; per-query BM25 stats; row-locked head election
  (`SELECT … FOR UPDATE` on `heads` keys) preserving the fork model under
  `READ COMMITTED`.
- Beware: a shared seq sequence under concurrency changes `ORDER BY seq DESC`
  recall ordering between interleaved writers — acceptable, but document it and
  keep conformance assertions on sets, not order, where writers interleave.
- Subject-scoped hard delete (`cal_forget_user`/`cal_forget_scope` first real
  implementation): gated by `allow_destructive_ops` AND a txn-local GUC
  (mirroring Atmatic's `app.allow_audit_purge` pattern), emitting an audit
  grain. **Requires the OMS-conformance design decision first (CLAUDE.md
  invariant #3).** `ErasureProof` DTO already exists on the trait.
- Pg perf gates + RESULTS.md section: batched recall p50 < 2ms same-VPC,
  add p50 < 5ms; a `pg_bench` binary with hard exit codes, run in the Pg CI job.

---

## 7. Test & CI plan

- **Conformance suite** (from Phase 0.2) gains `conformance_pg.rs`:
  `#![cfg(feature = "postgres")]`, `DATABASE_URL` soft-skip locally (repo's
  existing skip idiom, actionable message), **hard-fail when `CI=true` and
  `DATABASE_URL` unset** — a broken job must not look like a skipped one.
- **Schema-per-test isolation**: `conf_<pid>_<counter>` schema names (no
  clock/rand — determinism rules), `DROP SCHEMA … CASCADE` on guard Drop, plus
  a startup sweep for leaked `conf_%` schemas. 63-byte identifier cap.
- **New CI job `postgres`** (the 7 existing jobs stay byte-identical):
  `pgvector/pgvector:pg16` service container with a mandatory health check;
  runs the Pg conformance suite, the Turso conformance suite (same code path,
  parity in one log), and `cargo clippy -p areev-store --features postgres
  --all-targets -- -D warnings` (the workspace clippy job never lints the
  feature). Decide explicitly: add `--features postgres` to the `msrv` job or
  accept the gap; docs.rs already builds all-features, so a feature compile
  break would break published docs silently — the clippy step covers it.
- **Golden leverage**: once `areev --db postgres://` exists, point the CLI golden
  suite's `import_golden()` at a schema URL — 40 exact-output assertions
  (including `golden_manifest_hashes_stable`: content addresses are computed in
  areev-core before SQL, so manifests must be byte-identical on Pg) become a
  free cross-surface conformance layer. Areev Loop goldens deferred to after the
  substrate migration settles.
- **Multichannel test** gets a Pg variant at Phase 3/4 — the natural end-to-end
  fork-parity proof.
- **js gate**: every Phase 1/3 batch touching areev-js ends with
  `napi build --release` + `node --test` by hand (not covered by
  `cargo test --workspace`).
- Perf regression guard for Turso: run `bench` + `voice_loop` after Phase 0.1
  and 0.4 — the wrapper and batching must not move the µs numbers.

---

## 8. Sequencing & effort

| Phase | Work | Size | Gate to next |
|---|---|---|---|
| 0.1 | `Db` wrapper + TursoDb | ~1 wk | workspace green + perf gates unchanged |
| 0.2 | conformance crate (Turso-only) + CAL setup() parameterization | ~1 wk (overlaps 0.1) | suite green, spine cases ported |
| 0.3 | fidelity fixes | 2–3 d | — |
| 0.4 | N+1 batching | ~1 wk | goldens identical, perf gates unchanged |
| 1 | trait promotion (28 methods, 14 DTOs) + host migrations incl. CLI routing pass | ~3 wk | zero production `with_store`; conformance green; py/js/golden suites green |
| 2 | PgStore stage 1 | 3–4 wk | Pg conformance green in CI; CLI goldens green on Pg |
| 3 | surfaces + docs + CI polish | ~1 wk | pilot-ready |
| 4 | stage 2 + gated erasure (after pilot feedback + OMS decision) | 3–4 wk | — |

**Pilot-ready (Atmatic, single-writer, Node binding on Cloud SQL): ~8–10
weeks.** Stage-2 quality: ~3–3.5 months. Phases 0.1–0.4 are valuable even if
Postgres later stalls.

Risk register (top 5):
1. Fork/head parity across two write paths → mitigated by conformance spine +
   the Phase 0.3 tiebreak fix + goldens.
2. Silent js breakage (outside workspace) → manual gate per batch; consider a
   CI job for js against a local Pg later.
3. CLI routing pass scope creep (31 direct call sites, some verbs are
   file-only by nature — `stream`/`follow`/`restore` stay path-based) →
   classify verbs up front: portable vs file-backend-only (error cleanly on a
   DSN).
4. Coercion wrong-answers (int4/numeric → `v_i64` = None → `unwrap_or(0)`)
   → all-bigint DDL + make strict accessors error, not None, in the Pg impl.
5. Behavior drift via `cal_add` conversion of typed writes (areev-loop
   `created_at` stamping) → loop goldens + explicit round-trip test.

---

## 9. Open decisions (unchanged from the proposal, now sharpened)

1. **Destructive-surface widening** (subject-scoped hard delete + schema drop):
   needs the OMS-conformance design before Phase 4. Stage 1 ships without it.
2. **Claim scoping docs pass** — committed as part of Phase 3, not optional.
3. **Feature flag vs separate crate**: feature flag (`areev-store/postgres`),
   matching the ancestor's `pg-store` precedent. Revisit only if the dependency
   tree bloats the default build.
4. **Stage 2 timing**: after the Atmatic pilot proves demand for multi-writer.
5. **New**: whether to also take the advisory-lock idea back to Turso as a file
   lock (fixes an existing UB hole) — recommended, small, independent.
