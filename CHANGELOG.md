# Changelog

All notable changes to Areev are documented here. The format is based on
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project
adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Fixed

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
    (Push), and `composite`. The last three are declared and validated now and
    fire in a later release.
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
  module with no WASI, a frozen two-function import set, fuel, a memory ceiling,
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

[Unreleased]: https://github.com/AreevAI/areev/compare/v1.3.0...HEAD
[1.3.0]: https://github.com/AreevAI/areev/compare/v1.2.2...v1.3.0
[1.2.2]: https://github.com/AreevAI/areev/compare/v1.2.1...v1.2.2
[1.2.1]: https://github.com/AreevAI/areev/compare/v1.2.0...v1.2.1
[1.2.0]: https://github.com/AreevAI/areev/compare/v1.1.0...v1.2.0
[1.1.0]: https://github.com/AreevAI/areev/compare/v1.0.2...v1.1.0
[1.0.2]: https://github.com/AreevAI/areev/compare/v1.0.1...v1.0.2
[1.0.1]: https://github.com/AreevAI/areev/compare/v1.0.0...v1.0.1
[1.0.0]: https://github.com/AreevAI/areev/releases/tag/v1.0.0
