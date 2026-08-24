# examples/agents

Vertical agents built on Areev — end-to-end workflows where Areev is the memory
and the runtime, and the business system on either end is somebody else's.

The rest of [`examples/`](../) teaches **one seam at a time**: a policy file, the
`--llm-cmd` protocol, the analyzer contract. These teach a **job**: mail arrives,
a workflow runs, a human approves (and corrects, by reply, until they do), a
system of record gets written, and what happened is recallable afterwards —
then the loop reads those runs back and proposes a fix a person signs.

## The index

| Agent | Job | Status |
|---|---|---|
| [`invoice-to-accounting/`](invoice-to-accounting/) | Poll AP mailboxes for two clients, extract invoices, park on human approval, take corrections by email reply around a bounded cycle, post to the expense sheet — then brief itself out of its own memory and propose fixes a person approves | **runnable ×3** — the same agent in Python, TypeScript, and Rust, one file each, minting one content-addressed plan; `<lang>/smoke.sh` then `<lang>/improve.sh`, all keyless |
| `insurance-documents/` | Read inbound policy/claim documents, extract coverage facts, route exceptions to a person, keep the client's coverage picture current | planned |
| `rcm-optimization/` | Revenue Cycle Management: watch claim denials, cluster the denial reasons per payer, propose coding/submission fixes with the denials as evidence | planned |
| `nda-red-flags/` | Read an inbound NDA, flag clauses against the red flags this memory has accumulated, produce a reviewable summary | planned |

New agents follow the same shape; the plan is roughly ten of these, each in
all three languages. Start from the invoice agent — its
[`CLAUDE.md`](invoice-to-accounting/CLAUDE.md) is the working contract.

## One agent, three languages

Each agent ships as parallel single-file stacks — `python/agent.py`,
`typescript/agent.mts`, `rust/src/main.rs` — each embedding Areev through
its own binding, all exposing the same subcommands. The agent-level
`smoke.sh`/`improve.sh` hold the assertions **once**; per-language wrappers
are three lines. Because every seeder pins `created_at`, all stacks mint the
identical plan hash — [`run-smokes.sh`](run-smokes.sh) asserts it, so a
stack cannot silently drift from its siblings.

## What every agent here is made of

Three seams, one shape — JSON on stdin, JSON on stdout, one process per
invocation:

| Leg | Seam | Contract |
|---|---|---|
| **Inbound** — what wakes it up | a `polling` / `webhook` Trigger + a connector | [`docs/triggers.md`](../../docs/triggers.md), providers: [`docs/email-providers.md`](docs/email-providers.md) |
| **Work** — what it does | a Workflow grain run by `areev run`, host tools via `--tool-cmd` | [`docs/run.md`](../../docs/run.md) |
| **Model** — where judgment needs language | `--llm-cmd`, optional | [`../llm/`](../llm/) |

So an agent example adds **no dependencies to this repo**. A vendor SDK, if you
need one, lives in your copy of the connector script — never here. That is the
same posture the core takes (no clap, no HTTP framework, no MCP SDK), applied
to examples.

## Two paths, always

- **Keyless** — mock connector, mock tools, committed fixtures. No credentials,
  no network, no model key. This is what the smokes and CI run, and it is the
  reason these examples stay correct across releases instead of quietly
  rotting. Run them all: [`run-smokes.sh`](run-smokes.sh); how it works:
  [`docs/testing.md`](docs/testing.md).
- **Live** — real connectors and real tools, opt-in behind env vars
  ([`docs/email-providers.md`](docs/email-providers.md)); deployment,
  scheduling, and the embedded-vs-Postgres decision:
  [`docs/deploy.md`](docs/deploy.md).

## Where the human goes

Each one parks on a `client` node — an approval a person answers with
`areev run respond --as <principal>` (here: by replying to an email) —
because these are agents that spend money or sign things. The principal who
triggered the ask structurally cannot answer it (separation of duties), and
the approver's identity *is* the audit record. A correction the human makes
on the way to "yes" is recorded as memory, which is where self-improvement
actually starts. An agent example that approves its own work is teaching the
wrong lesson.

## Adding one

Layout, the keyless-floor rule, the no-new-dependency rule, and the indexes
that need updating in the same commit are in the `areev-examples` skill
(`.claude/skills/areev-examples/`); the test contract is
[`docs/testing.md`](docs/testing.md).
