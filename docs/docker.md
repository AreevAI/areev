# Areev in Docker

The repo ships a [`Dockerfile`](../Dockerfile) that packages the one `areev`
binary with the two non-default features a container deployment wants already
compiled in: **`postgres`** (the server tier — `--db
postgres://…?schema=<name>`) and **`tls`** (native rustls for deployments
with nowhere to run a terminating proxy). Nothing else changes: the image
adds no daemon, no scheduler, and no new verbs — with one deliberate
exception, the image-provided `heartbeat` command below.

```bash
docker build -t areev .
docker run --rm areev --help
```

Build it as `areev:latest` deliberately: that is the image name
`areev trigger render --target k8s-cronjob` has always emitted — this image
is what that CronJob template runs.

## Sixty seconds, containerized

No Rust toolchain, no `cargo install` — a named volume is the memory:

```bash
docker run --rm -v areev-data:/data areev add john prefers "window seat"
docker run --rm -v areev-data:/data areev recall john --render sml
```

`$AREEV_DB` defaults to `/data/areev.db` inside the image, so one-shot verbs
need no `--db` flag; override it with a path or a `postgres://` DSN per
container. The console, with a token:

```bash
AREEV_UI_TOKEN=$(openssl rand -hex 16) docker compose --profile console up
# → http://127.0.0.1:7437  (any username, password = the token)
```

## One image, two roles

| Role | Command | What it is |
|---|---|---|
| **Console** | `ui --addr 0.0.0.0:7437 --allow-remote --token-env AREEV_UI_TOKEN` | the web console — browse, query, the run approval queue |
| **Heartbeat** | `heartbeat [trigger-run args…]` | an image-provided loop of one-shot `areev trigger run` evaluations ([`docker/heartbeat.sh`](../docker/heartbeat.sh)) |

Two container-specific notes, both about honesty rather than mechanics:

- **Inside a container the server must bind `0.0.0.0`**, and Areev refuses a
  non-loopback bind without `--allow-remote`. Passing it moves the exposure
  control to the Docker port publish: keep `-p 127.0.0.1:7437:7437` and put a
  TLS-terminating proxy in front before publishing wider — the same
  [deployment profile](deployment-profile.md) as bare metal, with the
  container boundary standing in for loopback. `--token-env` is optional on
  `ui` (a token-less console is read-only beyond loopback concerns), but a
  published port with no token is an open memory — set one.
- **`heartbeat` is provided by the image, not the binary.** The trigger
  design is "no daemon: cadence is data, evaluation is a command"
  ([triggers.md](triggers.md)) — a container's natural dumb heartbeat is a
  shell loop, so the image carries one. `AREEV_HEARTBEAT_SECS` (default 60,
  the render floor) sets the tick; everything after the word `heartbeat` is
  passed to `areev trigger run` verbatim, and host config rides the
  environment exactly as it would on a cron line:

```bash
docker run -d -v areev-data:/data -v "$PWD/tools:/work:ro" \
  -e AREEV_RUN_TOOL_CMD=/work/tools.sh \
  -e AREEV_RUN_MODEL=claude-sonnet-5 -e ANTHROPIC_API_KEY \
  areev heartbeat --ns accounting --max-usd 0.25 --ask-ttl 3600
```

Tool and connector scripts run via `/bin/sh` *inside* the container — mount
them (read-only) and point `$AREEV_RUN_TOOL_CMD` / `$AREEV_RUN_CONNECTOR_CMD`
at the mounted path, not at a path on the host that rendered the config.

## One memory, one writer — what it means in containers

The embedded backend takes an **exclusive OS file lock** (inside the pinned
Turso engine). A second *process* opening the same `.db` — even for a pure
read — fails at open with `STO-E001`; a second handle inside one process is
`STO-E002`. The one lock-free door is `areev blob get`, which reads CAS
attachments without opening the memory.

Containers make this visible immediately, because every role is its own
process. On an embedded file:

- **one container owns the memory at a time** — the root
  [`docker-compose.yml`](../docker-compose.yml) encodes this as profiles
  (`--profile console` *or* `--profile heartbeat` against one volume);
- while the heartbeat executes a fired run, the evaluator holds the file, so
  the run's tools cannot open it — the same rule as bare metal
  ([triggers.md](triggers.md) §"what a trigger-started run can see");
- `docker exec` into a serving console and running `areev add` on the same
  file gets `STO-E001`, by design — write through the console or stop it
  first.

This is not a Docker limitation to engineer around; it is the isolation
model. When you genuinely need the console, a heartbeat, and app instances
holding one memory **concurrently**, that is the definition of the server
tier: move the memory to the Postgres backend, where any number of processes
share a schema, writers serialize at `reserve_write`, and the trigger claim
protocol exists precisely so concurrent evaluators produce exactly one
firing.

## The fleet: many agents, one infrastructure

**One memory per agent** is the rule that keeps agents from contradicting
each other ([how-to-create-an-areev-agent.md](../examples/how-to-create-an-areev-agent.md)):
the memory is the unit of isolation, erasure, sync, and portability, so two
agents on the same box can never race each other's heads, poison each other's
recall, or block each other's erasure. Namespaces partition *within* an
agent; memories separate *between* agents. Concretely:

- **Embedded fleet (one box):** one volume + one heartbeat container per
  agent, a console container started against whichever memory you are
  inspecting. Agents share the image, the host, and nothing else.
- **Postgres fleet (shared cluster):** one schema per agent
  (`?schema=agent_billing`, `?schema=agent_support`), every role concurrent.
  [`docker/compose.fleet.yml`](../docker/compose.fleet.yml) is a running
  example — two agents, one Postgres, console + heartbeats all live at once.
  Adding an agent is adding a schema and a heartbeat service.
- **Cross-agent reads** go through read-only `ASSEMBLE` facade mounts
  (`--mount org=/data/org.db`) — never a shared
  writable memory. Mount paths are file-backend today: on a Postgres fleet,
  share knowledge by exporting a bundle from the source memory and following
  it as a local read-only replica.
- **Separation of duties survives co-location:** one process = one principal
  (`--as`), grants live in each memory as `mg:permits` Facts, and an
  approver structurally cannot be the initiator — so a worker agent and its
  reviewer can run on one host without the host becoming the trust boundary.
- **Budget the connections** on a shared Postgres: one handle is 1–2
  connections (telemetry sidecar), there is no built-in pool, and first open
  of a new schema runs the DDL bootstrap — provision schemas when the agent
  is created, not on its first turn
  ([deployment-profile.md](deployment-profile.md)).

## Cloud

The image is the deployment unit everywhere; the real decision is the
backend. A stateless platform has no durable disk, so it *is* the server
tier ([ARCHITECTURE.md §11](../ARCHITECTURE.md#11-deployment-topology)):
managed Postgres with the pgvector extension. A platform with a persistent
volume can stay embedded — cheaper and microsecond-fast, at the price of the
one-writer rule above.

| Platform | Long-running role (console) | The heartbeat | Memory |
|---|---|---|---|
| **AWS** | ECS/Fargate service | EventBridge Scheduler → ECS scheduled task, or a heartbeat service | RDS PostgreSQL (pgvector); embedded on EC2/EBS for a single-writer box |
| **GCP** | Cloud Run service | Cloud Scheduler → Cloud Run job | Cloud SQL for PostgreSQL (pgvector) — Cloud Run has no durable disk, so the server tier is the fit by design |
| **Azure** | Container Apps | Container Apps job (cron trigger) | Azure Database for PostgreSQL Flexible Server (allowlist `vector`) |
| **Kubernetes** | Deployment (one per served memory) | `areev trigger render --target k8s-cronjob` — emits this image's name | Postgres, or a RWO PersistentVolumeClaim per embedded memory |
| **Self-hosted** | the compose files in this repo | the `heartbeat` service | named volumes, behind a caddy/nginx TLS proxy |

Three honest caveats before you wire production:

- **The Postgres connection is plaintext today** (`tokio-postgres` with
  `NoTls`). Keep the database on a private network with the containers, or
  route through a TLS-wrapping local proxy (Cloud SQL Auth Proxy, PgBouncer
  with TLS upstream). Do not point a DSN across the open internet.
- **There is no official published image yet.** `docker build` from a
  release tag and push to the registry your platform pulls from (ECR /
  Artifact Registry / ACR). Multi-arch: `docker buildx build --platform
  linux/amd64,linux/arm64` — the engine's C pieces compile natively under
  emulation.
- **Secrets ride the environment, never the command line** — `--token-env` /
  `--key-env` / `--credential NAME=ENV_VAR` all name variables, so they map
  directly onto your platform's secret manager. Variables named by
  `--token-env`/`--passphrase-env`/`--credential` are scrubbed from tool
  subprocess environments.

## See also

- [`deployment-profile.md`](deployment-profile.md) — the reviewed production
  shape these containers render: auth, TLS, principals, the Postgres
  connection contract
- [`triggers.md`](triggers.md) — the trigger model the heartbeat evaluates
- [`security-model.md`](security-model.md) — trust boundaries of the console
- [`quickstart.md`](quickstart.md) — every other install path
