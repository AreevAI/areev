# Per-subject crypto-erasure — proposal

**Status:** proposal, written 2026-08-11. Deliberately design-first: this is
a key-management design, and key management is where storage systems lose
data. No code until the open questions in §6 have answers.

Companion to [`gdpr.md`](gdpr.md), [`erasure.md`](erasure.md), and
[`security-model.md`](security-model.md).

---

## 1. The problem this solves

Areev erases a subject by deleting rows and replicating tombstones. That is
complete for the live store and for any replica that replays the tombstones,
and — since archive retention landed — it reaches archives within the
configured window (`gdpr.md` §3).

Three residues survive that model:

1. **The window.** Between an erasure and the next checkpoint-plus-expiry, a
   pre-erasure archive still holds the bytes. Bounded and documented, but not
   zero.
2. **Physical remnants.** On the embedded backend, deleted pages can linger
   in the file and its WAL until compaction (`erasure.md`, Backend notes).
3. **Offline copies.** A bundle someone exported to a laptop last year is
   beyond the reach of any tombstone.

Whole-memory crypto-erasure already answers all three — destroy the key and
the file is noise — but its granularity is the entire memory. Per-subject
crypto-erasure would give the same finality at the granularity a DSAR
actually asks for.

## 2. The shape

Each subject gets a derived key; the subject's data is encrypted under it;
erasure is key destruction.

```
subject_key = HKDF-SHA256(page_key, info = "areev.subject.v1:" || subject_id)
```

Derivation (rather than storage) means there is no per-subject keyring to
back up, sync, or lose — the key is reproducible from the memory key plus the
identity. But it also means **destruction cannot be "forget the key"**: it is
reproducible by construction. Something must record that a subject's key is
revoked, and that record must be the thing that is authoritative.

So the design needs a **tombstone-of-key**: a small, replicating grain
(`agent:authz`-adjacent) saying "subject fingerprint X is revoked as of T",
plus an engine rule that refuses to re-derive a revoked key. That converts
the property from "the bytes are unreadable" (cryptographic) to "the bytes
are unreadable unless someone patches the engine" (policy) — which is *weaker
than whole-memory crypto-erasure* and must be stated plainly rather than
marketed as equivalent.

The stronger alternative: a stored per-subject keyring, encrypted under the
memory key, where erasure genuinely destroys key material. That gets real
cryptographic finality, at the cost of a keyring that must survive backup and
restore — lose it and every subject's data is gone.

**Neither is obviously right.** That tension is the reason this document
exists instead of a branch.

## 3. What cannot be encrypted per-subject

This is the part that decides whether the feature is worth building, and it
is easy to hand-wave.

| Structure | Can it be subject-keyed? |
|---|---|
| Grain blob (the `.mg` bytes) | **Yes** — the natural unit. |
| CAS attachments | **Yes** — already keyed via `blobcrypt`; a per-subject subkey is a small change. |
| Term dictionary (`terms`) | **No, as designed.** Identifiers are interned as shared integer ids; the subject's own string sits in a table every query touches. Erasure already tombstones the string, but it is not encrypted *per subject*. |
| BM25 postings (`fts_post`) | **No.** Postings map token → grain; encrypting them per subject would break the inverted index, which is the whole point of having one. |
| Vector embeddings | **No, and this leaks.** An embedding is a lossy but real projection of the text; leaving it readable after a crypto-erasure leaves a semantic shadow of the erased content. It would have to be deleted (which is what erasure does today) — so vectors gain nothing from this feature. |
| Index rows (`triples`, `heads`, `entity_latest`) | **No.** They are integer ids; they must stay queryable. |

**So per-subject crypto-erasure protects grain bodies and attachments, and
does nothing for the index layer** — which still has to be deleted the way it
is today. The honest framing: this is a *defense-in-depth layer over*
tombstone erasure for offline copies and unreachable archives, not a
replacement for it. Any pitch that says "erasure becomes instant and total"
would be false.

## 4. What it would actually buy

- A bundle exported before the erasure becomes unreadable for that subject's
  grain bodies (its index rows were never in the bundle anyway).
- Pre-erasure archive segments lose their payload for that subject before the
  retention window expires.
- Physical remnants in file pages become ciphertext.

That is a genuine improvement to the three residues in §1 — worth having,
narrower than the phrase "crypto-erasure" suggests.

## 5. Interaction with what shipped

- `blobcrypt` (Art. 32 work) already establishes the pattern: HKDF-derived,
  domain-separated subkey; format magic so a directory can hold both
  generations; content addressing over plaintext. A per-subject variant is
  `info = "areev.subject.v1:" || id` and a second magic — the mechanics are
  proven.
- The subject *fingerprint* introduced for audit records (`gdpr.md`) is
  already the right way to name a subject in a revocation record without
  re-identifying them.
- `REPORT SUBJECT` / `FORGET SUBJECT` share one selector; a key-revocation
  path must use the *same* selector or the three operations drift.

## 6. Open questions (must be answered before code)

1. **Derived-and-revoked, or stored keyring?** §2. This decides whether the
   guarantee is cryptographic or policy-enforced. Recommendation: stored
   keyring encrypted under the memory key, because a policy-only guarantee
   should not be called crypto-erasure — but that makes keyring durability a
   new operational burden, and it must be documented as loudly as encryption
   at rest's `.kdf` sidecar is.
2. **Rotation.** Rotating the memory key must not orphan subject keys.
   Re-wrapping a keyring is straightforward; re-deriving from a new page key
   is not (every subject's data would need re-encryption).
3. **Escrow / recovery.** If an operator loses the keyring, the memory
   survives but every subject's bodies are gone. Is that acceptable, or does
   this need an escrow story? (Note that an escrow that can resurrect an
   erased subject is a compliance hazard in the other direction.)
4. **What happens to a subject who returns?** A new grain about a re-consented
   subject with a revoked key: new key version, or refuse? Versioned subject
   keys (`v1`, `v2`) with the revocation naming the version seems right.
5. **Cost.** Every read of a subject's grain becomes a decrypt. Recall p50 is
   budgeted at <200µs and the voice loop at a 50ms frame; AES-GCM on small
   payloads is fast, but this must be measured against those gates before it
   is defensible, not after.
6. **Does the index-layer gap make it not worth it?** §3. If a deployment's
   real exposure is the index and the vectors, this feature does not address
   it, and archive retention (already shipped) plus whole-memory
   crypto-erasure may be the better answer. **This question should be settled
   with a real deployment's threat model before any of the above is built.**

## 7. Recommendation

Do not build this yet. The shipped combination — tombstone erasure with
partition-key and text-mention reach, archive retention with a documented
window, encrypted blobs, and whole-memory crypto-erasure for destruction —
covers the obligations an audit actually tests. Per-subject crypto-erasure is
a genuine hardening of the residues, but it is narrower than its name, it
adds a durability burden, and §6.6 might conclude it is the wrong lever
entirely.

Revisit when a deployment presents a threat model where offline copies are
the binding constraint.
