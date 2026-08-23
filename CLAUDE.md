# Areev

Embedded memory engine for AI agents — reference implementation of OMS (Open
Memory Spec). Rust workspace of 16 crates (plus `areev-js`, a standalone napi
package built outside the workspace, and `adapters/` — the pip packages
`areev-langgraph` and `areev-crewai`, see `adapters/CLAUDE.md`). Memories
are immutable content-addressed grains in per-file Turso databases, queried
with CAL, and rendered into model-ready context in-process (no server in the
recall path).

**Status**: published — the library crates + the `areev` binary on crates.io,
`areev` on PyPI, and the Node binding on npm as **`@areev/areev`** (+ scoped
per-platform addons). Unscoped `areev` on npm is still blocked: npm's
similarity filter 403s it against `argv` (`Package name too similar to
existing package argv`), unresolved as of the 1.5.2 release attempt
(2026-08-22) despite an earlier belief the exception had been granted —
`areev-win32-x64-msvc`'s separate *security hold* was released that day, but
the *main package's* filter block was not, and the two got conflated here
once already. A support ticket for the unscoped name is open; until it
lands, npm releases publish under the pre-1.5.1 `@areev/*` scope (never the
bare `areev-<platform>` names, which 403 the same way). crates.io and PyPI
are unaffected — different registries, no naming block there.
`areev-py`, `areev-bench`, and `areev-conformance` stay `publish = false`;
`areev-js` ships to npm, not crates.io.
The version lives in `[workspace.package]` in the root `Cargo.toml` (all crates
inherit it) and `CHANGELOG.md` records each release — don't restate the number
here (it goes stale). `ARCHITECTURE.md` is the design source of truth — the
architecture and the numbered design decisions; `CHANGELOG.md` summarizes what
exists; `crates/areev-bench/RESULTS.md` has the benchmark numbers.

## Commands

```bash
cargo test --workspace            # full suite (2,100+ tests, ~1 min)
cargo test -p areev-cal          # per-crate
cargo run --release -p areev-store --example bench       # latency gates
cargo run --release -p areev-store --example voice_loop  # 50ms-cadence gate
cargo run -p areev -- recall --db demo.db --ns caller --subject john

python3 scripts/check_versions.py   # all five version sites agree
python3 scripts/repo_stats.py       # regenerate the README quality metrics
python3 scripts/repo_stats.py --check   # what CI asserts (2% drift tolerance)
cargo llvm-cov --workspace --lcov --output-path lcov.info   # then:
python3 scripts/coverage.py --lcov lcov.info             # regenerate docs/coverage.json
python3 scripts/coverage.py --lcov lcov.info --check      # what CI asserts (2-point tolerance)

scripts/build_demo.sh               # rebuild data/demo.db (the README's demo memory)
node scripts/shoot_console.mjs http://127.0.0.1:7461 demo/screens   # re-shoot its screenshots
```

- **Do not run blanket `cargo fmt`** — the tree is not uniformly rustfmt-clean
  (~177 files differ). Match surrounding style; format only
  the lines you touch.
- If CLI/MCP smoke tests fail with "spawn areev: No such file or directory":
  the cached test binary has a stale absolute path baked in via
  `CARGO_BIN_EXE_areev` (happens after the repo folder moves/renames).
  Fix: `touch crates/areev-cli/tests/*.rs` and re-run.
- CI (`.github/workflows/ci.yml`): test on ubuntu/macos/windows, clippy
  (`-D warnings`), MSRV build, `cargo doc`, coverage, Python (maturin + pytest),
  and Node (napi build + `node --test`), plus `versions` (every version site
  agrees) and `stats` (the README quality figures still match the tree).
  `security.yml` runs `cargo deny`. Still run tests locally before pushing.
- **The README quotes generated numbers.** `scripts/repo_stats.py` emits
  `docs/repo-stats.{json,md,html}` and four SVGs the README embeds (the
  quality chart + the per-crate coverage chart, light/dark each); the
  `stats` job fails the build if they drift >2% from the tree. Line coverage is
  a separate input: `scripts/coverage.py` turns the `coverage` job's LCOV trace
  into `docs/coverage.json`, which `repo_stats.py` renders into the same charts.
  The README's latency chart is separate: `scripts/bench_chart.py` renders
  `docs/assets/bench-latency-{light,dark}.svg` from numbers quoted out of
  `crates/areev-bench/RESULTS.md` — re-run it by hand when RESULTS.md changes
  (measurements are not tree-derivable, so it is not CI-checked).
  Coverage scores **source lines only** (no `tests/`, no `benches/`, no
  `#[cfg(test)]` blocks) and excludes what that job cannot execute
  (`areev-py` — pytest's job; `areev-bench` — explicit tools; `store/src/pg.rs`
  — needs `DATABASE_URL`), each exclusion carrying its reason in the JSON.
  Enforcement is **per crate** (`FLOORS` in `coverage.py`) plus a global floor,
  NOT one workspace target — a single number lets one crate's regression hide
  behind another's gain. Floors are a regression ratchet ~2 points under
  measured; raise one when real work lands, and lower one only with a reason.
  `areev-cli` and `areev-mcp` are the known-lowest and the next testing work.
  If CI flags either script, run it and commit the result — do not hand-edit
  the artifacts.
- **The version lives in five places**, only one of which is inherited. The
  `versions` job runs `scripts/check_versions.py`; the release runbook
  (`.claude/skills/areev-release`) is the source of truth for the order.

## Workspace (dependency order)

```
memory stack:  areev-core ← areev-store ← areev-cal ← areev-context ┐
loop engine: areev-loop ← areev-loop-adapter (adapter) · areev-llm (providers) ┤
runtime:     areev-run-core (pure scheduler) ← areev-run (driver)     ┤
triggers:    areev-trigger (evaluator; starts runs via areev-run)     ┤
                              all feed the leaf crates ↓                  ↓
             areev-mcp, areev-server, areev-py, areev (binary), areev-bench
```

| Crate | What | CLAUDE.md |
|---|---|---|
| `areev-core` | `.mg` format, canonical serialization, content addressing, 12 grain types, tool-schema rendering, the `anon` pseudonymization engine (Tier-0 detectors, policy, session tokens, keyed derivations) | yes |
| `areev-store` | The store: dictionary-encoded triples, hybrid recall, namespace prefix scoping (`"org.*"` on every plural read, backed by the `ns_reg` registry; writes/destruction stay exact-namespace), heads/forks, bundles, CAS blobs (encrypted under an HKDF-derived subkey when the memory is), DSAR `subject_report`, declarative `retention:<ns>` policies, memory-tool adapter, migration importers. Backend-agnostic logic over an internal `Db` seam — embedded Turso (default) or PostgreSQL (`feature = "postgres"`, one memory = one schema, concurrent writers serialized at `reserve_write` — the advisory lock is bootstrap-only, pgvector) | yes |
| `areev-conformance` | Backend-parameterized conformance suite (`publish = false`) — one case list (forks, replication, tombstones, PITR, BM25, vectors, CAS, CAL smoke) run against BOTH backends; the Pg runner needs `DATABASE_URL`/`AREEV_PG_URL` and hard-fails when `CI=true` without one | — |
| `areev-cal` | CAL lexer/parser/executor, ASSEMBLE, `AreevFacade` + mounts, and `render` — THE per-grain renderer (sml/markdown/text/toon/json + summaries + the one token estimator) every surface shares | yes |
| `areev-context` | Budget-aware orchestration over `areev_cal::render`: policies/presets, priority + diversity allocation with progressive disclosure (Full→Summary→Omit), timeline/census modes. Renders nothing itself — parity with CAL is test-pinned (`tests/render_parity.rs`) | yes |
| `areev-loop` | Substrate-agnostic self-improvement engine: `OmsSubstrate`/`LlmBackend` traits (+ the §7.4 capability-gated blob seam), 13 analyzers (11 default-on; `retention_sweep` and `goal_stagnation` are default-off, `run_outcome` reads run journals), four gates, recommendation lifecycle with Rule E1 (`code_revision` pins its evalset; apply only through the recorded gating edge), LLM DISCOVER→GROUND→VERIFY verifier, outcome measurement (no Areev deps) — `docs/loop.md` | — |
| `areev-loop-adapter` | Areev substrate adapter for Areev Loop (`areev_loop::OmsSubstrate` over `AreevFacade`) + recall-telemetry sidecar | — |
| `areev-llm` | Out-of-box LLM backends (OpenAI-compatible/Anthropic/Ollama over a small blocking HTTP client) for Areev Loop, the `remember()` free-text→Fact extraction (`extract.rs`), and the `ToolCallLlm` tool-calling seam (`toolcall.rs`) for the runtime | — |
| `areev-run-core` | The PURE `areev run` scheduler: sans-IO `step(env, state, events) → (commands, state)`, frozen condition grammar, plan validation (Tarjan + cycle bounds), re-entry generations, `RUN-Ennn` errors. No clock/rand/IO in its dep tree — CI-enforced | yes |
| `areev-run` | The `areev run` driver (a host, peer of areev-mcp): journal (intent=Pending Tool grain, result=supersession re-stating identity), checkpoints, resume with same-key crash redelivery, HITL respond with separation of duties, budgets, cancel, journal-consistent `verify`; plus the two host-boundary controls shared with `areev-trigger` — the content-addressed code executor (`executor_uri` behind a host-side pin) and the credential broker + outbound allowlist | yes |
| `areev-trigger` | Standing rules that start workflows: the `Trigger` grain's eight kinds over four primitives, the one-shot `trigger run` evaluator (claim, cursor, backoff, catch-up), the connector contract (same JSON-on-stdio shape as `--tool-cmd`), composite gates with correlation windows, and heartbeat rendering (the egress broker moved to `areev-run` so tools could share it). No daemon: cadence is data, evaluation is a command — `docs/triggers.md` | — |
| `areev-mcp` | Stdio MCP server (see below) | — |
| `areev-server` | Web console + areevd hub (see below) | — |
| `areev` | The `areev` binary (see below) | — |
| `areev-py` | PyO3 bindings (see below) | — |
| `areev-bench` | Reproducible benchmark harnesses (latency, honesty, LoCoMo accuracy) | — |
| `areev-js` | Node (napi) bindings — **standalone package, not a workspace member** (see below) | — |

## Cross-cutting invariants

1. **Grains are immutable and content-addressed** (SHA-256 over the whole
   `.mg` blob). Nothing ever edits a stored blob; every edit is a
   supersession, every removal a tombstone (`forget`) or crypto-erasure.
   Store code mutates the *index layer* only.
2. **Canonical serialization is frozen** (NFC, sorted keys, compact keys,
   omit-defaults). Changing it silently changes every content address and
   breaks OMS conformance — see `crates/areev-core/CLAUDE.md`.
3. **CAL destruction is authorization-gated, not structural** (CAL 1.3,
   [`docs/cal-all-you-need-proposal.md`](docs/cal-all-you-need-proposal.md)).
   The destructive statements are `FORGET <hash>` (single-grain tombstone,
   `delete` verb), `FORGET SUBJECT "<id>" [WITH text_mentions]` (identity
   erasure, `erase` verb), and `PURGE OLDER THAN <n><d|h|m> [TYPE t]`
   (retention sweep, `erase` verb) — BECAUSE mandatory on the latter two,
   optional-but-recorded on the hash form; every execution writes a Tier-2
   audit Observation in `agent:authz`. Grants live in the file as
   `mg:permits` Facts (`areev_core::authz`); the session's `AuthzSet`
   decides, and `CalExecutorConfig::allow_destructive_ops` remains a
   process-wide restrictive **cap** over any grant (`--no-destructive-ops`).
   Destruction takes a hash, an identity, or an age — never a predicate:
   `DELETE`/`ERASE`/`TRUNCATE`/… stay lexer-blocked non-tokens, `FORGET
   USER/SCOPE` stay text-refused, `DROP` accepts only TEMPLATE/QUERY, and
   saved-query bodies stay read-only. Statement classification has ONE
   source of truth (`areev_cal::classify`, exhaustive, no wildcard).
   [`docs/erasure.md`](docs/erasure.md) records the erasure requirements;
   its former "out of CAL" deviation is retired by CAL 1.3. Audit records
   name a subject **fingerprint** (`authz::subject_fingerprint`), never the
   identity — an immutable, replicating grain naming the erased subject
   would undo the erasure it records. The read-only mirror is `REPORT
   SUBJECT` (classifies `Read`, `read`-gated, not behind the destructive
   cap): the report and the erasure share ONE selector, so a DSAR discloses
   exactly what an erasure removes. [`docs/gdpr.md`](docs/gdpr.md) is the
   article→capability map.
4. **CAL syntax is an OMS conformance contract** — no new CAL syntax
   without a spec-level decision.
5. **One memory = one isolation unit** — a file on the embedded backend, a
   Postgres schema on the `postgres` backend; either way it is the unit of
   erasure, sync, and portability. Write concurrency is backend-specific:
   the embedded backend is single-writer, enforced by a process-wide
   open-path registry so a second handle on one file fails at open
   (`STO-E002`) instead of silently drifting its cached allocators and
   corrupting the first handle's writes; the `postgres` backend admits
   multiple concurrent writers per schema (`STO-E002` is never raised
   there), serialized at `reserve_write`'s row lock — the advisory lock
   covers schema bootstrap only, not writes (`docs/deployment-profile.md`).
   Rust/Python release on drop; Node calls `close()`. Adding a grain that is
   already stored is a no-op returning the existing hash, not an error.
   Cross-memory queries go through
   ASSEMBLE with facade mounts, not shared connections. Files are
   self-describing: the `meta` table carries file-truths (`text_index`,
   `entity_relations`, embedding provenance) and CAL host metadata — saved
   queries and custom templates ride there as `qry:<name>`/`tpl:<name>` rows,
   so they travel with the file rather than living in any one client, and
   they **replicate**: bundles/segments carry the registry (+ retention
   policies) in a v2 `MGB2` meta segment — latest-wins on import,
   `last_run_at` stays local, PITR skips it (infrastructure truths like
   `text_index` deliberately do not replicate). Bare `open()` honors them;
   `open_with()` deliberately re-stamps and reports changes via
   `open_warnings()`. Host config (embedder capability, executor limits) is
   per-process and never persisted in the file.
6. **Dependency-light by policy**: no clap (hand-rolled args), no HTTP
   framework (std `TcpListener`), no MCP SDK (hand-rolled JSON-RPC), no
   workspace-wide async runtime (store wraps a private tokio current-thread
   runtime behind a sync API). Think twice before adding a dependency.

## Docs contract — specs update with the change, same commit

Every public surface has a canonical doc, and a change to the surface is
incomplete until that doc moved with it. This applies to every feature
family (`areev run`, `areev loop`, `areev anonymize`, hub/console, …):

| You changed… | You must also update… |
|---|---|
| A CLI verb or global flag | the in-binary `USAGE` help; `docs/cookbook.md` when it's a user-facing task |
| An MCP tool (add/remove/shape) | `docs/mcp-reference.md` — including the **pinned tool count** in its prose/headings |
| CAL behavior or the result payload | `docs/cal-reference.md` (its ```sql fences are **executable** — `docs_examples.rs` fails CI on a non-parsing example); new *syntax* additionally needs the OMS spec decision |
| An error code | `ERROR_CODES.md` (append-only) |
| Store semantics (recall/erasure/replication/meta) | the crate's `CLAUDE.md` **and** a `areev-conformance` case — both backends |
| Auth, crypto, keys, bind, request parsing | `docs/security-model.md` |
| A subsystem with its own reference doc | that doc (`docs/run.md`, `docs/loop.md`, `docs/erasure.md`, `docs/gdpr.md`, …) |
| An architecture-level decision or new cross-cutting subsystem | `ARCHITECTURE.md` §10 as a **named** decision |
| Python/Node binding methods | keep both in lockstep + regenerate `areev-js/index.d.ts` (napi build) |
| A release | `CHANGELOG.md` (the release runbook owns this) |

The failure mode this prevents is real: the anonymization feature shipped
three phases before `security-model.md`, `ARCHITECTURE.md`, and
`cal-reference.md` caught up. Sweep this table before every commit that
touches a public surface (the `areev-invariants` gate includes it).

## Error codes

Every user-facing error carries a stable `DOMAIN-Ennn` code (3-letter
uppercase domain, `-E`, digits) as the **leading token of its `Display`
string**, plus a `code()` method. Domains: `FMT` (.mg format), `MEM`
(grains + tool-schema binding), `STO` (Turso store), `CRY` (crypto), `VAL`
(input validation), `CAL` (query language), `SYS` (internal). A reported code
alone locates the variant and subsystem. **Codes are append-only** — never
renumber or reuse one. Source of truth for text is inline on `AreevError`
(`areev-core/src/error.rs`), `SchemaSubsetError`, and `CalError`
(`areev-cal/src/errors.rs`); the full registry + the rule for adding one is
[`ERROR_CODES.md`](ERROR_CODES.md). Format/uniqueness are test-enforced
(`error_code_tests`, `test_all_error_codes_have_unique_codes`).

## Smaller crates

- **areev-mcp**: 25 tools (`areev_recall/search/nearest/add/supersede/forget/remember/cal`,
  the trajectory logger `areev_record_tool_call`,
  the DSAR read `areev_subject_report`,
  the graph/time reads `areev_related/entity_at/step_actions`, the
  run<->memory join `areev_run_trace/runs_touching`, the §7.4 forensics
  `areev_tool_provenance`, the loop pair `areev_loop/recommendations`,
  the runtime six `areev_run_start/resume/respond/cancel/verify/list`,
  and `areev_run_manifest` (persists a reproducible run config)
  — host tools execute only via `$AREEV_RUN_TOOL_CMD`, respond REQUIRES a
  `responder` principal. `--profile memory|full` (default full) narrows
  `tools/list`+`tools/call` to the 12-tool read/write/query set, dropping
  the 13-tool workflow-runtime family, for hosts that only want chat memory)
  over newline-delimited JSON-RPC 2.0 on stdio, protocol rev `2025-06-18`.
  Convention: tool failures are `isError: true` *results*; only protocol
  errors are JSON-RPC errors. Notifications (no id) get no response. No
  in-crate tests — exercised by `areev-cli/tests/mcp_smoke.rs`, which drives
  the real binary over real stdio.
- **areev-server**: hand-rolled std-only HTTP/1.1, one request per
  connection. `ui` console binds loopback and is **unauthenticated by
  default**; `with_auth(token)` (CLI `areev ui --token-env VAR`) requires the
  token on **every** request — browsers via the native HTTP Basic prompt (any
  username, password = token), scripts via `Authorization: Bearer` — and a 401
  carries `WWW-Authenticate: Basic` so browsers prompt. `into_hub(token, dir)`
  is the separate hub mode (CLI `areev hub`, where `--token-env` is
  **mandatory**): bearer auth on POSTs + the `/api/segment*` surface, which is
  gated on **reads too** — only the non-segment reads are open. Both `ui` and
  `hub` can also terminate TLS natively (`--tls-cert`/`--tls-key`, the
  non-default `tls` build feature, rustls) — the documented default remains a
  TLS-terminating reverse proxy in front of the loopback bind
  (`docs/deployment-profile.md`); native TLS is for deployments with nowhere
  to run one. Base64 for Basic is hand-rolled (no dep). Body cap 1 MiB.
  Cross-origin POSTs are rejected via Origin check (drive-by protection).
  The console is one embedded HTML file (`console.html`, vanilla JS, no
  build step) — a plain-language memory browser with an interactive graph,
  an Analytics tab
  (grain-type census across all 12 types, namespace breakdown, a growth
  trend, and recall-leg status — the Query page's "WHAT'S IN THIS MEMORY"
  on-ramp, generalized), the loop review queue, and a Developer-mode toggle
  that reveals hashes/op-log/CAL; design source of truth is the Paper file
  "Areev", page "Console v2 — Redesign".
  The operations pages are three, not four: **Workflows** (the plan canvas —
  which also draws the Trigger grains pointing at the open plan, read-only in
  a "STARTED BY" lane, plus a per-step status rail from a selected run),
  **Runs** (executions, grouped waiting/in-flight/finished), and **Tools**
  (definitions + their configuration, and every execution grain). Triggers
  have NO page: the binding is trigger → plan, so a flat list cannot show that
  two triggers start one plan — `#triggers` redirects to Workflows. The canvas
  overlay and the Runs cards read ONE shared run index (`loadRuns`), and
  trigger nodes live outside `WF_DRAFT` so the Save path structurally cannot
  serialize them.
  Read-only `GET /api/config` reports effective config + file-vs-host
  reconciliation warnings.
  `tests/multichannel_tests.rs` is the §8 acceptance test (voice + WhatsApp +
  email sharing one memory via the hub). The `/api/run/*` surface (list /
  inspect / respond / cancel) + the console Runs tab are the runtime's HITL
  queue: **`run.respond` refuses shared-token and anonymous callers** — only
  a per-principal credential (`areev ui --auth`) may approve, because the
  approver's identity IS the audit record; cancel keeps the low bar.
- **areev**: ~29 verbs (incl. `hub`, `migrate` from other memory systems,
  `reindex`, the graph/time reads `related`/`entity-at`/`step-actions`, the
  join `run-trace`/`runs-touching`, and the DSAR read `subject-report`),
  hand-rolled `parse_args` → HashMap; global `--embed-cmd` installs
  a `CommandEmbed` for vector recall on any verb. Opens honor
  the file's meta declarations; `--index-text true|false` explicitly
  re-stamps; open warnings print to stderr.
  `hook claude-code` only *prints* the settings snippet (never writes user
  config); `capture-stop` reads Claude Code hook JSON from stdin and stores
  the last exchange as thread-indexed Events.
- **areev-py**: `#[pyclass] Areev` over `AreevFacade`. FFI convention:
  **scalars in, JSON strings out**; errors → `PyValueError`. abi3-py39
  cdylib; build with maturin (`build.rs` handles macOS
  `-undefined dynamic_lookup` for bare cargo builds).
- **areev-js**: `#[napi]` methods over `AreevFacade`. Same **scalars in, JSON
  strings out** convention as `areev-py`; native Node addon via napi-rs (not
  wasm). Standalone package — **not** a `cargo` workspace member, so
  `cargo test --workspace` skips it; CI's `node` job builds it with
  `napi build --release` and runs `node --test __test__/smoke.mjs`.

## The demo memory + README screenshots

`data/demo.db` is **committed** (the one `.db` exception in `.gitignore`) and
is what every README screenshot is taken from: one accounts-payable story —
knowledge, nine governed runs, a real open fork, a declared trigger, and
thirteen recommendations `areev loop run` genuinely computed. Rebuild it with
`scripts/build_demo.sh` (seeder → saved queries → fork → trigger → runs →
loop → WAL checkpoint), then re-shoot with `scripts/shoot_console.mjs`
against a running `areev ui`. **A console change means re-shooting** — the
README quotes the UI, so a stale screenshot is a stale claim. The seeder's
`--plan-only` mode is on a fixed clock so the example's committed `plan.mgb`
keeps its content address.

## Local artifacts (gitignored, don't commit or rely on)

`demo.db*` and `*.db/-wal/.blobs` (scratch memories), `m0-data/` (spike
outputs), `name-reservation/` (registry placeholder stubs), `target/`.

## Naming

Brand "Areev", CLI binary `areev` (package/crate `areev`), hub daemon
"areevd", Python module `areev`, npm packages `@areev/areev` +
`@areev/areev-<platform>` (unscoped `areev`/`areev-<platform>` is the intent
once npm's similarity-filter exception lands — see Status above — not yet
in effect; don't call the scoped names "deprecated" until it does). The OMS
spec itself is external (CC0); OMS
conformance is the compatibility mechanism with other implementations.
