# OMS amendments — the anonymization batch

**Status:** amendment proposal, written 2026-08-16 — and drafted into the
unmerged OMS 1.6 / CAL 1.3 RFC (`release/oms-v1.6` in the OMS repo: OMS
§10.5 Pseudonymized Egress, CAL §8.1.2/§8.2.1 `WITH anonymize`, CAL §8.17
`REHYDRATE`, CAL-E127). The spec-level half of
[anonymization-proposal.md](anonymization-proposal.md) (P5): everything that
feature wants from CAL and the wire format, batched so conformance moves
once. Nothing here ships until the OMS process accepts it — CAL syntax is a
conformance contract, and per that proposal's D7 the file-declared policy
(not query text) remains the gate regardless of what is ratified here.

| # | Amendment | Shipped in Areev? | Conformance impact |
|---|---|---|---|
| B1 | `anon:<ns>` meta keys | **Yes** (P1, write-if-absent replication) | New reserved meta prefix |
| B2 | `vault:` meta keys, reserved non-replicable | **Yes** (P3) | Reserved prefix that MUST NOT replicate |
| B3 | `anonymized` payload report | **Yes** (P1) | New optional response field |
| B4 | `WITH anonymize("<level>")` recall option | No — this document | New WITH option (strengthen-only) |
| B5 | ASSEMBLE per-source `anonymize` override | No — this document | WITH option on named sources |
| B6 | `REHYDRATE` statement | No — this document | New statement + classification |

B1–B3 shipped ahead of ratification because the egress boundary could not
wait for a spec cycle to be safe; they are recorded here as what an
implementation must do to interoperate with files that carry them. B4–B6
are gated on acceptance.

---

## B1 — `anon:<ns>` meta keys

One JSON policy per namespace under the reserved `anon:` prefix (schema in
[anonymization-proposal.md](anonymization-proposal.md) §8.1). Conformance
requirements:

- **Replication is write-if-absent** — like `retention:<ns>`, a sync never
  silently swaps a live local policy, and an applied row takes effect on
  the live handle, not at the next open.
- **An unreadable row fails reads closed.** A policy the implementation
  cannot parse must not silently mean "no policy" — reads of covered
  namespaces refuse until the row is repaired; policy writes keep working
  so it can be.
- **Declaring a policy stamps `min_reader_version`**, so an older
  implementation is warned loudly at open that the file declares
  protection it cannot honor.

## B2 — `vault:` meta keys, reserved and non-replicable

`vault:<ns>:<placeholder>` rows hold the sealed placeholder→value mapping.
Conformance requirements are prohibitions:

- The prefix is **reserved**: implementations MUST NOT repurpose it.
- Vault rows **MUST NOT ride bundles or segments** in either direction —
  export omits them and import refuses them, because the re-identification
  table travelling to a replica undoes the pseudonymization it serves.
- Values are sealed under a key derived from the file's encryption key
  (Areev: `HKDF(page_key, "areev.vault.v1")`, row key as AAD), so
  destroying the file key destroys the vault — crypto-erasure reaches it
  by construction.

## B3 — the `anonymized` payload report

When an egress policy is active, query responses carry an `anonymized`
object alongside the result: the covered namespaces, whether a host floor
forced coverage, and per-namespace **mapping ids** — never the mappings.
Implementations that do not anonymize simply omit the field; consumers MUST
treat its absence as "values are as stored", never as "values are safe".

## B4 — `WITH anonymize("<level>")` (recall tier)

```
RECALL facts WHERE subject = "caller:john" WITH anonymize("strict")
```

**Semantics: strengthen-only.** The option may raise the effective
treatment for this one query — e.g. force `redact` where the policy says
`pseudonym`, or demand additional detector tiers — and MUST NOT weaken or
disable a file-declared policy. Levels: `"standard"` (the declared policy,
a no-op where one is active — useful as an assertion), `"strict"` (every
category treated at `redact` severity).

**Why strengthen-only is load-bearing, twice over.** First, CAL's
forward-compatibility rule is that unknown WITH options warn and skip — so
on an older implementation this option silently does nothing, which is
survivable only if it was never the gate. Second, a weakening spelling
would make every query author a policy author; the file's declaration must
outrank query text or the policy is advisory.

## B5 — ASSEMBLE per-source `anonymize` override

The same option on a named ASSEMBLE source, same strengthen-only rule:

```
ASSEMBLE FROM facts AS work WITH anonymize("strict"),
         FROM org.facts AS background
```

A mounted source's own file policy still applies first; the override can
only add severity on top. This is the multi-file case where per-source
treatment genuinely differs (a mounted org replica may warrant stricter
handling than the session's own memory).

## B6 — `REHYDRATE`

```
REHYDRATE "<text>" WITH mapping("<mapping_id>")
```

**Semantics.** Replaces exact placeholder tokens in `<text>` with their
originals from the mapping the id names — the round trip's return leg as a
query-language spelling. Unmatched tokens are left intact and reported,
never guessed.

**Classification: this is the contentious one, so it is stated rather than
hidden.** Rehydration is re-identification. The statement classifies as a
**new class or as `Control` with a mandatory grant** — it must NOT classify
as a plain `Read`: an implementation with authorization must gate it on the
reveal/admin grant and write the Tier-2 audit record (fingerprints, never
identities), exactly as the API-level reveal does today. An implementation
whose classification enum is exhaustive-without-wildcard (as Areev's is)
will be forced to decide this at compile time, which is the point.

**Resolution semantics.** The id resolves against the session's live
mappings first, then the file's vault. A bare id whose mapping no longer
exists is an error, not an empty substitution — silently returning the
placeholders would look identical to a model echoing them.

---

## What this batch deliberately excludes

- **No weakening spelling** (`WITH anonymize(off)` does not exist at any
  level) — see B4.
- **No vault query surface.** The vault is not addressable from CAL beyond
  B6's id resolution; enumeration stays an authorized API/CLI operation.
- **No detector configuration in query text.** Which detectors run is
  policy + host capability; a query demanding a detector the host lacks
  fails closed rather than negotiating.
