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

> **Why Areev Loop does the opposite.** The self-improvement engine persists its
> state *as a grain*, and that is also right — most of it is governance
> (`creators`, `audit_heads`, `status_index`) that the self-approval block reads,
> so it must travel with the file or a replica would let someone approve their
> own recommendation. And a replica skipping because a peer analysed the same
> shared memory minutes ago is correct, where skipping a mailbox poll is not.
> Governance replicates; cursors do not. See [`loop.md`](loop.md).

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

### The plan has to resolve before the trigger fires

`--workflow <WF_HASH>` binds the trigger to a plan, and **the plan's nodes must
already resolve** — each one either bound to a Tool Definition in `bindings`,
or matched by a Definition head whose `tool_name` equals the node id. A node
that is neither is **abstract**: legitimate, but it crosses the model boundary,
so it needs a tool-calling model at fire time (`--model`, or `$AREEV_RUN_MODEL`
on the heartbeat). Without one the firing fails with `RUN-E006` — see
[run.md](run.md).

The natural first attempt — declare a plan, point a trigger at it — therefore
used to fail at the *first firing* rather than at declaration, with
`trigger status` reporting `waiting` in between. Since 1.5.2 `trigger add`
says so up front:

```
$ areev trigger add --db ap.db --ns accounting --type interval --interval 900 \
    --workflow 4cb34f1a… --because "sweep"
warning: 1 node(s) in this plan are abstract — no binding and no matching tool
definition: gtm.send. They need a tool-calling model at fire time
(`areev trigger run --model ...`, or $AREEV_RUN_MODEL on the heartbeat);
without one the firing fails with RUN-E006
declared trigger a32027ab…
```

A warning, not a refusal: a plan can arrive by sync after the trigger is
declared, a Definition can be added later, and abstract nodes are perfectly
fine with a model configured.

### Re-declaring a plan mints a NEW plan

Grains are content-addressed over the **whole** `.mg` blob, and the blob's
header carries `created_at`. Two `add("workflow", …)` calls with identical
node/edge/binding JSON therefore return **different** hashes, one second apart.
There is no "re-add is free" — an idempotent-declare loop that re-adds on every
boot mints a new plan each time, while the trigger declared earlier still
points at the old one. Nothing reports an error; plans simply accumulate and
the cadence keeps running the version nobody is editing.

(Only a *byte-identical* blob collapses to a no-op, which in practice means the
same grain re-imported, not the same fields re-authored. Excluding `created_at`
from the address is not an option: canonical serialization is frozen — changing
it moves every content address ever computed and breaks OMS conformance.)

So declare idempotently by **recalling first**, keying on something stable:

```python
existing = json.loads(m.recall(grain_type="workflow", subject="gtm.send", limit=1))
wf = existing[0]["hash"] if existing else m.add(
    "workflow",
    json.dumps({"nodes": ["gtm.send"], "edges": [], "bindings": {}, "retries": {}}),
    "gtm.send",   # the subject is the stable identity; the hash is not
)
```

The same applies to the trigger itself. Give it a `name` — it is returned by
`trigger_list`, `trigger_status` and `areev trigger list`, and it is the one
identifier that survives re-declaring the plan.

## Declared context (`--context-query`)

A trigger-started run begins blind on the embedded backend: the evaluator
holds the memory while the run executes, so neither the connector nor the
run's tools can open it (#85). The trigger declares the fix:

```bash
areev trigger add --db accounting.db --ns accounting \
  --type polling --observer gmail --interval 120 --workflow <WF_HASH> \
  --context-query triage_ctx \
  --because "poll the mailbox; carry the triage context in"
```

`triage_ctx` is an ordinary saved query (`DEFINE QUERY "triage_ctx"() AS
{ … }` — it replicates with the file). At fire time the **evaluator** runs it
— read-only, against the memory it already holds — and places the result into
the run input as `context`, beside `trigger`/`connector`/`scope`/`item`. The
declaration replicates with the trigger grain, so *what a fired run gets to
see* is auditable, not host-local configuration.

Fail closed: a trigger that declared context never fires without it. A
missing query or a failed read refuses the firing (retried on the evaluator's
normal cadence) rather than starting a run without the context it promised.

### Binding parameters from the firing item (1.5.1, #92)

A parameterless query can carry durable knowledge but nothing about the item
that fired. The declaration may bind saved-query parameters from the item's
payload, using the same JSON pointers `--dedup-key` understands:

```bash
areev trigger add --db accounting.db --ns accounting \
  --type polling --observer gmail --interval 120 --workflow <WF_HASH> \
  --context-query 'triage_ctx($session = /session, $sender = /email/from)' \
  --because "carry the thread the message belongs to"
```

`triage_ctx` is then an ordinary *parameterized* saved query
(`DEFINE QUERY "triage_ctx"($session, $sender) AS { … }`), so a source like
`RECALL events WHERE session_id = $session RECENT 10` finally becomes
expressible on the trigger path. At fire time the evaluator resolves each
pointer against the item's payload and runs the query with those bindings —
still read-only, still against the memory it already holds. The whole
spelling is stored verbatim on the trigger grain, so the binding replicates
and audits like the rest of the declaration.

Fail closed, with `--dedup-key`'s precedent: a pointer that does not resolve,
or that lands on an object/array rather than a scalar, refuses the firing —
never a query with a hole in it. Bound values travel as a parsed AST, never
inside CAL text, so a hostile payload value cannot inject CAL. A malformed
spelling is refused at `trigger add`; a binding the query does not declare
warns at declaration and is ignored at run (`CAL-W006`).

On the PostgreSQL tier the lock constraint disappears (reads never block), so
tools *can* query the memory mid-run — `--context-query` remains useful there
for the auditability of the declaration and for plans that must stay portable
back to the embedded tier. See [run.md](run.md#backend-divergence-reading-the-memory-mid-run-85).

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

### Blobs: attachments without inlining (1.5.1, #93)

A connector cannot write the CAS itself — the evaluator holds the memory
while the connector runs — so an item may hand attachments back **through**
the response, and the evaluator (the party already holding the writer)
stores them:

```json
{ "items": [
    { "id": "<message-id>",
      "payload": { "email": { }, "attachments": [ { "filename": "inv.pdf", "blob": "@0" } ] },
      "blobs": [ { "filename": "inv.pdf", "mime": "application/pdf", "b64": "…" } ] } ],
  "cursor": "1802611", "more": false }
```

At fire time the evaluator `put_blob`s each entry (idempotent on content —
a re-delivered message costs only the transfer), rewrites every
`"blob": "@N"` payload reference to the resulting `cas://sha256:…` address,
and attaches a matching `content_refs` entry (uri / mime_type / size_bytes /
checksum, filename in metadata) to the Event it writes. The run receives
ordinary `cas://` addresses and its tools use `blob get` exactly as on the
host-driven path; GC, bundles, and erasure's sole-reference reclamation see
these blobs with no special case. Strings under a `blob` key that don't
start with `@` (e.g. an already-rewritten `cas://` address) pass through
untouched.

Budgets are enforced on **decoded** size — 16 MiB per item, 48 MiB per
response by default (`trigger run` inherits them from the evaluator's
options) — and a violation of any kind (over budget, undecodable `b64`, a
dangling `"@N"`) is **`TRG-E011`: the whole poll is refused with the cursor
unmoved**. Refusing loudly beats truncating — a silently dropped attachment
is an invoice posting without evidence, and a lost item is worse; the
connector gets fixed and the same page is re-polled.

## What the run receives

A trigger does not pass the item through as the run input. It wraps it, so a
run can always say what fired it:

```json
{ "trigger": "<hash>", "connector": "gmail", "scope": "mailbox:…",
  "item": { … the connector's or delivery's payload … } }
```

`run start` passes its `--input` through unchanged, so **one plan started both
ways sees two shapes**. Tools that must work either way should resolve it once:

```python
payload = payload.get("item", payload)
```

A tool that reads a top-level key without this dies on the trigger path only,
is retried per the node's `retries`, and the pass still reports
`runs_started: 1` with an empty `errors` list — the firing genuinely succeeded;
the run is what failed.

## Budgets

A firing starts a real run, so it takes the same ceilings `run start` does —
`max_tokens`, `max_usd_micros`, `max_wall_ms`, `ask_ttl_sec` and
`llm_max_tokens`, on both `trigger run` and `trigger deliver`. They are
optional and add no implicit limit when omitted.

```python
db.trigger_run(tool_cmd="./tools.sh", max_usd_micros=250_000, ask_ttl_sec=3600)
```

Budgets matter more here than anywhere else: a standing rule fires unattended,
so an unbounded run has nobody watching it, and an ask with no TTL parks
forever.

## The runner a firing gets (1.5.2, #90)

A firing builds **the same runner `run start` builds**. Whatever executes a
plan by hand executes it on a heartbeat:

| What | `areev trigger run` / `deliver` | Python / Node | Environment |
|---|---|---|---|
| Host tools | `--tool-cmd` | `tool_cmd` | `$AREEV_RUN_TOOL_CMD` |
| Polling connector | `--connector-cmd` | `connector_cmd` | `$AREEV_RUN_CONNECTOR_CMD` |
| Code-carrying tools (Tier C) | `--allow-executor` | `allow_executor` | `$AREEV_RUN_ALLOW_EXECUTOR` |
| Sandbox dispatch (`runtime`) | `--sandbox-cmd` | `sandbox_cmd` | `$AREEV_RUN_SANDBOX_CMD` |
| Prepared-code cache | `--executor-cache` | `executor_cache` | `$AREEV_RUN_EXECUTOR_CACHE` |
| Abstract nodes | `--model` / `--base-url` / `--key-env` | `model` / `base_url` / `key_env` | `$AREEV_RUN_MODEL` / `…_BASE_URL` / `…_KEY_ENV` |
| Outbound credentials | `--credential` / `--allow-host` / `--tool-egress` | `credentials_json` | — |
| Ceilings | the budget flags above | the budget arguments above | — |

Every one of these also reads its `$AREEV_RUN_*` variable, with the flag
winning when both are set: a heartbeat is a cron line, a launchd plist or a
k8s CronJob, not an interactive command, and an operator should not have to
re-edit the scheduler entry to pin an address.

Before 1.5.2 the trigger path built a deliberately reduced runner — a bare
`CommandExecutor` with no model — so a plan with a code-carrying node refused
at start with `RUN-E018` and one with an abstract node with `RUN-E006`, no
matter which flags were passed. `--context-query` and `runtime` shipped in the
same release and were meant to compose; used together the run refused, so an
agent could have declared context **or** sandboxed tools on the trigger path,
not both.

The pin is still the authorization, and it still comes from the **host**: a
grant living in the file would arrive in the same bundle as the code it
authorizes. What changed is where the host can state it, not who may.

A firing now also starts runs when there is no `--tool-cmd` at all — a plan
whose nodes are all pinned code, or all abstract, needs no subprocess. With
none of `--tool-cmd`, `--allow-executor` or `--model` the pass still ingests
items and records firings without executing, which stays a useful mode.

## Naming the workflow

`--workflow` takes the plan's **64-hex content address**, optionally prefixed
`sha256:`. Both spellings are accepted on write and normalized to the bare form
on the way in, so `trigger_list` returns what you declared and a round-trip
comparison matches. Anything that is not an address is refused at declaration
and reported as `unusable` by `trigger status` (`TRG-E002`) — before 1.5.2 a
`sha256:`-prefixed reference was accepted, listed, reported `waiting` forever,
and then died at fire time on `FMT-E001: invalid hex hash: Odd number of
digits`.

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

### What a firing records

Every firing writes one Observation in `agent:triggers` carrying `trigger`,
`kind`, `workflow`, `items`, `runs_started`, and `duplicates` — plus, when they
are not zero, `unidentifiable`, `ingested`, `failures`, and `seeded`.

Those conditional fields are what make the record self-explaining. A firing that
reports `items 5, runs_started 0, duplicates 0` and nothing else is a mystery,
and the zero duplicates actively misleads: it says the items were not skipped as
duplicates without saying why they were skipped. `unidentifiable 5` says the
connector emitted items whose declared dedup key resolved to nothing — a
connector bug, visible as one.

A **delivery** records on the same terms, including one that named nothing.
`trigger status` and the command's own output are per-host and transient; the
Observation replicates, and it is the audit record.

## Watching the memory itself

A `memory` trigger fires when grains matching a predicate appear. The predicate
is CAL `WHERE` syntax, and it sees the grain's own fields plus two envelope
fields, `grain_type` and `hash`:

```bash
areev trigger add --db crm.db --ns sales \
  --type memory --workflow <WF_HASH> \
  --where 'grain_type = "fact" AND relation = "signed_nda"' \
  --because "kick off onboarding when an NDA is recorded"
```

The content address is the item identity, so `--dedup-key` is not needed and the
same grain can never fire twice. **The first evaluation seeds at the head of the
op log and matches nothing** — the same reason a poll primes rather than
replaying a mailbox, since otherwise declaring a trigger on an established
memory would start a run for every historical grain that matches.

> **Watch `NOT` over a field the grain does not have.** Evaluation is total: a
> missing field compares false, so `NOT status = "processed"` is *true* for
> every grain with no `status` field at all. SQL would give you null here. On a
> read that means extra rows; on a memory trigger it means one run per changed
> grain, so name the type you mean (`grain_type = "fact" AND NOT ...`) rather
> than relying on a negation to exclude the grains you never meant to match.

## Composing triggers

A `composite` fires when a boolean expression over its members is satisfied.
Members are declared under **aliases**, and the expression names the aliases —
a content address is not a legal identifier in any expression grammar, and an
alias reads better and survives a member being re-declared at a new address.

```bash
areev trigger add --db ap.db --ns accounting \
  --type composite --workflow <WF_HASH> \
  --members 'invoice=<hash-a>,purchase_order=<hash-b>,escalation=<hash-c>' \
  --where '(invoice = true AND purchase_order = true) OR escalation = true' \
  --correlate /thread_id --window 10m \
  --because "match an invoice to its purchase order before posting"
```

`--members` takes `alias=hash` pairs. `--window` takes a unit — `90s`, `10m`,
`2h`, `1d`; a bare number is refused rather than guessed, because the field
stores milliseconds while `--interval` takes seconds and being wrong by three
orders of magnitude is silent.

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

A gate that names an undeclared member is refused with `TRG-E008` **when it is
declared**, not when it first comes due: it could never be satisfied, so the
trigger would be silently dead, and a declaration that sits in the memory
looking live is the failure mode with no symptom.

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
  "method": "GET", "credential": "gmail",
  "headers": { "X-Goog-User-Project": "my-project" } }
```

(`headers` is optional and carries non-credential request headers only — the
broker refuses the ones it owns; see [run.md](run.md).)

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

## A declaration that cannot fire

A trigger is refused at authoring time when it can never fire — an unparseable
cron, a timezone this build does not support, a composite gate naming a member
the declaration does not carry. That check runs on `areev trigger add`, on both
bindings' `trigger_add`, and on the generic `add("trigger", …)`.

Authoring-time refusal cannot be the only defence, though: a declaration can
arrive by **bundle import** from an implementation that validated differently,
or predate the check. So the evaluator reports one rather than assuming it was
caught:

```bash
$ areev trigger status --db ap.db --ns accounting
a32027ab5633  due      interval  4cb34f1a2c9d
599423634b3d  unusable schedule  4cb34f1a2c9d
              cannot fire: TRG-E006: schedule "0 9 * * *" is unusable: timezone
              "Asia/Kolkata" is not supported in this release — cron is evaluated in UTC. …

$ areev trigger run --db ap.db --ns accounting
claimed 1 · items 1 · runs 1 · duplicates 0 · not due 0 · locked 0 · unusable 1
error: 599423634b3d: TRG-E006: schedule "0 9 * * *" is unusable: timezone …
$ echo $?
1
```

The pass exits non-zero while an unusable declaration exists, so a heartbeat
surfaces it instead of succeeding quietly every tick. Fix the declaration or
disable it.

`unusable` is counted **apart from `not due`** on purpose. Folded in there it is
indistinguishable from a healthy trigger waiting its turn, which is the failure
mode with no symptom: the work simply never happens and every report looks
green. `trigger_status()` carries the same reason as an `unusable` field, absent
when the declaration is fine, and such a trigger is never reported as `due` —
saying otherwise would promise a firing that cannot happen.

## Putting it on a heartbeat

```bash
areev trigger render --db accounting.db --ns accounting --target cron
areev trigger render --db accounting.db --ns accounting --target k8s-cronjob
```

Targets: `cron`, `launchd`, `systemd`, `k8s-cronjob`. These are **templates,
not API clients** — Areev holds no cloud credentials and creates no cloud
resources. You apply the output with whatever you already use.

The host targets (`cron`, `launchd`, `systemd`) genuinely run on the machine
that produced the render, so they name that machine's binary by absolute path.
`k8s-cronjob` runs the *image's* binary instead, so its `command[0]` is plain
`areev` — the name on `PATH` inside the image. The `--db` path is still the one
you wrote: mount the memory and make that path resolve inside the container,
because a heartbeat pointed at a path that does not exist fails every tick,
which looks exactly like nothing being due.

In a container fleet with no crontab at all, the repo's Docker image carries
the dumb heartbeat as an image command: `docker run areev heartbeat --ns
accounting` loops this same one-shot evaluation at `$AREEV_HEARTBEAT_SECS`
ticks, and `k8s-cronjob` renders against that image's name, so the template
applies unedited — see [`docker.md`](docker.md).

The rendered interval is the **greatest common divisor** of your declared
intervals, floored at 60s — deliberately coarser than your shortest trigger.
The memory owns the real cadence; rendering a 30-second cron because one trigger
asked for 30 seconds would put the schedule back in the crontab, which is the
thing this feature exists to stop.

## From Python and Node

Every `areev trigger` subcommand has a binding method, so a host that already
embeds Areev does not have to ship, pin and sign the `areev` binary alongside it
just to fire a rule it can already declare. Names mirror the CLI
(`trigger_add`/`triggerAdd`, `trigger_run`/`triggerRun`, …), the FFI convention
is unchanged — scalars in, JSON strings out — and the returned reports are the
same `EvalReport` and `TriggerStatus` shapes the CLI prints under
`--format json`.

```python
m = areev.Areev("accounting.db", ns="accounting")
m.trigger_add(json.dumps({
    "kind": "polling", "workflow": plan_hash,
    "connector": "gmail", "scope": "label:invoices",
    "dedup_key": ["message_id"],
    "interval_secs": 300,
}), because="invoices arrive by email")

# One idempotent pass — safe to call concurrently from several nodes.
report = json.loads(m.trigger_run(
    connector_cmd="./gmail.sh",
    tool_cmd="./tools.sh",
    credentials_json=json.dumps({"gmail": "GMAIL_TOKEN"}),
))
```

```javascript
const report = JSON.parse(await m.triggerRun(
  null, false, null, null, './gmail.sh', './tools.sh',
  JSON.stringify({ gmail: 'GMAIL_TOKEN' }),
))
```

Both `trigger_add` and the generic `add("trigger", …)` validate the schedule —
the cron expression is parsed, a non-UTC timezone is refused (`TRG-E006`), and
a composite's gate is checked against its own members — so the path you reach
for first is not the unvalidated one. `trigger_add` additionally requires
`because`.

One deliberate difference from the CLI:

- **An unset credential variable is an error, not a skip.** The CLI drops a
  `--credential` whose variable is unset; the binding refuses, because a host
  wiring this up programmatically has no console on which to notice the
  omission, and the failure would otherwise surface as an unexplained 401 from
  someone else's API.

`credentials_json` maps a credential **name** to the **environment variable**
its value is read from, never to the value itself — the same discipline as
`--credential NAME=ENV_VAR`, so a secret never crosses the FFI boundary as a
literal.

There is still no daemon. `trigger_run` is a call the host makes on its own
heartbeat, and `trigger_render` writes the config for one.

## In the console

Triggers have no page of their own. They render on the **workflow canvas**, in a
"STARTED BY" lane to the left of the plan's entry steps, with a dashed arrow into
each one — because the binding points trigger → plan and never the reverse, a
trigger is only fully legible next to the plan it starts. A flat list cannot show
you that two triggers start the same plan; the lane shows it at a glance. Each
plan card in the Workflows list carries the same fact in miniature (the rule when
there is one trigger, the count when there are several), and a trigger whose plan
is not in the current namespace gets an explicit callout under the list rather
than vanishing from the console.

Clicking a trigger opens its whole declaration in the inspector — the rule said
out loud (a `memory` trigger's serialized `Condition` tree renders as
`subject = "globex" AND relation = "open_incidents"`, not as JSON), the
`because`, the connector and scope, the dedup key, concurrency and catch-up.

Every row is read-only text, and there are no inputs, because the console
genuinely cannot author one: CAL has no `ADD trigger`, and `ADD workflow`'s
`ON "..."` clause was removed in 1.3, so a surface that writes only through
`/api/cal` has nothing to write. Firing stays out for a separate reason: it
spends budgets and executes effects, so the actor's identity is the audit
record, and "whoever holds the console token" is not an identity — the same
reason `run.respond` refuses a shared-token caller. The CLI and the bindings
both carry a real principal, which is why they may fire and the console may
not.

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
| `TRG-E011` | a connector's blob payload violated the contract (bad base64, dangling `"@N"`, budget overrun) — the poll refused whole, cursor unmoved |
