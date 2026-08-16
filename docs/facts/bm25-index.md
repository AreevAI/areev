# Fact sheet — the BM25 index, and how to delete it

*Written 2026-08-04 against `main`. This layer exists to work around an upstream
defect. **It is meant to be removed**, and this sheet is the removal
instructions. If you are reading it because the upstream issue closed, skip to
[§4](#4-how-to-remove-it).*

## 1. Why we do not use Turso's `USING fts`

Areev's BM25 leg used to be `CREATE INDEX idx_fts ON grains USING fts (text)`
— Turso's experimental, tantivy-backed full-text index. Measured on an M4 Max,
`--release`, single writer:

| rows | `INSERT` (one row) | `MATCH`, 1 row matches | `MATCH`, every row matches |
|---|---|---|---|
| 500 | 1.6 ms | 2.5 ms | 2.8 ms |
| 3,000 | 57.9 ms | 18.8 ms | 20.1 ms |

Three things are wrong there, and only the first is about speed:

1. **`INSERT` cost grows with the table** — 6x the rows, ~36x the per-insert
   cost. Batching does not amortize it: identical per-row cost at batch sizes
   1, 50 and 500.
2. **`MATCH` cost is independent of how many rows match.** A term in exactly
   one row costs the same as a term in all 3,000. Cost tracking table size
   rather than result size is the definition of an index not working.
3. **A version bump does not fix it.** `turso` is pinned `=0.7.1` (deliberately
   — encryption-at-rest rides its experimental AES-256-GCM), and `0.8.0-pre.2`
   measures identically.

Reproduced in ~50 lines of bare `turso` with no Areev code involved. Filed
upstream: **[tursodatabase/turso#8170](https://github.com/tursodatabase/turso/issues/8170)**
(the write half was reported independently the same day; we added the read
half). Control arm: the same inserts with no index are flat at ~0.0 ms.

This matters disproportionately for agent memory, which is the workload that
writes one small grain and runs one free-text query *per turn*, against a file
that grows for months. That is precisely the shape that cannot absorb a
per-write cost proportional to accumulated history.

## 2. What replaced it

A plain inverted index, entirely inside `crates/areev-store/src/lib.rs`:

| piece | where |
|---|---|
| `fts_vocab`, `fts_post`, `fts_doc` tables | the `SCHEMA` const |
| `tokenize`, `token_freqs`, `BM25_K1`/`BM25_B` | just above `projected_text` |
| vocabulary id assignment | `Areev::fts_term_id` |
| posting writes | `insert_prepped` (+ `fts_delta` for the collection stats) |
| posting deletes | `Areev::forget` |
| scoring | `Areev::search_text` (+ `live_seqs`) |
| bulk-load path | `defer_text_index` / `rebuild_text_index` |
| legacy-file self-heal | end of `open_internal` |

Tokenizing is deliberately plain — lowercase, split on non-alphanumeric, drop
tokens over 64 chars. No stemming, no stopwords: both make results depend on a
language guess, and the same function runs at index and query time so whatever
it does, the two agree. Scoring is textbook BM25 (`k1=1.2`, `b=0.75`) with
Robertson/Sparck-Jones idf.

`N` and `avgdl` live in memory (`fts_docs`, `fts_total_len`), loaded once at
open and adjusted on write, because an aggregate over the postings on every
search would reintroduce the exact O(corpus) cost this replaced.

Measured after the change, same machine:

| grains | `add` | BM25, 1 doc matches | BM25, every doc matches |
|---|---|---|---|
| 500 | 0.6 ms | 0.3 ms | 0.6 ms |
| 4,000 | 0.4 ms | 0.4 ms | 4.1 ms |

Writes are flat. Rare-term lookup is flat. The *common*-term column growing is
correct and expected — that query genuinely matches every document, so cost
tracks matches. That is the behaviour the old index did not have.

## 3. What this is not

- **Not a search engine.** No phrase queries, no proximity, no boolean
  operators, no fuzzy matching. A query is a bag of terms.
- **Not language-aware.** "run" does not match "running".
- **Not a ranking guarantee.** Superseded grains keep their postings and are
  filtered after scoring (`search_text` over-fetches 4x before the liveness
  filter for this reason), and `N` counts them, so idf includes tombstoned
  documents. Immaterial to ranking; worth knowing before trusting a score
  numerically.

## 4. How to remove it

When #8170 is fixed and released, and a measurement on the current release
reproduces flat single-row `INSERT` **and** selectivity-dependent `MATCH`:

1. Bump `turso` in `crates/areev-store/Cargo.toml` (still pinned `=`, still
   for the encryption reason).
2. Restore `CREATE INDEX IF NOT EXISTS idx_fts ON grains USING fts (text)`
   behind `if opts.index_text` in `open_internal`, where the `DROP INDEX
   idx_fts` line is now.
3. Replace `search_text`'s body with the one-query version:
   `SELECT seq FROM grains WHERE text MATCH ?1 AND ns = ?2 AND svt IS NULL
   LIMIT ?3`. Delete `live_seqs`, `fts_delta`, `fts_term_id`, `tokenize`,
   `token_freqs`, the BM25 constants, and the `tokens`/`doc_len` fields on
   `GrainPrep`.
4. Delete the three tables from `SCHEMA` and the posting writes in
   `insert_prepped` / deletes in `forget`.
5. Restore `defer_text_index` to dropping the index and `rebuild_text_index`
   to re-creating it.
6. Keep the open-time self-heal, inverted: files written *during* this era have
   postings and no `idx_fts`.

**Do not remove it just because the issue is closed.** Re-run the repro in the
issue against the new release first — the numbers in §1 are the acceptance
test. Ranking will also shift (tantivy stems and ours does not), so
`crates/areev-cal` tests asserting on hit order deserve a re-read.

Everything above is confined to one file plus the CLI's `reindex` message,
which was the point of keeping it small.
