# Areev Error Codes

Every user-facing error in Areev carries a stable, machine-readable code so a
bug report only needs the code — it points straight at the variant, the
subsystem, and the source file. This file is the canonical registry.

## Format

```
DOMAIN-Ennn        error   (e.g. MEM-E001, CAL-E116)
DOMAIN-Wnnn        warning (e.g. CAL-W003)
```

- **DOMAIN** — exactly three uppercase ASCII letters, naming the subsystem.
- **E / W** — error or (non-fatal) warning.
- **nnn** — digits, zero-padded to at least three. Unique within a domain.

The code is always the **leading token of the error's `Display` string**:

```
MEM-E001: grain not found: 3288d0d4…
CAL-E116: WITH hyde needs an external LLM and is not implemented in Areev — …
```

So whether a user pastes the bare code or the whole message, we get the same
handle. Each coded error type also exposes a `code()` method returning the bare
code (`AreevError::code`, `CalError::code`, `SchemaSubsetError::code`) for
structured logging and interface envelopes.

## Domains

| Domain | Subsystem | Type / source of truth |
|--------|-----------|------------------------|
| `FMT` | `.mg` binary format, header, canonical serialization, content addressing | `AreevError` — `areev-core/src/error.rs` |
| `MEM` | Grain & memory operations: lookup, supersession, tool grains + schema binding | `AreevError`, `SchemaSubsetError` — `areev-core` |
| `STO` | Turso storage layer: I/O, indexes, op-log, sync | `AreevError` — surfaced from `areev-store` |
| `CRY` | Cryptography: keys, at-rest cipher, signing, crypto-erasure | `AreevError` |
| `VAL` | Request / input validation (cross-cutting) | `AreevError` |
| `CAL` | CAL language: lexer, parser, executor, ASSEMBLE, templates, saved queries | `CalError` — `areev-cal/src/errors.rs` |
| `SYS` | Internal / unexpected engine faults | `AreevError` |
| `LOP` | Areev Loop self-improvement engine: analyzers, recommendation lifecycle, governance gates | `areev_loop::Error` — `crates/areev-loop/src/error.rs` |
| `AUT` | Authorization: principals, verbs, grants, the credential map | `AreevError` — `areev-core/src/authz.rs` |
| `RUN` | The `areev run` scheduler and driver: plan validation, budgets, journal, leases | `RunError` — `crates/areev-run-core/src/error.rs` |
| `TRG` | Triggers: declaration validity, schedules, claims, connectors | `TriggerError` — `crates/areev-trigger/src/error.rs` |
| `PCK` | Agent packs: manifest shape, reference resolution, expected-hash agreement | `areev::pack::PackError` — `crates/areev-cli/src/pack.rs` |

The MCP server, HTTP console, CLI, and Python binding do not mint their own
codes — they surface the underlying `AreevError` / `CalError` (and thus its
code) through their own envelopes (MCP `isError` result, HTTP body, stderr,
`PyValueError`). Two exceptions, both because the crate owns a concept no
other domain names: the `areev-loop` engine crate has zero areev dependencies
and owns `LOP`, and the CLI's **pack library** (`areev::pack`, #315) owns
`PCK` — a pack's manifest, its symbolic references and its `expected_hash`
agreement are the pack format's own rules, and a Rust host calling
`install_pack` has to branch on the cause. REVIEW/APPLY *syntax* errors stay
in the substrate's `CAL` domain; `LOP` covers engine semantics (lifecycle,
gates, analyzers). Store and authorization errors raised while installing a
pack pass through unchanged.

## Registry — non-CAL codes

`AreevError` (`areev-core/src/error.rs`):

| Code | Variant | Meaning |
|------|---------|---------|
| `MEM-E001` | `NotFound` | No grain at the given content address |
| `MEM-E002` | `SupersessionConflict` | Head already superseded by a different grain (locally; via import this becomes a fork) |
| `MEM-E110` | `ToolRenderUnsupported` | A Tool grain cannot be rendered to the requested provider format |
| `FMT-E001` | `Format` | Malformed `.mg` blob / header / hash |
| `FMT-E002` | `Serialization` | Canonical (de)serialization failure |
| `VAL-E001` | `Validation` | Invalid request/input (e.g. RECALL with neither subject nor query) |
| `STO-E001` | `Storage` | Turso storage-layer failure |
| `STO-E002` | `StoreBusy` | Another writer holds this memory. Raised when a **second handle** is opened on a file this process already has open — the embedded backend is single-writer per file, and a second handle keeps its own sequence/dictionary allocators. Not raised on the Postgres backend, which admits multiple concurrent writers per memory by design |
| `STO-E003` | `TlsUnavailable` | The DSN asks for an encrypted connection this build cannot make — `sslmode=require`/`verify-ca`/`verify-full` on a binary compiled without the `postgres-tls` cargo feature. A **refusal, not a downgrade**: the alternative is connecting in plaintext to a database the operator asked to encrypt |
| `STO-E004` | `ReadOnly` | A write was attempted through a handle opened with `AreevOptions::read_only` / CLI `--read-only`. Raised on both backends — the postgres backend additionally never attempts the write against the database, so a least-privilege SELECT-only role never sees a raw `42501` |
| `STO-E005` | `ReadOnlyOpenFailed` | A `read_only: true` postgres open could not verify the schema it was pointed at — names whether the schema is absent (needs creating/migrating) or present but not fully bootstrapped (needs an owning role to open it read-write once) |
| `STO-E006` | `SupersessionChainTooDeep` | `Areev::supersession_chain`'s backward walk from a grain to its supersession root did not terminate within `MAX_SUPERSESSION_CHAIN_HOPS` (64) hops — the `supersedes` links are cyclic or corrupt, so the walk fails loudly rather than looping forever |
| `STO-E007` | `AnnIndexUnsupported` | An approximate-nearest-neighbour index (pgvector HNSW) was requested on a backend that has none. Vector recall on the embedded engine is an **exact scan** with no ANN structure to build; answering the request with a silent no-op would leave the caller believing a corpus was indexed while its latency stayed linear in corpus size, so the refusal is explicit |
| `STO-E008` | `SchemaNotProvisioned` | A read-WRITE postgres open found the schema absent, or stamped at an older `meta.pg_schema` version, and the DSN carries `?provision=never` — so **no advisory lock and no DDL were attempted, not even `CREATE SCHEMA`**. The mode exists so a deployment can guarantee its runtime role holds no `CREATE` and its schema changes go through a migration step (`areev provision`, or the operator's own job). Like `STO-E005`, the message names which of the two operator actions is needed — create the memory, or migrate it forward — because they are different jobs |
| `STO-E009` | `LegalHold` | A destruction was refused because the namespace it names is under a legal hold (#278). Its own code rather than `VAL-E001` because "deferred by a hold" is an EXPECTED, reportable outcome a records-retention obligation produces — a host must be able to record it without parsing a message, and to tell it apart from a malformed request. Carries the namespace, the hold's owner and its stated reason. Raised by every destruction path: `forget`, `forget_subject`, the age-based sweeps, and `drop_postgres_schema`. `WITH override_hold BECAUSE "…"` (CAL), `--override-hold --because` (CLI) is the explicit, audited way through; it needs `admin` on the namespace in addition to `erase`/`delete` |
| `STO-E010` | `AsyncContext` | A **blocking** open was attempted from inside an async runtime (#322). `Areev` drives its own current-thread Tokio runtime and `block_on`s it, and Tokio refuses to start a runtime from a runtime worker — so the open panicked from inside Tokio, several frames below anything the caller wrote, naming no Areev API. The coded error names the two supported answers (`AsyncAreev` for the store, `areev_cal::AsyncFacade` for the governed facade) and the escape hatch (`spawn_blocking` / a plain thread). Raised only on a runtime WORKER: an open on the blocking pool is legal and unaffected |
| `CRY-E001` | `CryptoError` | Key / cipher / signing / erasure failure |
| `CRY-E002` | `AttestationInvalid` | An attestation signed by a **trusted** author key does not verify over the content hash it names — the grain or the attestation was altered after signing. At bundle import the whole bundle is refused before any write; `verify --attestations` counts it |
| `CRY-E003` | `AttestationRequired` | The import policy is `require` and a grain arrived with no valid attestation from a trusted author (unsigned, or signed by an unknown key). The whole bundle is refused before any write |
| `CRY-E004` | `SigningKeyInvalid` | A signing seed, public key, or trusted-authors document is malformed (wrong length, bad hex, unknown `alg`, unsupported `policy`) |
| `SYS-E001` | `Internal` | Unexpected internal fault (should not happen — file a bug) |
| `CAL-E083` | `AccumulateRetryExhausted` | ACCUMULATE retry budget exhausted (CAL-domain, bubbles through the store) |
| `CAL-E084` | `AccumulateInternal` | ACCUMULATE internal failure |
| `CAL-E085` | `AccumulateBackpressureRejected` | ACCUMULATE inflight cap exceeded |
| `AUT-E001` | `AuthzDenied` | A verb the session's grants don't cover — names the verb, namespace, and principal |
| `AUT-E002` | `AuthzUnknownPrincipal` | A principal name no credential authenticates |
| `AUT-E003` | `AuthzConfigInvalid` | The credential map failed to load or validate (unknown key, bad version, malformed entry — fail closed) |
| `AUT-E004` | `AuthzTokenUnrecognized` | A presented bearer token matched no credential (the message never echoes the token) |

`areev_loop::Error` — Areev Loop engine (`crates/areev-loop/src/error.rs`), append-only:

| Code | Variant | Meaning |
|------|---------|---------|
| `LOP-E001` | `Substrate` | A substrate call (grain read/write, CAL) failed |
| `LOP-E002` | `CalUnsupported` | The substrate cannot execute the given CAL |
| `LOP-E010` | `InvalidTargetRef` | A `target_ref` did not parse to a known scheme |
| `LOP-E011` | `InvalidProposal` | A proposal payload failed validation (incl. missing BECAUSE) |
| `LOP-E012` | `InvalidRecommendation` | A recommendation draft/grain is malformed |
| `LOP-E020` | `LifecycleViolation` | An illegal lifecycle transition was attempted |
| `LOP-E021` | `SelfApproval` | The approving actor authored the recommendation |
| `LOP-E022` | `ScopeDenied` | The caller lacks a required scope (review/apply) |
| `LOP-E023` | `DestructiveGated` | Destructive apply without admin + allow_destructive |
| `LOP-E030` | `AnalyzerFailed` | One analyzer's run failed (its findings are dropped) |
| `LOP-E031` | `ParamInvalid` | An analyzer parameter is outside its `ParamSpec` |
| `LOP-E032` | `CapabilityMissing` | A required substrate capability (forks/telemetry/embeddings) is absent |
| `LOP-E040` | `NotFound` | No recommendation at the given hash |
| `LOP-E050` | `LlmBackend` | The optional LLM enrichment backend (`--llm-cmd`) is misconfigured or failed (never fatal — the contribution is dropped) |
| `LOP-E099` | `Internal` | Unexpected internal fault (should not happen — file a bug) |

`SchemaSubsetError` — portable tool-schema (bind-tool) validation
(`areev-core/src/types/json_schema_subset.rs`):

| Code | Variant | Meaning |
|------|---------|---------|
| `MEM-E101` | `NotObject` | Schema root is not `type: "object"` |
| `MEM-E102` | `BannedKeyword` / `BadFormatValue` | Keyword or `format` value outside the portable subset |
| `MEM-E104` | `ContainsPii` | PII detected in a schema string (description/default/enum/…) |
| `MEM-E105` | `TooDeep` | Schema nesting exceeds `MAX_SCHEMA_DEPTH` |
| `MEM-E106` | `PatternInvalid` | `pattern` failed to compile or exceeded the regex size limit |

`MEM-E103` is intentionally unassigned (reserved, matching the upstream OMS
numbering). `InstanceErrorKind` is an internal `detail` classifier
(`shape` / `type` / `required` / `size`), not a top-level code.

### `RUN` — the runtime (`areev-run-core/src/error.rs`)

These have existed since the runtime shipped and were missing from this
registry; recorded here so the domain is discoverable rather than only findable
in source.

| Code | Variant | Meaning |
|------|---------|---------|
| `RUN-E001` | `Stalled` | No node can advance and the run is not finished |
| `RUN-E002` | `UnboundedCycle` | A cyclic SCC carries no `max_cycles` edge |
| `RUN-E003` | `Unreachable` | A node cannot be reached from the entry |
| `RUN-E004` | `UnresolvedRef` | A node does not resolve to a usable Tool Definition: a binding that names something else (or nothing), or an unbound node whose Definition lives in the plan's namespace rather than the run's |
| `RUN-E005` | `InvalidCondition` | An edge condition does not parse |
| `RUN-E006` | `NoToolLlm` | An abstract node has neither a bound tool nor an LLM |
| `RUN-E007` | `BudgetExhausted` | A budget axis was spent |
| `RUN-E008` | `DanglingIntent` | An intent has no result and the host refuses redelivery |
| `RUN-E009` | `ReplayDivergence` | Verify replay did not reproduce the journal |
| `RUN-E010` | `ManifestMismatch` | The manifest does not match the plan |
| `RUN-E011` | `UnknownAsk` | No pending ask with that `tool_call_id` |
| `RUN-E012` | `Unauthorized` | The principal may not perform the run verb |
| `RUN-E013` | `Canceled` | The run was cancelled |
| `RUN-E014` | `ReducerLawViolation` | A reducer broke its declared law |
| `RUN-E015` | `CheckpointTooLarge` | A checkpoint exceeded its size bound |
| `RUN-E016` | `Tainted` | Duplicate run id, fork collision, or an ask with no journaled intent |
| `RUN-E017` | `RetentionRefused` | Retention policy refused the write |
| `RUN-E018` | `CodeExecRefused` | A code-carrying tool was refused: the host has not pinned its address (`--allow-executor`), the `executor_uri` is not a `cas://sha256:` content address, or a client tool named an executor |
| `RUN-E019` | `InvalidPlan` | The plan failed structural validation |
| `RUN-E020` | `Storage` | The store failed underneath the driver |
| `RUN-E021` | `LeaseLost` | Another driver took this run over mid-flight; this driver's writes are refused |
| `RUN-E023` | `AnonReplayUnsafe` | An anonymization policy covers the run's namespace with a scope whose placeholders are not value-derived, so an abstract node's model boundary would make `verify` diverge |
| `RUN-E022` | `EgressRefused` | A host command's mediated I/O was refused: destination outside the run's allowlist, a method its grant does not permit, a credential it may not spend, a request header it may not set — undeclared, or one the broker owns — or a CAS blob read without the `blob` capability (the trigger evaluator reports the same condition as `TRG-E009`) |
| `RUN-E024` | `ContextExceeded` | An abstract node's transcript is over its context ceiling and nothing is left to fold — the node's input and the kept tail alone exceed it. The ceiling is either `--llm-context-tokens` (measured against the provider's own reported prompt tokens) or, when none was configured, the provider's own limit learned from its refusal; the message says which. Raise the ceiling, bound oversized tool results (`--llm-tool-result-chars`), or split the node |
| `RUN-E025` | `ModelMismatch` | A run is being resumed under a model configuration it did not start under (#287). Raised BEFORE the lease is taken and before any grain is written, so a run that must not continue here does not look like it started to. `areev run fork` is the sanctioned way through: a fork writes a new manifest carrying the new pin and records what it forked from, so the change is a recorded decision rather than undocumented drift. A manifest with no pin — every run written before 1.9.0 — resumes under anything |
| `RUN-E026` | `EngineMismatch` | This run was written by a scheduler generation whose decisions differ from this build's (#288). Only the `scheduler_epoch` is compared, never the version string: a patch upgrade must not strand every parked approval run. The epoch moves exactly when a change makes an existing journal replay differently — the #251 class |
| `RUN-E027` | `ConcurrencyLimit` | Starting this run would exceed a per-memory or per-principal concurrency cap (#296). RETRYABLE by nature: the cap is a backstop beneath the host's own dispatcher, not a verdict on the run. Nothing is written under the run id, so the same id starts once a slot frees, and a trigger firing refused here leaves its item unconsumed (the #129 rule) |
| `RUN-E028` | `TransferLimitInvalid` | A Tool declares a brokered-transfer ceiling (`runtime_limits.max_response_bytes` or `max_request_bytes`) that is not a positive integer, is zero, or exceeds the 32 MiB (33554432-byte) hard maximum (#339). Refused at run start, before any upstream I/O, and never clamped; the write path refuses the same declaration as a `VAL` error. The broker raises it too, before dispatch, for a host that registered such limits directly |

### `TRG` — triggers (`areev-trigger/src/error.rs`)

| Code | Variant | Meaning |
|------|---------|---------|
| `TRG-E001` | `Malformed` | The declaration cannot fire as written (no interval, no cron, a composite with one member) |
| `TRG-E002` | `UnresolvedWorkflow` | `workflow` does not resolve to a Workflow grain |
| `TRG-E003` | `NoConnector` | A trigger is due but the host configured no connector command |
| `TRG-E004` | `ConnectorFailed` | The connector exited non-zero, timed out, or did not emit JSON |
| `TRG-E005` | `ClaimLost` | The lease expired mid-firing and another evaluator took over; the release was refused |
| `TRG-E006` | `BadSchedule` | The cron expression is invalid, or names a timezone this release refuses |
| `TRG-E007` | `UnknownTarget` | Unknown `trigger render` target |
| `TRG-E008` | `UnknownMember` | A composite predicate references a member it does not declare |
| `TRG-E009` | `EgressRefused` | The connector tried to reach a host outside its allowlist |
| `TRG-E010` | `Storage` | The store refused or failed underneath the evaluator |
| `TRG-E011` | `BlobContract` | A connector's blob payload violated the contract (bad base64, dangling `"@N"` reference, or budget overrun); the poll was refused whole with the cursor unmoved |
| `TRG-E012` | `ConnectorCode` | The trigger names its connector as a GRAIN (#185) and this host will not run it: no `--allow-executor` pin, an unreadable Definition or code blob, a Definition carrying no `executor_uri`, a declared runtime with no `--sandbox-cmd`, or a blob-reading module on an evaluator wired no memory locator |

## Registry — PCK codes (agent packs)

Defined on `areev::pack::PackError` (`crates/areev-cli/src/pack.rs`), the
library half of `areev pack` (#315). Store and authorization errors pass
through unchanged — an `AUT-E001` raised while installing stays an
`AUT-E001`.

| Code | Variant | When |
|------|---------|------|
| `PCK-E001` | `Malformed` | The manifest, a grain file, or a shipped evalset is malformed — including an evalset case the `areev eval create` validator would refuse (#316) |
| `PCK-E002` | `ExpectationMismatch` | A grain's `expected_hash` differs from what the pack builds it to. Carries `{file, expected, built}`. Nothing is written: installing it would change what runs, and everything pointing at the old hash (every trigger above all) would now point somewhere else |
| `PCK-E003` | `UnresolvedRef` | A `blob:` or `grain:` reference names nothing the pack carries — a FORWARD reference included, which is why the manifest is an ordered list and not a set |
| `PCK-E004` | `AddressDrift` | The address a grain stored under differs from the address it was built to, so `validate` no longer describes `install` |

## Registry — CAL codes

The CAL codes are defined inline on `CalError` / `CalWarning` in
`areev-cal/src/errors.rs` (each `#[error(...)]` string opens with its code)
and are the source of truth. Ranges:

| Range | Area |
|-------|------|
| `CAL-E001`–`E019` | Lexing / parsing |
| `CAL-E020`–`E022` | Type & pipeline compatibility |
| `CAL-E030`–`E031` | Budget / timeout |
| `CAL-E032`–`E039` | ASSEMBLE, LET, COALESCE |
| `CAL-E040`–`E050` | Templates |
| `CAL-E051`–`E059` | Saved queries |
| `CAL-E060` | Field not available on grain type |
| `CAL-E061` | Engine-level field used where it cannot be honoured (under `NOT`/`OR`, or with an unsupported comparator) |
| `CAL-E070`–`E071` | Unsafe input / ASSEMBLE timeout |
| `CAL-E080`–`E085` | ACCUMULATE |
| `CAL-E090`–`E091` | Crypto during execution / hash not found |
| `CAL-E092` | Invalid query — store rejected input as invalid (not a budget overrun) |
| `CAL-E093` | Store error — the statement failed for a reason CAL has no more specific code for (a legal hold `STO-E009`, a read-only open `STO-E004`, a busy store, an internal failure). Carries the store's `DOMAIN-Ennn` detail, which is the code worth acting on — also as a value, `CalError::store_code()` (#331). Added in 1.9.1 (#321) as the honest name for what used to arrive as `CAL-E030 Budget exceeded`: nothing mapped from the store is a resource overrun, so a host routing `CAL-E030` as retryable was retrying legal holds |
| `CAL-E100` | Unsupported CAL version |
| `CAL-E110`–`E116` | Multi-format, user vars, scope, LLM-dependent options |
| `CAL-E117`–`E119` | Template limits and inheritance (OMS CAL §10.7–§10.8) |
| `CAL-E120` | Invalid JSON+CAL |
| `CAL-E121` | Not authorized — the session's grants don't cover this statement (carries the `AUT-Ennn` detail; the code itself is `CalError::store_code()`, #331) |
| `CAL-E122` | `PIN`ned `ASSEMBLE` sources do not fit the `BUDGET` — a pin is never summarised or dropped, so the statement fails instead of degrading it |
| `CAL-E123` | A `GROUP BY` names more fields than one composite key may have (max 4) |
| `CAL-W001`–`W012` | Warnings (unknown relation, deprecated operator, `{{#each}}` cap, bounded `CONTRADICTIONS` scan, …) |
| `CAL-W013` | `WITH auto_relate` accepted but not implemented — no relations are inferred |
| `CAL-W014` | A `WITH` option parsed and ran but cannot change the result on this statement (e.g. `score_breakdown` on `RECALL`) |
| `CAL-W015` | A post-retrieval stage (`ORDER BY`, a type-specific `WHERE` filter, `COUNT`) widened its scan to `max_limit` and still filled it — the answer is the top-k of a window, not of the memory |
| `CAL-W016` | A pipeline stage was attached to a payload it cannot act on (e.g. `ORDER BY` on a multi-source `ASSEMBLE`) and was skipped |
| `CAL-W017` | An `ASSEMBLE` token budget dropped grains its sources had already retrieved, naming the sources and the counts — including when the 4000-token default applied because no `BUDGET` clause was written |
| `CAL-W018` | A `GROUP BY` key names a field no grain in the result carries, so every row fell into one group under the empty key — the ranking has one group because the key is absent, not because one value dominates |

`CAL-E116` is the "needs an external LLM, not implemented" error for
`WITH hyde` / `WITH llm_rerank` — Areev takes no LLM dependency by policy.

## Adding or changing a code

1. **Codes are append-only.** Never renumber, reuse, or repurpose a code — it
   is a permanent debugging handle that may already be in a user's logs.
2. Adding an error variant: pick the next free number in the right domain,
   put `DOMAIN-Ennn: ` at the front of its `Display` string, add the arm to
   the type's `code()`, and add a row here.
3. New subsystem with no fitting domain → add a 3-letter domain to the table
   above first (keep it mnemonic).
4. Tests pin the standard: `areev-core`'s `error_code_tests`
   (code prefixes Display, `DOMAIN-Ennn` shape) and `areev-cal`'s
   `test_error_codes_match_display` / `test_all_error_codes_have_unique_codes`.
   Extend the representative-variant lists when you add a variant.
