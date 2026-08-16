# Erasure model — bulk deletion requirements

> **Status note (2026-08-10).** This document's original "OMS deviation"
> framing is retired: CAL 1.3 (spec draft `release/oms-v1.6`) brings both
> bulk operations into the language as authorization-gated Tier-2
> statements — `FORGET SUBJECT "<id>" [WITH text_mentions] BECAUSE "…"` and
> `PURGE OLDER THAN <n><d|h|m> [TYPE t] [IN "<ns>"] BECAUSE "…"` — gated by
> the session's `erase` grant and recorded as audit Observations. The
> host-level store/binding/CLI operations below remain, now as the same
> machinery the CAL statements call. The requirements are unchanged.

Areev's base removal model follows OMS: a single-grain tombstone (`FORGET
<hash>`) plus memory-level erasure (delete
the file / crypto-erase its key on the embedded backend, `DROP SCHEMA …
CASCADE` on the postgres backend). Regulated deployments — healthcare-class
privacy regimes with right-to-erasure and retention obligations (GDPR /
PDPL-family) — need two operations that sit BETWEEN those granularities.
This document is the requirement record for them.

## Requirements

- **REQ-ERASE-1 (right to erasure).** A host MUST be able to erase every
  grain holding a structured reference to one identity — the full
  supersession history, not just the live heads — including the identity's
  dictionary entry (the identifier string is itself erasable data), the
  erased grains' own value strings once unreferenced, vocabulary tokens
  that occurred only in the erased text, telemetry rows keyed on the
  identity string, and CAS attachments only those grains referenced.
  Result: `Areev::forget_subject(ns, subject)`, exposed as
  `forget_subject` / `forgetSubject` in the bindings and
  `areev forget-subject … --yes` in the CLI.
- **REQ-ERASE-2 (retention).** A host MUST be able to erase every grain
  older than a cutoff (`created_at`), optionally scoped to a namespace and
  grain type, suitable for a nightly sweep. Result:
  `Areev::forget_older_than(ns, cutoff_ms, grain_type)`, exposed as
  `forget_older_than` / `forgetOlderThan` and `areev purge-older-than <days>
  … --yes`.
- **REQ-ERASE-3 (replication).** Bulk erasure MUST reach replicas. Each
  erased grain gets its own ordinary op-log tombstone, so a bulk erasure
  replays over the existing bundle/sync machinery exactly like individual
  forgets — no new wire format, idempotent on replay.
- **REQ-ERASE-4 (atomicity under concurrency).** Enumeration and deletion
  run in one transaction after the multi-writer serialization point, so a
  concurrently-committed grain cannot slip between the census and the
  deletes, and a failure erases nothing.
- **REQ-ERASE-5 (auditability without re-identification).** The operation
  returns an `ErasureReport` (counts only: grains, dictionary entries,
  vocabulary tokens, blobs). It deliberately contains no identity material;
  the HOST decides what to log. The engine writes no audit grain of its own
  — an engine-written record naming the subject would re-introduce the
  reference being erased.

  **The same rule binds the hosts.** CAL and the CLI both audit (they are
  the surfaces a human or agent invokes), and their record names a
  **fingerprint** — `sha256(identity)[..8]` hex, scheme declared in the
  record's `subject_ref` — never the identifier. An audit grain is
  immutable, replicates, and lands in archives, so a raw identifier there
  survives the erasure it records, un-erasable by the subject selector
  (which never matches `subject:<id> ns:<ns>` as a partition key). The
  fingerprint keeps the property that matters — given a candidate identity,
  recompute and **verify** that a record concerns that person — without
  making the log enumerable. Human-readable references belong in BECAUSE,
  which names a *request*, not a data subject. Hash-form `FORGET` records a
  content address and `PURGE` records an age; neither is fingerprinted.
  One builder (`areev_core::authz::audit_observation`) serves every
  surface, so the shapes cannot drift.
- **REQ-ERASE-6 (surface discipline).** *(Amended by CAL 1.3 — see the
  status note above; the original "not reachable from CAL text" wording
  described the pre-1.3 grammar.)* Destruction takes a hash, an identity,
  or an age — **never a predicate**. In CAL both bulk operations are
  authorization-gated Tier-2 statements (`erase` grant required, BECAUSE
  mandatory, one audit Observation per execution); `FORGET USER`/`FORGET
  SCOPE` stay text-refused, `DELETE`/`ERASE`/`TRUNCATE` stay lexer-blocked
  non-tokens, and `cal_forget_scope` remains an unwired stub. On the host
  surfaces the CLI demands an explicit `--yes`, and
  `CalExecutorConfig::allow_destructive_ops` (`--no-destructive-ops`) stays
  a process-wide restrictive cap over any grant. An empty subject is
  refused outright — with prefix matching it would select everything, and
  an unset variable must never read as "erase all".
- **REQ-ERASE-7 (partition keys).** Hosts model composite records as
  identity-prefixed keys (`pat#visit1`, `pat:thread-2`). Identity matching
  MUST cover them: any dictionary term equal to the identity or starting
  with it followed by a non-alphanumeric separator, case-exactly and
  identically on every backend — and never a longer word (`patricia` is
  not `pat`). Applies to subject/object positions, sessions, and run ids
  alike, and the matched key strings are tombstoned with the identity.
- **REQ-ERASE-8 (search symmetry, opt-in).** Whatever the engine can FIND
  by the identity's tokens, it can ERASE by them: with `text_mentions`
  enabled, grains whose indexed text contains every token of the identity
  join the erasure set (the BM25 inverted index is the mechanism, so this
  needs text indexing on and fully built — requesting it with indexing off
  or a deferred/unrebuilt index is an error, not a silent partial erasure). Opt-in, never default: token matching
  over-reaches for identifiers that are ordinary words ("may", "mark"),
  and erasing every grain containing a common word is the wrong failure
  mode. For distinctive identifiers (contact codes, record numbers) it
  closes the prose-mention gap.
- **REQ-ERASE-9 (access is the same selection as erasure).** A host MUST be
  able to SHOW everything the identity selector matches without erasing it
  — GDPR Art. 15 (access) and Art. 20 (portability) are answered by the
  erasure machinery in read-only mode, over **one** selector: the report
  and the erasure cannot diverge, because divergence would mean disclosing
  one set and deleting another. Result: `Areev::subject_report(ns,
  subject)` and `subject_bundle_with` (a portable MGB1 export), exposed as
  CAL `REPORT SUBJECT "<id>" [WITH text_mentions]` (a **read** —
  `read`-gated, not behind the destructive cap), `areev subject-report`
  (`--out` JSONL, `--bundle` portable), MCP `areev_subject_report`, and
  `subject_report`/`subject_bundle` in both bindings. The report writes no
  audit grain: the audit obligation is on destruction, not access, and a
  read that recorded the identity would re-introduce the reference
  REQ-ERASE-5 keeps out. Preconditions are shared with erasure — an empty
  subject is refused, and `text_mentions` without a built index is a hard
  error, never a silent partial answer (a partial DSAR response is a
  compliance failure, not a degraded read).

## The OMS deviation, stated plainly

OMS models removal as per-grain tombstones and treats grain immutability +
file-level erasure as the privacy story. **Subject-scoped and age-scoped
bulk erasure are Areev extensions that go beyond the spec**, added because
"can't delete" is a compliance problem, not a feature, for personal data.
The deviation is deliberately shaped to be conformance-neutral:

- Wire format, content addressing, and grain immutability are untouched —
  erasure is a batch of ordinary tombstones plus index/dictionary hygiene.
- The CAL surface is byte-identical to before (REQ-ERASE-6), so CAL-level
  OMS conformance claims are unaffected.
- A peer implementing plain OMS interoperates: it sees ordinary `OP_FORGET`
  records and converges (it will simply keep its own dictionary entries —
  dictionary hygiene is local).

## Scope contract (what "about a subject" means)

`forget_subject` erases grains found through **dictionary-indexed
references** in the namespace — for the identity itself AND every
partition-style key carrying it as a boundary-guarded prefix
(REQ-ERASE-7):

- triple **subject** position (the grain is about the identity),
- triple **object** position (the grain points at the identity —
  over-deletion is the safe direction for erasure),
- **thread events** whose session id is the identity (or an
  identity-prefixed key),
- **run records** whose run id is the identity (`run_trace` /
  `runs_touching` must go empty for an erased identity),
- and, with `text_mentions` enabled (REQ-ERASE-8), grains whose **indexed
  text** contains every token of the identity — search symmetry through
  the engine's own inverted index.

Dictionary hygiene rides the same transaction: every term the erased
grains touched (their subject/relation/object VALUES, session and run
ids) is **tombstoned** — the string replaced with an unrecallable
placeholder — once nothing references it. Tombstoning rather than
deleting is deliberate: under concurrent writers another instance may
hold the id in its cache, and a dangling id would silently corrupt its
next write, while a tombstoned id stays referentially valid and merely
makes the old name unfindable. `fts_vocab` tokens left with no postings
are removed, telemetry rows keyed on any matched identity string —
partition keys included — (query rollups, the recall ring log) are
scrubbed, and CAS attachments are reclaimed
**targeted** — only the erased grains' own references, checked for
surviving users under the write serialization, never a store-wide gc
that could race another writer's in-flight upload. (Residual window: a
concurrent upload of byte-identical content in the erasure instant; and
`gc_blobs`, the explicit full-store sweep, still requires quiescent
writers — both documented on the APIs.)

Residual limits, stated honestly: text-mention matching reaches exactly
what the index reaches — grains whose text was never indexed (`index_text`
off, or written before indexing) and identity forms the tokenizer splits
differently (a phone number renders as its digit runs) are matched only
through their structured references. `user_id` inside a grain body is not
an index; use subject/session/run for erasable identity scoping.
**Distinctive identifiers in structured fields remain the contract**;
partition keys and text mentions are the widening, not a substitute.

## Backend notes

- **Postgres**: rows are gone at commit; physical space follows autovacuum.
  Telemetry tables ride the same schema and are scrubbed per hash. Memory-
  level erasure is `drop_postgres_schema`.
- **Embedded (file)**: the same logical guarantees, with the standing
  physical-remnant caveat (`docs/security-model.md`): deleted pages can
  linger in the file/WAL until compaction, so **crypto-erasure remains the
  strong path** for whole-memory destruction; `forget_subject` is the
  surgical tool within a memory that keeps living.
- Erase-then-rewrite of the SAME identity from a different, still-open
  handle is outside the contract on the multi-writer backend (its cached
  dictionary id may be stale); route post-erasure writes through a fresh
  handle or the erasing instance.

## Testing

Conformance cases (both backends, one list): `subject_erasure_is_complete`
(history + object references + thread events + dictionary + vocabulary,
survivor untouched, text unfindable afterwards), `subject_erasure_replicates`
(tombstones reach a peer, idempotent replay), `retention_erases_only_older`
(exclusive cutoff, type scoping). Binding smokes drive both operations over
live Postgres.
