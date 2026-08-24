# GDPR: obligations → Areev capabilities

**Software cannot *be* GDPR-compliant — a deployment is.** You (the
controller or processor) hold the obligations; Areev's job is to make each
one mechanically easy to satisfy and easy to *evidence*. This page maps
articles to the mechanism that answers them, states the deployment
requirements you must meet for those answers to hold, and lists the limits
honestly. Lift it into your DPIA; it is engineering documentation, not legal
advice.

Companions: [`erasure.md`](erasure.md) (the erasure requirement record,
REQ-ERASE-1..9), [`security-model.md`](security-model.md) (threat model,
auth, what is and isn't encrypted), [`cal-reference.md`](cal-reference.md)
§8 (the destruction model),
[`oms-1.6-amendments.md`](oms-1.6-amendments.md) (the spec changes this work
required). Design-stage: [consent cascade](consent-cascade-proposal.md),
[per-subject crypto-erasure](crypto-erasure-proposal.md),
prompt-safe pseudonymization (design doc kept outside the repo; the shipped
surface is `anon:<ns>` policies + the egress boundary — see
`crates/areev-store/src/anon_gate.rs`).

---

## 1. The article map

| Obligation | Mechanism | Surfaces |
|---|---|---|
| **Art. 15** — right of access | `subject-report`: every grain referencing an identity (exact + partition keys like `pat#visit1`, full supersession history, optionally indexed-text mentions), as `{hash, type, fields}` JSONL | `areev subject-report`, CAL `REPORT SUBJECT "<id>"`, MCP `areev_subject_report`, `subject_report()` (py/js) |
| **Art. 20** — data portability | The same selection as a portable MGB1 bundle, importable into any OMS store | `areev subject-report --bundle out.mgb`, `subject_bundle()` (py/js) |
| **Art. 16** — rectification | Supersession: the correction and the corrected value both survive, with the reason on the grain — a rectification that provably happened | `SUPERSEDE … BECAUSE`, `areev supersede`, all bindings |
| **Art. 17** — erasure | `FORGET SUBJECT`: every matched grain (history included), its dictionary entries, erased-only vocabulary, telemetry rows, and sole-referenced CAS attachments — replicating as ordinary tombstones, which delete on replicas too | `areev forget-subject --yes`, CAL `FORGET SUBJECT … BECAUSE`, `forget_subject()` (py/js) |
| **Art. 17 / 5(1)(e)** — storage limitation | Declared per-namespace retention that travels with the file, enforced on a cron or through the loop's governed review queue; plus the ad-hoc age sweep. Archive reach via checkpoint-and-retain (§3) | `areev retention set/list/clear/sweep`, `areev purge-older-than --yes`, CAL `PURGE OLDER THAN … BECAUSE`, loop analyzer `loop.retention_sweep` |
| **Art. 5(2), 30** — accountability | A Tier-2 audit Observation per destructive execution (principal, verb, target, reason, count) in `agent:authz`, plus the loop's hash-chained lifecycle trail — exported as JSONL evidence | `areev audit export [--since MS] [--out FILE]`, `RECALL observations WHERE namespace = "agent:authz"` |
| **Art. 33** — breach notification | The same export answers "what was accessed/destroyed, by whom, when, and why" over a time window | `areev audit export --since <ms> --until <ms>` |
| **Art. 32, 25** — security, data protection by design | Grants-in-file RBAC (verbs × namespaces per principal, fail-closed), anonymous read-only default on the console, credential map holding no raw secrets, encryption at rest (AES-256-GCM, Argon2id-derived key) | `GRANT`/`REVOKE`, `areev ui --token-env`, `open_encrypted` |
| **Art. 28, 44–49** — processors, transfers | File-backed memories you own; self-hosted Postgres backend; no mandatory cloud in any path, so residency is wherever you put the file or the database | — |

**The access/erasure symmetry is the load-bearing property.** The report and
the erasure run **one selector** (`REQ-ERASE-9`): what a DSAR discloses is
exactly what an erasure removes. They cannot drift, because drift would mean
disclosing one set and deleting another. "Show me everything, then delete
it" is two commands over one selection:

```bash
areev subject-report "pat" --db memory.db --ns caller --out pat.jsonl --bundle pat.mgb
areev forget-subject  "pat" --db memory.db --ns caller --yes --because "Art. 17 request #42"
areev audit export --db memory.db --out evidence.jsonl
```

The report is a **read**: `read`-gated, not behind the destructive cap,
available on the read-only console, and it writes no audit grain — the audit
obligation is on destruction, not access, and a read that recorded the
identity would re-introduce the reference erasure just removed.

### The audit trail does not re-identify

An audit grain is immutable, replicates to peers, and lands in archives. So
a subject erasure records a **fingerprint** of the identity —
`sha256(identity)[..8]` in hex, the scheme named in the record's
`subject_ref` field — never the identifier itself. Writing the raw
identifier there would leave the erased subject permanently recallable from
`agent:authz`, un-erasable by the subject selector, and copied into every
bundle and segment made afterwards: an Art. 17 failure hiding inside the
Art. 30 record.

The fingerprint keeps both properties you need. Given a candidate identity,
anyone can recompute the digest and **verify** that a specific record is
about that person — which is what "prove you erased me" actually requires —
but the log cannot be mined to enumerate who was erased. Put your
human-readable reference (ticket or request number) in `BECAUSE`: it names a
*request*, not a data subject, and it is the field you control.

```bash
areev forget-subject "pat" --db memory.db --ns caller --yes --because "Art. 17 request #42"
# verify a record refers to "pat":
python3 -c 'import hashlib;print(hashlib.sha256(b"pat").hexdigest()[:16])'
```

Hash-form `FORGET` records the content address (already-deleted content, not
identity material) and `PURGE` records an age and namespace — neither needs
a fingerprint.

---

## 2. Deployment requirements

These are **requirements**, not suggestions. The article map above assumes
them; a deployment that skips one has a finding waiting.

1. **One memory per trust domain.** A memory is the unit of erasure and of
   isolation, and `areev ui --token-env`'s bearer token is a single shared
   secret over the whole of it — anyone holding it reads every namespace in
   that memory. One memory shared across tenants is a cross-tenant
   disclosure, not a misconfiguration. Multi-tenant, HA, or
   compliance-heavy deployments belong on the **Postgres backend** (one
   memory = one schema, `DROP SCHEMA` is memory-level erasure); the
   embedded file backend is for personal/edge use. Per-principal
   credentials (`areev ui --auth`) narrow *within* a memory; they do not
   make one memory safe to share across trust domains.
2. **TLS-terminating proxy for anything non-loopback.** All Areev HTTP is
   plaintext by design (no HTTP framework, no TLS stack — see
   `security-model.md`). The token protects against unauthorized callers,
   never against an eavesdropper. Loopback-only, or front it with a proxy.
3. **A documented archive-retention window.** Erasure tombstones replicate
   forward and delete on replicas, but bundles and `areev stream`
   generations are *point-in-time archives* — they hold pre-erasure bytes
   until they age out. Configure the window explicitly
   (§3) and state it in your DPIA. This is the same treatment DPA guidance
   accepts for database backups and WAL archives: erasure reaches archives
   within a bounded, documented, honored window.
4. **Keep identity references in structured fields.** Erasure and access
   both reach what the *indexes* reach — subject/object positions, thread
   sessions, run ids, and (opt-in) indexed text. A `user_id` buried in a
   free-text blob with no structured reference is neither reportable nor
   erasable. `WITH text_mentions` widens the reach; it does not replace
   this.

---

## 2a. Declaring retention (storage limitation)

Retention policy is a **file-truth**: a `retention:<ns>` row that travels
with the memory, so a copy, a sync, or a restore on another host still says
what the data's lifetime is. Declaring never deletes; enforcement is a
separate, audited act.

```bash
areev retention set --db memory.db --ns support --days 90 --type event \
  --because "support transcripts age out at 90d"
areev retention list  --db memory.db
areev retention sweep --db memory.db --yes        # enforce (cron)
```

Two enforcement paths, same semantics:

- **Cron** — `areev retention sweep --yes` applies every declared policy and
  writes one audit record per policy that actually erased something (a
  sweep that finds nothing deliberately writes nothing: a destruction trail
  records destructions, not that a job ran).
- **Governed** — enable the `loop.retention_sweep` analyzer (off by default;
  toggled from the console's Setup panel or `POST /api/loop/config`, and
  configured with `max_age_days` / `grain_type` / `max_grains`) and retention
  proposals enter the review queue with a mandatory reason, separation of
  duties, and the hash-chained audit trail. The proposal names **each grain**
  it would remove — a reviewer sees exactly what disappears rather than
  approving a predicate — and applying it needs admin scope plus
  `allow_destructive`. This is the product's own governance model applied to
  compliance.

  Note the deliberate asymmetry: the cron path issues one bulk age sweep; the
  governed path issues individual tombstones. Bulk erasure from a *proposal*
  is exactly the shape the destructive gate exists to stop, so the loop
  substrate refuses it — a recommendation can only ever remove grains it
  named.

## 3. Archive retention (checkpoint-and-truncate)

Erasure removes data from the live store and, via tombstones, from replicas
that replay them. Archives are different: a bundle written yesterday still
contains yesterday's bytes. The mechanism is the one every database uses for
backups — periodically snapshot the (already-erased) live store, then drop
archives older than the window:

```bash
areev stream --db memory.db --to /var/lib/areev/archive --retain 30d
areev stream --db memory.db --to /var/lib/areev/archive --checkpoint   # force one now
```

A checkpoint starts a **new generation** whose first segment is a full
snapshot of the live store; retention then drops whole generations older
than the window (never individual segments inside a live generation, which
would strand followers). A follower whose generation has aged out
re-baselines from the newest snapshot automatically.

**The guarantee to state in your DPIA:** an erasure performed at time T is
absent from every retained archive by T + the retention window, because
every generation written after T is snapshotted from the already-erased
store and every generation written before T is deleted at that age. Pick the
window explicitly — there is deliberately no implicit default, since a
silent retention window would be a compliance claim Areev made on your
behalf.

---

## 4. Limits, stated honestly

- **The `.blobs` CAS sidecar is encrypted only from the release that added
  `areev blobs encrypt`.** Encryption at rest covers the memory database
  (grains, indexes, op-log, WAL) and the telemetry sidecar; attachments
  written by an older build stay plaintext until migrated. Run
  `areev blobs encrypt --db memory.db` on any file whose attachments predate
  it, and check `open_warnings()` — the file tells you which state it is in.
  On Postgres, blobs live in-schema and inherit the database's own
  encryption (TDE/pgcrypto).
- **Physical remnants on the embedded backend.** Deleted pages can linger in
  the file and its WAL until compaction. For whole-memory destruction,
  crypto-erasure (destroy the key) is the strong path; `forget_subject` is
  the surgical tool *within* a memory that keeps living. On Postgres, rows
  are gone at commit and space follows autovacuum.
- **Text-mention reach.** `WITH text_mentions` matches what the BM25 index
  matches. Grains written with `index_text` off, or identity forms the
  tokenizer splits differently (a phone number renders as its digit runs),
  are reached only through their structured references. Requesting it
  without a fully built index is a **hard error**, never a silent partial
  answer — a partial DSAR response is a compliance failure, not a degraded
  read.
- **Library callers audit themselves.** The CAL statements and the CLI verbs
  write the Tier-2 audit grain. `forget_subject()` called directly from
  Python/Node/Rust deliberately does not (the engine writing a record naming
  the subject would re-introduce the reference being erased — REQ-ERASE-5).
  If you erase from library code, either log it yourself or route through
  the facade/CAL path.
- **Content addressing is not anonymization.** A grain's hash is derived
  from its contents; two identical grains share an address. Hashes are
  pseudonymous identifiers, not anonymized data.
- **`areev ui` is unauthenticated by default** (loopback, trusted local
  operator). Pass `--token-env VAR` for any shared machine.

---

## 5. Postgres deployment notes

For multi-tenant, HA, or compliance-heavy deployments:

- **Isolation.** One memory = one schema. `DROP SCHEMA … CASCADE` is
  memory-level erasure; `pg_dump -n <schema>` is memory-level export.
- **Encryption.** Areev's page cipher is file-backend-only and is rejected
  at open on Postgres — use the database's own mechanisms (TDE at rest,
  `pgcrypto` for column-level, TLS in transit).
- **Backups and WAL.** Align `wal_keep_size` / archive retention / base
  backup retention with the §3 window, or the archives outlive the
  guarantee. Erasure inside a memory is ordinary DML on this backend, so it
  reaches PITR archives only as fast as those archives roll over.
- **Autovacuum.** Space reclamation after a large erasure follows autovacuum;
  the rows are gone at commit either way.

---

## 6. Consent (Art. 6/7) — current state

OMS specifies a Consent grain (`0x0A`) and Areev stores it: you can record
consent grants and withdrawals as first-class, content-addressed,
replicating grains, and recall them (`RECALL consents WHERE subject =
"pat"`). What is **not yet implemented** is the *cascade* — automatically
erasing everything whose processing basis points at a withdrawn consent. See
[`consent-cascade-proposal.md`](consent-cascade-proposal.md) for the design.
Until it ships, a withdrawal is recorded as a grain and the corresponding
erasure is invoked explicitly (`FORGET SUBJECT`, or a scoped `PURGE`).
