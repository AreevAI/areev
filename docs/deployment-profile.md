# Design-partner deployment profile

The Wave-3 gate artifact (governed-agents §8): the configuration that is
sufficient for a design-partner security review TODAY, without waiting for
the Wave-6 enterprise plane. OIDC/SSO pulls forward only when a signed
partner requires it.

## The shape

```
┌ client apps / agents ────────────────────────────────────────────┐
│  LangGraph (AreevCheckpointSaver / AreevStore)  ·  CrewAI backend  │
│  areev CLI · MCP (stdio) · Python/Node bindings                   │
└───────────────┬──────────────────────────────────────────────────┘
                │ in-process (no server in the recall path)
┌ shared surfaces (optional) ──────────────────────────────────────┐
│  TLS-terminating reverse proxy (caddy/nginx/traefik)             │
│      └→ areev ui   --token-env AREEV_UI_TOKEN     (127.0.0.1)      │
└───────────────┬──────────────────────────────────────────────────┘
┌ storage ─────────────────────────────────────────────────────────┐
│  single-tenant: per-team memory FILES (encrypted at rest,        │
│      open_encrypted / AREEV_PASSPHRASE)                            │
│  multi-tenant:  PostgreSQL backend — one memory = one schema,    │
│      CONCURRENT writers per schema, pgvector                     │
└──────────────────────────────────────────────────────────────────┘
```

The containerized rendering of this exact shape — the image, the compose
files, the trigger heartbeat, and the AWS/GCP/Azure/Kubernetes mappings — is
[`docker.md`](docker.md).

## The five controls, in order

1. **Token auth on every shared surface.** `areev ui --token-env VAR`
   requires the token on every request (browser Basic prompt or
   `Authorization: Bearer`).
   Tokens come from the environment — never the command line, never a
   file in the repo.
2. **TLS at a terminating proxy, or native TLS where there's nowhere to put
   one.** The console is std-only HTTP/1.1 on loopback; this
   profile's default is production exposure through caddy/nginx with TLS
   and the loopback bind left as-is — the proxy is not a workaround, it is
   the profile. `areev ui` can also terminate TLS
   natively (`--tls-cert`/`--tls-key`, the non-default `tls` build feature,
   rustls) for deployments with nowhere to run a proxy — the exception path,
   not a replacement for the documented default.
3. **The multi-principal credential map.** Grants live IN the file as
   `mg:permits` Facts, and the run verbs are Control-tier: `run.execute` /
   `run.respond` (approver ≠ initiator **and** ≠ the run's `initiator`,
   structurally), `run.cancel` deliberately low. Erasure follows the same
   grants (`delete` / `erase` verbs) — provisioning is `areev`-CLI
   statements, auditable in the file itself.

   `facade.bind_principal` / `--as` remains the path for a host that
   **serializes requests**: it swaps one process-wide slot, so two concurrent
   requests race. A host serving many signed-in people uses
   `facade.principal_session(p)` instead, which since 1.9.0 (#302) is itself
   a CAL facade — reads as well as writes run under the session's own
   fail-closed rights, on any number of threads, over ONE store handle. "One
   OS process = one principal" is therefore no longer the rule; it was, and
   on the embedded backend (where a second handle is refused) it made
   multi-principal serving impossible. See
   [`security-model.md`](security-model.md) §"Multi-principal hosts".

   Projecting an external policy engine into a memory: `set_grants(principal,
   &[Grant], because)` makes a principal's live grants EQUAL a desired set in
   one pass, superseding heads in place where it can (#309), so narrowing a
   packed grant no longer means a window with no access or a window with too
   much. An equal set writes nothing. `authz_epoch()` is a single indexed
   read that changes whenever the memory's policy does — what a host caching
   bound sessions checks per request instead of evicting on a timer.
4. **Postgres for multi-tenant.** One memory = one schema keeps tenant
   isolation at the storage boundary; pgvector serves recall. Unlike the
   embedded backend, this one admits **multiple concurrent writers per
   memory** — `STO-E002` is never raised here, and the advisory lock covers
   *schema bootstrap only*, not writes. Concurrent writers block and
   serialise at `reserve_write` rather than erroring. Connection credentials
   are the platform's secret-manager problem (env vars in the service unit);
   the connection lifecycle, per-handle cost, and reconnect contract are in
   [Postgres connection contract](#postgres-connection-contract) below.
5. **Retention + holds, declared.** `areev retention set` (with
   `--min-days` floors) and `areev hold set` are file-truths that travel
   with the memory; `areev audit export` produces the hash-chain-verified
   accountability evidence a reviewer asks for first.

## Postgres connection contract

The numbers below are measured against `pgvector/pgvector:pg16` on loopback
Docker (Apple M4 Max). Treat them as shape, not as managed-database figures.

**Transport security: the DSN decides, and the build can only refuse.**
Compile with the `postgres-tls` cargo feature (on in the container image, on
in both bindings, off in the stock `areev` binary) and the DSN's `sslmode`
is honored with libpq's exact ladder:

| `sslmode` | encrypted | chain checked | hostname checked |
|---|---|---|---|
| `disable` | no | — | — |
| `prefer` (the default when you omit it) | if the server offers it | no | no |
| `require` | yes | no | no |
| `verify-ca` | yes | yes | no |
| `verify-full` | yes | yes | yes |

**`require` does not validate anything** — that is libpq's meaning, not a
shortcut taken here, and it is deliberate: AWS RDS signs with its own
`rds-ca-*` roots, so a `require` that quietly checked Mozilla's trust store
would fail every stock RDS DSN. **Use `verify-full`**, and add
`sslrootcert=/path/to/provider-ca.pem` wherever the provider signs with a
private root (RDS does; Azure Flexible Server's DigiCert chain is already in
the compiled-in Mozilla bundle). `sslrootcert=system` and omitting it both
mean that compiled-in bundle — no OS trust store is read, which is the only
promise a static binary can keep. Client certificates are not supported.

A binary built *without* the feature refuses `require` and above with
**`STO-E003`**, naming the feature; it never downgrades to plaintext. That
refusal is the whole point — the failure mode being prevented is a DSN that
asks for encryption and silently gets none. `disable` and `prefer` behave
identically with and without the feature, so no existing deployment changes.

This makes the local TLS-terminating proxy (Cloud SQL Auth Proxy, PgBouncer
with a TLS upstream) optional rather than mandatory. It is still the right
answer when the proxy is also pooling (session mode only — see below) or doing
IAM auth — point the DSN at it with `sslmode=disable`.

**Connections: one pool per DSN per process, `?pool=P` (default 8).** A
memory handle owns no connection (#181, third increment). Every handle on one
DSN — one server, one role, one TLS setting — borrows from one process-wide
pool: a connection per statement, or one for the length of a transaction,
returned as soon as that ends. So a process holding a thousand memories holds
at most P connections between them, an idle memory holds none, and the
telemetry sidecar rides the same pool rather than dialling its own. Set P on
the DSN (`postgres://…/db?schema=t7&pool=16`) or out of band with
`$AREEV_PG_POOL`, the DSN winning; the first open sizes the pool and a later
open naming another size is told so in `open_warnings()` rather than obeyed.
Size it for *concurrent transactions*, not for memories: a write holds its
connection until it commits, and callers past the cap wait their turn (FIFO)
rather than failing. A pool of 1 serialises everything in the process,
including the broker's blob door, so keep 2 or more where capability tools
run. Connections carry `application_name = areev`, which is what
`pg_stat_activity` counts and what the conformance suite asserts against.

The old advice — cache handles in an LRU and close idle ones — is
withdrawn: handles are cheap to hold open now, since holding one costs no
connection.

**What a process actually holds, for a capacity plan.** A pool is keyed by
the DSN, so the unit that multiplies is not the memory but the *pool*:

> connections held ≈ distinct DSNs opened × that pool's peak concurrency,
> until reaped.

"Distinct DSNs" means distinct server, role and TLS setting — every memory on
one role shares one pool and `?pool=P` caps it, which is the shape #181 was
designed for. **Give every tenant its own role and the multiplier is the
tenant count**: 500 tenants on one worker are 500 pools, and `?pool=` bounds
each of them separately, not their sum. That is the topology to size for if
you use one.

**Idle connections are reaped: `?pool_idle_secs=T` (default 300).** A
connection that has sat idle for T seconds is closed, and a pool whose
connections have all gone — with no handle and no statement holding it — is
dropped entirely, releasing its runtime's worker threads too (#229). So an
idle memory holds no connection *and* an idle process trends to zero, rather
than to one connection per role it has ever touched. One reaper thread per
process does this, looking every half-T (at most every 30 s), so a connection
outlives its TTL by at most that. Set T on the DSN or out of band with
`$AREEV_PG_POOL_IDLE_SECS`, the DSN winning, on the same first-open-wins
terms as `?pool=`; `pool_idle_secs=0` never reaps, which is what every build
through 1.8.0 did. Reopening a memory whose pool was reaped costs one dial and
nothing else — no warning, no bootstrap (the schema is stamped), no
reconfiguration.

Two sizing consequences. A short T on a busy process is a re-dial tax, not a
saving — the connections it closes are about to be wanted again; T is there
to bound what an *idle* process holds. And a long T is how you keep a
latency-sensitive path warm: nothing is reaped while work keeps arriving, so
steady-state traffic never pays for a dial.

**A pooler may run in transaction mode, on one condition and with one
caveat.** The store keeps nothing it needs on the session: every statement
names its tables schema-qualified (`"tenant_7".grains`, never `grains`
resolved through `search_path`), runtime DDL and catalog probes bind the
schema name instead of reading `current_schema()`, the bootstrap lock is
transaction-scoped (`pg_advisory_xact_lock`) and the bootstrap's own
`search_path` is `SET LOCAL` to that transaction, and a cached prepared
statement the backend no longer knows (`26000`) is re-prepared and retried
when it happens outside a transaction. The in-process pool depends on the same
property — any of its connections serves any schema — and the whole
two-backend conformance suite runs with `RESET ALL` issued before every
statement outside a transaction (`tests/pg_chaos.rs`), through a pool of two
(`?pool=2`), and — measured, not inferred — through a real PgBouncer in
transaction mode (#181).

The **condition**: the pooler must track prepared statements across backends.
The driver names every parameterized statement it sends, one-shot or cached,
so behind a pooler that does not track them a statement prepared on one
backend is unknown to the next — and when that happens *inside* a
transaction the transaction is aborted, which nothing store-side can undo.
PgBouncer 1.21+ tracks them when `max_prepared_statements` is above zero;
below 1.21, or with it at zero, use **session mode**. The error you get
otherwise names both remedies.

The **caveat**: `hnsw.ef_search` (`ensure_vector_index` /
`set_vector_ef_search`) is a wish of the *handle*, not of any session. Inside
a transaction the store applies it with `SET LOCAL`, which an external
transaction-mode pooler cannot lose. Outside one it is reconciled onto the
in-process pool's connection before the statement — which an external
transaction-mode pooler in front of that connection *can* drop between
statements, silently: ANN recall falls to pgvector's default of 40 with no
error. A deployment that tunes `ef_search` behind such a pooler should also
set it at the database level (`ALTER DATABASE … SET hnsw.ef_search = 100`).

pgvector's `vector` type and `<=>` operator are resolved through
`search_path` by Postgres itself, so the extension must live in a schema on
the *default* `search_path` (`public`, where the store installs it), not in a
private one only the store's session-level `search_path` reached.

| | Pooler mode |
|---|---|
| Fine | PgBouncer `session` mode; PgBouncer 1.21+ `transaction` mode with `max_prepared_statements > 0`; pass-through proxies (the Cloud SQL Auth Proxy is a TLS/IAM tunnel); Neon's direct endpoint |
| Session mode only | A transaction-mode pooler that does not track prepared statements: PgBouncer below 1.21 or with `max_prepared_statements = 0` |
| Check your pooler's docs | Supavisor, PgCat, Neon's `-pooler` endpoint: each has added prepared-statement tracking; the condition above is what to look for |
| Set it at the database too | A tuned `ef_search` for reads *outside* a transaction, through a transaction-mode pooler (see the caveat) |

**Open cost: provision schemas ahead of the request path.** First open of a
NEW schema runs the full DDL bootstrap under an advisory lock — hundreds of
milliseconds, fine as a provisioning step and unacceptable inside a request.
Steady-state open of an existing, populated schema is tens of milliseconds.
Create the tenant's schema when the tenant is created, not on their first turn.

**Open handles before bulk operations.** A bootstrapping open — the first
open of a schema, or the first open of a build that carries a migration —
runs DDL that takes a `ShareLock` on the tables it touches, and that lock
waits behind any long transaction already in flight: a second `open()` on a
schema mid-way through a bulk `add_embeddings` or a reindex blocks until that
transaction commits, and looks like a hang. Steady-state opens issue no DDL
(#180) and are unaffected, and so is `--read-only`, which never issues DDL at
all. So: provision the schema (`areev provision`) and open every handle you
will need *before* starting a bulk load, rather than opening a fresh one in
the middle of it.

**Outage recovery: automatic for reads, explicit for writes.** A managed
Postgres restarts, fails over, and drops connections as routine maintenance.
A connection that dies is discarded by the pool rather than returned — its
prepared statements died with it — and the next borrow dials a fresh one:

- a **read** is replayed transparently: it had no effect, so running it twice
  is running it once;
- a **write** is **not** replayed. The connection may have died *after* the
  server committed it, and nothing client-side can tell the difference, so
  replaying risks applying it twice. The call returns its error (`STO-E001`,
  carrying the Postgres SQLSTATE — `57P01` for an administrator restart) and
  the handle is usable again on the next call, on a fresh connection. **Your
  host must treat a failed write as a unit of work to redo**, exactly as it
  would any other transaction failure.
- nothing is replayed **inside an open transaction**: the transaction died
  with the connection, so the whole unit has to be re-run.

Conformance: `a_handle_recovers_from_a_database_outage` in
`areev-conformance`, run against a real server with a real
`pg_terminate_backend`.

**Writers: concurrent, not single.** See control 4 above — `STO-E002` is never
raised on this backend, and concurrent writers block and serialise at
`reserve_write` rather than erroring.

**Least-privilege read-only role (`--read-only`, issue #127).** Without
`--read-only`, the role that opens a memory **must own the schema** —
bootstrap (`CREATE SCHEMA IF NOT EXISTS`, every `PG_SCHEMA` DDL statement,
`PG_SEED`'s counter upserts) runs on **every** open, not just the first, and
Postgres checks the `CREATE` privilege on the database before it checks
whether the schema already exists, and table OWNERSHIP before it checks
whether an index already exists — so even fully idempotent `IF NOT EXISTS`
DDL 42501s a role that merely has SELECT on an already-migrated schema. A
dashboard, a reporting job, or a read-only console had no way to be handed
anything narrower than the owning credential.

`--read-only` (`AreevOptions::read_only` in the bindings) fixes this: the
open skips bootstrap entirely and instead verifies the schema and its core
tables exist via SELECT-only probes, so a role with exactly these grants can
open an existing, already-migrated memory and recall from it — and gets a
coded refusal (`STO-E004`) on any write, never a raw `42501`:

```sql
GRANT CONNECT ON DATABASE areev TO areev_readonly;
GRANT USAGE ON SCHEMA "<schema>" TO areev_readonly;
GRANT SELECT ON ALL TABLES IN SCHEMA "<schema>" TO areev_readonly;
-- Tables created by a LATER migration need this too, or they silently sit
-- outside the grant until someone re-runs the GRANT SELECT above by hand:
ALTER DEFAULT PRIVILEGES IN SCHEMA "<schema>"
  GRANT SELECT ON TABLES TO areev_readonly;
```

**Least-privilege read-WRITE role.** Since a current schema's open issues no
DDL, a read-write role no longer has to own the schema either. It does need
`DELETE`, which is easy to leave out because it sounds like an erasure-only
right and is not: superseding a grain collapses the head with a `DELETE`
followed by an `INSERT`, so a role without it opens, reads, and then fails on
the first `add`.

```sql
GRANT CONNECT ON DATABASE areev TO areev_rw;
GRANT USAGE ON SCHEMA "<schema>" TO areev_rw;
GRANT SELECT, INSERT, UPDATE, DELETE ON ALL TABLES IN SCHEMA "<schema>" TO areev_rw;
GRANT USAGE ON ALL SEQUENCES IN SCHEMA "<schema>" TO areev_rw;
ALTER DEFAULT PRIVILEGES IN SCHEMA "<schema>"
  GRANT SELECT, INSERT, UPDATE, DELETE ON TABLES TO areev_rw;
ALTER DEFAULT PRIVILEGES IN SCHEMA "<schema>" GRANT USAGE ON SEQUENCES TO areev_rw;
```

This role cannot bootstrap or migrate — that is the point. Provision with an
owning role (`areev provision`), then hand the application this one. Add
`?provision=never` to its DSN to make the guarantee enforceable: an absent or
stale schema is then refused with **`STO-E008`** naming the operator action
needed, with no lock taken and no `CREATE SCHEMA` attempted, instead of the
application quietly bootstrapping on a request path.

**A mount is a connection.** `--mount alias=<path|DSN>` opens each target
read-only, and a Postgres mount is its own connection on top of the budget
above — a console with three mounts draws four.

Run `areev provision --db <DSN> --schema <schema>` (or the owning role's
normal, non-`--read-only` open) at least once first — `--read-only` never
creates the schema, so there must be something for these grants to point at.
The same rule covers migrations: a build that changes the
schema (the digest-keyed `terms` dictionary, #160, is one) applies the
change on the owning role's next read-write open, and until then a read-only
open of that memory refuses with `STO-E005` naming what is missing, rather
than reading a shape it does not understand.

**Whether a schema migration is rolling-deploy safe depends on the
migration**, and the binary knows which: `areev provision --check` reports
`rolling_deploy: safe | drain_writers_first | unknown`, computed from
`PG_ROLLING_SAFE_FROM` — the oldest stamped version whose writers keep
working during and after this build's bootstrap. Ask it rather than reasoning
from the release notes. The two migrations so far:

| From → to | Verdict | Why |
|---|---|---|
| unstamped → **1** (the `terms` digest key) | `drain_writers_first` | it DROPS a constraint an older writer's `ON CONFLICT (term)` still needs — detail below |
| **1 → 2** (1.9.0, #307: `oplog.ns`) | `safe` | one NULLABLE column plus a backfill. An older writer inserts without it and its rows simply read as unattributed, which is exactly how pre-1.9.0 rows behave anyway |

The v1 case, which is the one that bites:

**That migration is not rolling-deploy safe.** The bootstrap advisory
lock serializes *openers* against each other; it does not stop writers that
are already inside the schema on the previous build. When the `terms`
migration drops the text uniqueness constraint, an older binary still running
against that schema fails its next dictionary insert (`42P10`: no unique
constraint matches its `ON CONFLICT (term)`; `23502`: `term_hash` is NOT
NULL), and the message names nothing an operator would connect to a deploy.
Keeping the old constraint for a transition release was considered and
rejected: the constraint *is* the ~2704-byte cap, so a build that kept it
would ship the bug it claims to fix. Therefore: **stop or drain every writer
on the old build before the first new-build open of a memory**, per schema.
Expect that first open to hold an exclusive lock on `terms` while it adds and
backfills the column and builds the digest index — one time, proportional
to the dictionary's size (seconds for tens of thousands of distinct terms).
Reads on the embedded backend are unaffected; this is a Postgres-tier rule.

**Ask the binary, do not diff the source** (1.9.0, #308).
`areev provision --check --db DSN [--schema NAME] [--telemetry MODE]
[--format json]` is a **read-only** probe: SELECTs only, no advisory lock, no
DDL, no `meta` write, so it runs under the documented least-privilege
read-only role. It reports, per schema, whether it `exists`; per stamp
(`pg_schema`, `telem_meta.schema_version`, `link_index`, `ns_registry`) the
found and wanted values; `pending: […]`; and
`rolling_deploy: "safe" | "drain_writers_first" | "unknown"` — computed from
`PG_ROLLING_SAFE_FROM`, a constant beside `PG_SCHEMA_VERSION` naming the
oldest stamped version whose writers keep working during and after this
build's bootstrap. `unknown` means there is no stamp to compare against:
absence of evidence is never reported as safety.

Exit **0** current, **2** pending or absent (the `loop list --fail-on`
convention), **1** error — so a release pipeline branches on the code and
reads the JSON only when it wants detail. The library entry point is
`areev_store::pg::check_provision(url, schema, telemetry)`.

Before this, the only probe was to open with `?provision=never` and watch for
`STO-E008`: an error path, covering one stamp, saying "stale" without saying
what was pending.

The motivating consumer is `areev ui --read-only`: paired
with #124 (the console no longer displays its own DSN), a read-only console
instance never needs — and never holds — write authority over the memory it
renders. The same open is reachable from the bindings —
`areev.Areev(dsn, read_only=True)` and `new Areev(dsn, …, readOnly)` (#183) —
so an embedded console, evaluator or analytics reader is handed the
SELECT-only role rather than the owner's credential.

**Paired layout — engine metadata in its own schema (#353).** By default
one memory is one schema: the grains and the engine's bookkeeping about them
share it. A data contract may require the bookkeeping to be *physically*
elsewhere — a memory schema that holds nothing but memory, and a metadata
schema beside it. Add `meta_schema=<name>` to the DSN:

```
postgres://app:***@pg:5432/areev?schema=drpaul_prod_memory&meta_schema=drpaul_prod_memory_metadata
```

Every table then has exactly one home. The classification is
`areev_store::pg::META_TABLES`, and it is by *kind*, not by table name:

| Where | Tables | Why |
|---|---|---|
| **memory schema** | `grains`, `triples`, `osp`, `entity_latest`, `heads`, `thread_idx`, `prov_idx`, `run_idx`, `corpus_idx`, `fts_vocab`/`fts_post`/`fts_doc`, `embeddings`, `oplog`, `terms`, `blobs` | the grains, every index derived from them, the op-log, the dictionary (every subject/relation/object string *is* content) and the CAS blobs — exactly what `pg_dump -n <memory>` must carry for the grains to be readable, and what a `DROP SCHEMA` must destroy for them to be gone |
| **metadata schema** | `meta`, `counters`, `ns_reg`, `telem_meta`/`telem_recall_log`/`telem_grain_access`/`telem_query_stat`/`telem_budget_stat` | id allocation, the namespace inventory, the telemetry sidecar (a separate *file* on the embedded backend — the separate schema is the same rule), and the `meta` table as one unit |

`meta` moves whole. Every key in it is engine bookkeeping about the memory
rather than a grain of it: the `pg_schema`/`link_index`/`ns_registry` stamps;
the `text_index`/`entity_relations` declarations and embedding provenance;
`min_reader_version`; saved queries and templates (`qry:`/`tpl:`); retention
policies and floors (`retention:`/`retention_floor:`); anonymization
policies and the sealed mapping vault (`anon:`/`vault:`); legal holds
(`hold:`); trigger leases and cursors (`trg:`). The vault is the one family
someone could argue over — its rows are keyed by memory-derived tokens — and
the decision is that a mapping the engine keeps about content is still
metadata; splitting the table by key would put the one choke point every
policy read shares on two schemas. Nothing is duplicated into the memory
schema and no view stands in for it: introspecting the memory schema after
any amount of use finds none of these tables, which the paired conformance
runner asserts.

What does **not** change: CAL, Run and Loop semantics, every content
address, the registry's inverses and the `mg:permits` grants — the routing is
below all of them, in the one place statements are schema-qualified (#181),
so a write that touches both schemas is still one transaction on one
connection, and a transaction-mode pooler is as safe as before. Both names
come from the DSN and nowhere else, are validated to `[a-z_][a-z0-9_]*`
and quoted on every use; no grain, message or model output can select a
schema. No schema version moves: a paired memory and a single one carry the
same `pg_schema` stamp (read from the metadata schema), so `rolling_deploy`
verdicts apply unchanged.

Provision the pair the way you provision a schema — the verb reads the
layout off the DSN, or takes it as a flag:

```bash
areev provision --db 'postgres://owner:***@pg:5432/areev' \
  --schema drpaul_prod_memory --meta-schema drpaul_prod_memory_metadata
areev provision --check --db '…?schema=drpaul_prod_memory&meta_schema=drpaul_prod_memory_metadata'
#   reports meta_schema, exists (both), every stamp, rolling_deploy
```

The runtime role needs the same grants as above **on both schemas** — and
nothing on any other schema, which is the point of the layout:

```sql
GRANT CONNECT ON DATABASE areev TO areev_rw;
GRANT USAGE ON SCHEMA "drpaul_prod_memory", "drpaul_prod_memory_metadata" TO areev_rw;
GRANT SELECT, INSERT, UPDATE, DELETE ON ALL TABLES IN SCHEMA "drpaul_prod_memory" TO areev_rw;
GRANT SELECT, INSERT, UPDATE, DELETE ON ALL TABLES IN SCHEMA "drpaul_prod_memory_metadata" TO areev_rw;
GRANT USAGE ON ALL SEQUENCES IN SCHEMA "drpaul_prod_memory" TO areev_rw;
GRANT USAGE ON ALL SEQUENCES IN SCHEMA "drpaul_prod_memory_metadata" TO areev_rw;
ALTER DEFAULT PRIVILEGES IN SCHEMA "drpaul_prod_memory"
  GRANT SELECT, INSERT, UPDATE, DELETE ON TABLES TO areev_rw;
ALTER DEFAULT PRIVILEGES IN SCHEMA "drpaul_prod_memory_metadata"
  GRANT SELECT, INSERT, UPDATE, DELETE ON TABLES TO areev_rw;
```

`?provision=never` and `--read-only` behave on a pair exactly as on a single
schema: the stamp is read from the metadata schema, an absent or stale pair
is `STO-E008` naming both schemas, a read-only open verifies both and never
creates either. Erasure is `drop_postgres_schema` on the same DSN, which
drops **both** schemas in one transaction after reading the holds from where
they now live; export is `pg_dump -n <memory> -n <metadata>`, and a restore
of the pair to one recovery point restores one memory (the stamps, counters
and registry it needs are in the second schema, so restore them together).

**A layout is a property of the memory, and an open never changes it.** A
DSN that names `meta_schema=` over a memory whose own schema already holds
`meta` (the single layout), or one that names none over a memory schema that
holds `grains` without `meta` (what a paired memory looks like from its
memory schema — nothing else does, since `meta` is the first table the
bootstrap creates and the bootstrap is one transaction), is refused with
**`STO-E011`** before any lock or DDL, on read-write, `provision=never` and
read-only opens alike. The alternative — quietly bootstrapping a second,
empty `meta`/`counters`/`ns_reg` beside the real one — would run the memory
with every hold, policy, saved query and trigger lease absent and two writers
allocating ids from two counter rows. Moving an existing single-schema memory
to the pair is therefore an explicit operator step, with every writer
stopped:

```sql
BEGIN;
CREATE SCHEMA "drpaul_prod_memory_metadata";
ALTER TABLE "drpaul_prod_memory".meta              SET SCHEMA "drpaul_prod_memory_metadata";
ALTER TABLE "drpaul_prod_memory".counters          SET SCHEMA "drpaul_prod_memory_metadata";
ALTER TABLE "drpaul_prod_memory".ns_reg            SET SCHEMA "drpaul_prod_memory_metadata";
ALTER TABLE "drpaul_prod_memory".telem_meta        SET SCHEMA "drpaul_prod_memory_metadata";
ALTER TABLE "drpaul_prod_memory".telem_recall_log  SET SCHEMA "drpaul_prod_memory_metadata";
ALTER TABLE "drpaul_prod_memory".telem_grain_access SET SCHEMA "drpaul_prod_memory_metadata";
ALTER TABLE "drpaul_prod_memory".telem_query_stat  SET SCHEMA "drpaul_prod_memory_metadata";
ALTER TABLE "drpaul_prod_memory".telem_budget_stat SET SCHEMA "drpaul_prod_memory_metadata";
COMMIT;
```

(`ALTER TABLE … SET SCHEMA` moves a table's indexes and owned sequences with
it; skip the `telem_*` lines for a memory provisioned with `--telemetry
off`.) Then add `meta_schema=` to every DSN that names the memory and
re-grant the runtime role on the new schema. The reverse migration is the
same statements the other way. Opt-in throughout: a DSN without the
parameter is the single-schema layout, byte for byte what it was.

## Async hosts

`Areev` is **blocking and drives its own Tokio runtime** — a current-thread
runtime it `block_on`s for every operation. Tokio will not start a runtime from
a runtime worker, so a blocking handle must not be opened, called, *or dropped*
on the executor. The drop is the half that gets missed: it fires at shutdown or
at the end of a test, long after code that looked correct.

Since 1.9.1 neither case panics. A blocking open on a worker returns
**`STO-E010`** naming the way out, and the store's teardown relocates its
runtime to a plain thread rather than panicking. Both are safety nets, not the
recommended shape — a relocated teardown is a slow drop.

Pick by what the host needs:

| The host needs… | Use |
|---|---|
| The raw store, async | `areev_store::AsyncAreev` |
| The **governed** facade, async — authorization, `PrincipalSession`, `set_grants`, `authz_epoch`, CAL under a session | `areev_cal::AsyncFacade` (1.9.1) |
| The blocking API, from an async process | open and call inside `tokio::task::spawn_blocking`, or on a plain thread |

`AsyncFacade` exists because `AsyncAreev` wraps the store only, so an async
service that also authorizes had no async-safe owner and hand-rolled one
(#322). It takes a **closure** rather than mirroring each facade method,
because `PrincipalSession<'f>` borrows its facade and cannot cross an `.await`
— so a whole request runs inside one closure, on one blocking thread:

```rust
use areev_cal::AsyncFacade;

let f = AsyncFacade::open("agent.db", Some("ops")).await?;

let answer = f.with(|facade| {
    let session = facade.principal_session("user:amy")?;   // borrows `facade`
    let ex = areev_cal::CalExecutor::new(Default::default());
    // …every statement in this request runs under amy's fail-closed rights…
    facade.authz_epoch()
}).await?;

f.close().await?;          // teardown off the executor; dropping also works
```

Notes:

- **Clones share one facade** and calls serialise on it (the store is
  `&mut`-driven). Callers queue asynchronously rather than occupying blocking
  threads, so a burst cannot exhaust the host's blocking pool.
- **`close()` is optional but explicit.** Dropping the last handle tears down
  off the executor too; `close().await` is for when teardown must have
  *happened* — graceful shutdown, copying the `.db` file, the end of a test.
- **`from_facade`** takes a facade the host built itself (read-only mounts, an
  installed embedder). Build it off the executor; this only takes ownership.
- Per-principal rights are worth caching: `resolve_rights(p)` + `session_with`
  (#324) give a host a set it can key by `(principal, authz_epoch)` instead of
  re-reading grants under the store mutex on every request.

## Decision backends (optional)

A decision (System One) backend scores and orders recall — typed questions
in, calibrated probabilities out, no text generated. It is **off unless the
host configures it**; with nothing configured every path is the deterministic
rule and nothing leaves the process. The contract is
[decision-model-proposal.md](decision-model-proposal.md); the egress and key
handling are in [security-model.md](security-model.md#decision-backends-egress);
the how-to is the cookbook's "Decision backends" section.

The host passes an **ordered chain** — `--decide <entry>,<entry>,…` (env
`AREEV_DECIDE`), optionally `--decide-cmd <cmd>` (env `AREEV_DECIDE_CMD`)
appended as the last entry. Entries are tried in order under one deadline
(`--decide-timeout-ms`, env `AREEV_DECIDE_TIMEOUT_MS`, default 2000); when
all fail, the deterministic rule answers.

| Spec entry | Endpoint | Key env | Notes |
|---|---|---|---|
| `typesafe:<model>` | `$TYPESAFE_BASE_URL` or `https://api.typesafe.ai` + `/v1/systemone` | `TYPESAFE_API_KEY` | US-hosted; ZDR enterprise only |
| `openrouter:<model>` | `$OPENROUTER_BASE_URL` or `https://openrouter.ai/api` + `/v1/systemone` | `OPENROUTER_API_KEY` | model `jev-1.13` / `jev-latest`; no TypeSafe account |
| `vercel:<model>` | `https://ai-gateway.vercel.sh/typesafe/v1/systemone` | `AI_GATEWAY_API_KEY` | model `typesafe-ai/jev`; BYOK/ZDR |
| `openjev:<model>` | `https://api.openjev.sh/v1/systemone` | `OPENJEV_API_KEY` | model `openjev`; unaffiliated, token-funded proxy — **development only** |
| `cloudflare:<model>` | `https://api.cloudflare.com/client/v4/accounts/$CLOUDFLARE_ACCOUNT_ID/ai/run/<model>` | `CLOUDFLARE_API_TOKEN` + `CLOUDFLARE_ACCOUNT_ID` | model `typesafe/jev`; ZDR |
| `systemone:<url>[#model]` | `<url>/v1/systemone` (or as given if it already ends in `/systemone`) | `AREEV_DECIDE_API_KEY` (optional) | self-hosted: LiteLLM `/typesafe`, von, kev, jev-rs, jev-sim, oido, chakuho; default model `jev-latest` |
| `llm:<llm spec>` | Areev's existing LLM providers | as today | emulated; `calibrated = false` — may reorder, never omit |
| `--decide-cmd <cmd>` | stdin wire request → stdout wire response | none | no shell; 300 s default; always last in the chain |

**Region and residency.**

| Situation | Chain to use |
|---|---|
| No residency requirement | any hosted entry; `openrouter:` needs no TypeSafe account |
| EU / UK / CA residency | `cloudflare:` (ZDR) or `vercel:` with BYOK + ZDR, or self-hosted `systemone:` |
| Air-gapped | self-hosted `systemone:` (von / kev / jev-rs / oido) or `--decide-cmd` |
| No decision model reachable at all | `llm:<local or regional LLM>` (rank-only) then the deterministic floor |
| Development | `openjev:openjev` (free) — never for customer memories |

**Latency.** A hosted backend answers in 70–500 ms. It never sits in the 50 ms
voice-loop gate (`voice_loop` example): a voice host either leaves the backend
off or bounds recall with `--recall-deadline-ms` (env
`AREEV_RECALL_DEADLINE_MS`); a backend that misses its deadline fails open to
the RRF order. Only a
local encoder clone (sub-25 ms) qualifies for that path, and it is still
opt-in.

## SSO note (trusted-header mode)

The proxy shared secret (`--sso-secret-env`) is an **impersonation-grade
credential**: whoever holds it can present any identity header, including
approval-capable principals. Guard it exactly like an admin token (secret
manager, per-instance rotation), terminate it at the same proxy that does
the IdP handshake, and never reuse it across environments. The identity a
proxy asserts still only gets what the FILE grants it — but the file
cannot tell a real IdP assertion from a forged one once the secret leaks.

**Rotating it does not require a zero-overlap cutover.**
`--sso-secret-env-next VAR` opens a window in which **either** secret proves
the proxy, so the fleet moves over one node at a time and the old value is
retired once nothing presents it — TLS key rotation's shape. The console
prints a warning on every start while the window is open, because a rotation
left half-finished is an extra impersonation-grade credential live in
production. The procedure, for both a planned rotation and a suspected leak
(where the answer is a hard cutover, **not** a window), is
[runbooks/sso-secret-rotation.md](runbooks/sso-secret-rotation.md).

**Because that secret is impersonation-grade, a proxy-asserted identity may
not approve.** `areev ui --sso-approvals` defaults to `deny`: an SSO identity
keeps every read and review but is refused at `POST /api/run/respond`, because
an approval whose trust root is a fleet-wide shared secret produces an audit
record indistinguishable from a genuine one. Approvers should hold a
per-principal credential (`--auth`). `--sso-approvals allow` accepts the
trade-off explicitly, and is defensible when the proxy↔console hop is itself
trustworthy — a Unix socket, or mTLS, with the secret never leaving the host.
The console cannot verify that, which is why it cannot be the default. A
**group-derived** principal (`--sso-groups-header`) may never approve at all,
under any setting: a role identifies nobody who can be asked why.

**Prefer a channel-bound proof to a static header secret.** The strongest
form of this deployment puts the proxy and the console on the same host and
connects them over a Unix domain socket or mTLS, so filesystem permissions or
a client certificate — not a copyable string — are what prove the proxy. The
shared secret remains supported and is what the flags configure; it is simply
the weakest of the three.

## What to hand the reviewer

- This document.
- `docs/gdpr.md` (article → capability map) and `docs/erasure.md`.
- An `areev audit export` sample from a staging file.
- The FORGET-SUBJECT demo: `crates/areev-store/tests/subject_report_tests.rs::
  report_matches_erasure_selection_exactly` — erasing an identity erases its
  memories, their supersession history, and their index rows, with a
  receipt, through the *same selector* the DSAR report uses, so "show me
  everything" and "delete it" are two calls over one selection. The CAL-level
  mirror is `crates/areev-cal/tests/erasure_cal_tests.rs`.

## Native OIDC (non-default `oidc` build)

For deployments with nowhere to run an authenticating proxy, or where people
approve HITL asks through the console and the approver's identity must be
stronger than a shared secret can carry, `areev ui --oidc-*` runs the
authorization-code+PKCE flow in-process and issues an `HttpOnly`
`SameSite=Strict` session cookie. **An OIDC principal may approve by default**
— its identity was proven by a signature verified against the issuer's
published key set, which is exactly what `--sso-approvals` exists to
substitute for. Setup: [runbooks/oidc-setup.md](runbooks/oidc-setup.md).

Sessions are in-process: restarting the console logs everyone out, and nothing
auth-related is ever persisted in a memory file (invariant 5). Areev is an
OIDC **client**, never an authorization server.

## Explicitly not in this profile

SAML, SCIM / directory sync, user provisioning, MFA, RBAC beyond the grant
vocabulary, org-level audit aggregation. A partner who requires them signs
first; the profile above is what design-partner pilots run on.
