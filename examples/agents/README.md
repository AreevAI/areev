# examples/agents

Vertical agents built on Areev — end-to-end workflows where Areev is the memory
and the runtime, and the business system on either end is somebody else's.

The rest of [`examples/`](../) teaches **one seam at a time**: a policy file, the
`--llm-cmd` protocol, the analyzer contract. These teach a **job**: mail arrives,
a workflow runs, a human approves, a system of record gets written, and what
happened is recallable afterwards.

## The index

| Agent | Job | Status |
|---|---|---|
| [`invoice-to-accounting/`](invoice-to-accounting/) | Poll an accounting mailbox, extract invoices, park on human approval above a threshold, write to the accounting system — then read its own runs back and propose a fix a person approves | **runnable** — `./smoke.sh` then `./improve.sh`, both keyless |
| `nda-red-flags/` | Read an inbound NDA, flag clauses against the red flags this memory has accumulated, produce a reviewable summary | planned |

## What every agent here is made of

Three seams, one shape — JSON on stdin, JSON on stdout, one process per
invocation:

| Leg | Seam | Contract |
|---|---|---|
| **Inbound** — what wakes it up | a `polling` / `webhook` Trigger + a connector | [`docs/triggers.md`](../../docs/triggers.md) |
| **Work** — what it does | a Workflow grain run by `areev run`, host tools via `--tool-cmd` | [`docs/run.md`](../../docs/run.md) |
| **Model** — where judgment needs language | `--llm-cmd`, optional | [`../llm/`](../llm/) |

So an agent example adds **no dependencies to this repo**. A vendor SDK, if you
need one, lives in your copy of the connector script — never here. That is the
same posture the core takes (no clap, no HTTP framework, no MCP SDK), applied
to examples.

## Two paths, always

Every agent runs in two modes, and the first one is the one that matters here:

- **Keyless** — mock connector, mock tools, committed fixtures. No credentials,
  no network, no model key. This is what `smoke.sh` runs and what CI runs, and
  it is the reason these examples stay correct across releases instead of
  quietly rotting.
- **Live** — the real connector and the real tools, opt-in behind env vars.

## Where the human goes

Each one parks on a `client` node — an approval a person answers with
`areev run respond --as <principal>` — because these are agents that spend money
or sign things. The principal who triggered the ask structurally cannot answer
it (separation of duties), and the approver's identity *is* the audit record.
An agent example that approves its own work is teaching the wrong lesson.

## Adding one

Layout, the keyless-floor rule, the no-new-dependency rule, and the indexes that
need updating in the same commit are in the `areev-examples` skill
(`.claude/skills/areev-examples/`).
