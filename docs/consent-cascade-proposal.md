# Consent-cascade erasure — proposal

**Status:** proposal, written 2026-08-11. Design-first: no code until the
OMS 1.6 amendment lands (see [`oms-1.6-amendments.md`](oms-1.6-amendments.md)).
Companion to [`gdpr.md`](gdpr.md) §6 and [`erasure.md`](erasure.md).

**The claim.** Revoking a consent should erase everything that consent
authorized — automatically, in one audited operation. OMS already specifies
the vocabulary (a Consent grain and a `processing_basis` common field);
nothing in the agent-memory space implements the cascade, and the spec is
ours to extend.

---

## 1. Why this matters (Art. 6/7/17)

Under GDPR every processing operation needs a lawful basis. When that basis
is consent (Art. 6(1)(a)), the data subject may withdraw it at any time
(Art. 7(3)), and withdrawal must be "as easy as giving it". After withdrawal,
processing that relied on that consent has no basis — so the data must go
(Art. 17(1)(b)).

Today an Areev deployment answers this by hand: record the withdrawal, then
work out which grains relied on that consent, then erase them. The middle
step is the hard one, and it is exactly the step a memory engine can do
mechanically — **if** each grain records the basis it was written under.

## 2. What exists today

| Piece | State |
|---|---|
| `Consent` grain (type byte `0x0A`) | **Implemented** in `areev-core` — `subject_did`, `grantee_did`, `scope`, `is_withdrawal`, `basis`, `jurisdiction`, `prior_consent`, `witness_dids`. Serializes, replicates, recalls (`RECALL consents WHERE subject = "pat"`). |
| `processing_basis` common field | **Vocabulary only.** It appears in the compact-key table (`pbasis`) and nowhere else: no field on `GrainCommon`, no write path, no reader. A caller who sets it today gets it silently in `extra_fields`. |
| The cascade | **Absent.** No code links a grain to the consent that authorized it. |

Two defects to fix on the way:

1. **Registry/struct mismatch.** `registry.rs` advertises `queryable_fields`
   for Consent that the struct does not have: `consent_action`, `purpose`,
   `grantor_did`, `expires_at`, `granted`. Queries against them can only ever
   match `extra_fields`. Either add the fields or correct the registry — and
   the answer should follow OMS 1.6, not our convenience.
2. **`prior_consent` and `witness_dids` are accepted but dropped** by CAL
   ADD's Consent builder (`json_build.rs` lists them as allowed fields but
   never assigns them). `prior_consent` is precisely the link a withdrawal
   needs, so this must be fixed before the cascade can work at all.

## 3. The design

### 3.1 Promote `processing_basis` to a real common field

Add `processing_basis: Option<String>` to `GrainCommon`, serialized under the
existing compact key `pbasis`, **omit-default**.

Omit-default is the load-bearing detail: a grain that does not set it
serializes byte-identically to today, so **every existing content address is
unchanged** and the frozen-serialization invariant holds. Only new grains
that actually carry a basis serialize the new key. This is the same
compatibility shape every other optional common field already uses.

The value is a reference to the authorizing Consent grain — its content
address (`sha256:…`), because content addresses are the one identifier in
this system that cannot drift.

### 3.2 Populate it on the write paths

- CAL: `ADD fact … SET processing_basis = "sha256:…"`, and an ergonomic
  session-level default so a host does not repeat it per write:
  `ADD … WITH basis("sha256:…")`, or a session binding set once.
- Bindings and MCP: an optional `processing_basis` argument on the add
  surfaces, following the scalars-in convention.
- The store validates shape (a well-formed content address) but **not**
  existence: a grain must be writable before its consent grain has synced, or
  a replica could reject legitimate writes. Dangling bases are surfaced by
  the reporting query in §3.5, not by refusing the write.

### 3.3 The cascade

A withdrawal is an ordinary Consent grain with `is_withdrawal = true` and
`prior_consent` naming the consent being withdrawn. Adding one does **not**
erase anything by itself — an append is not a destruction, and silent
deletion on write would be the worst possible shape for this feature.

Erasure is an explicit, authorized, audited act:

```
FORGET BASIS "sha256:<consent-hash>" BECAUSE "consent withdrawn (ticket #91)"
```

- Selector: a new `ErasureSelector::Basis(consent_hash)` — every grain whose
  `processing_basis` equals that hash. Implemented as an index over the
  `pbasis` value (a dictionary term like any other), so it reuses the
  existing `erase_where` machinery wholesale: one transaction, replicating
  tombstones, dictionary and vocabulary sweep, CAS reclamation, the Tier-2
  audit record — all inherited, none re-implemented.
- Transitive by construction: if a derived grain records the same basis, it
  is in the set. If it records a *different* basis, it is not — and that is
  correct, because it was authorized separately.
- Authorization: the `erase` verb, same as `FORGET SUBJECT`.
- The audit record names the **consent hash**, not the subject — a content
  address of an authorization, not identity material, so the fingerprint rule
  in `gdpr.md` does not apply.

### 3.4 What the cascade must NOT do

- **Not erase the consent record itself.** The Consent and its withdrawal are
  the evidence that the erasure was lawful (Art. 7(1): the controller must be
  able to demonstrate consent). They survive; the data they authorized does
  not.
- **Not cascade across bases.** A grain written under legitimate interest or
  contract keeps its basis and stays. Multi-basis processing is real, and
  quietly deleting data that had another lawful basis is its own violation.
- **Not fire automatically on withdrawal.** Withdrawal is a fact; erasure is
  an operation. Keeping them separate is what makes the operation reviewable
  and the timing the controller's to decide (they may owe a retention
  obligation that outranks the withdrawal).

### 3.5 Reporting

Before erasing, a controller needs to see the blast radius — same
show-me-then-delete symmetry as `REPORT SUBJECT`:

```
REPORT BASIS "sha256:<consent-hash>"
```

Read-classified, `read`-gated, returning the grains that would be erased plus
a summary. It also surfaces **dangling bases** (a `processing_basis` naming a
consent this memory does not hold) — a data-quality signal a compliance
review wants.

## 4. Work plan

| Step | Touches | Gate |
|---|---|---|
| 1. Fix the Consent registry/struct mismatch + the dropped `prior_consent`/`witness_dids` in CAL ADD | `areev-core/types/{consent,registry}.rs`, `areev-cal/json_build.rs` | grain roundtrip + conformance |
| 2. `processing_basis` on `GrainCommon` (omit-default) | `areev-core/types/grain.rs`, `format/{serialize,deserialize}.rs` | **golden serialization must not move for grains that omit it** |
| 3. Write paths (CAL, MCP, bindings) | per the add-operation playbook | cross-surface parity |
| 4. `ErasureSelector::Basis` + `FORGET BASIS` + `REPORT BASIS` | `areev-store`, `areev-cal` (lexer→classify→parity table) | conformance on both backends |
| 5. Docs | `gdpr.md` §6, `erasure.md` (REQ-ERASE-10), `cal-reference.md` | docs-examples test |

Step 2 is the one that needs care: it is a change to the frozen serialization
surface. It is safe **only** because omit-default makes it invisible to every
grain that does not use it — that property must be proven by test (existing
golden addresses unchanged), not assumed.

## 5. Open questions

1. **Basis on the grain, or a link?** This proposal puts the basis *in* the
   grain (immutable, travels with it, one lookup). The alternative — a
   `related_to` link from grain to consent — is retrofittable to existing
   grains but adds an index hop and can be severed independently of the
   grain. Recommendation: in the grain; links are for annotations, and a
   lawful basis is not an annotation.
2. **Expiry.** Consent with `expires_at` (once the field exists) could feed
   the retention analyzer: an expired consent is a basis that no longer
   authorizes, so its grains become sweep candidates. Attractive; needs care
   that clock skew never deletes early.
3. **Cross-memory cascade.** If grains in memory B were written under a
   consent recorded in memory A, nothing propagates the withdrawal. ASSEMBLE
   mounts are read-only by construction, so this is a deployment question
   (one consent registry per trust domain) rather than an engine one — but it
   should be stated in `gdpr.md` rather than left implicit.
