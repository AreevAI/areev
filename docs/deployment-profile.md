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
answer when the proxy is also pooling or doing IAM auth — point the DSN at it
with `sslmode=disable`.

**Connections per handle: 1, or 2 with telemetry.** One `tokio_postgres`
client per `Areev` handle, plus a second for the recall-telemetry sidecar. The
**bindings default telemetry to `aggregate`**, so a stock Node or Python handle
opens **two**. Pass `telemetry="off"` when you do not want the sidecar. A
multi-tenant host with one memory per tenant multiplies this by tenants *and*
by instances against the server's `max_connections` — cache handles per tenant
with an LRU and close idle ones (`close()` in Node; drop in Rust/Python), or
put a pooler (PgBouncer in transaction mode, Cloud SQL Auth Proxy) in front.
There is no built-in pool.

**Open cost: provision schemas ahead of the request path.** First open of a
NEW schema runs the full DDL bootstrap under an advisory lock — hundreds of
milliseconds, fine as a provisioning step and unacceptable inside a request.
Steady-state open of an existing, populated schema is tens of milliseconds.
Create the tenant's schema when the tenant is created, not on their first turn.

**Outage recovery: automatic for reads, explicit for writes.** A managed
Postgres restarts, fails over, and drops connections as routine maintenance.
The handle now replaces a dead session in place — clearing the
prepared-statement cache and the BM25 stats cache, both of which belonged to
the dead session — and:

- a **read** is replayed transparently: it had no effect, so running it twice
  is running it once;
- a **write** is **not** replayed. The connection may have died *after* the
  server committed it, and nothing client-side can tell the difference, so
  replaying risks applying it twice. The call returns its error (`STO-E001`,
  carrying the Postgres SQLSTATE — `57P01` for an administrator restart) and
  the handle is usable again on the next call. **Your host must treat a failed
  write as a unit of work to redo**, exactly as it would any other transaction
  failure.
- nothing is replayed **inside an open transaction**: the transaction died
  with the connection, so the whole unit has to be re-run.

Conformance: `a_handle_recovers_from_a_database_outage` in
`areev-conformance`, run against a real server with a real
`pg_terminate_backend`.

**Writers: concurrent, not single.** See control 4 above — `STO-E002` is never
raised on this backend, and concurrent writers block and serialise at
`reserve_write` rather than erroring.

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

## Explicitly not in this profile

Native OIDC (SSO v0 trusted-header mode, above, is in this profile — full
OIDC/SAML integration is not), RBAC beyond the grant vocabulary, org-level
audit aggregation — all Wave 6. A partner who requires them signs first; the
profile above is what design-partner pilots run on.
