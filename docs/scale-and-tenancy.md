# Laying out a large multi-tenant corpus

**Status:** reference. Written 2026-08-28, from the measurements in
[`crates/areev-bench/RESULTS.md`](../crates/areev-bench/RESULTS.md) §8 and the
authorization positions in [`authz-proposal.md`](authz-proposal.md) §D4 and
[`auth-proposal.md`](auth-proposal.md) §"No intra-memory ACLs".

Most Areev deployments never read this: a personal agent, a voice assistant, a
per-user memory file are all one memory with a handful of namespaces, and the
defaults are right. This document is for the other shape — a **shared corpus**
where many documents belong to many matters, several people have different
rights over different subsets, and the whole thing is one or two orders of
magnitude past the 10k grains the latency gates are set at. A firm's case
files, a fund's deal room, a support org's account history.

Such a deployment has to answer two questions before it writes its first grain,
and the single most useful thing to know is that **they are the same question**.

---

## 1. The partition boundary is the permission boundary

Areev enforces authorization at **memory × namespace × verb**. That is the
finest granularity there is, and it is a design position rather than a backlog
item:

> Grain-level / subject-level ACLs stay a non-goal (the isolation unit is the
> memory — invariant 5). — [`authz-proposal.md`](authz-proposal.md) §D4

> No intra-memory ACLs. The memory is the isolation unit. —
> [`auth-proposal.md`](auth-proposal.md)

So there is no document-level ACL to wait for. If a subset of the corpus needs
its own access rule, that subset must **be** a namespace or **be** a memory —
there is no third option, and no amount of application code makes a grain
individually permissioned inside a namespace someone can read.

The practical consequence: draw the partition map from the *access* map, not
from the data model. If two documents will always be visible to exactly the
same people, they can share a namespace no matter how different they are. If
one document must be visible to a subset, it needs its own namespace even if it
is otherwise identical to its neighbours. Getting this backwards — partitioning
by document type, then discovering the access rules cut across it — is the
expensive mistake, because the partition is baked into every grain's namespace
and moving it means rewriting the corpus.

---

## 2. What each boundary costs

Namespaces and memories are not interchangeable, and RESULTS.md §8 measures the
difference. At 100k grains:

| | separate namespaces, one memory | separate memories |
|---|---|---|
| Isolation | authorization (grants), enforced per verb | structural — separate file or Postgres schema |
| Erasure of one unit | `FORGET SUBJECT` / retention sweep | delete the file / `DROP SCHEMA` |
| Export of one unit | subject bundle | copy the file / `pg_dump -n` |
| Cross-unit query | native (prefix scope, `ASSEMBLE`) | needs facade **mounts** |
| Write concurrency | one writer per memory (embedded) | independent |
| BM25 within a unit | **1.2 ms** (vs 220 ms flat) | fast — small corpus |
| Structural recall | sub-ms at 1M grains | sub-ms |
| Vector k-NN within a unit | fast **if scoped** — see §3 | fast |
| Vector k-NN across units | **605 ms** — brute-force union scan | worse: no cross-mount k-NN |

Read that table as: **namespaces are for things you will query together;
memories are for things you must be able to destroy, export, or hand over
separately.** A regulator's "delete everything about this matter" is trivially
answered by a memory and answered by policy inside a namespace. A "find every
matter that looks like this one" is native across namespaces and awkward across
memories.

The usual right answer for a firm-shaped corpus is **one memory per firm, one
namespace per matter** — with a second memory only where a legal or contractual
wall genuinely requires that no query be able to span it. Reach for
memory-per-matter when the count is small and the walls are hard; it does not
scale to thousands of units gracefully, because each is a file or a schema and
cross-unit reads need explicit mounts.

---

## 3. Make the vector leg rule things out

The one leg that does not get fast for free is vector search. It is an **exact
scan** on both backends by default, linear in the vectors a query cannot rule
out — 10.6 ms at 10k grains, 121 ms at 100k, 1,187 ms at 1M on the embedded
tier (RESULTS.md §8a). Everything else is index-backed and stays sub-millisecond
at a million grains.

Three levers, in the order to reach for them:

1. **Scope the query.** A k-NN narrowed by namespace or subject is served from
   `idx_grains_ns_s` as an exact search over the survivors: 0.20 ms at 100k
   grains against 249 ms unscoped. This is both the fastest option and the
   exact one. Most retrieval in a matter-shaped corpus is already scoped —
   "what do we know about *this* matter" — so most queries need nothing else.
2. **Partition so the scope can be spelled.** Namespace-per-unit makes BM25
   180× faster (its posting index is keyed `(term, ns)`) and, since
   `nearest_vector`/`nearest_semantic` accept a scope, makes the vector leg
   proportional to the scope rather than the corpus. Measured over 200
   namespaces at 100k grains: one unit 0.75 ms, one sector (an eighth of the
   tree) 18.9 ms, the whole tree 150 ms — against 605 ms for the same
   whole-tree question before, when it could only be asked through
   `recall_hybrid`.

   The catch is that a scope can only select what the namespace encodes. A flat
   `matter.<id>` supports "this matter" and "all matters" and nothing between;
   `matter.<practice>.<id>` also supports "everything in this practice", which
   is the query people actually ask. **Put the hierarchy you will want to query
   by into the namespace at ingest** — it is not addable later without
   rewriting the corpus, for the same reason §1's partition map is not.
3. **Then, and only for the genuinely corpus-wide query, an ANN index.**
   `Areev::ensure_vector_index` builds a pgvector HNSW index (Postgres only;
   the embedded backend answers `STO-E007`), taking the unfiltered query from
   24 ms to 1.0 ms at 100k grains. It is opt-in because it makes recall
   approximate.

**Measure recall with your own model before trusting an ANN index.** The
latency win is unconditional. The accuracy cost is a property of your embedding
model's geometry, not of the index, and cannot be inherited from anyone else's
benchmark. Measured here against a real model (`mxbai-embed-large`, dim 1024,
30k grains) HNSW costs nothing worth counting — recall@10 of 0.97 at default
build parameters, 1.00 at `m=32, ef_construction=200`, for a 12× speedup. The
*same index* measured with a synthetic embedder reports 0.33. The tell that a
recall number is measuring the embedder rather than the index is that it does
not move when `ef_search` does (RESULTS.md §8e).

Two commands settle it for your corpus: `pe_scale --dump-topk` against the
exact scan, then `--compare-topk` with `--ann`. Do it before an ANN index
reaches a corpus anyone relies on.

A note on tiers: the PostgreSQL backend is **faster** at the vector scan than
the embedded one (24 ms vs 121 ms at 100k), because `pgvector`'s `<=>` is SIMD
over a native `vector` type. It is also ~16× slower to bulk-load through the
grain path (370 vs 5,600–7,000 grains/s), which is round-trip cost, not storage
cost. Size ingest windows accordingly; a million grains is ~3 minutes embedded
and ~45 minutes over loopback Postgres.

---

## 4. What stays the application's job

Areev is a library with a host-process trust boundary
([`security-model.md`](security-model.md)). A shared corpus with real users
needs a service in front of it, and these belong to that service, not here:

- **Identity.** Areev maps an authenticated principal to grants. Getting from
  an IdP to a principal is the deployment's: an authenticating proxy
  (`--sso-header`) is the documented default, native OIDC is available behind
  the non-default `oidc` feature ([`runbooks/oidc-setup.md`](runbooks/oidc-setup.md)).
  No SAML, no SCIM, no directory sync — provisioning is writing grant Facts,
  which is scriptable via CAL ([`procurement.md`](procurement.md)).
- **Anything finer than namespace × verb.** Per §1, that is not delegable to
  Areev; it is a filter your service applies, over a corpus laid out so the
  filter is not load-bearing for security.
- **Certifications.** SOC 2 / ISO 27001 attach to a hosted offering. Areev is a
  library and self-hosted binaries.
- **Backup / DR.** Inherit the storage tier's. This is the main argument for
  the Postgres backend in a regulated deployment: your existing failover, PITR
  and backup drills cover the memory too.

What Areev does bring to that review is in [`procurement.md`](procurement.md),
answered row by row with the command that proves each: tamper-evident audit
export, retention floors and legal holds, DSAR read and erasure sharing one
selector, per-principal credentials with separation of duties, and encryption
at rest with crypto-erasure on the file backend.
