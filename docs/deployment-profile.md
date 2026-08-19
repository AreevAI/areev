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
│      └→ areev hub  --token-env AREEV_HUB_TOKEN    (127.0.0.1)      │
└───────────────┬──────────────────────────────────────────────────┘
┌ storage ─────────────────────────────────────────────────────────┐
│  single-tenant: per-team memory FILES (encrypted at rest,        │
│      open_encrypted / AREEV_PASSPHRASE)                            │
│  multi-tenant:  PostgreSQL backend — one memory = one schema,    │
│      CONCURRENT writers per schema, pgvector                     │
└──────────────────────────────────────────────────────────────────┘
```

## The five controls, in order

1. **Token auth on every shared surface.** `areev ui --token-env VAR`
   requires the token on every request (browser Basic prompt or
   `Authorization: Bearer`); `areev hub` makes `--token-env` mandatory.
   Tokens come from the environment — never the command line, never a
   file in the repo.
2. **TLS at a terminating proxy.** The console and hub are std-only
   HTTP/1.1 on loopback; production exposure goes through caddy/nginx
   with TLS and the loopback bind left as-is. (Native TLS is Wave 6;
   the proxy is not a workaround, it is the profile.)
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

## SSO note (trusted-header mode)

The proxy shared secret (`--sso-secret-env`) is an **impersonation-grade
credential**: whoever holds it can present any identity header, including
approval-capable principals. Guard it exactly like an admin token (secret
manager, per-instance rotation), terminate it at the same proxy that does
the IdP handshake, and never reuse it across environments. The identity a
proxy asserts still only gets what the FILE grants it — but the file
cannot tell a real IdP assertion from a forged one once the secret leaks.

## What to hand the reviewer

- This document.
- `docs/gdpr.md` (article → capability map) and `docs/erasure.md`.
- An `areev audit export` sample from a staging file.
- The FORGET-SUBJECT demo: `adapters/areev-crewai/tests/test_backend.py::
  test_forget_subject_erases_sourced_memories` — erasing an identity
  erases its memories, their supersession history, and their index rows,
  with a receipt, through the same selector the DSAR report uses.

## Explicitly not in this profile

Native TLS, OIDC/SSO, RBAC beyond the grant vocabulary, org-level audit
aggregation — all Wave 6. A partner who requires them signs first; the
profile above is what design-partner pilots run on.
