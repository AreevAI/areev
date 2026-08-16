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
| `CAL-E070`–`E071` | Unsafe input / ASSEMBLE timeout |
| `CAL-E080`–`E085` | ACCUMULATE |
| `CAL-E090`–`E091` | Crypto during execution / hash not found |
| `CAL-E092` | Invalid query — store rejected input as invalid (not a budget overrun) |
| `CAL-E100` | Unsupported CAL version |
| `CAL-E110`–`E116` | Multi-format, user vars, scope, LLM-dependent options |
| `CAL-E117`–`E119` | Template limits and inheritance (OMS CAL §10.7–§10.8) |
| `CAL-E120` | Invalid JSON+CAL |
| `CAL-E121` | Not authorized — the session's grants don't cover this statement (carries the `AUT-Ennn` detail) |
| `CAL-W001`–`W012` | Warnings (unknown relation, deprecated operator, `{{#each}}` cap, bounded `CONTRADICTIONS` scan, …) |
| `CAL-W013` | `WITH auto_relate` accepted but not implemented — no relations are inferred |
| `CAL-W014` | A `WITH` option parsed and ran but cannot change the result on this statement (e.g. `score_breakdown` on `RECALL`) |

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
