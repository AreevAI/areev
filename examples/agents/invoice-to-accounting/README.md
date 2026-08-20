# invoice → accounting

> **Status: placeholder.** This README is the specification; the scripts and
> fixtures are not written yet. Nothing here runs. See
> [Intended layout](#intended-layout) for what lands, and
> `.claude/skills/areev-examples/` for the rules any implementation must meet.

An accounting mailbox is polled on a cadence. Each new message is checked for an
invoice; anything above a threshold parks for a human; approved invoices are
written to the accounting system exactly once. What the agent did, and why, is
recallable afterwards.

This is the worked version of the snippet in
[`docs/triggers.md`](../../../docs/triggers.md) — the same accounting mailbox,
carried all the way through to a system of record.

## What it demonstrates

| | |
|---|---|
| **Triggers** | a `polling` trigger whose cadence is data in the memory, not a crontab — evaluated by a one-shot command safe to run concurrently |
| **Idempotency** | the run id derives from `(trigger, connector, dedup value)`, so a replayed mailbox page produces one run and one recorded skip — not a double payment |
| **HITL** | an approval node above a threshold, answered by a **different principal** than the one who raised it |
| **Memory** | vendor terms, prior invoice numbers, and past corrections accumulate as grains, so the second invoice from a vendor is handled better than the first |
| **Governed egress** | the accounting credential is attached by the broker on the way out; the connector never holds the token |

## The shape

```
polling trigger  ──▶  extract  ──▶  check     ──▶  approve   ──▶  post
(gmail connector)     (host tool)   (threshold)    (client)       (host tool)
                                        │                            │
                                        └── under threshold ─────────┘
```

- `extract`, `check`, and `post` are **host tools** — they run through
  `--tool-cmd`, input JSON on stdin, result JSON on stdout.
- `approve` is a Tool grain with `executor_kind: "client"`. The run parks; a
  person answers with `areev run respond --as user:controller`; `areev run
  resume` finishes it.
- The edge from `check` carries a condition in the frozen grammar
  (`amount_over_threshold == true`), so the small invoices skip the gate and the
  large ones cannot.

## Running it (once implemented)

**Keyless** — no credentials, no network, no model key. This is what CI runs:

```bash
./smoke.sh          # fixtures → runs → asserts the posted payloads
```

**Live** — same plan, real connector and tools:

```bash
export GMAIL_TOKEN=... ACCOUNTING_TOKEN=...
areev trigger run --db accounting.db --ns accounting \
  --connector-cmd ./connectors/gmail.sh \
  --tool-cmd ./tools/accounting.sh \
  --credential accounting=ACCOUNTING_TOKEN
```

## Intended layout

```
trigger.sh                   areev trigger add --type polling --observer gmail
                             --scope 'mailbox:accounts@example.com' --interval 120
                             --dedup-key /message_id --workflow <WF_HASH>
plan.py                      the Workflow grain: nodes, edges, bindings, retries
connectors/gmail-mock.sh     reads fixtures/mailbox/, emits {items, cursor, more}
connectors/gmail.sh          the real one, env-gated
tools/accounting-mock.sh     records posts to a file instead of the network
tools/accounting.sh          the real one, env-gated
fixtures/mailbox/            synthetic messages, incl. a replay and a malformed one
fixtures/expected/           what a correct run posts
smoke.sh                     the keyless end-to-end; non-zero on drift
```

## Deliberate edge cases in the fixtures

A happy path alone would not prove much. The fixture set must include:

- the **same message delivered twice** — proves one run, one skip, one payment;
- an invoice **just over** and one **just under** the approval threshold;
- a **malformed** message — the run fails visibly rather than posting garbage;
- a **second invoice from a known vendor** — the leg where memory earns its
  place.

## Not in scope

No OCR and no PDF parsing in this tree. `extract` is a host tool: the mock reads
structured fixtures, and the real one is wherever your extraction already lives.
The example teaches the seams and the governance, not a document pipeline.
