# invoice → accounting

**The problem.** Accounts payable runs on email. Invoices land in a
mailbox, approvals crawl through reply chains, and every correction an
approver makes dies in someone's sent folder — the same category mistake
gets made, caught and re-corrected month after month, and nobody can say
afterwards who approved what, or why.

**What you get.** Invoices posted to the books with a named approver and a
written reason on every row, corrections taken **by reply** around a
bounded cycle until the approver says yes — and a desk that reads its own
history on a schedule and proposes its own fixes, which a person signs, so
next month's invoices are categorised from what was signed rather than
re-corrected by hand.

The desk itself: vendors send PDF invoices to a mailbox; the agent parses
them, extracts expense rows, emails the approver, takes corrections by
reply, posts the approved row to a spreadsheet.

```
 vendor                        the areev invoice agent                     approver
───────►  ap mailbox  ──poll──►  parse ─► extract ─► validate ──clear──►  post row ─► reply
 "invoice   (O365 /   trigger      │                    │                  to sheet    to
  attached"  Gmail)                │ no text layer      │ needs review        ▲       vendor
                                   ▼                    ▼                     │
                                 FAILED,          email the ask ─────►  [ a person ]
                                 loudly                 ▲    "approve | revise | reject"
                                                        │                     │
                                                        └── apply corrections ┘
                                                            (bounded cycle, ≤3)
```

And the second loop, the one that makes it *self-improving* — scheduled, not
magic: every correction and every failed run is already a grain in the
agent's memory, so improvement is CAL queries over its own record:

```
 runs, corrections, tool calls ──► areev loop (deterministic analyzers,
        (the journal)               scheduled: cron / heartbeat / CI)
                                          │ Recommendation + evidence, by hash
                                          ▼
                                    [ a person ] ──approve/reject, signed──►  new facts,
                                          ▲                                   new aliases,
   next run's context ◄── saved CAL queries assemble the lessons back ◄───────┘
```

Nothing here needs a credential, a network, or a model key: the whole thing
runs from committed fixtures, so CI proves it on every release. The live
mailbox connectors ([`connectors/`](connectors/)) are opt-in behind env vars.

## Run it — pick your language

The same agent is implemented three times, **one file per language**, each
embedding Areev in-process through its own binding. All three expose the
same subcommands, are driven by the same two act scripts, and — because
every seeder pins `created_at` — **mint the identical plan hash**: three
languages, one content-addressed agent.

| Stack | Needs | Run |
|---|---|---|
| [`python/`](python/) | `pip install areev` | `python/smoke.sh` then `python/improve.sh` |
| [`typescript/`](typescript/) | node ≥ 22.6, `npm i @areev/areev` (in-tree: a built `crates/areev-js`) | `typescript/smoke.sh` then `typescript/improve.sh` |
| [`rust/`](rust/) | a Rust toolchain | `rust/smoke.sh` then `rust/improve.sh` |

Or everything at once (what CI runs): [`../run-smokes.sh`](../run-smokes.sh).

A few seconds later:

```
OK -- 3 posted, 1 refused, 1 correction round-tripped, 2 approvals signed by name.
OK -- 1 correction became memory, 1 pattern found in 9 runs, 1 decision signed by name.
```

## Week one — `smoke.sh`

Four invoices arrive across two client mailboxes:

| Mail | What it is | What the run does |
|---|---|---|
| acme / Meridian Freight, 860 USD | small and confident | **Posts itself.** Nobody is woken up. |
| acme / Cobalt Cloud, 4,400 USD | over the client's threshold | **Parks.** Dana approves by reply; their name goes on the row. |
| acme / a photographed page | no text layer | **Fails, loudly.** A silent empty extraction posts a blank row; this one stops. |
| brightco / "Cobolt Cloud", 1,900 USD | misspelled vendor, low confidence | **Parks.** Priya replies `revise` + `Vendor: Cobalt Cloud`; the run goes **around the cycle** — corrections merged, re-asked — then `approve` posts the corrected row. |

Three governance properties are asserted, not narrated:

- **The agent cannot approve its own ask.** The desk emails itself an
  approval and the runtime refuses — the responder principal structurally
  cannot be the one that started the run.
- **Redelivery is one run, not two.** Another heartbeat tick over the same
  mailbox starts nothing (`--dedup-key /message_id`).
- **The correction became memory at the moment it was approved**: the alias
  `Cobolt Cloud → Cobalt Cloud` lands as a fact in `org.brightco.vendors`,
  and the correction itself is recorded as a failed `extract_rows` outcome
  the loop can cluster.

## Weeks two and three — `improve.sh`

More mail. Northgate keeps emailing photographs. And the payoff of week
one's correction shows up before any model or loop is involved: the same
misspelled vendor arrives again, the trigger's declared context now carries
the alias fact, extraction canonicalizes the name and settles the confidence
question — **the invoice that needed a person in week one posts itself in
week three.**

Then the desk reads its own record:

- **It briefs itself out of its own memory.** `desk_pulse` — a saved CAL
  query stored *in the memory file* — assembles the plan, the tool
  definitions, recent activity, and the lessons under a token budget. That
  briefing is what a scheduled improvement pass hands to a model: the agent
  describing its current setup — its queries, grains, and workflows — from
  the same file it runs on.
- **`areev loop run` finds the pattern**: `HIGH — Workflow 3da2300c1296
  failed 4/9 recent runs (44%): parse_attachments: … scanned image`. The
  analyzers are deterministic; no model key was involved. (The smoke also
  shows tuning them to a low-volume desk via `set_analyzer_config` — at
  four invoices a week, the stock thresholds would stay silent a quarter.)
- **The gate holds**: the engine refuses to apply its own advisory finding,
  and refuses any decision without a written reason. A person approves,
  signing name and reasoning; the lesson is recorded against the vendor; a
  second loop run proposes nothing — the same evidence does not nag.

## What this example is teaching

**A plan is data and travels as its hash.** The workflow (9 nodes, a
client-gated approval, a `max_cycles`-bounded correction cycle) is authored
as a grain via JSON `add` — from Python, TypeScript, and Rust — and all
three seeders produce the same content address. `run-smokes.sh` asserts it.

**Three keys, three jobs** (conflating them is the classic modeling error):

```
run_id     = message id      one governed run per inbound email
session_id = thread id       the conversation, spans runs
subject    = vendor/invoice  the thing itself, spans threads
```

**Namespaces are the multi-tenant design.** The desk serves many clients;
each client's knowledge lives in its own subtree, and the read side scopes
by prefix:

```
org.ops                 the runtime lane: plan, tool definitions, triggers,
                        run journals, raw mail events    (never policied!)
org.acme                client rules (the review threshold is a FACT here)
org.acme.vendors        aliases, payment terms, lessons
org.brightco            second client, same shape
org.brightco.vendors
```

One query reads the whole desk (`WHERE namespace = "org.*"` — what
`extract_ctx` and the loop do); one client is `"org.acme.*"`; **writes,
grants, retention and erasure take exact namespaces only** — a wildcard
never widens a destructive surface. Under a bound principal the prefix
expansion fails closed against the session's grants.

**Keep operational grains out of policied namespaces.** The example sets an
egress-anonymization policy (audit mode) on the client subtrees and
deliberately **never** on `org.ops` — a rewriter that turns dates into
`[DATE_1]` and 64-char hashes into `[PERSON_1]` breaks plans, bindings, and
every piece of operational JSON it touches. Content namespaces get policy;
the ops lane does not.

**Retrieval and presentation ship inside the agent.** `extract_ctx` (what
extraction gets to know: skill instructions, desk facts, the email thread)
and `desk_pulse` are `DEFINE QUERY` rows in the file itself; the trigger
*declares* its context (`--context-query "extract_ctx($session = /thread)"`)
and the evaluator assembles it at fire time. Tune the prompt recipe with
`DROP QUERY` + `DEFINE` — no agent redeploy, and the queries replicate with
the memory.

**The tools and the mailbox are subprocess seams.** `agent tools` and
`agent connector` are JSON-on-stdio, one process per invocation — the same
contract in all three languages, and the reason the mock and the live
connectors are interchangeable.

## Going live

The mock connector and tools are the only fake parts. To make it real:

1. **Mailbox** — swap the mock for a live connector:
   [`connectors/outlook_graph.py`](connectors/outlook_graph.py) (Microsoft
   365 / Outlook, the default most desks want) or
   [`connectors/gmail.py`](connectors/gmail.py) (Google Workspace — the
   pattern proven by a production deployment). Both are stdlib-only,
   env-gated, and return attachments as blobs the evaluator stores in the
   CAS. Setup, auth, and the payload contract:
   [`../docs/email-providers.md`](../docs/email-providers.md).
2. **Tools** — replace the `tools` subcommand's mock handlers: `pdftotext`
   for parse, a model call for extract (`temperature 0`, strict JSON), your
   spreadsheet API for post (Microsoft Graph workbook append, Google Sheets
   `values:append`). Keep each handler idempotent on `row_key`. Hand
   credentials to the run, not the tool: `--credential NAME=ENV_VAR`
   + `--allow-host` broker them so the token never enters the tool process.
3. **Reply classification** — the deterministic classifier here (verb +
   `Field: value` lines, quoted text stripped, marker in the subject) is the
   floor; add an LLM interpretation leg only for replies it cannot classify,
   and leave genuine questions unactioned for a person.
4. **Schedule both loops** — the ingest heartbeat (`agent ingest` every
   couple of minutes) and the improvement pass (`agent improve` nightly).
   Set `LOOP_LLM_CMD` to any [`../../llm/`](../../llm/) backend and the
   nightly pass adds verified LLM reflection over the same CAL-assembled
   context — DISCOVER→GROUND→VERIFY, every model finding grounded in
   grains before it can become a recommendation.
   Cron, launchd, or the repo's Docker image with its `heartbeat` role —
   including the embedded-vs-Postgres storage decision:
   [`../docs/deploy.md`](../docs/deploy.md).
5. **Tighten identity** — the smoke maps email senders to principals; in
   production, approvals deserve per-principal credentials (`areev ui
   --auth`, where `run.respond` refuses shared-token callers) and grants in
   the file (`GRANT run.respond ON org.ops TO "user:dana" …`).

## The pieces

```
python/  typescript/  rust/   the same agent, one file each, own binding
smoke.sh, improve.sh          the two acts -- language-neutral assertions,
                              driven through each stack's 3-line wrapper
fixtures/mail/<client>/       nine synthetic invoices ("MAIL_UPTO" is the clock)
fixtures/replies/             approve / revise / reject replies, with markers
connectors/                   the LIVE mailbox pollers (env-gated, never CI)
```

## Where to go next

- [`../../how-to-create-an-areev-agent.md`](../../how-to-create-an-areev-agent.md)
  — the assembly manual this example follows
- [`../../../docs/run.md`](../../../docs/run.md) — the runtime: journal,
  budgets, asks, `verify`
- [`../../../docs/triggers.md`](../../../docs/triggers.md) — the trigger
  model and the connector contract
- [`../../../docs/loop.md`](../../../docs/loop.md) — analyzers, gates, the
  recommendation lifecycle
- [`../../../docs/cal-reference.md`](../../../docs/cal-reference.md) — CAL,
  `DEFINE QUERY`, templates, ASSEMBLE
