# Changelog

All notable changes to Areev are documented here. The format is based on
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project
adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

Areev descends from **DejaDB** (github.com/AreevAI/dejadb, frozen at 1.2.0);
the pre-rename release history lives in that repository's `CHANGELOG.md`.

## [Unreleased]

## [1.1.0] — 2026-08-16

### Added

- **Anonymization: prompt-safe pseudonymization** (`areev anonymize`,
  cookbook recipe 16). Declare one `anon:<ns>` policy — a file-truth that
  replicates write-if-absent and fails reads closed when unreadable — and
  every model-facing read (recall/search/CAL/MCP/graph reads) returns typed
  placeholders (`[PERSON_1]`) instead of identities:
  - **Detection** is layered: built-in Tier-0 (structural known-identity
    propagation, regex + Luhn/mod-97 validators, secrets, keyword cues,
    dictionaries), a pluggable NER command seam (`--anonymize-cmd`), and a
    grounded LLM detector (`--anonymize-llm-cmd`) — a policy demanding an
    uninstalled detector fails closed. Actions: `pseudonym`, `mask`,
    `redact`, `generalize:month|year|decade`, `allow`.
  - **The round trip**: mappings stay in process custody
    (`anon_mappings()`, `rehydrate_text()`; payloads carry an `anonymized`
    report with mapping *ids* only). `PseudonymizingBackend` wraps any
    `LlmBackend` so extraction requests leave pseudonymized and responses
    return rehydrated.
  - **Ingress mode + `memory` scope** (encrypted memories): value-derived
    tokens transform *before* the content address commits; `FORGET
    SUBJECT`/`REPORT SUBJECT` recompute the stored pseudonym from the real
    identity, so pseudonymized-at-rest never means erasure-proof.
  - **The sealed vault** (`vault:` rows under an HKDF subkey of the page
    key; never replicated; erased with the subject; TTL-swept): tokens
    continue across processes, and `areev anonymize reveal` /
    `reveal_tokens()` is admin-gated and Tier-2 audited by fingerprint.
  - Surfaces: CLI verb family + `--anonymize-egress` host floor, Python and
    Node methods in lockstep, the console's Anonymization card + per-grain
    "Model view" (`GET /api/anon/preview`, `POST /api/anon/config`),
    `/api/config` observability, conformance cases on both backends
    (Postgres: egress/audit work; value-derived features refuse loudly —
    no page cipher there).
  - Explicit text APIs ship too: `scan_text` / `anonymize_text` /
    `rehydrate_text` and the store-free `areev anonymize scan`.
  - Honest scope, by design: this is **pseudonymization** of the egress
    channel, not anonymity — see `docs/security-model.md` and
    `ARCHITECTURE.md` §10 for the threat model and named decision.
- **`min_reader_version` stamping on anonymization policies** so older
  builds warn loudly at open; `anon:` joins the replicable meta prefixes,
  `vault:` is reserved and never replicates.

### Changed

- **One rendering stack.** Per-grain
  rendering now has a single implementation — `areev_cal::render` — shared
  by CAL's `FORMAT` arms and `areev-context`, with byte parity pinned by a
  cross-surface golden. Output changes that follow:
  - `FORMAT sml` emits semantic per-type elements
    (`<fact confidence="0.95" date="2026-01-13">john prefers window
    seat</fact>`) instead of generic `<grain type=…>` field dumps; event
    elements carry the speaker as `role="…"`.
  - `FORMAT markdown` gains dedicated arms for state / workflow / reasoning /
    consensus / consent / recommendation grains (topology and labels instead
    of a raw field-pair dump); fact/event/tool lines are byte-identical to
    before.
  - `recall --render` (markdown/json/toon/plain) converges on the CAL
    shapes: markdown carries the documented `- ` bullet and the
    confidence-below-1.0 rule, json is the `{hash, grain_type, fields}`
    envelope, toon rows come from the registry columns.
  - `FORMAT toon`'s `state` rows read the OMS §8.3 `context` key (previously
    `context_data`, which never matched — rows always fell back to
    `state,state`).
  - One `chars/4` token estimator (`render::estimate_tokens`) serves
    `ASSEMBLE … BUDGET` and the areev-context allocators, so a budget means
    the same thing on every path.
- **Progressive disclosure is real.** The context allocators emit
  Full→Summary→Omit (70%/95% thresholds); budgeted `FORMAT TEMPLATE` renders
  pick their disclosure tier from tokens-per-grain, so `ELEMENT_SUMMARY`
  fires under pressure and `ELEMENT_OMIT` accounts for dropped grains —
  behavior the reference already promised. JSON and TOON stay whole-entry
  (a prose summary inside a structured dump would corrupt it).
- **The registry replicates.** Bundles/segments carry saved queries,
  templates and retention policies in a v2 `MGB2` meta segment (emitted only
  when the file has registry rows — registry-free bundles stay MGB1 and
  readable by older builds; older builds refuse an MGB2 bundle loudly).
  Import merges latest-wins on `updated_at`; `last_run_at` never replicates
  and survives locally; retention rows apply only when locally absent; a
  point-in-time restore skips the segment. New conformance cases cover both
  backends; `ImportStats` gains `meta_applied`/`meta_skipped`.

### Removed

- The six whole-result builtin templates (`triples`, `progressive`,
  `llm_system_prompt`, `llm_chat`, `weekly_standup`, `toon`) — unused, and
  `toon`/`triples` shadowed the same-named `FORMAT` arms with different
  output. Builtins are now exactly the three §10.1 sectioned presets
  (`structured`/`readable`/`compact`), and a builtin can never take a
  `FORMAT` arm name. `FORMAT TEMPLATE toon` now returns `TemplateNotFound`
  — use `FORMAT toon`.
- The never-wired `CalExecutorConfig::max_cal_queries`/`max_cal_templates`
  caps (no host set them, and their `Some(-1)` = unlimited convention was
  implemented backwards). The registry-level limits (100 queries/namespace,
  50 templates, body-size caps) remain the enforcement.
- Dead `areev-context` dependency declarations in `areev-py`, `areev-js`,
  and `areev-server`.

### Docs

- Saved queries and templates are now discoverable where agents look:
  the `cal-for-llms.md` grammar card gains a SAVED block, the MCP reference
  documents the `DESCRIBE QUERIES` → `RUN` pattern under `areev_cal`, and
  cookbook recipe 15 walks the ship-assembly-logic-in-the-file pattern
  (the Hermes provider's override). `llms.txt`'s MCP tool count corrected
  (14 → 23); `docs/facts/context-assembly.md` re-verified.

## [1.0.2] - 2026-08-16

### Fixed

- **`verify` on canceled runs, at every cancel phase.** Replay fed
  `CancelSeen` at a superstep's open whenever the coming checkpoint
  carried the cancel — a phase the live driver only produces when the
  marker predates the run, so a cancel landing during the first
  superstep (before any checkpoint) failed verify on slow machines.
  Replay now places the cancel by the journal's own evidence: with the
  wave's resolutions when the closing checkpoint shows they ran, or by
  rewinding the boundary and feeding it first when the journal shows
  the live driver canceled before dispatching. A new phase-sweep test
  exercises every placement on every machine.
- **Windows `--tool-cmd` quoting.** The 1.0.1 `cmd /C` fix still routed
  the command through `Command::arg`, whose MSVC quoting `cmd.exe` does
  not parse; the command string now goes through `raw_arg`.
- **RUSTSEC-2025-0134.** Replaced the unmaintained `rustls-pemfile`
  with `rustls-pki-types`' PEM support (already in the tree via
  rustls); the `tls` feature's surface is unchanged.

## [1.0.1] - 2026-08-16

### Fixed

- **`areev run` wave determinism.** The driver fed effect completions to
  the pure scheduler in racy arrival batches, each with its own clock
  reading — scheduler state depended on thread timing (an unjournaled
  decision), so two identical runs could checkpoint differently and
  `areev run verify` could diverge from a live run under load. The driver
  now drains every dispatch wave fully and feeds one close reading plus
  all resolutions in dispatch order — exactly the cadence `verify`
  replays. Journal-answered replays join the same wave rather than
  resolving early.
- **Windows `--tool-cmd`.** `/bin/sh` was hardcoded in the host tool
  executor and the eval seam; both now use the platform shell
  (`cmd /C` on Windows).
- **`areev-run-core` purity gate.** Dropped the workspace's only `chrono`
  use (a `created_at` fallback in canonical serialization, now
  `std::time`), so the CI gate that keeps clock/rand/IO out of the pure
  scheduler's dependency tree actually passes.

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
  Node (`npm install @areev/areev`, napi native addon; the unscoped `areev` name is pending an npm similarity-filter exception), same facade, scalars in /
  JSON out.
- **Adapters** — `areev-langgraph` (checkpointer, store, memory saver) and
  `areev-crewai` (storage backend, knowledge source, audit listener) on
  PyPI.

### Benchmarks

- Reproducible latency, honesty, and LoCoMo-accuracy harnesses in
  `crates/areev-bench` (`RESULTS.md` has the numbers), with perf gates
  (`bench`, `voice_loop`) run as examples.

[Unreleased]: https://github.com/AreevAI/areev/compare/v1.1.0...HEAD
[1.1.0]: https://github.com/AreevAI/areev/compare/v1.0.2...v1.1.0
[1.0.2]: https://github.com/AreevAI/areev/compare/v1.0.1...v1.0.2
[1.0.1]: https://github.com/AreevAI/areev/compare/v1.0.0...v1.0.1
[1.0.0]: https://github.com/AreevAI/areev/releases/tag/v1.0.0
