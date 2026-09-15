# Grain Attestation — Implementation Plan

**Status: Phases 0–5 BUILT (2026-09-15) on `feat/grain-attestation`; Phase 6
(measurement) pending. The decision for
[#77](https://github.com/AreevAI/areev/issues/77): option 1 designed in full,
then built as the phases below. This document is the "how"; the
`ARCHITECTURE.md` §10 entry "Authenticity is an attestation grain, not an
envelope" is the "why".**

> **Implementation record.** As built, three details differ from the draft
> below and the draft is left as written for the reasoning: (1) keys are
> installed on the open handle with `set_signing_key` / `set_trusted_authors`
> / `set_attest_policy` (the `set_embedder` pattern) rather than through
> `AreevOptions`, which keeps every open signature and every binding
> constructor untouched; (2) `verify_attestations` returns its own
> `AttestReport` instead of extending `VerifyReport`, so `verify` is
> byte-identical rather than merely compatible; (3) the trusted-authors
> document's policy defaults to `verify`, not `off` — installing one says the
> host wants attestations checked, and `off` remains the state without one.
> The former COSE scaffold is retired (§9), the reserved namespace guard is
> in `prep_from_blob`, and the two-pass import is `parse_bundle_records` +
> `check_bundle_attestations` in `crates/areev-store/src/lib.rs`.

> **What changed from the dormant scaffolding.** `areev-core` carries a
> COSE Sign1 sketch (`serialize_grain_signed`, `unwrap_if_cose`, an empty
> `signing` feature) that sets a header bit, re-hashes, and wraps the blob
> in a CBOR envelope. That shape changes every signed grain's content
> address, needs a CBOR dependency in the root crate, and needs a place to
> store the envelope. This plan replaces it with a **detached attestation
> stored as an ordinary grain**: nothing about the `.mg` format, the content
> address, the bundle format, or the store schema moves. The scaffold is
> retired, not finished.

## 0. The decision in one paragraph

A grain's authenticity is recorded by a second, immutable grain: an
Observation in the reserved namespace `agent:attest` that carries the
attested content hash, the signing key's id, the algorithm, and an Ed25519
signature over that hash, linked to its subject with a `related_to` edge.
The signing key and the list of trusted public keys are host configuration
and never enter the file. Verification runs only where bytes cross a trust
boundary — bundle import and `follow` — and only when the host asks for it.
Content hashes, canonical serialization, the bundle format, and the Postgres
schema stamp are all unchanged, so every existing file, bundle, replica,
binding call, and downstream deployment keeps working without a migration.

## 0.1 How this answers #77

The issue names three blockers and three acceptance criteria. Each maps to
a section here.

| #77 said | This plan |
|---|---|
| **No trust model** — nothing to verify against | §3: host-held Ed25519 author keys, a host-side trusted-authors map, rotation by adding keys and revocation by removing them, verification only at import and `follow` |
| **The spec isn't finalized** — OMS §9 has not settled envelope semantics, and canonical serialization is frozen (invariant 2) | §2 and §14: the signature lives in a separate grain, so the `.mg` bytes and the content address of the attested grain never change; §9 can land later without invalidating anything written now |
| **New dependency in the root crate** (invariant 6) | §10: one pure-Rust crate in `areev-store`, nothing in `areev-core`, no CBOR |
| Latency gates | §5: verification never runs on the recall path; signing is opt-in; §11 Phase 6 measures it |
| *Acceptance:* a recorded decision | §0 is the `ARCHITECTURE.md` §10 entry text |
| *Acceptance:* a plan a future PR can execute | §11, six phases with files and tests |
| *Acceptance:* the "don't try to build it" gotcha updated | §9 and Phase 1 |

Of the three options the issue lists, this is **option 1 with the design
fixed in enough detail that option 2 becomes a sequence of ordinary PRs**.
Option 3 (strengthen `verify` and `audit export` instead) is not chosen,
but its one concrete gap — import trusting the bundle's stated hash — is
Phase 0 here, because a signature over a hash is only as strong as the hash
check under it.

## 1. Terminology

| Word | Means | Does not mean |
|---|---|---|
| **attestation** | the `agent:attest` grain that says "key K signed hash H" | a COSE envelope around the blob; a "signed bundle" — bundles are containers, grains are what gets attested |
| **author key** | an Ed25519 keypair a host holds; its `key_id` is the first 16 hex chars of SHA-256(public key) | a user identity; a credential from `areev auth` |
| **trusted authors** | a host-side map `key_id → public key` | anything stored in a memory |
| **policy** | `off` / `verify` / `require`, per open | a per-grain flag |

## 2. The attestation grain

Built exactly like the corpus-export record in
`crates/areev-store/src/lib.rs` (`record_corpus_export`): an Observation in
a reserved namespace, a JSON `context`, `related_to` edges to the subject.

```text
Observation
  observer_id      = <key_id>
  observer_type    = "attestation"
  subject          = "sha256:<hash hex>"          # the attested grain
  created_at       = <the attested grain's created_at>   # see determinism
  common.namespace = "agent:attest"
  common.context   = {
      "attestation": true,
      "alg":         "ed25519",
      "key_id":      "<16 hex>",
      "attests":     "sha256:<hash hex>",
      "sig":         "<64 bytes, hex>",
      "domain":      "areev-attest-v1"
  }
  common.related_to = [ { hash: <hash>, relation_type: "mg:attests" } ]
```

**Signed message.** `b"areev-attest-v1\0" || hash_bytes` (32-byte raw
hash). Domain-separated so a signature can never be mistaken for another
protocol's. Nothing else is bound in on purpose: an attestation that moves
to another memory still states a true fact, "K signed H", so replay is not
an attack.

**Determinism.** Ed25519 is deterministic and `created_at` is pinned to the
subject's, so the same key attesting the same hash always mints the same
attestation grain. Re-attesting is a no-op (invariant 1: adding an existing
grain returns the existing hash), and two replicas that both attest converge
on one grain instead of two.

**Erasure.** The grain names a hash, never a subject. It mirrors the audit
rule in `docs/erasure.md`: an attestation that survives its subject's
tombstone reveals nothing. `FORGET <hash>` on the subject leaves the
attestation as an orphan that `verify` reports as `attest_orphaned`; `PURGE`
sweeps attestations like any other grain by age. No cascade is added.

**Reserved namespace.** Add `ATTEST_NS = "agent:attest"` to
`crates/areev-core/src/authz.rs` beside `AUTHZ_NS` and `HARNESS_NS`, and
extend the reserved-prefix matcher in `crates/areev-core/src/ns.rs` and its
tests. User writes to `agent:attest` are refused like the other two; the
store writes it through the same internal path `record_corpus_export` uses.

## 3. Trust model

- **Who signs:** the host process that holds the key. On a laptop that is
  the CLI; in a deployment it is the worker that opens the memory. A key
  proves *which host* wrote a grain, not which human or which model.
- **Key material:** a 32-byte Ed25519 seed, supplied by the host at open,
  wrapped in `zeroize::Zeroizing` (already a store dependency), never
  persisted. Sources, in order: `AreevOptions::signing_key`, CLI
  `--signing-key-env VAR` (hex seed in the named variable, matching the
  `--token-env` convention), bindings `set_signing_key(hex)`.
- **Trusted authors:** a JSON file, host-side, the `loop-policy.json`
  pattern:
  ```json
  { "version": 1,
    "keys": { "3f9a…": "<32-byte public key, hex>", "…": "…" },
    "policy": "verify" }
  ```
  Supplied via `AreevOptions::trusted_authors`, CLI `--trusted-authors FILE`,
  bindings `set_trusted_authors(json)`. The file's `policy` is the default;
  `--require-attested` raises it to `require` for one invocation.
- **Rotation:** add the new key, keep the old public key in `keys` for as
  long as grains it signed matter. Revocation is removal from `keys`;
  grains it attested then verify as `unknown_key`. There is no revocation
  grain, because an authorization never replicates (§10, "A declaration
  replicates; an authorization never does").
- **What it does not defend against:** whoever holds the host's key can
  sign anything. An attestation proves the writing host, not a person and
  not a model, and it gives no protection against the operator of that
  host. `security-model.md` must say this in the same breath as the
  feature.

## 4. Signing on write

When a signing key is configured, every successful `add`, `supersede`, and
run-journal write in a non-reserved namespace is followed by its
attestation in the **same** write path, before the call returns. Reserved
namespaces (`agent:authz`, `agent:harness`, `agent:attest`) are not
attested; attestations of attestations are refused.

Cost: one Ed25519 sign (~25 µs) plus one extra grain insert per write, so
the op-log and bundles roughly double in row count. This is why it is
opt-in. Phase 6 records the measured numbers in
`crates/areev-bench/RESULTS.md`; the existing latency gates run with
signing off and are not touched.

`areev attest <hash>` and `areev attest --all [--ns PREFIX]` retro-attest
existing grains (idempotent, see determinism). Store methods:
`attest(&Hash) -> Result<Hash>` and `attest_all(ns: Option<&str>) ->
Result<AttestStats>`.

## 5. Verification on import and follow

`import_bundle` (which `follow` also uses) becomes two-pass when policy is
not `off`:

1. **Parse** the whole bundle into records without inserting anything.
   Recompute each record's content address from its blob and refuse the
   bundle if it differs from the stated hash (Phase 0, needed regardless).
2. **Collect** the attestation records, verify each against `trusted
   authors`, and classify every non-reserved grain as `attested`,
   `attest_invalid` (a trusted key's signature does not verify: tampering),
   `unknown_key`, or `unattested`.
3. **Decide** before the first insert:
   - `verify`: any `attest_invalid` refuses the whole bundle (`CRY-E002`);
     everything else is applied and counted.
   - `require`: additionally any `unknown_key` or `unattested` grain refuses
     the whole bundle (`CRY-E003`).
   - Nothing is written when a bundle is refused: `ImportStats.applied == 0`
     is test-pinned.
4. **Apply** as today.

`ImportStats` gains `attested`, `attest_invalid`, `unknown_key`,
`unattested`, all zero when policy is `off`. The recall path is untouched:
verification never runs on read, so the 200 µs recall gate and the 50 ms
voice-loop gate are unaffected by construction.

Under `off` (the default) unsigned and signed bundles import exactly as
they do today, which is what keeps archived `.mgb` files restorable forever.

## 6. The `verify` report

`areev verify --attestations` (and `verify_attestations()` in the store)
walks every attestation, checks it against `trusted authors`, and extends
`VerifyReport` with `attested`, `attest_invalid`, `attest_unknown_key`,
`attest_orphaned`, `unattested`. The existing keys keep their meaning; when
policy is `require`, `attest_invalid > 0` also flips `integrity` away from
`"ok"`, so consumers that read only the old fields still see the failure.
Without the flag the report is byte-for-byte what it is today. It is
read-only work: it runs on a `--read-only` handle and a SELECT-only
Postgres role.

## 7. Surfaces

| Surface | Change | Rule |
|---|---|---|
| `areev-store` | `AreevOptions { signing_key, trusted_authors }`, `attest`, `attest_all`, `verify_attestations`, two-pass import | host config never persisted (invariant 5) |
| CLI | global `--signing-key-env VAR`, `--trusted-authors FILE`, `--require-attested`; verbs `attest <hash>`, `attest --all`; `verify --attestations` | `USAGE` + `docs/cookbook.md` |
| Python + Node | `set_signing_key(hex)`, `set_trusted_authors(json)`, `attest(hash)`, `attest_all(ns)`, `verify_attestations()`; JSON out | **new methods, never new positional parameters** — the constructor and the run verbs are positional and callers re-declare them, so an inserted argument shifts silently; regenerate `areev-js/index.d.ts` |
| MCP | none in this pass | keeps the pinned tool count and the 12-tool `memory` profile stable; add `areev_attest` later if a host asks |
| CAL | none | attestations are ordinary grains, reachable through the existing `related` reads; no syntax, so no OMS spec decision |
| Console | `verify` card shows the new counters when present | re-shoot screenshots only if the card is in one |

## 8. Error codes (append-only)

| Code | Variant | When |
|---|---|---|
| `CRY-E002` | `AttestationInvalid` | a trusted key's signature does not verify over the stated hash — the bundle is refused |
| `CRY-E003` | `AttestationRequired` | policy `require` and a grain has no valid attestation from a trusted key |
| `CRY-E004` | `SigningKeyInvalid` | the supplied seed or public key is not 32 bytes / not valid hex |

Text lives on `AreevError` in `areev-core/src/error.rs`; rows go to
`ERROR_CODES.md`; the uniqueness test covers them.

## 9. Retiring the scaffold

- Delete `serialize_grain_signed` (`serialize.rs`) and the `signing`
  feature from `crates/areev-core/Cargo.toml` **and** from
  `crates/areev-cal/Cargo.toml` (its DESCRIBE feature list names it too).
- Keep `unwrap_if_cose`'s refusal of a leading `0x84` byte, now
  unconditional: a COSE-wrapped blob is not a `.mg` blob and fails closed.
- Keep header bit 0 (`is_signed`) reserved and unused. It is OMS-reserved;
  do not repurpose it.
- Replace the "Don't try to build it" gotcha in `crates/areev-core/CLAUDE.md`
  with a pointer to the §10 entry and this plan.

None of this is a breaking change for crates.io users: the feature never
compiled, so no downstream code can reference the removed items.

## 10. Dependencies

`ed25519-dalek = "2"` in `areev-store` only (BSD-3-Clause, MSRV 1.81 against
the workspace's 1.90, no `rand` feature since keys are supplied). It pulls
the `sha2 0.10` / `digest 0.10` line, which is already in `Cargo.lock` as a
tolerated duplicate under `deny.toml`'s `multiple-versions = "warn"`. No
`coset`, no CBOR: the signature is bytes in a JSON context. `ring` was
considered because it is already in the `areev` binary's graph via `ureq`,
but it is not in `areev-store`'s default build and is a heavier compile for
the per-platform Node addons. `areev-core` gains no dependency.

## 11. Phases

Each phase is one PR, green on its own, in this order.

**Phase 0 — import checks its hashes.** `import_bundle_until` recomputes
each record's content address and refuses a mismatch (an existing `FMT` error, no
new error code needed). Test: a bundle with one relabelled record imports nothing.
Conformance case on both backends. Independent of everything below and
worth shipping first.

**Phase 1 — decision and docs.** `ARCHITECTURE.md` §10 entry, this plan,
`crates/areev-core/CLAUDE.md` gotcha, `docs/security-model.md` roadmap line
pointing here. Closes the acceptance criteria of #77.

**Phase 2 — the grain and the key.** `ATTEST_NS`, reserved-ns matcher,
`AreevOptions::signing_key`, `attest`, `attest_all`, attestation on the
write path. Tests: same key + hash → same attestation hash; reserved
namespaces never attested; `attest_all` idempotent; key zeroized on drop.

**Phase 3 — verification on import.** `trusted_authors`, policy, two-pass
import, `ImportStats` counters, `CRY-E002/3/4`. Tests: tampered attestation
refuses the bundle with `applied == 0`; `require` refuses an unsigned
bundle; `off` imports both unchanged; unknown key counted not refused under
`verify`. Conformance case `cases/attestation.rs` on both backends,
including a `follow` round trip through a directory.

**Phase 4 — `verify` and the CLI.** `verify_attestations`, the global
flags and verbs, `USAGE`, `cookbook.md`. CLI smoke test drives the real
binary; `--read-only` + `verify --attestations` on Postgres with the
SELECT-only role from `docs/deployment-profile.md`.

**Phase 5 — bindings.** Python and Node in lockstep, `index.d.ts`
regenerated, smoke tests in both. Methods only.

**Phase 6 — measure and document.** `bench` and `voice_loop` examples run
with signing on; numbers into `RESULTS.md` as a labelled row, not a gate.
`security-model.md` "Integrity, not authenticity" rewritten to state what
is now verified and what still is not (§3, last bullet). `docs/gdpr.md`
gains one line on attestations and erasure.

## 12. Docs contract sweep (per `CLAUDE.md`)

| Changed | Update |
|---|---|
| CLI verbs and flags | `USAGE`, `docs/cookbook.md` |
| Error codes | `ERROR_CODES.md` |
| Store import / verify semantics | `crates/areev-store/CLAUDE.md`, conformance case both backends |
| Crypto and keys | `docs/security-model.md` |
| Bindings | both bindings + `areev-js/index.d.ts` |
| Architecture decision | `ARCHITECTURE.md` §10 (Phase 1) |
| Release | `CHANGELOG.md`, and say explicitly: **`PG_SCHEMA_VERSION` and `TELEM_SCHEMA_VERSION` do not move** |

## 13. Compatibility guarantees

These are the promises the build must keep, each pinned by a test:

1. Content addresses of existing grains are unchanged. Pinned already by
   the byte-exact Vector 1 test in `areev-core` (address `3288d0d4…`) and by
   `examples/agents/run-smokes.sh`, which asserts the three language stacks
   mint one plan hash; both must stay green untouched.
2. `.mg` wire format unchanged; no new grain type; no registry row.
3. Bundles stay MGB1/MGB2; attestations are records like any other. Older
   builds import them as plain Observations.
4. No new table; `PG_SCHEMA_VERSION` stays `"1"`; no `provision` needed;
   read-only roles need no re-grant.
5. `verify()` without the flag is byte-identical to today.
6. Default policy `off`: an unsigned bundle imports as it always has.
7. Binding constructors and `runStart` / `runResume` / `triggerRun` keep
   their positional shapes; everything new is a method.
8. Error codes appended, never renumbered.

Anything built on Areev — a hosted deployment, a marketplace, an
adapter — inherits these eight guarantees without a coordinated change: the
feature is opt-in, adds no tables, moves no schema stamp, and changes no
format. The release notes state that explicitly (§12).

## 14. Relationship to OMS §9

OMS §9 names COSE signing as the high-assurance path and is not final.
This plan does not implement §9 and does not prevent it: the header bit and
the `0x84` refusal stay reserved, and an attestation grain can later carry a
COSE Sign1 envelope in a `cose` context field as a second encoding of the
same signature. If §9 settles on envelope-in-place with a changed content
address, that is a format decision for the spec, and Areev would ship it as
a new grain version behind `min_reader_version`, with attestations as the
bridge for grains written before it. Record that here when it happens; do
not pre-build it.

## 15. Non-goals

- Proving who a human or model author is. That is `areev auth` and the
  approval ladder, not this.
- Defending a memory against the operator of the host that holds the key.
- Signing packs. Pack provenance is content addressing plus the host's
  executor pin (`docs/pack.md`); attesting a pack's grains at `pack
  install` is a natural follow-up, not part of this pass.
- An MCP tool, CAL syntax, or console page in this pass.

## 16. Exit criteria

- [ ] Phase 0 merged and in the changelog.
- [ ] Phase 1 merged; #77 closed with a link to the §10 entry.
- [ ] Phases 2 to 5 merged with every test named above; conformance green on
      both backends in CI.
- [ ] Phase 6 numbers in `RESULTS.md`; `security-model.md` no longer says
      "signature verification is not yet enforced on import".
- [ ] Release notes state that no schema stamp moved and no bundle format
      changed.
