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
3. **The multi-principal credential map.** One OS process = one
   principal. Governed deployments bind a principal per service
   (`facade.bind_principal` / `--as`), grants live IN the file as
   `mg:permits` Facts, and the run verbs are Control-tier: `run.execute`
   / `run.respond` (approver ≠ initiator, structurally), `run.cancel`
   deliberately low. Erasure follows the same grants (`delete` / `erase`
   verbs) — provisioning is `areev`-CLI statements, auditable in the file
   itself.
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

**A schema migration is not rolling-deploy safe.** The bootstrap advisory
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

The motivating consumer is `areev ui --read-only`: paired
with #124 (the console no longer displays its own DSN), a read-only console
instance never needs — and never holds — write authority over the memory it
renders. The same open is reachable from the bindings —
`areev.Areev(dsn, read_only=True)` and `new Areev(dsn, …, readOnly)` (#183) —
so an embedded console, evaluator or analytics reader is handed the
SELECT-only role rather than the owner's credential.

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
