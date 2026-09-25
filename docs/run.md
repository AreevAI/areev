# `areev run` — the governed workflow runtime

Every agent framework executes graphs. Almost none can *prove* an execution
afterwards: crash recovery duplicates side effects, "who approved this?" is a
chat search, and "what did the agent actually do?" is a log grep. `areev run`
executes a **Workflow grain** as a governed run whose entire history lives in
the same memory file as everything else Areev stores — journaled before it
happens, checkpointed as it happens, replayable after it happens, and gated
by humans where you say so.

The design rests on three moves:

1. **Intent before dispatch.** Before any effect fires, a Pending Tool grain
   is written carrying the effect's full identity. The result *supersedes*
   that intent. A crash between the two is therefore visible on resume, and
   the effect is re-delivered under the **same idempotency key** — journaled
   as a redelivery, never minted as a duplicate. Exactly-once across
   interrupts; at-least-once across crashes, with the key your tool needs to
   deduplicate.
2. **A pure scheduler.** The step function (`areev-run-core`) is sans-IO —
   no clock, no randomness, no store access in its dependency tree
   (CI-enforced). Every decision it makes is a function of the journal, which
   is what makes the third move possible:
3. **Verification, not trust.** `areev run verify` re-derives every checkpoint
   from the journaled events and byte-compares against the stored chain,
   writing nothing. If anyone edited history — or if a nondeterminism bug
   crept in — verify names the checkpoint and the differing fields.

## The 10-minute proof (no LLM, no waiting)

```bash
areev run demo --db runs.db      # seeds a 2-node plan: host tool → human approval
```

The demo prints the workflow hash (plans are content-addressed grains — the
same plan is the same hash, forever). Start a run:

```bash
areev run start --db runs.db --workflow <WF_HASH> --run-id demo-1 \
  --input '{"who":"world"}' --tool-cmd 'printf '\''{"greeting":"hello"}'\'''
```

The host tool executes, then the run **parks** on the approval node and
prints a `requires_action` envelope:

```json
{"kind":"requires_action","run_id":"demo-1",
 "asks":[{"node":"approve","tool_name":"approve","approval":true,
          "tool_call_id":"<ASK>","input":{"greeting":"hello","who":"world"}}],
 "checkpoint":"…"}
```

Answer it — as a **different** principal, because the principal who triggered
an approval ask structurally cannot answer it — then resume and verify:

```bash
areev run respond --db runs.db --run-id demo-1 --ask <ASK> \
  --result '{"approved":true}' --as user:officer
areev run resume  --db runs.db --run-id demo-1    # → {"finished":"Completed"}
areev run verify  --db runs.db --run-id demo-1    # → {"verified": true, "steps": […]}
```

Responding and resuming are deliberately separate acts: recording a human's
answer must not require holding the run's writer handle open while the human
thinks.

## Authoring a plan

A plan is a **Workflow grain**: nodes, edges, and per-node bindings to Tool
grains. Author it from any surface that adds grains — the bindings' generic
`add`, MCP `areev_add`, or the Rust builders:

```python
import areev, json
db = areev.Areev("runs.db", ns="ops")

fetch   = db.add("tool", json.dumps({"tool_name": "fetch", "kind": "definition",
                                     "tool_description": "fetch the order"}), ns="ops")
approve = db.add("tool", json.dumps({"tool_name": "approve", "kind": "definition",
                                     "tool_description": "human approves the refund",
                                     "executor_kind": "client"}), ns="ops")

wf = db.add("workflow", json.dumps({
    "nodes": ["fetch", "retry_fetch", "approve", "refund"],
    "edges": [
        {"src": "fetch",       "dst": "approve", "cond": "order.found == true"},
        {"src": "fetch",       "dst": "retry_fetch", "cond": "!order.found"},
        {"src": "retry_fetch", "dst": "fetch", "max_cycles": 3},
        {"src": "approve",     "dst": "refund", "cond": "approved == true"},
    ],
    "bindings": {"fetch": fetch, "retry_fetch": fetch, "approve": approve},
    "retries": {"fetch": 2},
    "reducers": {"attempts": "sum"},
}), ns="ops")
```

What each piece means:

- **`bindings`** — node → Tool grain hash, pinned at *run start* into the
  run's manifest (resolution freeze: a plan can't change out from under a
  running run). Three executor shapes fall out of what you bind:
  - a **host tool** (the default) executes through your `--tool-cmd`;
  - a Tool with **`executor_kind: "client"`** is a human gate — the run
    parks and waits for `respond`;
  - a binding to another **Workflow grain** is a **subgraph** — it executes
    inline as a child run with its own journal (child run id is
    deterministic: `parent~sha256(parent, node, attempt)[..16]`, so a bounded
    cycle re-running the node gets a fresh child while a park and its answer
    reach the same one). A child that parks on a human gate **bubbles** its
    asks to the parent — see below.
  - an **unbound** node (no binding, no same-named Definition grain) is an
    **abstract node** — a journaled LLM tool-calling loop; see below.
- **`retries`** — `retries: {node: n}` means *n re-attempts* after the
  first failure, per re-entry generation.
- **`max_cycles`** — the loop-authoring primitive. Cycles are legal, but
  every cycle must carry at least one `max_cycles`-bounded edge or the plan
  is refused at load (`RUN-E002` — an unbounded loop is a bug, not a
  feature). The cycle's re-entry point can be **any** node in the loop —
  above, the back-edge closes on `fetch`, the entry, but a mid-graph gate
  (`notify -> gate -> converse -> gate`, the back-edge targeting `gate`,
  not the entry) works identically.
- **`reducers`** — per-state-key merge functions, frozen into the manifest:
  `lww` (the default for undeclared keys), `append`, `sum`, `max`, `min`.
  They are law-tested for batching invariance, which is what makes fan-out
  results order-independent.
- **`reads`** — node → a declared read of the run's **own memory**
  (`entity_at`, `related` or `recall`). The runtime answers that node itself, from the
  file it already holds; no tool is involved. See
  [Reading the run's own memory](#reading-the-runs-own-memory-reads).

Plan validation runs at load: unreachable nodes (`RUN-E003`), unresolvable
bindings (`RUN-E004`), malformed conditions (`RUN-E005`), unbounded cycles
(`RUN-E002`), structural shape (`RUN-E019`).

### Reading the run's own memory (`reads`)

A tool must never open the memory its own run is holding — on the embedded
tier the file lock refuses it outright ([below](#backend-divergence-reading-the-memory-mid-run-85)),
and on every tier a tool holding a handle can read anything. So an as-of read
used to happen in the *driver*, before `run start`, pinned into the input. A
host that only calls `run start`/`resume` — a queue worker — has nowhere to
do that. Declare the read on the plan instead, and the runtime answers it:

```python
wf = db.add("workflow", json.dumps({
    "nodes": ["extract", "cover_at_loss", "known_at_notice", "assess"],
    "edges": [{"src": "extract", "dst": "cover_at_loss"},
              {"src": "cover_at_loss", "dst": "known_at_notice"},
              {"src": "known_at_notice", "dst": "assess"}],
    "bindings": {"extract": extract, "assess": assess},
    "reads": {
        # what cover was IN FORCE on the date of loss (world clock)
        "cover_at_loss": {"op": "entity_at", "ns": "org.uw.policies",
                          "subject_from": "/policy_id", "relation": "mg:coverage_limit",
                          "at_from": "/date_of_loss", "axis": "world"},
        # what the desk KNEW when the notice arrived (knowledge clock)
        "known_at_notice": {"op": "entity_at", "ns": "org.uw.policies",
                            "subject_from": "/policy_id", "relation": "mg:coverage_limit",
                            "at_from": "/received_at", "axis": "knowledge",
                            "into": "cover_known"},
    },
}), ns="org.uw")
```

`assess` then finds, in its merged state, `cover_at_loss` and `cover_known`
holding **exactly** what `db.entity_at(subject, relation, at, axis=…)` returns
— `{"found": true, "grain": {…}}`, or `{"found": false}` when nothing was on
that clock at that instant (a backdated fact, asked about before it was
received, is the honest miss). `"op": "related"` returns `db.related`'s
`{"start", "reached"}` the same way.

`"op": "recall"` answers "what do I hold about this subject" — the last five
statements for an account, every open item on a case — and returns exactly
what `db.recall(subject, relation, k, ns)` returns: a list of
`{"hash", "type", "fields"}`, newest first, **never more than `k`**:

```python
"reads": {
    "recent_statements": {"op": "recall", "ns": "org.ap.ledger",
                          "subject_from": "/account", "relation": "mg:statement",
                          "k": 5},
    # as-of: per relation, what entity_at answers on that clock at that instant
    "account_at_notice": {"op": "recall", "ns": "org.ap.ledger",
                          "subject_from": "/account", "k": 16,
                          "at_from": "/received_at", "axis": "knowledge"},
}
```

With an instant it is an **as-of recall**: for each relation the subject has
(or the one named), the grain `entity_at` answers at that instant on that
axis, ordered newest first on the asked clock (`valid_from` for world,
`created_at` for knowledge) and bounded by `k`. So a named relation with
`k: 1` is exactly `entity_at` in recall's shape, and an as-of recall is one
grain per relation where a current recall returns every live grain.

| Key | `entity_at` | `related` | `recall` | |
|---|---|---|---|---|
| `op` | `"entity_at"` | `"related"` | `"recall"` | required |
| `ns` | ✓ | ✓ | ✓ | the run's namespace or a dotted descendant, exact (no `"org.*"`); default the run's |
| `into` | ✓ | ✓ | ✓ | the state key the answer lands under; default the node id |
| `subject` / `subject_from` | ✓ | | ✓ | exactly one: a literal, or a JSON pointer into the node's input |
| `relation` | ✓ | | optional | literal; a `recall` without one reads every relation |
| `k` | | | ✓ | 1–64 (default 16); past 64 is refused, never clamped |
| `at` / `at_from` | ✓ | | optional | exactly one (at most one for `recall`); epoch ms or an ISO-8601 date/timestamp (UTC) |
| `axis` | ✓ | | ✓ | `world` (default) or `knowledge`; on `recall` only with an instant |
| `start` / `start_from` | | ✓ | | exactly one, as `subject` |
| `relations` | | ✓ | | a list, or a comma-separated string |
| `direction`, `depth`, `limit` | | ✓ | | `out`/`in`/`both` (default `out`), 1–4 (default 2), 1–512 (default 64) |

Only the subject, the start and the instant may come from state; the
relation, the axis, the namespace, the count and the walk's shape are literals
on the plan, so a reviewer reads exactly what a run may see. There is no free
text and no predicate: a question richer than these three reads is a saved
query feeding a trigger's [`--context-query`](triggers.md), or a driver-side
pre-read into the run input (ARCHITECTURE.md §10, "In-run recall is a third
typed read; saved queries are not"). Pointers resolve against
the node's input — the merged state for a node, the task's own input for a
[`$send`](#fan-out-send) task, so one read node can fan out across many
subjects (pair it with an `append` reducer on its `into` key).

The rules, each enforced rather than advised:

- **Refused at start, naming the node** — an unknown key (`axsi` does not
  quietly read the world axis, `query` does not become free text), a missing
  or doubled operand, a count out of range (`k: 0`, `k: 65`, `depth: 9`), a
  pattern namespace, a node that both
  binds a tool and declares a read (`RUN-E019`); a namespace outside the run's
  own, or one the session holds no `read` grant on (`RUN-E012`). Grants are
  checked again at each read, because a resume need not run under the session
  that started it.
- **It is the runtime's read, not a tool's.** The driver performs it on its
  own thread. It is never offered to an abstract node's model, never handed to
  `--tool-cmd`, a native blob or a `wasm32-areev-io` module, and the executor
  pool refuses one outright if it ever arrives there.
- **The count ceiling is the runtime's.** `k` is refused past 64 at start,
  and the executed recall is truncated to the plan's `k` whatever the store
  returned; a pinned declaration whose `k` is out of range (a hand-edited
  manifest) fails the node rather than read more.
- **Journaled like every effect.** An intent before, a result after — named
  `mg:entity_at` / `mg:related` / `mg:recall` — whose `read` field records what
  was read *as resolved*: `{op, ns, subject, relation, at, axis, grain}` (the
  grain's hash, or `null` for a miss) for `entity_at`;
  `{op, ns, subject, relation, k, grains}` (every result hash, in order, and
  `relation: null` when none was named) plus `at` and `axis` for an as-of
  `recall`. `run-trace` shows it, and `verify` and `shadow` answer
  it from the journal, so a determination stays reproducible after the file has
  moved on. `run inspect` prints each read's frozen declaration under `read`.
- **Failures fail the node, never a guess.** A pointer that lands on nothing,
  or on the wrong type, is `schema_validation_failed` and is not retried (the
  same state fails the same way); a store error is `executor_error` and obeys
  the node's `retries`.
- **Reads go through the store's egress boundary**, exactly as
  `db.entity_at` / `db.recall` do, so an `egress` anonymization policy on the
  target namespace applies.

Plans with `reads` are authored through the generic JSON `add` (like
`max_cycles` and `reducers`); CAL `ADD workflow` has no syntax for them, and
the console opens such a plan view-only rather than let a resave turn every
read into an abstract LLM step.

### The condition grammar (frozen)

Edge conditions evaluate against the run's context (the merged node
results). The v1 grammar is deliberately small and will not grow quietly:

```text
cond    := path op literal | path "exists" | ["!"] path
op      := "==" | "!="
path    := segment ("." segment)*        segment := [A-Za-z0-9_-]+
literal := JSON string | number | true | false | null
```

Strict JSON equality, no coercion (`"1" != 1`). Truthiness: `false`, `null`,
missing, `""`, `0`, `[]`, `{}` are false; everything else is true. Parse
errors are load-time; **evaluation is total** — a missing path is falsey,
never a runtime error.

### The host-tool contract (`--tool-cmd`)

One subprocess seam on every surface (CLI `--tool-cmd`, MCP
`$AREEV_RUN_TOOL_CMD`, bindings `tool_cmd`):

- the command runs via `/bin/sh -c`, once per effect;
- the tool **input JSON arrives on stdin**;
- `AREEV_TOOL_NAME`, `AREEV_TOOL_HASH`, and `AREEV_IDEMPOTENCY_KEY` arrive in
  the environment — the key is what your tool uses to deduplicate a
  crash-window redelivery;
- the **result JSON leaves on stdout**. A non-zero exit or non-JSON stdout
  is a Failed effect (stderr is captured into the failure detail).

Without a tool command configured, host-tool nodes fail loudly rather than
silently — there is no built-in "just run it" executor.

**What the journal keeps and what the model sees are not the same thing.** The
result grain always carries your tool's full output, verbatim. When the tool was
called *by a model* inside an abstract node, `--llm-tool-result-chars` bounds
how much of it enters that node's transcript — the model may see a head, a tail
and a pointer where a 40 KB log dump was. Write tools whose output a model can
use, and reach for the journal (`areev run-trace`) when you want all of it.

Every host-executed tool (`--tool-cmd` and a pinned `--allow-executor` blob
alike) runs under a wall-clock ceiling, fixed at 300s until it could be
raised (#133): a tool that never exits parks a pool worker and, at the next
wave boundary, the whole driver. `--executor-timeout SECS` (CLI),
`executor_timeout_secs` (Python/Node), `$AREEV_RUN_EXECUTOR_TIMEOUT` (MCP,
and any surface's out-of-band form) overrides it — `0` waits forever, the
pre-1.3 behaviour, restored on request. A document-analysis leg making a
dozen model calls needs longer than the `pdftotext`-shaped tool the default
was sized for.

By default a tool inherits this process's environment minus the variables
Areev was *told* hold secrets — `--passphrase-env`, `--token-env`,
`--credential` and the rest of the `*-env` family
([security-model.md](security-model.md)). That is the non-breaking default,
because a deployed tool may legitimately read an API key out of the
environment.
`--tool-env VAR,…` (CLI), `$AREEV_RUN_TOOL_ENV` (MCP), `tool_env=` (Python),
`toolEnv` (Node) inverts it: the environment is cleared and only the named
variables get through, on top of the minimal set (`PATH` above all) without
which a bare command name resolves to nothing. Name what a tool may see when
this process holds secrets Areev has no way to know about. A bare `--tool-env`
passes nothing but that minimal set, and naming a variable Areev already knows
holds a secret does not re-admit it — the name is dropped and reported. It
applies to `--tool-cmd`, to a `trigger run` connector, and to a pinned
**native** blob; the sandbox seam clears unconditionally and cannot be widened
by this flag (pinned by test).

**Presence is the setting, not the value.** `--tool-env ""` and
`AREEV_RUN_TOOL_ENV=""` both mean *clear to the minimal set* — the strictest
posture — and are honored as such. Every other `$AREEV_RUN_*` variable reads an
empty value as "not configured", which is right for them (`AREEV_RUN_SANDBOX_CMD=""`
means "no sandbox") and would be exactly backwards here: it would take an
operator asking for the strictest environment and silently hand them the
loosest. Only an **absent** flag and variable keep the inherit default. The
bindings follow the same rule: `tool_env=""` (Python) and `toolEnv: ""`
(Node) clear to the minimal set, on the run executors, a pinned native blob
and the trigger connector alike; `None` / `null` keep the inherit default.

### The model boundary (anonymization)

If an `anon:<ns>` policy declares `egress` (or `both`) for the run's namespace,
an abstract node's prompt is pseudonymized on the way out and the model's
tool-call arguments are rehydrated on the way back:

```
run state ──pseudonymize──▶ model ──rehydrate──▶ host tool
  real                  [EMAIL_7C1A]              real
```

The boundary is the **model, not the tool**. A tool posting an invoice must
receive real values — a pseudonymized supplier writes a corrupt record — while
the model doing the extraction works just as well on a placeholder.

This closes a gap rather than adding a feature: the store's gate is an egress
boundary on *reads*, and an abstract node's prompt is not a read. A trigger
hands its payload straight into `run start` in process, so the one place a
model was actually called was the one place an `egress` policy did not reach.

Five things follow from it:

- **Only what a model produced is rehydrated** (#350). A placeholder is
  resolved — or refused — only when a model turn of this run emitted it,
  whether as a tool call's arguments or as output flowing into a downstream
  node's input. A subgraph's result counts as model output too, since it may
  carry a child's. Text a host or code tool wrote itself is dispatched
  **verbatim**, even when it has the placeholder's shape: a pipeline that
  pseudonymizes a record with its own `[PERSON_1]` markers, with no model in
  the plan, is neither refused as "unresolvable" nor rewritten to whatever an
  older run's mapping holds under that key. The run's own configuration —
  Tool Definitions, their declared `capabilities.http.hosts`, the manifest,
  a stored input — is frozen from the stored grain, never through the egress
  rewrite, so a declared `http://127.0.0.1:7792` stays that host.
- **Rehydration fails closed.** A placeholder the run cannot resolve — a model
  inventing `[EMAIL_DEADBEEF]`, say — **fails the node** rather than
  dispatching. Sending the placeholder itself to a vendor is worse than
  failing.
- **The journal keeps the pseudonymized form.** Rehydration happens for
  dispatch only, and the idempotency key derives from the pseudonymized input,
  so `verify` replays byte-identically whether or not a policy is live.
- **The policy must be `scope: memory`**, which means an encrypted memory.
  Session scope numbers tokens by order of appearance, so a replay would
  pseudonymize differently and `verify` would diverge. A plan with an abstract
  node under any other scope is refused at start with `RUN-E023`, which names
  the fix.
- **Only what the detectors catch is replaced.** Tier-0 detects `email` and
  `phone` by pattern; `person` matches interned known identities and the
  policy's `custom_terms`. A bare personal name that the memory has never seen
  as a subject is **not** pseudonymized. Declare the terms you care about
  rather than assuming a name is caught.

A namespace with no declared policy is untouched, exactly as before.

### Brokered egress (`--credential`, `--allow-host`, `--tool-egress`)

A tool that posts to a vendor API does not need to hold the token:

```bash
areev run start --workflow <WF> --run-id r1 --tool-cmd ./tools.sh \
  --credential zoho=ZOHO_TOKEN,graph=GRAPH_TOKEN \
  --allow-host 'https://books.zoho.com,https://graph.microsoft.com' \
  --tool-egress 'zoho_post:zoho:POST,send_email:graph:POST,parse_pdf::'
```

The tool gets `AREEV_EGRESS_URL` and `AREEV_EGRESS_TOKEN` in its environment —
**never a credential value** — and posts the call it wants:

```json
{ "url": "https://books.zoho.com/api/v3/bills", "method": "POST",
  "credential": "zoho", "headers": { "X-Tenant-Id": "acme" },
  "body": "{...}" }
```

The broker checks the destination against `--allow-host`, the method and
credential against that tool's grant, attaches the credential, and makes the
request. The token never enters the tool's process, so a compromised tool has
nothing to exfiltrate.

The broker is on every surface that starts a run, not only the CLI (#201).
The five settings are the same spec strings everywhere, parsed by one parser
(`areev_run::EgressSpec`), so a spec that works on the CLI works verbatim
from a binding — and a refusal from a binding-driven run is journaled exactly
as from the CLI: `403` carrying `RUN-E022` to the tool, an Observation in
`agent:harness`:

| CLI | Python (`run_start`/`run_resume`/`trigger_run`/`trigger_deliver`) | Node | Environment (`areev serve`, and any surface out of band) |
|---|---|---|---|
| `--credential` | `credentials=` | `credentials` | `$AREEV_RUN_CREDENTIAL` |
| `--allow-host` | `allow_hosts=` | `allowHosts` | `$AREEV_RUN_ALLOW_HOST` |
| `--tool-egress` | `tool_egress=` | `toolEgress` | `$AREEV_RUN_TOOL_EGRESS` |
| `--credential-ttl` | `credential_ttl_secs=` | `credentialTtlSecs` | `$AREEV_RUN_CREDENTIAL_TTL` |
| `--resolver-env` | `resolver_env=` | `resolverEnv` | `$AREEV_RUN_RESOLVER_ENV` |

The variables are server-bound in the sense `$AREEV_RUN_TOOL_CMD` is: read
at `areev serve` start, never from an MCP client, because the grant IS the
authorization. A flag wins over its variable. On the trigger surface
`credentials` configures the connector-poll broker too (owners dropped, as
one `--credential` flag does both), beside the older `credentials_json`.

`headers` is optional and carries **non-credential** request headers — the
ones enterprise APIs require and no credential expresses: `X-Goog-User-Project`
on Google calls made with user credentials, `anthropic-version`, `x-ms-version`,
a tenant id. The broker refuses `Authorization`, `Proxy-Authorization`,
`Cookie`, `Host`, and any header a configured credential rides in, whatever
their casing: those are the broker's to set, and a caller that could write them
would be holding the credential channel it exists not to hold. A malformed name
or a value containing CR/LF is a `400` — header injection dies at the parse, not
at the socket. Because the caller chose these values, they are **journaled in
full**, unlike a credential, which is journaled by name only.

`--tool-egress tool:credentials:methods` is the grant, `+`-separated within a
field. Three rules are deliberate:

- **A tool with no grant gets nothing** — not even the broker's address.
- **A grant naming no method may only read.** Connectors read; tools write, and
  the write verb is the one worth making deliberate. `parse_pdf::` above grants
  nothing at all.
- **Naming a credential is not being allowed to use it.** The tool chooses
  *which* by name; the host decides whether it may. One tool's token buys
  nothing of another's scope.
- **Being allowed to use it is not being allowed to send it anywhere.** Write
  `cred@host` to pair a credential with the hostname it may go to:

  ```bash
  --tool-egress 'sync:gmail@gmail.googleapis.com+sheets@sheets.googleapis.com:POST'
  ```

  A tool that reads a mailbox and writes a sheet needs both secrets, and
  without the pairing nothing stops it sending the mailbox token to the sheets
  API — a bug reaches that as easily as malice. The host is a **bare
  hostname** (`*.example.com` works); scheme and port are narrowed by
  `--allow-host`, because this spec is colon-delimited and a URL would tear
  apart in it. An unpaired `cred` keeps its old meaning: any host the rest of
  the chain permits.

The grant is host configuration, never a grain — a Definition declaring its own
reach would be a permission arriving in the same bundle as the code it
authorizes.

### Where a credential comes from

`--credential NAME=ENV_VAR` reads the value once, at startup. That is right for
a static API key and wrong for everything a cloud issues: an access token
expires in about an hour, so a heartbeat running overnight starts failing with
someone else's `401` and nothing in your logs says why.

Two more sources resolve **at call time**, inside the broker:

```bash
# anything that prints a token on stdout
--credential 'sheets=cmd:gcloud auth print-access-token'
--credential 'zoho=cmd:vault kv get -field=token secret/zoho'
--credential 'db=cmd:aws secretsmanager get-secret-value --secret-id db --query SecretString --output text'

# or read Vault/OpenBao directly, with no vault binary in the image
--credential 'sheets=vault:secret/data/google#access_token' \
  --resolver-env VAULT_ADDR,VAULT_TOKEN
```

The tool still names a label and holds nothing — only where the value came
from changed. Four things worth knowing:

- **Values are cached for `--credential-ttl` seconds** (default 300) and minted
  again after, so revoking a secret upstream takes effect without a restart.
- **A failing resolver refuses the call.** It never falls through to an
  unauthenticated request, and the error names *which* credential failed
  without ever repeating what the resolver printed.
- **`--resolver-env` is how a resolver gets its own credentials.** Those
  variables are withheld from every *other* subprocess and re-admitted only for
  resolvers, which otherwise see little more than `PATH` and `HOME`. Naming
  them is not optional bookkeeping: a `VAULT_TOKEN` left ambient is readable by
  every `--tool-cmd` you run, and it can fetch every secret, not just this one.
- **Bind a principal on the name side** for these — `--credential
  'sheets@user:alice=cmd:…'` — because a command may itself contain `@`.

A resolver command containing a comma has to live in a script: commas separate
credentials in this flag.

**A refusal is journaled, not just logged.** It reaches the tool as a `403`
carrying `RUN-E022`, prints to stderr when the run ends, *and* lands in the
memory as an Observation in `agent:harness`:

```bash
areev cal 'RECALL observations WHERE namespace = "agent:harness"' --db runs.db
# → observation_kind "egress_refusal", with run_id, caller, destination, reason
```

An agent reaching for somewhere it was not allowed is the most audit-worthy
event this subsystem produces, and a terminal that has scrolled cannot answer
"did it ever try?". One Observation per **distinct** `(caller, destination,
reason)` per run, deduplicated where the refusal is recorded — a tool retrying
forty times against one blocked host is one audit fact, not forty, so the
record is bounded by the plan's shape rather than by how hard something
retries. The per-attempt count stays in the log line. Like the run-outcome
record, it is not a journal entry, so `verify` is unaffected.

Omitting all three flags leaves tools exactly as they were: no broker, and
whatever credentials your tool script already reads for itself.

**Honest limits.** Exfiltration *through* an allowed host still works (encode
data into a draft, a label, a filename); hostname allowlisting cannot see
through DNS tricks or domain fronting; and a brokered tool cannot use a vendor
SDK, because the SDK wants its own sockets. This raises the bar; it is not a
boundary.

### Code-carrying tools (`executor_uri`)

A Definition may name its executor by content address instead of relying on
whatever `--tool-cmd` happens to be:

```json
{ "tool_name": "zoho_post", "kind": "definition",
  "executor_uri": "cas://sha256:<64 hex>" }
```

The blob is an ordinary CAS blob, so it travels in bundles and `get_blob`
verifies its digest on every read. The contract inside is identical to
`--tool-cmd`: JSON on stdin, JSON on stdout, `AREEV_TOOL_NAME` /
`AREEV_TOOL_HASH` / `AREEV_IDEMPOTENCY_KEY` in the environment (plus
`AREEV_EXECUTOR_URI`) — **including brokered credentials**: with
`--credential`/`--tool-egress` configured, a granted blob gets
`AREEV_EGRESS_URL`/`AREEV_EGRESS_TOKEN` on the same terms as a `--tool-cmd`
(#87 — the authoring style whose provenance the host can prove no longer gets
the weaker credential story).

**Nothing code-carrying runs unless the host pinned its address:**

```bash
areev run start --workflow <WF> --run-id r1 \
  --allow-executor <64 hex>[,<64 hex>...] [--executor-cache DIR]
```

The pin exists on every surface that starts runs (#87): `allow_executor` /
`executor_cache` on `run_start`/`run_resume` in Python and Node (same comma
list), and `$AREEV_RUN_ALLOW_EXECUTOR` / `$AREEV_RUN_EXECUTOR_CACHE` set at
`areev serve` start for MCP — server-bound like `$AREEV_RUN_TOOL_CMD`,
because the pin IS the authorization and an MCP client must not grant it to
itself. The console's HTTP surface does not start runs at all (its runner is
deliberately non-executing), so it carries no pin.

Because bundles carry blobs, importing a peer's memory imports their connector
code — so the authorization to execute it deliberately does **not** live in the
file. There is no grant form; a permission arriving in the same bundle as the
code it authorizes is not a permission. An unpinned address is refused at start
with `RUN-E018`, before the run takes a lease, naming the address so pinning it
is a copy-paste.

Two more refusals, both `RUN-E018` at resolve: an `executor_uri` this build
cannot dispatch (anything but `cas://sha256:<64 hex>`), and an `executor_uri` on
a **client** tool, which is answered by a person through `respond` and has no
executor to name. Every value either dispatches or is refused — a value that is
silently ignored is the failure this runtime exists to refuse.

The address is pinned into the manifest at start, so superseding the Definition
mid-run cannot change what executes. The blob is materialized to
`<cache>/<hex>` (mode 0700) and reused; the path *is* the content address.

A pinned **native** executor is not sandboxed — it runs as you, exactly like
`--tool-cmd`. The pin is a judgement about provenance, not a container. It is
also platform-specific: a blob is bytes, so pin per platform.

### Declared runtimes — dispatching to the sandbox (`runtime`)

Provenance and isolation are independent knobs (#86). A Definition may declare
the runtime its blob executes under:

```json
{ "tool_name": "validate_rows", "kind": "definition",
  "executor_uri": "cas://sha256:<64 hex>",
  "runtime": "wasm32-areev",
  "runtime_limits": { "fuel": 200000000, "max_pages": 256 } }
```

Absent (or `"native"`) is exactly the behaviour above. `"wasm32-areev"` routes
the pinned blob to **areev-sandbox** — a pure `wasm32` module under wasmi: no
WASI, a frozen one-function import set, fuel and memory ceilings, and
platform-independent by construction (one `.wasm` blob runs everywhere the
sandbox does). The engine constructs the sandbox's argv itself
(`--module <cached blob> --fuel N --max-pages N`, input JSON on stdin), so
`runtime` + `executor_uri` in the memory is the whole declaration.

The sandbox is host config, like the pin: `--sandbox-cmd 'areev-sandbox'` on
the CLI, `sandbox_cmd` on `run_start` in Python/Node, `$AREEV_RUN_SANDBOX_CMD`
for `areev serve`. A plan declaring `wasm32-areev` on a host with no sandbox
refuses at start (`RUN-E018`, naming the missing flag), and the runtime is
**frozen into the run manifest** with the address — a mid-run supersession
cannot re-route a blob from the sandbox to native exec. An unknown runtime
string refuses at resolve rather than falling back to native, which would run
foreign bytes as a program. (`areev-sandbox` is a separate `publish = false`
binary, so it is not on crates.io — but it ships: the container image carries
it at `/usr/local/bin/areev-sandbox`, and every CLI release archive carries it
beside `areev`. Both are built from one tree, and `areev-sandbox --version`
agrees with `areev --version` — a sandbox from a different tree than the
engine it bounds is the pairing that shipping them together prevents. On the
image, `--sandbox-cmd areev-sandbox` resolves on `PATH`.)

### Capability tools — persisting an I/O tool as a grain (`wasm32-areev-io`)

`wasm32-areev` is pure compute, by design and permanently. That left a gap:
Tier C is the only tier that produces a persistable, content-addressed tool,
and the tools every real agent needs do I/O. So a mailbox poller had to be a
native blob (persisted, but *not sandboxed — it runs as you*, and
platform-specific) or a host `--tool-cmd` script (sandboxed by nothing, and
outside the memory entirely).

`runtime: "wasm32-areev-io"` (#101) closes it. The guest still gets no socket:
it gets one more import, `areev::fetch`, answered by the credential broker the
run already has.

```json
{ "tool_name": "send_ask", "kind": "definition",
  "executor_uri": "cas://sha256:<64 hex>",
  "runtime": "wasm32-areev-io",
  "runtime_limits": { "fuel": 200000000, "max_pages": 256,
                      "max_calls": 64, "max_response_bytes": 1048576,
                      "max_request_bytes": 1048576 },
  "capabilities": [
    { "http": { "hosts": ["https://gmail.googleapis.com"],
                "methods": ["POST"],
                "path_prefixes": ["/gmail/v1/users/me/"],
                "credentials": ["gmail"],
                "headers": ["X-Goog-User-Project"] } },
    { "blob": { "read": true } }
  ] }
```

The two capabilities are **independent**. `{"blob": {"read": true}}` (#106)
lets a module read the memory's stored bytes by content address — the
attachment a trigger's connector already filed — through the same broker on
the same token, and grants no network. A module that parses attachments and
calls nothing declares only `blob`; one that calls an API and reads nothing
declares only `http`. Read-only, by address only: there is no enumeration, no
write, and no namespace access, so a module fetches bytes it was handed a
`cas://` reference to and cannot browse the memory. Every read lands as a
`blob_read` Observation naming the address and the byte count.

A blob-only module still needs a grant, because the token is what identifies
the caller: `--tool-egress 'parse_attachments::'` names neither a credential
nor a method, minting a token and authorizing no egress whatsoever.

**Both backends** (#202). The read never opens the memory, so serving a blob
cannot contend with the run holding it. On the embedded backend that means the
`.blobs` sidecar beside the file, which avoids the driver's exclusive write
lock; on PostgreSQL it means one short-lived connection of the broker's own and
a schema-qualified `SELECT` against the in-schema `blobs` table — no lock at
all, and independent of `search_path`, so it is safe behind a pooler. A
conformance case runs the same module against both.

`headers` names the non-credential request headers the module may set, and is
deny-by-default like `credentials`: declaring none permits none. A name the
broker owns (`Authorization`, `Cookie`, `Host`, `Proxy-Authorization`) is
refused **at write time**, so a module that tries to declare the credential
channel is unwritable rather than writable-and-refused-later. Matching is
case-insensitive, because HTTP field names are.

**One `http` block per service.** `capabilities` may carry several, and a call
must be admitted by a **single** block in full — its host *and* its path *and*
its method *and* its credential *and* its headers. That is what keeps a
two-service tool from sending one service's secret to the other:

```json
"capabilities": [
  { "http": { "hosts": ["https://gmail.googleapis.com"],
              "methods": ["GET"], "credentials": ["gmail"] } },
  { "http": { "hosts": ["https://sheets.googleapis.com"],
              "methods": ["POST"], "credentials": ["sheets"] } }
]
```

Merged into one block, that declaration would permit `gmail` on a Sheets
request — the lists are read as a pairing per block, never as two independent
memberships. Blocks are alternatives, so writing several can only narrow: a
call refused by every block is refused, and the refusal names the pairing
(*"it declares both, but no single capability pairs them"*) rather than
pretending the credential was undeclared. The host-side `--tool-egress
'sync:gmail@gmail.googleapis.com:GET'` expresses the same pairing, and both are
checked — a declaration arrives with the tool, so it is never the only thing
deciding where a secret may go.

**`capabilities` declares; it never grants.** The effective set is
`declared ∩ host-granted`, checked on every call, so the declaration can only
narrow what `--allow-host` / `--credential` / `--tool-egress` already permitted.
That is the same split `--allow-executor` makes for the code itself: the
declaration replicates with the bundle, the authority does not. What it buys is
audit (a synced memory says what a tool may reach without reading anyone's
command line) and a tighter bound than the host grant can express — the
host-side allowlist is host-only, while a capability may pin `path_prefixes`
and `methods` too.

Deny by default throughout: no declaration means no reach, no declared
`methods` means `GET`/`HEAD` only, no declared `credentials` means none. Host
entries use the same grammar as `--allow-host` (scheme mandatory, `*.dom`
excludes the apex, no bare `*`) — one parser, in `areev-core`, shared by the
CAL write path and the broker, so a tool that writes is a tool that runs.

Running one:

```bash
areev run start --db m.db --workflow <plan-hash> \
  --allow-executor cas://sha256:<64 hex> \
  --sandbox-cmd areev-sandbox \
  --credential gmail=GMAIL_TOKEN \
  --allow-host https://gmail.googleapis.com \
  --tool-egress 'send_ask:gmail:POST'
```

The same run from a binding takes the same spec strings (#201):

```python
m.run_start(plan, "r1", allow_executor="<64 hex>", sandbox_cmd="areev-sandbox",
            credentials="gmail=GMAIL_TOKEN",
            allow_hosts="https://gmail.googleapis.com",
            tool_egress="send_ask:gmail:POST")
```

```js
await m.runStart(plan, 'r1', null, null, null, null, null, null, null, null, null, null,
  '<64 hex>', null, 'areev-sandbox', null, null, null,
  'gmail=GMAIL_TOKEN', 'https://gmail.googleapis.com', 'send_ask:gmail:POST')
```

and `areev serve` reads them from `$AREEV_RUN_CREDENTIAL`,
`$AREEV_RUN_ALLOW_HOST` and `$AREEV_RUN_TOOL_EGRESS` at start.

**A trigger's connector is one of these too** (#185). A polling trigger may
name a Definition as its `connector_tool`, and the evaluator resolves it
through the same reader, pins it with the same `--allow-executor`, dispatches
it through the same `CodeExecutor` and answers its `areev::fetch` /
`areev::blob_get` from the per-poll broker — so everything in the table below
holds identically on the heartbeat path, with `TRG-E012` in place of
`RUN-E018` at the pin. See [`triggers.md`](triggers.md#the-connector-contract).
Three blessed blobs ship for exactly this shape:
[`blessed-tools.md`](blessed-tools.md).


What is enforced, and where:

| Check | Where | Failure |
|---|---|---|
| `capabilities` without `runtime: "wasm32-areev-io"` | CAL write, and again at resolve | write rejected / `RUN-E018` |
| the capability runtime with no `capabilities` | resolve | `RUN-E018` — a capability runtime declaring nothing can reach nothing |
| a malformed declaration | CAL write, and again at resolve | write rejected / `RUN-E018` |
| the runtime with no broker, or no `--tool-egress` for this tool | dispatch | node fails, naming the missing flag |
| a module importing `areev::fetch` undeclared | sandbox instantiation | `ForbiddenImport`, by name, before one instruction |
| a module importing `areev::blob_get` without `{"blob": {"read": true}}` | sandbox instantiation | `ForbiddenImport` — gated on the DECLARATION, not the runtime, so an http-only module never gains it |
| a blob read by a caller that declared no `blob` capability | broker, per read | 403 + a journaled refusal |
| a blob read when the host wired no memory | broker, per read | 503 — declaring is not granting on this door either |
| a malformed or unknown `cas://` address | broker, per read | 404 — the address is the only way in, so there is nothing to enumerate |
| host / path / method / credential / header outside the declaration | broker, per call **and per redirect hop** | 403 + a journaled refusal |
| a request header the broker owns (`Authorization`, `Cookie`, `Host`, `Proxy-Authorization`, or one carrying a configured credential) | broker, per call — before the call budget is spent | 403 + a journaled refusal; the answer is the same for every caller, so it costs nothing to ask |
| a malformed header name, or a value containing CR/LF | broker, per call | 400 — header injection is refused as malformed, not merely denied |
| an evasive path (`..`, `%2e`/`%2f`/`%5c`, `\\`) against declared `path_prefixes` | broker, per call | 403 — refused rather than normalized |
| anything outside the host grant | broker, per call | 403 + a journaled refusal |
| a private/loopback destination (`127.0.0.0/8`, `10/8`, `169.254/16`, `::1`, `fc00::/7`, …) under an **unrestricted** policy | broker, per call and per hop | 403 — a declaration alone cannot authorize local reach; name it in `--allow-host` |
| a credential owned by a different run principal (`--credential name=VAR@principal`) | broker, per call | 403 + a journaled refusal |
| more than `max_calls`, or a response over `max_response_bytes` | broker, per call | 403 + a journaled refusal — an overrun is an error, never a truncation; the message names the EFFECTIVE ceiling |
| a `body_ref` upload over `max_request_bytes` | broker, per call — before connecting upstream | 413 + a journaled refusal naming the ceiling; the upstream sees no byte |
| `max_response_bytes` / `max_request_bytes` zero, non-integer, or above 32 MiB | write time (`VAL`), run start (`RUN-E028`), broker | refused, never clamped |

Brokered HTTP uses text by default (`body` remains a UTF-8 string). The
opt-in `response_mode: "artifact"` writes exact response bytes to the run's
CAS and returns `ref`, `sha256`, `bytes`, and bounded `mime`; `body_ref` plus
`content_type` sends exact stored bytes (mutually exclusive with text `body`)
using `POST_ARTIFACT`, `PUT_ARTIFACT`, or `PATCH_ARTIFACT` as the method. These
markers map to the real granted methods here; older brokers reject them
before dispatch instead of sending an empty upload.
Upload requires a declared blob read and `Content-Type` header permission.
Binary responses and requests are bounded before storage or dispatch (#339):
`runtime_limits.max_response_bytes` bounds the stored response and
`runtime_limits.max_request_bytes` the `body_ref` upload, each **1 MiB by
default** and declarable up to a **32 MiB hard maximum**
(`areev_core::types::capability::MAX_TRANSFER_BYTES`). Out-of-range
declarations are refused with `RUN-E028` at start rather than clamped; an
overrun is `RUN-E022` naming the effective limit — a response is counted as it
is read (chunked and close-delimited bodies included) and refused at limit + 1,
an upload is sized from its stored blob's metadata — before its bytes are
loaded and before any upstream connection. A
read error, including a body the transport cut short of its `Content-Length`,
is explicit, not a successful empty or short body. Text mode is unchanged:
`max_response_bytes` bounds a capability caller's text body as before. The egress Observation carries
only digests, length, MIME, CAS address, and credential *name*; its content
reference keeps the blob live through CAS garbage collection. A consumer must
require `ref` and verify its digest to reject a text-only older peer. The
driver binds its open memory on every run, resume, and fork; without one,
artifact mode refuses rather than writing an unencrypted sidecar.

The declaration is **frozen into the run manifest** beside the runtime, so a
supersession mid-run cannot widen what a module reaches, and a resume or a
verify reads the set the run started with.

**Multi-principal isolation.** Two extra gates matter when one engine process
serves more than one user, which is precisely the case where grain-stored code
run for user A must not reach user B's data or credentials:

- **Private space is not "the internet".** A capability tool under an
  unrestricted egress policy (no `--allow-host`) still cannot reach loopback,
  link-local, private-range or cloud-metadata addresses on its declaration
  alone — a synced memory can declare any host it likes, and reaching the
  local console or `169.254.169.254` takes an explicit
  `--allow-host` entry, the operator's auditable act. The rule binds every
  redirect hop too. Non-capability callers (connectors, `--tool-cmd` tools)
  are unaffected: their reach was always pure host config. (Syntactic only —
  a public hostname that *resolves* to a private address is the documented
  DNS-rebinding limitation of hostname allowlisting, unchanged.)
- **Credentials can bind to a principal.** `--credential name=VAR@principal`
  ties a credential to the run principal that owns it; a run executing as
  anyone else is refused it, and so is a path that bound no principal at all
  (fail-closed). The tool grant says which *tools* may ask; this says which
  *runs* may be answered. The driver binds the run's principal automatically,
  so the gate cannot be forgotten. An unqualified `--credential name=VAR` is
  unchanged — spendable by any run its grant admits.

**Determinism.** A pure `wasm32-areev` module is re-execution-provable: same
module, same input, same fuel. A capability module is deterministic *modulo
journaled effects* — which is why it is a separate runtime name and not a flag
on the first. `verify` is unaffected either way: it answers a tool node from
its journaled **result** grain and does not re-execute the tool, so a
capability tool's result is journaled and superseded like any other. Every
brokered call is additionally recorded as an `egress_call` Observation in
`agent:harness` (see `security-model.md`) — evidence about the run, never a
step of it, so replay stays byte-identical.

Not in this phase: verify-by-re-execution against the recorded call log,
connectors resolved as capability tools by content address, concurrency, and
streaming.

### Backend divergence: reading the memory mid-run (#85)

Whether a **tool subprocess** can read the memory its own run holds depends on
the storage tier, and it silently decides whether an agent design is portable.
The door that works on **every** tier, with no tool holding a handle, is a
[declared read](#reading-the-runs-own-memory-reads) on the plan — the runtime
answers `entity_at` / `related` / `recall` itself:

- **Embedded (Turso file)**: no — the file lock is exclusive, so even a pure
  `RECALL` from inside a tool is refused (`STO-E001`). Use the doors that
  exist: a plan's **`reads`** have the runtime answer as-of reads, bounded
  recalls and graph walks itself, `areev blob get` reads CAS attachments lock-free, a
  **capability tool** reads them with `areev::blob_get` through the broker
  (#106, above), and a **trigger's `--context-query`** has the evaluator
  assemble a saved query's result into the run input before the run starts
  ([triggers](triggers.md)). None of them opens the file from a tool, which is
  why they work while the run holds it.
- **PostgreSQL (server tier)**: yes — any number of handles may hold the same
  schema and reads never block (MVCC), so a tool may open the memory and
  query it mid-run. If your production target is Postgres, tools can read
  their own memory directly; keep `--context-query` for the declaration's
  auditability, portability back to the embedded tier, or both.

**Why the embedded tier has no read-only open, and what it would take.** The
obvious fix — open read-only and let WAL's concurrent readers through — is
deferred rather than declined, and the reason is worth stating so nobody
re-derives it. The exclusive lock is taken inside `turso_core` (`fcntl`
`F_SETLK`), and the pinned `turso = "=0.7.2"` facade exposes no read-only
open; the pin exists because encryption-at-rest is audited against that exact
version. Even past the lock, today's open path *writes*: schema DDL replay, a
second locked `.telemetry.db` sidecar, stamp-gated heal passes, and the
anon-vault write-behind on egress. So it is a store-level project gated on a
deliberate, re-audited engine bump (or a custom no-lock IO implementation),
not a patch — tracked on #85.

## What a run writes (the journal)

The journal proper — intents, results, checkpoints — lives in **the run's own
session namespace** (whatever `--ns` was passed; the store default `shared`
otherwise), so choose the namespace whose retention, anonymization and erasure
policies should govern the record. The run's *administrative* records — the
manifest and its `run:<id>` link, cancel, redelivery, the rejected-response
audit, the run-outcome census, egress refusals — live in the reserved
`agent:harness` namespace; trigger firing records use `agent:triggers`.
(Earlier revisions of this page claimed the whole journal lived in
`agent:harness` — it never did, and an operator who believed it could leave a
run journal outside every policy they had declared. #87.)

**The plan's namespace and the run's are independent, and that cuts both
ways.** A plan is addressed by hash, so any namespace may run it; the record
lands where the run is, not where the plan is. That is the point — the
journal should sit under the retention and erasure policy you chose for it —
but it is also what a forgotten `--ns` looks like: the run works, and every
`RECALL … --ns <the plan's namespace>` that goes looking for its results
finds nothing. `areev run start` says so once, on stderr, when the two
differ:

```
areev: note: this plan lives in namespace 'ap', but the run reads and
journals in 'shared' — pass `--ns ap` if its record should sit with the plan
```

It is a note, not a refusal: running one plan across many tenants'
namespaces is a supported shape, not a mistake.

| Record | Grain | Namespace | When |
|---|---|---|---|
| Intent | Tool grain, `status = pending` | session `--ns` | **before** every effect dispatch |
| Result | supersession of the intent, re-stating its identity + usage | session `--ns` | when the effect settles |
| Checkpoint | State grain (scheduler state + the superstep's decision record), chained by `derived_from` | session `--ns` | every superstep |
| Manifest | the frozen plan resolution, budgets, principal, `initiator` (#293), the **model pin** (`llm`: provider, model, region, host tag, request-profile digest — #287) and the **engine pin** (`engine`: version + `scheduler_epoch` — #288) — plus its `run:<id> mg:harness` link Fact, which carries the run's session namespace (`run_ns`) so the run index can be listed per tenant (#165). A link from before that stamp has no `run_ns`: a scoped listing excludes it and counts it as `unattributed`, an unscoped one shows it with a null namespace. Manifests without the 1.9.0 fields serialize **byte-identically** and resume under anything | `agent:harness` | at start |
| Cancel / audit / redelivery / run-outcome / egress refusals | Facts and Observations | `agent:harness` | as they happen |

Every journal record carries the full effect identity — run id, task path,
node, attempt, effect sequence, kind — and `tool_call_id` is a digest of that
key, so it is both occurrence-unique and reproducible under replay. Because
results *supersede* intents, "the current state of every effect" is just the
heads, and the full history is one `HISTORY` query away.

Read it back:

```bash
areev run-trace --run-id demo-1           # what the run recorded, and what it produced
areev run inspect --run-id demo-1         # manifest, budgets, phase, spend, pending asks, fork lineage
areev run list [--last N] [--offset N]    # recent runs, newest first; stderr notes truncation
areev run list --ns ops                   # ...scoped to one session namespace ("ops.*" also works)
areev runs-touching --hash <GRAIN>        # the reverse join: which runs produced/refined this grain
```

`run inspect`'s `pinned[]` is the **frozen resolution**, not a summary of it:
each row carries the node, the tool name and the executor kind, plus
`executor_uri`, `runtime` and the `capabilities` declaration whenever the
Definition named code. A capability tool therefore cannot read as an
ordinary `--tool-cmd` node (#230) — which it did, identically, until 1.8.0:

```jsonc
"pinned": [{ "node": "vendor_api", "tool": "vendor_api", "executor": "host",
             "executor_uri": "cas://sha256:6c088ed0…",
             "runtime": "wasm32-areev-io",
             "capabilities": [{ "http": { "hosts": ["http://127.0.0.1:7788"], … } }] }]
```

Note `executor` stays `host`: it is the *answering party* (a host, not a
person at `run respond`), and code is how that host answers. What the run
will execute is `executor_uri`.

### Where a run's records live (#301)

Run evidence splits in two, and the split is a tenancy boundary.

**Ids and counters** — the manifest link, the cancel Facts, the run-outcome
census — stay in the memory-wide `agent:harness`, so `run list`, cancel and
the lease paths are unchanged.

**Content-bearing records** have two opt-in placements, because
`read ON agent:harness` — which anything that lists or inspects runs needs —
otherwise disclosed the inputs, fold summaries and outbound-call records of
**every** namespace in the memory, retention and erasure of a namespace left
them behind, and no grant could express "the run records of this namespace
only":

- **`--input-placement run-ns`** stores the run's input as its own State
  grain in the run's session namespace; the manifest keeps only
  `input_ref: {hash}`. `load`, `resume`, `verify`, `shadow` and `fork`
  resolve it, and a missing input grain **refuses** rather than replaying
  against `null` — replaying a different run under the same id would read as
  a journal integrity failure rather than a missing premise.
- **`--harness-ns`** writes fold summaries and egress-call, egress-refusal
  and blob-read Observations to `agent:harness.<run_ns>`, frozen in the
  manifest. A dotted CHILD of the harness namespace, deliberately: keeping
  them out of the agent's own recall scope is why they are not written to the
  run namespace in the first place, `agent:harness.*` still reads everything,
  and one namespace's run evidence becomes separately grantable, retainable
  and erasable.

Without the flags, placement is exactly as before.

## Crash recovery and resume

`areev run resume --run-id ID` picks up from the latest checkpoint.

**Two pins are checked first — before the lease is taken and before any grain
is written**, so a run that must not continue here does not look like it
started to:

- **`RUN-E025 ModelMismatch`** (#287) when the manifest's `llm` pin differs
  from the transport on offer, or when a pinned run is resumed with no LLM at
  all. Everything else that shapes a run is frozen — tool resolutions,
  runtime, capabilities, reads, reducers, every LLM ceiling — but the model
  was not, so a run parked on a human approval could finish days later on a
  different model, provider or region with nothing in the journal saying so.
  `areev run fork` is the sanctioned way through: a fork writes a new
  manifest carrying the new pin and records `fork_of`, making the change a
  recorded decision rather than undocumented drift.
- **`RUN-E026 EngineMismatch`** (#288) when the manifest's
  `engine.scheduler_epoch` differs from this build's. Only the epoch is
  compared, never the version string: a patch upgrade must not strand every
  parked approval run. The epoch moves exactly when a change makes an
  existing journal replay differently — 1.8.3's #251 fix is the recorded
  case, where a verifier holding an older run could not tell tampering from
  "written by 1.8.2".

A manifest carrying neither pin — every run written before 1.9.0 — resumes
under anything, exactly as it always did.

On resume:

- answered asks settle; expired asks are journaled and fail their node;
- a **dangling intent** (crash between intent and result) is adopted —
  looked up before written, so there is only ever one intent grain per key —
  and re-dispatched under the same idempotency key, with the redelivery
  recorded as an Observation (`on_dangling = fail` turns this into
  `RUN-E008` instead, if your tools can't tolerate redelivery);
- a run whose manifest exists but whose first checkpoint was lost
  reconstructs from the manifest's input;
- the **gap itself is recorded**. A run that stopped between supersteps and
  came back later charges nothing for the dead span: it accrues as
  `elapsed_ms` — reported, never billed — exactly as a HITL park does, and the
  reading the driver took on picking the run back up is journaled on the
  superstep it opened. So `areev run inspect` can tell you a run took four
  hours of calendar time and ninety seconds of work, and say which part was
  downtime.

That last point is what makes a recovered run **verifiable**. `step` normally
closes a superstep and opens the next one in the same call, at the same
reading, so a replay would run straight through a boundary the live driver
crashed at — charging the crash gap as active wall and diverging on
`spent.wall_ms` alone. Journaling the resume reading is what lets `verify`
reproduce the boundary instead of guessing at it.

## Human-in-the-loop, precisely

- Asks are addressed by `tool_call_id`, **never by index** — an index is a
  race with the scheduler.
- An approval ask **structurally refuses** `responder == the principal that
  triggered it`, and — since 1.9.0 (#293) — `responder == the run's
  `initiator``. This is not a policy toggle and there is no flag to disable
  it. (The check runs after the known-ask and expiry checks, so an unknown or
  expired ask is reported as such rather than as a separation-of-duties
  refusal.)
- **`--initiator`** (`$AREEV_RUN_INITIATOR`, a trailing parameter on the
  bindings) names who or what a run was started ON BEHALF OF, frozen into the
  manifest. Event, poll and schedule runs execute under an agent's SERVICE
  principal, so `principal` alone could not name the person behind the work
  and the approval check could not refuse them. The field is free-form
  attribution: a value that names no principal — a trigger occurrence id —
  simply never matches a responder. A same-plan fork takes the FORKER's own
  initiator, never the base run's; a subgraph child inherits the parent's.
  MCP never takes it from a client: an identity a caller can assert about
  itself is not an identity.
- A Tool Definition may declare **`ask_kind: "confirmation"`** (#294) — an ask
  the run's own initiator or principal may answer, for a REVERSIBLE write a
  firm's policy lets the requester confirm. Absent means `"approval"`, the
  stricter reading, and an unrecognised value is refused at resolve. The
  value is frozen in the manifest, so a mid-run supersession cannot downgrade
  an approval someone is already parked on, and the run **refuses to start**
  unless the host passed `--allow-confirmation-asks`
  (`$AREEV_RUN_ALLOW_CONFIRMATION_ASKS`): a Definition can arrive in a bundle
  or a pack, and a weakening delivered together with the thing it weakens is
  not a permission — the `--allow-executor` reasoning. Everything else holds:
  the TTL, the `run.respond` grant check, journaled rejections, and MCP still
  refusing its own run's asks.
- On governed sessions (grants present in the file), the responder's own
  grants must cover `run.respond`. Grants are ordinary `mg:permits` Facts —
  they live in the file, sync with it, and are granted in CAL:

  ```
  GRANT run.respond ON ops TO "user:officer" WITH because("refund approvals")
  ```

- Rejected and expired responses are **journaled as Observations before the
  error returns** — a losing approval attempt is audit evidence, not a
  silent 4xx. Since 1.9.0 (#292) that includes the two the code used to
  skip: the separation-of-duties refusal (the most audit-relevant one the
  runtime makes) and the `run.respond` grant refusal, which now loads the run
  first so the record carries `run_id`.
- `--ask-ttl <sec>` on start bounds how long an ask may sit unanswered.
- Refusing an ask is a first-class answer: `--is-error true` journals the
  refusal and fails the node as user-aborted.
- A **subgraph child that parks bubbles its asks to the parent**: the parent
  node parks on the same `tool_call_id`s, and `respond`/`resume` on the
  *parent* route to the child. You answer the run you started, however deep
  the gate sits. The bubble is journaled as the subgraph effect's own result,
  so `verify` reproduces the park from the parent's journal alone — it never
  re-runs the child. A bubble round advances the effect's `effect_seq`, not
  the node's `attempt` — which is what sends the answer back to the *same*
  child, and what keeps a park off the node's retry budget. Separation of
  duties is judged against the parent's triggering principal before the
  answer is forwarded. `--ask-ttl` still applies: an expired bubbled ask is
  forwarded anyway, and the child's own resume settles it as `Timeout`.

The web console (`areev ui`) surfaces pending asks in its **Runs tab**, which
groups runs as *Waiting on you* / *In flight* / *Finished* so an ask cannot be
buried under finished history. Each card carries a per-step strip in plan order,
tinted by what that step did — the same join the Workflows canvas draws as a
status rail, read from one shared index so the two surfaces cannot disagree.
`run.respond` over HTTP refuses shared-token and anonymous callers outright:
only a per-principal credential may approve, because the approver's identity
*is* the audit record, and the Approve/Refuse buttons say so when they are
unavailable. Cancel deliberately keeps the low bar.

The same rule extends to **trusted-header SSO**: an identity asserted by an
authenticating proxy is refused unless the console was started with
`areev ui --sso-approvals allow`. Its proof is one shared, fleet-wide proxy
secret, so whoever holds that secret can approve as anyone — including the
officer named in the audit record. A proxy-asserted identity keeps every read
and review; it loses only this verb. Approvers should hold a per-principal
credential (`areev ui --auth <map>`), whose grants are the same either way —
what differs is the strength of the proof, not the rights.

## Steering a running run (the input queue)

A person can redirect a run without stopping it, and without a plan having to
model a human gate just to *receive* a message:

```bash
areev run input --run-id demo-1 --message "use the express carrier"
```

Each message is a Fact on the run, journaled in the run's own namespace. The
run's driver picks it up at its next wave boundary and the **next superstep**
hands every node it dispatches the queued messages, in order, in their input
under the reserved key `$inbox`.

The driver applies a message only while a superstep is **open**, and an open
is the only thing that drains the queue — so a message is inert for the whole
superstep that observed it. That is what makes `verify` exact: which wave the
driver happened to poll on cannot show up in a checkpoint, so replay places
messages by counting the journal against the checkpoint's own `inputs_seen`,
never against a timestamp.

Three bounds, stated. A message queued *while a node is running* is seen by
the next superstep, not by the node in flight — an abstract node's LLM loop
lives inside one superstep, so steering lands after its turn ends. A message
queued *before the run starts* reaches the second superstep, not the first;
the first superstep's input is `--input`. And a fork does not inherit the base
run's queue.

`run.execute` is the verb — steering advances a run, so it is granted like
starting one, not like the brake. The same surface exists as
`areev_run_input` (MCP), `db.run_input(run_id, message)` (Python) and
`m.runInput(runId, message)` (Node).

## Budgets

`--max-tokens`, `--max-usd`, `--max-wall-ms`, `--max-supersteps`. Spend is
accounted from the journal (usage rides every result grain), so a resumed
run's accounting equals a live run's. Exhaustion is a **parked checkpoint**,
not a corrupted run: `areev run fork` re-opens a budget-exhausted terminal
under raised budgets, continuing exactly where it stopped.

Every axis here is **cumulative**, never per-request: `--max-tokens` bounds
what the run spends in total, not how large any one model request may be. What
bounds a single request is the abstract node's transcript, which is a separate
matter — see [What bounds the transcript](#what-bounds-the-transcript-and-what-does-not).

**`--max-usd` can finally exhaust** (1.9.0, #291). Every effect used to be
stamped `usd_micros: 0`, so a positive dollar budget was unreachable, the
loop's spend flag never raised, and the Verify gate read an unpriced run as
costing `$0` — `0 > 0 × ratio` evaluates `within`, not `not_measurable`. A
transport now prices its own usage through `ToolCallLlm::price_usd_micros`
(defaulted to `None`, which means **unpriced, not free**); a priced effect
carries `usd_priced: true`, which is what keeps the two apart downstream.
`verify` and `shadow` replay the journaled figure and never re-price, so a
later rate change cannot diverge an old run.

**Two run-level ceilings** (1.9.0, #295): `--max-run-effects` and
`--max-tool-calls`. `--max-effects` bounds ONE node attempt; a run's total
was bounded only by nodes × retries × cycles × fan-out × that number.
Exhaustion is a resumable `BudgetExhausted { axis: Effects | ToolCalls }`,
never a node failure — the undispatched call survives as the flow's need, so
`areev run fork` under a raised cap continues exactly there. A tool call is
any dispatched host tool, plan-bound or model-issued; client asks and memory
reads do not count. A run with no cap set keeps a byte-identical `Spent` and
verifies unchanged, because the counter is `skip_serializing_if` zero and is
only incremented when the manifest sets the cap.

**Concurrency caps** (1.9.0, #296): `--max-concurrent` and
`--max-concurrent-per-principal` claim compare-and-set slot rows beside the
run lease. N CAS rows is what makes a cap hard under races — counting and
then acquiring is not. At the cap, `RUN-E027` before anything is written, so
the run id stays free and a trigger firing leaves its item unconsumed. A
parked run holds no slot; a crashed holder's slot is reclaimable after the
TTL. The cap is HOST configuration, never a file truth: how many runs a
deployment may execute at once is a property of the deployment.

## Verify and shadow

```bash
areev run verify --run-id demo-1     # one run: byte-compare replay
areev run shadow --runs a,b,c        # many runs: replay with ZERO effect dispatches
```

`verify` re-drives the run from the **manifest's input** with the clock
scripted from journaled readings and every effect answered from the journal,
writing nothing — then byte-compares each commanded checkpoint against the
stored chain. **Crash-recovered runs verify too**: a superstep the live driver
opened after a resume carries that reading in its decision record, and the
replay rewinds to the checkpoint it just verified and re-enters the boundary
the same way — so the dead span lands in `elapsed_ms` on both sides. Divergence names the differing fields (`RUN-E009`). Canceled
runs verify too (the cancel is replayed from checkpoint state), and
checkpoints past the last verifiable point are reported honestly as
unverified rather than skipped.

`shadow` is the same machinery as a batch pre-flight: the replay path holds
no executor, so "re-execute these journaled runs with zero side effects" is
structural, not a promise. Bare, it answers **consistency**: each named run
is re-driven under its **own** manifest and the report says whether every
checkpoint still matches the journal.

```bash
areev run shadow --runs a,b,c --plan <CANDIDATE_HASH>      # a stored candidate plan
areev run shadow --runs a,b,c --plan-file draft.json       # an unstored draft (validated first)
```

With a **candidate plan** it answers the question a reviewer actually has —
*would the plan change I am about to approve have done better on the runs I
already paid for?* — from the journal, without dispatching an effect or
calling a model. For each run: a manifest is resolved for the candidate the
way `fork --plan` does (V3/V7 re-validation), seeded from the run's recorded
input; the run is re-driven through the pure scheduler with every requested
effect answered from the journal by its exact key (node, attempt, effect
sequence, kind), and `retries`, `max_cycles` and edge conditions taken from
the candidate; the terminal label is `completed` / `failed` / `stalled` /
`canceled` / `budget_exhausted` — or **`out_of_support`** when the candidate
asked for an effect the journal never recorded (a renamed or rebound node, a
branch the live run never took). Out of support is a report field, never an
error, and such a run earns no score; a draft that fails V1–V7 surfaces the
usual `RUN-Ennn`. Spend is the sum over the journaled results the candidate
actually consumed (tokens and USD; wall time is not re-derivable under a
different plan and is not reported). The report gives, per run and in
aggregate, the outcome under incumbent vs candidate, supersteps, effects
replayed and out of support, spend, a `verdict` of `same` / `better` /
`worse` / `out_of_support`, `no_worse` (no scored run is worse), and
`out_of_support_fraction`. When the candidate *is* the incumbent plan the
rehearsal is also a verify: every checkpoint is byte-compared and the row
carries `identity.consistent`. `effect_dispatches` and `writes` are stated
as 0 in the artifact. A canceled run rehearses as canceled (the cancel the
live driver saw is fed at the same superstep). The precedent is Dream-RSI
(arXiv 2609.14858 §3): a completed discovery tree is a replay simulator, and
replay can only answer effects it recorded.

### Rehearsing a candidate *version* (`--reexecute pure`)

Answering every effect from the journal by its key means the **binding is
never consulted** — so a candidate that changes nothing but a tool's *bytes*
(the same node, rebound to a Definition whose `executor_uri` names a
different blob) replays the old blob's result and rehearses as `same` by
construction. That is the patch class most likely to change an answer, and
`--reexecute pure` is the opt-in that closes it:

```bash
areev run shadow --runs a,b,c --plan-file draft.json --reexecute pure \
  --allow-executor <ADDR>,<ADDR> --sandbox-cmd areev-sandbox
```

A bound node whose **candidate** Definition is a `wasm32-areev` module — pure
Tier C, whose frozen import set is exactly `areev::emit`: no clock, no
filesystem, no sockets — is re-run in the sandbox under its pinned `fuel` and
`max_pages`, on the input the replayed state built, instead of being answered
from the journal. Everything else is still answered from the journal and
listed under `not_reexecuted` with the reason: native blobs (a program, not a
proof), `wasm32-areev-io` (a capability module reaches the network through the
broker, so re-running it *would* be an external effect), client, abstract,
subgraph and memory-read nodes, and any address this host has not pinned. The
host authorization is the same one a run takes and it is checked the same way
— a rehearsal is the same act of running someone's code, so `--allow-executor`
and `--sandbox-cmd` are required and an unpinned candidate address is refused
by name rather than silently run.

The report then adds, per run: `reexecuted` (nodes that ran),
`not_reexecuted` (`{node, why}`), `sandbox_executions`, and the terminal
merged-context diff as **key paths only** —

```json
{ "changed_keys": ["/Amount", "/verdict"], "added_keys": [], "removed_keys": [] }
```

RFC 6901 pointers, **never values**, so the report can go on a control
channel that must not carry content. Point the option at the *incumbent*
plan and the rehearsal is also a verify: the module runs and every checkpoint
still byte-compares, which is the Tier C table's "re-execution-provable" row
cashed in rather than asserted — and what makes a reported `changed_keys`
evidence about the candidate rather than noise about the sandbox. `effect_dispatches` stays `0` and keeps
meaning *no external effect*: what ran is counted separately as
`sandbox_executions`, never folded in. `writes` stays `0` too. Without the
option the report is byte-identical to one taken before the option existed —
every field above is absent, not empty. The diff is reported only when the
replay reached a terminal state with nothing out of support; a context
abandoned mid-run would diff as wholesale removal and read as a finding.

The mode is available on `areev run shadow` and in the bindings
(`options` / `optionsJson`). It is deliberately **not** on the MCP tool or
the `/api/run/shadow` endpoint: those are reads served by hosts that hold no
executor pin, and a read that executes code is not a read.

The same rehearsal is reachable from `areev_run_verify` on MCP (pass `plan`
and `runs`), from `GET /api/run/shadow?runs=…&plan=…` and `POST
/api/run/shadow` (which also takes an unstored `plan_body`), from
`run_shadow(run_ids, plan=…, plan_body=…, options=…)` /
`runShadow(runIds, plan?, planBody?, optionsJson?)` in the bindings (the
options object takes `reexecute` plus the pins a pure re-execution needs —
`allow_executor`, `sandbox_cmd`, `executor_cache`, `executor_timeout_secs`;
snake_case is canonical, camelCase is accepted), and from the Workflows
canvas (**Rehearse** on a draft, against the last runs of the open plan). It
is also what
[Areev Loop](loop.md) attaches to a `plan_revision` proposal as its `replay`
block, and what a `plan_replay` policy refuses an applicable revision on —
see the `plan_replay` policy field there.

## Time travel and migration (`fork`)

```bash
areev run fork --run-id demo-1 --as-run demo-1b --at 1          # branch from superstep 1
areev run fork --run-id demo-1 --as-run demo-2  --plan <HASH>   # migrate onto a NEW plan
```

Same-plan forks inherit the manifest's pins and the scheduler state at the
chosen (closed) checkpoint; new-plan forks are migrations — the new plan is
fully re-validated and restarts at its entry with the inherited context as
input. The fork's seed checkpoint `derived_from`s the base checkpoint and an
`mg:fork_of` Fact indexes the lineage, so ancestry is a query. A cancel on a
base run is honored across its fork descendants.

## The kill switch and the oversight report

```bash
areev run cancel --run-id demo-1 --because "operator abort"
areev run oversight-report --run-id demo-1     # or --plan <HASH> for the newest run of a plan
```

`cancel` writes a marker Fact — deliberately the **lowest-privilege** run
verb, because a brake must never be blocked by missing privilege. A live
driver drains at its next superstep boundary; `resume` finalizes a parked
one, and a **paused** one (below) is finalized by the cancel itself.

### Pause: a resumable stop the host asks for (#344)

```bash
areev run pause  --run-id demo-1 --because "quota reached"
areev run resume --run-id demo-1            # continues it — same run id, no fork
```

`cancel` is terminal; a budget axis parks a run that only a `fork` continues.
`pause` is the third stop: the **host** asks, the run parks resumably, and
`resume` continues it under the **same run id, manifest and pins**. It is what
a host that meters work in its own units — rows extracted, documents
processed, anything counted from a node's output rather than tokens or
dollars — needs when its count crosses a limit: not "finish and overspend",
not "cancel and redo", but "stop where you are and wait".

- **Where it stops.** `pause` writes a request Fact (`mg:run_pause`, in the
  run's namespace, with the principal and the reason) and returns. A live
  driver polls it at every wave boundary, with the steering queue, and honours
  it where it would otherwise **open** the next superstep: the open superstep
  finishes, its results merge and its checkpoint is written exactly as in an
  uninterrupted run, and then nothing further is dispatched. So a pause asked
  while node `b` is executing takes effect after `b` — it never interrupts a
  node. Terminal outcomes still win: a run with nothing left to do completes
  (or is canceled, or exhausts a budget) rather than pausing.
- **What it looks like.** The driving `start`/`resume` returns `{"parked":
  {"kind": "paused", "reason": "paused", "superstep", "paused_by", "because",
  "requested_at", "paused_at", "checkpoint", "asks": []}}`, and its event
  stream ends at `RunPaused` the way a gate's leg ends at `AskRaised`. The run
  holds **no lease and no concurrency slot** while paused, like any park, so
  any driver may continue it. `inspect` reports `phase: "paused"` and a
  `pause` block — `status`, `paused_by`, `because`, `requested_at`,
  `paused_at`, `superstep`. The driver journals where it parked
  (`mg:run_paused`).
- **Continuing.** `resume` on a paused run consumes the request — it writes
  `mg:run_unpause` naming the request and who resumed — and continues from the
  parked checkpoint. Nothing re-executes: nothing past the checkpoint was ever
  dispatched. A consumed request never re-pauses the run; a new `pause` is a
  new request. A request made while the run is parked on a **human gate** is
  not consumed by the resume that settles the answer — it applies at the next
  boundary after it, which is what the asker was promised.
- **Verify.** The pause leaves nothing in scheduler state
  (`EventIn::PauseRequested` holds a single `step` call and is never stored),
  so the parked checkpoint is byte-identical to the uninterrupted run's, and
  the resumed superstep is an ordinary resume boundary: `verify` replays a
  paused-then-resumed run through the rule it already has for a crash, with
  the paused span accrued as `elapsed`, never as wall. A run still paused
  verifies up to the pause; a paused run canceled afterwards verifies through
  its canceled terminal.
- **Refusals and interactions.** `pause` needs `run.execute` — the grant
  `resume` takes; stopping a run resumably is a scheduling decision, unlike
  cancel's deliberately low bar. It is **idempotent**: asking again while a
  request stands writes nothing and answers `already: true` with the standing
  request. It is refused with **`RUN-E029`** when the run already finished
  (completed, failed, stalled, canceled, out of budget) or a cancel is pending
  against it — **cancel wins over pause**. `cancel` on a paused run finalizes
  it as canceled at once: nobody is driving it, and it is parked precisely
  because its host has not decided what happens next.
- **Scope, stated.** A pause stops the run it names at that run's own
  boundary; a subgraph child running inline finishes as the parent's one
  effect first. It does not raise Core's budgets — `fork` remains the path for
  those.

Surfaces: `areev run pause`, MCP `areev_run_pause`, `db.run_pause(run_id,
because=…)`, `m.runPause(runId, because)`. The binding call is safe from an
`on_event`/`onEvent` callback — the shape a metering host uses:

```js
let asked = false
const onEvent = (line) => {
  const e = JSON.parse(line)
  if (e.event === 'CheckpointWritten' && !asked && overLimit()) { asked = true; m.runPause('r1', 'limit reached') }
}
const out = JSON.parse(await m.runStart(plan, 'r1', '{}', toolCmd, ...Array(13).fill(null), onEvent))
// out.parked.reason === 'paused' — later, once the limit is raised:
await m.runResume('r1', toolCmd)
```

`oversight-report` answers the EU AI Act **Article 14** questions as a
command: where a human can intervene (the client-gated nodes), who is
authorized to (the `run.respond` grants in the file), what expires when
(ask TTLs), and how fast the kill switch actually drained — **measured**
from the journaled cancel Fact to the terminal checkpoint close, not
asserted. The article→capability→command map is
[`docs/eu-ai-act.md`](eu-ai-act.md). Both `inspect` and `oversight-report`
are also `Runner` methods, reachable in-process from the Python/Node
bindings (`run_inspect`/`run_oversight_report`) — a tenant-deployed agent
service renders these without shipping the CLI binary just for two
read-only reports.

## LLM nodes (abstract nodes)

An unbound node becomes an abstract node: a journaled tool-calling loop.

```bash
areev run start --workflow <WF> --run-id r1 --input '{}' \
  --model claude-sonnet --llm-max-tokens 4096 \
  --tool-cmd 'my-tools'          # the model may only call the plan's HOST pins
```

- Providers: `claude-*` (Anthropic), `openai:*`, `ollama:*`, or any
  OpenAI-compatible endpoint; keys come from the environment
  (`ANTHROPIC_API_KEY`, `OPENAI_API_KEY`, …). No model runs unless you
  configure one — an abstract node without an LLM is `RUN-E006` at start,
  not a silent skip.
- **A node is not abstract just because this run cannot see its
  Definition.** A named node resolves through the RUN's namespace, so a plan
  started somewhere other than where its tools were authored would find an
  empty catalogue — and, with a model configured, quietly become an abstract
  node: the Definition's `executor_uri`, runtime and capabilities dropped, a
  model answering instead, and the run reporting Completed having called
  nothing. So resolution asks one more question before it falls through. If
  the plan grain itself lives in another namespace and a Definition of that
  name is *there*, the run is refused at start (`RUN-E004`), naming the node,
  the namespace searched and the one that would have worked (#230):

  ```
  RUN-E004: node 'vendor_api' has no binding, and namespace 'shared' — where
  this run reads and journals — holds no Tool Definition named 'vendor_api'.
  The plan grain itself lives in namespace 'ap', which does. Start the run
  there (`--ns ap`), or bind the node to a Definition by hash so it resolves
  from any namespace
  ```

  A node that names no Definition anywhere, including the plan's own
  namespace, is genuinely abstract and behaves exactly as before. A **bound**
  node is unaffected either way: a binding is a content address, so it
  resolves from any namespace at all.
- The tools *offered* to the model are exactly the manifest's pinned host
  Definitions, **each once, however many nodes bind it** — a Definition bound
  to two nodes (a terminal step reached from two branches) is one offered tool,
  first occurrence in plan order, because every provider refuses a tools list
  with a repeated name (#270). De-duplicating the offer changes nothing about
  which executor a bound node runs. Tool arguments are validated against the pinned schemas —
  strictly, with one re-prompt on violation; an unknown tool name gets one
  re-prompt, then fails the node.
- **A Definition with a dot in its name is callable.** Anthropic and OpenAI
  forbid `.` in a tool name, so `receipt.prepare` is offered to the model as
  `receipt_prepare` — and a call to *either* spelling resolves to the pinned
  Definition, dispatches under its canonical name, and journals under it
  (#251). Exact match wins, so a plan offering both `a.b` and `a_b` still runs
  the one that is literally named what the model called. Only a name matching
  neither form is unknown; the correction lists the tools the way the model
  was shown them. Nothing about naming a tool is a plan-authoring constraint.
- A tool failure inside the loop is **model-visible** (the model can react),
  never scheduler-retried behind its back.
- Every model turn and every tool call is journaled with token usage, so
  budgets recompute from the journal and **verify never calls the model**.
- Per-turn token reservation means an un-dispatched turn survives budget
  exhaustion as a need — the raised-budget fork resumes exactly there.
- **From Python and Node**, `run_start`/`runStart` and `run_resume`/`runResume`
  take the same `model` (plus `base_url`/`key_env` and `llm_max_tokens`). The
  spec is resolved *before* the run is journaled, so a bad provider or a
  missing key fails without leaving behind a run that could never advance. The
  backend is host config and is deliberately not journaled with the run, which
  is why `run_resume` takes it too rather than recovering it from the manifest.

### What bounds the transcript (and what does not)

An abstract node's transcript is the **whole attempt** — every model turn,
every tool result, verbatim — and each turn sends all of it. Three things
follow, none of them visible from the flag names:

- `--llm-max-tokens` is the **output** ceiling for one call, and the per-call
  reservation checked against the token budget. It says nothing about how
  large the prompt may grow.
- `--max-tokens` bounds **cumulative** spend across the run, not the size of
  any one request. A run can sit far under its token budget and still issue a
  request no provider will accept.
- The only bound on the loop itself is a **count**: `--max-effects N` (default
  **16**) effects per node attempt, with model turns, model-issued tool calls
  and re-prompts sharing one counter. A node that needs one more fails
  `ExecutorError: llm loop exceeded max_effects_per_attempt` — at the default,
  and at one tool call per turn, that is roughly eight turns, so an agent doing
  real work usually wants it raised. The number is **frozen into the run
  manifest** at start, so a `resume` and a `verify` bound the loop exactly as
  the start did; passing it on a resume changes nothing. Same knob as
  `max_effects_per_attempt` (MCP `areev_run_start`, Python, Node) and on
  `trigger run`, which starts runs nobody is watching.

So a transcript that outgrows the model's context window is **the provider's
error and nothing more**: the runtime does not summarize, trim, or re-plan it.
The rejection is terminal, so the node fails with the provider's message as its
detail. Two shapes reach it, and only the first has a bound today.

**One oversized tool result** — a file read, a log dump, a JSON dump — can
exhaust the window inside a single round, because a tool result otherwise
enters the transcript exactly as the tool returned it. No summary can shrink a
single entry, so this needs its own bound:
`--llm-tool-result-chars N` (`llm_tool_result_chars` on MCP, Python and Node;
default: unbounded). Over the bound, the model is shown

```json
{"truncated": true, "chars": 41822, "head": "…first N/2…", "tail": "…last N/2…",
 "journal": {"attempt": 1, "effect_seq": 3}}
```

— the true length, the two ends, and the journal coordinates of the whole
thing. **The journal is never bounded**: the full result grain was written
before the scheduler ever saw the outcome, so `run-trace` and a DSAR still
show every byte and `verify` still replays byte-identically. The number is in
**characters** because that is what it counts; there is no tokenizer in the
runtime, and a "token" bound computed as `chars / 4` would be a guess wearing a
precise name. The bound applies to a failed tool's detail too (it carries the
tool's stderr), and is frozen in the manifest like every other run ceiling.

It does **not** bound the state a node is handed. A reducer that accumulates a
large value puts that value in `messages[0]` of every abstract node below it;
that is a plan-shape question, not a transcript one.

**Or the loop simply grows** — enough turns, each carrying the ones before it,
to pass the window under the effect cap. That is what `--llm-context-tokens`
is for.

### The fold: what happens when the transcript outgrows the window

`--llm-context-tokens N` (`llm_context_tokens` on MCP, Python and Node) sets a
ceiling on the whole transcript. **You usually do not have to** — see
"Folding is on by default" below. Before each turn the
scheduler compares the **provider's own reported prompt tokens for the previous
turn**, plus the `--llm-max-tokens` reservation for this one, against it. Over
the ceiling, it emits one more turn — a **summarizer** — over the middle of the
transcript, and replaces that middle with the answer:

```
messages[0]  the node's instruction + input      ← never folded
messages[1]  [Areev fold 1: transcript entries 1..9 of attempt 1 were replaced
             by this summary, journaled at effect_seq 9. The full record — every
             turn and every tool result — is in this run's journal.]
             <the summary>
messages[2…] the last few entries, verbatim      ← the kept tail
```

Seven things are worth knowing before you turn it on.

- **Nothing is deleted.** The fold edits scheduler state; every folded turn and
  tool result is still a journaled intent + result grain, still visible in
  `run-trace`, still addressable by `(run, task_path, node, attempt,
  effect_seq)`. The journal is the archive — that is the whole design
  ([ARCHITECTURE.md](../ARCHITECTURE.md), "The journal is the archive").
- **The summarizer is an ordinary journaled turn.** It spends an effect and a
  token reservation like any other, its request is in the journal (including the
  exact prompt, versioned), and `verify` answers it from the journal — so a
  folded run still replays byte-identically and **still never calls the model**.
- **The summarizer is offered no tools**, so a fold can have no side effects.
- **The trigger is the provider's number, not an estimate.** Areev's `chars / 4`
  token estimator belongs to `ASSEMBLE`; the runtime never consults it.
- **Raise `--llm-max-tokens` when you use folds.** A summarizer turn runs under
  the same per-call output ceiling as every other turn, and the default (1024)
  is tight for summarizing a long middle.
- **`RUN-E024` when nothing is foldable.** If the node's input and the kept tail
  alone exceed the ceiling there is no middle to summarize, and a second fold
  would only summarize summaries — so the node fails, naming the ceiling and the
  three ways out (raise it, bound the tool results, split the node).
- **The measurement lags one round — so set `--llm-tool-result-chars` too.**
  See below; this is the one way a ceiling on its own still lets a node die.

The ceiling is frozen in the manifest at start, like every other run limit, so a
`resume` folds exactly where the start would have. `areev run inspect` reports
the effective limits under `limits`, which is where to look when you want to
know what a run is actually bounded by.

#### Folding is on by default

You do not have to set a ceiling for a long agent to survive. Two mechanisms
cover it, and which one applies depends on what your provider will tell us.

**A ceiling derived from the model.** When the backend can state its own
context window, an unset `--llm-context-tokens` defaults to *window − the
per-call output reservation*, and an unset `--llm-tool-result-chars` follows
from that (a quarter of the ceiling in tokens, converted at 4 chars/token).
Setting either flag overrides its derived value. Only `claude-*` reports a
window today: 200,000, and deliberately as a **floor** rather than a
specification — a model with a larger window folds a little earlier than it
strictly must, which costs one summary, whereas a number that is too high
costs the run. A stale table that fails *silently* is the thing being avoided
here, so there is no table: one number, for one family, checked.

**The provider's own refusal.** Where a provider states overflow
*structurally* — OpenAI-compatible endpoints return
`error.code = "context_length_exceeded"` — the runtime does not need to
predict anything. The refused turn is journaled as a failed effect, the
transcript is folded, and the **same turn is re-sent** on something smaller.
This is what closes the one-round measurement gap described above: the ceiling
can be unset, or set too high, or right about a transcript that has since
grown, and the provider's verdict beats all three.

Areev never reads a provider's error *prose* to reach that conclusion. A seam
that matched on wording would work until a vendor reworded it and then fail
silently. Providers that report overflow only in prose (Anthropic's
`invalid_request_error`) are covered by the derived ceiling instead — which is
exactly why that floor exists.

Neither path can loop: a fold costs an effect, so `--max-effects` bounds the
whole thing, and a refusal with nothing left to fold is `RUN-E024` naming the
provider's limit rather than inventing a ceiling nobody set.

**Folding is visible to the loop, two ways.** Every terminal run records how
many times it had to summarize itself, and [Areev Loop](loop.md)'s
`run_outcome` analyzer flags a workflow that needs one on essentially every
run: a plan-shape signal (split the node, bound its tool results, or accept the
summaries), advisory, nothing to auto-apply. A fold now and then is the
mechanism working and says nothing.

Each summary is also written as an Observation in `agent:harness`
(`observation_kind: "fold_summary"`, carrying the run, node and the
`effect_seq` of the folded range). The same text is already in the fold
effect's result grain, but a journal Tool grain is not reachable by recall —
its payload sits in `tool_content`, which the text index does not cover — so
what an agent worked out over fifty turns would be findable only if you already
knew the run. As an Observation it is typed, indexed, and part of what the loop
reads.

It is **evidence about the run, not the agent's memory**, and the namespace
says so. A fold summary is working state — *"round 4 outstanding"* — and
recording that verbatim as a durable memory would pollute recall rather than
improve it. Turning one into a lesson that is still true tomorrow is the LLM
verifier's job ([`loop.md`](loop.md)): it may propose a lesson **citing** a
summary, and the four gates decide whether it is ever applied. Nothing here
reaches the agent's namespace on its own.

That path needed a deliberate carve-out to work at all. An all-namespace scan
hides every `agent:` namespace, because those also hold the file's grants and
Tier-2 audit records and an analyzer sweeping them as ordinary memory once
proposed tombstoning the grants. So the loop reads fold summaries by
**explicit** namespace and by an explicit list of observation kinds
(`HARNESS_EVIDENCE_KINDS`), with a reserve of its own — the governance
exclusion is untouched, and adding a kind to that list is a reviewed one-line
decision rather than a blanket un-hiding.

#### The two bounds are complementary, not alternatives

The check runs on the **previous** turn's reported number, so the transcript it
actually sends is bigger than the one it measured — by exactly one round: that
turn's assistant entry, plus that round's tool results. Both were appended after
the provider reported.

That gap is the ceiling's blind spot, and on its own it is **unbounded**:

```
turn 4 sent, 88,000 tok → provider reports 88,000   ← the measurement
  + assistant entry (the tool call) ....    200 tok │ appended after,
  + tool result: a 40k-token log dump . 40,000 tok  │ unmeasured
                            transcript = 128,200 tok

turn 6 check: 88,000 + reserve 4,096 = 92,096 ≤ 100,000 → no fold
              …and the turn goes out carrying 128,200. The provider
              rejects it, and the node dies with the ceiling set.
```

`--llm-tool-result-chars` is what makes the blind spot finite: with it, one
round can add at most the assistant entry plus (tool calls in that round × the
cap). Bound the results and the same run measures 91,200, fits, and folds a few
rounds later with headroom in hand.

So: **set both.** The per-result bound caps how far the transcript can move
between measurements; the ceiling caps the total. A ceiling alone is a bound on
a transcript that no longer exists. (This is the same shape as the §6.7 budget
overshoot, which is likewise bounded by one dispatch rather than eliminated —
stated, not hidden.)

What survives either way is the **journal**: one intent + result grain per
effect, written before the node failed and untouched by its failure. Every turn
and every tool result is still addressable, and `run-trace` still shows all of
them. The transcript is scheduler state; the record is the journal.

## Decisions in a run

A **decision backend** answers typed questions with calibrated probabilities
— no text, one call, 70–500 ms hosted ([decision-model-proposal.md](decision-model-proposal.md)).
The runtime uses one in three places. All three are opt-in, all three fall
back to what the run did before, and none can approve, apply or gate anything:
a decision model may score and order, but only code omits or branches.

The host installs a backend with one call, `Runner::with_decider(backend)`,
which wraps whatever executor stack it already built. Hosts install their
configured chain (`--decide <chain>` / `--decide-cmd`, `$AREEV_DECIDE`)
through it. The run pins what it started under in its manifest
(`decider: {describe, calibrated}`), so `resume` and `verify` ask exactly what
the run asked, whatever host they run on. A run on a host with no backend pins
nothing, asks nothing, and writes the same bytes it always did.

### The decision node (`executor_uri: "areev://decide"`)

A Tool Definition whose `executor_uri` is the reserved `areev://decide` is a
**decision node**. The driver answers it through the host's backend, never
through `--tool-cmd` and never through the pool. It journals the answer as an
ordinary Tool execution grain under the Definition's own name, so `run-trace`,
`step-actions` and the loop's `run_outcome` see it like any other tool.
Edges branch on the answer in the frozen condition grammar:

```json
{"kind": "definition", "tool_name": "triage", "executor_uri": "areev://decide",
 "input_schema": {"type": "object", "properties": {"state": {}, "questions": {"type": "object"}},
                  "required": ["state", "questions"]},
 "strict": true,
 "decide": {"questions": {"route": {"type": "choice",
              "instructions": "Does this item need a person today?",
              "criteria": {"escalate": "a person must act today", "ignore": "routine"}}}}}
```

```json
{"nodes": ["triage", "escalate", "file"],
 "edges": [{"src": "triage", "dst": "escalate", "cond": "triage.answers.route.choice == \"escalate\""},
           {"src": "triage", "dst": "file",     "cond": "triage.answers.route.choice == \"ignore\""}],
 "bindings": {"triage": "<definition hash>", "escalate": "<…>", "file": "<…>"}}
```

- **The request.** The node's input is the run's state, as for any bound
  node. `state` is the input's `state` key when it has one, and the whole
  input otherwise. `questions` are the Definition's `decide.questions`, frozen
  at start. They win over any `questions` key in the input, so a payload a
  trigger handed the run cannot change what the plan asks. Without frozen
  questions, the input must carry them. A `strict` Definition's
  `input_schema` is checked against that `{state, questions}`, the same way a
  model's call to a strict tool is checked. A violation is
  `SchemaValidationFailed`, and nothing is asked.
- **The result** is `Decision::to_json()` — `{answers, model, provider,
  calibrated, latency_ms, usage?}` — under the node's own id. `decide.into`
  names a different key. The decision's usage counts against the run's token
  budget.
- **No backend, no run.** A plan that binds a decision node on a host without
  a backend is refused with `RUN-E030` at start, naming the node, before the
  run exists. `resume` checks the same before it takes the lease. A malformed
  `decide` declaration (an unaskable question, an unknown key, an `into` that
  is empty or `$`-prefixed) is `RUN-E019` at start.
- **Failures follow the node's retry table.** Deadline → `Timeout`;
  transport, malformed answer, rate limit or an exhausted chain →
  `ExecutorError` (retryable under `retries`); an unaskable question →
  `SchemaValidationFailed`; a refused egress → `Unknown`. A decision node has
  no deterministic fallback, because it is the branch point you asked for.
  Give it `retries`.
- A decision node can be a `$send` target, which gives one judgment per
  fanned-out item. It is never offered to an abstract node's model.

### The decision-guided fold

When the [fold](#the-fold-what-happens-when-the-transcript-outgrows-the-window)
triggers and the run pinned a **calibrated** backend, the backend gets the
first look at the foldable window. The scheduler emits one decision effect,
journaled as `mg:decide`. The request's `state` is the window, oldest first,
with every tool result replaced by a note (`"ok, 4213 chars (omitted)"`) —
the backend never sees a result's contents. For each result entry `n` it asks
two yes/no questions: `keep_call_n` ("does the fact that this call happened
still matter?") and `keep_result_n` ("are its contents still needed
verbatim?"). Only on a calibrated answer:

| Answer | What the next turn sees |
|---|---|
| `keep_result ≥ 0.5` | the entry, verbatim |
| `keep_call ≥ 0.5 > keep_result` | the call, and the result cut to 300 characters plus a note |
| both `< 0.5` | nothing — dropped from the **prompt** |

A result never leaves its call behind. A round (an assistant entry and the
results that answer it) leaves the prompt only whole, assistant entry
included. In a round with anything kept, every call stays and a result marked
for dropping is truncated instead. A note naming the dropped and truncated
entries, the backend and the `effect_seq` takes the window's place, as the
summarizer's note does.

Then the next turn goes out and the provider measures the pruned transcript.
If it is **still** over the ceiling, or the provider refuses it, the next
trigger goes straight to the summarizer fold. A window is never asked about
twice.

It **fails open**. An uncalibrated answer, a failed call, a malformed answer
or an answer that keeps everything means nothing is edited, and the
summarizer fold runs over exactly the window it would have folded with no
backend. A run whose pinned backend is uncalibrated is never asked at all: an
uncalibrated backend may reorder but never omit, and this omits.

### Narrowing the tool offer

An abstract node is offered every pinned host tool. When a calibrated backend
is pinned and **more than 8** tools are, the scheduler asks one `choice` over
the tool names before the node's first turn. The state is the node's
instruction and input, the plan's `name` (and `goal`/`description`), and each
tool's one-line description. The offer keeps the **top 8 by probability plus
any tool at p ≥ 0.05**, in manifest order. The manifest's pinned set is the
universe, and narrowing only removes from it. A pinned tool the model was not
shown is unknown to it: calling one is the unknown-tool re-prompt. An
uncalibrated answer, a failure or a table that does not cover every tool
leaves the full offer.

The narrowed set rides every turn's journaled input as `offer` —
`{tools, seq, provider, model, calibrated, latency_ms}`. The driver offers
exactly those Definitions, keyed off the journal the way the summarizer's
no-tools rule is. That makes `verify` offer the same set.

### What the journal holds

- **Every decision is an effect**: an intent and a result grain under the
  node, attempt and `effect_seq` it was asked at. A crash re-delivers it under
  the same key. `verify` and `shadow` answer it from the journal and **never
  ask the backend again**.
- **Provenance travels with the answer.** `provider`, `model`, `calibrated` and
  `latency_ms` are in the result grain (and so in the state a decision node
  writes). A narrowing carries them in the turn's `offer`. A fold carries them
  in its record.
- **A fold's record** is in the superstep's decision record (the checkpoint's
  `decisions.folds`). It is the `kind: "decide"` variant of a fold, alongside
  the summarizer's `input.fold`: `{kind, v, node, attempt, seq, from, to,
  kept, truncated, dropped, applied, reason?, provenance?}`. `dropped` names
  every entry removed from the prompt, and every one of them is still a
  journaled grain. `applied: false` with a `reason` records a fail-open.
- **Versioned wording.** The questions' text rides the journal inside each
  request. `FOLD_DECIDE_V` versions it the way `FOLD_PROMPT_V` versions the
  summarizer's prompt, which did not change.
- **Egress.** A decision's `state` crosses the same namespace boundary an
  abstract node's prompt does (an `egress`/`both` anonymization policy
  pseudonymizes it first), and the host's backend should itself be the
  pseudonymizing chain (`PseudonymizingDecider`). If the transform fails,
  nothing is sent.

## Fan-out (`Send`)

A node's result may carry the reserved `$send` key:

```json
{"$send": [
  {"node": "worker", "input": {"v": 1}},
  {"node": "worker", "input": {"v": 2}}
]}
```

Each spawn executes the target node with its own input under a task path
(`parent/0000`, `parent/0001`, …); the batch joins before the target's
downstream edges fire. Validation is all-or-nothing (one malformed spawn
fails the batch, not half of it), and declared reducers (`append`, `sum`, …)
make the merged results order-independent.

A spawn target is a **host tool node or an abstract node** — never a client
gate, a subgraph, or the spawner itself. Fanning out to an abstract node
gives each task its own LLM loop, journaled under its own task path
(`node@parent/0000`), so N documents get N independent agent loops from one
plan instead of N nodes.

## Watching a run

- `--events` streams structured run events (JSON lines) to stderr while
  stdout stays the machine surface.
- `--otel-endpoint http://collector:4318` exports one OTLP/HTTP trace batch
  per run at completion; resumes join the same trace (the trace id derives
  from the run id). `http://` only — TLS is the collector's job in this
  profile, so point it at a local agent or sidecar.
- Streaming is **observational only**: journals are byte-identical with no
  subscriber, a normal one, or a slow one — pinned by test.

### The span shape

Three levels, and the middle one is synthesized rather than journaled:

| Span | When | Name | Kind |
|---|---|---|---|
| `areev.run` | every run | `areev.run` | INTERNAL |
| `invoke_agent` | one per **abstract node activation** (per attempt) | `invoke_agent {node}` | INTERNAL |
| `chat` | one per model turn | `chat {model}` | **CLIENT** |
| `execute_tool` | one per tool the model called | `execute_tool {tool}` | INTERNAL |
| *(unnamed)* | any other journaled effect — a bound Host node, a Client ask, a subgraph | the node id | INTERNAL |

A plain bound workflow node is deliberately **not** dressed up as
`execute_tool`: it is not a GenAI operation, and labelling it one would put
tools nobody's model chose into a model-spend view.

### The GenAI attribute contract

Spans carry the current OpenTelemetry
[GenAI semantic conventions](https://opentelemetry.io/docs/specs/semconv/gen-ai/),
so a GenAI-aware backend (Grafana, Langfuse, Arize, Datadog LLM
Observability, …) classifies model spend, tool calls and finish reasons
**with no Areev-specific code**. The internal argument for aligning with
semconv rather than inventing a vocabulary is in
[`areev-adaptive-agents-proposal.md`](areev-adaptive-agents-proposal.md)
("a cheap standards-alignment move worth taking now" — note also that
OTel's own direction of travel is to model *memory* as a thing that emits
telemetry).

| Attribute | On | Source |
|---|---|---|
| `gen_ai.operation.name` | chat / execute_tool / invoke_agent | the effect's `EffectKind` + whether an abstract node owns it |
| `gen_ai.provider.name` | all three | `ToolCallLlm::provider()` — `anthropic`, `openai`, `ollama`, `gcp.vertex_ai`, `openrouter`; `_OTHER` for a host's own backend |
| `gen_ai.request.model` | chat, invoke_agent | `ToolCallLlm::model()` |
| `gen_ai.response.model` | chat | the request model — see the caveat below |
| `gen_ai.request.max_tokens` | chat | the manifest's `llm_max_tokens` (§6.7's per-dispatch reservation), default 1024 |
| `gen_ai.request.temperature` | chat | 0.0, the fixed temperature abstract-node turns are issued at — **absent** when the transport sends none (see below) |
| `gen_ai.usage.input_tokens` / `.output_tokens` | chat | the journaled `EffectOutcome::Completed` figures — the same numbers budgets spend |
| `gen_ai.response.finish_reasons` | chat | the result's `stop_reason` (`end_turn` / `tool_use` / `max_tokens` / `other`), always an **array** |
| `gen_ai.tool.name` | execute_tool | the pinned Tool Definition's name |
| `gen_ai.tool.call.id` | execute_tool | the **model's** call id, the one the transcript's `tool_result` addresses — *not* `JournalKey::tool_call_id()`, which is Areev's journal digest |
| `gen_ai.tool.type` | execute_tool | `function` |
| `gen_ai.agent.name` | all three | the abstract node's id |
| `gen_ai.agent.id` | all three | `agent:{run}/{task_path}/{node}/{attempt}` — per **attempt**, because a retried node is a second invocation |
| `gen_ai.conversation.id` | chat, invoke_agent, root | the run id: one journal, one transcript, and a resume continues both |
| `error.type` | any failed effect | the `FailCause`, snake_cased (`timeout`, `executor_error`, `schema_validation_failed`, `user_aborted`, `unknown`) |

Two deliberate absences:

- **`gen_ai.usage.cost` is present exactly when the effect was PRICED**
  (1.9.0, #291). Core still prices nothing itself; a transport prices its own
  usage through `ToolCallLlm::price_usd_micros`, and one that does not
  returns `None`. Absent therefore means "nobody priced it", never "this run
  was free" — the distinction an always-zero attribute destroyed.
- **`gen_ai.request.temperature` is absent where nothing was sent** (1.9.0,
  #283). Current Claude models reject sampling parameters, so the adapter
  omits the field for them; an attribute asserting `0.0` on a request that
  carried no temperature is a false statement about the model's
  configuration, on the channel an operator uses to explain a run's
  behaviour. `ToolCallLlm::effective_temperature` is the seam, defaulted so
  host transports keep reporting what they always reported.
- **`gen_ai.response.model` echoes the request model.** The provider does
  return the model it served, and since 1.9.0 the journal carries it
  (`served_model` / `served_region`, #287) — this attribute will say so once
  the span carries it through. Where a provider aliases (`gpt-4o` → a dated
  build) or a router resolves elsewhere, the two genuinely differ.

Every `areev.*` attribute stays on the span beside these — `areev.superstep`,
`areev.task_path`, `areev.attempt`, `areev.effect_seq` (plus
`areev.effect_kind` / `areev.executor_kind`), and `areev.run_id` /
`areev.outcome` on the root. They are the run-provenance join, and nothing in
`gen_ai.*` expresses it: `gen_ai.*` says what the model did, `areev.*` says
which journaled effect it was — which is what makes a span addressable back
into the journal with `areev run-trace`.

**Why the attributes ride the event.** The exporter is a §6.10 observer: it
runs on the bus's own thread, with no store handle, while the driver holds the
memory's single writer. It cannot read the journal back to enrich a span, so
everything a span says has to arrive inside the `RunEvent` — which is why
`NodeDispatched` and `EffectSettled` carry model, usage and call-id fields.
They are all optional and skipped when absent, so the `--events` JSON-lines
contract stays additive: a run with no model in it emits the lines it always
did.

### The in-process callback (bindings)

`--events` is a CLI affordance; a host embedding Areev gets the same stream as
a **callback** (#182) — `on_event=` on Python's `run_start`/`run_resume`,
`onEvent` on Node's. Each is handed **exactly the line `--events` prints**: one
§6.10 `RunEvent` as a JSON object with an `"event"` tag.

```python
db.run_start(wf, "r1", tool_cmd=..., on_event=lambda line: print(json.loads(line)["event"]))
```
```js
await m.runStart(wf, 'r1', null, toolCmd, ...Array(12).fill(null), (line) => console.log(JSON.parse(line).event))
```

There is no per-language event class and no deserializer, deliberately. The
vocabulary is append-only and every field the OTel work added is `Option` +
`skip_serializing_if`, so a subscriber matches on the tag, ignores what it does
not know, and a run with no model in it sees the lines it always did.

Three things a subscriber's author needs to know:

- **`RunFinished` is emitted at a TERMINAL outcome.** A run that parks on a
  human gate ends its `run_start` leg at `AskRaised` (a host pause ends it at
  `RunPaused`); `RunResumed` … `RunFinished` arrive on the `run_resume` leg. Waiting for `RunFinished` from
  a start that parks waits forever. `dropped_events` on that last line is the
  honesty counter: how many events the bounded (1024, drop-oldest) buffer
  discarded because the subscriber could not keep up.
- **Attaching a callback turns on `TokenChunk` deltas** from an abstract node's
  model turn — the driver only builds a token sink when there is a subscriber,
  so model text streams through the same callback with no further plumbing.
  Observational in the same sense as everything else here: the journaled result
  is the model's final message, not the concatenated deltas.
- **A callback that raises never fails the run.** Python reports it unraisable
  (the treatment CPython gives an exception in `__del__`); Node's threadsafe
  call is non-blocking and discards the result. The bindings' version of the
  §6.10 invariance test runs one plan twice, observed and not, and asserts
  `run_verify` passes both times and `run_inspect` matches.

Node needs the callback converted to a threadsafe function on the JS thread
before the work is queued — `RunObserver::event` fires on the event bus's own
thread, a third thread from both the JS thread and the libuv worker — which is
why `runStart`/`runResume` can throw synchronously as well as reject.

The trigger surface (`trigger_run`/`trigger_deliver`) takes no callback in
either binding, although a firing starts a real run and the CLI's `--events`
does reach it. A knowable asymmetry, not an oversight.

## Surfaces

The same runtime on every surface — one journal, one set of rules:

| Surface | Shape |
|---|---|
| CLI | `areev run start/resume/respond/input/pause/cancel/list/inspect/verify/fork/shadow/oversight-report/demo`, plus `areev run-trace` / `areev runs-touching` |
| MCP | the eight `areev_run_*` tools ([reference](mcp-reference.md)); host tools only via `$AREEV_RUN_TOOL_CMD`; the acting principal is server-bound — `principal`/`responder` are never client-supplied |
| Python | `db.run_start(workflow, run_id, input_json, tool_cmd, …, allow_executor=…, executor_cache=…, sandbox_cmd=…, executor_timeout_secs=…, on_event=…)`, `run_resume` (same tail), `run_respond(…, responder=…)`, `run_input`, `run_pause`, `run_cancel`, `run_verify`, `run_shadow(run_ids, plan=…, plan_body=…, options=…)`, `run_fork`, `run_list`, `run_inspect`, `run_oversight_report(run_id=…, plan=…)`, `changes_since` — JSON strings out. `on_event` is a callable taking one JSON string: the same §6.10 line `--events` prints |
| Node | `await m.runStart(…, onEvent)` and the same set (`runRespond`, `runInput`, `runPause`, `runFork`, `runInspect`, `runOversightReport`, …) — promises, JSON strings out. `onEvent` is `(event: string) => void`, called from the event bus's own thread |
| HTTP / console | `GET /api/run/list`, `GET /api/run/inspect`, `POST /api/run/respond` (per-principal credential required), `POST /api/run/cancel`; the console's Runs tab is the approval queue. The console's **Workflows** tab visualizes and edits plans themselves — an editable node/edge graph over the same Workflow grains, built entirely on `/api/browse` and `/api/cal` (`ADD workflow`), no dedicated route. It also draws what a plan does *not* contain: the Trigger grains that point at it (read-only, in their own lane) and, when a run is selected, a status rail per step from that run's journal grains — a client-side join on `mg:step_action:<node>`, not a new endpoint. The **Tools** tab is the other half of that picture: the Tool definitions a node can bind to, each with its schema, locked params and the plans that bind it, plus every execution grain grouped by run. A plan with a bounded-cycle edge or a per-node retry count opens view-only: `ADD`/`SUPERSEDE workflow` has no surface syntax yet to author either (`* N` populates `retries`, not `max_cycles`) — and for the same reason, connecting an edge that would close a cycle in an editable plan is refused rather than silently saved as an unbounded one |

Authorization uses three verbs, granted like any other
([CAL DCL](cal-reference.md)): `run.execute` (start/resume/pause), `run.respond`
(answer asks), `run.cancel` (the brake). An unbound local session is the
owner and holds every right — grants matter once a file is shared.

## The run ↔ memory join

Because the journal and the memory share one file, provenance is closed in
both directions with no extra infrastructure:

- an agent's `record_tool_call` can name the run and workflow node that made
  it (`run_id`, `workflow_hash` + `node_id` → the `mg:step_action` link);
- `areev runs-touching --hash <grain>` walks from any fact back to the runs
  that produced or refined it;
- `areev tool provenance <hash>` chains a piece of governed code to the loop
  recommendations that target it and the runs that executed it;
- the [Areev Loop](loop.md) `run_outcome` analyzer reads run terminals and
  proposes findings — *"this workflow failed 4 of 6 runs"*, *"this plan has
  spent $4.10"* — with the run grains cited as evidence;
- a `plan_revision` the loop drafts is **rehearsed** against the plan's own
  journaled runs before a reviewer sees it (`areev run shadow --plan-file`
  under the hood): the recommendation carries a `replay` block — per-run
  outcome under incumbent vs candidate, effects out of support, spend delta
  — and a `plan_replay` policy refuses to stamp it applicable when it is
  worse than the incumbent on the same runs. Applying stays human, with a
  BECAUSE.

## Error codes

All runtime errors lead with a stable `RUN-Ennn` code. The ones you'll
actually meet: `RUN-E002` unbounded cycle (add `max_cycles`), `RUN-E004`
unresolvable binding, `RUN-E005` bad condition, `RUN-E006` abstract node
without an LLM, `RUN-E007` budget exhausted (fork to raise), `RUN-E009`
replay divergence (names the differing fields), `RUN-E011` response names no
pending ask, `RUN-E012` missing grant, `RUN-E013` canceled, `RUN-E024`
transcript over `--llm-context-tokens` with nothing left to fold,
`RUN-E025` the model this run started under is not the one on offer (fork),
`RUN-E026` a different scheduler epoch wrote this run (fork), `RUN-E027` a
concurrency cap — retryable, and nothing was written, `RUN-E029` a pause
asked of a run that already finished or has a cancel pending, `RUN-E030` a
decision node on a host with no decision backend. The full registry is
[`ERROR_CODES.md`](../ERROR_CODES.md).

## Bounds, stated

- An abstract node runs at most `--max-effects` effects per attempt (turns, tool
  calls and re-prompts share the counter), default **16**; one more fails the
  node. Frozen in the manifest at start.
- A WHOLE run runs at most `--max-run-effects` effects and dispatches at most
  `--max-tool-calls` host tool calls (1.9.0, #295) — both unbounded by
  default. Exhausting either is a resumable budget stop, not a node failure.
- At most `--max-concurrent` runs execute at once in a memory, and
  `--max-concurrent-per-principal` for one principal (1.9.0, #296) — both
  unbounded by default.
- `--llm-tool-result-chars` bounds ONE tool result in the transcript
  (characters, default unbounded); the journal keeps every result in full.
- `--llm-context-tokens` bounds the WHOLE transcript, in the provider's own
  reported prompt tokens (default: no ceiling). Past it the middle is folded
  into one journaled summary; `RUN-E024` when nothing is foldable. It measures
  the PREVIOUS turn, so it lags one round — pair it with
  `--llm-tool-result-chars`, which is what makes that lag finite.
- **Both bounds are off by default.** Without them an abstract node's transcript
  grows until the provider rejects it, and that rejection fails the node — the
  runtime will not shrink what it sends unless you ask it to.
- **A fold reacts to a provider's context-length error only where that error is
  STRUCTURED.** OpenAI-compatible endpoints return
  `error.code = "context_length_exceeded"` and the runtime folds and re-sends
  on it. Anthropic reports the same condition in prose, which the seam refuses
  to parse; it is covered by the derived 200k ceiling instead. A streaming turn
  keeps status-as-error and is proactive-only.
- Subgraphs and declared memory reads run inline on the driver thread, so
  parallel siblings of either kind serialize.
- The condition grammar is frozen; there is no expression language beyond
  it, deliberately.
- One memory = one writer: while a driver holds the file, another process
  (including a second `areev run` on the same file) is refused at open by
  the OS file lock (`STO-E001`); a second handle inside the same process is
  `STO-E002`. Respond-then-resume across processes works because each verb
  opens, works, and closes.

## Run leases

A run is leased while a driver advances it. The lease is taken at `start` /
`resume`, renewed **after every settled result and at each superstep
boundary**, and released when the run reaches a terminal outcome.

The TTL is `--lease SECS` (`$AREEV_RUN_LEASE`; spelled as on `trigger run`),
defaulting to 600 s with a 5 s floor. Two things had to ship together (1.9.0,
#299). Renewing only at superstep boundaries is why nothing shorter was safe:
an abstract node's turns and tool calls all happen inside ONE superstep, so
with a 300 s tool timeout and sixteen effects a perfectly healthy driver could
outlive its own lease and be taken over mid-flight — the failure the lease
exists to prevent. Renewing inside the superstep is what makes a short TTL
safe; a knob without it would have been harmful.

The holder is **`{principal}#{host}/{pid}`** (`--node`, `$AREEV_NODE_ID`),
not `principal#pid` (1.9.0, #300). Re-entering an equal holder is by design —
that is what resuming your own run is — so two containers both running as
PID 1 under one service principal were the SAME holder and did not exclude
each other: the second pod acquired a LIVE lease, bumped the fence, and both
drivers dispatched the open superstep's effects. That is the normal
Kubernetes shape. A restarted pod is a new holder and waits out the TTL,
which is why this pairs with a configurable lease.

Before this existed, two drivers advancing one run **last-write-wins in the
journal, silently**: `journal::ingest` overwrites a second result for the same
key, and the owner-nonce ownership check is a documented gap. The doc comment on
`RUN-E016 Tainted` claimed forked supersession tips were detected as taint —
they were not. The lease prevents the case rather than noticing it afterwards.

- A driver that stalls past its lease loses it. Its next checkpoint is refused
  with **`RUN-E021 LeaseLost`** instead of landing behind whoever took over.
- An expired lease is reclaimable, so a crashed driver does not park its run
  forever — the cost of node loss is one lease TTL, not a recovery procedure.
- Re-entering a lease this driver already holds is ordinary (that is what
  resuming your own run is).
- The lease is a `meta` row with the fence *inside* its value, so a renewal is a
  compare-and-swap against the exact row the holder last saw. No fencing-token
  column is needed, because the lock and the data are the same row.
- On the embedded backend one memory is one writer, enforced at open, so two
  drivers cannot reach one run anyway. This earns its keep on Postgres.
