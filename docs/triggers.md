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

`webhook` and `manual` fire through `areev trigger deliver` rather than
`trigger run`: the host owns the listener and hands Areev the payload.

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

## Outbound control and credentials

Two controls, neither of which needs an isolation runtime — and which together
address the thing a sandbox does not.

**A deny-by-default allowlist.** Declare `int:allowed_outbound_hosts` in a
trigger's config, using Fermyon Spin's semantics:

```json
{ "int:allowed_outbound_hosts": ["https://gmail.googleapis.com",
                                 "https://*.googleapis.com"] }
```

Scheme, host and port all take part in the match. `*.example.com` covers
subdomains but not the apex and not `evil-example.com`. A bare `*` is refused:
it would let a declaration look policed while permitting the whole internet.
Omitting the key entirely is unrestricted — and reported as such, so "no policy"
never masquerades as a policy.

**Credential brokering.** Pass `--credential gmail=GMAIL_TOKEN_VAR` and the
connector gets `AREEV_EGRESS_URL` in its environment instead of a token. It
posts the call it wants:

```json
{ "url": "https://gmail.googleapis.com/gmail/v1/users/me/messages",
  "method": "GET", "credential": "gmail" }
```

and the broker checks the allowlist, attaches the credential, and makes the
request. The token never enters the connector's process. Posta's CB4A calls this
Model A; Cloudflare shipped it in April 2026, Deno in February, and it is the
whole of Nango's product.

Why this rather than a sandbox: a polling connector legitimately needs the
network *and* the credential, so isolation does not constrain what actually goes
wrong. The January 2026 n8n community-node compromise exfiltrated decrypted
OAuth tokens, and the malicious node never violated a sandbox — it read a
credential it was given and made a request it was allowed to make. Zapier runs
each task in a Firecracker microVM and that attack still works.

**The honest limits.** This raises the bar; it is not a boundary. Exfiltration
*through* an allowed host still works — encode data into a draft, a label, a
filename. Hostname allowlisting cannot see through DNS tricks or domain
fronting. And a brokered connector cannot use a vendor SDK, because the SDK
wants its own sockets; that is the same trade Nango makes.

## Webhooks without a listener

Areev never opens a port. The host already terminates TLS and authenticates the
sender — it is far better at both than a memory engine would be — and hands the
payload over:

```bash
areev trigger deliver --db accounting.db --ns accounting --id <TRIGGER> < payload.json
```

Idempotent on the same terms as a poll: the payload's dedup value mints the run
id, so a webhook delivered twice — which every provider does — produces one run
and one recorded skip.

Note `--payload -` does *not* mean stdin here. The argument parser treats a
following token beginning with `-` as another flag, so omit the flag and pipe
instead.

## Putting it on a heartbeat

```bash
areev trigger render --db accounting.db --ns accounting --target cron
areev trigger render --db accounting.db --ns accounting --target k8s-cronjob
```

Targets: `cron`, `launchd`, `systemd`, `k8s-cronjob`. These are **templates,
not API clients** — Areev holds no cloud credentials and creates no cloud
resources. You apply the output with whatever you already use.

The rendered interval is the **greatest common divisor** of your declared
intervals, floored at 60s — deliberately coarser than your shortest trigger.
The memory owns the real cadence; rendering a 30-second cron because one trigger
asked for 30 seconds would put the schedule back in the crontab, which is the
thing this feature exists to stop.

## In the console

A read-only Triggers tab lists what is declared. Firing stays in the CLI on
purpose: it spends budgets and executes effects, so the actor's identity is the
audit record, and "whoever holds the console token" is not an identity — the
same reason `run.respond` refuses a shared-token caller.

Whether a trigger has actually fired is per-host state that does not replicate,
so a console cannot know it for another machine. Use `areev trigger status` on
the machine that evaluates them.

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
