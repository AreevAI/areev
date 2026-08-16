# Changelog

All notable changes to Areev are documented here. The format is based on
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project
adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

Areev descends from **DejaDB** (github.com/AreevAI/dejadb, frozen at 1.2.0);
the pre-rename release history lives in that repository's `CHANGELOG.md`.

## [Unreleased]

## [1.0.0] - 2026-08-16

The first release under the Areev name — the complete engine formerly
published as DejaDB 1.2.0, plus the governed-agents program (the `areev run`
runtime, agent-grade capture, the ecosystem adapters, and the enterprise
plane), renamed on every surface.

### The memory engine

- **Immutable, content-addressed grains** in the `.mg` format — 12 grain
  types, canonical serialization (NFC, sorted keys, omit-defaults), SHA-256
  content addressing. Every edit is a supersession, every removal a
  tombstone or crypto-erasure; nothing ever rewrites a stored blob.
- **One memory = one isolation unit** — a single file on the embedded Turso
  backend, a schema on the PostgreSQL backend (`feature = "postgres"`,
  advisory-locked writers, pgvector) — the unit of erasure, sync,
  portability, and write parallelism. Files are self-describing: saved
  queries, templates, and index declarations travel with the file.
- **Hybrid recall in microseconds** — dictionary-encoded triples, an owned
  BM25 inverted index, optional vector recall via a pluggable embedder
  (`--embed-cmd`), graph/time reads (`related`, `entity-at`,
  `step-actions`), heads/forks with explicit merges, bundles, encrypted
  incremental sync, and CAS blob storage (encrypted under an HKDF-derived
  subkey when the memory is).
- **CAL — the Context Assembly Language** — lexer/parser/executor,
  `ASSEMBLE` with facade mounts for cross-memory queries, and budget-aware
  SML/TOON/Markdown/JSON rendering for model-ready context.

### Governance

- **Authorization in the file** (CAL 1.3): grants ride as `mg:permits`
  Facts; destruction (`FORGET <hash>`, `FORGET SUBJECT`, `PURGE OLDER
  THAN`) is authorization-gated with mandatory `BECAUSE` and a Tier-2 audit
  Observation on every execution; `REPORT SUBJECT` shares one selector with
  erasure so a DSAR discloses exactly what an erasure removes.
- **GDPR compliance pack** — [`docs/gdpr.md`](docs/gdpr.md) article→
  capability map, DSAR `subject-report` on every surface, `audit export`,
  declarative `retention:<ns>` policies, and erasure that names its
  subject by fingerprint, never by identity.

### Areev Loop — governed self-improvement

- Substrate-agnostic engine: 13 deterministic analyzers, four gates, a
  recommendation lifecycle with pinned evalsets, the DISCOVER→GROUND→VERIFY
  LLM verifier, outcome measurement across horizons, and out-of-box LLM
  backends (OpenAI-compatible / Anthropic / Ollama). Trajectory capture,
  `analyze_only` replay against the immutable past, and `areev corpus`
  export with erasure-aware provenance.

### `areev run` — the governed runtime

- A pure sans-IO scheduler (`areev-run-core`: `step(env, state, events) →
  (commands, state)`, frozen condition grammar, plan validation, `RUN-Ennn`
  errors, no clock/rand/IO in its dependency tree — CI-enforced) under a
  journaling driver (`areev-run`): intent-before-effect journal grains,
  checkpoints, crash-safe resume with same-key redelivery, HITL respond
  with separation of duties, budgets, cancel, and journal-consistent
  `verify`.

### Surfaces

- **`areev`** — the CLI (~29 verbs), including `migrate` importers from
  other memory systems, `hub` (the areevd sync daemon), `ui` (the embedded
  web console: memory browser, interactive graph, loop review queue, runs
  tab), and `hook claude-code` session capture.
- **MCP** — 23 tools over newline-delimited JSON-RPC 2.0 on stdio,
  protocol rev `2025-06-18`.
- **Bindings** — Python (`pip install areev`, abi3, sync + async) and
  Node (`npm install areev`, napi native addon), same facade, scalars in /
  JSON out.
- **Adapters** — `areev-langgraph` (checkpointer, store, memory saver) and
  `areev-crewai` (storage backend, knowledge source, audit listener) on
  PyPI.

### Benchmarks

- Reproducible latency, honesty, and LoCoMo-accuracy harnesses in
  `crates/areev-bench` (`RESULTS.md` has the numbers), with perf gates
  (`bench`, `voice_loop`) run as examples.

[Unreleased]: https://github.com/AreevAI/areev/compare/v1.0.0...HEAD
[1.0.0]: https://github.com/AreevAI/areev/releases/tag/v1.0.0
