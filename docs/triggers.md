# Triggers

A trigger is a standing rule that starts a workflow. It is declared as a grain,
so the cadence lives in the memory rather than in someone's crontab, and it is
evaluated by a one-shot command that is safe to invoke concurrently.

There is still **no daemon and no scheduler**. The OS scheduler stays a dumb
heartbeat; the memory decides what is actually due — the same posture
`areev loop run --if-stale` already takes, and the same one anacron and
systemd's `Persistent=true` take.

## The three parts

| Part | Lives in | Replicates |
|---|---|---|
| **Declaration** — what to watch, how often, what to start | a `Trigger` grain (`0x0D`) | yes |
| **Evaluation state** — next due, cursor, lease, fence | a `trg:<hash>` row in the store's `meta` table | **no** |
| **Firing record** — what fired, what it produced | an Observation in `agent:triggers`, plus Events and runs | yes |

The split is deliberate. Replicating evaluation state is wrong twice over: two
synced hosts would ping-pong on each other's watermark, and a dev memory
restored from prod would inherit prod's cursor and silently skip real work while
reporting success. Same reasoning that keeps a saved query's `last_run_at` local.

## Kinds

Eight declared kinds over four irreducible primitives.

| `--type` | Fires when | Primitive |
|---|---|---|
| `interval` | every N seconds since the last firing | Time |
| `schedule` | a cron expression matches (UTC) | Time |
| `once` | a single absolute instant passes | Time |
| `polling` | a connector reports new items | Time + Poll |
| `memory` | grains matching a predicate appear | State predicate |
| `webhook` | the host delivers a payload | Push |
| `manual` | an operator fires it | Push |
| `composite` | a boolean expression over other triggers is satisfied | meta |

`memory`, `webhook` and `composite` are accepted and validated now; they fire in
a later release. Declaring one today records intent without doing anything,
which `trigger status` reports rather than hiding.

**Rate drifts, cron does not.** `--interval` anchors to the *last firing*, so a
three-minute job on a five-minute interval leaves a two-minute gap and the
effective period depends on runtime. `--cron '*/5 * * * *'` anchors to
wall-clock boundaries. Both are correct; pick knowingly.

## Getting started

```bash
# Declare: what to watch, how often, and what to start.
areev trigger add --db accounting.db --ns accounting \
  --type polling --observer gmail \
  --scope 'mailbox:accounts@example.com' \
  --interval 120 --workflow <WF_HASH> --dedup-key /message_id \
  --because "poll the accounting mailbox for invoices"

# See what would happen. Touches nothing — the safe first command.
areev trigger run --db accounting.db --ns accounting --dry-run

# Evaluate for real.
areev trigger run --db accounting.db --ns accounting \
  --connector-cmd ./gmail-connector.sh --tool-cmd ./tools.sh
```

Then put `trigger run` on whatever heartbeat you already have. It can be much
coarser than your shortest interval: the command is cheap, and the memory
decides what is due.

## The connector contract

Same shape as `--tool-cmd`, because a connector **is** a tool: JSON on stdin,
JSON on stdout, one process per invocation. That means one contract to learn,
and connectors inherit the spawn hardening every host command gets — a wall-clock
ceiling, an output cap, and the withholding of any variable named by
`--passphrase-env` or `--token-env`.

**stdin**

```json
{ "trigger": "<hash>", "connector": "gmail",
  "scope": "mailbox:accounts@example.com",
  "cursor": "1802529", "max_items": 100,
  "config": { "int:cursor_field": "since" } }
```

**stdout**

```json
{ "items": [ { "id": "<dedup value>", "payload": { } } ],
  "cursor": "1802611", "more": false }
```

- **`cursor` absent means "leave it where it is."** It does not mean null, which
  would rewind the source.
- **`more: true` means there is a backlog.** The next invocation runs
  immediately instead of waiting out the interval, so a cold start drains
  without hammering.
- **A non-zero exit, a timeout, or non-JSON output is `TRG-E004`.** The claim is
  released, `next_due_at` is pushed out by an exponential backoff floored at the
  declared interval, and the failure is visible in `trigger status`. A broken
  connector backs off; it never hot-loops.

## Idempotency

The run id is derived from `(trigger, connector, dedup value)`, so the same item
always mints the same id — and `areev run start` refuses a duplicate. Connector
replay, overlapping cursors, and two nodes racing all produce **one run and one
recorded skip**.

That is why correctness here does not rest on the lease. The lease only stops
two nodes making the same expensive connector call; losing it costs an API call,
never a missed or duplicated firing.

`--dedup-key` takes JSON pointers. Several, joined in order, identify the
*occurrence* rather than the entity — `--dedup-key /id,/updated_at` treats an
edit as new work, which is how you get "changed" semantics from a source that
only reports "created".

**The first poll seeds and fires nothing.** It records the connector's current
position and stops. Otherwise declaring a mailbox trigger would start a run for
every message in history.

## Composing triggers

A `composite` fires when a boolean expression over its members is satisfied.
Members are declared under **aliases**, and the expression names the aliases —
a content address is not a legal identifier in any expression grammar, and an
alias reads better and survives a member being re-declared at a new address.

```
members:   invoice = <hash-a>, purchase_order = <hash-b>
predicate: (invoice = true AND purchase_order = true) OR escalation = true
correlate: /thread_id
window:    10m
```

The expression is written in CAL `WHERE` syntax and stored as a `Condition`
tree — a data structure, not a language. That is deliberate: the runtime's edge
grammar is frozen with a standing exclusion on expression languages, and new CAL
syntax is an OMS conformance decision, so Argo Events' `"(a && b) || c"` string
is the one shape that cannot be copied. Kestra's combinator-node form is the
compatible alternative, and Knative independently shipped its structural
filters ahead of its SQL dialect for the same stability reason.

**Correlation and windowing are one mechanism.** Partial matches are keyed by
`(trigger, correlation value)` and each carries its own expiry. Argo Events keys
satisfied dependencies per *sensor*, so Monday's `dep-a` can pair with Tuesday's
`dep-b`, and its only remedy is a wall-clock reset cron that silently does
nothing if the process is down at the reset instant. Keying by correlation with
a per-match expiry removes the need for a reset entirely.

A gate that names an undeclared member is refused with `TRG-E008`: it could
never be satisfied, so the trigger would be silently dead.

## Missed occurrences

Declared per trigger, using the vocabulary these systems settled on:

- `--catchup last` (default) — collapse everything missed into one firing. A
  laptop closed over a weekend wakes to one run, not four hundred.
- `--catchup none` — drop them.
- `--catchup all` — replay each one.

- `--concurrency forbid` (default) / `allow` / `replace`, from Kubernetes
  CronJob.

## Concurrency across hosts

`trigger run` is safe to invoke from several nodes against one memory. A claim
is a conditional write on the state row; exactly one caller wins, and the losers
report `skipped_locked` and exit 0 — that is the steady state, not a fault.

The fence lives inside the compared value, so a node that stalls past its lease
cannot write back behind whoever took over: its release matches no row and is
refused with `TRG-E005`.

## Timezones

**This release evaluates cron in UTC only.** A non-UTC `--timezone` is refused
with `TRG-E006` rather than mishandled.

That is deliberate rather than unfinished. Real implementations disagree about
what a cron expression means in a DST gap — AWS EventBridge and robfig/cron skip
the occurrence, Vixie cron fires it immediately — and they only agree on the
fall-back fold. Firing at the wrong local hour and being believed is worse than
refusing, so the choice is being made explicitly rather than guessed.

## What a trigger cannot do

A connector runs **as you, with your privileges**, exactly like `--tool-cmd`.
Areev does not sandbox it. See [`security-model.md`](security-model.md) for what
the seam does and does not guarantee.

## Errors

| Code | Meaning |
|---|---|
| `TRG-E001` | the declaration cannot fire as written |
| `TRG-E002` | `workflow` does not resolve to a Workflow grain |
| `TRG-E003` | due, but no connector command is configured |
| `TRG-E004` | the connector failed |
| `TRG-E005` | claim lost — the lease expired mid-firing |
| `TRG-E006` | invalid cron, or an unsupported timezone |
| `TRG-E007` | unknown render target |
| `TRG-E008` | a composite references an undeclared member |
| `TRG-E009` | the connector tried to reach a disallowed host |
| `TRG-E010` | the store failed underneath the evaluator |
