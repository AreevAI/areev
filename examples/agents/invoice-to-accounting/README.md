# invoice → accounting

An accounts-payable mailbox. Invoices arrive, get extracted, and land in the
expense sheet — except the ones a person has to look at first, which park until
somebody says yes.

Nothing here needs a credential, a network, or a model key. That is the point:
the whole thing runs from committed fixtures, so CI proves it on every release
and it cannot rot quietly between them.

```bash
cargo build --release -p areev     # once, from the repo root
cd examples/agents/invoice-to-accounting
./smoke.sh
```

A few seconds later:

```
2 posted, 1 refused, 1 approval recorded against a named person.
```

## What just happened

```
                         ┌── under 2,500 ─────────────────────────┐
                         │                                        ▼
invoice ─▶ parse ─▶ extract ─▶ validate ─▶ ask ─▶ [ a person ] ─▶ post ─▶ reply
             │                                         │
             │ no text layer                           │ rejected
             ▼                                         ▼
          FAILED                                 reply, never post
```

Three invoices go in:

| Fixture | What it is | What the run does |
|---|---|---|
| `01-under-threshold.json` | 860 USD of freight | Posts itself. Nobody is woken up for a small, confident row. |
| `02-needs-approval.json` | 4,400 USD of software | **Parks.** A person approves it, and their name goes on the posted row. |
| `03-scanned-page.json` | A photographed invoice | **Fails.** The parser gets no text and says so, rather than posting a blank row. |

The third one is the one worth staring at. A pipeline that "handles" an
unreadable attachment by extracting nothing writes a row of nulls into your
books. This one stops.

## The four things this is actually demonstrating

**A plan is data, so it travels.** `plan.mgb` is an ordinary memory bundle
carrying seven tool definitions and the workflow that binds them. `smoke.sh`
imports it into an empty file. Its content address is
`fc991baf…` on every machine, because a plan is a grain and a grain is its
bytes.

**The approver cannot be the requester.** `smoke.sh` tries to approve the
parked run as `agent:ap-intake` — the principal that *started* it — and asserts
that the runtime refuses. Separation of duties is a property of the operation,
not a policy someone remembers to enforce.

**The threshold is a memory, not a constant.** `amount_at_or_above:2500_usd
route_to human_review` is stored as a fact. That is what makes it something
`areev loop` can later propose changing, with a written reason and a rollback.

**Afterwards, it is queryable.** `areev run-trace --run-id large` returns the
run's journal, out of the same file the vendor terms live in. There is no
separate log to join against.

## The pieces

```
plan.mgb        the workflow + its 7 tool definitions, as a portable bundle
tools.py        the host tools: JSON on stdin, JSON on stdout, one process per call
fixtures/       three invoices — one clean, one over the threshold, one unreadable
smoke.sh        the whole thing, with assertions. Non-zero on drift.
```

`tools.py` is the only file you would replace to make this real. Point
`append_sheet` at your accounting API instead of a JSONL file, and the plan,
the journal, the approval gate, and the audit trail do not change. If that API
needs a token, hand it to the run rather than the tool:

```bash
areev run start --workflow <WF> --run-id inv-1 --tool-cmd ./tools.py \
  --credential books=BOOKS_TOKEN \
  --allow-host 'https://books.example.com' \
  --tool-egress 'append_sheet:books:POST'
```

The tool gets a broker address and a capability token, never the secret — and a
tool with no grant gets nothing.

## Where to go next

- [`docs/run.md`](../../../docs/run.md) — the runtime: journal, checkpoints,
  resume, budgets, `verify`
- [`docs/triggers.md`](../../../docs/triggers.md) — how the mailbox wakes this
  up on a cadence, without a daemon
- [`docs/loop.md`](../../../docs/loop.md) — turning the corrections a human
  makes here into proposed changes

## Regenerating the plan

`plan.mgb` is built from the same seeder as the README's demo memory, on a
fixed clock so its content address does not move:

```bash
cargo run --release -p areev-store --example seed_accounting_demo -- /tmp/plan.db --plan-only
areev bundle --db /tmp/plan.db --out examples/agents/invoice-to-accounting/plan.mgb
```
