# Deploying an agent example

The canonical container guide is [`docs/docker.md`](../../../docs/docker.md)
— one image, `postgres` + `tls` compiled in, a `console` role and a
`heartbeat` role, and the cloud table (AWS/GCP/Azure/K8s). This page is only
the agent-shaped residue: what an example like
[`invoice-to-accounting/`](../invoice-to-accounting/) needs when it leaves
the laptop.

## What actually needs scheduling

An Areev agent has **no daemon**. Two one-shot commands run on a cadence:

| Cadence | Command | What it does |
|---|---|---|
| Every 1–5 min | `agent ingest` (= `areev trigger run` / `db.trigger_run(...)`) | evaluate due triggers: poll the mailbox, dedup, start runs, park asks; also when parked runs get answered by your reply-reading path |
| Nightly (or per-push in CI) | `agent improve` (= `areev loop run`) | the deterministic analyzers over the agent's own journals → recommendations for a person |

Anything that can run a command on a schedule works: cron, launchd, systemd
timers, a CI schedule, or the image's `heartbeat` role — which is exactly a
shell loop of `areev trigger run` with `AREEV_HEARTBEAT_SECS` as the tick.

## The storage decision — embedded Turso vs Postgres

**One memory per agent** either way; the backend decides *who can hold it
at once* ([`docs/docker.md`](../../../docs/docker.md) §"One memory, one
writer").

**Embedded (a `.db` file + `.blobs/` sidecar — the default).**
Microsecond reads, zero infrastructure, and an **exclusive file lock**: one
process holds the memory at a time. The heartbeat, the console, and a
manual `areev` command on the same file take turns (`STO-E001` when they
don't). This is the laptop/single-VM shape and what the example smokes use.

```bash
docker run -d -v ap-desk:/data -v "$PWD:/work:ro" \
  -e AREEV_RUN_TOOL_CMD="/work/python/agent.py tools" \
  -e AREEV_RUN_CONNECTOR_CMD="/work/connectors/outlook_graph.py" \
  areev heartbeat --ns org.ops --max-usd 2.00
```

**Postgres (`--db 'postgres://…?schema=ap_desk'`, pgvector).**
One schema per agent; any number of processes share it — heartbeat, console
with its HITL approval queue, and your app, all concurrently; writers
serialize at `reserve_write`. This is the server tier: pick it when the
console must stay up while the heartbeat runs, or on platforms with no
durable disk (Cloud Run, Fargate). Mind the honest caveats in docker.md:
plaintext DSN (keep it on a private network), 1–2 connections per handle,
bootstrap schemas at provision time. A few embedded-only surfaces
(`blob get` offline reads, vault-backed anonymization) differ on Postgres —
check [`docs/deployment-profile.md`](../../../docs/deployment-profile.md)
before committing.

Fleet note (ten agents, one box): agents share the image and the cluster,
never a writable memory. `docker/compose.fleet.yml` in the repo root is the
running two-agent example.

## The pieces an agent deployment carries

- **The memory** (volume or schema) — the agent itself: plan, tools,
  saved queries, triggers, journals, lessons. Back this up; the code around
  it is replaceable.
- **Your scripts, mounted read-only** — tools + connector run via `/bin/sh`
  *inside* the container: mount them and point `$AREEV_RUN_TOOL_CMD` /
  `$AREEV_RUN_CONNECTOR_CMD` at the mounted paths.
- **Secrets in the environment, never argv** — `--credential NAME=ENV_VAR`
  / `--token-env` name variables, mapping directly onto your platform's
  secret manager; better, `cmd:`/`vault:` resolvers mint per-call tokens.
- **The console, with auth** — `areev ui --allow-remote --token-env
  AREEV_UI_TOKEN` behind TLS; per-principal credentials (`--auth`) if
  approvals happen in the console, because `run.respond` refuses
  shared-token callers.
- **The loop policy file** — the only place auto-apply can be granted;
  version it next to the seeder ([`examples/policy/`](../../policy/)).
