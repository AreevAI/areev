# Changelog

All notable changes to Areev are documented here. The format is based on
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project
adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- **Decision backends: typed, calibrated judgments as an optional seam**
  (`docs/decision-model-proposal.md`). `areev_llm::decide` takes a `state`
  plus named `noul` / `choice` / `score` questions and returns typed answers
  with probabilities, `provider`, `model`, `calibrated` and latency. One
  wire shape (`POST /v1/systemone`), many providers: `typesafe:`,
  `openrouter:`, `vercel:`, `cloudflare:`, `openjev:` (development only),
  self-hosted `systemone:<url>[#model]`, the LLM-emulated `llm:<spec>`
  (`calibrated = false`) and a JSON-on-stdio `--decide-cmd`. The host orders
  them as a **chain** that fails open to the deterministic rule under one
  deadline (default 2000 ms). Nothing is default-on; with no chain
  configured every path is unchanged. Rules recorded in ARCHITECTURE.md §10:
  a decision model may score and order, only code omits/gates/approves/
  applies; an uncalibrated backend may reorder, never omit. New error domain
  **`DEC`** (`DEC-E001`…`DEC-E008`; `E008` is the fail-closed egress refusal
  that stops a chain). The pure seam lives in `areev_core::decide`, the
  adapters in `areev_llm::decide`. No new dependency.
- **Decision surfaces.** Global flags `--decide`, `--decide-cmd`,
  `--decide-timeout-ms`; the verb `areev decide --state … (--questions |
  --noul | --choice … --option k=desc | --score … --level desc)`; MCP reads
  `AREEV_DECIDE` / `AREEV_DECIDE_CMD` / `AREEV_DECIDE_TIMEOUT_MS` (tool count
  unchanged); `set_decider` / `decide` (Python) and `setDecider` / `decide`
  (Node); `GET /api/config` reports `decide: { chain, calibrated }`.
- **Real recall scores.** `SearchHit.score` carries the fused score
  (rank-normalized RRF, top = 1.0, or the reranker's score normalized to
  [0,1]) instead of a constant, and MCP `areev_search` rows gain `score` (and
  `provider` when a reranker answered), so `min_score` filters on something.
- **Command reranker.** `CommandRerank` (stdin `{"query", "docs"}` → stdout
  an array of scores, mirroring `CommandEmbed`) via `--rerank-cmd` /
  `AREEV_RERANK_CMD`, `set_reranker_command` / `setRerankerCommand`.
- **Recall deadline.** `--recall-deadline-ms` / `AREEV_RECALL_DEADLINE_MS`,
  `set_recall_deadline_ms` / `setRecallDeadlineMs` and the facade's
  `set_recall_deadline` thread a deadline into hybrid recall; a reranker
  that misses it fails open to the RRF order.
- **`DecisionRerank`.** With `--decide` set, recall installs the decision
  chain as its reranker (batched `score` questions, in-process LRU keyed by
  content hash — nothing persisted in the memory file). Measured on full
  LoCoMo retrieval (`crates/areev-bench/RESULTS.md` §9): hit@1 18.6% →
  51.8%, MRR@10 0.250 → 0.567, against an oracle ceiling of 66.3%; one run
  cost $0.61. An egress policy wraps the chain in `PseudonymizingDecider`
  on every surface; `areev-bench` gains `decide_calibrate` (ECE / Brier /
  threshold bands over a labeled JSONL) and `accuracy` gains `AREEV_DECIDE`
  plus a `--rerank-oracle` positive control.
- **Decision-guided context assembly** (`areev-context`, CAL `ASSEMBLE`).
  With a decider installed, one request per assembly asks the query's
  intent (`timeline` / `current_state` / `general`, replacing the keyword
  lists) and, per candidate, a relevance `score` and a "would a summary lose
  a needed detail" `noul`. Only a **calibrated** answer may omit
  (relevance below 0.10) or prefer Full (verbatim ≥ 0.50, still under the
  95% budget line); an uncalibrated one only reorders; any failure is the
  old allocation byte for byte. `FormattedContext.decision` carries the
  provenance; `FormatPolicy.decide` tunes the thresholds; ASSEMBLE's
  calibrated drops warn `CAL-W019`. The Claude Code `recall-hook` uses it
  under its 1500 ms deadline. Rendering is unchanged (`render_parity` pins
  it).
- **Decisions in a run** (`docs/run.md`, "Decisions in a run").
  A Tool Definition whose `executor_uri` is the reserved `areev://decide` is
  a **decision node**: the driver answers it through the host's chain,
  journals the answer as an ordinary Tool execution grain, and edges branch
  on it in the frozen condition grammar (`triage.answers.route.choice ==
  "escalate"`); a host with no backend refuses at start with **`RUN-E030`**.
  Two scheduler-internal decisions, both calibrated-only and journaled as
  `mg:decide` effects: the **decision-guided fold** prunes a transcript
  (keep / truncate / drop per entry, results never separated from calls,
  index-layer only) before the summarizer fold, and **tool-offer narrowing**
  keeps the top 8 plus anything at p ≥ 0.05 when more than 8 tools are
  pinned. The run pins `decider: {describe, calibrated}` in its manifest so
  `resume` and `verify` replay without asking. `Runner::with_decider` is
  the host call; `run start`/`resume` and `trigger run` wire `--decide`.
  Packs may carry a decision node: it needs no `--allow-executor` pin and
  raises no warning (`docs/pack.md`). `docs/triggers.md` shows a polling
  trigger judging items through one, with the Trigger grain unchanged.
- **Decisions in the loop** (`docs/loop.md`, "Decision backend (optional)").
  `areev loop run --decide …` (or `$AREEV_DECIDE`) gives the engine a
  calibrated judge, through the loop's own dependency-free
  `areev_loop::DecideBackend` trait and the `areev_loop_adapter::LoopDecider`
  bridge: GROUND and VERIFY take their 0.75 routing number from a `noul`
  over the draft and its evidence (the LLM's self-report is kept beside it
  as `llm_confidence`); the duplicate sweep asks "same claim?" for pairs
  between Jaccard 0.5 and 0.9; the contradiction sweep asks "can both be
  true?" on relations outside the seeded functional set; a free-text tool
  failure cause is classified into the closed enum. Everything it touches
  is a draft carrying `judged_by` provenance, **never auto-applied**, and an
  uncalibrated backend is never asked. The run report gains `decider`
  (backend, calibrated, calls, failures); new `LOP-E051` (never fatal).
- **PostgreSQL paired layout: engine metadata in its own schema** (#353).
  `?meta_schema=<name>` on the DSN puts a memory's engine metadata — the
  `meta` table (declarations, stamps, saved queries/templates, retention and
  anonymization policies, the vault, legal holds, trigger leases),
  `counters`, `ns_reg` and the telemetry sidecar's `telem_*` — in a second,
  physically separate schema, while the grains, every index over them, the
  `terms` dictionary, the `oplog` and the CAS `blobs` stay in the memory
  schema. One routing table (`pg::PgLayout` over `pg::META_TABLES`) drives
  every statement, DDL included, so a write spanning both schemas is still one
  transaction, and CAL/Run/Loop semantics, content addresses and grants are
  unchanged. Reaches every host through the DSN — CLI, console, both
  bindings, bench — with no new parameter; `areev provision` gains
  `--meta-schema` and `provision --check` reports the pair;
  `drop_postgres_schema` drops both; `provision=never` and `--read-only`
  work on a pair exactly as on a single schema. An open whose DSN disagrees
  with the memory's existing layout — in either direction — is refused with
  the new **`STO-E011`** before any DDL: moving to the pair is an explicit
  `ALTER TABLE … SET SCHEMA` step, documented in
  `docs/deployment-profile.md` ("Paired layout") with the classification
  table and the role grants. Opt-in; without the parameter the single-schema
  layout is byte-for-byte what it was, and no schema version moves.
  Conformance: the whole Postgres case list runs a second time under the
  paired layout (`tests/pg_paired.rs`), plus introspection proving no
  classified metadata is left in the memory schema, two pairs in one
  database sharing no registry or counters, concurrent writers on a pair, a
  least-privilege role granted on both schemas (and refused once revoked on
  either), and the layout-mismatch refusals.

### Fixed

- **The egress broker gives back the memory it was lent when it is dropped,
  not when its thread exits.** `Broker::drop` signals its detached accept
  thread and returns; the thread noticed the flag on its next 5 ms poll and
  only then dropped its clone of the store bound by `bind_artifact_store`.
  An embedded memory's file lock is process-wide and lives as long as any
  handle does, so a host that closed its handle and immediately handed the
  file to another process — the Node binding's `close()` followed by a CLI
  run on the same file — could find it still locked (`STO-E001`, "locked by
  another process"), which is how the binding's egress parity test failed on
  a slow macOS runner. The drop now takes the store out of the shared slot
  itself; an upload in flight keeps its own clone until it lands. Pinned by
  `dropping_the_broker_releases_the_bound_store_synchronously`.

## [1.9.5] — 2026-09-24

### Fixed

- **With an egress policy live, only what a model produced is rehydrated**
  (#350). `areev run` used to run placeholder rehydration over the input of
  every dispatch, so a host or code tool whose input carried a
  `[PERSON_1]`-shaped marker no model produced failed with `the model
  produced placeholder this run cannot resolve` — every run of a
  model-free pseudonymizing pipeline failed once the floor was on — and a
  marker that happened to match an older mapping key was silently rewritten
  to that mapping's value. Rehydration (and its fail-closed refusal) now
  covers only placeholders emitted by a model turn of the run (or a
  subgraph's result); anything else is dispatched verbatim. An LLM turn's own
  input is no longer rehydrated, so a refused tool call no longer also fails
  the model's next turn.
- **Run configuration is frozen from the stored grain, not through the egress
  rewrite** (#350). The manifest resolved bound Tool Definitions with the
  egress-bounded `get`, so a `wasm32-areev-io` Definition declaring
  `hosts: ["http://127.0.0.1:7792"]` was pinned as `http://[IPV4_1]:7792` and
  its first brokered call refused with `RUN-E022`. The manifest (bindings,
  stored config, input by reference), the shadow argument-schema load and the
  trigger connector Definition now read through the new `Areev::get_stored`.

## [1.9.4] — 2026-09-23

### Added

- **Host-initiated resumable run pause** (#344). `runPause(runId, because)`
  (Node) / `run_pause` (Python), `areev run pause --run-id … --because …`
  and the MCP tool `areev_run_pause` (27 tools; 15 in the runtime family)
  ask a live run to stop at its next superstep boundary. The run parks with
  reason `paused` (the checkpoint an uninterrupted run would have written,
  holding no concurrency slot), `runInspect` reports phase `paused` with who
  paused it, when and why, and the event stream ends with `RunPaused`.
  `runResume` continues it under the SAME run id, manifest and pins, no node
  re-executes, and `verify` passes across the pause. A repeat pause is
  idempotent (`already: true`); cancel wins over pause and finalises a paused
  run as canceled. Pause takes `run.execute`, the grant resume takes. Pausing
  a finished run, or one with a cancel pending, is refused with the new
  **`RUN-E029`**. Records are `mg:run_pause` / `mg:run_paused` /
  `mg:run_unpause` Facts in the run's namespace (ARCHITECTURE.md §10).
- **Built-in Indian tax-identifier detectors** (#347). Tier-0 categories
  `in_gstin` (15-char GSTIN, state code 01–38/97/99, mod-36 check character;
  validated) and `in_pan` (10-char PAN with holder-type letter,
  cue-gated on `PAN` / `PAN No` / `Permanent Account Number` /
  `Income Tax PAN`). A PAN inside a valid GSTIN is covered by the GSTIN
  detection. Conformance-tested on both backends.
- **`recommendation(hash)` on both bindings** (#348) returns the object
  `areev loop show` prints, proposal body included (`cal` / `edit` /
  `data`), so a host can measure a proposal before approving it; a hash
  prefix resolves. `recommendations('{"include":"proposal"}')` adds
  `action_kind` and the flattened proposal to each row; the default row is
  unchanged. Both are coverage-filtered exactly like the listing (#312).
  `areev loop show` and the bindings now share one builder
  (`areev_loop_adapter::recommendation_detail`), and `show` gains
  `action_kind`.
- **`loopRun` gateway arguments** (#346). Node `loopRun(…, baseUrl, keyEnv)`
  (appended, so positional callers are unchanged) / Python
  `loop_run(base_url=, key_env=)` resolve the reflection AND grounding
  models against a gateway with a key named by variable, matching the CLI's
  `--llm-base-url` / `--llm-api-key-env` and `runStart`'s pair.
- **`anonymizeEgressFloor()` / `anonymize_egress_floor()`** (#345) read
  the egress floor back.

### Changed

- **Raising the egress anonymization floor needs no grant** (#345).
  `setAnonymizeEgressFloor(true)` used to demand `admin` on `*`, so the
  least-privilege principal-bound handle that most needs the floor could not
  set it. Raising only strengthens protection and is now open to any handle.
  Lowering it keeps the `admin` check (`AUT-E001`), so a bound agent can turn
  the floor on but never off. The rule lives in
  `AreevFacade::set_anonymize_egress_floor`, shared by both bindings
  (docs/security-model.md, docs/compliance-profiles.md).

## [1.9.3] — 2026-09-23

### Added

- **Brokered artifacts above 1 MiB** (#339). A Tool may now declare
  `runtime_limits.max_response_bytes` (artifact-mode downloads) and the new
  `runtime_limits.max_request_bytes` (`body_ref` uploads) up to a documented
  **32 MiB hard maximum**; undeclared transfers stay bounded at 1 MiB. A
  zero, non-integer or over-maximum declaration is refused, never clamped: a
  `VAL` error on write, the new **`RUN-E028`** at run start (before any
  upstream I/O), and again at broker registration. Over-limit responses are
  refused at limit + 1 under every framing (Content-Length, chunked,
  read-to-close) and the refusal names the *effective* ceiling; a truncated
  transport fails instead of storing short bytes; an oversized `body_ref` is
  refused by its stored size (`Areev::blob_len`, which reads no body) before
  a byte is loaded or sent upstream. Text mode and existing declarations are
  unchanged. The blessed `http.call` blob is unchanged (same address).
- **`op: recall` — a third typed in-run read** (#342). A plan's `reads` may
  recall a subject's grains (`ns`, `subject`/`subject_from`, optional literal
  `relation`, `k` 1–64 default 16, optional `at`/`at_from` + `axis` for an
  as-of recall, `into`). The ceiling is enforced by the runtime (refused at
  start past 64, truncated to `k` at execution); journaled as `mg:recall`
  with resolved operands and result hashes, replay-verified by `verify` and
  `shadow`. The as-of form is `entity_at` per relation (`Areev::recall_at`),
  conformance-tested on both backends. `op: saved_query` was declined —
  ARCHITECTURE.md §10, "In-run recall is a third typed read; saved queries
  are not".
- **Pack validate/install from Node and Python** (#341). Node
  `packValidate(dir)` / `areev.packInstall(dir, options)`; Python
  `areev.pack_validate(dir)` / `Areev.pack_install(dir, *, expected_hash,
  ns, executor_pins, dry_run)`. Both install under the handle's bound
  principal through `areev::pack::install_pack`, all-or-nothing, and carry
  the typed code (`err.code` / `areev.PackError.code`). Every example pack
  installs at the same plan hash the CLI prints.
- **Executor pins at install** (#341). `InstallOptions::executor_pins`, the
  bindings' `executorPins`/`executor_pins` and CLI `areev pack install --pin
  TOOL=ADDR,...` are *checked* against the pack's code-carrying tools (new
  **`PCK-E005`** refuses the whole install on a mismatch) and never written,
  so pins leave the plan hash and the op-log untouched. The pack report (and
  `--format json`) gains `executors: [{tool, file, executor_uri, pinned}]`;
  `validate` also reports `allow_executor`. `install` gains
  `--expected-hash` / `expected_hash` (`PCK-E002` on mismatch), and
  `InstallOptions.namespace` is now honoured.
- **A freestanding sandbox guest ABI** (#340). `docs/sandbox-abi.md`
  specifies the `wasm32-areev`/`-io` contract in WAT, independent of any Rust
  crate, with reference echo modules in **C, Zig and AssemblyScript** under
  `areev-tools/examples/` (byte-reproducible, rebuilt and checked in CI, run
  twice through the sandbox for fuel/output determinism, and through the
  `areev-sandbox` binary by `areev-conformance --features sandbox`). A
  refused import — WASI above all — now names the ABI document. The WASI
  shim and interpreter options were not taken; the page records why.

### Changed

- **Every plan read refuses a wildcard namespace (`"org.*"`) at run start**
  (#342). `entity_at` and `related` previously failed only when the node
  ran.
- **Rust API:** `CapabilityLimits` gains `max_request_bytes`; code building
  it without `..Default::default()` needs the field. A text-mode
  `max_response_bytes` above 32 MiB, formerly accepted, is now refused.
- `areev pack install` now runs through `areev::pack::install_pack`, so the
  CLI and the library share one code path.

### Security

- **`install_pack` checked permissions only after writing blobs and
  saved-query/template rows** (#341). A principal refused at the grain batch
  left the op-log unchanged but had already written CAS blobs and registry
  rows through an ungated path. Every permission is now checked before the
  first write: `write` on each grain's namespace and on the pack's namespace
  for blobs and new registry rows, `admin` on `*` to replace a different
  existing registry row or to install a bundle pack.

## [1.9.2] — 2026-09-22

### Fixed

- **Brokered HTTP tools now support bounded, byte-exact artifacts** (#336).
  Opt-in `response_mode: "artifact"` stores response bytes in the run's CAS
  and returns a reference, digest, byte count, and MIME type. `body_ref` sends
  stored bytes with an explicitly permitted content type. Text mode stays
  compatible; oversized or failed reads now refuse explicitly. The run journal
  records artifact metadata and provenance without embedding binary data or
  credentials, and older brokers reject unsupported binary modes.

### Changed

- **`CAL-E093` and `CAL-E121` carry the store's code as a value** (#331).
  `CalError::store_code()` returns the store's `DOMAIN-Ennn` (`STO-E009` legal
  hold, `STO-E002` busy, `AUT-E001` denied, …) and `None` for errors CAL raised
  itself; the console's `/api/cal` error payload adds it as `store_code` beside
  `code`, which also survives the sanitized `error` text. A host no longer
  finds `"STO-E009"` inside the message. Messages are unchanged.
- **A store refusal of `FORGET` / `FORGET SUBJECT` / `PURGE` is now an error**,
  not an `unsupported` payload (#331) — `CAL-E093`/`CAL-E121` with its
  `store_code`, as `docs/cal-reference.md` §9 already described. The console
  had read the old `ok: true` as success and toasted "Forgotten" for a
  hold-refused delete. `--no-destructive-ops` still returns `unsupported`.
- **Rust API:** `CalError::{StoreError, NotAuthorized, InvalidQuery,
  CryptoError}` gain a `store_code: Option<&'static str>` field. Code that
  constructs these variants, or destructures them without `..`, needs the
  field added.

## [1.9.1] — 2026-09-19

A **security release**, plus the four Rounic follow-ups raised against 1.9.0
and a legal-hold deadlock found while fixing one of them. The headline is
GHSA-rmrx-26f6-f97w: a handle bound to a principal restricted CAL and almost
nothing else. Hosts that pass `principal=` / `--as` to less-trusted code
should upgrade; a host that never binds a principal is unaffected by it.

### Security

- **A bound principal now binds every method, not only CAL**
  ([GHSA-rmrx-26f6-f97w](https://github.com/AreevAI/areev/security/advisories/GHSA-rmrx-26f6-f97w),
  CWE-862). A handle opened with `principal=` (Python), `principal` (Node) or
  `areev serve --mcp --as` is documented to fail closed (CAL 1.3 §9). It did so
  for CAL and for five methods; the rest reached the store through
  `AreevFacade::with_store`, which applies no authorization. A principal granted
  `read` on one namespace could therefore **read any namespace** through
  `recall`, `latest`, `search`, `history`, `related`, `entity_at`, `nearest`,
  the run-history reads and the memory-wide `changes_since` op-log; **write** an
  ungranted namespace through `remember()`; and **erase** in one through the
  memory tool's `delete` — while the equivalent CAL statement was correctly
  refused with `AUT-E001`. On MCP the same hole reached `areev_search`,
  `areev_related` and `areev_remember` under `--as`.

  Every namespace-scoped call on the three principal-bindable surfaces now goes
  through the new gated accessors (`AreevFacade::store_read` / `store_write` /
  `store_checked`), which check the verb against the session's *effective*
  rights — an active `PrincipalSession`'s when there is one — before the store
  is touched. Memory-wide operations take `Verb::Read`/`Verb::Admin` on `"*"`;
  calls that return per-namespace rows (`changes_since`, `provenance`, the
  anonymization listings) are filtered to what the principal may read; a walk
  over a namespace list (`related`, a scoped feed) is all-or-nothing; and the
  memory tool is gated per *command* (`view` → `read`, `create`/`str_replace`/
  `insert`/`rename` → `write`, `delete` → `delete`, anything unrecognized →
  `admin`). `set_embedder_command` and `set_anonymizer_command` now authorize
  *before* probing the command, so a refused caller cannot spawn a subprocess.

  **An unbound (owner) handle is unaffected** — `AuthzSet::owner` allows every
  verb everywhere — so a host that never binds a principal sees no change. Three
  test layers keep the hole closed: `areev-cal/tests/host_surface_gating.rs`
  fails the build on any new ungated `with_store` in the bindings or MCP unless
  the store method is listed with a reason; `crates/areev-py/tests/test_principal_gating.py`
  and `crates/areev-js/__test__/principal_gating.mjs` drive the whole public
  surface under a zero-grant principal and assert `AUT-E001`, with an
  owner-unaffected positive control; and `mcp_as_principal_binds_every_tool_not_only_cal`
  does the same over real stdio. Reported by Rounic verification of 1.9.0;
  Rounic is a Rust host on the gated facade and was not itself affected.

### Fixed

- **A CAL refusal now carries a code that says "refused"** (#321). `map_store_err`'s
  catch-all was `CAL-E030 BudgetExceeded`, so a `RECALL` refused for lack of a
  grant arrived as a *budget* error with the `AUT-E001` detail buried in the
  message — and a host routing on the code (a refusal to 404 + a `Denied` audit
  record, an overrun to a retry) had to match a substring to tell them apart.
  1.9.0 had already fixed this for `DERIVED FROM` (#304), leaving the recall
  path the odd one out against `docs/cal-reference.md`. Every `AUT-E…` variant
  now maps to `CAL-E121`, in the recall path and both `HISTORY … DIFF` arms.
  While there: the catch-all's name was wrong for most of what reached it, so
  the remainder is now **`CAL-E093 Store error`** carrying the store's own
  `DOMAIN-Ennn` — a legal hold (`STO-E009`), a read-only open (`STO-E004`), a
  busy store. Nothing mapped from the store was ever a resource overrun, so
  `CAL-E030` now means only what it says: CAL's own budget accounting.
- **A legal-hold refusal no longer deadlocks the process.** `cal_delete` and
  `cal_forget_user` locked the store inline in the scrutinee of an `if let` /
  `match`; Rust holds a scrutinee's temporaries for the whole construct, so the
  guard was still alive inside the refusal arm — and `audit_hold_refusal` locks
  the same non-reentrant `Mutex`. `FORGET <hash>` and `FORGET SUBJECT` under a
  hold therefore **hung forever** instead of refusing, which is exactly the path
  #278 added to make a deferral auditable. Found while adding #321's tests: no
  CAL- or CLI-level test had ever placed a hold, so the store-level conformance
  cases passed while the facade path was unreachable.
- **A blocking open or drop inside an async runtime no longer panics** (#322,
  first half). `Areev` drives its own current-thread Tokio runtime, so
  `Areev::open` on a runtime worker panicked from inside Tokio ("Cannot start a
  runtime from within a runtime") several frames below anything the caller
  wrote, naming no Areev API — and dropping a handle there panicked again in
  tokio's blocking shutdown, at process shutdown or in a test's drop. The open
  now returns **`STO-E010`** naming `AsyncAreev`, `AsyncFacade` and
  `spawn_blocking`; `TursoDb`'s `Drop` relocates its runtime to a plain thread.
  A blocking open inside `spawn_blocking` is unaffected — the check is the real
  `block_on`, caught, not a guess about the thread, because no public Tokio API
  distinguishes a blocking-pool thread from a worker.

### Added

- `AreevFacade::store_read` / `store_write` / `store_checked` / `store_as` —
  store access gated on a verb and a namespace, and `effective_authz()`, the
  session-aware rights read a host should use instead of `authz()` when it
  needs to check several namespaces itself.

- **`areev_cal::AsyncFacade`** — an async-safe owner for the GOVERNED facade
  (#322). `AsyncAreev` wraps the raw store only, so an async host that also
  needed authorization (`AreevFacade`, `PrincipalSession`, `set_grants`,
  `authz_epoch`, CAL under a session) hand-rolled one: open on a plain thread,
  every call through `spawn_blocking`, and a `Drop` releasing the last handle on
  a dedicated thread. `AsyncFacade::open(path, ns).await`,
  `.with(|facade| …).await`, `.with_mut(…)`, `.from_facade(f)` and
  `.close().await`. It takes a **closure** rather than mirroring each method
  because `PrincipalSession<'f>` borrows its facade and cannot cross an
  `.await`; inside the closure a whole request runs on one blocking thread.
  Clones share one facade and queue asynchronously rather than occupying
  blocking threads. `docs/deployment-profile.md` gains an "Async hosts" section.
- **`Areev::hold_records()`** and `HoldRecord::at_ms` (#323). `place_hold` has
  stored the placement time since #278; `holds()` dropped it on the way out, so
  a product listing legal holds kept a second copy of every hold just to show
  "placed on" — one that drifts from the engine's record, which since #278 is
  the authority on what a hold stops. `holds()` stays as the short form;
  `HoldRecord` is now `#[non_exhaustive]` so a later field is not another
  break, and a hold row written before the field existed reads as `at_ms: 0`
  rather than failing the listing. `areev hold list --format json` carries all
  four facts, and the text form names the time.
- **Cached rights, a typed session write, and a readable grant set** (#324).
  `AreevFacade::resolve_rights(principal)` returns the fail-closed `AuthzSet`,
  and `session_with(rights)` builds a borrowed session from it for free —
  `principal_session` is now the composition of the two. A host serving many
  principals can cache the set by `(principal, authz_epoch)` (#309) instead of
  re-reading grants under the store mutex on every request; the set is a
  snapshot by design, and the epoch moves when it goes stale.
  `PrincipalSession::add(&grain)` is the typed, attributed write — same
  `write`-on-the-namespace check and same `author_did` stamping as `cal_add`,
  through the grain builders rather than a stringly-typed field map (it refuses
  under an anonymization ingress policy, which applies to the structured write
  path, and points at `cal_add`). `AuthzSet::namespaces(verb)` answers "which
  namespaces may this principal read" as `GrantedNamespaces::All` or
  `Exact(BTreeSet<String>)` — O(grants) rather than probing every namespace the
  host knows, and disclosing nothing new since `Grant`'s fields are public.

## [1.9.0] — 2026-09-18

The **Rounic governance wave**: 41 issues raised against 1.8.5 by a
multi-tenant product for investment firms built on Areev, closed together.
Three themes — destruction that every path honours, runs that stay verifiable
for years, and a tenancy boundary that holds on the OUTPUT side as well as the
input side.

### Fixed

- **The Anthropic tool-calling adapter no longer sends `temperature`** to
  models that reject it (#283). Claude Opus 4.7 and later, Sonnet 5 and the
  Fable 5 models return HTTP 400 for any sampling parameter, and a 400 is
  terminal here — so every abstract-node turn on a current Claude model died
  on its first try. The legacy set that still accepts the field is CLOSED by
  construction (Claude 3.x/4.0/4.1/4.5/4.6 and `claude-haiku-4-5`), so unlike
  a growing per-model table it cannot go stale, and it fails safe: a model
  wrongly left out runs at the provider default. Nothing changes on legacy
  models. Telemetry no longer claims a temperature that was not sent —
  `ToolCallLlm::effective_temperature` is the new defaulted seam, and
  `gen_ai.request.temperature` is absent where nothing was sent.
- **`tags EXCLUDE […]` actually excludes** (#318). The filter parsed, was
  advertised as filterable by `DESCRIBE FIELDS`, was marked consumed by
  push-down — and was read by nothing, so `tags EXCLUDE ["label:restricted"]`
  returned exactly the grains it was asked to exclude, and `areev corpus
  --select` wrote them into the export under an immutable manifest recording
  the exclusion. Both set forms now filter where the sibling `subject_in` /
  `relation_in` / `object_in` sets do. The dead `needs_payload_postfilter`
  helper and its dangling doc reference are gone.
- **World-axis `ENTITY … AT` orders by world time** (#305). Two OPEN-ENDED
  windows both contain every instant after the later start, so the answer was
  whichever was written last — a system-time tie-break on a world-time
  question, and exactly what an out-of-order backfill produces. Ordering is
  now `COALESCE(valid_from, created_at) DESC, seq DESC`: among windows
  containing T, the one that took effect most recently wins, and write order
  only breaks an exact tie. A memory that never sets `valid_from` keeps
  today's answers.
- **A refused self-approval is journaled** (#292). `docs/run.md` promised
  every rejected response is journaled before the error returns; the
  separation-of-duties refusal — the most audit-relevant one the runtime makes
  — returned `RUN-E012` without writing it. So is a `run.respond` grant
  refusal, which now loads the run first so the record carries `run_id`.
- **A brokered non-UTF-8 response body is an error, not an empty 200** (#298).
  Both body-read paths mapped a decode failure to an empty string with the
  REAL status attached, so a connector fetching a PDF received
  `{"status":200,"body":""}` and the audit grain recorded a 0-byte success.
  Now a typed `502` naming the decode failure, and a recorded refusal.
- **`areev reindex` backfills `osp` rows** for a relation declared after the
  grains exist (#310). `osp` rows are written on the add path only when the
  relation is already in `entity_relations`, so years of history stayed
  invisible to every reverse walk and nothing backfilled them — the re-stamp
  only warned. A rebuild now replays every triple's reverse row, inserting it
  when the relation is declared and removing it when it has left the set. The
  warning names the fix.
- **A run is priced** (#291). `usd_micros` was a hard-coded `0` at every
  producer, so a positive `--max-usd` could never exhaust, the `run_outcome`
  spend flag never raised, and the Verify gate read an unpriced run as costing
  `$0` — `0 > 0 × ratio` evaluates `within`, not `not_measurable`.
  `ToolCallLlm::price_usd_micros` is the new defaulted seam; a priced effect
  carries `usd_priced: true`, which keeps UNPRICED distinct from free. No
  journal format change: `verify` and `shadow` replay the journaled figure and
  never re-price, so a later rate change cannot diverge an old run.
- **A principal's grants past the cap are reported, not silently truncated**
  (#309). `authz_grants` read at most 256 heads and returned the first 256, so
  a live grant on the 257th namespace simply stopped working — fail-closed,
  but undiagnosable. Now a coded refusal naming the cap.

- **`shadow` of a draft plan honours the draft's own fields.** A candidate
  body (`--plan-file`, a loop-drafted `plan_revision`) is not in the store, and
  resolution read `reducers` from the store alone, patching them in afterwards;
  anything else the body declared was invisible to the rehearsal. Resolution now
  takes the body's fields directly (`RunManifest::resolve_with_fields`), so a
  draft's `reads` rehearse as reads rather than as LLM steps out of support.

### Added

- **Legal holds bind every deletion path** (#278, #279). The hold check moved
  to the store choke points — `Areev::forget` and `erase_where`'s identity
  selector — and runs INSIDE the transaction after `reserve_write`, so a hold
  placed concurrently on Postgres cannot lose the race. CAL `FORGET <hash>`,
  the MCP tool, the bindings, the console, the memory tool's `delete`/`rename`,
  the mem0 importer and the loop's rollback all inherit it; `drop_postgres_schema`
  reads `hold:` rows before `DROP SCHEMA … CASCADE`. New `STO-E009`, used on
  the age path too. The D10 override ships explicit and audited: CAL
  `FORGET … WITH override_hold BECAUSE "…"`, CLI `--override-hold --because`,
  store `forget_overriding` / `forget_subject_overriding` — requiring `admin`
  on the namespace in addition to `erase`/`delete`, and naming the overridden
  hold in `context.hold_overridden`. A REFUSED attempt is recorded too
  (`erase.refused` / `delete.refused`, `grains_erased: 0`), which is the
  evidence a controller needs to answer an Art. 17 request on an Art. 17(3)
  ground. `hold:` rows now ride a bundle and apply on a point-in-time import,
  so a restored or synced memory comes back HELD; bundle replay applies a
  replicated tombstone even under a local hold and counts it in
  `ImportStats::forgets_under_hold` — a hold binds the memory where destruction
  is DECIDED, and a follower that aborted would diverge permanently.
- **The destruction audit trail is hash-chained** (#280). `Areev::append_audit`
  puts every Tier-2 record on ONE chain per memory — `derived_from` names the
  predecessor, `context.seq` makes a GAP detectable, which `derived_from`
  alone cannot do once an interior record has been forgotten. Shared by the
  CAL facade and the CLI writers, so the shapes stay identical. `audit export`
  verifies it, emits `seq` / `previous_audit` / `chain_root`, distinguishes a
  window edge (`previous_outside_window`) from a break, and reports pre-chain
  records as `unchained` rather than as breaks. A chained record is no longer
  forgettable by hash; an age-based purge of the audit namespace stays
  possible. `docs/procurement.md`'s claim is now true.
- **US identifier detectors** (#281): `us_ssn`, `us_itin` and `aba_routing`,
  structure- or checksum-validated and cue-gated where shape alone is a bare
  digit run. A dashed SSN was reported as `phone`, so a policy allowing
  business contact numbers leaked it; a bare SSN and both routing numbers were
  not detected at all. Overlap resolution gains a specificity tiebreak so a
  validator-backed category beats a shape-only one — detection stays additive,
  so a phone-redacting policy still covers the span.
- **Thinking blocks survive the tool boundary** (#284). `ToolCallResponse` and
  `ChatMessage::Assistant` carry `provider_content`: the assistant turn's
  content exactly as returned, opaque to Areev. The Anthropic adapter captures
  `thinking` / `redacted_thinking` blocks (streaming included, reassembled
  from `thinking_delta` / `signature_delta` and never streamed to `on_token`)
  and replays a present `provider_content` BYTE-IDENTICALLY. The run journal
  and the scheduler transcript carry it through, so a `resume` after a crash
  between turns replays it. Absent for every provider that has none, so
  existing runs journal and verify unchanged. `ToolCallResponse::new(…)` is
  the constructor to use, so the next optional field is not another source
  break.
- **Per-endpoint request profiles** (#285). `RequestProfile` selects
  `max_tokens` vs `max_completion_tokens`, sends or omits `temperature`, and
  passes ARBITRARY extra top-level fields through untouched — `store`,
  `reasoning_effort`, OpenRouter's `provider`, Anthropic's `thinking` /
  `output_config` / `inference_geo`. Keys the adapter owns are refused at
  CONSTRUCTION, naming the key, so a misconfiguration is a startup error
  rather than a failed run. The default profile produces today's bytes
  exactly. `--llm-token-field`, `--llm-no-temperature`, `--llm-extra-body`.
- **The credential seam can sign a request** (#286). `Credential::authorize`
  is a defaulted method taking `{method, url, body}` and returning the exact
  headers to send — which is what AWS SigV4 needs and what minting a string
  could not give. The adapters serialize the body ONCE and send the same
  bytes they handed the credential. `Anthropic::with_auth_scheme` covers the
  common case (`x-api-key` or `Bearer`). An `Err` is terminal and nothing is
  sent.
- **The model and the engine are frozen with the run** (#287, #288).
  `RunManifest.llm` (provider, model, region, host tag, request-profile
  digest) and `RunManifest.engine` (version + `SCHEDULER_EPOCH`). `resume`
  refuses a mismatch before the lease is taken and before any grain is
  written — `RUN-E025` / `RUN-E026`, pointing at `areev run fork`, which
  writes a new manifest carrying the new pin. Only the epoch is compared for
  the engine, so a patch upgrade does not strand parked approval runs.
  `ToolCallResponse.served_model` / `served_region` record what the provider
  says it actually served, so an alias or a router resolving elsewhere is no
  longer invisible. Manifests without the fields serialize byte-identically
  and resume under anything.
- **Batched, asymmetric embeddings** (#290). `EmbedBackend::embed_as(text,
  EmbedInput)` and `embed_batch`, both defaulted. Query sites embed as
  `Query`, the write path hoists embedding out of per-grain prep into ONE
  `embed_batch` per write — N grains was N sequential model calls, and with
  `CommandEmbed` N process spawns. `CommandEmbed` passes
  `AREEV_EMBED_INPUT=document|query` to the child. A wrong count or dimension
  refuses the whole write with nothing stored.
- **`initiator`, and confirmation asks** (#293, #294). A run started by a
  service on a person's behalf can name that person, and the approval check
  refuses them as well as `principal`. A Tool Definition may declare
  `ask_kind: "confirmation"` — an ask the run's own initiator may answer —
  frozen in the manifest so a mid-run supersession cannot downgrade a parked
  approval, and refused at START unless the host passed
  `--allow-confirmation-asks`: a Definition can arrive in a bundle or a pack,
  and a weakening delivered with the thing it weakens is not a permission.
- **Run-level ceilings and concurrency caps** (#295, #296). `BudgetAxis::Effects`
  and `ToolCalls` bound a WHOLE run (`--max-run-effects`, `--max-tool-calls`);
  `--max-concurrent` and `--max-concurrent-per-principal` claim CAS'd slot rows
  beside the run lease, so the cap is hard under races (counting then acquiring
  is not). Exhaustion is a resumable `BudgetExhausted`, never a node failure;
  a refusal at the cap is the retryable `RUN-E027` with nothing written. A run
  with no cap keeps a byte-identical `Spent` and verifies unchanged.
- **Cron in a firm's own time zone** (#297), behind areev-trigger's `tz`
  feature (enabled by the CLI and both bindings). The DST policy is DECLARED
  and test-pinned: a time skipped by spring-forward fires once at the first
  valid instant after the gap; a repeated time fires once, at the earlier
  occurrence; `next_due_after` is strictly-after, searching past the fold so
  the catch-up loop cannot stall. An unknown zone name is refused in every
  build — a typo must never fall back to UTC and fire at the wrong hour while
  looking correct.
- **A configurable, mid-superstep-renewed run lease and a host-qualified
  holder** (#299, #300). `--lease SECS` / `$AREEV_RUN_LEASE` with a 5 s floor,
  renewed after every result rather than only at superstep boundaries — which
  is what makes a short TTL safe, and what the fixed ten minutes was hiding: a
  healthy driver could already outlive its own lease inside one abstract
  node's superstep. The holder is now `{principal}#{host}/{pid}`
  (`--node` / `$AREEV_NODE_ID`); two containers running as PID 1 under one
  service principal were the SAME holder and did not exclude each other.
  `RunLease::peek` reports the holder and expiry for `run inspect`.
- **Run evidence can follow the run's namespace** (#301).
  `--input-placement run-ns` stores the run's input as its own grain in the
  run's namespace, the manifest keeping only `input_ref`; a missing input
  grain refuses rather than replaying against `null`. `--harness-ns` writes
  fold summaries and egress/blob records to `agent:harness.<run_ns>` — a
  dotted child, so they stay out of the agent's own recall scope while
  becoming separately grantable, retainable and erasable. Ids and counters
  stay in `agent:harness`, so `run list`, cancel and the lease paths are
  unchanged.
- **A `PrincipalSession` IS a CAL facade** (#302). `CalExecutor::execute(cal,
  &session)` now runs ANY statement — read or write — under the session's own
  fail-closed rights, so one process can serve many signed-in people without a
  facade per principal (which the embedded backend refuses outright).
  Implemented as a thread-local scope rather than a second shared slot:
  `bind_principal`'s race is that it swaps a PROCESS-WIDE value, and a
  thread-local cannot race by construction. `PrincipalSession::in_namespace`
  points namespace-defaulting reads at the caller's own namespace.
- **Graph and as-of reads across a namespace SET** (#303).
  `RELATED "…" VIA "…" WHERE namespace IN ("a","b")` and the same clause on
  `ENTITY … AT`; store `related_scoped` / `entity_at_scoped`. A walk is not
  composable from per-namespace calls — the frontier, `seen`, depth and cap
  are shared state. Every named namespace is read-checked as itself and one
  ungranted term refuses the statement whole; patterns are refused; the
  100-term `CAL-E011` cap is the shared one.
- **`DERIVED FROM` answers what the session can read** (#304). It required
  `read ON *`, so the only way to offer reverse provenance to a person holding
  exact namespace grants was a wide-open service principal with the host
  post-filtering its results — the authorization decision moved out of the
  engine. Now: the PARENT must be readable, and children are narrowed to
  readable namespaces with NO count of what was withheld (a count would
  disclose that another namespace derived something from this grain).
  Authorization refusals surface as `CAL-E121`, not as `unsupported`.
- **Telemetry that keeps no query text, and a namespace scrub** (#306).
  `--telemetry aggregate-hashed` keeps the same rollups with the key hashed
  (HMAC under a key derived from the memory's own AEAD key), no sample, and no
  ring log. `areev telemetry scrub --ns NS --yes` and
  `Areev::telemetry_scrub_namespace` reach the row nothing else could: a
  ZERO-RESULT free-text query names no grain hash and need not contain the
  erased identity.
- **A namespace-scoped change feed** (#307). `oplog` gains a nullable `ns`
  column (backfilled on open from the grains, written at every insert site
  including `OP_FORGET`), `Areev::changes_since_scoped`, `OpRecord.ns`, and
  `areev log --ns a,b`. A tombstone is now ATTRIBUTABLE, which resolving its
  hash could never do because the grain is gone. `op_seq` stays the
  memory-wide sequence, so cursors remain comparable. Postgres schema version 2.
- **`areev provision --check`** (#308): a SELECT-only report of every stamp's
  found-vs-wanted value and what is pending, plus
  `rolling_deploy: safe | drain_writers_first | unknown` from the new
  `PG_ROLLING_SAFE_FROM` constant. Exit 0 current, 2 pending. Library entry
  point `areev_store::pg::check_provision`.
- **An atomic grant transition and a policy epoch** (#309).
  `AreevFacade::set_grants(principal, &[Grant], because)` makes a principal's
  live grants EQUAL the desired set in one pass — superseding heads in place
  where it can, retiring the rest — so narrowing a packed grant no longer
  means a window with no access or a window with too much. An equal set writes
  nothing. `AreevFacade::authz_epoch` is a single indexed read that changes
  whenever the memory's policy does.
- **The loop's queue is namespace-scoped** (#312). A `Recommendation` carries
  an engine-stamped `scope`: the namespaces the producing analyzer was run
  over, stamped where `dedup_key` and `origin` are, so an analyzer, an
  external command or a model draft cannot set it. A principal covers a
  recommendation when its grants allow the verb on every namespace in that
  scope; a grant on `areev-loop` or `*` still means the whole queue, and an
  empty scope is covered only by such a grant (fail closed). One filtered read
  (`visible_recommendations`) that the CLI, the server, MCP, both bindings and
  `DESCRIBE LOOP` all go through, so a surface added later cannot forget it.
- **A grader can report field metrics and usage** (#313). `areev eval run
  --tool-cmd` sets `$AREEV_EVAL_REPORT` beside `$AREEV_EVAL_CASE`; a command
  may write `{"metrics": {…}, "usage": {…}}` there. Each metric's MEAN lands
  in the `mg:eval_run` summary as `<name>` beside `<name>_n` — which the
  Verify gate already reads as a host-defined field — and usage sums into
  `input_tokens` / `output_tokens` / `usd_micros`. Read fail-closed: a
  malformed report fails the case, and a metric colliding with a reserved key
  refuses. A command that writes nothing produces today's summary exactly.
- **`areev eval --case-ns NS`** (#314): the evalset and every case's input and
  output go to NS, while the summary and the re-acceptance record stay in
  `agent:harness` — the split `areev run` already makes. Confidential
  evaluation cases are then governed by the namespace they came from, and go
  when its sources do.
- **`areev pack validate|install` are a library** (#315, #316).
  `areev::pack::{validate_pack, install_pack}` over a typed `PackReport` and a
  `PCK` error domain, so a Rust host installs an agent without shipping and
  spawning the binary; `install_pack` takes the CALLER's facade, so it runs
  under their bound principal, and writes through `cal_add_batch` so a refusal
  leaves no partial agent. The two verbs are now printers over these.
  `export` stays CLI-only on purpose: it is an authoring step against a memory
  you own, not something a provisioning path runs per tenant. The manifest
  surface gains warned unknown top-level keys, a reserved `"host"` object
  returned verbatim, and evalset validation shared with `areev eval create`.
- **The engine withdraws a recommendation whose premise moved** (#317).
  `RecStatus::Withdrawn`, reachable from `Pending` and `Approved` by the
  engine only. Premise drift was checked on APPLIED recommendations only, so a
  pending finding whose every cited grain had been retracted stayed pending
  and could still be approved — and applying it produced a recommendation to
  revert it. A withdrawal strikes no cooldown and is excluded from the dedup
  keys, so the same finding on new evidence is proposed normally.
- **`audit export` carries the gating edge and, on request, the outcomes**
  (#319). Loop rows gain `gating: {evalset, run_id, passed, failed}` — in the
  parsed body all along and dropped — and `--with-outcomes` adds the Verify
  gate's measured checkpoints, marked `chained: false` because they come from
  the rebuildable state blob rather than hash-chained grains.

- **Shadow a candidate *version*, not only a candidate plan** (#277).
  `areev run shadow --reexecute pure` (bindings: `run_shadow(…, options=…)` /
  `runShadow(…, optionsJson?)`) re-runs a bound node whose **candidate**
  Definition is a pure `wasm32-areev` module in the sandbox, on the input the
  replayed state built, instead of answering it from the journal. Until now
  the rehearsal never consulted the binding, so the patch class most likely to
  change an answer — a tool's *bytes* — rehearsed as `verdict: "same"` by
  construction. Only that runtime re-executes, because its frozen import set
  is exactly `areev::emit` (no clock, no filesystem, no sockets): native
  blobs, `wasm32-areev-io` (a capability module reaches the network),
  client/abstract/subgraph/memory nodes and any address this host has not
  `--allow-executor` pinned still answer from the journal and are reported
  under `not_reexecuted` with the reason. The report adds `reexecuted`,
  `not_reexecuted`, `sandbox_executions`, and the terminal merged-context diff
  as **key paths only** — `changed_keys` / `added_keys` / `removed_keys`, RFC
  6901 pointers, never values — so it is safe on a control channel that must
  not carry content. `effect_dispatches` stays `0` and keeps meaning *no
  external effect*; `writes` stays `0`. Without the option the report is
  byte-identical to before. The mode is not on the MCP tool or
  `/api/run/shadow`: those are reads served by hosts that hold no executor
  pin. `docs/run.md`, "Rehearsing a candidate version".
- **A run can read its own memory: plan-declared `reads`** (#255). A Workflow
  grain's `reads` field names, per node, an `entity_at` (`subject`, `relation`,
  `at`, `axis`) or a `related` walk against the run's own namespace or a dotted
  descendant; the subject, start and instant may come from state by JSON
  pointer (`subject_from`, `at_from`, `start_from`), everything else is a
  literal on the plan. The runtime answers that node itself on the driver thread
  and merges `{into: …}` — byte-identical to `db.entity_at` / `db.related` —
  into state, so a host that only calls `run start`/`resume` no longer has to
  pre-resolve as-of reads in a driver step it does not have. No tool ever holds
  a handle: a read is never offered to a model, never handed to `--tool-cmd`, a
  native blob or a `wasm32-areev-io` module, and the executor pool refuses one
  outright. Each read is an ordinary journaled effect (`mg:entity_at` /
  `mg:related`) whose result carries a `read` record — namespace, resolved
  operands, axis, instant and grain hash — so `verify` and `shadow` answer it
  from the journal after the file has moved on. Malformed declarations
  (including unknown keys) refuse at start with `RUN-E019`; a namespace outside
  the run's, or one the session cannot `read`, with `RUN-E012`. `run inspect`
  prints each frozen declaration under `read`; the console draws read steps as
  "Memory read" and opens such plans view-only. `docs/run.md`, "Reading the
  run's own memory".

### Changed

- **`areev-run` depends on `areev-llm` with `default-features = false`**
  (#289). Cargo unifies features, so the runtime's dependency edge compiled
  the openai, anthropic and ollama factory arms into every crate downstream of
  it — and a host declaring `areev-llm` with `default-features = false` got
  all three anyway. The feature flags exist so a regulated build can state
  which model endpoints its artifact can REACH; that statement was not true
  through this edge. The no-feature build is now clippy-clean and tested.
- `docs/gdpr.md`'s Art. 33 row no longer says `audit export` answers "what was
  accessed" (#282). It answers what was DESTROYED, REVEALED or HELD; access
  logging is the host's responsibility, and the same page has always said reads
  write no audit grain.

## [1.8.5] — 2026-09-17

### Fixed

- **An abstract node is offered each Definition once, however many nodes bind
  it** (#270). `RunManifest::executors` built the offer from the pinned list,
  which holds one entry per plan node, so a Definition bound to two nodes — the
  ordinary shape for a terminal step reached from two branches — reached the
  model twice, and every provider refuses a tools list with a repeated name
  (`tools: Tool names must be unique`, HTTP 400 on the whole request, reported
  as `Failed { node: "<abstract node>", detail: "ExecutorError: … HTTP 400" }`).
  The offer now holds the first occurrence of each name, in plan order; each
  bound node still executes its own binding. Measured on Areev Cloud's
  `invoice-to-accounting` pack (Core's own example), whose every job failed at
  `extract_rows` on 1.8.3. PR #271 landed only the `docs/run.md` wording of
  this; 1.8.4 shipped without the code, which is why this entry is here and
  not there.

## [1.8.4] — 2026-09-17

### Added

- **The Verify gate can compare against the agent's best run, and always
  shows it** (#258). `outcome_evalset.baseline: "high_water"` makes an
  evalset verdict's baseline the best run journaled before the apply (max
  for a higher-is-better field, min otherwise) instead of the newest one, so
  an agent that fell from its own peak reads `regressed` and the revert is
  proposed. Measured need: on the ad-buy corpus (seed 3) the agent reached
  238 of 280, two approved rules took it to 128, and the gate reported
  `held` — correctly, because day one's 35 was the only run before the
  apply. Opt-in, not the default: it charges the whole fall from the peak to
  whichever rule was applied last, which on a noisy evalset proposes reverts
  of rules that did nothing wrong. Independently of the choice, every
  outcome record now names the run it compared against (`baseline_kind`,
  `baseline_run_id`) and, on an evalset metric, carries `best_before` — the
  peak before the apply — in `areev loop outcomes`, `GET /api/loop/outcomes`,
  the bindings' `loop_outcomes()` and the console's outcome card, so the
  lost opportunity is visible on a `held` too. A revert drafted under
  `high_water` names both figures and the run it fell from.

- **A minimum effect size for the Verify gate, shared with `eval run
  --tolerance`** (#262). `outcome_evalset.min_effect: {"count": n}` (in the
  field's unit) or `{"points": p}` (percentage points; scaled by the
  baseline run's total for `passed`/`failed`/`total`, read as `p/100` for
  `error_rate` and host-written ratio fields) is a floor under the verdict:
  a worsening of at most that much is `held`, and the outcome record carries
  the `tolerance` it held under so a `held` under a floor is distinguishable
  from a `held` at zero. Default none — any drop is a regression, exactly as
  before; measured need: a 359 → 355 dip on 387 trials, within what one
  adapter read twice can differ by, proposed a revert. It is a floor, not a
  significance test, and the docs say so. `is_regression` gained the
  tolerance and stays the one function the recorded verdict, the revert
  draft and `areev eval run --baseline RUN --tolerance N` all call, so the
  two readers of "did it get worse" cannot drift; a `--tolerance` that is
  not a number ≥ 0 is now refused instead of silently read as zero.

- **The Verify gate sees what a lesson costs, not only what it scores**
  (#259). `areev eval run` journals `effects` (executor calls) and `wall_ms`
  on every run, and `input_tokens`/`output_tokens` from the provider's usage
  on the `--model` path; `run_value` promotes `effects`, `tokens`, `usd`,
  `wall_ms` and `cost_per_pass` (undefined, not zero, when nothing passed),
  read fail-closed like `passed`/`failed` — a string or float makes the cost
  not measurable, never zero. A cost key the summary omits is read from the
  runtime's `run_outcome` Observation for the same run id, so an evalset run
  that is also an `areev run` run quotes one spend to the gate and to the
  `run_outcome` analyzer. `outcome_evalset.cost: {"field": "tokens",
  "max_increase_ratio": 1.5}` reads that column beside the quality field:
  quality held but cost past the bound records the new verdict
  `held_costlier` and `outcome_review` emits an advisory Flag citing both
  runs — never a revert, a cost/quality trade is a human decision;
  `regressed` dominates, and a revert on a run that also breached the bound
  names the cost delta. `areev loop outcomes`, `/api/loop/outcomes`, the
  bindings and the console card show both columns. The receipts bench
  harness writes the cost keys; `tau2`, `appworld` and PAST-Bench do not
  carry usage per record and are unchanged.

- **Near-duplicate lessons are marked, and a lesson budget can be stated**
  (#260). Measured (ad-buy seed 3): ten approved rules stated four distinct
  facts, each approvable alone, and the agent fell from 238 to 128.
  `authored_dedup_key` sees the same text; meaning is now measured at ROUTE
  — an authored lesson is compared with every live lesson on its entity by
  cosine over the substrate's embedder (new `SubstrateRead::embed`, wired
  through `Areev::embed_text`) or, keyless, by token-set Jaccard. Policy
  `near_duplicate: "flag"` (default) queues it carrying
  `near_duplicate_of: [{hash, score, method}]` with a NEAR-DUPLICATE summary
  and the existing rule beside it on the console card, the CLI, the server
  and MCP rows; `"suppress"` drops it before the queue and the funnel counts
  `dropped_near_duplicate`. A new default-off analyzer, `lesson_pile`, flags
  an entity over `max_active` (8) live lessons with each member's latest
  Verify-gate verdict (analyzers see them via `AnalyzeCtx::verdict_for`),
  and DISCOVER gains a `consolidation` kind answerable only to that finding:
  one lesson that supersedes every member with a marker (the prompt holds
  one rule, not N copies), gate-judged and human-applied; `rollback`
  restores every member — pinned on the real store, and the reference
  substrate's `retract` now un-supersedes to match. Fifteen analyzers.

- **`areev loop replay` — score a loop configuration against the past**
  (#257; `docs/loop-proposal.md` §17 rung 1, built). `areev loop replay
  --config candidate.json [--window 90d | --since MS] [--step per-pass|1d]`
  steps `now` through the recorded passes (reconstructed from the audit
  trail) or a fixed stride, runs the deterministic analyzers under the
  candidate at each step reading only grains created at or before it (a
  prefix view that also refuses every write by type), carries the state the
  loop had — the watermark per step, rejection and measured-revert
  cooldowns, the rehearsal's own queue for dedup — and reports, per
  analyzer and in total, findings under the candidate beside the
  **incumbent** (always a row): overlap with recorded decisions (approved /
  rejected / never reviewed / never proposed), with outcomes (regressed /
  drifted / held), and queue volume per step. Where the substrate can name a
  content address without writing (new `SubstrateRead::address_of`; Areev
  computes it by serializing the grain), each would-be finding names the
  exact grain a live pass would have stored — the golden identity test pins
  the queue byte for byte. `origin = llm` and `origin = command` are not
  replayed and say so; telemetry-fed analyzers too. The CLI prints the
  op-log length before and after. Same request on `POST /api/loop/replay`
  (token-guarded), the console Setup view's **Preview** beside each
  analyzer toggle, and the bindings' `loop_replay` / `loopReplay`. No
  auto-adoption.

- **`areev run shadow --plan`: a plan change rehearsed against the journal**
  (#256). `areev run shadow --runs a,b,c --plan <HASH>` (or `--plan-file
  draft.json`, validated by `PlanGraph::build` first) resolves a manifest for
  the candidate the way `fork --plan` does, seeded from each run's recorded
  input, and re-drives the run through the pure scheduler with every effect
  answered from the journal by its exact key — `retries`, `max_cycles` and
  edge conditions from the candidate, zero dispatches and zero writes by
  construction. Per run and in aggregate: outcome under incumbent vs
  candidate, supersteps, effects replayed and **out of support** (an effect
  the journal never recorded — a report field, never an error, and no score
  for that run), spend consumed, a `same`/`better`/`worse`/`out_of_support`
  verdict, `no_worse` and `out_of_support_fraction`; when the candidate is
  the incumbent the rehearsal is also a verify (`identity.consistent`).
  Same rehearsal on MCP (`areev_run_verify` with `plan` + `runs` — the tool
  count is unchanged), `GET|POST /api/run/shadow` (POST takes an unstored
  `plan_body`), the bindings' `run_shadow(run_ids, plan=, plan_body=)` /
  `runShadow(runIds, plan?, planBody?)`, and the Workflows canvas
  (**Rehearse** a draft against the last runs of the open plan). The loop
  closes on it: a `plan_revision` proposal carries the rehearsal as its
  `replay` block (`areev loop show`, the console card) through a new
  `SubstrateRead::plan_replay` seam the Areev adapter implements over
  `areev-run`, and a `plan_replay` policy `{"min_runs": 3,
  "require_no_worse": true, "max_out_of_support": 0.5}` stores a worse or
  unscorable revision as advisory with a reason naming the runs — a gate,
  not an auto-deploy. `ARCHITECTURE.md` §10 records the decision.

## [1.8.3] — 2026-09-16

### Added

- **The selfimprove bench can open a Postgres memory** (#250). `selfimprove_aba
  --db PATH|postgres://…?schema=…` — or `$AREEV_BENCH_DB`, the same variable the
  Python harnesses have honoured since 1.8.0 (#200) — moves the MEMORY out of
  the workdir while the transcripts and `report.json` stay in it. The A/B/A/B
  track is the one with no external dataset, a keyless deterministic floor and
  a programmatic scorer, which makes it the natural case for proving a
  provisioned schema answers like an embedded file; it was also the only track
  that could not target one, because the Rust binary hard-coded
  `<workdir>/bench.db`. With a DSN set no `bench.db` is created at all, the
  report records the memory redacted (a password must not ride into a published
  run directory) beside a `db_backend` field, `?provision=never` is honoured so
  the run's role needs no `CREATE`, and "fresh memory" is judged by what the
  schema HOLDS rather than by whether a file exists. `selfimprove_learn` stays
  file-only and says so: its method is one fresh copy of the captured memory per
  pass, and a schema cannot be copied. Gated in CI's postgres job
  (`cargo test -p areev-bench --features postgres --test selfimprove_pg`);
  without the feature a DSN is refused by name, never treated as a filename.

- **One answer to "which grain do I use?"** — `docs/grains.md`. The
  decision table for all thirteen types, the rule of thumb (true → Fact,
  happened → Event, measured → Observation, intended → Goal, procedure →
  Workflow, capability → Tool), the pairs people mix up, how each type is
  written on every surface, the two field traps, and `add` vs `supersede` in
  time. The guidance existed, but only inside the agent-building manual,
  three sections in; the quickstart showed nothing but a Fact; and the
  surfaces where the choice is actually made gave no help at all. Now each
  grain type carries a one-line `purpose` on the registry
  (`GrainTypeMeta::purpose`), `DESCRIBE <type>` reports it, the MCP
  `areev_add` description states the rule and enumerates the addable types
  from the registry instead of trailing off in an ellipsis, and the page's
  "Use it for" column is test-pinned to quote every registry row verbatim —
  the engine's answer and the doc's answer cannot drift. Its CAL examples
  are CI-parsed like the reference's. Every other doc that touched the
  question (the agent guide, FAQ, ARCHITECTURE §2.3, the CAL and MCP
  references, the quickstart, `llms.txt`) now defers to it.

### Fixed

- **An abstract node could not call a tool with a dot in its name** (#251).
  Anthropic and OpenAI forbid `.` in a tool name, so a pinned Definition called
  `receipt.prepare` is offered to the model as `receipt_prepare` — and the
  model, correctly, called back with what it was given. The scheduler's
  unknown-tool check compared that against the un-normalized set, so **every**
  call to a dotted tool from inside an abstract node failed the node after one
  corrective re-prompt the model could not possibly satisfy. The reverse map the
  normalizer's own doc comment promised ("the invoker reverse-maps on the return
  path") was never written. Now it is, once, where the model's answer enters the
  scheduler: exact match wins, then a unique normalized match, so the
  unknown-tool guard, the strict-argument validator (which had silently skipped
  every dotted tool, its schema keyed by the canonical name) and the dispatch
  all see the Definition's own `tool_name`. A name matching neither form still
  gets exactly one correction, now listing the tools the way the model was shown
  them. The outbound half is fixed with it: the tool-calling adapters render a
  replayed `tool_use`/`tool_calls` name — and a `ToolChoice::Named` — through the
  same normalizer, because a transcript naming a tool the request never offered
  is a 400 on the loop's *second* turn. Found by Areev Cloud's first
  model-backed capture pack; it removes the rule that a pack with an abstract
  node must name its tools without dots.

  **What it means for `areev run verify` on older runs.** A run recorded
  BEFORE this fix whose abstract node called a dotted tool no longer replays.
  That covers both the runs the bug failed outright *and* — measured, not
  assumed — the ones that COMPLETED, because a model that obeyed the
  corrective re-prompt and re-issued the dotted name got its tool run on the
  second turn. In both, replay now dispatches that tool at an `effect_seq` the
  old journal spent on a re-prompt. `verify` reports this honestly rather than
  erroring: the superstep before the divergence is still marked
  journal-consistent, and the diverging step names the effect (`has no
  journaled result — not verifiable past this point`). No fix is planned and
  none is possible without keeping the defect alive behind a per-run epoch —
  the journal records what a scheduler that no longer exists decided, and a
  `verify` that returned true for it would be the audit surface lying. No
  journal shape moved; a run that never called a dotted tool is unaffected.

- **A dead registry row that lied about required fields.** Each grain type
  carried a `required_add_fields` list in `types/registry.rs` that *nothing
  read* — the CAL JSON builder kept its own copy, and that copy is what
  `DESCRIBE` and `VAL-E001` have always used. Unread, the registry's version
  had drifted: `observation` claimed to need `observer_id`/`observer_type`
  when the write path demands `content`, `consent` was missing `user_id`,
  and `workflow` claimed to need `nodes` though an empty container is legal
  by design. No output was ever wrong — the wrong copy was simply the one a
  reader would find first, and the new `purpose` sentence now sits in that
  same row, so it had to become true rather than merely unused. The
  builder's `required_fields` now reads the registry, the existing
  `required_fields_match_the_validator` test pins the row to what the
  builder arms enforce, and `DESCRIBE` output is byte-identical to before.

- **A macOS-only CI flake in the credential broker's binding test.** The
  Python stub upstream in `areev-py`'s #201 test answered without ever
  reading the POST body it was sent, so `socketserver` closed the connection
  over two queued bytes — and a close over unread data sends an RST rather
  than a FIN, which on macOS discards what the peer has already buffered.
  The broker read the status line but lost the body it had been promised,
  and reported the admitted call as a 200 with an empty body: its documented
  behaviour for a mid-body read failure, and indistinguishable from an
  upstream that answered with nothing. Harmless on an idle machine and
  roughly even money on a loaded one — 10 of 20 runs failed under CPU
  saturation, 0 of 20 after, with the leftover bytes confirmed in the kernel
  queue on every failing run and absent on every passing one. The stub now
  drains to `Content-Length`, which the Rust stub for the same test
  (`areev-cli/tests/common/egress201.rs`) always did and Node's `http`
  server does on its own. Test-only: no shipped behaviour changed.

### Changed

- **The crypto stack moves to the current RustCrypto generation, with the
  compatibility actually proven** (#240). `sha2` 0.10→0.11, `argon2` 0.5→0.6,
  `getrandom` 0.2→0.4, `hkdf` 0.12→0.13 and `aes-gcm` 0.10→0.11 — five majors
  at once, across passphrase key derivation, the CAS sidecar's AEAD and every
  nonce and token in the tree. The code changes are small: `getrandom` renamed
  its one function, `hybrid-array` deprecated `from_slice` in favour of the
  infallible `From<[u8; N]>`, and a digest no longer renders itself as hex.

  The risk was never the compile. **A round trip seals and opens with the same
  build, so the whole 2,800-test suite would have stayed green if a derivation
  had shifted — and every passphrase-encrypted memory and every encrypted blob
  in the world would have become unopenable.** There were no committed
  fixtures to catch it either. So the reference values were computed with the
  exact crates 1.8.2 shipped and are now pinned as known-answer tests: the
  Argon2id key for a fixed passphrase, salt and parameters; the HKDF-SHA256
  blob subkey; and a ciphertext produced by aes-gcm 0.10.3, which still
  decrypts with its address binding intact. All three hold. Those tests stay,
  so the next bump that does move one fails loudly instead of silently — and
  if it ever fires, the bump is a migration with a re-key path, not an upgrade.

  One cost recorded rather than hidden: `areev-store` named `aes-gcm` directly
  because the storage engine already pulled the same version, keeping the blob
  sidecar on the identical primitive as the database pages. The engine stays on
  0.10.3, so a default build now links both. Taken deliberately — holding a
  security primitive back to match an upstream pin is the worse trade — and it
  reverses on its own when the engine catches up. The manifest says so now
  instead of asserting something false.

## [1.8.2] — 2026-09-14

The theme is context: an abstract node's transcript now has bounds, manages
itself when it passes them, and leaves behind something the loop can learn
from. Plus the two defects that only a real provider and a real crash could
have found.

### Added

- **The fold: a transcript past the ceiling becomes one more journaled turn**
  (#236). An abstract node's transcript grew until the provider rejected it,
  and the node died with the provider's message as its detail. Three bounds
  now exist, and all three are frozen into `RunManifest` at start so `verify`
  reproduces the run that hit one instead of replaying past it under a
  default. `--max-effects N` lifts the per-attempt effect cap off the two
  driver literals that made 16 unraisable — roughly eight turns at one tool
  call each, which is not an agent. `--llm-tool-result-chars N` bounds a
  SINGLE tool result, because the likeliest way a node dies is not a long
  conversation but one file read or log dump entering the transcript verbatim;
  no summarizer can shrink a single entry, so that bound has to exist
  separately. Past it the model sees the true length, the head, the tail and
  the journal coordinates of the whole result; under it the value passes
  through BYTE-IDENTICALLY, which is why a deployed run that sets nothing has
  unchanged checkpoints and still verifies — pinned by its own test rather
  than assumed. `--llm-context-tokens N` is the whole-transcript ceiling: over
  it the scheduler emits one more turn, a summarizer over the middle of the
  transcript whose answer is spliced in place of that middle. It is
  deliberately not truncation — every turn is already a journaled grain, so
  the summary is one too, and the full transcript stays in the journal. Each
  flag is also `llm_context_tokens` / `llm_tool_result_chars` /
  `max_effects_per_attempt` on MCP `areev_run_start`, Python, Node, and
  `trigger run`. `docs/run.md` gains "What bounds the transcript (and what
  does not)"; the context-assembly fact sheet gains §13.

- **Long contexts handle themselves, and the loop notices when they don't**
  (#238). Folding shipped opt-in — two flags, set together, or a long agent
  still died. Two mechanisms make it automatic, picked by what the provider
  will actually tell us. Where overflow is reported STRUCTURALLY
  (OpenAI-compatible `error.code = "context_length_exceeded"`) nothing has to
  be predicted: the refused turn is journaled, the transcript is folded, and
  the same turn is re-sent on something smaller. That closes the one-round
  measurement gap the fold shipped with, because the ceiling can be unset, too
  high, or right about a transcript that has since grown, and the provider's
  verdict beats all three. Only a structured code counts — Anthropic reports
  the same condition as prose, and a seam matching on wording would work until
  a vendor reworded it and then fail silently. For that path the ceiling is
  derived from the model instead: an unset `--llm-context-tokens` becomes the
  window minus reserved output, and the per-result bound derives from the
  ceiling, so an operator who sets one gets the other. This is deliberately
  NOT a model→window table — a table of numbers we cannot verify fails
  SILENTLY when it goes stale, and a too-high entry means the ceiling never
  fires, which is the crash it was added to prevent. One number (200k), one
  family (`claude-*`, prefix-checked so Bedrock and Vertex names claim
  nothing), documented as a FLOOR. `run inspect` now reports the effective
  `limits`, because a ceiling the runtime picks for you is otherwise invisible.
  Every terminal run records `folds`, and the loop's `run_outcome` raises an
  advisory Flag on a workflow that needs one on essentially every run — a
  plan-shape decision with nothing to auto-apply. A fold now and then is the
  mechanism working and says nothing.

- **A fold summary becomes evidence the loop can read** (#241, #245). What an
  agent works out over fifty turns ends up in exactly one place: the summary a
  fold produces. That text was already stored as the fold effect's result
  grain, but a journal Tool grain is not reachable by recall — its payload
  lives in `tool_content`, which the store's text projection does not index,
  and it carries no subject/relation/object. Each fold now also writes a
  `fold_summary` Observation in `agent:harness` carrying the run, the node and
  the `effect_seq` of the range it stands for. Typed and indexed, so the LLM
  path can propose a lesson CITING a summary and the four gates decide whether
  it is ever applied. Two boundaries are deliberate: it is evidence, not
  memory — the namespace is the statement, and a summary is working state
  whose verbatim recording would pollute recall rather than compound it; and
  it is written twice on purpose — the result grain stays the record `verify`
  replays, the Observation is the readable copy. Confirmed against a live
  model on a memory containing nothing but a fold summary: one pending lesson
  whose sole cited evidence is the summary grain.

- **`areev trigger retarget` re-points a standing rule at its plan's head**
  (#243). A trigger names its plan by content address, so editing the plan
  leaves the trigger starting the version it was declared against. That
  default is right and stays — a plan edit must not silently change what an
  unattended heartbeat runs. What it cost was VISIBILITY: a trigger on a
  three-edits-old plan rendered identically to a current one, fired forever,
  succeeded every time, ran the old logic, and nothing in `list`, `show`,
  `status` or the run said a word. Silence is the symptom of every trigger
  failure and must not also be the symptom of a correct default. So the drift
  is reported and never followed. `trigger list` marks it inline and names the
  fixing command on stderr; JSON and `trigger status` carry `plan_head` and
  `plan_superseded`, absent when current so a clean listing stays clean. On
  `retarget`, `--workflow` is OPTIONAL — without it the trigger follows its own
  plan's chain to the live head. The destination is validated before anything
  is written, because re-pointing a standing rule at a plan that cannot run
  replaces a stale firing with a broken one discovered by a heartbeat nobody is
  watching; a no-op retarget is refused rather than written into a chain every
  later reader walks. `Areev::current_head` is the new store primitive, the
  forward mirror of the backward `supersession_chain`. Evaluation state keys on
  the chain ROOT (#128), so a re-pointed polling trigger resumes rather than
  replaying its backlog — asserted, not assumed.

- **`FailureCause::ContextOverflow`** (#242). A context overflow had nowhere to
  land in the grain vocabulary, so the journal filed it as `executor_error` and
  carried the real classification in an extra field beside it. Two mechanisms
  for one fact, and a taxonomy whose stated purpose — letting dashboards bucket
  failures without parsing strings — pointed operators at the transport layer
  when the problem was the transcript. The variant is not a breaking format
  change: `FailureCause` serializes as a snake_case string and unknown values
  are dropped on read, so an older reader degrades to "no cause recorded" and
  no grain re-addresses. OMS 1.6 adds `"context_overflow"` to §6.5's OPEN
  `error_type` enum on the same spelling (openmemoryspec/oms#11). Worth
  recording that these remain two vocabularies, not one: they overlap only on
  `timeout`, and aligning one spelling is not unifying them.

- **PreCompact capture for the Claude Code hook** (#236). Compaction sits
  between `UserPromptSubmit` and `Stop`: the host is about to drop the
  conversation and nothing told Areev, so whatever the last Stop had not stored
  was lost. `hook claude-code` now also wires PreCompact to `capture-stop`,
  unchanged in its write path — one verb serves both events because it reads
  the cumulative transcript and is idempotent by content address. No matcher,
  so manual `/compact` and auto-compaction both reach it.

### Fixed

- **A crash-recovered run verifies, and its downtime is reported** (#237).
  Every run that crashed and resumed failed `verify` with `RUN-E009` — on
  `spent.wall_ms`, and on nothing else. That is precisely the run an auditor
  asks about, against the strongest claim Areev makes. `step` closes a
  superstep and opens the next in the SAME call at the same reading; a live
  driver that died in between came back with a fresh, later reading and opened
  there, charging nothing for the gap, while a replay ran straight through the
  boundary and billed the downtime as active wall. The divergence was exactly
  the crash-to-resume span, every time. `EventIn::Resumed` makes the boundary a
  journaled fact: the gap becomes `elapsed_ms`, the checkpoint stamps
  `resumed_at`, and `verify` rewinds to the checkpoint it just byte-compared
  and re-enters the boundary the way the driver did. A second defect was
  hiding underneath — `state.rs` promised that crashed gaps accumulate in
  `elapsed_ms`, reported but never charged; for crashes they were charged to
  NEITHER and simply vanished, so an operator asking how long a run was stalled
  got nothing. Both halves of that sentence are now true, and `inspect` can say
  four hours of calendar time and ninety seconds of work. Three exclusions are
  load-bearing: idle checkpoints only (a parked one already stamped
  `paused_at`), no checkpoint at all is not a resume boundary, and a fork's
  SEED is not one either — it is synthetic and has never executed, and on a
  time-travel fork the span back to the base is lineage, not downtime.
  `resumed_at` is optional and skipped when absent, so a checkpoint written
  before it existed serializes byte-identically and no stored run's verdict
  changes retroactively.

- **`strict` is no longer claimed for a schema that cannot honour it** (#244).
  Every Tool Definition rendered to an OpenAI-compatible endpoint was rejected
  with `400 invalid_function_parameters`: `'additionalProperties' is required
  to be supplied and to be false`. `strict` defaults to true but the renderer
  never emitted that key, so the default asserted on the author's behalf a
  property of a schema nothing had checked. Any Definition whose author had not
  hand-written the key was affected, which is to say almost all of them —
  **abstract nodes could not call host tools on an OpenAI-compatible provider
  at all**, and it had been so since 1.0.0. The renderer now either makes the
  claim true or does not make it: objects are closed recursively when the claim
  is kept, and a schema with optional properties renders `strict: false` rather
  than having `required` forced onto it, since imposing that would turn a
  genuinely optional argument into a mandatory one. Four snapshots move by one
  line each, schemas untouched. Fixture tests assert the JSON we emit; only a
  provider validates it, which is why a 2,700-test suite never saw this.

- **The loop's reference substrate no longer models more than production
  allows** (#245). `TestSubstrate` did not model the `agent:` namespace
  exclusion at all, so it was strictly more permissive than any real substrate
  — which is why the fold-summary evidence path shipped believing it worked and
  only a live provider showed `evidence: 0`. It models the exclusion now, and
  that immediately surfaced a second case of the same class: a test asserting
  that a journaled harness run counts toward `after_grains` activity, which it
  does not. A reference substrate more permissive than production does not just
  miss bugs, it certifies them.


## [1.8.1] — 2026-09-11

### Added

- **Idle Postgres connections are reaped, and a quiet pool is evicted**
  (#229). `?pool=` bounds one pool, and the pool is keyed by the DSN — so a
  host that gives every tenant its own Postgres role has one pool per tenant,
  and nothing ever released one: a long-lived worker's connection count grew
  with the number of distinct roles it had *ever touched*, not with how many
  were in use, and only a restart brought it down. Now a connection idle past
  `?pool_idle_secs=T` (default 300 s, `$AREEV_PG_POOL_IDLE_SECS` out of band,
  the DSN winning, `0` to keep the previous never-reap behaviour) is closed by
  one process-wide reaper thread, and a pool whose connections have all gone —
  with no handle and no statement holding it — is dropped from the registry,
  releasing its runtime's worker threads too. An idle memory now genuinely
  holds nothing; reopening one whose pool was reaped costs one dial and emits
  no warning. `?pool=` semantics are unchanged, and the parameter is stripped
  before the DSN reaches the driver like every other store parameter. Proven
  by a new case in `tests/pg_pool.rs` — eight memories on eight DSNs (the
  shape eight roles would have) holding eight connections after every handle
  is dropped, then zero, with all eight pools gone — and by the whole Pg
  conformance suite, chaos hook included, running with a one-second TTL.
  `docs/deployment-profile.md` now states the retained-connection model a
  capacity plan needs.

- **A blessed `rest.poll`: a paginated REST connector is a declaration, not a
  crate** (#231). `mailbox.poll` said a production connector "differs in one
  line" — true for one provider, five Rust crates for five, each carrying the
  same three decisions in code: where the items are, where the cursor is, how
  the next page is asked for. Those are the parts most likely to be wrong, and
  in code they are invisible to the pack, to `areev tool provenance` and to the
  loop's `code_revision` gate. `rest.poll` takes them as JSON pointers in the
  Definition's `config` (`items`, `id`, `cursor_from`, `next_page`, plus
  `query`/`headers`/`credential`/`page_size`/`cursor_param`/`order`), so two
  Definitions naming one blob poll two different APIs. Pointers are RFC 6901
  with one extension — `-` selects an array's last element, which is what makes
  `/messages/-/id` writable. The four cursor rules connector authors get wrong
  live in it once: an absent cursor leaves the stored one alone, the watermark
  advances over everything looked at (`max_items` is asked of the SOURCE as a
  page size, never used to slice a page afterwards), the first poll seeds, and
  a page token rides in the cursor so `more: true` drains without hammering.
  Reach is unchanged — the `capabilities` block still bounds it and the broker
  still refuses what it does not admit. `mailbox.poll` is untouched and remains
  the keyless, network-free floor; the other four addresses are unchanged.
- **A Tool Definition's `config` reaches the connector it names** (#231),
  merged under the trigger's own config (the trigger wins). The Definition is
  the connector's wiring, beside the `capabilities` block it must agree with;
  the trigger is the instance. Either alone behaves exactly as before.

### Fixed

- **A run started where it cannot see the plan's Definitions refuses instead
  of substituting** (#230). A node that names its tool rather than binding it
  resolves through the RUN's namespace, so a plan started somewhere other
  than where its tools were authored found an empty catalogue — and, with a
  model configured, quietly became an abstract node: the Definition's
  `executor_uri`, runtime and capabilities dropped, a model answering
  instead, and the run reporting Completed having called nothing. Resolution
  now asks one more question before falling through: if the plan grain lives
  in another namespace and a Definition of that name is there, the run is
  refused at start (`RUN-E004`, before the lease), naming the node, the
  namespace searched and the one that would have worked. A node that names
  no Definition anywhere is still abstract, and a bound node still resolves
  from any namespace.
- **`run inspect` reports the resolution it froze, not a summary of it**
  (#230). A bound capability tool printed as a bare `"executor": "host"` —
  byte-identical to a node with no Definition at all — so a correct run and
  a broken one were indistinguishable in the one command you would run to
  tell them apart. Each `pinned[]` row now also carries `executor_uri`,
  `runtime` and the `capabilities` declaration when the Definition named
  code.
- **`areev run start` notes a plan/run namespace mismatch** (#230). Running
  a plan from another namespace is supported and sometimes intended, but it
  is also what a forgotten `--ns` looks like — the run works and its record
  lands where nobody is looking. One line on stderr, naming the flag that
  would move it.

- **A connector that reports an error no longer reads as an empty page**
  (#231). `PollResponse` tolerates unknown fields, so `{"error": "…"}` — the
  shape every blessed blob fails in — deserialized into an empty page with no
  cursor: a source that was DOWN, reported as a source with nothing new, on
  every tick, silently. A poll that answers an error now fails (`TRG-E004`):
  the claim is released, the cursor stays put, the backoff applies, and the
  code it carried is in the message.

## [1.8.0] — 2026-09-11

### Added

- **One connection pool per process for the Postgres backend** (#181, third
  and closing increment). A memory handle no longer owns a connection: every
  handle on one DSN borrows from one process-wide pool — a connection per
  statement, or one for the length of a transaction — so N memories in a
  process hold at most P connections between them, an idle memory holds
  none, and the telemetry sidecar and the broker's blob door ride the same
  pool. P is `?pool=` on the DSN or `$AREEV_PG_POOL` (default 8, the DSN
  winning); the first open sizes the pool and a later open asking otherwise
  is told in `open_warnings()`. Prepared statements and the ANN session
  settings moved onto the connection; a handle applies what it wants with
  `SET LOCAL` per transaction, and the bootstrap's `search_path` is `SET
  LOCAL` too, so no schema is ever left on a pooled session. Connections
  carry `application_name = areev`. Proven by a new conformance binary
  (twelve telemetry-on handles and six concurrent writers through a pool of
  three, counted in `pg_stat_activity`), a two-backend isolation case (two
  memories served in turn by one pool share nothing), and the whole suite
  through `?pool=2`, the chaos hook and PgBouncer. The deployment profile's
  "keep an LRU of handles" advice is withdrawn.


- **`$send` can fan out to an abstract node** (#187). A spawn target had to
  be a host tool node, so a plan that wanted N documents classified by an
  agent had to enumerate N nodes or drop to a single tool call. Each task now
  gets its OWN LLM loop, journaled under its own task path, and the loop's
  answer settles that task — the batch joins before the target's downstream
  edges fire, exactly as a host fan-out does. `abstract_flows` is keyed by
  node-and-task rather than node; a node's own loop keeps the bare-index key,
  so existing journals replay byte-identically.

- **A person can steer a running run** (#187). `areev run input --run-id ID
  --message TEXT` — also `areev_run_input` (MCP), `db.run_input` (Python),
  `m.runInput` (Node) — queues a message on the run. The next superstep hands
  every node it dispatches the queued messages, in order, under the reserved
  `$inbox` key in their input. A chat-style plan no longer has to misuse a
  human-gate ask to receive a message. Steering is journaled as a Fact on the
  run and is applied only while a superstep is open, so it stays inert for the
  whole superstep that observed it. That is what keeps `verify` exact: replay
  counts the journaled messages against the checkpoint's `inputs_seen`, never
  against when the driver happened to poll. A message queued before the run
  starts therefore reaches the second superstep — the first one's input is
  `--input`. `run.execute` is the verb: steering advances a run rather than
  braking it.

- **A subgraph child that parks on a human gate now bubbles its asks to the
  parent** (#187). The gate used to fail the parent node with "subgraph run
  parked" — a HITL step had to live in the top-level graph, which is exactly
  the composition a subgraph exists to allow. The child's open asks travel as
  the subgraph effect's own journaled result, so the parent parks on the same
  `tool_call_id`s and `respond`/`resume` on the *parent* route down to the
  child: you answer the run you started, however deep the gate sits. Because
  the bubble is a journaled result, `verify` reproduces the park from the
  parent's journal alone and never re-runs the child. Two consequences worth
  stating: a child run id is now derived from the parent and the NODE
  (`parent~sha256(parent, node, attempt)[..16]`), which a bounded cycle still
  varies per generation while a park and its answer reach the same child; and
  a bubble round advances `effect_seq` rather than `attempt`, so parking never
  spends the node's retry budget.
- **A trigger can name its connector by content address** (#185). A polling
  connector was a host command (`--connector-cmd`), which put the code most
  likely to be wrong — cursors, pagination, a provider's quirks — outside the
  memory: it did not travel in a bundle, `areev tool provenance` could not
  chase it, and the loop could not propose a `code_revision` against it. A
  Trigger now carries **`connector_tool`**, a reference to a Tool Definition
  whose `executor_uri` is the blob:

  ```bash
  areev trigger add --type polling --workflow <PLAN> --observer gmail \
    --connector-tool <DEFINITION> --interval 900 --dedup-key /message_id \
    --because "the AP desk watches this mailbox"

  areev trigger run --allow-executor <64 hex> --sandbox-cmd areev-sandbox
  ```

  A Definition rather than a `cas://` blob directly, because the runtime, the
  limits and the `capabilities` declaration that decides where the code may
  reach live on the Definition — a second place to write them would be a second
  place for them to disagree. The evaluator reads it through the *same*
  `pin_from_definition` `RunManifest::resolve` uses, and dispatches through the
  same `CodeExecutor`, so the sandbox argv, the capability registration and the
  cleared environment are one implementation rather than a second that drifts
  on the surface nobody watches.

  **The authorization does not travel with the code.** `--allow-executor` on
  the evaluating host is what runs it; without it the poll refuses with the new
  **`TRG-E012`**, naming the address to pin, before a broker is started and
  before anything is spent. The same code covers a Definition carrying no
  `executor_uri`, a declared runtime with no `--sandbox-cmd`, and a
  blob-reading module on an evaluator wired no memory locator. `connector` (the
  name) stays required beside `connector_tool`: the run id is derived from
  `(trigger, connector, dedup value)`, so revising the code must not renumber
  the runs. Both bindings take the same pin on `trigger_run`/`trigger_deliver`
  (`allow_executor`, `sandbox_cmd`, `executor_cache`, `executor_timeout_secs`),
  each reading its `$AREEV_RUN_*` variable, because a heartbeat is a cron line.
  `trigger show` now prints the connector and, when it is a grain, says so.
  `examples/grain-connector/` runs the whole path keyless and offline.

  *Breaking for direct constructors:* `areev_trigger::Evaluator` gained a
  `connector_code` field. A host that runs host-command connectors only passes
  `None` — and a trigger naming a connector Definition then refuses rather than
  falling through to `--connector-cmd`, because a fallback would run a
  different program than the declaration names.

- **`areev pack validate | install | export`** (#178) — a pack is an
  installable agent: a manifest, the grains it seeds, and the code blobs those
  grains name. It replaces the installer everyone was writing (blobs into the
  CAS, `executor_uri` rewritten to the address the bytes turned out to have,
  grains seeded in order, plan hashes checked) with one verb, and self-hosters
  get the same door a managed deployment does.

  ```bash
  areev pack validate ./pack                 # opens no memory at all
  areev pack install  ./pack --db agent.db   # refuses on a hash mismatch
  areev pack export   --db agent.db --ns ops --out ./pack
  ```

  References are **symbolic** — `"blob:<name>"` and `"grain:<id>"` — because an
  address is a measurement of bytes, not something an author can write down;
  install stores the blob, learns its address, rewrites the Definition,
  addresses *that*, and rewrites the plan that binds it. `expected_hash` is
  **refused, not warned**, with nothing written: a plan installed at a
  different address than the deployment expects has changed what runs, and
  every trigger pointing at the old hash is now pointing elsewhere. Grains are
  built and addressed with no store at all (content addressing is a pure
  function of the serialized grain), so `validate` speaks for `install` and can
  run in CI on a pull request. Export writes either reviewable grain JSON
  (refusing to write anything it cannot rebuild to the same address) or a
  bundle plus a manifest of what it must contain. Saved queries and templates
  travel too — a pack without them installs a trigger whose `context_query`
  names nothing. `docs/pack.md` is the reference; every agent under
  `examples/agents/` now ships a `pack/`, and `run-smokes.sh` asserts each one
  installs the same plan its language stacks mint.

- **Blessed shared tools: `http.call`, `mcp.call`, `a2a.call`** (#179) — three
  `wasm32-areev-io` blobs shipped in this repository with documented content
  addresses (`areev-tools/dist/blessed.json`, `docs/blessed-tools.md`). A pack
  binds one by address, declares where it may reach, and gets an outbound leg
  with no code of its own.

  The point is not convenience. A tool gateway that decides *where a request
  may go* is code everyone installing it must trust; `http.call` makes no such
  decision — it hands the request to the broker and the answer back, verbatim,
  in both directions. Where it may go, which method, which credential and which
  headers are the Definition's `capabilities` and the host's grant: data, in
  the memory, replicated with the tool, auditable without reading any code.
  Two Definitions may name the same blob with different declarations, which is
  exactly the shape a fleet wants. `mcp.call` and `a2a.call` add one JSON-RPC
  envelope and one unwrap, copying the caller's `arguments` **byte for byte**
  rather than round-tripping it.

  They are `no_std` with no dependencies (`http.call` is 2,598 bytes), built
  from `areev-tools/` — a standalone workspace, like `areev-sandbox` — and
  tested where the engine is: `areev-sandbox/tests/blessed_tools.rs` runs the
  committed bytes under real wasmi against a loopback broker stand-in, and
  asserts the import gate holds (a tool that declared no network does not get
  `areev::fetch` linked at all). CI checks that every published address is the
  address of the file beside it.

- **`examples/agents/sanctions-screening` seeds deterministically.** Its
  evalset Fact — the gate Rule E1 pins on the `screen` tool — was stamped with
  the wall clock, so the tool's hash, and therefore the desk's plan hash,
  differed on every seed. A plan hash that moves cannot be pinned by a pack,
  quoted in a README, or pointed at by a trigger, and nothing caught it because
  the cross-stack hash comparison needs two stacks and this desk has one. The
  seeder now pins `created_at` like every other grain it writes. Pinning it
  surfaced a second bug, fixed below rather than worked around: a dated evalset
  is exactly what `cold_grains` was mis-flagging as a retire candidate.


- **The benchmark harnesses can run against a Postgres memory** (#200).
  `AREEV_BENCH_DB` (or `receipts/run.py --db`) names the memory — a file path
  or a `postgres://…?schema=…` DSN, handed to `areev.Areev` verbatim — for the
  receipts experience/evaluation/regress legs and the PAST-Bench backend,
  which had both hard-coded a file under their work directory. What a schema
  cannot do is be copied, and the harnesses now say so: `evaluate.py` runs
  every arm on the one memory and produces arm A by rolling back on it
  (last, and for real — `dryrun.sh` takes arm A after the regress leg, with
  the new `--append`, since that rollback ends the state regress verifies);
  snapshots and per-pass learner copies are refused on
  a DSN; the loop policy a leg records goes to that leg's work dir. A DSN is
  printed and recorded only redacted. Unset, the published file-backed runs
  are unchanged — the keyless dry run's summaries match the baseline.
- **`areev::blob_get` works on the Postgres tier** — the whole class of
  attachment-parsing capability tools was unavailable on the backend the
  server tier actually runs on. `{"blob": {"read": true}}` (#106) is what lets
  a `wasm32-areev-io` module read the attachment a trigger's connector already
  filed, by address, read-only, every read journaled as a `blob_read`
  Observation. On PostgreSQL the broker answered `501`: the read is lock-free
  because it goes to the `.blobs` sidecar without opening the database, and
  that sidecar is an embedded-backend thing. The documented alternative — the
  tool opens the memory itself — needs handing the tool a credential to the
  memory, which is exactly what a capability tool exists to avoid.

  `read_blob_offline` now serves a `postgres://…?schema=…` locator too: one
  short-lived connection of its own, one schema-qualified `SELECT` against the
  in-schema `blobs` table, closed on return. It still never opens the memory,
  so it cannot contend with the run holding it — and on this backend there is
  no exclusive lock to avoid in the first place, which makes it cheaper than
  the embedded case rather than harder. Qualifying the table (#181) keeps it
  independent of `search_path`, so it is safe behind a pooler. Blobs are never
  sealed here (the blob key derives from the page cipher, which Postgres
  refuses), so the sealed branch cannot arise.

  `areev blob get` lifts the same restriction: it skipped the lock-free path
  for a DSN, a workaround for the limitation this removes, so the two
  blob-reading surfaces now behave identically
  ([#202](https://github.com/AreevAI/areev/issues/202)).

- **CAL can summarise by frequency, extract from text, and navigate a JSON
  payload** (#209, #210, #211, #217) — three reads that could only be done by
  over-fetching and finishing the job in host code, which also defeated
  `BUDGET` (the budget was spent on the rows about to be discarded). The
  spec-level decisions are recorded in
  [`docs/oms-1.7-amendments-cal-expressiveness.md`](docs/oms-1.7-amendments-cal-expressiveness.md).

  **Per-group counts** (#209). `GROUP BY <field>` followed by `COUNT` now
  projects one row per group carrying its size, **most frequent first** (ties
  by key ascending, so the answer is reproducible across backends and runs):

  ```sql
  RECALL tools WHERE is_error = true LIMIT 400 GROUP BY tool_name COUNT
  ```

  That combination previously returned the plain total — identical to `COUNT`
  alone, silently discarding the grouping — so **no new syntax was needed**
  and no meaningful answer is taken away. Frequency is how a memory says what
  *matters*: "which tool fails most", "which topic does this user raise most",
  "which policy is cited most". Render it with the new `group.*` template
  variables (`{{group.count}}x {{group.key}}`), or make an `ASSEMBLE` source
  of it so a frequency roll-up is a *section of a prompt*.

  **The key can name two things at once** (#217). "Which tool fails most" is
  the first question anyone asks a memory of tool calls; "and with what" is
  the half that says what to do about it — an agent told `phone.login` failed
  six times learns less than one told it failed with a 401. `GROUP BY` takes
  up to four fields, joined into one key with ` · ` (`CAL-E123` past that),
  and the parts render individually:

  ```sql
  RECALL tools WHERE is_error = true LIMIT 400 GROUP BY tool_name, tool_content COUNT
  ```
  ```
  DEFINE TEMPLATE top_failures ELEMENT {- ({{group.count}}x) {{group.key.0}}: {{group.key.1}}}
  → - (6x) phone.login: Response status code is 401
  ```

  Three related gaps close with it. **A Tool's body is reachable from a
  template**: it is projected as `tool_content` (the compact key `cnt` expands
  to it), which every built-in format printed and no template variable named,
  so a CAL-rendered block could say which call failed but never how —
  `{{grain.tool_content}}` resolves, `{{grain.content}}` projects the same
  text on a Tool, and `tool_content` is queryable and groupable.
  **A `LIMIT` after `COUNT` binds**, so a top-N of a ranking is a top-N
  (`total_available` still reports the whole ranking's size, so a page never
  reads as the whole answer). And **`CAL-W018`** now announces a `GROUP BY`
  key no grain carries, which used to return a single empty-key group —
  indistinguishable from a ranking with one dominant value.

  **Extracting filters** (#210): `first_line`, `split("<sep>", n)`,
  `strip_prefix`, `strip_suffix`, `between("<open>", "<close>")`, and
  `match("<pattern>"[, n])`. Memories store text people wrote, and titles,
  ticket ids, error codes and thread keys all live inside it:

  ```
  {{grain.object | between("[", "]")}}     → Q3 close handoff
  ```

  **A JSON path accessor** (#211), in both a render and a filter:

  ```
  {{grain.object | get("error.code")}}     → rate_limited
  ```
  ```sql
  RECALL tools WHERE input.app = "phone"
  RECALL facts WHERE object.error.code = "rate_limited"
  ```

  `record_tool_call` round-trips a tool's `input` as parsed JSON, so Python
  and Node hosts already received the structure — it was specifically the CAL
  path that could not see inside. A field name may now be a dotted path of up
  to 8 segments (it accepted one dot before, which did not reach the shape a
  stored error envelope actually has), and a value stored *as a JSON string*
  navigates identically to a parsed one. **A path that does not resolve is
  UNKNOWN**, so navigation inherits the fails-closed rule rather than adding
  one: `input.app != "phone"` does not widen to everything.

  The filter set stays **closed** — `DESCRIBE CAPABILITIES` reports it, and
  OMS conformance means two implementations must render a grain identically.
  Bad arguments are refused when the template is *defined* (`CAL-E049`), not
  when it renders, so "what will this saved query show me?" stays answerable
  by reading it; rendering itself stays total, because one unparseable grain
  must not fail the render of the other 199. `match` uses a non-backtracking
  engine — no backreferences, no lookaround — because a template runs over
  untrusted grain content on every turn, where a backtracking regex is a
  denial-of-service primitive. Patterns are length-capped and compiled through
  a bounded cache; extractor input is clipped at 64 KiB; paths are
  depth-capped.

  Deliberately **not** added: host-registered functions (a template calling
  one would render differently depending on who opened the file, breaking the
  property that makes saved queries worth having — the registry travels *with*
  the memory), a general expression language in templates, and any
  transformation that parses, mutates and re-serialises. For new corpora the
  paved road is still to store the shape you want to read: two fields rather
  than one payload, which makes the value filterable as well as renderable.

- **World-time validity is queryable and renderable** (#206). `valid_from`,
  `valid_to`, `system_valid_from` and `system_valid_to` are `GrainCommon`
  fields on every grain type — serialized since 1.0, read by the loop's
  `staleness` analyzer, and present in every JSON payload — but they were
  absent from CAL's filterable set, so the one read that makes a validity
  window worth writing answered `CAL-E060`. They now filter and sort on every
  type with the usual comparators and `IS NULL`, resolve in templates
  (`{{grain.valid_to | date}}`), and appear in `DESCRIBE FIELDS`. "What is
  currently valid" is a query:

  ```sql
  RECALL facts WHERE namespace = "desk"
    AND (valid_to IS NULL OR valid_to > 1788866000000)
  ```

  This is what a waiver, a delegation, an out-of-office or a price valid until
  a date needs. Every host previously over-fetched and post-filtered, which
  also defeated `BUDGET` — the budget was spent on grains about to be
  discarded.

- **The container image and the release archives carry `areev-sandbox`.**
  `runtime: "wasm32-areev"` and `"wasm32-areev-io"` dispatch a pinned blob to
  the sandbox, but the sandbox is `publish = false` and shipped in nothing: the
  image built only `areev`, and the release attached only `areev`. So a
  container deployment could install a capability tool and never run one — the
  tier was unreachable from the deployment shape it most obviously exists for.
  Both binaries are now built from one tree in one stage, `areev-sandbox
  --version` agrees with `areev --version`, and `--sandbox-cmd areev-sandbox`
  resolves on the image's `PATH`. The sandbox travels **inside** each release
  archive rather than as a separate asset, so the pair cannot be mixed across
  versions. `docker.yml` proves the whole path: it authors a code-carrying
  Definition over MCP, starts a run against it, and asserts the sandbox judged
  the bytes rather than the host failing to reach one
  ([#203](https://github.com/AreevAI/areev/issues/203)).

- **A preview channel for unreleased changes.** npm and PyPI published only on
  a published GitHub Release, so proving a Core change against a downstream's
  test suite cost a full release cycle — and the only alternative, pointing a
  consumer at a local checkout, cannot run in CI. `release-npm` and
  `release-pypi` now take a `preview: true` dispatch from any branch and
  publish `X.Y.Z-preview.<run number>` under npm's `preview` dist-tag and as a
  PyPI pre-release, so `npm install @areev/areev` and `pip install areev` are
  unaffected. Nothing is committed: `scripts/stamp_preview.py` stamps at build
  time, and `check_versions.py --preview` still refuses a tree whose two
  published sites disagree ([#204](https://github.com/AreevAI/areev/issues/204)).

  Two things constrained the shape. The version had to be
  `-preview.<number>`, not the `-preview.<sha>` first proposed: PEP 440
  numbers its pre-releases and PyPI rejects the `+local` segment a sha would
  need, so a sha is publishable to npm and not to PyPI. And only
  `package.json` and `pyproject.toml` may be stamped — crates depend on each
  other as `version = "1.7.0"`, and Cargo does not match a prerelease against
  `^1.7.0`, so stamping the workspace makes every inter-crate requirement
  unsatisfiable and nothing builds at all.

### Fixed

- **`areev tool provenance` reports the runs that executed a tool.** The doc
  said it chained code "to the runs that executed it"; the command answered
  `runs_touching: 0` for a Definition a run had just executed. The walk behind
  it is a PROVENANCE walk — `derived_from` up, the reverse index down — and a
  journal's result grain supersedes its intent while neither supersedes the
  Definition, so there was no edge to find. What connects a run to the code it
  ran is §8.4's `spec_hash`, and `runs_touching` now reads it backwards for a
  Tool Definition (`Areev::runs_executing`, exposed on its own too). A bounded
  scan rather than an index read, because `spec_hash` is a field and not a
  `related_to` link — and skipped entirely unless the target IS a Definition,
  so no other caller pays for it. Pinned by a conformance case on both
  backends; `examples/blessed-tools/` asserts both runs, the refused one
  included, because a request the broker turned down is still a run that
  touched that code.

- **`cold_grains` no longer proposes retiring a Rule E1 gate.** An evalset a
  live Tool Definition pins is read by the gate at review time, never through
  recall, so it aged past the grace window and was flagged as a retire
  candidate — the one grain that must not be retired, since removing it removes
  the gate a `code_revision` has to pass. Recall counts measure whether a fact
  informs *answers*; that fact's job is to judge *code*. Facts pinned as
  `evalset_hash` by a live Definition are now excluded, and deliberately only
  those: a general "any hash a live grain mentions" sweep would also swallow
  the evidence a pending recommendation cites, and since this analyzer's own
  findings cite the fact they flag, every cold finding would suppress itself on
  the next run. `examples/agents/sanctions-screening` drops the config widening
  it needed and asserts the clean result instead.

- **An `ASSEMBLE` source can carry its own pipeline, and no longer drops a
  nested assembly's grains.** Sources had no pipeline at all, so a source
  could not be ranked, bounded or summarised in place. Separately,
  `assemble.rs` carried a **second copy** of `extract_grains` that had drifted
  from the executor's: it saw only the `Grains` payload, so a nested
  `Assembled` result silently contributed nothing to the enclosing assembly.
  There is now one extractor.

- **A `WHERE` predicate on a field the grain does not carry no longer matches
  everything** (#207). `object` is a real field name in general — Fact,
  Observation and Goal all declare it — but means nothing for a Skill, and the
  per-grain evaluator read that absence as `false`, which made every *negation*
  of it `true`. So `RECALL skills WHERE object != "retired"` returned every
  skill, the retired one included, with nothing in the payload to distinguish
  "the filter ran and matched everything" from "the filter did not run" — the
  opposite of the fails-closed contract §3.4 states. Evaluation is now
  three-valued: absence is UNKNOWN, `AND`/`OR` combine by SQL's truth tables,
  and UNKNOWN does not match. `!=`, `NOT (… = …)` and `NOT IN` all narrow.
  Nothing that already matched stops matching (`T ∧ U` and `F ∧ U` already
  collapsed to no-match, `T ∨ U` already matched), and the omit-default
  discriminators still resolve their defaults, so `kind != "definition"` keeps
  returning legacy execution grains. `areev-trigger`'s composite gates share
  the evaluator and mean the opposite by an absent field — "this member has not
  fired" is definite, not unknown — so `gate_satisfied` now materializes every
  referenced member instead of encoding the answer in a gap.

- **`description` is queryable on skills** (#207). Required on the Skill struct
  since 1.4 but absent from the registry's `queryable_fields`, so the one field
  every Skill must carry was the one `WHERE` refused with `CAL-E060`.

- **`{{… | date}}` renders the right year** (#206). Every timestamp a template
  can name is epoch milliseconds, but the filter handed its input to a
  seconds-based formatter, so `{{created_at | date}}` rendered *58657-02-23*
  for a grain written today. `relative` never had the bug because
  `humanize_time(created_ms, now_secs)` names its units. `_now` is milliseconds
  too, so the whole filter surface speaks one unit; `format_epoch` keeps its
  public seconds signature.

- **`ASSEMBLE` says what its budget dropped** (#208). `ASSEMBLE` applies a
  token budget whether or not the caller writes one — 4000 by default, ceiling
  16000 — and when it bound it discarded the tail of each source in silence.
  The payload actively hid it: `total_available` reported the **post**-budget
  count, so `grains.len() == total_available` held for a truncated assembly
  exactly as it did for a complete one. Measured on a real memory, 229
  matching grains came back as 80 with `warnings: None`.

  A budget that drops grains now emits **`CAL-W017`**, naming the sources and
  the counts, and saying whether the budget was written or defaulted:

  ```
  CAL-W017: the default BUDGET 4000 tokens dropped 130 of 200 grains from
  source(s) [e] — this assembly is a window, not the whole match.
  ```

  `total_available` is now the **pre**-budget count, so the drop is computable
  rather than announced only in prose; each source's `grain_count` still
  reports what survived. `docs/cal-reference.md` states the default and the
  ceiling where `BUDGET` is documented — neither number appeared there, so "no
  `BUDGET` clause" read as "no budget".

  The default itself was kept rather than removed: an unbudgeted assembly that
  returned everything could overflow the context window it is being composed
  for, which is the worse failure. Silence was the defect, not the number.

  Why it matters: a host composing a prompt from an assembly had no way to
  detect that its rules, its policies or its recent turns were trimmed — it
  would publish a number produced from a truncated prompt and never know.
  `RECALL` has announced the same kind of cut as `CAL-W015` since 1.5.1.

- **The console shows CAL warnings.** Every other surface honoured the
  "silence means the query did what you asked" contract — the bindings and the
  MCP tool return `warnings`, the CLI prints them to stderr — but the console
  received them from `POST /api/cal` and dropped them on the floor. That was
  the worst place for it: this is the surface a person reads an answer from,
  and a warning is exactly the news that the answer is a window rather than
  the whole match. All seventeen (`CAL-W001`–`W017`) now appear above the
  result on the Query page, in plain language, with the `CAL-Wnnn` code shown
  only in Developer mode.
- **`record_tool_call` takes `ns` in both bindings** (Python `ns=`, Node
  `ns`), exactly as `add()` does. It was the only write on either surface that
  could not leave the session namespace, so a host recording calls into
  per-domain child namespaces (`domain.phone`, `domain.spotify`) had to open a
  second handle — which the single-writer registry refuses (`STO-E002`).
- **An empty `tool_env` / `toolEnv` in the bindings now means "clear to the
  minimal set", as the CLI's `--tool-env ""` does** (#197). Both bindings
  filtered the empty string out before building the policy, so the strictest
  request — clear the environment, admit nothing beyond what a command needs
  to start — was answered with the loosest posture, inherit everything. A host
  wanting the clear-only policy had to name a variable already in the minimal
  set just to select it. `None` / `null` keep the inherit default, unchanged.
  Pinned in both bindings' tests, on the run executors and the trigger
  connector.
- **The credential broker reaches the bindings and `areev serve`** (#201).
  A `wasm32-areev-io` tool's `areev::fetch` is answered by the broker, and
  the broker was built only from CLI flags — so a host driving runs through
  Python or Node, which is what a service does, could persist a capability
  tool, pin it, point at the sandbox, and still have every fetch fail. The
  parser moved into `areev-run` as `EgressSpec`, the one behind
  `--credential`/`--allow-host`/`--tool-egress`/`--credential-ttl`/
  `--resolver-env`, and every surface calls it: the same-named parameters on
  `run_start`, `run_resume`, `trigger_run` and `trigger_deliver` in both
  bindings (`credentials`, `allow_hosts`, `tool_egress`, `credential_ttl_secs`,
  `resolver_env`; camelCase in Node) take the flags' spec strings verbatim,
  and `$AREEV_RUN_CREDENTIAL`, `$AREEV_RUN_ALLOW_HOST`,
  `$AREEV_RUN_TOOL_EGRESS`, `$AREEV_RUN_CREDENTIAL_TTL` and
  `$AREEV_RUN_RESOLVER_ENV` are the server-bound spellings for `areev serve`
  — and the out-of-band fallbacks for the CLI, like the rest of the family.
  The blob door (`{"blob": {"read": true}}`) is wired on the same path, a
  refusal from a binding-driven run is journaled exactly as from the CLI
  (`403` + `RUN-E022`, an Observation in `agent:harness`), and a parity test
  drives one capability tool from Node, Python, the CLI and the MCP server
  with one declaration and one set of grants.

### Changed

- **`areev-bench` harnesses now run on the engine's own surfaces.** Every
  model-facing prompt block that can be is a saved `ASSEMBLE` query registered
  in the memory file and rendered by a registered template, so a memory handed
  to someone else carries how to read it. Tool calls are recorded through
  `record_tool_call` rather than flattened, AppWorld's evidence lives in
  per-app child namespaces read through `"appworld.*"`, and the governed
  learning pass is an `areev run` workflow whose review node parks for a human
  — so separation of duties is enforced by the runtime (`RUN-E012`) instead of
  by harness convention. Ordering within a section relies on a source's own pipeline (#209/#215, above). The moves are byte-for-byte against the renderers
  they replace, gated by `crates/areev-bench/scripts/parity_check.py`; the
  new-track blueprint is `crates/areev-bench/BENCH-TEMPLATE.md`.

- **A failed spawn names the command that failed, not the blob.** The
  code executor formatted every spawn error as `spawn <materialized blob
  path>`, including when the thing that could not be spawned was the **sandbox
  binary** — so a missing `--sandbox-cmd` reported a path that exists and is
  not the problem, which is exactly the diagnosis a host without a sandbox
  needs to make.
- **`areev-sandbox` joins the version lockstep.** It sat at 1.6.0 against a
  1.7.3 workspace — two minors of silent drift on a binary whose whole job is
  to be the security boundary paired with the engine. It is now the sixth site
  `scripts/check_versions.py` asserts, alongside the other detached package
  (`areev-js`), and it gained the `--version` flag that makes the pairing
  checkable at all.

## [1.7.3] — 2026-09-07

### Added

- **Postgres: the store holds nothing on the session** (#181, second
  increment). Every statement names its tables schema-qualified, the bootstrap
  lock is transaction-scoped (first increment), and a cached prepared
  statement the backend no longer knows (`26000`) is re-prepared and retried.
  Runtime DDL and catalog probes bind the schema name instead of reading
  `current_schema()`. So a transaction-mode pooler in front of the store is
  now correct rather than a documented hazard: the full conformance suite runs
  with `RESET ALL` issued before every statement outside a transaction
  (`tests/pg_chaos.rs`), which drops `search_path` and every GUC, and the
  suite passes through a real PgBouncer in transaction mode. The one thing
  the pooler must do itself is track prepared statements across backends —
  the driver names every parameterized statement, so PgBouncer 1.21+ with
  `max_prepared_statements > 0`, or session mode; a `26000` inside a
  transaction now says exactly that instead of reading like a driver bug.
  What is still session-scoped, and documented as such: `hnsw.ef_search`. The
  in-process pool itself (one pool across N schemas) is the third increment.

- **The vector-at-scale surface reaches the bindings and the CLI** (#141).
  A host that manages memories through Python or Node alone could not build
  the ANN index, could only write embeddings one transaction at a time, and
  had no way to check what the index cost it. Now, in both bindings and as
  `areev vector-index <status|build|drop|check>`:
  - `add_embeddings([{hash, vector}, …])` writes a batch in **one
    transaction**, chunked two hundred rows per statement — the per-vector
    path was a transaction and three round trips each, which is why a bulk
    re-ingest measured round-trip-bound (~545 vectors/s) rather than
    embedding-bound. All-or-nothing: one unknown hash or wrong dimension
    refuses the batch before anything is written, and the error names the row.
  - `ensure_vector_index(m, ef_construction, ef_search)`, `drop_vector_index()`
    and `vector_index()` expose the pgvector HNSW index that was Rust-only.
    The embedded engine still refuses to build one (`STO-E007`) and reports
    `index: null`, because its reads are exact.
  - `vector_recall_check(queries, k, ns, ef_search)` grades the index against
    the exact scan **with the caller's own query vectors** — recall is a
    property of the embedding model's geometry, so nobody else's number
    transfers — over the scope named (the CLI insists on `--ns`, because the
    default scope is a narrow one the structural index serves exactly). The
    report carries the `ef_search` it graded at, and `ef_search` retunes the
    session first without a rebuild. With no index built it reports
    `recall: 1.0, index: null` and runs nothing; a scope with no exact
    neighbours reports `recall: null` rather than a 1.0 that reads as a pass.
    The exact side and a tuned `ef_search` both survive a Postgres reconnect,
    which a replayed read would otherwise land on default settings.
  Pinned on both backends by the conformance case
  `bulk_embeddings_land_atomically`. Not added as an MCP tool: building an
  index is an operator action, not something an agent should reach for
  mid-conversation.
- **Recorded late:** 1.7.0 made `nearest_vector` / `nearest_semantic` accept
  an `"org.*"` prefix scope like every other plural read, and added the
  `(ns, s, p)` index that turns a filtered vector scan from a full scan into a
  seek (`46e6a79`). The change shipped without a changelog line; this is it.
- **Postgres: a steady-state open issues no DDL at all** (#180). A bootstrapped
  schema is stamped with its schema version in `meta.pg_schema`; the next open
  reads that stamp before taking any lock and skips the 43 DDL statements, the
  advisory lock, the counter seeding, the dictionary slurp and the open-time
  `meta` writes entirely. Measured on loopback with `log_statement=all`: **67
  statements down to 2, 40 DDL down to 0**, and open latency from ~26 ms to
  ~5 ms. A schema written by an earlier build migrates once, then goes quiet.
  The stamp is guarded by a test that digests `PG_SCHEMA`/`PG_SEED` and fails if
  either changes without a version bump — a stamp that lies would make a stale
  schema skip a migration it needs, which is the failure mode where every recall
  silently returns empty.
- **`areev provision --db <DSN> [--schema NAME]`** (#180) creates or migrates a
  Postgres memory's schema ahead of use, telemetry tables included, so the first
  real request writes nothing. It dispatches before the memory is resolved, like
  `auth`, because it names no memory to open.
- **`?provision=never` on a DSN guarantees no DDL on the request path** (#180).
  An absent or stale schema is refused with the new **`STO-E008`**, which names
  which of the two operator actions is needed, with no lock taken and no
  `CREATE SCHEMA` attempted.
- **`--mount` accepts a Postgres DSN** (#184), so a cross-memory `ASSEMBLE` can
  span backends. Mounts now open **read-only on both backends**: a `SELECT`-only
  Postgres role can back one, and a mount path that does not exist is refused
  (`STO-E005`) instead of being created as an empty memory. Mount specs split on
  `,` only before another `alias=`, so a multi-host DSN survives, and a DSN is
  redacted everywhere it is printed. Mounting the primary's own schema is
  refused rather than silently double-counted.
- **Run spans carry the OpenTelemetry GenAI semantic conventions** (#186).
  `areev run --otel-endpoint` now emits `chat {model}` (CLIENT), `execute_tool
  {tool}` and a synthesized `invoke_agent {node}` per abstract-node attempt,
  with `gen_ai.provider.name`, request model/max_tokens/temperature,
  `gen_ai.usage.*`, `gen_ai.response.finish_reasons`, `gen_ai.tool.name`/
  `.call.id`/`.type`, `gen_ai.agent.name`/`.id`, `gen_ai.conversation.id` and
  `error.type`. A GenAI-aware backend classifies them with no Areev-specific
  configuration, and every `areev.*` provenance attribute is kept beside them —
  that is what takes a slow span back to `areev run-trace`. There is deliberately
  no cost attribute: Areev prices nothing, and an always-zero one would read as
  "this run was free" rather than "nobody priced it".
- **Run events reach the Python and Node bindings** (#182). `on_event=` and
  `onEvent` on `run_start`/`run_resume` receive each run event as the same JSON
  line `areev run start --events` prints. Observational by construction: the
  journal is byte-identical with or without a subscriber, a slow callback delays
  events rather than the run, and a raising one is reported unraisable. Attaching
  a callback also enables model `TokenChunk` deltas. Note that `RunFinished` is
  emitted only at a terminal outcome, so a run that parks on a human gate ends
  its start leg at `AskRaised` and finishes on the resume leg.
- **`areev tool provenance` chases the executor blob** (#179, partial), reporting
  whether the pinned code is present and how large. Read lock-free, so it answers
  while a run still holds the file. A non-`cas://` scheme reports "not content
  addressed" rather than "absent" — "we did not look" and "it is not there" are
  different findings.
- **Compliance profile presets** ([`docs/compliance-profiles.md`](docs/compliance-profiles.md)).
  GDPR, healthcare and financial deployments assembled as the exact commands
  that configure them, with the distinction that decides whether an auditor's
  answer holds: a **file-truth** (`anonymize set`, `retention set`,
  `retention floor`, `hold set`) travels with a copy of the memory, a **host
  config** (`--anonymize-egress`, `--no-destructive-ops`, `--read-only`) is a
  cap for one process and is forgotten on the next open. Documentation of
  existing flags — no new surface
  ([#190](https://github.com/AreevAI/areev/issues/190)).
- **A read-only open in the Python and Node bindings.** `--read-only` has
  refused every write on the CLI since 1.7.0, but the binding constructors
  had no equivalent, so a console, an evaluator or an analytics reader
  embedding Areev had to hold owner-grade credentials to do nothing but read.
  `Areev(path, read_only=True)` / `new Areev(path, …, readOnly)` opens the
  same way: writes fail with `STO-E004`, an absent memory is never created,
  no telemetry sidecar is attached (its flush is a write), and on the
  Postgres backend the open issues no DDL at all — SELECT-only verification
  instead, which is what makes a role holding only `USAGE` and `SELECT` a
  workable identity. An explicit `index_text`/`indexText` is refused
  alongside it, because that re-stamps the file's declaration
  ([#183](https://github.com/AreevAI/areev/issues/183)).
- **`--tool-env` names what a host tool's environment contains, instead of
  what it must not.** Host tools spawn under `InheritExcept`: everything this
  process holds minus the variables Areev was told hold secrets. A host whose
  own environment carries secrets Areev never named therefore had to keep
  them out of the process entirely. `--tool-env VAR,…` (env
  `$AREEV_RUN_TOOL_ENV`, `tool_env=` in Python, `toolEnv` in Node) clears the
  environment and passes only the named variables plus the minimal set a
  command needs to start. It reaches `--tool-cmd`, a `trigger run` connector,
  and a pinned **native** blob; the sandbox seam already cleared and is
  unchanged. An allow list cannot re-admit a variable Areev was already told
  holds a secret — the name is dropped and reported, so #100's invariant stays
  unconditional; `--resolver-env` remains the one deliberate exception, for
  credential resolvers ([#188](https://github.com/AreevAI/areev/issues/188)).
- **A refused read-only open leaves nothing behind.** Deriving a passphrase key
  writes a `<path>.kdf` sidecar when one is absent, which is right when
  creating an encrypted memory and wrong on the way to a refusal: pointing
  `--read-only --passphrase-env` at a path that does not exist failed correctly
  with `STO-E005` but left a stray `.kdf` file. The precondition is now one
  shared rule (`areev_store::read_only_requires_existing`) applied before the
  derivation on all three surfaces — CLI, Python and Node.
- **The loop's cadence and the Verify gate's schedule take the deployment's
  own units, as policy.** `outcome_evalset.checkpoints` schedules a
  re-measurement in `after_ms`, `after_runs` (evalset runs journaled since the
  apply) or `after_grains` (grains written since it); a bare integer still
  means milliseconds, so every policy, snapshot and state blob written before
  reads unchanged, and the 1d / 7d / 30d default is untouched. It exists
  because a schedule counted in days is inert on a deployment that finishes in
  minutes: on PAST-Bench the default fired zero verdicts and zero reverts
  across 78 governed runs — the half of governance the receipts harness had
  just proved worked — because a family finishes in seven minutes and the
  first checkpoint was a day away. `cadence` lifts the per-call run gate
  (`--min-new`, `--if-stale`) into the policy file so CLI, MCP and console
  share one rhythm, and adds `every_events` (turns) and `every_sessions`;
  unset, a pass is due whenever it is called, as before. Flags override the
  block; a sweep always runs. A skipped pass reports `cadence_not_due`.
- **DISCOVER may author a Skill.** A `skill` proposal — description,
  `when_to_use`, ordered steps — derived from a trajectory that succeeded,
  named by its target, placed in the evidence's namespace, and superseding a
  live skill of the same name rather than duplicating it. Governed like every
  draft: GROUND, VERIFY, the confidence floor, a review with a BECAUSE, never
  auto-applied. `skills: {enabled, min_steps}` in the policy, default on with
  two steps. Measured need: on PAST-Bench the agent performed a procedure
  correctly on every seed and then, asked whether to save it, said "nothing
  to save"; the store was empty at evaluation and scored below having no
  memory. Every skill had depended on the model volunteering one mid-task.
- **DISCOVER may author a plan.** A `plan` proposal — steps bound to tools
  the cited evidence shows were called, edges with conditions in the
  runtime's frozen grammar — applies as a Workflow grain (validated by the
  substrate's plan validator before it can be stamped applicable) beside a
  Skill of the same name, in one batch; a live pair of that name is
  superseded. `plans: {enabled, min_nodes}`, default on. A skill is what a
  model reads; a plan is what the runtime can check, run, journal and patch.
- **The Verify gate asks a second question: does the premise still stand?**
  When a grain an applied recommendation cited is later superseded by a
  different value or retracted, the gate records `drifted` and
  `outcome_review` proposes the revert (`outcome.premise_drift`). A
  value-identical supersession is not drift. `premise_drift: true` by
  default. Measured need: a governed lesson encoding a superseded rule cost
  0.32 on PAST-Bench's migration family.
- **`min_evidence`** (default 1): the fewest distinct grains an LLM draft
  must cite to be offered as a change. Under it the draft is stored and
  reviewable but applies as nothing; the funnel counts the demotions as
  `advisory_thin_evidence`. An audit of 88 governed decisions found 15 of 28
  approvals had generalised one instance into standing policy — `2` is the
  setting that audit argues for.

- **Batch reads for the bench harness.** `evaluate.py --batch` submits a
  whole held-out arm as one job to a batch endpoint — the OpenAI files
  shape or OpenRouter's inline `/api/beta/batches` (`scripts/batch_toolcall.py`,
  self-checked against a local mock of both and validated live on
  OpenRouter and on OpenAI directly); usage rows carry the tier's discount
  or the provider's reported cost and `cost.py` uses them. The per-call
  adapter takes `--base-url`/`--key-env`, so an OpenAI model is called at
  `api.openai.com` on both paths. The governed stream stays
  synchronous by construction.
- **`crates/areev-bench/CURVE.md` — the tuning learning curve.** Three
  seeds of a governed deployment over 320 real FARA registration forms
  (VRDU), the memory snapshotted at 20/40/80/160/320 documents and a
  Qwen3-1.7B tuned at each checkpoint from scratch and continually, read
  against registrants it never learned from. The tuned model goes from 55%
  to 93% exact and passes the LLM carrying the loop's rules (79%) between
  80 and 160 documents; the memorisation gap shrinks from +42 to +3 points;
  a Qwen3-0.6B reaches 95%. The verify leg fed the loop its own checkpoint
  reads and it reverted the one rule that had cost twenty points; two
  engine defects that leg exposed are fixed under *Changed*. The baselines
  every earlier chart had are on the same sets: no memory is flat at 25%,
  and mem0 — as installed, domain-prompted, and framed under the governed
  arm's own instruction header — is 26% at every checkpoint on every seed,
  with `mem0_peek.py` showing why (the stores hold the conventions; a
  top-ten drawn by similarity to a form returns other forms); given the
  task question and instruction framing the governed loop provides by
  construction, mem0 reaches 64% on the final read and is held there by
  the conventions nobody reviewed. All API cost
  for three seeds: $0.66; mem0's three-seed runs cost $0.87 and $1.02. `results/curve-vrdu-2026-09-06/` holds the
  numbers and receipts; `publish_curve.sh` regenerates them.
- **The DISCOVER objective is host policy.** `discover_objective` in the loop
  policy file selects what the LLM proposer optimizes for: `review_queue`
  (the default, byte-for-byte the previous instruction apart from the cite
  sentence) makes abstention free and a wrong finding cost double — right for
  a queue a person triages; `learner` makes withholding a lesson over a
  recurring failure cost the same as a wrong one, for a deployment that has
  to improve from this pass. It changes the scoring paragraph only: GROUND,
  VERIFY, the confidence floor and human review are identical under both.
- **An authored lesson can carry an outcome metric.** `outcome_evalset` names
  the evalset every applicable LLM-authored proposal is re-measured against
  after apply, so the Verify gate finally has something to re-run for the
  proposals a reviewer approves from prose alone — nothing errors when a
  lesson is merely useless. Baseline is the newest run journaled before the
  proposal, current is a run journaled after the apply; no baseline run means
  no metric rather than a fabricated one, and the direction is mandatory.

- **The tuning learning curve.** `build_vrdu_reg.py` adds VRDU's FARA
  registration forms — the first corpus with a real filing-date timeline
  and an entity key — and `dataset.split_entity` holds out *organisations*
  rather than documents, so a tuned model's unseen-set score can claim
  generalisation by construction. `curve_tune.sh` snapshots the governed
  memory at geometric checkpoints (`run.py --snapshot-at`) and trains two
  adapters at each — from scratch on everything so far, and continually
  from the previous adapter on only the documents since — reading both
  against unseen and seen held-out sets beside the LLM carrying the same
  rules. `slm_train.sh` scales epochs to the corpus and keeps the
  lowest-validation-loss checkpoint, never the last; the base is Qwen3.5-2B
  under `mlx_lm`. The prompt path no longer scans the whole namespace, so a
  320-document deployment stays under CAL's grain cap. `CURVE.md` is the
  pre-registration; DocILE is the planned scale run.

- **Four ways to carry something forward, on cost and accuracy.** The
  receipts harness gains three arms beside the governed loop and a cost
  ledger under all of them. `mem0_arm.py` runs real `mem0ai` 2.0 as its
  README says to — `add()` after each document, `search()` before the next —
  in three modes (as installed, with its domain hint, and verbatim store),
  against the same receipts, agent and held-out set as the governed run.
  `slm_corpus.py` / `slm_train.sh` / `slm_eval.sh` turn a run's governed
  memory into a LoRA on Qwen2.5-1.5B with `mlx_lm` and evaluate it — tuned
  and untuned — under arm B's exact prompt through `scripts/slm_serve.py`,
  which speaks the agent adapter's JSON-on-stdio contract to a local
  `mlx_lm.server`. `structure.py` reads one fixed rule set under every
  (position × format) cell, formats rendered by CAL's own renderers.
  Every model call now meters into `$AREEV_USAGE_LOG` — both OpenRouter
  adapters, mem0's SDK calls, and the local model — and `cost.py` prices it
  from a pinned table, flagging anything unpriced. `fourway.py` assembles
  the accuracy and cost charts; `FOURWAY.md` records the result.

- **A ledger that changes its mind.** `receipts/ledger_profile.py` gains
  `regimes`, letting a filing convention be *replaced* mid-deployment on
  top of the `arc` that already lets requirements arrive, and `drift.sh`
  drives three arms across it — the governed loop, the same memory rendered
  ungoverned (every correction verbatim, nothing retracted), and the frozen
  day-one agent — scored per checkpoint under the convention in force.
  `DRIFT.md` records the one clean seed: governed 173 against ungoverned 60
  before the change, and after it the loop learned the new rule and kept
  the old one beside it, while the Verify gate marked every rule `held`
  against a day-one baseline the agent still beats. A measured negative on
  the revert half, with the two missing mechanisms named.

- **The receipts ablation is complete — a 2×2, four cells.** `RECEIPTS.md`
  separates the two things run 2 changed at once. Both are large main
  effects from run 1's baseline: naming the observer in the evidence
  projection is **+175**, swapping the learner is **+185**, both together
  **+312**, with arm A at 97/720 in all four cells. Cell D was
  pre-registered as an expected null and is not one; the correction, and
  what it costs the authoring-rate diagnostic that predicted it, are
  published beside the result.

- **A second public corpus for the receipts experiment.** `ADBUY.md` repeats
  it on VRDU ad-buy forms — real US FCC political-advertising invoices, ten
  times a receipt's length, filed `YYYY-MM-DD` where SROIE files
  `DD/MM/YYYY`. Three seeds, 468 paired wins and 0 losses in 840 trials, and
  the day-one field improves (the claim `RECEIPTS.md` records as
  unsupported). `receipts/build_vrdu.py` builds it, stdlib only; nothing from
  the corpus is committed.

### Changed

- **`drop_vector_index` on the embedded backend is a no-op, not `STO-E007`.**
  What a drop promises is exact reads, and a file memory's reads are exact
  already; Postgres answers the same way (`DROP INDEX IF EXISTS`) when none
  is built. A caller branching on the old refusal sees `Ok` now. Building one
  there still refuses, because that would promise an index that cannot exist.
- **`set_grain_embedding` is the one-row form of `set_grain_embeddings`** and
  so now honours `--read-only` (`STO-E004` instead of a raw permission error,
  and no DDL), and re-checks the grain under the row lock before writing —
  on the multi-writer backend a forget landing between resolve and insert no
  longer leaves a vector behind for erased content.
- **Successful tool calls reach the evidence bundle when skill authoring is
  on** — after the failures, inside the same reserved share, so a busy desk's
  successes cannot bury the failure signal. With `skills.enabled: false` the
  bundle is exactly what it was. The Tool brief now carries the call's
  `input`: a procedure is not reconstructible from tool names and outputs.
- `areev loop policy` prints the two new defaults (`skills`, `min_evidence`);
  the goldens are re-blessed. `areev loop outcomes` labels a run- or
  grain-counted checkpoint as `@1 run` / `@50 grains`; time checkpoints
  render as before.

- **A grain write refuses an unspellable namespace** (`VAL-E001`): one
  carrying whitespace, a control character, or an invisible formatting
  character (zero-width space, BOM, soft hyphen). Namespaces stay opaque
  strings — `org.sales.emea`, `agent:authz` and `部門:営業` are all equally
  fine — but a write is the operation that *mints* a namespace, and nothing
  downstream can tell a new name from a mistyped one, so a typo there is
  accepted by every surface and found by none. That is not hypothetical: a
  bad substitution turned a harness's `"agent:harness"` into
  `"age, build_messagesnt:harness"`, twelve hours of evaluations journaled
  into it successfully, and the loop that reads that namespace recorded no
  verdict and proposed no revert for a lesson that had cost the agent every
  exact match it had. Read surfaces are deliberately unchanged and
  replication replay is exempt, so a file written before this rule stays
  readable, erasable and disclosable under the name it used.
- **A lesson's outcome verdict compares against the newest evalset run
  before its apply**, not the run the proposal froze, when one exists. A
  deployment that journals its evalset on day one and then approves rule
  after rule was measuring its twentieth rule against day one: on a real
  corpus (`crates/areev-bench/CURVE.md`) a rule that contradicted an earlier
  one took the agent from 86% to 66% on held-out documents and read as
  `held` against day one's 26%. With nothing journaled between proposal and
  apply the two runs are the same, so no verdict recorded before this
  changes. The proposal-time snapshot, every later measurement and the
  apply gate now read a run's fields through one function
  (`areev_loop::eval::run_value`).
- **A revert is keyed by what it reverts.** Two lessons on one entity that
  both regressed produced one revert: the deterministic dedup key is
  analyzer + target + action, so the second draft was dropped as a
  duplicate. `revert_dedup_key` adds the reverted recommendation's hash.


- **A revert the Verify gate caused puts the finding on cooldown.** A
  rollback normally lets a finding re-propose ("the situation returned"),
  which is right for an operator's own rollback and wrong after a measured
  regression — there the situation never left, so the next pass re-proposed
  the lesson the reviewer had just been asked to revert. Applying an
  `outcome_review` revert now strikes the same doubling cooldown a rejection
  earns; a manual `areev loop rollback` still earns none.
- **A draft may cite evidence by bundle id.** Evidence items carry a short
  `id` (`e1`, `e2`, …) beside the hash, and a DISCOVER draft may cite that,
  the full hash, or an unambiguous ≥12-hex prefix. Measured, the 64-hex
  transcription check was where most of a small model's drafts died — not
  fabrication, just copying. An uncited draft is still dropped.
- **An Observation reaches the model with its observer named.** The grain
  records `observer_id`/`observer_type`; the evidence projection dropped
  both, so a person's correction arrived as an anonymous sentence and read
  identically to the agent having asked for something. Measured, a model
  given a run of unattributed corrections proposed rules to stop the agent
  asking. `Policy.evidence_attribution` selects `named` (new default) or
  `anonymous` (the previous rendering), because an observer id can be a
  person's name and whether it belongs in a prompt is the host's call.
- **An authored proposal dedups on its content.** The dedup key excludes
  proposal content, which is right for an analyzer finding and wrong for an
  authored lesson, where the content *is* the finding: a second lesson on one
  entity was silently dropped as a duplicate of the first. LLM-authored
  executable proposals now key on a fingerprint of their normalized content
  as well; advisory flags keep one open flag per target.

### Fixed

- **An empty `--tool-env` / `AREEV_RUN_TOOL_ENV` silently selected the *weaker*
  environment posture** (#188). Presence of the flag is the setting, but it was
  read through the helper that treats an empty value as "not configured" — right
  for every other `$AREEV_RUN_*` knob (`AREEV_RUN_SANDBOX_CMD=""` means "no
  sandbox"), and exactly backwards here, where empty means *clear to the minimal
  set*. A systemd unit or wrapper script setting `AREEV_RUN_TOOL_ENV=""` asked
  for the strictest environment and was handed the loosest, on the unattended
  path the flag exists for, with nothing to notice. Only an absent flag and
  variable now keep the inherit default.
- **Postgres: `step_actions` without a node id missed predicates** interned by
  another writer, and **`areev reindex` reported 0 indexed documents** — both
  surfaced by the open path no longer seeding in-process state the backend is
  authoritative for.
- **Postgres bootstrap is atomic.** It runs in one transaction holding
  `pg_advisory_xact_lock` (#181, first increment), so a half-applied schema is no
  longer reachable and the lock cannot be leaked on a failure path.
- The **sandbox seam's cleared environment is pinned by test** (#188): an
  operator allow list that names a variable never reaches a `wasm32-areev`
  module, even though it does reach a pinned native blob. The regression this
  guards against is a one-line refactor — hoisting the policy above the
  `if sandboxed` — so the test asserts it with a list the operator *did* name,
  rather than only proving the default is safe.
- **The deployment profile no longer recommends a pooler mode the store cannot
  survive.** `docs/deployment-profile.md` suggested PgBouncer in transaction
  mode for multi-tenant hosts. The store keeps three things on the session —
  `search_path` pinned at open, the bootstrap advisory lock (`pg_advisory_lock`,
  not the `_xact_` form), and the hot-path queries as server-side named prepared
  statements cached per connection — and transaction pooling hands each
  transaction to whichever backend is free, so none of them survives. The
  prepared-statement failure is the loud one (`prepared statement "s0" does not
  exist`); the `search_path` failure is the dangerous one, because one schema is
  one memory, so a statement landing on a connection scoped to a different
  schema is a query answered from **another tenant** rather than a query that
  fails. Session mode is now stated as the requirement, with the invariant a
  proxy must satisfy (one client connection, one server session, for its whole
  life) and a table of what is and is not safe — including Neon's `-pooler`
  endpoint, which is transaction-mode PgBouncer and is the hostname its
  quickstarts hand out
  ([#189](https://github.com/AreevAI/areev/issues/189)).

- **The Postgres dictionary no longer bounds what a grain may say.** `terms`
  was `text UNIQUE`, and a Postgres btree entry caps at ~2704 bytes *after*
  compression — so any subject, relation or object that did not compress
  under that was refused with a raw `54000` from four layers down. It
  depended on entropy, not length: a 2.7 KB base64 or JSON value failed while
  8 KB of `x` passed, `areev import` failed on the same values, and `areev
  loop run` generated the failure itself, because its persisted ledger is a
  JSON object written as a Fact and crossed the line at roughly eight
  recommendations. The dictionary is now unique on `term_hash` (SHA-256 of
  the term), computed in Rust on insert; the first read-write open of an
  existing schema adds and backfills the column under the bootstrap lock,
  creates the unique index, and drops the text constraint. A `--read-only`
  open of a schema that predates the migration refuses with `STO-E005`
  naming the missing column. The embedded backend is unchanged (it has no
  such limit) and no content address moves. **Operators: this migration is
  not rolling-deploy safe** — an older binary still writing to a migrated
  schema fails its next dictionary insert, so drain writers on the old build
  before the first new-build open (`docs/deployment-profile.md`). The cap is
  matched by definition, not by the default constraint name, so a restored
  or hand-repaired schema loses it too. Pinned by the conformance case
  `incompressible_values_of_any_size_are_stored` on both backends and by a
  Postgres-only migration test that stages the old shape
  ([#160](https://github.com/AreevAI/areev/issues/160)).
- **`GRANT` can now spell every verb it documents.** `supersede`, `loop.run`
  and `loop.apply` end in statement keywords (`SUPERSEDE`, `RUN`, `APPLY`),
  and the verb parser only accepted identifiers there, so those three were
  refused with `CAL-E002` — which meant no bound principal could ever be
  granted `loop.apply`, and the documented "review and apply are different
  people" separation could not be expressed in a file. The keyword tokens
  are accepted in the verb position, where nothing else can appear; a test
  now walks `Verb::ALL` through `GRANT` and `REVOKE` so the next verb is
  covered too ([#161](https://github.com/AreevAI/areev/issues/161)).
- **The run index is paged and scoped on the server.** `GET /api/run/list`
  returned a fixed newest-50 page with no total, and the console's Runs tab
  filtered *that* page by namespace client-side — so on a memory with two
  tenants, the quieter one read as having no runs while it had open
  approvals. The route now takes `ns` (exact or `org.*`), `limit` (clamped
  to 500) and `offset`, and answers with `total` and `truncated`; the console
  asks the server for the picked namespace and says "showing N of M". The
  `run:<id> mg:harness` link Fact carries the run's session namespace (`run_ns`) from now on.
  A run from before that stamp existed is not known to be in any namespace,
  so a **scoped** listing excludes it and reports the count in
  `unattributed` rather than guessing from the run's plan (a plan's namespace
  is not the session's) or admitting it to every scope (which would make
  scoping meaningless right after an upgrade). The **unscoped** listing — the
  default — still shows every such run, labelled "namespace not recorded".
  `areev run list` documents `--last`, gains `--offset`, notes truncation on
  stderr, and — like the route — resolves outcomes through the run index
  instead of rescanning the outcome census once per row.
  `RunManifest::persist(m)` keeps its signature (deprecated) and stamps no
  namespace; the runtime uses `persist_in_namespace(m, ns)`
  ([#165](https://github.com/AreevAI/areev/issues/165)).
- **`release-cli`'s `sbom` job checks out the tag it is backfilling.** It
  had no `ref`, so a `workflow_dispatch` backfill regenerated the SBOM from
  `main` and uploaded it under the older tag's name — the fix #159 applied
  to `build` now covers `sbom` too
  ([#162](https://github.com/AreevAI/areev/issues/162)).
- **`areev hold` and `retention floor|floor-clear|floors` reach `--help`.**
  Both verbs shipped working and dispatched, but the usage text listed
  `retention <set|list|clear|sweep>` and no `hold` line at all — so the two
  controls a records-retention deployment most needs were discoverable only
  by reading the source.
- **`areev hold release` records why.** `set` has always demanded a
  `--because` "because a hold with no recorded rationale is not auditable",
  but `release` — the act an auditor actually asks about — accepted the flag
  and ignored it, and the hold row that carries the placement reason is
  *deleted* on release. Both transitions now demand a reason and write a
  Tier-2 audit record, so `areev audit export` shows `hold.set` and
  `hold.release` with who and why. **Breaking for scripts**: a bare
  `areev hold release --ns NS` is now refused.
- **`areev anonymize scan|test` no longer name a memory they never open.**
  Both are pure text processing, but they resolved the default memory first —
  printing `using default memory ~/.areev/default.db` and creating the
  directory. They now dispatch before `resolve_db`, like `auth`.

## [1.7.2] — 2026-09-02

### Fixed

- **Imported grains are free-text searchable without a reopen.** The bundle
  import path wrote the grain row but not its BM25 postings, so a memory that
  had just imported answered `search_text` with nothing. A reopen hid it,
  because the open-time self-heal rebuilds an empty text index — which is also
  why no test caught it. A long-running follower (`areev stream` / `follow`)
  that imports and serves from one handle answered every free-text query empty
  for the life of the process. Pinned on both backends by
  `imported_grains_are_text_searchable_without_reopen`.

### Changed

- **A retracted grain is withheld from assembled context, not merely demoted.**
  `verification_status = "retracted"` applied a `-0.3` priority penalty and
  still reached the model. A retraction states that a memory may not be acted
  on, so `areev-context` now withholds it — from the main body and from the
  Knowledge Update section, which is built earlier and would otherwise leak the
  withdrawn value as "what changed". `FormatPolicy::include_retracted(true)`
  admits them for audit and forensic reads. `contested` keeps its `-0.15`
  demotion: that one is a degree, not a withdrawal. The store is unchanged and
  still returns retracted grains, because `subject_report` (DSAR) shares one
  selector with erasure and must disclose exactly what an erasure removes.
- `verification_status` is classified in one place
  (`areev_core::verification::Trust`). `areev-context` and the corpus exporter
  each parsed the field independently and disagreed — the non-OMS value
  `"rejected"` was excluded by one and ignored by the other. It now maps to
  `retracted` consistently.


## [1.7.1] — 2026-09-01

Three defects in the path between evidence and action in Areev Loop. Each
produced a plausible **null result rather than an error** — the loop ran, the
run completed, and it read as a model with little to say — which is why they
survived a synthetic benchmark and surfaced the first time the loop was
pointed at a real workflow.

### Fixed

- **A human's note reached the model as an empty string.** `grain_brief`
  renders each grain for the LLM evidence bundle; its fact-triple branch needs
  subject+relation+object, and an Observation has no relation, so it fell
  through to a fallback that checked `content`/`body`/`text`/`summary` and
  never `object` — where the store actually puts an Observation's text. Every
  human note in every memory was handed to the model as `""`: the highest-value
  evidence a memory holds was the one shape that rendered to nothing, so an
  explicit instruction from a person could never become a lesson. Callers had
  begun duplicating the text into `body` to work around it; that workaround is
  removed with the cause. `testkit`'s `add_observation` had encoded the same
  mistake, which is why no test caught it — adds `add_human_note`, shaped as
  the store writes one, and a regression test.
- **A lesson could only prevent, never start doing something.** The lesson
  contract asked for a rule "preventing a recurring mistake, phrased to apply
  BEFORE it happens". When an agent simply never produces a field, the rule it
  needs is additive, and the vocabulary had nowhere to put one — so the model
  reached for the nearest allowed shape ("validate all required fields before
  submission"), which passed DISCOVER, GROUND, VERIFY, human review, apply and
  render, and changed nothing. Measured: 0/30 field coverage under that rule
  against 6/6 under "capture X, Y, Z" — same fields, same prompt position,
  different verb. A lesson can clear every gate the engine has and still be a
  no-op, because no gate asked whether the wording named an action the agent
  could take. The contract now admits an additive rule alongside an ordering
  one and says a lesson must name the action, not a check on it.
- **A failing grader was indistinguishable from a model with nothing to say.**
  `grounded: 0` conflated the gate correctly refusing every draft with the
  backend never answering; `LlmFunnel` now reports `ground_verdicts` and
  `ground_call_failed` separately, and the bench adapter retries more before
  giving up.
- **The bench loop adapter could not talk to OpenAI-served models.** Its pin
  probe sent `response_format=json_object` with the message body `"{}"`, which
  OpenAI rejects ("'messages' must contain the word 'json' in some form"), so
  a working provider pin was reported as a bad tag.

### Added

- `crates/areev-bench/EXPENSE.md` — governed self-improvement measured on a
  real expense workflow (an operator's own invoices; the corpus stays private,
  only counts travel). Paired per invoice and scored by McNemar, because the
  unpaired version was worthless: between-run noise exceeded the effect. Three
  seeds give 21–0, 32–0 and 30–0 wins for the learned rules (p<0.0001) against
  a noise floor of 0–1 discordant pairs in 210. The document also records what
  the result does not show. `scripts/expense_curve_chart.py` renders its chart.

## [1.7.0] — 2026-08-31

### Security

- **A proxy-asserted SSO identity may no longer answer a HITL approval by
  default.** `run.respond` already refused shared-token and anonymous callers
  because the approver's identity *is* the audit record — but the
  trusted-header path set that identity from a header trusted via
  `x-areev-proxy-secret`, a static fleet-wide value the code itself documents
  as impersonation-grade. Whoever held it could approve as anyone, including
  the officer named in the resulting audit grain, and nothing downstream could
  tell: it is a well-formed approval by a granted principal. The strongest
  governance control in the product rested on the weakest identity primitive.
  SSO identities keep every read and review and lose only this verb;
  `areev ui --sso-approvals allow` accepts the trade-off explicitly.
  **Behavior change**: a deployment relying on proxy-asserted approvals gets a
  403 after upgrade, with the flag named in the message. Failing closed on an
  approval path is the correct direction to break.
- **Repeated failed authentications from one source are now refused.** After
  10 consecutive failures, further credential-bearing requests from that IP
  get `429` until the streak goes idle. It rejects rather than delays — a
  sleep on a serial accept loop is a denial-of-service lever — and rejects
  before routing, so no store access or constant-time scan happens.
  Credential-less requests are neither counted nor blocked, so browsers can
  still get their 401 challenge.
- **Proxy-asserted identities are validated, not merely trusted.** An identity
  header carrying control characters (CR/LF injection into audit grains),
  internal whitespace, over 128 bytes, or a reserved principal name
  (`anonymous`, `user:console`) is now ignored — treated as absent, so a
  misconfigured proxy degrades to anonymous rather than taking the console
  down.

### Added

- **An LLM finding can now propose five kinds of change, not one.** A DISCOVER
  draft carries a closed `proposal` vocabulary — `lesson`, `fact`,
  `query_revision`, `plan_revision`, `code_revision` — each resolving to a
  statement an apply already knows how to run and roll back. Scope is never
  model-named: the subject of a fact, the name of a query, the plan hash, the
  tool, and (for code) the evalset that grades it all come from the target or
  the substrate. Resolution happens *before* GROUND and VERIFY, so the gates
  judge the exact change an apply would make. Auto-apply is unchanged twice
  over: `origin = llm` is categorically ineligible, and only the `memory`
  target class is auto-appliable at all.
- **`plan_revision` carries field-level edits with a `from` staleness check**,
  over an allowlist of `edges.<i>.cond`, `edges.<i>.max_cycles` and
  `retries.<node>`. Node topology is not expressible by any proposal a model
  can write, so a plan revision stays reviewable as a short list of scalar
  deltas.
- **Substrate capabilities `plans` and `code`** (`validate_plan`,
  `tool_evalset`). The Areev adapter hands a candidate plan to the runtime's
  own `PlanGraph::build`, so the loop carries no second opinion about what a
  runnable plan is; a substrate declaring neither degrades those proposal
  kinds to advisory rather than pretending to have checked them.
- **`RunResult.llm_funnel`** — the DISCOVER pipeline's attrition, stage by
  stage: evidence → proposed → cited (split into `dropped_uncited` and
  `dropped_target`) → grounded → kept → stored. "The model contributed
  nothing" has five causes that need opposite fixes and all previously
  rendered as an empty queue. Surfaced on `loop_run()` in both bindings.

- **`areev auth mint|list|revoke`** — the credential-map lifecycle. `mint`
  emits a 256-bit CSPRNG token prefixed `areev_pat_` (recognizable to secret
  scanners), prints it once, and stores only its SHA-256. Credentials gain an
  optional `id` (with a stable non-positional fallback, so maps written before
  this still load), a `label`, and an optional `expires_at` — past which the
  credential is refused *indistinguishably* from an unknown token. Revoking
  one id leaves the principal's other credentials working. `areev ui` warns at
  startup about credentials expiring within 14 days, and about a
  `--token-env` value Areev did not mint (unknown entropy).
- **IdP groups → principals** (`areev ui --sso-groups-header NAME`, with a
  `groups` table in the credential map), so SSO no longer needs a grant grain
  per person. An identity with its own grants outranks its group, and a
  group-derived principal may never approve — under any setting, since a role
  identifies nobody who can be asked why. `--sso-principal-prefix` keeps
  IdP-sourced principals visibly distinct from local ones.
- **Native OIDC for the console** (non-default `oidc` build feature):
  authorization-code + PKCE (S256), RFC 8414 discovery — which makes Google
  and Entra ID config rather than code — `id_token` validation against the
  issuer's published JWKS, and an `HttpOnly` `SameSite=Strict` session cookie.
  No token ever reaches the browser; sessions are stored under their digest
  and expire on both an idle and an absolute clock; logout invalidates
  server-side. **An OIDC principal may approve by default** — a verified
  signature is a stronger claim than a shared proxy secret, which is the
  entire reason the feature exists. Symmetric algorithms are refused outright
  (algorithm confusion), and the allowlist fails closed on unknown ones.
  Recorded as the second dependency-policy exception (`jsonwebtoken`) in
  ARCHITECTURE.md; setup in `docs/runbooks/oidc-setup.md`; the design and
  what was deliberately rejected (per-vendor SSO, OAuth client-credentials,
  Areev as an authorization server) in `docs/auth-proposal.md`.
- `GET /api/whoami` now reports `identity_source`
  (`oidc`/`sso`/`sso-group`/`credential`/`none`) and `may_approve`, so the
  console can tell an approver where they stand before they try rather than at
  the 403.

### Changed

- **The DISCOVER evidence bundle reserves a share per source** rather than
  filling first-come: cited findings ≤24, recent tool errors ≤16,
  human-authored Observations ≤8, recent facts taking the remainder of 64. A
  single `tool_failure` finding may cite 64 grains on its own, which let
  determinism fill the whole bundle so the model never saw a grain no analyzer
  had already flagged. The Observation reserve encodes a second principle:
  volume is the wrong sort order for evidence, because a person's instruction
  is a complete rule stated *once* and is outnumbered from the moment it is
  written.
- **DISCOVER is told that outcome records are actionable evidence** — that
  restating a deterministic finding earns nothing, and that comparing rejected
  against accepted outcomes is how a problem with no error attached gets
  found, with a two-observation floor. Without it a model handed both kinds of
  evidence reliably writes about the error text and ignores the rest.

- Console auth guidance now points at `--auth` (per-principal, attributable)
  before `--token-env` (one shared secret, unattributable, cannot approve) —
  in the startup banner, the read-only write refusal, `USAGE`, and the
  cookbook.
- `areev_core::time` holds the one ISO-8601 parser; `areev_store::migrate`
  re-exports it rather than keeping a second copy.

### Fixed

- **`loop.tool_failure` scores a failure mode against its own opportunities,
  not every call to the tool.** The rate gate divided a cluster by the tool's
  *total* calls, so sibling failure modes masked each other: the more distinct
  ways a tool broke, the smaller each mode's share, and the tools failing in
  the most ways were the hardest to learn from. On a real 150-task agent trace
  (772 calls, 139 failures across 5 modes) every mode landed between 9% and
  30% and the analyzer proposed **nothing at all** — the flagship analyzer was
  effectively blind to a competent agent, whose failures spread across
  signatures instead of concentrating in one. The denominator is now the
  tool's successful calls plus the cluster being scored, since a call that
  failed at an earlier check never reached the failure under test. On that
  same trace the loop now proposes the dominant mode (46 failures, 44% of its
  opportunities) while its smaller siblings correctly stay silent. Recurrence
  metrics carry the matching sample size, and the rendered summary no longer
  says "% of calls" for a rate that is not over calls.

- **A published run may carry an editorial note, and regenerating its manifest
  no longer deletes it.** `verify_run.py` compares `MANIFEST.md` byte-for-byte
  against a regenerated one, so the "measured against the six-rule
  environment — not comparable to a re-run" banner added to each committed run
  failed the build. Regenerating was not the fix: `--write-manifest`, the
  command the error message names, would have silently removed the one
  sentence telling a reader the numbers are not comparable. One blockquote
  under the title now survives regeneration. It is prose, never a number —
  every file in the directory is still checksummed, so a note cannot make a
  changed transcript verify.
- **The bench's shape gate no longer carries a fixed rate margin anywhere.**
  The A0→B gate became floor-relative for a stated reason — a threshold that
  moves whenever the workload changes measures the workload, not the plumbing
  — but the passive-arm check kept a fixed 0.05. The workload's silent and
  instructed rules are ones no context provider fixes either, which took that
  arm's edge to exactly 3 tasks of 60 (which *is* 0.05) and failed the gate on
  its own boundary. Both are now measured in tasks against the run's own
  measured noise floor. Separately, adding the A0R floor state had shifted a
  positional slice, so B2 was being checked and reported as a passive arm;
  states are selected by name.
- **`jsonwebtoken`** — the native-OIDC dependency — **is attributed in
  `THIRD-PARTY-NOTICES.md`**, where it was missing since the feature landed.

## [1.6.5] — 2026-08-26

### Fixed

- **The external-embedding seam works with no embedder installed.**
  `set_grain_embedding` (`addEmbedding` on the bindings) never called
  `ensure_embeddings`, the DDL that creates the PostgreSQL `vector(dim)`
  column — only `set_embedder` did. A host that computes its own vectors,
  which is the seam's whole purpose, therefore hit
  `42703 column "vec" of relation "embeddings" does not exist` on a memory
  that had never installed an embedder, and every later `nearest_vector`
  failed the same way. Worse, provenance was stamped *before* the write:
  the memory ended up declaring an embedding space it had no column to
  hold, weakening the guarantee `ARCHITECTURE.md` §6 rests on. The DDL now
  runs first and provenance is recorded only after the vector is stored —
  a refused write leaves the declaration untouched. A no-op on the embedded
  backend, so both tiers keep identical semantics; pinned by two
  `areev-conformance` cases that run against both.

### Changed

- **`ARCHITECTURE.md` §6 now says what the embedder seam actually offers.**
  It advertised "bring a remote HTTP embedder" without noting that
  `EmbedBackend::embed` is synchronous, so an in-process async embedder is
  not expressible from the bindings — the three routes that do work are now
  named. It also now states that vector recall on the PostgreSQL tier is an
  exact scan with latency linear in corpus size, with measured figures: an
  undocumented ceiling is harder to design around than a documented one.

## [1.6.4] — 2026-08-26

### Security

- **The console no longer discloses its own database password.** `areev ui`
  rendered the `--db` value into the served HTML and returned it from
  `GET /api/stats` and `GET /api/config`. On the Postgres backend that value
  is a DSN carrying an inline password, so a console **read** token — the
  thing you hand a colleague so they can look at one namespace of one
  memory — also handed over authority across every schema on that server,
  and on many managed setups the ability to create roles and databases. All
  three surfaces now show a redacted copy
  (`postgresql://user:***@host:5432/db?sslmode=verify-full`), keeping the
  host, database, schema and options readable for diagnostics. Redaction is
  display-only: a credential map's `memories` entry still matches the **raw**
  `--db` value, so `areev ui --auth FILE` deployments are unaffected
  ([#124](https://github.com/AreevAI/areev/issues/124)).

### Fixed — triggers

- **Superseding a trigger no longer orphans its cursor and its dedup fence.**
  Evaluation state (`trg:` row) and run ids are now keyed on the **root of the
  trigger's supersession chain** rather than the declaration's current hash.
  Re-pointing a trigger at an improved plan is the loop's own Level-1 flow —
  and because triggers deliberately do not follow supersession heads, the only
  way to re-point one is to supersede it, which minted a new hash and
  discarded all of its state. The cursor re-seeded, so a mailbox connector
  seeking "newest" silently skipped every message that arrived since the last
  poll while reporting a healthy tick; and the dedup fence reset, so any
  connector overlap across the re-point re-processed work already done. For a
  trigger that has never been superseded, root == head and nothing changes;
  one superseded *before* this fix adopts its existing state under the root
  key, and run ids minted under the old hash are still recognized as
  already-processed, so the upgrade cannot itself cause the double-processing
  it prevents. `areev trigger show` now prints the cursor (and the state key
  when it differs from the declaration's hash)
  ([#128](https://github.com/AreevAI/areev/issues/128)).
- **A refused run start no longer consumes the item.** A firing where any item
  failed to start was treated as a success: the cursor advanced past the item,
  `consecutive_failures` reset and `last_error` cleared. A host running a
  stale executor pin — the window after a `code_revision` recommendation is
  applied but before the blob is synced — consumed its source one tick at a
  time while reporting healthy; for a mailbox trigger, silently discarded
  mail. Such a firing now holds the cursor, increments `consecutive_failures`,
  records `last_error`, backs off, and does not consume a satisfied
  composite's correlation key — the same posture `TRG-E011` already took for a
  connector contract violation. Retrying is safe because the dedup fence is
  stable (above): items that did start return as duplicates. The report names
  the held cursor as `cursor_held`
  ([#129](https://github.com/AreevAI/areev/issues/129)).
- A latent bug found while fixing the two above: `start_run` wrote the ingest
  Event — which carries the `run_id` the duplicate check looks for — *before*
  calling the starter, so a failed start still left a `run_id`-bearing grain
  behind. Once #129 made retries actually happen, every retry of a failed item
  would have matched that leftover grain and reported `Duplicate`, stranding
  it permanently. The Event is now written only for outcomes the dedup check
  should treat as seen (`Ingested`/`Started`/`Duplicate`), never for a failure.

### Added

- **`--read-only`, on both backends.** Opens a memory that refuses every
  write with `STO-E004` and, on Postgres, issues no DDL at all — no
  `CREATE SCHEMA`, no `CREATE … IF NOT EXISTS` index maintenance, no seed,
  no advisory lock — verifying with SELECT-only probes instead and refusing
  by name (`STO-E005`) when the schema is absent or not bootstrapped. This
  is what makes a least-privilege Postgres role possible: because Postgres
  checks privilege *before* existence, `CREATE SCHEMA IF NOT EXISTS` needed
  `CREATE` on the database and `CREATE UNIQUE INDEX IF NOT EXISTS` needed
  *ownership* of the table even when both already existed — so the narrowest
  role that could open an existing, fully migrated memory was its owner, and
  anything that merely reads (a dashboard, a reporting job, `areev ui`) had
  to be given write authority over it. `docs/deployment-profile.md` carries
  the `GRANT` recipe ([#127](https://github.com/AreevAI/areev/issues/127)).
- **`areev ui --allow-origin ORIGIN[,ORIGIN...]`** — the missing half of
  `--allow-remote`. A remotely served console loaded and read fine but
  rejected every POST, and CAL runs by POST, so the whole query surface was
  dead: `--allow-remote` lifts the Host check, and nothing lifted the Origin
  check. The check itself stays — browsers re-attach cached HTTP Basic
  credentials to cross-site requests, so Origin is what distinguishes the
  console's own page from an attacker's page riding a viewer's login.
  Operators now name their public origin instead of stripping the header at
  the proxy. Exact match on scheme + host[:port]; no wildcards, no subdomain
  matching ([#125](https://github.com/AreevAI/areev/issues/125)).
- **Server-tier Linux release assets.** Every release now also carries
  `areev-<v>-{x86_64,aarch64}-unknown-linux-gnu-postgres.tar.gz`, built with
  `--features postgres-tls`, so the deployment shape the docs recommend no
  longer requires a 15-minute compile. The stock assets keep neither feature,
  which is what keeps the edge binary dependency-light. `install.sh` fetches
  them with `AREEV_FLAVOR=postgres`
  ([#123](https://github.com/AreevAI/areev/issues/123)).
- **Console auth failures are counted and logged** in one stable, greppable
  shape — `areev: console auth FAILED from <ip> (<n> consecutive)`, never the
  presented token — so a `401` is separable from ordinary traffic and an
  operator can drive a fail2ban-style rule off it. Deliberately no in-process
  delay or lockout: the console serves one connection at a time, so a
  per-request sleep would let an unauthenticated caller stall it for
  everyone, and behind the reverse proxy the deployment profile requires,
  every request arrives from the proxy's IP, where an IP lockout would lock
  out every user at once. Rate limiting belongs at that proxy, which is also
  the only place that can see the caller's real address
  ([#126](https://github.com/AreevAI/areev/issues/126)).

### Fixed

- The "this build lacks the postgres backend" error named the `postgres`
  feature. `postgres` alone *refuses* a DSN carrying `sslmode=`
  (`STO-E003`), and Azure Flexible Server, RDS with `rds.force_ssl` and
  Cloud SQL all require TLS — so anyone following the hint exactly hit a
  second wall one step later. It now names `postgres-tls`, and mentions the
  prebuilt asset. Same correction in `docs/quickstart.md` and in the two
  equivalent `areev-py` messages
  ([#123](https://github.com/AreevAI/areev/issues/123)).
- `scripts/install.sh` now runs the binary before reporting success. The
  Linux assets need **GLIBC 2.39** (the release runner image), so on Ubuntu
  22.04 — in support until 2027 — the install "succeeded" and the binary then
  failed with a bare `version 'GLIBC_2.39' not found`. The baseline is now
  stated in `docs/quickstart.md` and on the workflow
  ([#123](https://github.com/AreevAI/areev/issues/123)).

### Documented

- `docs/security-model.md` gains the console's full auth surface: that the
  token's entropy is the only control on that path, that HTTP Basic is
  browser-cached and re-attached cross-site (which is *why* the Origin check
  is load-bearing rather than defence in depth), and that there is
  consequently **no logout** — closing the tab does not clear the credential.
  A `SameSite=Strict; HttpOnly; Secure` session cookie is the intended fix
  for both and is tracked separately; it does not exist yet
  ([#126](https://github.com/AreevAI/areev/issues/126)).

## [1.6.3] — 2026-08-25

### Added

- **Native TLS for the Postgres backend** (`postgres-tls` cargo feature) —
  the DSN's `sslmode` (libpq's full five-rung ladder, including `verify-ca`
  and `verify-full`, which the driver does not understand on its own) and
  `sslrootcert` are honored, so a managed Postgres that requires encryption
  on the wire — Azure Flexible Server's `require_secure_transport`, RDS's
  `rds.force_ssl`, Cloud SQL — connects without inserting a TLS-wrapping
  proxy inside the trust boundary. rustls with compiled-in webpki roots, no
  OpenSSL. On in the container image and in both bindings; off in the stock
  `areev` binary, where an encrypting DSN is now **refused by name
  (`STO-E003`) rather than downgraded to plaintext**. `sslmode=disable` and
  the `prefer` default are unchanged, so no existing deployment moves.
  Note that `require` follows libpq and encrypts *without* validating the
  certificate — use `verify-full`, with `sslrootcert` where the provider
  signs with a private root ([#117](https://github.com/AreevAI/areev/issues/117)).

### Fixed

- Postgres connect-time failures now carry their cause. These never reach a
  server, so they have no SQLSTATE, and `pg_err` reported only the driver's
  `Display` — "error connecting to server". A TLS rejection lands in exactly
  that class, where "invalid peer certificate: UnknownIssuer" is the whole
  diagnosis.
- **`outlook_graph.py` (invoice-to-accounting example): two live-only bugs.**
  The attachment listing's `$select` named `contentBytes`, which is declared
  on `microsoft.graph.fileAttachment` and not on the base `attachment` type
  the collection is typed as — Graph answered `400 BadRequest` and every
  message with an attachment failed the poll. And the poll read `/messages`,
  which spans **all** folders, so the desk's own approval mail came back out
  of Sent Items as a fresh candidate invoice on the next tick (with a new
  message id, so `/message_id` dedup could not catch it); it now reads
  `…/mailFolders/inbox/messages`. Neither reproduces under the keyless CI
  floor, which uses the fixture connector by design
  ([#118](https://github.com/AreevAI/areev/issues/118)).

## [1.6.2] — 2026-08-24

### Removed

- **`areev hub` (the "areevd" sync daemon) and the `/api/segment*` endpoints.**
  Areev no longer runs a networked sync service. Replication is what it already
  was underneath — `areev stream` writes generations of `.mgb` segments into a
  directory, `areev follow` applies them — and moving that directory is now
  always the deployment's job (rsync, object storage, a shared volume). Gone with it: `UiServer::into_hub`, `POST /api/segment`,
  `GET /api/segment`, `GET /api/segments`, the hub `--retain` archive sweeper,
  and the console's "Sync across apps" settings tab (`#settings/sync` now lands
  on the Agent tab).

  The forcing argument is that a hub token is one shared secret over an entire
  memory — anyone holding it could pull every segment, which is the whole file
  — and a bundle push is an op-log replay that never crosses the facade's verb
  checks, so the per-principal credential map governing every other write path
  had no purchase on it. A surface that can only be all-or-nothing cannot
  participate in the authorization model the rest of the system is built on.
  Rationale in full: ARCHITECTURE.md §10, "Sync is file-to-file; Areev runs no
  networked sync service".

  **Migrating:** a fleet that pushed segments to a hub replaces the HTTP hop
  with a directory the peers share (`areev stream --to DIR --retain 30d` on the
  writer, `areev follow --from DIR` on each reader). A deployment that needed
  concurrent writers against one memory belongs on the **Postgres backend**
  (`feature = "postgres"`, one memory = one schema), which is the supported
  answer and always was.

### Added

- **Brokered credentials can be minted per call instead of read once** (#113).
  `--credential` accepted only an environment variable, which made every
  brokered secret static for the life of the process — the wrong shape for the
  credentials capability tools actually use. A Google access token expires
  roughly hourly, so an unattended heartbeat needed a refresh step outside
  Areev that it could silently get wrong, and a run parked on a human gate for
  a day resumed with yesterday's token. Vault and secret-manager users had it
  sharper: their whole model is short TTLs and central revocation, and an
  environment variable defeats both.

  Two more sources resolve **inside the broker, at call time**:

  ```bash
  --credential 'sheets=cmd:gcloud auth print-access-token'
  --credential 'sheets=vault:secret/data/google#access_token' --resolver-env VAULT_ADDR,VAULT_TOKEN
  ```

  `cmd:` takes the command's trimmed stdout through the same subprocess seam
  `--tool-cmd` and `--embed-cmd` use, so it covers `vault`, `gcloud`, `aws` and
  `az` with no vendor client in the dependency graph; `vault:` reads a
  Vault/OpenBao KV secret natively (v1 and v2) so a container needs no `vault`
  binary. What the guest sees is unchanged — it names a label and holds
  nothing.

  Values are cached for `--credential-ttl` (default 300s) and minted again
  after, so a revocation upstream takes effect without a restart. A resolver
  that errors, times out, or returns nothing **refuses the call** rather than
  sending it unauthenticated, and the error names which credential failed
  without ever repeating what the resolver printed. A minted value is
  validated as an HTTP header value, because one containing CR/LF would forge
  a second header on every request it rides. A 401 on a minted credential
  always invalidates the cached value and re-issues the request exactly once —
  but only for `GET`/`HEAD`: a write that 401'd may already have been applied
  upstream, and the broker does not get to guess.

  `--resolver-env VAR,…` names the variables a resolver needs for its **own**
  authentication. They are registered as secrets (withheld from every
  subprocess seam) and re-admitted only for resolver spawns, which run under
  `EnvPolicy::ClearExcept`. This is load-bearing rather than tidy: a
  `VAULT_TOKEN` left ambient is readable by every `--tool-cmd` child and can
  fetch *every* secret, not just the one it was for — #100's leak, one level
  up. Bind a principal on the name side for these sources (`--credential
  'sheets@user:alice=cmd:…'`), because a command may itself contain `@`.
  `Credential`'s `Debug` is now redacted. Available on `areev run`, `areev
  trigger run`, and both bindings' `credentials_json`.
  Setup per platform: `docs/cookbook.md` §19. Rationale: ARCHITECTURE.md §10,
  "A brokered credential's source is a seam, resolved in the broker".

### Security

- **A brokered credential is now bound to a host, not only to a caller**
  (#112). `capabilities.http` carried `hosts` and `credentials` as independent
  lists and the permit check tested them independently, so **any declared
  credential could be attached to any declared host** — and a second `http`
  entry was refused outright, so a tool talking to two services had no way to
  say which secret belonged to which. A tool that reads a mailbox and writes a
  sheet could therefore send the mailbox token to the sheets API: the
  confused-deputy case the broker exists to prevent, reachable by an ordinary
  bug (one wrong label in the guest) as easily as by malice.

  Both halves of `declared ∩ host-granted` can now express the pairing, and
  both are checked. `capabilities` accepts **repeated `http` blocks**, and a
  call must be admitted by ONE block as a whole tuple `(host, path, method,
  credential, headers)`. The host-side grant gained the same:
  `--tool-egress 'sync:gmail@gmail.googleapis.com:POST'` pairs a credential
  with the bare hostname it may reach (`*.example.com` works; scheme and port
  stay with `--allow-host`, because the spec is colon-delimited and a URL
  would tear apart in it). A refusal that names both halves reads apart from
  an undeclared credential — different bugs, different fixes.

  **Compatible in both directions.** A single-block declaration behaves exactly
  as before, an unpaired grant still means any host the rest of the chain
  permits, and N blocks admit the union of N tuples rather than the
  cross-product their merger produced — so the change can only narrow.
  `CapabilityDenied` gains a `CredentialHost` variant and `CallerGrant`'s
  `credentials` field is now private behind `credential()` /
  `credential_for()`; `Broker::start` takes `CredentialSource` values
  (`Credential` converts with `.into()`).
  Rationale: ARCHITECTURE.md §10, "A brokered credential is bound to a host,
  not only to a caller".

  **Scope:** this covers the `areev run` path — brokered tools and capability
  tools. A *trigger connector* still holds every credential the trigger
  configured for any host in its `allowed_outbound_hosts`: one connector runs
  per evaluation pass, so its grant is derived from the credential list rather
  than written, and it carries no declaration. Unchanged from previous
  releases, now noted in `docs/triggers.md`; give a trigger only the
  credentials its connector needs.

## [1.6.1] — 2026-08-23

### Added

- **Capability tools can set non-credential request headers** (#105). A
  brokered `areev::fetch` request takes an optional `headers` map, and a Tool
  grain declares which names it may use as `capabilities.http.headers`. This
  was the last thing between capability tools and the APIs they are pitched at:
  every Google API called with user credentials requires `X-Goog-User-Project`
  or answers `403 … requires a quota project`, and that header is not a
  credential, so neither the broker nor the guest could set it. The same gap
  blocked `anthropic-version`, `x-ms-version`, and every tenant header.

  The credential channel stays closed. `Authorization`, `Proxy-Authorization`,
  `Cookie`, `Host` — and any header a configured `Credential::Header` rides in,
  which is known only after resolution — are refused at any casing: declaring
  one is refused at **write** time, sending one at call time. That refusal is
  deliberately free rather than costing a call from the budget: the
  spend-before-checking rule exists so a module cannot probe *per-caller*
  policy for nothing, and "may I write the Authorization header?" has one
  answer for everyone. Malformed names and values carrying CR/LF are a `400`,
  because header injection is malformed rather than merely denied, and it must
  die at the parse instead of at the socket where it would split one request
  into two.

  Declared headers are deny-by-default like credentials, matched
  case-insensitively, checked on every redirect hop, and travel exactly as far
  as the credential does — a cross-origin hop drops both, since a quota project
  or tenant id was meant for the host the caller named and not for one an
  intermediary chose. They are journaled on the `egress_call` Observation
  **with their values**, the deliberate asymmetry against the credential's
  name-only record: the caller supplied them, so recording them discloses
  nothing it did not already hold, and turns "it was allowed to reach Google"
  into "it billed this quota project on these four requests".

  The sandbox needed no change — it forwards the guest's JSON verbatim, so the
  guest ABI is the broker ABI.

- **Capability tools can read CAS blobs** (#106). A Tool grain declares
  `{"blob": {"read": true}}` and its module gains `areev::blob_get`, reading
  one stored blob by content address. This closes the gap that left a whole
  class of tool stuck outside Tier C: a trigger's connector already files email
  attachments as CAS blobs, and the tool that parses them is the one that most
  wants sandboxing — its input is untrusted by construction — yet it was the
  one tool that structurally could not be, because `wasm32-areev-io` could
  reach the network but not the bytes the memory already held.

  Read-only, and by address only: no enumeration, no write, no namespace
  access, so a module fetches bytes it was handed a `cas://` reference to and
  cannot browse the memory. The two capabilities are independent — a module
  that parses attachments and calls nothing declares only `blob` — and gated
  asymmetrically on purpose: `--allow-fetch` derives from the pinned runtime,
  because a host-side grant narrows it afterwards, while `--allow-blob` derives
  from the pinned *declaration*, because nothing narrows a blob read after the
  fact.

  **The read goes through the broker, not the sandbox**, and that is the
  design's substance rather than a detail. Reading the `.blobs` sidecar
  directly from the subprocess looks free — the read is lock-free, which is
  what already lets `areev blob get` work mid-run — but the subprocess is
  handed no memory path, cannot take `areev-store` without giving up the
  five-dependency standalone posture that makes it a credible boundary, reads
  nothing on the Postgres backend, and has **no channel back to the driver**:
  stdout is the guest's result and the stderr fuel line is prose. A read
  performed there could not be journaled, putting the hole in the evidence
  exactly where the untrusted bytes are. Through the broker, every read lands
  as a `blob_read` Observation naming the address and byte count, drained on
  the same superstep boundary as `egress_call` — and the guarantee becomes one
  sentence: **the guest gets neither a socket nor a file descriptor.**

  Success answers raw bytes rather than JSON, since a blob is binary and
  base64 would tax every guest with a decoder to read its own attachment;
  errors stay JSON and are told apart by status, never by sniffing a payload
  that may legitimately begin with `{`. Ceiling is `runtime_limits.max_blob_bytes`
  (default 8 MiB, the payload cap). Embedded backend only — on PostgreSQL a
  blob lives in-schema, so the call returns a `501` naming the limitation
  rather than reporting the attachment as missing.

### Changed

- **The framework adapters moved out of this repo.** `areev-langgraph` and
  `areev-crewai` now live in `AreevAI/areev-adapters` and are developed
  against the published PyPI `areev`, so they version against their upstream
  frameworks instead of against the core (`ARCHITECTURE.md` §10, "Framework
  adapters live outside the repo"). Nothing changes for anyone installing
  them: both stay on PyPI at 1.0.0 and work against current Areev. They are
  **parked** — no new releases planned until someone asks — so this repo's
  CI no longer runs their suites, and until the adapters repo is un-parked
  an areev release is not gated on them.
- The Hermes provider smoke, which rode the removed `adapters` CI job for
  its maturin-built venv, now runs as the last step of the `python` job (and
  so on macOS as well as Linux).
- **The console's memory graph draws entities, not values.** Every fact's
  right-hand side used to become a node, so `amount: 4400.00`,
  `currency: USD` and `payment_terms: net_45` were drawn as peers of the
  people and vendors they describe — the demo memory rendered as 91 nodes and
  126 edges for the ~35 entities it actually holds, which is a hairball
  rather than a graph. An object is now kept only when the memory also knows
  something *about* it (it is a subject somewhere too) or it arrived through a
  relation that points at an entity (`vendor`, `owner`, `reports_to`,
  `headquartered_in`, …), with a literal-shaped veto so an unfamiliar schema
  cannot smuggle a scalar back in. The same file now draws as 35 nodes and 36
  edges with no orphans. A memory whose relations are not recognised could be
  filtered down to nothing, so a graph left with fewer than three linked
  entities falls back to unfiltered rather than showing an empty canvas.
- **The graph is legible and reproducible.** Labels are placed by priority —
  the focused node, then the relations coming off it, then names, near before
  far — each trying several positions before it is dropped, with the node
  circles treated as obstacles, so names no longer stack on each other or
  print across a node. Start positions are seeded from the node name instead
  of `Math.random()`, so a file lays out the same way on every reload and a
  re-shot screenshot is byte-identical. A rebuild that places nothing new
  keeps the positions it has, which stops the rewind scrubber re-converging
  the whole layout under the cursor on every drag.
- The graph legend said "Things they like", which read as a personal-assistant
  memory and misnamed every invoice and process in a business one; it is now
  "Everything else", alongside "Projects & processes".
- The console rail carries the full v2 lockup (the A, `reev`, and the
  improvement loop) rather than the A beside a text "Areev" — two bitmaps,
  because `BRAND.md` wants the white `reev` as its own artwork rather than a
  CSS recolor. All ten README screenshots were re-shot against the real
  console, as a console change requires.

## [1.6.0] — 2026-08-23

### Security

- **The outbound allowlist now governs every redirect hop, not just the first**
  (#99). The broker's HTTP agent followed up to ten redirects on its own, while
  `policy.permits` was checked exactly once — on the caller-supplied URL,
  before dispatch. So an allowed host answering `302 Location:
  http://169.254.169.254/latest/meta-data/` had its follow-up performed and the
  cloud metadata service's body handed back to the tool: host allowlisting is
  this subsystem's core control, and a redirect walked straight through it. It
  affected every brokered tool and connector, not a hypothetical. Auto-follow
  is now off (`max_redirects(0)`) and the broker follows by hand, re-checking
  the allowlist on every hop and re-checking the grant whenever a `303`
  changes the method. The invariant is now enforced rather than intended: **no
  byte is sent to, and no body is returned from, a host the allowlist does not
  permit.** A blocked redirect journals a refusal (`RUN-E022` / `TRG-E009`)
  worded apart from an aimed-at one — "it tried to reach there" and "it was
  redirected there" are different stories for whoever reads the record — and
  chains are bounded at ten hops with the bound itself auditable.

  The mirror image is fixed in the same change. ureq's `redirect_auth_headers`
  defaults to `Never`, so the brokered `Authorization` was dropped on *every*
  redirect, including the same-origin ones Google and Microsoft APIs use
  routinely; the follow-up arrived unauthenticated, 401'd, and nothing in the
  journal said why. The credential now re-attaches exactly when scheme, host
  and port are unchanged, and is dropped otherwise. A `Location` that is
  relative resolves against its base; one that is not a resolvable `http(s)`
  URL, or that carries a control character, is refused rather than guessed at.

- **`--credential NAME=ENV_VAR` no longer leaks the raw secret into every child
  process** (#100). The withhold list was three flags long
  (`--passphrase-env`, `--token-env`, `--anon-key-env`) and `--credential` was
  not on it, so the credential value stayed in the inherited environment of
  every tool, connector and sandbox subprocess — readable from
  `/proc/self/environ`, a core dump, or an `env`-printing bug. A tool never
  needed to call the broker at all, which is the exact opposite of what
  brokering is for. Reading a credential is now what registers its variable as
  a secret: `Credential::bearer_from_env` calls `deny_env_var`. Placing it
  there rather than at a flag-parsing site is the point — **four** hosts read
  credentials this way (`areev run`, `areev trigger run`, and the Python and
  Node bindings), so a fix at any one of them would have left the other three
  open. Children still receive `AREEV_EGRESS_URL` + `AREEV_EGRESS_TOKEN`,
  which are applied after the environment policy.

  The existing test did not catch this because it removed the variable from
  the *parent* before spawning, validating the broker's request path rather
  than the deployment where an operator exports a token and leaves it
  exported. The regression test keeps it exported.

- **The sandbox seam spawns under `EnvPolicy::ClearExcept`.** A wasm host has
  no claim on the operator's whole environment, and it is now also the process
  holding a broker token. Native code blobs keep `InheritExcept` — they are
  ordinary programs and may legitimately read an ambient variable.

- **A credential reflected in a response body is scrubbed.** Response headers
  never cross the broker, so the body was the only channel by which an echo or
  verbose-error endpoint could bounce the injected `Authorization` back to the
  caller and into the audit trail.

- **The private-space deny recognized only canonical IP literals.** Under an
  unrestricted egress policy, `is_private_destination` is the sole control
  stopping a synced capability tool from reaching loopback, link-local, or
  metadata address space — but it parsed the host with `Ipv4Addr::from_str`,
  which accepts only dotted-quad. A libc resolver (and therefore ureq) still
  maps the historical `inet_aton` forms to the same address, so a Tool grain
  declaring `hosts: ["http://2852039166"]` — decimal for `169.254.169.254`,
  the cloud metadata service — sailed straight through it: the exact case the
  check exists to close. It now canonicalizes decimal, hex, octal, and short
  (`127.1`) forms, and covers the RFC 6598 shared/CGNAT range
  (`100.64.0.0/10`), which `Ipv4Addr::is_private` does not.

- **A credential could return after the redirect chain left its origin.** The
  same-origin check compared each hop against the URL the caller *started*
  at, so a chain `A(cred) → 302 B (cross-origin, cred dropped) → 302 back to
  A/<path B chose>` re-attached the credential on the final hop — more
  permissive than browsers or `curl --location`, which drop it for good once
  the chain leaves the origin. A hop to a different origin now retires the
  credential for the rest of the chain, and the success audit records the
  credential name only when it actually rode the final request — not
  whichever name the caller asked for.

- **A shared broker re-journaled an earlier run's egress calls as its own.**
  `areev trigger run` reuses one broker across the runs it fires in sequence,
  and `Broker::calls()` accumulates for the broker's whole life without
  draining — so a run's journaling cursor starting at 0 re-wrote a prior
  run's already-journaled calls into the immutable store a second time, under
  the new run's id, principal, and clock. The cursor now seeds from what the
  broker already holds at drive entry.

- **A handful of fail-open edges closed during review, before anything
  shipped**: `--credential NAME=VAR@` with an empty principal (a typo, or an
  unset shell variable in the owner position) now refuses rather than
  silently binding an unbound credential; a poisoned mutex on the
  per-principal owner map now fails closed instead of skipping the owner
  check; a `wasm32-areev-io` tool with no `capabilities` is refused at write
  time, matching the check the manifest already made at run start; and
  `max_response_bytes` clamps rather than truncates on a 32-bit target.

### Added

- **Capability tools: an I/O tool can be a grain** (#101). Tier C was correct
  for pure compute and, for two releases, that made it half a promise — it is
  the **only** tier producing a persistable, content-addressed tool, and it
  forbade all I/O, so the tools every real agent needs (poll a mailbox, append
  a sheet, call a model) could not be grains. The options for an I/O tool were
  a native blob (persisted, but *not sandboxed — it runs as you*, and
  platform-specific) or a host `--tool-cmd` script (sandboxed by nothing, and
  outside the memory entirely).

  The tier now has two runtimes, because there are two determinism stories:

  | Runtime | Import set | Determinism |
  |---|---|---|
  | `wasm32-areev` | `areev::emit` | pure — re-execution-provable (unchanged) |
  | `wasm32-areev-io` | `+ areev::fetch` | deterministic *modulo journaled effects* |

  **The guest still never gets a socket.** It gets one unforgeable capability
  to *ask the host*; the sandbox binary's trusted Rust half forwards over
  loopback to the credential broker, holding a revocable broker token and
  never a credential. This needed no new IPC — the engine already injected the
  broker's address and token into that process for uniformity, inert only
  because the *guest* could not reach them. The isolation claim is
  strengthened, not weakened: no socket, no credential, no clock, no
  environment, and the host enforces policy and records everything.

  A new `capabilities` field on the Tool grain declares what a module may
  reach — hosts, methods, path prefixes, credential names. It **declares; it
  never grants**: the effective set is `declared ∩ host-granted`, checked on
  every call, so a declaration can only narrow what `--allow-host` /
  `--credential` / `--tool-egress` already permitted. That is the same split
  `--allow-executor` makes for the code itself — the declaration replicates
  with the bundle, the authority does not. What it buys is audit (a synced
  memory says what a tool may reach without reading anyone's command line) and
  a **tighter** bound than the host grant can express: `--allow-host`
  allowlists hosts only, while a capability may pin `path_prefixes`, closing
  the exfiltration case a host-only grant structurally cannot — a malicious
  tool POSTing stolen context to an *allowed* host's upload endpoint.

  Deny by default throughout, and enforced at five heights: CAL refuses a
  malformed declaration at **write** time; the manifest refuses a bad
  runtime/declaration pairing at **start** and freezes the declaration beside
  the pinned runtime, so a mid-run supersession cannot widen reach; dispatch
  refuses a capability module whose host wired no broker, naming the missing
  flag; the sandbox refuses a module importing `areev::fetch` without
  `--allow-fetch` at **instantiation**, by name, before one instruction runs;
  and the broker checks declaration, grant, allowlist, method, call budget and
  response ceiling on **every call — and every redirect hop**, so a `302` on a
  declared host cannot walk a module off its declared paths. The
  `path_prefixes` match refuses evasive shapes (`..` segments, `%2e`/`%2f`/
  `%5c`, backslashes) outright rather than normalizing them, the response
  ceiling bounds what the broker *reads* rather than measuring after
  buffering, and `areev::fetch` is non-reentrant by mechanism — a guest whose
  `alloc` calls `fetch` again gets `-1`, not a recursion. Two further gates make the runtime safe for
  a process serving more than one user: a capability declaration cannot reach
  loopback/private/metadata address space by itself (that takes an explicit
  `--allow-host` entry, and the rule binds every redirect hop), and
  `--credential name=VAR@principal` binds a credential to its owning run
  principal so a run executing as anyone else — or as none — is refused it. The
  driver binds the run principal automatically. The host-prefix grammar has one parser,
  in `areev-core` beside the grain field, shared by the write path and the
  broker — two would be how a tool becomes writable and then unrunnable.

  Ceilings are `runtime_limits` keys (`max_calls`, `max_response_bytes`, next
  to `fuel` and `max_pages`), and an overrun is a typed error, never a
  truncation.

- **Successful brokered calls are journaled, not only refusals.** A new
  `egress_call` Observation in `agent:harness` records caller, method, final
  URL, status, redirect count, request and response **digests**, response size
  and the credential **name**. "It was allowed to reach Gmail" is a policy
  statement; "it sent these four requests" is the evidence, and only the first
  was in the memory before. Bodies are digests because a grain is immutable
  and replicates; the credential is a name because that is all the broker ever
  received. Refusals dedup on `(caller, destination, reason)` and calls do
  not — a refusal is a policy fact and forty retries are one of them, but a
  call is an effect and forty are forty. Neither is a journal entry, so
  `verify` stays byte-identical whether or not a broker was configured.

### Changed

- **A non-2xx from an upstream reaches the caller as a status, not a broker
  error.** `http_status_as_error` had to be turned off for the broker to read
  a redirect's `Location` at all, and it fixes a smaller wrong on the way: a
  404 or a 429 used to arrive as `502 {"error": "upstream: …"}`,
  indistinguishable from the connection having failed. The broker's contract
  is to answer with the response, so it now does.

- **The Node binding publishes as `@areev/areev` again, not `areev`.** npm's
  similarity filter still 403s the unscoped name against `argv` — the
  1.5.2 release attempted it twice (once before, once after an unrelated SBOM
  fix) and both times the four platform packages published while the main
  package failed, leaving a broken partial release on the registry. A
  support ticket for the unscoped name is open; until it resolves, `npm
  install @areev/areev` is correct and `npm install areev` is not. crates.io
  and PyPI are unaffected.

### Not in this release

Deliberately out of #101's first phase: verify-by-re-execution against the
recorded call log, connectors resolved as capability tools by content address,
concurrency, streaming, raw sockets, and guest-visible clock or RNG — the last
of those permanently, because it is the determinism boundary.

## [1.5.2] — 2026-08-22

### Fixed

- **A trigger builds the same runner `run start` builds** (#90). The trigger
  path constructed a deliberately reduced runtime — a bare `CommandExecutor`
  with no model — so a plan with a **code-carrying (Tier C) node refused at
  start with `RUN-E018`** and one with an **abstract node with `RUN-E006`**,
  no matter which flags the operator passed; the same plan ran happily from
  `run start`. `--context-query` (#92) and `runtime` (#86) shipped in the same
  release and were meant to compose; used together the run refused, so an
  agent could have declared context **or** sandboxed tools on the trigger
  path, not both. `trigger run` and `trigger deliver` now take
  `--allow-executor`, `--executor-cache`, `--sandbox-cmd`, `--model` /
  `--base-url` / `--key-env`, the egress trio, and the observers — from one
  shared builder rather than a second copy, so a stack that grows a component
  cannot grow it on only one path. Both bindings gain `allow_executor`,
  `executor_cache` and `sandbox_cmd` on `trigger_run`/`trigger_deliver`.
  Every setting also reads its `$AREEV_RUN_*` variable (flag wins), because a
  heartbeat is a cron line, not an interactive command. A firing now also
  starts runs with **no** `--tool-cmd` at all — a plan whose nodes are all
  pinned code, or all abstract, needs no subprocess, and gating on one was the
  same reduction one level down. The pin is still the authorization and still
  comes from the host; what changed is where the host may state it, not who
  may. `RUN-E018`/the runtime refusal now name `areev trigger` among the
  surfaces to pin on — the old message named three, none of them the one the
  operator was using.
- **A trigger-started run carries the budgets it was given.** `run start` has
  taken `--max-tokens`/`--max-usd`/`--max-wall-ms`/`--ask-ttl` on every
  surface since the runtime shipped; the trigger path took none of them and
  built `RunOptions::default()`, in the CLI and both bindings alike. Moving a
  workflow behind a trigger therefore dropped every ceiling silently — on the
  one path that fires unattended, where an unbounded run has nobody watching
  it and an ask with no TTL parks forever.
- **`areev trigger --credential NAME=VAR` refuses an unset variable** instead
  of dropping it. `run start` and both bindings already did; the trigger path
  dropped it silently, which does not stay silent — it surfaces downstream as
  an unexplained 401 from someone else's API, hours later, on a heartbeat
  nobody is watching.
- **A `sha256:`-prefixed workflow reference fires** (#73). Both spellings were
  accepted at declaration and only the bare one worked at evaluation, which
  hex-decoded the whole string: the trigger validated, listed, reported
  `waiting` forever, then died at fire time on `FMT-E001: invalid hex hash:
  Odd number of digits`. References are now read through a known scheme
  prefix (`sha256:`, `grain:sha256:`) everywhere and **normalized to the bare
  form on write**, so `trigger_list` returns what was declared and a
  round-trip comparison matches. A reference that is not an address at all is
  refused at declaration and reported as `unusable` by `trigger status`
  (`TRG-E002`) rather than sitting in `waiting`.
- **`name` is returned by every trigger read surface** (#73). It was accepted
  on write and read back by nothing, so identity fell onto the workflow hash
  — which is stable only until the plan is re-declared. `areev trigger
  list`/`status`/`show`, `trigger_list()` and `trigger_status()` now carry it,
  and `areev trigger add --name` sets it. A blank name is treated as absent.
- **`trigger add` says at declaration time what the plan will need at fire
  time** (#73). Pointing a trigger at a plan whose nodes do not resolve used
  to fail at the *first firing*, on the operator's mailbox rather than at
  their keyboard, with `trigger status` reporting `waiting` in between. It now
  warns when the workflow is not in the memory, and when nodes are abstract
  (naming them, and the model configuration they will need). A warning rather
  than a refusal: a plan can arrive by sync afterwards, a Definition can be
  added later, and abstract nodes are legitimate with a model configured.
- **The Python docs-example guard no longer asserts a block the README does
  not have.** The 1.5.1 revamp made the README visual-first and removed its
  Python proof block; the guard kept asserting it, so the `python` CI job went
  red on a docs change — the guard outliving the thing it guarded. It now
  covers the two docs that do carry a block.

### Added

- **SSO proxy secrets rotate without a zero-overlap cutover** (#79).
  `areev ui --sso-secret-env-next VAR` opens a window in which **either**
  secret proves the proxy, so a fleet moves over one node at a time and the
  old value is retired once nothing presents it — TLS key rotation's shape.
  The secret is impersonation-grade (it can assert any identity, including
  approval-capable principals), and rotating one atomically across a proxy
  fleet is not achievable in practice, so the honest choices were an outage or
  a gap — and an operator facing either under suspected-compromise pressure
  defers the rotation, which is the outcome that actually costs. Two secrets
  at a time, deliberately; both compared in constant time with no
  short-circuit, so timing cannot reveal which matched; rotating to the same
  value is refused; and the console warns on **every** start while the window
  is open, because a rotation left half-finished is an extra live credential.
  New runbook: [`docs/runbooks/sso-secret-rotation.md`](docs/runbooks/sso-secret-rotation.md),
  covering the planned rotation **and** the suspected-compromise case, where
  the answer is a hard cutover and *not* a window.
- **Release artifacts carry provenance and an SBOM** (#81). All three release
  workflows now attach a Sigstore-backed build provenance attestation
  (`actions/attest-build-provenance`, keyless — no key for this project to
  lose) and publish a CycloneDX SBOM alongside the artifact; npm packages also
  publish with `--provenance`, which is what `npm audit signatures` checks.
  The bindings get **two** SBOMs each, because a wheel or a `.node` addon
  links a Rust tree `pip`/`npm` cannot see, so a package-manager-only bill
  would truthfully describe almost nothing. Verification is one command
  (`gh attestation verify …`), documented in
  [`docs/security-model.md`](docs/security-model.md). This complements
  `cargo-deny` rather than replacing it: `cargo-deny` says the dependencies
  are acceptable, the SBOM says which ones shipped, the attestation says who
  built them.

### Documentation

- `docs/triggers.md` gains **the runner a firing gets** (the full flag /
  binding / environment table), **the plan has to resolve before the trigger
  fires**, **re-declaring a plan mints a NEW plan**, **naming the workflow**,
  and **what the run receives** — a trigger wraps the item as
  `{trigger, connector, scope, item}` while `run start` passes its input
  through unchanged, so one plan started both ways sees two shapes, and a tool
  reading a top-level key fails on the trigger path only while the pass still
  reports `runs_started: 1` with no errors.
- **Re-adding a Workflow is not free, and the docs now say so** (#73). Grains
  are content-addressed over the whole `.mg` blob and the header carries
  `created_at`, so two identical `add("workflow", …)` calls return different
  hashes: an idempotent-declare loop mints a new plan every boot while the
  trigger still points at the old one, silently. Excluding `created_at` from
  the address is not an option — canonical serialization is frozen, and
  moving it would change every content address ever computed and break OMS
  conformance. `docs/triggers.md` documents the recall-first declare pattern
  instead.
- `docs/run.md` records **why the embedded backend has no read-only open**
  (#85) — the exclusive lock lives inside a pinned `turso` whose facade
  exposes none, and today's open path writes regardless (DDL replay, the
  telemetry sidecar, heal passes, the anon-vault write-behind), so it is a
  store-level project gated on a re-audited engine bump, not a patch.
- `docs/security-model.md` documents trusted-header SSO and its rotation
  window under data-in-transit, and adds **release artifacts: provenance and
  bill of materials**.

## [1.5.1] — 2026-08-22

### Fixed

- **CAL `WHERE` fails closed** (#91). A filter is now pushed down, evaluated
  per grain, or refused — never dropped. Before, a common field outside a
  grain type's queryable set (`status`, `priority`, `epistemic_status`, …)
  passed validation, fell out of push-down, and returned **everything** with
  only a stderr `CAL-W010` — so `RECALL tools WHERE status = "failed"`
  returned the successes, in the right shape and order. Now a field the
  target type cannot carry refuses with `CAL-E060` before the scan; `NOT`
  and `OR` are honoured with real boolean semantics by the one authoritative
  per-grain evaluator (`NOT tool_name = "x"` returned precisely the set the
  author asked to exclude; `a OR b` pushed only `a`); previously-dropped
  comparators (`confidence < x`, `subject != y`, `IS NULL`) now filter; and
  engine-level fields (`query`, `time`, `entity`, `contradicted`, `scope`,
  `tags`) refuse with the new **`CAL-E061`** where they cannot be honoured
  (under `NOT`/`OR`, unsupported comparator) instead of widening. `EXISTS`
  (which answered `true` if *any* grain of the type existed), `HISTORY …
  WHERE`, and ASSEMBLE's post-filter share the contract. Leniency was
  deliberately not kept behind an opt-in: the safe direction is the default
  direction. `DESCRIBE FIELDS <type>` now lists exactly the registry's
  filterable set for that type.
- **`kind` and `status` are queryable on `tools`** (#91). Definitions and
  execution records are one grain type split by `kind`; without it every
  host invented a child-namespace workaround to keep definitions from
  outranking real results. Both fields are stored omit-default and the
  filter materializes the default (`kind = "execution"`, `status =
  "completed"` match grains that never wrote the field). The phantom
  `tool_phase` field — advertised, parsed, and never written — is removed
  from the queryable set.

### Added

- **`--context-query` can see the firing item** (#92). The declaration may
  bind saved-query parameters from the item's payload with the JSON
  pointers `--dedup-key` already understands: `--context-query
  'triage_ctx($session = /session)'`. The evaluator resolves each pointer
  at fire time and runs the query with those bindings via the parsed-AST
  `RUN` path (no CAL text splicing, so payload values cannot inject CAL).
  Fail-closed with `--dedup-key`'s precedent: an unresolvable pointer or a
  non-scalar value refuses the firing. The whole spelling is stored
  verbatim on the trigger grain (same field, same compact key; the plain
  name form is byte-identical to 1.5.0), so the binding replicates and
  audits with the declaration. Malformed spellings refuse at `trigger add`.
- **Polling connectors can persist CAS blobs** (#93). An item may return a
  `blobs` array (`{filename, mime, b64}`); the **evaluator** — the party
  already holding the writer — stores each entry (`put_blob`, idempotent on
  content), rewrites `"blob": "@N"` payload references to the resulting
  `cas://sha256:…` address, and attaches matching `content_refs`
  (uri/mime_type/size_bytes/checksum, filename in metadata) to the Event it
  writes. Attachments ingested by trigger and by host are now
  indistinguishable: `blob get` works mid-run, dedup is content-addressed,
  and erasure's sole-reference reclamation needs no special case. Budgets
  are enforced on decoded size (16 MiB/item, 48 MiB/response, evaluator
  options) and any contract violation — over budget, undecodable base64, a
  dangling `"@N"` — is the new **`TRG-E011`**: the whole poll refuses with
  the cursor unmoved, because a silently dropped attachment is an invoice
  posting without evidence and a lost item is worse. The RFC 4648 base64
  decoder moved to `areev_core::b64` (one implementation, shared with the
  server's HTTP Basic path).

## [1.5.0] — 2026-08-22

### Added

- **Trigger-started runs carry declared context** (#85). A Trigger grain may
  name a saved query (`--context-query NAME`, field `context_query`, compact
  key `tcq`, omit-default — every existing trigger keeps its content
  address): at fire time the **evaluator** runs it read-only against the
  memory it already holds and places the result into the run input as
  `context`. This is the embedded backend's answer to its own exclusive
  lock — a tool inside a run cannot open the memory its run holds, but the
  evaluator can, and the declaration replicates with the trigger, so what a
  fired run sees is auditable rather than host-local. Fail closed: a trigger
  that declared context never fires without it. The backend divergence is
  now documented (`docs/run.md`): on the PostgreSQL tier reads never block,
  so tools *can* query the memory mid-run — the read-only embedded open
  (#85 proposal A) stays tracked, gated on a deliberate turso bump.
- **Tool Definitions declare their runtime, and the engine dispatches to the
  sandbox** (#86). `runtime: "wasm32-areev"` (+ optional `runtime_limits:
  {fuel, max_pages}`; both omit-default, compact keys `axr`/`axl`) routes a
  pinned `cas://` blob to **areev-sandbox** instead of native exec — the
  engine constructs the sandbox argv itself (`--module <cached blob>
  --fuel N --max-pages N`), so provenance (`--allow-executor`) and isolation
  become independent knobs. The runtime is frozen into the run manifest with
  the address; an unknown runtime refuses at resolve rather than running
  foreign bytes natively; a declared runtime on a host with no sandbox
  refuses at start, naming the missing config. The sandbox runner is host
  config on every surface: `--sandbox-cmd` (CLI), `sandbox_cmd`
  (Python/Node `run_start`/`run_resume`), `$AREEV_RUN_SANDBOX_CMD`
  (`areev serve`). Also: the sandbox's `ForbiddenImport` message and module
  docs no longer claim `areev::alloc` is importable (it is a guest
  **export**; the frozen import set is `areev::emit` alone — regression-
  pinned), and `areev-sandbox`'s version stamp is no longer stale.
- **The executor pin reaches every surface that starts runs** (#87).
  `allow_executor`/`executor_cache` on Python and Node
  `run_start`/`run_resume` (the CLI's comma list), and
  `$AREEV_RUN_ALLOW_EXECUTOR`/`$AREEV_RUN_EXECUTOR_CACHE` set at `areev
  serve` start for MCP — server-bound like `$AREEV_RUN_TOOL_CMD`, because
  the pin IS the authorization. Previously a plan naming a code-carrying
  Definition was unrunnable from every non-CLI surface (RUN-E018 with no
  recourse); the refusal now names the pin mechanism per surface. The
  console's HTTP surface deliberately does not start runs, so it carries no
  pin.

### Fixed

- **Code-carrying tools reach the credential broker** (#87).
  `CodeExecutor::execute_code` now injects
  `AREEV_EGRESS_URL`/`AREEV_EGRESS_TOKEN` on the same terms as
  `CommandExecutor` — granted tools only — and the CLI hands the broker to
  the code executor even without `--tool-cmd`. Previously the pinned blob,
  the authoring style whose provenance the host can actually prove, was the
  one that could NOT use brokered credentials.
- **Two doc corrections** (#87): `docs/run.md` no longer claims the run
  journal lives in `agent:harness` (intents/results/checkpoints live in the
  run's session `--ns`; the manifest and administrative records are the
  `agent:harness` residents — an operator following the old text could
  leave a journal outside every declared policy), and the 1.3.0 changelog
  bullet claiming webhook/manual/composite triggers "fire in a later
  release" is corrected in place — all eight kinds fire since 1.3.0.

- **The tuning seam** — the last mile of the corpus path, closing the slow
  learning loop under the same governance as the fast one. `areev tune --cmd
  'TRAINER'` hands a governed corpus to a **host-supplied** trainer (JSON on
  stdio, stderr inherited, no timeout by default — Areev still never trains
  and takes no training dependency) and registers the returned adapter as an
  `mg:adapter` Fact in `agent:harness`: base model + adapter + quantization
  pinned as one tuple, `derived_from` naming the corpus export manifest, the
  Rule E1 evalset pin embedded. Integrated (`--select … --out`) and
  bring-your-own (`--corpus … --manifest`) corpus modes; lineage cannot be
  asserted from the command line — the manifest must be a recorded export.
- **`adapter_revision` — a new eval-gated recommendation class** mirroring
  `code_revision`: the new builtin `adapter_intake` analyzer (14 builtins now)
  proposes the newest unpromoted candidate per served model; apply is refused
  without a recorded clean run of the pinned evalset and writes an immutable
  `(model:<name>, mg:adapter_promotion)` Fact — the host contract: serve what
  a live promotion names, stop when it is retracted (rollback's inverse).
  One candidate per served model by design; auto-apply is impossible three
  independent ways. When a baseline eval run exists the recommendation
  carries an `evalset:<pin>:failed` metric, so a post-promotion regression
  makes `outcome_review` propose the revert.
- **`areev eval run --model provider:name`** — grade an evalset against a
  model behind the ToolCallLlm seam instead of a host command: how a tuned
  adapter served by vLLM/SGLang (`openai-compat:<served-name>`) or Ollama is
  gated, with `--base-url`/`--key-env`/`--llm-max-tokens`, fail-closed case
  prevalidation, the same scorer as `--tool-cmd`, and the graded model
  recorded in the `mg:eval_run` summary. (`--base-url`/`--key-env` also
  joined `areev run start`'s USAGE, where they existed undocumented.)
- **Gated apply reaches every surface** — the loop's documented full-lifecycle
  parity now includes the gating edge. One shared loader
  (`Engine::gating_evidence`) serves the CLI's `--gating-run`, Python/Node
  `apply_recommendation(..., gating_run=…)` / `applyRecommendation(...,
  gatingRun)`, MCP `areev_recommendations` `gating_run`, and
  `POST /api/loop/apply` `gating_run` — on every surface the stats are read
  back from the journaled `mg:eval_run` Fact, never taken from the caller.
  The console's review queue asks for the gate run id on gated
  recommendations (their rows now carry `evalset_hash`). Fused
  approve-and-apply callers are refused **before** the approval lands when
  the gating run is missing or unknown (`preflight_apply` gained
  `has_gating`; `ensure_executable` now knows a gated revision's Data
  payload is executable — both latent classification gaps exposed by the
  first production producer of gated recommendations).
- **The record family grows two members in the bindings**:
  `record_corpus_export` / `recordCorpusExport` (the immutable export
  manifest, for hosts that select and serialize in-process — the CLI verb
  stays the paved road) and `record_adapter` / `recordAdapter` (the adapter
  registration `areev tune` performs, for hosts that train in-process).
  `record_adapter` now also verifies its lineage anchor **is** a corpus
  export manifest on every surface, not just the CLI.
- **Erasure reaches the seam**: the stale-export notice on
  `forget-subject`/`purge-older-than`/`retention sweep` (and the CAL erasure
  audit) now walks one provenance hop further and names the **adapters**
  derived from a stale corpus — `stale_adapters` beside `stale_corpora` in
  the Tier-2 audit record and `areev audit export`. Still auditable
  suppression and re-derivation, never an unlearning claim.

### Changed

- **The position on weight tuning is stated on the record, and the tuning seam
  is named as roadmap.** Areev's boundary is unchanged and now explicit as a
  named decision in `ARCHITECTURE.md` §10: it emits a governed corpus
  (`areev corpus`) and grades the result (`areev run shadow`, `areev eval`), and
  it never trains — no trainer, no training dependency. What is announced as
  *not yet built* is the seam itself (`areev tune --cmd`, an adapter registry
  grain, promotion as a gated apply); the design of record is
  `docs/areev-adaptive-agents-proposal.md` §5. The SEAL rows in
  `docs/loop-explainer.md` §14 and `docs/loop-reflection.md` are reframed from
  "avoid weight updates" to "order them last, behind a governed corpus and a
  replay harness" — a published competitive argument should not be reversed
  quietly. No accuracy or context-savings claim accompanies any of this until
  the replay harness has measured one.

## [1.4.0] — 2026-08-21

### Added

- **Line coverage is measured, published and gated per crate.**
  `scripts/coverage.py` turns the `coverage` job's LCOV trace into
  `docs/coverage.json`, which the README chart renders alongside the line
  counts. It scores source lines only — `tests/`, `benches/` and
  `#[cfg(test)]` blocks are excluded, because a test body is executed by
  definition — and excludes what that job structurally cannot run (`areev-py`,
  which pytest drives; the benchmark harnesses; the Postgres backend, which
  needs a live server), each exclusion carrying its reason in the JSON.
  Enforcement is **per crate plus a global floor**, not one workspace target:
  a single number lets a regression in one crate hide behind a gain in
  another, and these crates do not carry the same risk. The per-crate floors
  are the tight gate — regression ratchets a couple of points under each
  crate's measurement — with a looser aggregate floor under the whole set,
  deliberately given headroom because a gate that fails on cross-platform
  noise gets lowered, and a lowered floor protects nothing.
- **Tests for the CLI and MCP surfaces that had none** — the `areev trigger`
  read and lifecycle verbs (`show`, `status`, `pause`/`resume`, `render`,
  `deliver`), the `areev hold` and `areev retention floor` guards over
  age-based destruction, and the three MCP tools `mcp_smoke.rs` never called
  (`areev_supersede`, `areev_runs_touching`, `areev_recommendations`,
  including the recommendation lifecycle and every argument refusal). Plus a
  CAL error-contract test that pins the leading-token rule (every `Display`
  begins with its `DOMAIN-Ennn` code), keeps `DELETE`/`ERASE`/`TRUNCATE`/
  `DROP TABLE` rejected at the lexer as repros rather than as a claim, and
  checks that every one of the 78 emitted `CAL-Ennn` codes falls inside a
  range `ERROR_CODES.md` documents. Together these lifted `areev-cli`
  62.1% → 72.3%, `areev-mcp` 65.2% → 73.0%, and the workspace to 80.1%.
- **A real demo memory, committed to the repo** — `data/demo.db` (~800 KB,
  466 grains) holds one coherent story end to end: an accounts-payable
  agent's vendor knowledge and category rules, nine governed runs (six
  posted, one a person refused, one waiting on a person, one honest
  failure), a real open fork
  from two channels editing offline, a declared polling trigger, saved CAL
  queries, and thirteen recommendations that `areev loop run` actually
  computed from that history. Nothing in it is hand-written to look
  convincing; `scripts/build_demo.sh` regenerates the whole artifact from
  `crates/areev-store/examples/seed_accounting_demo.rs`, and
  `scripts/shoot_console.mjs` re-shoots the README's screenshots against it.
- **`examples/agents/invoice-to-accounting/` is runnable**, replacing the
  placeholder README. `./smoke.sh` imports the plan from a portable bundle,
  runs three fixtures through it — one auto-posted, one parked for a human,
  one photographed page that fails rather than posting a blank row — and
  asserts the outcomes, including that the principal who *started* a run is
  refused when it tries to approve it. Keyless: no credential, no network,
  no model key — and CI now runs it (`agent-example`), so the keyless floor
  is enforced rather than claimed.
- **`areev_search` and `areev_nearest` join the MCP tool surface (23 → 25
  tools), and `serve --mcp` gained `--profile memory|full`.** Both bindings
  and the CLI have had hybrid free-text recall (`search`) and the
  embedding-similarity novelty check (`nearest`) since early on, but MCP —
  the surface an LLM agent actually calls — only ever got structural
  `areev_recall`, which needs the caller to already know the exact
  `(subject, relation)` pair. The natural agent query ("what do we know
  about the Johnson account") is free text, and without `nearest` an agent
  had no cheap way to check "do I already know something like this" before
  `areev_add`, so long-lived sessions tended to accumulate near-duplicate
  facts reworded slightly across turns. Both fail loudly (never a silent
  empty list) when their prerequisite is missing — `areev_search` needs a
  text index or an embedder, `areev_nearest` needs an embedder — naming the
  MCP-specific remedy (`--index-text true` + `reindex`, or `--embed-cmd`),
  not a bindings-only one that doesn't apply to this surface. Separately,
  `--profile memory` narrows both `tools/list` and `tools/call` to the
  twelve read/write/query tools, dropping the thirteen-tool workflow-runtime
  family (`areev_run_*`, `areev_loop`, `areev_recommendations`,
  `areev_tool_provenance`, `areev_record_tool_call`, `areev_run_manifest`) —
  a host that only wants Areev as chat memory no longer hands its agent a
  dozen governed-run tools it will never call; `--profile full` (the
  default) is unaffected. See [`docs/mcp-reference.md`](docs/mcp-reference.md).

- **The console draws a workflow's whole picture on one canvas.** A `Trigger`
  grain names the plan it starts (`trigger.workflow`), and the binding points
  trigger → plan and never the reverse — a plan that grew a list of triggers
  would change content address every time one was added and orphan its own run
  history. That direction is precisely why a flat list is the wrong surface:
  it cannot show you that two triggers start the same plan. Triggers now render
  in a "STARTED BY" lane on the workflow canvas, dashed-bordered and
  dash-arrowed into the plan's entry steps, with the full declaration in the
  inspector — including a `memory` trigger's serialized `Condition` tree said
  out loud (`subject = "globex" AND relation = "open_incidents"`) rather than
  dumped as JSON. They are read-only, and not by preference: CAL has no
  `ADD trigger` and `ADD workflow`'s `ON "..."` clause was removed in 1.3, so a
  console that writes only through `/api/cal` has nothing to write; the panel
  offers the exact CLI command instead of an input it could not honour. Trigger
  nodes are held in their own arrays, never in `WF_DRAFT`, so the Save path is
  structurally incapable of serializing the lane into a plan grain.
- **A run overlay on the canvas.** Selecting a run tints each step by what it
  did in that run — a client-side join over the journal's own Tool grains via
  `mg:step_action:<node>`, with no new endpoint. The journal's Pending-then-
  supersede shape does the work: `isCurrent()` alone leaves exactly one row per
  `(run, node)`, which IS that step's current state. A step still Pending in an
  *open* run is waiting on a person; the same row in a canceled or failed run is
  simply where it stopped, and is drawn grey rather than orange so the UI never
  invites an approval that can never arrive.
- **A Tools page.** Tool definitions and executions are one grain type split by
  `kind`, so they are two tabs of one page: the catalog (each entry opening its
  full configuration — executor kind, input schema as a property table, locked
  params, annotations, and the plans that bind it) and every execution grain,
  grouped by run, with calls made outside any run given their own group rather
  than filtered out of existence. Built entirely on `/api/browse`.

### Changed

- **README is visual-first**: real console screenshots (light and dark, via
  `<picture>`) instead of design exports, an architecture diagram, a
  sixty-second runnable path, and the problem stated as a table before any
  of the mechanism. The stale `dejadb`-branded assets are gone.

- **Console navigation follows the order you meet things in**: Workflows →
  Runs → Tools. The standalone Triggers tab is gone (folded into the canvas
  above); `#triggers` redirects to Workflows rather than dead-ending a
  bookmark, plan cards carry what starts them and how they last ran, and a
  trigger whose plan is not in the current namespace gets an explicit callout
  under the list — a standing rule must never silently vanish from the console.
- **The Runs page groups by what it wants from you** — *Waiting on you* /
  *In flight* / *Finished* — instead of one flat grid that buried an ask under
  finished history. Each card resolves its plan's name, and carries the same
  per-step strip the canvas draws as a rail. The Approve/Refuse buttons now
  disable when the session cannot use them and say which credential is missing:
  `run.respond` refuses a shared console token even when that token can write
  everything else. The Runs page and the canvas overlay read ONE shared run
  index, so the two surfaces cannot drift apart.

### Removed

- **`README.zh-CN.md`.** A translation that lags the README is worse than no
  translation — it was still describing the pre-Console-v2 shape and pointing
  at screenshots that no longer exist.
- **`seed_support_demo.rs` / `seed_workflow_demo.rs`.** Both seeded a
  different fictional company than anything the README now shows.
  `seed_accounting_demo.rs` replaces them, and it is the single source for
  both `data/demo.db` and the example's `plan.mgb`.

### Fixed

- **`areev_recommendations` silently ignored `status: "all"`** over MCP,
  returning only the pending queue. `docs/mcp-reference.md` documents `all` as
  one of the four accepted filters, so an agent asking for every
  recommendation was told — with no error — that nothing had ever been
  approved or applied. The cause was a filter chain that could not tell "the
  caller said `all`" from "the caller said nothing", since both arrive as
  `None`; a dropped filter fails **open**, and the wrong answer goes straight
  into a model's context. Now pinned by `mcp_smoke.rs`, which asserts `all` is
  a superset of `pending`.
- **Run checkpoints read as "A state with no readable text" in the console's
  memory browser.** A checkpoint's body is the scheduler's serialized state,
  which has no sentence in it, so every one of them fell through to the
  type-name fallback — on any file with governed runs in it, the browser's
  default page was a wall of identical unreadable rows. They now say which
  run and which step they belong to. (What remains is a design question, not
  a bug: whether runtime bookkeeping belongs in the plain memory browser at
  all.)

- **The console's Triggers tab rendered into a pane that never became
  visible.** Every page section ships `hidden` in the markup and is revealed
  only by the one array in `render()` that clears the attribute; `'triggers'`
  was missing from it. The hash routed, the nav item highlighted and
  `renderTriggers()` filled its container on every render — while the section
  stayed hidden along with all eight others, so the tab showed an empty page.
  Nothing in the file could catch it, because the defect is a *missing* string
  rather than a wrong one: a test now parses `console.html` and asserts that
  the set of `id="page-X"` sections, the sidebar's `data-page` values and that
  array agree.

- **A refused egress-broker call could reset the caller's own connection
  instead of delivering its 401/403 JSON body.** `serve_one` read the
  request's token, decided to refuse it (unknown token, or a caller with no
  grant), wrote the response and dropped the connection — all without
  reading the request body the caller had already started sending. Closing a
  socket with unread data queued sends an RST rather than a clean FIN, so
  under enough scheduling delay the caller's own `write` could fail with a
  raw `ConnectionReset` and never see the refusal at all — a security-
  relevant "why was I denied" path degrading to an opaque I/O error under
  load. Found as a one-off `ConnectionReset` in the test suite during the
  1.3.1 release, confirmed as a real, reproducible defect (not test
  flakiness) by isolating it: 5/60 failures on the pre-fix code under
  verified CPU load, 0/60 after. The two refusal paths whose bodies are
  always small and legitimate (bad token, no grant) now drain the request
  body before responding; the "body too large" refusal deliberately does
  not, since draining an oversized claimed body is the resource-exhaustion
  risk that refusal exists to avoid. A regression test forces the same race
  deterministically, without needing artificial system load, by making the
  body large enough to force real TCP backpressure rather than fit entirely
  inside OS socket buffers — verified to fail on the very first run against
  the pre-fix code.

## [1.3.1] — 2026-08-20

### Added

- **Triggers reach the Python and Node bindings.** 1.3.0 shipped the trigger
  evaluator to the CLI only — `areev-trigger` was a dependency of `areev-cli`
  and nothing else, and there is no MCP tool either — so a binding host could
  *declare* a standing rule (the `Trigger` grain has always been authorable
  through `add("trigger", …)` and queryable through `RECALL triggers`) but had
  no way to **fire** one. It had to shell out to the `areev` binary: a second
  artifact to ship, pin and sign per deployment, for a rule the process was
  already holding the memory for. All nine subcommands are now methods —
  `trigger_add`/`list`/`show`/`status`/`run`/`deliver`/`pause`/`resume`/`render`
  (camelCase on Node) — returning the same `EvalReport`/`TriggerStatus` JSON the
  CLI prints under `--format json`. Two deliberate differences from the CLI:
  `trigger_add` also runs the schedule validation `add("trigger", …)`
  structurally cannot (cron parsing, the UTC-only refusal, a composite's gate
  against its own members — that check lives in `areev-trigger`, above the CAL
  grain builder), and an unset `--credential` variable is refused rather than
  silently dropped, because a host wiring this up programmatically has no
  console on which to notice, and the omission would otherwise surface as an
  unexplained 401 from someone else's API. Still no daemon: `trigger_run` is a
  call the host makes on its own heartbeat.
- **`anon_key` is reachable outside Rust.** The host-supplied anonymization
  root added in 1.3.0 (#46) was settable only through `AreevOptions` — not from
  the CLI, and not from either binding — so the feature whose whole purpose is
  making the mapping vault and value-derived tokens work on **Postgres** (which
  refuses `encryption_key`, a page-cipher capability) and on plaintext files
  was unreachable from the two surfaces those deployments actually use. Now
  `--anon-key-env VAR` on any CLI command and `anon_key=`/`anonKey` on both
  constructors, as 64 hex characters. The CLI takes the variable *name*, never
  the key, so it stays out of shell history and `ps`, and that variable joins
  `--passphrase-env`/`--token-env` in the deny-list every subprocess seam
  scrubs. A malformed key is refused at open rather than deriving a different
  token space — the failure mode that looks like working software right up
  until a rehydrate comes back empty.
- **Abstract nodes can run from a binding.** `run_start`/`run_resume` (and
  their camelCase twins) take `model`, `base_url`, `key_env` and
  `llm_max_tokens`. Both bindings hard-coded `llm: None` when building the
  `Runner`, so a plan with an abstract node refused at load with `RUN-E006` and
  there was no argument that could have prevented it — all of #45's provider
  and credential work (Vertex under workload identity, the feature-gated
  providers) was unreachable from the Python or Node agent service the bindings
  exist for. The spec is resolved *before* the run is journaled, so a bad
  provider or a missing key fails without leaving behind a run that can never
  advance. `trigger_run`/`trigger_deliver` take the same arguments, so a
  trigger may start a plan with abstract nodes.

### Fixed

- **A trigger that could never fire was stored, and then looked healthy**
  (#67). `areev trigger add` validated a declaration and refused a bad one, but
  `add("trigger", …)` — the path a host authoring programmatically actually
  reaches for — performed no equivalent check. The evaluator then counted the
  result under `not due`, which is indistinguishable from a healthy trigger
  waiting its turn, so the symptom was work silently not happening on whatever
  schedule was supposed to be running, with a green `trigger status`. Both
  binding write paths now run the schedule check (cron parse, the UTC-only
  refusal, a composite's gate against its own members). Because authoring-time
  validation cannot be the only defence — a declaration can arrive by bundle
  import from an implementation that validated differently, or predate the
  check — the evaluator also reports one rather than assuming it was caught: a
  new `unusable` counter on the run report, counted **apart from**
  `skipped_not_due`, an `unusable` reason on `trigger_status()`, and an
  `unusable` state in `areev trigger status` instead of `waiting`. Such a
  trigger is never reported as `due`.
- **A top-level `timezone` on a JSON trigger declaration was silently
  discarded** (found while reproducing #67). The evaluator reads
  `config["int:timezone"]`, which is where the CLI's `--timezone` writes, but a
  hand-written declaration naturally spells it `"timezone"` at top level — and
  that landed in `extra_fields`, where nothing reads it. The trigger was
  stored, reported healthy, and fired in UTC while its author believed it was
  on local time: silence, on a schedule. It now maps to the config key, and a
  declaration that sets both to *different* values is refused rather than
  resolved by a precedence rule nobody would remember.
- **`trigger render --target k8s-cronjob` emitted the authoring host's local
  binary path into a container spec** (#69). The manifest paired
  `image: areev:latest` with `command[0]` set to `std::env::current_exe()` —
  an absolute path from the machine that ran the render, guaranteed wrong
  inside the container, and sitting next to a right-looking `image:` line so it
  was not obvious which half the operator was meant to fix. Container targets
  now use the name on `PATH` in the image (`areev`); the host targets
  (`cron`, `launchd`, `systemd`) keep the absolute path, which is correct for
  them because they run on the machine that produced the render. The rendered
  `--db` path carries a comment saying it must resolve inside the container.
  The regression survived because the render test's context already used
  `exe: "areev"` — the same string the fix produces — so a render that spliced
  in `current_exe()` looked identical to one that did not; the new test uses a
  path that could only have come from the authoring machine.

## [1.3.0] — 2026-08-20

### Added

- **Repository quality metrics, generated and gated** — `scripts/repo_stats.py`
  measures the tree (source vs test lines, test count, error codes, per-crate
  breakdown) and emits five artifacts: a light and dark SVG for the README, a
  GitHub-renderable `docs/repo-stats.md`, a standalone `docs/repo-stats.html`
  report, and `docs/repo-stats.json`. Test code is counted **per block, not per
  file**, so a source file with a `#[cfg(test)]` module contributes its
  implementation to source and only the module body to tests — file-granularity
  counting inflates the ratio roughly 4x. A new `stats` CI job runs `--check`
  and fails the build when the published figures drift more than 2% from the
  tree, so the README's numbers cannot go quietly stale.
- **`scripts/check_versions.py`** — asserts that all five version sites agree
  (`[workspace.package]`, `areev-py/pyproject.toml`, `areev-js/package.json`,
  `areev-js/Cargo.toml`, and the ~54 literals baked into the generated
  `areev-js/index.js`), optionally pinned to the release tag. Run as a
  `versions` job on every CI run and as a `preflight` gate in the PyPI and npm
  release workflows. Both drift modes it catches have shipped before: a
  workspace-only bump makes the publish workflows skip-existing over the
  released version (a green run that ships nothing), and a `package.json` bump
  without regenerating `index.js` breaks `require()` for anyone with
  `NAPI_RS_ENFORCE_VERSION_CHECK` set.

- **`ASSEMBLE` literal sections and pinning** (#42). `label: LITERAL "…"`
  renders host-supplied text at its authored position; `label: PIN …` marks a
  source non-degradable — costed off the top and never trimmed, with
  **`CAL-E122`** when the pins alone exceed `BUDGET`. A compliance-mandated
  instruction can now live in the statement instead of as a mutable grain, and
  cannot be summarised away by a long conversation. Render order is documented
  as FROM-clause order, explicitly independent of `PRIORITY`, with a test.
  **Out-of-order `ASSEMBLE` clauses are now a parse error** rather than
  silently detaching. New CAL syntax ahead of the OMS spec — recorded as a
  named decision in `ARCHITECTURE.md` §10.
- **A host-supplied anonymization key** (#46). `AreevOptions::anon_key` is the
  HKDF root for the session/memory/vault subkeys when given, else the page key
  as before. The mapping vault and deterministic value-derived tokens now work
  on **Postgres** — which refuses `encryption_key` because it is a page-cipher
  capability — and on plaintext files. Never persisted; rotating it is a
  crypto-erasure of the mapping table. Conformance case on both backends.
- **Healthcare / national-ID detectors and CI-testable fixtures** (#47).
  Singapore NRIC/FIN (weighted mod-11 with era offsets) and UAE Emirates ID
  (`784` prefix + Luhn) are checksum-gated; MRNs are cue-gated on a nearby
  `MRN`/`medical record number` rather than matching bare digit runs.
  `co_occurrence` rules express "redact A when B is within N characters" — a
  name beside a condition is health data, which no per-category action can
  say — and `term_sets` name the categories they compare. `areev anonymize
  test --fixtures F` asserts must-redact / must-not-redact and exits non-zero
  on any miss or false positive.
- **Pluggable LLM credentials and feature-gated providers** (#45).
  `areev_llm::cred::Credential` mints the auth value per request instead of
  reading a `String` once, so Application Default Credentials work: a
  `vertex:<model>` provider reaches the **regional** `aiplatform` endpoint under
  workload identity with no key on disk (the region is never defaulted and
  `global` is refused). Service-account key JSON is refused by name — signing
  its JWT needs an RSA dependency this tree does not carry. Providers are
  individually feature-gated; **OpenRouter is off by default**, so a regulated
  build can state that its artifact cannot reach a third-party router.
- **A parsed-statement cache, and `calPrepare`** (#44). The executor caches
  parsed statements by exact text, so a real-time turn stops re-lexing and
  re-parsing on every turn — and it serves every surface, not just one. The
  bindings built a fresh executor per `cal()` call and so could never hit a
  cache; one executor now lives on the handle. `calPrepare`/`cal_prepare`
  validates and warms a statement at startup. `RESULTS.md` §1b adds measured
  binding-level p50/p95/p99 for `RECALL`, a three-source `ASSEMBLE`, and
  `thread_tail`, on both backends.
- **Executable, undoable definition rewrites in the loop** (#28). A proposal
  may rewrite a saved query or template — where a self-improving agent's
  prompt-assembly actually lives. `OmsSubstrate::definition_inverse` records
  the statement that restores the previous definition (or a `DROP`), so
  `ROLLBACK` really undoes it; a substrate that cannot produce one refuses the
  apply rather than applying something rollback could not reverse. Definition
  targets are excluded from auto-apply by name, like `code` and `evalset`.

- **Triggers** (#36): a standing rule that starts a workflow, declared as a
  `Trigger` grain (type `0x0D`) and evaluated by `areev trigger run` — a
  one-shot idempotent command safe to invoke concurrently. There is still no
  daemon and no scheduler; what changes is that the cadence is data in the
  memory instead of a fact buried in someone's crontab.
  - Eight kinds over four primitives: `interval`/`schedule`/`once` (Time),
    `polling` (Time + Poll), `memory` (state predicate), `webhook`/`manual`
    (Push), and `composite`. All eight fire: webhook and manual through
    `trigger deliver`, composites settled in the same evaluator pass as
    their members. (This bullet originally claimed the last three would
    "fire in a later release" — stale before it shipped; `docs/triggers.md`
    was always right. Corrected 2026-08-22, #87.)
  - Idempotency by construction: the run id is derived from
    `(trigger, connector, dedup value)`, so a re-delivered item is one run and
    one recorded skip. Correctness does not rest on the lease — the lease only
    prevents duplicate connector calls.
  - The first poll seeds the cursor and fires nothing, so declaring a mailbox
    trigger does not replay history.
  - `--catchup last|none|all` and `--concurrency forbid|allow|replace` for
    missed occurrences and overrun.
  - Connectors reuse the `--tool-cmd` seam, so there is one subprocess contract
    and they inherit its timeout, output cap and secret scrub.
  - Cron is **UTC only**; a non-UTC timezone is refused with `TRG-E006` rather
    than mishandled across a DST boundary.
  - **Outbound allowlisting** (`int:allowed_outbound_hosts`, Fermyon Spin
    semantics) and **credential brokering**: `--credential NAME=ENV_VAR` gives
    the connector `AREEV_EGRESS_URL` instead of a token, and a loopback broker
    checks the destination and attaches the credential on the way out. A
    destination outside the allowlist is refused with `TRG-E009` before any
    request is made.
  - `areev trigger render --target cron|launchd|systemd|k8s-cronjob` emits
    heartbeat config for infrastructure you already run and creates nothing. The
    rendered interval is the GCD of declared intervals floored at 60s, not the
    shortest one — the memory owns the cadence.
  - `areev trigger deliver` ingests a webhook or manual payload. Areev never
    opens a port: the host owns the listener and hands the payload over.
  - A read-only Triggers tab in the console, on the existing `/api/browse`
    surface with no new server route.
  - CAL: `RECALL triggers WHERE kind = "polling" AND enabled = true` — the
    grain-type plural set grows to 13, which is what typed queryable fields buy.
  - New docs: [`docs/triggers.md`](docs/triggers.md).

- **Run leases** (`RUN-E021`): a run is leased while a driver advances it, taken
  at start/resume, renewed at each superstep boundary, and released when the run
  finishes **or parks**. Two drivers on one run previously last-write-wins in
  the journal, silently — `journal::ingest` overwrites a second result for the
  same key and the owner-nonce check is a documented gap, so the `Tainted` doc
  comment's claim that forked tips were detected was not true of the shipped
  code. This prevents the case rather than noticing it afterwards. An expired
  lease is reclaimable, so a crashed driver does not park its run forever.

- **`areev-sandbox` (Tier C)**: a standalone package that runs a pure `wasm32`
  module with no WASI, a frozen one-function import set (`areev::emit`; `alloc` is a guest export), fuel, a memory ceiling,
  and a module-size cap applied before decode. Deliberately outside the
  workspace so `wasmi`'s tree and MSRV never reach workspace `cargo deny`, MSRV
  checks or test time; it has its own CI job. Protects the host from the tool —
  explicitly not credential protection, which is what the egress allowlist and
  broker are for.

- **`read_blob_offline` in the Python and Node bindings.** The lock-free CAS
  read added in 1.2.1 reached only the CLI, so a `--tool-cmd` subprocess
  written in Python or Node — the common case for a binding host — still had
  no way to fetch an attachment while its own run held the memory. It had to
  shell out to the `areev` binary (a second artifact to ship, pin and sign per
  deployment) or hand-roll the read and risk skipping the content-address
  verification. Same contract as the Rust and CLI paths: no database open, no
  lock, hash re-verified on read, `None`/`null` for a sealed blob.
- **`run_inspect`/`run_oversight_report` in the Python and Node bindings**
  (#34): the two read-only run reports — the frozen manifest, budgets,
  phase, spend, pending asks, and fork lineage; and the EU AI Act Article
  14 answers, measured from the journal — were CLI-only. Both are now
  thin `Runner` methods (`Runner::inspect`, `Runner::oversight_report`)
  the CLI's `areev run inspect`/`areev run oversight-report` call too, so
  a tenant-deployed Python/Node agent service renders them in-process
  instead of shelling out to the CLI binary for two read-only reports.
  `GET /api/run/inspect` on the hub/console now returns the same full
  report instead of a smaller, independently hand-rolled subset.

### Changed

- **README repositioned around adaptive agents.** The pitch led with "embedded
  memory engine" and carried a migration section comparing Areev to other memory
  stores; being another memory player is not the position. It now leads with the
  substrate for agents whose behaviour changes on evidence, under human
  authority, in steps that can be inspected, undone and re-measured — and
  explains the three systems that make that possible (graph engineering, context
  engineering, governance) plus the loop that closes them. Competitor comparisons
  are gone from the README, the package READMEs, and `README.zh-CN.md`;
  `areev migrate` remains documented in `docs/migrate.md` as a capability rather
  than a positioning. Added an Examples section linking the runnable material in
  `examples/`.

  Claim discipline follows the strategy docs' own rules: "self-improving" is
  scoped to the agent's **memory**, never to model outputs; `verify` is named by
  the tier that actually ships (**journal-consistent**) rather than the two that
  do not; `runs_touching` is stated with its limit (a run that merely *read* a
  grain leaves no grain, so nothing can attest to it); erasure reach is stated
  with the archive window it does not cover; and nothing anywhere claims to be
  "compliant".

- **`workflow_dispatch` is now a safe dry run on all three release workflows.**
  `release-npm` and `release-pypi` published to the registries for real on a
  manual dispatch from any branch; their publish jobs are now guarded on
  `github.event_name == 'release'`, matching the guard `release-cli` already
  had.
- **Release builds are `--locked`.** The maturin and napi builds resolved a
  fresh dependency graph at release time, so published wheels and native addons
  could contain a dependency set no test run had ever seen. Both now build from
  the committed lockfile, and `npm ci` replaces `npm install` where a
  `package-lock.json` is committed.
- **The release runbook publishes the GitHub Release *before* crates.io.** The
  PyPI, npm and CLI workflows build from local `path` dependencies and never
  read crates.io, so they had no reason to wait behind the twelve-crate publish
  chain — they now start immediately and run concurrently with it.
  `cargo publish --workspace` replaces the hand-maintained bottom-up tier list
  (which went stale twice and failed mid-publish), with
  `cargo publish --workspace --dry-run` moved into pre-flight.
- **Release workflows carry `concurrency` groups** keyed on the tag, so a
  re-run cannot race a manual dispatch.
- **README**: added a Quality section with the generated metrics chart; removed
  the legacy rename notice and the placeholder overview video; the status line
  no longer restates a version number that goes stale (it points at this file).
  `README.zh-CN.md` kept in sync.

- **One bounded spawn path for every host command seam** (`areev_core::proc`,
  mirrored privately in `areev-loop`, which may not depend on an areev-*
  sibling; `proc_contract.rs` pins the two together). Five hand-rolled copies
  across six seams are gone, and with them three real defects:
  - **No wall-clock ceiling.** A tool that never exited held its run-pool worker
    and then the driver itself, forever. Now 300s by default, then killed —
    surfacing as a retryable `Timeout` for tool effects rather than a hang.
    `CommandExecutor::with_timeout(None)` restores the old behaviour.
  - **No output cap.** stdout was read to EOF into memory unbounded. Now 64 MiB
    per stream, drained past the cap so the child never blocks on a full pipe.
  - **A stdin deadlock.** Every seam wrote its whole payload before reading a
    byte of output, so a child that filled the pipe buffer while still reading
    its input hung, and so did we. stdin now writes on its own thread.

### Removed

- **`Workflow.trigger`** (breaking). A free-text "activation condition" that
  nothing ever read — neither `areev-run-core` nor `areev-run` — so it described
  an activation that could not activate anything, while the console offered to
  set it. A trigger is now a `Trigger` grain that points *at* a plan, which is
  the only direction that works: a Workflow is content-addressed and a run's
  manifest pins its hash, so a plan carrying a list of triggers would change
  address every time one was added.
  - CAL's `ADD workflow "n" ON "..."` clause is removed and **refused by name**,
    with a message pointing at `areev trigger add`. Silently ignoring it would
    leave an author believing they had scheduled something.
  - Old blobs still deserialize: an unknown field is preserved and ignored, so
    this costs a vestigial key in grains already written and nothing else.
  - The console's plan subtitle becomes a read-only shape summary.

### Fixed

- **`crates/areev-js/Cargo.lock` had drifted, and nothing would have caught it
  until a release failed.** areev-js is a detached cargo workspace, so a
  dependency added to a crate it depends on never reaches its lockfile —
  `areev-run` gained `getrandom` and `ureq` for the egress broker and this
  lockfile did not follow. Dependabot's `cargo` entry for `/` does not cover it
  either. Because `release-npm.yml` now builds `--locked`, that drift would have
  surfaced as a failed **release** rather than a failed build. Lockfile
  regenerated, plus two guards so it cannot recur: the `node` CI job asserts
  `cargo metadata --locked` and now builds with the same `npm ci` /
  `--locked` flags the release uses, and `dependabot.yml` gains a `cargo` entry
  for `/crates/areev-js`.

Nine findings from an external evaluation of 1.2.2 as the context assembler
and memory for a regulated healthcare voice + chat agent (#42–#50), plus the
loop's definition-rewrite gap (#28). Every one was reproduced against the code
before it was fixed.

- **`ORDER BY` ranked a truncated window, and vanished on `ASSEMBLE`** (#43).
  A pipeline stage runs over what the statement already returned — a
  `default_limit` page — so `ORDER BY priority DESC | LIMIT 5` returned the
  top 5 *of the newest 50* and looked exactly like a correct answer.
  `CONTRADICTIONS` already widened its scan for this reason; that fix is now
  generalized to every stage with the same shape (`ORDER BY`, type-specific
  `WHERE` post-filters, `COUNT`), with the caller's bound re-applied
  afterwards and **`CAL-W015`** when even the widened scan fills. `ORDER BY
  created_at` is pushed into the scan and is exact at any size — it is the one
  sort key the `grains` table carries as a column; the rest live inside the
  content-addressed blob. `ORDER BY` on a multi-source `ASSEMBLE` now emits
  **`CAL-W016`** instead of being silently discarded. `WITH recency_weight(w)`
  is **implemented** — it was parsed, stored, and read by nothing since 1.0,
  while ten built-in saved queries passed it.
- **`session_id` was a post-filter over a 50-row page** (#49). It is now pushed
  into `idx_thread(ns, session, seq)`, so `RECALL events WHERE session_id = …`
  is bounded by turns of *that conversation* rather than rows of the namespace
  — on a busy namespace the tail of a conversation could be entirely outside
  the window and the query answered "nothing". No new CAL syntax: the existing
  `WHERE session_id` spelling now pushes down. `thread_tail` is exposed on the
  Node and Python bindings.
- **A Postgres handle never recovered from a database outage** (#48). One
  `tokio_postgres` client with no reconnect meant a routine managed-database
  restart (`57P01`) permanently poisoned a long-lived handle. The session is
  now replaced in place, clearing the prepared-statement and BM25-stats caches
  that belonged to it; **reads replay, writes do not** (a write may have
  committed before the connection died), and nothing replays inside a
  transaction. `docs/deployment-profile.md` gains the connection contract —
  connections per handle, open cost, pooling guidance — and its stale
  "advisory-locked single writer" claim is corrected to multi-writer.
- **Windows `require()` failed on a package npm had refused** (#50). The
  Windows leg built fine; npm's spam filter rejected the *name*
  `areev-win32-x64-msvc`, and the release shipped a manifest promising it
  anyway. Scoping the package makes napi derive `@areev/areev-<platform>`
  names, which the filter does not reject — Windows works rather than being
  dropped. `prepare-npm.mjs` now hard-fails a release when a declared target
  produced no artifact. Three stale proposal headers corrected.
- **The CLI aborted with no message on Windows.** Windows gives a process's
  main thread 1 MiB where Linux and macOS give 8, and the deepest paths —
  `areev loop apply` threading the argument dispatcher through the engine, the
  substrate adapter, the CAL facade and the store — sat just over it, so the
  command died with `STATUS_STACK_OVERFLOW` and no output. `main` now runs the
  CLI on a thread whose stack size it chooses, making headroom identical on
  every platform instead of depending on a number the platform picks.
- **`WITH recency_weight(0)` returned more grains than the statement asked
  for.** The re-ranking widens its candidate scan and truncates back to the
  caller's bound afterwards; the widening tested "is the option present" and
  the truncation "is the weight above zero", so a weight of exactly zero — the
  same answer as no option at all — widened and never came back, and
  `RECENT 3` answered with twelve. Both now read one predicate; zero, negative
  and NaN weights all take the unwidened path.

- **Known-identity propagation now reaches `scan_text`/`anonymize_text`**
  (#32): these free-text APIs read the store's known-identity table for the
  facade's default namespace — the same propagation table grain-egress
  reads already build — so a subject interned by an intake step (e.g. a
  `subject` written under the namespace) is now detected/pseudonymized in
  prose passed to these APIs too, not only in `recall`/CAL results.
  `AnonPolicy` grows a `known: [{value, category}]` field so a caller can
  also inject identities it holds but never interned as a grain subject
  (an email's From header, a CRM row, a project codename), each with its
  own detection category. Both APIs' signatures are unchanged; the
  bindings pick this up with no code changes.
- **A cycle's back-edge can now close on any node, not only the plan's
  entry** (#33): a bounded cycle whose re-entry point was a mid-graph node
  (e.g. `analyze -> notify -> gate -> converse -> gate`, the back-edge
  targeting `gate`) validated cleanly and then stalled the run at the
  entry on superstep 1, because the scheduler's AND-join gate required
  that not-yet-resolvable back-edge before the node could ever go Ready —
  a rule only the entry node's unconditional bootstrap sidestepped.
  `PlanGraph` now classifies every edge as a DFS back-edge or not (from
  the same entry-rooted Tarjan traversal that already computes `scc_of`),
  and a node's first activation only gates on edges that could possibly
  have resolved by then. `run oversight-report`'s stall diagnosis also no
  longer blames the entry node when its own edge fired correctly.

### Security

- **Host command seams no longer inherit named secrets.** No subprocess seam
  called `env_clear`/`env_remove`, so `--passphrase-env` (the memory's
  encryption passphrase) and `--token-env` were inherited by every child of
  `--tool-cmd`, `--embed-cmd`, `--anonymize-cmd`, `--llm-cmd`, `--analyzer-cmd`
  and `areev eval`. The CLI wrapped its own copy in `Zeroizing` and then handed
  the raw variable to every child. Both flags name a *variable*, so the names
  are now registered at argument-parse time and withheld from every spawn. The
  rest of the environment is still inherited — an `--llm-cmd` that reads its own
  API key from the environment keeps working.
- **A plan's `tool_name` is validated before it reaches a child.** It arrives as
  `$AREEV_TOOL_NAME` and can come from an imported bundle (import verifies
  content integrity, not authorship). Names outside `[A-Za-z0-9_.-]{1,64}` are
  refused at `run start` rather than mid-superstep.

## [1.2.2] — 2026-08-18

### Added

- **A Workflows tab in the console** (#37): lists Workflow grains as cards
  and opens one into an editable node/edge graph — a deterministic
  left-to-right layered layout on canvas, add/rename/delete a step, rebind
  it to any Tool definition, drag a step's connector dot to wire it to
  another step, set/clear an edge's `WHEN` condition. Saving always writes
  a new `ADD workflow` grain, since plans are content-addressed and
  immutable and "editing" one means authoring a new version; a plan with a
  bounded-cycle edge or a per-node retry count opens **view-only**, because
  `ADD`/`SUPERSEDE workflow` has no surface syntax yet to author either
  (`* N` populates `retries`, not `max_cycles`). No new server routes —
  built entirely on the existing `/api/browse` and `/api/cal` surface.
  `crates/areev-store/examples/seed_workflow_demo.rs` seeds three demo
  plans into the "Northwind Support" corpus.
- **An Analytics tab in the console**: a grain-type census across all 12
  types, a namespace breakdown, a 14-day growth trend, and recall-leg
  status — generalizes the Query page's "WHAT'S IN THIS MEMORY" on-ramp
  (now removed from Query in favor of it) to cover every grain type
  instead of 4, and every namespace instead of just the bound one.

### Fixed

- **Workflow edge arrowheads were never visible, and edge selection didn't
  line up with what was drawn.** The graph stroked each edge along a
  border-adjusted bezier curve but evaluated the arrowhead position and
  click hit-testing on a different curve through the raw node centers, so
  the arrowhead landed inside the destination node (painted over by its
  opaque fill) and a click near an edge sampled a curve offset from the
  one on screen. Both now read off the exact curve that gets stroked.
- **A node bound to another plan ("subgraph") showed as "unbound" in the
  editor's "Runs as" picker**, contradicting the "Subgraph" badge shown
  directly above it — the option list was built from Tool definitions
  only, with no entry for a Workflow-grain target.
- **A crafted `BIND` binding could inject arbitrary CAL into a plan's save
  statement.** Every other value the Workflows editor writes into
  `ADD workflow` (node names, `WHEN`, the trigger, the reason) is quoted;
  the bound hash was spliced in bare. A plan opened in the console can
  have been authored outside it (the Rust/Python/Node API, or a synced
  bundle), so a binding value crafted to look like a hash followed by more
  CAL could append clauses — rebinding other steps or overriding the
  reason — the moment someone reopened and resaved that plan through the
  UI. The hash is now validated against the content-address format before
  it reaches the statement.
- **Drawing a cycle in the workflow editor saved silently and only failed
  later, at run time.** Every edge the console can author is
  unconditionally unbounded (`ADD workflow` has no syntax to re-emit a
  bound on save), so any cycle drawn through the editor was guaranteed to
  fail at run-load with `RUN-E002`. Connecting an edge that would close
  one is now refused up front.
- The sidebar's Workflows nav item didn't reset an open draft or selection
  the way navigating to a bare `#workflows` hash already did, so clicking
  it while mid-edit just re-rendered the same editor instead of returning
  to the plan list.
- Query's "start from a question" examples wrote hardcoded placeholder
  subjects (`"john"`, `"acme-corp"`) that almost never match a real
  memory's own data, so the first thing a new user tried reliably came
  back empty. They now pull an actual subject and value from the file's
  own Facts, falling back to filter-free forms only when the file has none
  yet.
- Console-wide: one shared namespace-picker component ("Namespace  value
  ⌄") replaced three different layouts across Activity, Workflows, and
  Analytics, each with its own alignment quirks; every native `<select>`
  in the console (the "Runs as" picker above, the anonymization policy
  picker) now matches the rest of the UI instead of the browser's default
  box; the "Areev" brand mark is clickable (home) and aligned with the nav
  icons below it; the breadcrumb home icon's optical alignment against its
  trail text.

## [1.2.1] — 2026-08-17

### Fixed

- **A grain carrying a `subject` without a relation or object reached no
  index at all** (#23). Structural indexing required all three positions, so
  an Event *about* a message id or a person was invisible to
  `recall(ns, subject, …)` — a silent empty result on a filter every surface
  accepts. The same root cause was the serious one: `forget_subject` and
  `subject_report` select through those indexes, so the identity's own grain
  survived erasure and went **undisclosed in a DSAR**, while the erasure
  reported success. Such grains now get a subject-anchored row (relation and
  object NULL, because the grain asserts neither — which also keeps the row
  inert to every relation-bound query). Never written to `heads`/
  `entity_latest`: a log entry about a subject has no "current value". Existing
  files are healed on open by a `link_index` stamp bump; the rebuild replays
  the rows and reconstructs `cur` from supersession state, so a reindex neither
  duplicates a grain nor resurrects a superseded one. Pinned on both backends
  (`subject_without_relation_is_indexed`).
- **`DEFINE QUERY` stored bodies that could never `RUN`** (#24). Define-time
  validation skipped parsing entirely whenever the body contained `$` — the
  shape most saved queries have — and fell back to a keyword blocklist, so any
  syntax error was stored and first surfaced when a caller ran it, typically an
  unattended agent long after the author had moved on. The body is now parsed
  at `DEFINE`. Bodies whose parameters sit in positions demanding a literal
  (`RECENT $limit`) are still accepted: the check re-parses with the parameters
  standing in, so only a body malformed *however* it is bound is refused
  (`CAL-E059`). The read-only and destructive guards are unchanged.
- **A Skill's `instructions` could not be reached through any rendered path**
  (#25). The field that *is* the skill was absent from the grain type's
  queryable fields (`PROJECT name, instructions` → `CAL-E060`) and no format
  emitted it, leaving raw JSON recall — which defeats budgeted assembly — as
  the only way to read it. `instructions` and `when_to_use` are now projectable
  and render at full disclosure.

### Added

- **`WITH progressive_disclosure(summary|headlines|full)` now executes**
  (#25). It was documented in `docs/cal-reference.md` but parsed and discarded,
  warning `CAL-W004`. It is the *body* axis, orthogonal to metadata: `summary`
  and `headlines` clip free-text bodies (40/80 chars, the same ladder budgeted
  template renders already use), and `full` leaves them whole **and** adds the
  long-form definition bodies no other tier carries — a Skill's `when_to_use`
  and `instructions`, so they reach a budgeted `ASSEMBLE` instead of being
  injected around it. Omitting the option renders exactly as before, byte for
  byte.
- **The CAS blob store reaches the CLI and both bindings** (#27):
  `areev blob put <FILE>|--stdin` prints the `cas://` URI (idempotent by
  construction), `areev blob get <cas-uri>` writes hash-verified bytes to
  stdout, and `put_blob`/`get_blob` ship in Python and Node — bytes in, bytes
  out, the one documented exception to the scalars-in/JSON-out convention.
  `blob get` deliberately **does not open the memory**: the embedded backend's
  file lock is exclusive, so while a run holds a memory even a reader is
  refused, which put an attachment out of reach of the very `--tool-cmd`
  subprocess the run spawned to process it. Reading the sidecar needs no lock
  and answers no consistency question — a blob is immutable and its address is
  its checksum, re-verified on read. Encrypted memories still open, since
  decryption needs the derived key. No MCP tool, deliberately: blob bytes would
  have to be base64'd into a tool result and land whole in the model's context.
- **Evalset-backed outcome metrics** (#29). A recommendation may carry
  `metric = "evalset:<EVALSET_HASH>:<field>"`, resolved by `areev loop outcomes`
  from the summaries `areev eval run` journals — `passed`, `failed`, `total`
  and `error_rate` work against any evalset, and any other field is read from
  the summary the harness wrote. This moves the honesty boundary legitimately
  rather than breaking it: an evalset run is itself an internal, bounded,
  attributable measurement. Two safeguards are load-bearing. A run journaled
  **before** the apply is never evidence (no run since → not yet measurable,
  and the checkpoint stays due; scoring the baseline against itself would
  report `held` forever, a fabricated receipt). And `MetricSnapshot.higher_is_better`
  states the direction, because the built-in metrics are recurrence counts
  where lower is better while an accuracy is the opposite — read the wrong way,
  the Verify gate would propose reverting the rules that worked. The regression
  comparison now lives in one function both the engine and `outcome_review`
  call. The apply gate (`--gating-run`) and the outcome edge read those
  summaries through one shared reader, so a rule cannot be admitted on one
  reading of an evalset and judged on another.

## [1.2.0] — 2026-08-17

### Added

- **Namespace prefix scoping (`"org.*"`)** — one convention on every read
  surface (CAL `WHERE namespace` / `namespace IN (…)`, the MCP `namespace`
  argument, `areev recall --ns`, ASSEMBLE sources, both bindings): a
  namespace value ending in `*` selects the base namespace **plus its
  descendants through the separator you wrote** (`"org.*"` = `org`,
  `org.sales`, `org.sales.emea` — never `organization`, never `org:x`).
  Malformed patterns (`org*`, bare `*`, mid-string `*`) refuse with
  `VAL-E001` instead of silently matching nothing. Backed by a
  count-maintained namespace registry (`ns_reg`, self-healed on open for
  older files) and a namespace-set recall path through all three hybrid
  legs; the single-exact-namespace hot path is untouched. Scopes widen
  **reads only**: `*` is now reserved in namespace names (writes refuse it;
  replication of pre-existing files still imports), and destruction,
  grants, policy, and point reads keep taking exact namespaces. Under a
  bound principal a prefix expansion **fails closed** — every covered
  namespace must be granted, and the refusal names the pattern, never a
  discovered namespace.

### Fixed

- `WHERE namespace IN (…)` now queries **every** member of the set (union,
  deduped, newest-first across the set); previously only the first member
  was consulted and the rest were silently dropped (#19). A
  `namespace_override`-pinned session now also clears caller-supplied `IN`
  sets, closing the corresponding pin-escape.

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

The first Areev release — the complete memory engine, plus the
governed-agents program: the `areev run` runtime, agent-grade capture, the
ecosystem adapters, and the enterprise plane.

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

[Unreleased]: https://github.com/AreevAI/areev/compare/v1.9.5...HEAD
[1.9.5]: https://github.com/AreevAI/areev/compare/v1.9.4...v1.9.5
[1.9.4]: https://github.com/AreevAI/areev/compare/v1.9.3...v1.9.4
[1.9.3]: https://github.com/AreevAI/areev/compare/v1.9.2...v1.9.3
[1.9.2]: https://github.com/AreevAI/areev/compare/v1.9.1...v1.9.2
[1.9.1]: https://github.com/AreevAI/areev/compare/v1.9.0...v1.9.1
[1.9.0]: https://github.com/AreevAI/areev/compare/v1.8.5...v1.9.0
[1.8.5]: https://github.com/AreevAI/areev/compare/v1.8.4...v1.8.5
[1.8.4]: https://github.com/AreevAI/areev/compare/v1.8.3...v1.8.4
[1.8.3]: https://github.com/AreevAI/areev/compare/v1.8.2...v1.8.3
[1.8.2]: https://github.com/AreevAI/areev/compare/v1.8.1...v1.8.2
[1.8.1]: https://github.com/AreevAI/areev/compare/v1.8.0...v1.8.1
[1.8.0]: https://github.com/AreevAI/areev/compare/v1.7.3...v1.8.0
[1.7.3]: https://github.com/AreevAI/areev/compare/v1.7.2...v1.7.3
[1.7.2]: https://github.com/AreevAI/areev/compare/v1.7.1...v1.7.2
[1.7.1]: https://github.com/AreevAI/areev/compare/v1.7.0...v1.7.1
[1.7.0]: https://github.com/AreevAI/areev/compare/v1.6.5...v1.7.0
[1.6.5]: https://github.com/AreevAI/areev/compare/v1.6.4...v1.6.5
[1.6.4]: https://github.com/AreevAI/areev/compare/v1.6.3...v1.6.4
[1.6.3]: https://github.com/AreevAI/areev/compare/v1.6.2...v1.6.3
[1.6.2]: https://github.com/AreevAI/areev/compare/v1.6.1...v1.6.2
[1.6.1]: https://github.com/AreevAI/areev/compare/v1.6.0...v1.6.1
[1.6.0]: https://github.com/AreevAI/areev/compare/v1.5.2...v1.6.0
[1.5.2]: https://github.com/AreevAI/areev/compare/v1.5.1...v1.5.2
[1.5.1]: https://github.com/AreevAI/areev/compare/v1.5.0...v1.5.1
[1.5.0]: https://github.com/AreevAI/areev/compare/v1.4.0...v1.5.0
[1.4.0]: https://github.com/AreevAI/areev/compare/v1.3.1...v1.4.0
[1.3.1]: https://github.com/AreevAI/areev/compare/v1.3.0...v1.3.1
[1.3.0]: https://github.com/AreevAI/areev/compare/v1.2.2...v1.3.0
[1.2.2]: https://github.com/AreevAI/areev/compare/v1.2.1...v1.2.2
[1.2.1]: https://github.com/AreevAI/areev/compare/v1.2.0...v1.2.1
[1.2.0]: https://github.com/AreevAI/areev/compare/v1.1.0...v1.2.0
[1.1.0]: https://github.com/AreevAI/areev/compare/v1.0.2...v1.1.0
[1.0.2]: https://github.com/AreevAI/areev/compare/v1.0.1...v1.0.2
[1.0.1]: https://github.com/AreevAI/areev/compare/v1.0.0...v1.0.1
[1.0.0]: https://github.com/AreevAI/areev/releases/tag/v1.0.0
