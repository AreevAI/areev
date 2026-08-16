# areev-bench — adoption benchmark results (first pass)

*Run: 2026-07-06 · Apple M4 Max, macOS 26.5 (laptop — commodity-CI rerun
pending, same caveat as the earlier m0 substrate spike) · all harnesses in this crate,
`--release`, workspace LTO profile · dataset everywhere: 10k facts / 800
subjects (the `bench.rs` shape), identical query workloads per surface via
seeded xorshift.*

Four benchmarks (per the design doc): frame chart, trust suite, **honesty metrics**
(§3), and the **LoCoMo self-run** (§4) — retrieval hit-rate plus LLM-judged
end-to-end answer accuracy (bring-your-own reader/judge).

## 1. Frame chart — "recall inside an audio frame"

`cargo run --release -p areev-bench --bin frame_chart`
(needs `cargo build --release -p areev` first for the MCP leg)

One retrieval op — up to 16 most-recent facts about a caller — measured over
every surface a voice developer could actually deploy. Nothing simulated:
the HTTP and MCP legs drive the real `UiServer` and the real `areev serve
--mcp` binary over real sockets/pipes.

| surface | p50 µs | p95 µs | p99 µs | p99 as % of one 50ms frame |
|---|---|---|---|---|
| A in-process `recall` (voice hot path) | 33.1 | 46.6 | 60.2 | 0.12% |
| B localhost HTTP `/api/cal` (sidecar) | 158.1 | 216.3 | 263.5 | 0.53% |
| C MCP stdio `areev_recall` (agent host) | 128.6 | 181.5 | 205.0 | 0.41% |
| — network memory service (reference) | — | — | — | Zep's own enterprise headline: "retrieval under 200 ms" = **400%** of one frame (vendor-stated, not measured here) |

Readings: (a) every Areev surface fits inside 0.6% of a frame; the
category's stated floor is 4 frames. (b) Even *our own* localhost sidecar
costs ~5x the in-process path — transport, not storage, is the latency
budget, which is the whole architectural argument. (c) MCP stdio at ~129µs
p50 is the Claude Code / agent-host number.

## 2. Trust suite — durability + integrity artifacts

`cargo run --release -p areev-bench --bin trust_suite` (exit 0 = all pass;
CI-gate shaped)

| artifact | result |
|---|---|
| T1 kill −9 mid-write → reopen | **PASS** — 4,858 grains survived a SIGKILL during continuous writes; `integrity=ok`, 0 hash mismatches, 0 undecodable |
| T2 tamper detection | **PASS** — attacker with file access flips 1 byte in 1 of 100 stored blobs (verified persisted via independent connection); `verify` content-address recheck reports exactly 1 mismatch |
| T3 deletion-remnant scan | **evidence, both ways** (below) |
| T4 point-in-time restore | **PASS** — 5,000-op bundle (0.7 MB); full restore 2.9s (~1,750 ops/s, integrity ok); restore-until-HLC applied exactly 2,501 ops |

### T3, the honest-erasure evidence

The same adversarial byte-scan (find a deleted secret in the raw files) run
against both engines:

- **SQLite, upstream defaults** (`secure_delete=OFF`): secret **still present**
  in the main db file after a WHERE-scoped `DELETE`. Gone only after a manual
  `wal_checkpoint(TRUNCATE); VACUUM;` — operations no application runs on a
  schedule. (Note: Apple's *system* sqlite3 ships `secure_delete=2`/FAST, which
  does scrub — that is an Apple patch, not stock SQLite behavior; the bench
  measures both.)
- **Areev `forget`**: recall returns nothing, the op-log records the
  tombstone — but secret bytes **still present** in the WAL at file level.
  `forget` is an auditable index-level removal, not byte erasure, exactly as
  designed.

Conclusion the suite exists to keep precise: logical deletion is not byte
erasure in *any* SQLite-lineage engine. The only honest erasure is per-file
crypto-erasure (key destruction) — now wired in the store
(`Areev::open_encrypted` / `open_with_passphrase`, AES-256-GCM + Argon2id)
and proven by `areev-store/tests/encryption_tests.rs`: reopen without the
key (or with the wrong key) is denied, and a plaintext-marker scan of the db
and WAL bytes finds no leak. The `.blobs` CAS sidecar remains plaintext
(loud open warning) — see `docs/security-model.md`.

## 3. Honesty metrics — the numbers incumbents won't publish

`cargo run --release -p areev-bench --bin honesty_metrics` (exit 0 = all gates hold)

Four structural properties, measured deterministically — no LLM, no network,
no competitor hosting, so anyone can reproduce them and nobody can fudge them.
Contrast column cites primary GitHub issues by number.

| metric | measured | the failure it answers |
|---|---|---|
| **M1 idempotency** | 808 byte-identical writes → **1 grain** (807 rejected on content address) | mem0 #4573: a hallucinated "User prefers Vim" stored **808×** (97.8% of a 10k store was junk) |
| **M2 staleness-rate** | 20 supersessions → recall surfaces **1** current value (0 stale), **21-deep** history retained; the same as naive appends → recall surfaces **all 21** | mem0 #5330 (stale co-ranks), #4536 (update deletes both → empty memory) |
| **M3 write-cost** | **136µs/write** amortized (7,343/s); single-add p50 **117µs** / p99 5.2ms — **0 LLM calls, 0 tokens, $0** | mem0: 2 LLM calls/write, ~$0.30–0.80 per 100-turn chat (openwalrus), 20s add (#2813) |
| **M4 provenance** | **100%** of 504 grains carry an op-log record (op + HLC + content address); derived facts trace to their source Observation; supersession lineage reconstructs | mem0 #4573: developers hand-build a `memory_sources` table to see why a memory surfaced |

Scope, kept honest:
- M1 is EXACT-duplicate collapse (identical content incl. `created_at`) — the
  property that makes bundle import / op-log replay / retried sync idempotent.
  It is NOT a paraphrase deduper; near-duplicate phrasings need a write-time
  novelty gate (roadmap).
- M2's clean recall depends on using `supersede` (the intended update path); a
  blind re-`add` of a new value co-ranks like an append-only store. The point
  is that Areev *has* the primitive and it costs an index-layer flip, not two
  LLM calls — the update model mem0 lacks.

## 4. Accuracy — LoCoMo self-run

`cargo run --release -p areev-bench --bin accuracy -- <locomo10.json> [conv_limit]`
(dataset: snap-research/locomo `data/locomo10.json`)

The Areev half of the LR-1 accuracy story. Every conversation turn is ingested
as an Event; each question asks `recall_hybrid` for the top-k turns and we check
whether a gold-evidence turn (LoCoMo `evidence` dia_ids) is in the set. Full
LoCoMo: **10 conversations, 5,882 turns, 1,982 answerable QAs.**

The embedder is pluggable (`EmbedBackend`), so we report both the no-API floor
and a real semantic model. hit@k = at least one gold-evidence turn (LoCoMo
`evidence` dia_ids) in the top-k.

| embedder | hit@1 | hit@10 | hit@20 | MRR@10 |
|---|---|---|---|---|
| **OpenAI text-embedding-3-small (512-d)** | **33.1%** | **74.5%** | **81.6%** | **0.465** |
| TF-IDF+bigram (no API, lexical floor) | 18.6% | 40.7% | 49.3% | 0.250 |

Real embeddings roughly double the floor; **k=20 is the chosen operating point**
(retrieval keeps climbing with k, but the reader — not recall — is the bottleneck).
This is the *retrieval* leg only (vector path), scored against a lenient "≥1
evidence turn" proxy.

Reproduce the real-embedder row (precompute once, then look up in-process):
```
python3 crates/areev-bench/scripts/embed_locomo.py locomo10.json cache.json 512
AREEV_EMBED_CACHE=cache.json \
  cargo run --release -p areev-bench --bin accuracy -- locomo10.json 10
```

### End-to-end answer accuracy (LLM-judged) — bring your own models

The reader answers each question from the recalled turns (session dates are
included so relative time — "yesterday", "last week" — resolves to absolute
dates, which LoCoMo's temporal category requires); an LLM judge grades the answer
against gold. Reader and judge are independently swappable — `$AREEV_LLM_CMD`
and `$AREEV_JUDGE_CMD`, any stdin→stdout command. `scripts/openai_chat.py` is a
ready OpenAI adapter; `AREEV_LLM_DEBUG=1` logs every (question, gold, answer,
verdict) tuple for the raw transcripts you must publish alongside any number.

```
AREEV_EMBED_CACHE=cache.json AREEV_TOPK=20 \
AREEV_LLM_CMD='python3 crates/areev-bench/scripts/openai_chat.py gpt-4o-mini' \
AREEV_JUDGE_CMD='python3 crates/areev-bench/scripts/openai_chat.py gpt-4o' \
  cargo run --release -p areev-bench --bin accuracy -- locomo10.json 10
```

Full run (gpt-4o-mini reader, gpt-4o judge, real embeddings, k=20, all 1,982 QAs,
2026-07-07): **54.2%**. Every question / gold / answer / judge verdict committed in
[`results/…k20….transcripts.jsonl`](results/locomo-gpt-4o-mini-k20-2026-07-07.transcripts.jsonl)
for audit.

| category | answer accuracy |
|---|---|
| single-hop | 71.2% |
| temporal | 67.9% |
| open-domain | 45.7% |
| multi-hop | 39.4% |
| adversarial | 23.5% |

A plain retrieve-then-read pipeline, cheap reader, no LoCoMo-specific tuning;
temporal resolves because session dates are fed to the reader. Caveat, kept
precise: the number depends on the reader/judge models + the retrieval above, not
the store alone — publish model ids + raw transcripts (`AREEV_LLM_DEBUG=1`); the
LoCoMo answer key is itself ~6% wrong (dev.to/penfieldlabs). ~$0.85 / ~50 min.

**CAL + context validation** (`cargo run -p areev-bench --bin cal_validate` —
correctness, not score; CI-gate shaped, exit 1 on any faithfulness miss). On 16
real LoCoMo questions, every Areev assembly path faithfully renders the recalled
grains: `facade.recall` 16/16, CAL `RECALL…FORMAT markdown` 16/16, CAL
`ASSEMBLE…FORMAT markdown` 16/16, `ContextAssembler` 16/16. This validates the
areev-cal parser→executor→facade→FORMAT and areev-context render paths on real
data (input→expected-output). Finding: `ContextAssembler` renders each turn's date
from `Event.created_at`, so driving the reader prompt through CAL/ContextAssembler
(rather than hand-formatting) requires turns to carry their real LoCoMo session
timestamp — the wiring for the next iteration.

## 5. In-process latency gates — `areev-store` examples

*Rerun 2026-07-14 · Apple M4 Max, macOS 26.5.2 · `--release`, workspace LTO
profile. These are the source of the README/FAQ in-process latency figures.
They live in `crates/areev-store/examples`, not `areev-bench`.*

`cargo run --release -p areev-store --example bench` — 13k grains (10k facts /
800 subjects + 3k events / 150 sessions), bare `open()` (FTS index **on**):

| operation | p50 µs | p95 µs | p99 µs | target µs | verdict |
|---|---|---|---|---|---|
| recall about subject (k≤16, deserialize) | 30.2 | 42.6 | 60.7 | 200 | PASS |
| `entity_latest` head (full grain) | 9.2 | 12.0 | 19.0 | 100 | PASS |
| thread_tail 20 events (deserialize) | 125.2 | 160.1 | 241.2 | 2000 | PASS |
| add single grain (full txn, **FTS on**) | 303,826 | 333,683 | 359,985 | 1000 | FAIL |

The `add` row FAILs its 1ms gate **by design of the load**: with the FTS index
live, every single-grain txn pays the ~140ms/write text-index tax (finding #1),
and single-row txns don't amortize it. This is the write path production
voice/edge deployments avoid — `AreevOptions { index_text: false }` (or
`defer_text_index()` for bulk loads) drops it to the tens-of-µs class; see the
honesty §3 write-cost metric (~136µs amortized) and the voice-loop write-back
below. Recall latency (the other three rows) is independent of how the data was
loaded.

`cargo run --release -p areev-store --example voice_loop` — 50ms-cadence loop,
FTS off (the voice/edge profile):

```
voice loop: 400 frames @50ms, 50 write-backs, wall 20.0s
frame recall  p50 79.0µs  p95 98.4µs  p99 151.9µs  (target <200µs)
write-back    p50 494.2µs p95 1085.7µs             (off audio thread in prod)
verdict: PASS
```

## 6. Edge reference — measured on real devices

**A ten-year-old $35 computer serves agent memory in microseconds, and a 2018
mini-PC matches a 2024 laptop.** Both numbers below come from the same harness
(`crates/areev-bench/scripts/edge_bench.py`), run on the devices themselves,
with the CPU clock sampled throughout every phase so nothing is published at a
reduced clock.

| | Raspberry Pi 3 Model B | Intel NUC8i3BEH |
|---|---|---|
| class | Feb 2016, $35 SBC | 2018 mini-PC |
| CPU | 4× Cortex-A53 @1.2 GHz | Core i3-8109U, 2c/4t @3.0–3.6 GHz |
| RAM | 905 MiB usable | 7.1 GiB |
| storage | SanDisk 64 GB microSDXC (`SC64G`) | WD 250 GB NVMe (`WDS250G3X0C`) |
| …measured here | 23.2 MB/s read · 18.7 MB/s write · **825 kB/s** 4 KiB dsync (~200 durable IOPS) | 2.4 GB/s read · 919 MB/s write · **3.1 MB/s** 4 KiB dsync (~770 durable IOPS) |
| OS | RPi OS Lite **arm64** Trixie · kernel 6.18 · glibc 2.41 · Py 3.13 | Ubuntu Server 26.04 · kernel 7.0 · glibc 2.43 · Py 3.14 |
| install | `pip install areev` — 16 s, no compiler | `pip install areev` — 16 s, no compiler |

```
pip install areev                                   # wheel; never build on-device
crates/areev-bench/scripts/edge_bench.py --vector   # emits the tables below
```

### Results

| operation | Pi 3 B (2016 ARM) | NUC8i3BEH (2018 x86) | for reference: §5 M4 Max, Rust API |
|---|---|---|---|
| recall p50 @ 500 / 2k / 8k grains | 348 / 360 / **361 µs** | 29 / 29 / **30 µs** | 30.2 µs @13k |
| recall p99 @8k | 591 µs | 67 µs | 60.7 µs |
| recall *miss* | 27 µs | 3 µs | — |
| `latest()` head read @8k | 690 µs | 51 µs | 9.2 µs |
| CAL `RECALL FACTS ABOUT` @8k | 2.79 ms | 0.26 ms | — |
| **`migrate` bulk import** (index deferred) | **3.90–4.04 ms/grain** | **0.37–0.39 ms/grain** | — |
| `add_fact` single txn, FTS live (empty → @500) | 26.3 → **201.2 ms** | 4.0 → **24.5 ms** | 304 ms @13k (§5) |
| Areev vector scan @ 500 / 2k | 9.4 / 34.9 ms | 0.9 / 3.7 ms | — |
| embed one query (all-MiniLM-L6-v2) | 270 ms | **23 ms** | — |
| end-to-end `nearest()` (model + scan) | ~280 ms | **24 ms** | — |
| RSS: engine / with model resident | ~50 / 348 MiB | ~102 / 409 MiB | — |

Three readings matter more than the individual numbers.

**Recall is flat in corpus size on both.** 16× the data, same latency — 348→361 µs
on the Pi, 29→30 µs on the NUC — because recall is an indexed point lookup, not
a scan. A device can accumulate memory for months and answer as fast on day 200
as on day 1. That is the property that makes an unattended edge deployment
sustainable, and it holds across a 12× hardware gap. (A *miss* costs 27 µs / 3 µs,
so any recall benchmark that doesn't assert a hit reports ~10× better than reality.)

**A 2018 mini-PC lands within a whisker of a 2024 laptop.** The NUC's 30 µs recall
matches §5's 30.2 µs on an M4 Max — and does it *through* the Python binding's FFI
and JSON round-trip, which the Rust benchmark skips. You do not need current
hardware to run this well.

**The write path is the one thing to design for, on both.** Single-grain
transactions against a live FTS index degrade as the index fills (26→201 ms on
the Pi, 4→24.5 ms on the NUC) while the deferred-index bulk path stays flat —
a **50× gap on SD and still 63× on NVMe**. Storage explains why the gap survives
better hardware: durable 4 KiB writes only improve ~4× from SD to consumer NVMe
(200 → 770 IOPS), because fsync latency, not bandwidth, is what a live index
pays per transaction. So on any device: bulk-load through `migrate` /
`defer_text_index()`, and run write-heavy workloads with
`AreevOptions { index_text: false }` (the voice/edge profile).

**Vector recall** is dominated by the embedding model on both — 270 of ~280 ms on
the Pi (~98%), 23 of 24 ms on the NUC (~96%); Areev's own scan is 0.9–35 ms.
On 2016-era ARM that means preferring the BM25 + `EnglishExpander` edge profile
or embedding off-device. On the NUC, 24 ms end-to-end makes on-device semantic
recall comfortably interactive.

### What this looks like on current hardware

The Pi 3 B is the **worst case Areev supports**, and the NUC is already
eight-year-old hardware. A Raspberry Pi 5 changes the three variables that bound
the Pi 3: a Cortex-A76 at 2.4 GHz (~5× the core), NVMe over PCIe instead of a
200-IOPS card, and 4–16 GB of RAM.

*Projected, not measured — stated so nobody mistakes it for a result:*

| | Pi 3 B (measured) | Pi 5 (projected) |
|---|---|---|
| recall p50 | 361 µs | ~60–90 µs |
| bulk import | 3.9 ms/grain | sub-millisecond |
| single `add_fact`, FTS live | 201 ms | ~5–20 ms |
| embed (MiniLM, on-device) | 270 ms | ~50–70 ms |
| build from source | impossible (1 GB) | comfortable |

The NUC column above is partial corroboration: on x86 with NVMe, recall already
lands at 30 µs and bulk import at 0.39 ms/grain — ahead of what this table
projects for a Pi 5, so the projection is conservative in the right direction.
Anyone can check it in one command: `edge_bench.py --vector` prints these tables
for whatever device it runs on.

### Method — how these numbers are kept honest

`edge_bench.py` enforces three rules, each of which corrects a way edge
benchmarks routinely mislead:

- **Recall benchmarks must hit.** A query for a subject that was never imported
  measures the miss path — 27 µs against 361 µs on the Pi, a 13× flattering
  error that looks like a spectacular result. The harness asserts a non-empty
  result before timing.
- **The clock is sampled during a phase, never after.** It recovers to nominal
  within milliseconds of the load ending, so a run measured at a reduced clock
  reads back as full speed if you check once it is over.
- **On a Pi, only `vcgencmd` knows the clock.** `/sys/.../scaling_cur_freq`
  reports the governor's *request*; it read a flat 1200 MHz across 412 samples
  of a phase the firmware was actually running at 600 MHz.

## 7. Server tier — the postgres backend

*Run: 2026-08-07 · Apple M4 Max → `pgvector/pgvector:pg16` in local Docker
(loopback TCP; ~0.1–0.3 ms per round trip) · `pg_bench` (`cargo run --release
-p areev-bench --features postgres --bin pg_bench`) · same dataset shape as
§5: 10k facts / 800 subjects, seeded xorshift, `index_text` on, 2,000 queries
+ 300 warmup.*

**These are a different latency class by design and must never be quoted next
to the §1/§5 embedded numbers without the topology.** The server tier exists
for deployments with no durable disk; its contract is millisecond-class
turn-level recall with multi-writer HA, not the embedded microsecond class.
Numbers scale with the network: same-VPC managed Postgres adds ~0.2–0.5 ms per
round trip over this table; cross-AZ more.

| pg_bench (postgres backend, loopback Docker) | p50 µs | p95 µs | p99 µs | sanity bound | verdict |
|---|---|---|---|---|---|
| recall about subject (k<=16, deserialize) | 240 | 391 | 477 | 5000 | PASS |
| entity_latest head (full grain) | 564 | 907 | 1188 | 3000 | PASS |
| hybrid free-text recall (k=8) | 12766 | 14788 | 15855 | 20000 | PASS |
| add single grain (full txn) | 3671 | 4429 | 5642 | 20000 | PASS |
| bulk load (add_batch 500/grain, FTS on) | ~3.3 ms/grain | — | — | — | — |

Readings:

- **Structural recall is ONE round trip** on this backend (the probe+blob
  join from the backend-shaped read work), which is why it lands at 240µs —
  ~8x the embedded 30µs, not the ~17x a per-blob fetch loop would cost.
  `entity_latest` pays two sequential round trips (probe, then fetch by
  hash), which is why the "cheaper" read is slower here — a batching
  candidate if it ever matters at this tier.
- **The hybrid number is a worst-ish case on purpose**: every document in
  this corpus shares the token "value", so the BM25 leg drags a ~10k-row
  posting list across the wire per query. Distinctive queries run far
  closer to the structural number.
- **Writes carry the serialization point**: the ~3.7ms single-add includes
  the `counters` claim that makes concurrent writers safe, ~16 statements,
  and live FTS postings. The same corpus loads at ~3.3ms/grain batched —
  compare the embedded engine's §5 finding that FTS-on single adds cost
  ~300ms there (fsync-bound): the write tax lives in different places.
- The sanity bounds are deliberately loose (they gate the harness, not the
  product): topology dominates, so publishable claims must name theirs.

1. **FTS write tax is per-row, not per-txn as documented.** Loading 10k
   facts through `add_batch` (500/batch) with default options
   (`index_text: true`) took **1,383s** (~138ms/grain); the identical load
   with `index_text: false` takes **1.0s**. The store CLAUDE.md says "~150ms
   per write txn" — batching does not amortize it.
   **FIXED for bulk loads**: `defer_text_index()` drops the FTS index for
   the duration and `rebuild_text_index()` re-creates it afterwards — Turso
   indexes all existing rows at CREATE INDEX time (measured: 500 pre-existing
   rows indexed in ~4.5ms vs ~160ms for a single 100-row live-index txn).
   `areev migrate` does this automatically; `areev reindex` exposes the rebuild
   (including text backfill for files that flipped `--index-text true` after
   writing). The per-row tax still applies to normal live writes with the
   index present.
2. **Raw-turso autocommit writes can silently fail to persist.** The T2
   tamper write initially "succeeded" via bare `execute(UPDATE)` on a raw
   turso connection but was gone after reopen; an explicit `BEGIN`/`COMMIT`
   persists. Upstream-report candidate; also relevant to anything else that
   opens store files with raw turso.
3. **`forget` leaves the object string in the terms dictionary and WAL**
   (T3b). Fine under the crypto-erasure story, but a `terms` GC (or at least
   a docs note) would tighten it.
4. **The read path is flat in corpus size; the write path is bounded by fsync**
   (§6). Recall holds steady from 500 to 8,000 grains on both measured devices
   (~361 µs on a Pi 3, ~30 µs on a 2018 NUC), so an edge deployment is bounded
   by *write* strategy, not by how much it remembers. Better storage does not
   rescue the live-index path: durable 4 KiB writes improve only ~4× from
   microSD to consumer NVMe (200 → 770 IOPS), because a single-grain txn pays
   fsync latency, not bandwidth — hence 50× (SD) and still 63× (NVMe) between
   the live-index and deferred-index paths. The FTS tax of finding #1 is the whole story
   there: 200 ms/grain live versus 4 ms/grain deferred, at only 500 grains.
5. **Two traps make edge benchmarks lie**, both now guarded by
   `scripts/edge_bench.py`: a recall benchmark that doesn't assert a *hit*
   silently measures the miss path (27 µs vs 361 µs on the Pi — a 13×
   flattering error), and on a Pi `/sys/.../scaling_cur_freq` reports the
   governor's request rather than the real clock, so it reads 1200 MHz through
   a phase the firmware is running at 600. Sample `vcgencmd measure_clock arm`
   *during* the phase, never after.

## Areev Loop analyzer precision (fixture floor)

`cargo run --release -p areev-bench --bin loop_precision`

Fixture-measured precision/recall for the deterministic Areev Loop analyzers
(proposal §8: no invented precision — measured numbers decide default-on).
The fixture plants, per analyzer, N=6 positives (situations the analyzer
should flag) and N=6 decoys (look-alikes it must not), then runs the real
engine over the in-memory reference substrate and classifies every proposed
recommendation by its deterministic summary. On this clean fixture a correct
analyzer scores precision 1.00 (never fires on a decoy); the bin exits
non-zero if a default-on analyzer drops below 0.90. This is an explicit fixture
gate, not a CI workflow step; ordinary CI runs the shared metric unit tests and
the engine's deterministic/golden tests through `cargo test --workspace`.

| analyzer | proposed | TP | FP | precision | recall |
|---|---|---|---|---|---|
| loop.cold_grains | 6 | 6 | 0 | 1.00 | 1.00 |
| loop.contradiction_sweep | 6 | 6 | 0 | 1.00 | 1.00 |
| loop.coverage_gap | 6 | 6 | 0 | 1.00 | 1.00 |
| loop.duplicate_sweep | 6 | 6 | 0 | 1.00 | 1.00 |
| loop.skill_stall | 6 | 6 | 0 | 1.00 | 1.00 |
| loop.staleness | 6 | 6 | 0 | 1.00 | 1.00 |
| loop.tool_failure | 6 | 6 | 0 | 1.00 | 1.00 |

(`loop.goal_stagnation` is default-**off** — "stalled" is ambiguous — and
`loop.budget_pressure`, default-on since its ASSEMBLE overflow datasource was
wired, is a single global signal; neither appears in this per-finding fixture,
and both are unit-tested separately. The two telemetry-fed fixtures,
`cold_grains` and `coverage_gap`, run over an injected telemetry snapshot in
the same harness.)

This is a **synthetic floor**, not a field number: it proves the analyzers
don't fire on obvious look-alikes and catch obvious positives. Real-world
precision needs a real telemetry + labels corpus (fork_surfacing and
outcome_review need concurrent heads / applied history and are exercised by
the crate tests, not this fixture). All seven fixture analyzers clear the
0.90 default-on bar.

## Areev Loop reflection — Effective Reliability (verifier machinery)

`cargo run --release -p areev-bench --bin loop_reflection`

Scores the LLM reflection pipeline on a reference corpus of planted positives
(real hidden issues DISCOVER should surface) and decoys (superficially similar
but legitimate), with a deterministic mock backend so the run is reproducible
in CI. **Effective Reliability = (useful-correct − wrong) / positives** —
it subtracts for confident-wrong, so over-generation lowers it, unlike raw
precision.

| pipeline | surfaced | useful | wrong | ER | precision | recall | spurious |
|---|---|---|---|---|---|---|---|
| no verifier (accept grounded) | 6 | 3 | 3 | +0.00 | 0.50 | 1.00 | 0.50 |
| with verifier (GROUND → VERIFY → ROUTE) | 3 | 3 | 0 | **+1.00** | 1.00 | 1.00 | 0.00 |

The verifier lifts ER from +0.00 to +1.00 on this corpus by filtering the
decoys; the explicit binary guards spurious = 0 and recall ≥ 0.9, while CI
unit-tests the shared scorer. This is the **machinery
number** (mock backend, reference corpus) — it proves the pre-queue filter
discriminates, not what a given model scores in the field. A live model can be
scored with `AREEV_LOOP_EVAL_MODEL` (see `loop_reflection.rs`); the live
approval-rate of `origin=llm` findings accrues per file and prints on
`areev loop`. A corpus-scale ER number on a labeled non-parasitic corpus is
tracked as an open follow-up in `docs/loop-reflection.md` §6.
