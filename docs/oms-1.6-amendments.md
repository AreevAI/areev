# OMS 1.6 amendments — the compliance batch

> Companion: [`oms-1.6-amendments-triggers.md`](oms-1.6-amendments-triggers.md)
> covers the trigger batch (new grain type, the §27.6 removal, the execution
> contract).

**Status:** amendment proposal, written 2026-08-11. One revision covering
every spec-level change the GDPR compliance work needs, so conformance moves
once rather than four times.

CAL syntax and canonical serialization are OMS conformance contracts:
Areev's own invariants say no new CAL syntax and no serialization change
without a spec-level decision. This document is that decision, written down.

| # | Amendment | Shipped in Areev? | Conformance impact |
|---|---|---|---|
| A1 | `REPORT SUBJECT` statement | **Yes** (1.0.0) | New read statement |
| A2 | `retention:<ns>` meta keys | **Yes** (1.0.0) | New reserved meta prefix |
| A3 | Tier-2 audit records name a subject *fingerprint* | **Yes** (1.0.0) | Changes an existing record's field value |
| A4 | `processing_basis` as a common field | No — [proposal](consent-cascade-proposal.md) | Serialization surface |
| A5 | Consent grain field reconciliation | No — proposal | Grain schema |
| A6 | `FORGET BASIS` / `REPORT BASIS` | No — proposal | New statements |

A1–A3 shipped ahead of ratification because they close live compliance gaps;
they are written here as the record of what an implementation must do to
interoperate. A4–A6 are gated on this document being accepted.

---

## A1 — `REPORT SUBJECT`: the DSAR read

```
REPORT SUBJECT "<id>" [WITH text_mentions]
```

**Semantics.** Returns every grain the identity selector matches for `<id>`
in the session namespace — the exact identifier, its partition-style keys
(`<id>` followed by a non-alphanumeric separator: `pat#visit1`, never
`patricia`), and the full supersession history — without modifying anything.
`WITH text_mentions` extends the selection to grains whose indexed text
contains every token of the identity.

**Classification: read.** Not destructive, not evolve. Consequences an
implementation MUST honor:

- authorized by the `read` verb, **not** `erase`;
- available wherever reads are (including a token-less read-only surface);
- **not** subject to a destructive-operations cap;
- writes **no** audit grain. Access is not destruction, and a record naming
  the identity would re-introduce the reference an erasure removes.

**The symmetry requirement (the point of the amendment).** `REPORT SUBJECT`
and `FORGET SUBJECT` MUST resolve the *same* selection. An implementation
that computes them independently is non-conformant even if both are
individually correct: what a subject-access request discloses and what an
erasure removes cannot be allowed to differ.

**Result shape.** `identity_names` (every matched identifier string) and
`grains` (`{hash, type, fields}`).

**Rationale.** Art. 15 access and Art. 20 portability were answerable only by
hand-written queries, while the engine already *computed* the exact selection
for erasure and exposed it write-only.

## A2 — `retention:<ns>` meta keys

Reserved `meta` key prefix, one row per namespace, value a JSON object:

```json
{"days": 90, "grain_type": "event", "because": "support tickets age out"}
```

`days` is a non-negative finite number (0 = no minimum age). `grain_type` and
`because` are optional.

**File-truth, not host config.** The policy travels with the memory, like
saved queries (`qry:`) and templates (`tpl:`) — a copy, a sync, or a restore
on another host still carries "events here live 90 days". A host that cannot
parse a declared policy MUST fail loudly rather than treat it as absent:
silently reading an unparseable policy as "no policy" means "keep forever" on
a file whose owner asked for deletion.

**Declaring is not enforcing.** Writing the row MUST NOT delete anything.
Enforcement is a separate, authorized, audited act.

**Rationale.** Art. 5(1)(e) storage limitation was a manual sweep with no
declared policy; a retention rule that lives only in someone's crontab is not
a property of the memory.

## A3 — Audit records name a subject fingerprint

A Tier-2 audit record for an identity erasure MUST record

```
subject:<fingerprint> ns:<namespace>
```

where `<fingerprint>` is the first 8 bytes of SHA-256 over the identity
string, lowercase hex. The record's context carries `subject_ref` naming the
scheme (`"sha256-64/hex"`) so a verifier can recompute it without reading an
implementation.

**This is a correction, not an addition.** Recording the raw identifier
leaves the erased subject permanently recallable from the audit namespace,
un-erasable by the subject selector (which does not match
`subject:<id> ns:<ns>` as a partition key), and copied into every bundle and
archive written afterwards — an Art. 17 failure inside the Art. 30 record.

The property the fingerprint preserves: given a candidate identity, anyone
can recompute the digest and **verify** that a record concerns that person
(answering "prove you erased me"); the log cannot be mined to enumerate who
was erased. Human-readable request references belong in `BECAUSE`, which
names a request, not a data subject.

Hash-form `FORGET` records the content address (already-deleted content, not
identity material) and age-based `PURGE` records an age and namespace —
neither is fingerprinted.

## A4 — `processing_basis` as a common field

Add `processing_basis` (optional string, compact key `pbasis`, **omit-default**)
to the common field set: a reference to the Consent grain that authorized the
grain's processing, as a content address.

**Compatibility requirement.** Because it is omit-default, a grain that does
not set it MUST serialize byte-identically to before this amendment — every
existing content address is unchanged. An implementation MUST prove this by
test rather than assert it.

Deferred to the [consent-cascade proposal](consent-cascade-proposal.md); the
key is already reserved in the compact-key table, so no key allocation is
needed.

## A5 — Consent grain field reconciliation

The Consent grain (`0x0A`) advertises queryable fields that its schema does
not define (`consent_action`, `purpose`, `grantor_did`, `expires_at`,
`granted`). OMS 1.6 MUST decide, per field, whether it joins the schema or
leaves the advertised set. `prior_consent` and `witness_dids` are specified
and MUST be preserved on write (an implementation that accepts and drops them
is non-conformant — and `prior_consent` is what a withdrawal needs to name
what it withdraws).

## A6 — `FORGET BASIS` / `REPORT BASIS`

```
FORGET BASIS "<consent-hash>" BECAUSE "<why>"
REPORT BASIS "<consent-hash>"
```

Destructive and read respectively, mirroring the SUBJECT pair: erase (or
show) every grain whose `processing_basis` names the given consent. Requires
A4. The destructive form takes `erase`; the read form takes `read`. The audit
record names the consent's content address — an authorization, not identity
material, so A3's fingerprint rule does not apply.

Deferred to the [consent-cascade proposal](consent-cascade-proposal.md).

---

## Conformance summary

An implementation claiming OMS 1.6 compliance-profile conformance MUST:

1. implement `REPORT SUBJECT` as a read, over the same selector as
   `FORGET SUBJECT` (A1);
2. honor `retention:<ns>` as a file-carried declaration, failing loudly on an
   unparseable one, and never delete on declaration (A2);
3. fingerprint subject identities in destruction audit records, and name the
   scheme in the record (A3).

A4–A6 join the profile if this document is accepted; until then they are
proposals and an implementation is conformant without them.
