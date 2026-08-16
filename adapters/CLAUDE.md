# adapters/

Wave-3 Python ecosystem adapters (governed-agents §8). NOT cargo workspace
members — plain pip packages developed against a maturin-built `areev`
(`VIRTUAL_ENV=~/.venvs/areev-adapters maturin develop --release -m
crates/areev-py/Cargo.toml`, then `pip install -e adapters/<pkg>`). CI job
`adapters` runs both suites at the pinned upstream floor
(langgraph-checkpoint >=4.2,<5; crewai >=1.10,<2); `adapters-canary.yml`
runs them weekly against unpinned latest.

## areev-langgraph

- **`AreevCheckpointSaver`** — one thread = ONE memory file under `root`
  (their thread is our isolation unit; `delete_thread` deletes the file
  incl. `.telemetry.db` sidecars); process-wide LRU handle cache,
  close-on-evict. Checkpoints form a TREE keyed
  `(thread, checkpoint_ns, checkpoint_id)`: re-put of the same id
  SUPERSEDES, `list()` never chain-collapses. `put_writes` upsert:
  idx >= 0 first-commit-wins, idx < 0 (WRITES_IDX_MAP) supersedes.
  Channel blobs ride `bl:` facts per (channel, version) with an `empty`
  type sentinel. DeltaChannel history mirrors InMemorySaver's walk.
  `prune keep_latest` keeps the newest checkpoint AND its whole ancestor
  chain (severing it silently breaks DeltaChannel — the upstream warning).
  `get_next_version` is DETERMINISTIC (fixed suffix, 32-digit pad,
  lexicographic == numeric, property-tested) unlike upstream's random tail.
- **`AreevStore`** — items as Fact heads (`put` supersedes, `delete`
  tombstones, `created_at` denormalized forward); `batch` sequential =
  read-your-writes; `$gt`-style filters post-filter over a 1000-candidate
  pool (documented cap); text `query` ranks through hybrid `search`;
  `supports_ttl = False` (retention is declarative, never per-item timers).
- **`AreevTraceMirror`** — sync callback handler; app thread enqueues, ONE
  worker owns the handle. Modes: `best-effort` (drop-oldest + counter;
  observability) vs `guaranteed` (backpressure; the only mode compliance
  may cite). Loss-honesty is tested: stored + dropped == emitted.
- **`_codec`**: percent-encode with NO safe chars; empty component = `"%"`
  (bare `%` is un-emittable by quote — without it `("",)` and `()`
  collide, which the property test caught). Composite index strings are
  decoded only after stripping their marker.

## areev-crewai

- **`AreevStorageBackend`** — record → Fact at subject
  `<source>#crw:<id>` (partition key: `FORGET SUBJECT <source>` selects
  record + history + index rows; THE erasure demo) or `crw:<id>` when
  unsourced. Every read is heads-only (ConsolidationFlow rewrites =
  supersession chains). `search` takes CrewAI's PRE-COMPUTED embedding →
  `nearest_vector` (hash-joined against current heads, so stale vectors
  never resurface; dim mismatch raises VAL). Predicate deletes (incl.
  `reset` — scope IS a predicate) enumerate heads, tombstone each, and
  write one `audit:crewai` sweep summary. `private` filtered store-side.
  Deviation, stated: `last_accessed` is caller data, never write-on-read.
- **`AreevAuditListener`** — schema-agnostic: any event persists as a
  generic fact (kind + JSON-safe payload); wildcard bus registration in
  `setup_listeners`; same best-effort/guaranteed matrix.

## Gotchas learned building these

- Facts refuse empty objects (VAL-E001) — empty bytes encode as `"-"`.
- `nearest_vector` returns `{hash, similarity}` ONLY — join against
  enumerated heads for payloads.
- `forget_subject` receipts use `grains_erased`, not `erased`.
- areev files spawn `.telemetry.db` sidecars — exclude from directory
  enumeration, include in file erasure.
- Store hatchling in this venv lacks editable-build hooks; the packages
  use setuptools.
