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

The MCP server, HTTP console, CLI, and Python binding do not mint their own
codes — they surface the underlying `AreevError` / `CalError` (and thus its
code) through their own envelopes (MCP `isError` result, HTTP body, stderr,
`PyValueError`). The `areev-loop` engine crate is the exception: it has zero
areev dependencies, so it owns the `LOP` domain. REVIEW/APPLY *syntax*
errors stay in the substrate's `CAL` domain; `LOP` covers engine semantics
(lifecycle, gates, analyzers).

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
| `CRY-E001` | `CryptoError` | Key / cipher / signing / erasure failure |
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
| `RUN-E004` | `UnresolvedRef` | A binding does not resolve to a usable Tool definition |
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
| `CAL-E100` | Unsupported CAL version |
| `CAL-E110`–`E116` | Multi-format, user vars, scope, LLM-dependent options |
| `CAL-E117`–`E119` | Template limits and inheritance (OMS CAL §10.7–§10.8) |
| `CAL-E120` | Invalid JSON+CAL |
| `CAL-E121` | Not authorized — the session's grants don't cover this statement (carries the `AUT-Ennn` detail) |
| `CAL-E122` | `PIN`ned `ASSEMBLE` sources do not fit the `BUDGET` — a pin is never summarised or dropped, so the statement fails instead of degrading it |
| `CAL-W001`–`W012` | Warnings (unknown relation, deprecated operator, `{{#each}}` cap, bounded `CONTRADICTIONS` scan, …) |
| `CAL-W013` | `WITH auto_relate` accepted but not implemented — no relations are inferred |
| `CAL-W014` | A `WITH` option parsed and ran but cannot change the result on this statement (e.g. `score_breakdown` on `RECALL`) |
| `CAL-W015` | A post-retrieval stage (`ORDER BY`, a type-specific `WHERE` filter, `COUNT`) widened its scan to `max_limit` and still filled it — the answer is the top-k of a window, not of the memory |
| `CAL-W016` | A pipeline stage was attached to a payload it cannot act on (e.g. `ORDER BY` on a multi-source `ASSEMBLE`) and was skipped |

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
