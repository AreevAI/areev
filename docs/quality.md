# Quality, measured

An engine you embed runs inside your process, holds the memory your agent is
trusted to act on, and — through `FORGET SUBJECT` — destroys data on request.
That is a lot to ask of a dependency, so the engineering is measured in the
open and regenerated from the tree on every CI run. This page explains how
each number is produced and what gates it; the numbers themselves live in the
generated artifacts it links.

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="assets/repo-stats-dark.svg">
  <img src="assets/repo-stats-light.svg" width="760"
       alt="Areev repository quality metrics — source and test line counts, test count, line coverage, and stable error codes, generated from the tree">
</picture>

## The tests

Tests are about a third of the codebase, and roughly half of that is
*integration* testing — the CLI and MCP suites drive the real binary over
real stdio, not mocks. `cargo test --workspace` runs the lot in under a
minute. Test lines are counted at **block** granularity, not file granularity:
a `src/` file with a 30-line `#[cfg(test)]` module and 400 lines of
implementation contributes 30 test lines — counting whole files as "tests"
would inflate the ratio roughly 4× on this tree, which is exactly the trap
[`scripts/repo_stats.py`](../scripts/repo_stats.py) exists to avoid.

## The coverage number — and why it is the lowest one we could quote

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="assets/coverage-dark.svg">
  <img src="assets/coverage-light.svg" width="760"
       alt="Line coverage per crate, each bar against its own CI floor">
</picture>

**Line coverage counts *source* lines only.** `tests/` and `benches/` are
excluded outright and `#[cfg(test)]` blocks line-by-line, because a test body
is executed by definition and counting it scores the suite against itself. On
the same scope with test code counted back in, the trace reads higher;
unfiltered — every instrumented line, which is what a naive `cargo llvm-cov`
summary prints — it also reads higher. **We publish the one that is hardest
on us.** The scope leaves out only code the coverage job structurally cannot
execute — the PyO3 bindings pytest drives, the Node addon that is not a cargo
workspace member, the benchmark harnesses, the Postgres backend that needs a
live server — and each exclusion carries its reason in
[`coverage.json`](coverage.json) rather than being quietly convenient.

**Coverage is enforced per crate, not as one workspace target.** A single
number lets a regression in the loop engine hide behind a gain in the CAL
parser, and those crates do not carry the same risk — so each has its own
floor and CI fails the build when any one slips below it. The floors are a
regression ratchet held a couple of points under measured — which is where a
regression actually gets caught — with a looser aggregate backstop under the
whole set. The lowest crates, `areev-cli` and `areev-mcp`, are named as the
next testing work rather than quietly averaged away. Full per-crate table:
[`repo-stats.md`](repo-stats.md#coverage).

## The gates

- **Every user-facing error has a stable code.** `DOMAIN-Ennn`, **append-only**
  — a code is never renumbered or reused, so an error you handle today keeps
  its meaning across upgrades ([`ERROR_CODES.md`](../ERROR_CODES.md)).
  Format and uniqueness are test-enforced.
- **Both storage backends run the same conformance suite.** One case list —
  forks, replication, tombstones, PITR, BM25, vectors, CAS, CAL — executed
  against embedded Turso *and* PostgreSQL, so backend choice cannot quietly
  change semantics.
- **CI is the gate, not a formality.** Tests on Linux, macOS and Windows;
  `clippy -D warnings`; a pinned MSRV build; `cargo doc`; coverage measured,
  checked against the published figure and floored per crate; `cargo deny`
  for advisories and licences; the Python and Node bindings built and tested
  on every commit; and the flagship agent example run end to end, keyless,
  both chapters.
- **The docs are executable.** The CAL examples in
  [`cal-reference.md`](cal-reference.md) are parsed by a test that fails CI
  on a stale one — the reference cannot drift from the language.
- **The published numbers cannot go stale.** Every figure in this page's
  charts is regenerated from the tree by
  [`scripts/repo_stats.py`](../scripts/repo_stats.py) (line counts) and
  [`scripts/coverage.py`](../scripts/coverage.py) (coverage, from the
  `cargo llvm-cov` trace CI already produces); CI runs `--check` and fails
  the build if the committed artifacts drift from the tree.

## The benchmarks

Latency, honesty, and accuracy are measured by reproducible harnesses in
[`crates/areev-bench`](../crates/areev-bench) — full methodology, raw data,
and committed transcripts in
[`RESULTS.md`](../crates/areev-bench/RESULTS.md). Headlines:

- **Latency** (Apple M4 Max): structural recall **~30 µs** p50 in-process,
  `entity_latest` **~9 µs**, a 50 ms-cadence voice loop with live write-back
  at **79 µs p50 / 152 µs p99** per frame — and the same recall through a
  localhost HTTP sidecar costs 158 µs, which is the whole argument for
  embedding: the cost is the network hop, not the store.
- **Edge hardware, measured on the devices**: a $35 Raspberry Pi 3 from 2016
  serves recall at **~361 µs, flat from 500 to 8,000 grains**; a 2018 Intel
  NUC matches the 2024 laptop at ~30 µs — through the Python binding's FFI
  ([RESULTS.md §6](../crates/areev-bench/RESULTS.md)).
- **Memory quality** ([LoCoMo](https://github.com/snap-research/locomo)):
  hit@10 / hit@20 of **74.5% / 81.6%** with OpenAI `text-embedding-3-small`;
  end-to-end answer accuracy 54.2% with a cheap untuned reader. Every answer
  and judge verdict is committed for audit — the category has a history of
  unreproducible claims, so we publish the receipts.
- **Memory integrity** (deterministic, no LLM):
  `cargo run -p areev-bench --bin honesty_metrics` asserts the dedup,
  staleness, provenance, and write-cost claims on every run.
- **Loop precision**: 1.00 on the labeled fixture, with a 0.90 failure floor
  when the fixture runner is invoked
  (`cargo run -p areev-bench --bin loop_precision`).

The README's latency chart is rendered from these published numbers by
[`scripts/bench_chart.py`](../scripts/bench_chart.py); it is re-run by hand
when `RESULTS.md` changes, because measurements are not something a CI
container can recompute honestly.
