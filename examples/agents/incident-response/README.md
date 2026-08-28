# incident response

An on-call desk that is **woken, not polled**. A monitoring system posts an
alert; the agent correlates it to the service, recalls what past incidents on
that service turned out to be, proposes a remediation — and parks for a human
before anything touches production. After the incident is resolved it writes
down the cause, so the next identical page arrives with its history attached.

Every other agent example in this repo polls a source on a heartbeat. This one
has **no connector at all**, because a push source is not asked — it tells you:

```
 beacon                     the areev incident desk                    on-call
───────►  YOUR listener  ──deliver──►  classify ─► recall ─► propose ──┐
 POST     (TLS, sender      webhook        │        runbook     │      │
 /alerts   auth, 200)       trigger        │                    ▼      │
                                           │            below the wake  │
   the same POST again ──────────────────► │            floor? record   │
   duplicates 1, runs 0                    │            and sleep       │
                                           ▼                     ▲      ▼
 an engineer replaying ──deliver──► (same plan, new run)         │  [ a person ]
 by hand                 manual                                  │  apply | record only
                         trigger                                 │      │
                                                                 │      ▼
                                            close ◄──────────── record  apply
                                          (incident              only   remediation
                                           record)                        │
                                                          the step cannot execute?
                                                          FAIL the run, loudly
```

**Areev never opens a port.** Your host already terminates TLS and
authenticates the sender — it is far better at both than a memory engine would
be. It hands the payload over, and everything after that hand-off is plan
nodes:

```python
db.trigger_deliver(webhook_trigger, request.body, tool_cmd=...)
```

And the second loop, the one that makes it *self-improving* — scheduled, not
magic: every run, every decision and every failure is already a grain in the
agent's memory, so improvement is CAL queries over its own record:

```
 runs, decisions, failures ──► areev loop (deterministic analyzers,
      (the run journals)        scheduled: cron / heartbeat / CI)
                                      │ Recommendation + evidence, by hash
                                      ▼
                                [ a person ] ──approve/reject, signed──► new facts
                                      ▲                                        │
 next page's context ◄── the trigger's declared context_query ◄────────────────┘
```

Nothing here needs a credential, a network, or a model key: the whole thing
runs from committed fixtures, so CI proves it on every release.

## Run it

| Stack | Needs | Run |
|---|---|---|
| [`python/`](python/) | `pip install areev` | `python/smoke.sh` then `python/improve.sh` |

Or everything at once (what CI runs): [`../run-smokes.sh`](../run-smokes.sh).

A few seconds later:

```
OK -- 1 production action by name, 1 loud failure, 1 redelivery ignored, 1 self-approval refused.
OK -- 1 cause became memory, 1 pattern found in 9 runs, 1 decision signed by name.
```

## Week one — `smoke.sh`

Three alerts arrive from `beacon`, the (fictional) monitoring system:

| Alert | What it is | What the run does |
|---|---|---|
| `01` checkout-api, 5xx rate 42%, **critical** | a real outage | **Pages.** The runbook says *scale*; rhea reads the graph, **overrides** it to *rollback*, and signs why. Her name is on the production row. |
| `02` notify-worker, cert expires in 27d, **info** | below the wake floor | **Closes itself.** Recorded, nobody woken — the gate guards production, not the inbox. |
| `03` ledger-sync, replication lag 812s, **critical** | a runbook step that cannot run | **Pages, is approved, and then FAILS.** The rollback is refused by the platform: a release freeze pins that service's deploy channel. The run fails loudly and the incident stays open. |

Four governance properties are asserted, not narrated:

- **A delivery starts a run; an identical redelivery starts nothing.**
  `duplicates: 1, runs_started: 0` — which matters because every alerting
  vendor retries, and a duplicate page that restarts a remediation is how a
  bad night becomes a bad week.
- **A payload naming no occurrence is reported, not dropped.** The sender
  renames its id field; the delivery comes back `unidentifiable: 1` and is
  journaled. A silently swallowed alert has no symptom.
- **One alert, two doors, two runs.** A `manual` trigger on the *same plan*
  lets an engineer replay an incident by hand. Idempotency is per standing
  rule, which is exactly what makes a deliberate replay possible — and the
  replay is a governed run like any other, so imara closes it `record_only`
  and production is not touched a second time.
- **The desk cannot approve its own remediation.** It answers its own gate and
  the runtime refuses (`RUN-E012`): the responder principal structurally
  cannot be the one that started the run.

## Weeks two and three — `improve.sh`

A change freeze opens and the standing rule is **paused with a reason**;
deliveries are refused loudly rather than quietly accepted — the sender's
retry is the queue. Then the freeze lifts and five more alerts land, including
the whole first batch replayed by the vendor (`duplicates: 3`).

And the payoff of week one shows up before any model or loop is involved. The
checkout 5xx alert returns under a new id. The cause rhea wrote down rides
back in through the trigger's declared `context_query`, so the proposal is no
longer a guess:

```
proposal: rollback (known) -- seen 1 time(s) on this service and signal;
last cause: connection pool exhausted by deploy 2026.08.24-3
```

**It still pages.** The payoff is a better proposal at the same gate, not a
removed human — a desk that auto-applies production actions because it has
seen the alert before is teaching the wrong lesson.

Then the desk reads its own record:

- **`areev loop run` finds the pattern**: `HIGH — Workflow fe5f922214b7
  failed 3/9 recent runs (33%): apply_remediation: … rollback refused —
  ledger-sync deploy channel is pinned (release freeze); the runbook step
  cannot execute`. Deterministic analyzers; no model key involved. (The smoke
  also tunes them to a low-volume desk with `set_analyzer_config` — at nine
  runs a fortnight the stock thresholds would stay silent for a quarter.)
- **The gate holds**: the engine refuses to apply its own advisory finding,
  and refuses any decision without a written reason. imara approves, naming
  the freeze and what has to change before that runbook step is offered again.
- **It does not nag**: a second pass stores nothing. The same evidence does not
  become a second recommendation.
- **It briefs itself out of its own memory** — `desk_pulse`, a saved CAL query
  stored *in the memory file*, assembles the plan, the tool definitions, the
  service catalog and the causes it has learned, under a token budget.

## What the fixtures exercise

| Fixture | Exercises |
|---|---|
| `alerts/01-checkout-5xx.json` | the gate; a human overriding the runbook's proposal |
| `alerts/02-notify-cert-expiry.json` | the wake floor — an alert that is recorded without paging anyone |
| `alerts/03,05,06-ledger-replication-lag.json` | a runbook step the platform refuses; three loud failures the loop clusters |
| `alerts/04-checkout-5xx-again.json` | **the memory payoff** — same shape, new id, recognized |
| `alerts/07-notify-queue-depth.json` | a human saying *no* to a proposed action, with a reason |
| `alerts/08-checkout-disk-usage.json` | second alert below the floor; nobody woken |
| `malformed-alert.json` | a delivery naming no occurrence → `unidentifiable`, journaled |
| `services.json` | the service catalog, seeded as facts — including the pinned deploy channel the remediation tool checks |
| `decisions/00-desk-approves-itself.json` | separation of duties (`RUN-E012`) |
| `decisions/01…07` | apply / record-only / override, each with a written reason and, once resolved, the cause worth keeping |

`ALERT_UPTO` is the clock: the host's receiver replays alert fixtures whose
two-digit prefix is ≤ it (`03` in week one, `08` in week two).

## What this example is teaching

**A push source needs no connector.** The seven other things an inbound
integration usually drags in — a poller, a cursor, a backoff, a "have I seen
this" table, a dead-letter queue, a schedule, and a daemon to run it — collapse
into one call and one declaration:

```python
db.trigger_add(json.dumps({
    "kind": "webhook",
    "workflow": plan_hash,
    "dedup_key": ["/alert_id"],                      # the "have I seen this" table
    "context_query": "incident_ctx($service = /service)",
}), "beacon posts every alert here; one run per alert", ns)
```

The dedup key mints the run id, so idempotency is a property of the
declaration rather than code you remember to write. There is no cursor because
there is nothing to resume, no backoff because nothing is being asked, and no
daemon because nothing is waiting.

**Two triggers, one plan.** The webhook and the manual replay are separate
standing rules pointing at the same content-addressed Workflow. That is why
the console has no Triggers page: the binding is trigger → plan, and a flat
list cannot show that two rules start one plan.

**The gate sits on the production-action path, not on the inbox.** An alert
below the wake floor is classified, recorded and closed without waking anyone;
only a proposal that would *do* something reaches a human. Wake floors are
facts in memory (`oncall wake_severity warning`), not constants in the agent —
which is what lets the loop propose moving one without a redeploy.

**A remediation that cannot execute must fail loudly.** The `apply_remediation`
tool checks the service's `deploy_channel` — a fact the run was handed through
the trigger's declared context — and exits non-zero when the runbook step is
unexecutable. Three nights of that is a cluster the loop can find; a
remediation that quietly does nothing is invisible until the postmortem.

**The audit record is the approver's identity.** Every production row carries
the engineer who decided, their written reason, what the desk proposed, and
what was actually applied — so an override is legible as an override.

**Retrieval ships inside the agent.** `incident_ctx` and `desk_pulse` are
`DEFINE QUERY` rows in the memory file; the trigger *declares* its context and
the evaluator assembles it at delivery time (a tool inside a run cannot open
the file its own run holds). Tune the recipe with `DROP QUERY` + `DEFINE` — no
redeploy, and the queries replicate with the memory.

## Going live

The receiver and the tools are the only fake parts.

1. **The listener** — replace `agent.py listen` (which replays a directory of
   fixtures) with your HTTP endpoint. Terminate TLS, verify the sender's
   signature, then call `trigger_deliver` with the raw body and return `200`.
   Retries are free: the hand-off is idempotent on the dedup key. See
   ["Webhooks without a listener"](../../../docs/triggers.md) in the trigger
   reference.
2. **The tools** — replace the `tools` subcommand's handlers with processes
   that call your platform (a deploy API for rollback, an autoscaler for
   scale, your ticketing system for `record_only`). Keep each one idempotent
   on the alert id, and keep the `apply_remediation` guard: no named human,
   no production action. Hand credentials to the *run*, not the tool —
   `--credential NAME=ENV_VAR` + `--allow-host` broker them so the token
   never enters the tool process.
3. **The gate** — the smoke maps engineers to principals directly. In
   production an approval deserves a per-principal credential (`areev ui
   --auth`, where `run.respond` refuses shared-token callers) and grants in
   the file. A shared on-call token identifies nobody who can be asked why.
4. **The catalog** — the service facts are seeded from `services.json` here.
   In production they come from your service registry on a schedule, which is
   a second (polling) trigger on a second plan.
5. **Schedule the improvement pass** — `agent improve` nightly. Set
   `LOOP_LLM_CMD` to any [`../../llm/`](../../llm/) backend and the pass adds
   verified LLM reflection over the same CAL-assembled context —
   DISCOVER→GROUND→VERIFY, every model finding grounded in grains before it can
   become a recommendation. Cron, launchd, or the repo's Docker image with its
   `heartbeat` role: [`../docs/deploy.md`](../docs/deploy.md).

## The pieces

```
python/agent.py            the whole agent, one file, embedded Areev
smoke.sh, improve.sh       the two acts -- language-neutral assertions,
                           driven through each stack's 3-line wrapper
fixtures/alerts/           eight synthetic alerts ("ALERT_UPTO" is the clock)
fixtures/decisions/        on-call decisions, each with a written reason
fixtures/services.json     the service catalog
fixtures/malformed-alert.json   a payload that names no occurrence
```

## Where to go next

- [`../../how-to-create-an-areev-agent.md`](../../how-to-create-an-areev-agent.md)
  — the assembly manual this example follows
- [`../../../docs/triggers.md`](../../../docs/triggers.md) — the eight trigger
  kinds, `deliver`, dedup keys, and declared context
- [`../../../docs/run.md`](../../../docs/run.md) — the runtime: journal,
  budgets, asks, `verify`
- [`../../../docs/loop.md`](../../../docs/loop.md) — analyzers, gates, the
  recommendation lifecycle
- [`../invoice-to-accounting/`](../invoice-to-accounting/) — the same shape
  driven by a **polling** source, in three languages
