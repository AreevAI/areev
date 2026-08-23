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
    deterministic: `parent~<tool_call_id prefix>`).
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

Plan validation runs at load: unreachable nodes (`RUN-E003`), unresolvable
bindings (`RUN-E004`), malformed conditions (`RUN-E005`), unbounded cycles
(`RUN-E002`), structural shape (`RUN-E019`).

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

Four things follow from it:

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

The grant is host configuration, never a grain — a Definition declaring its own
reach would be a permission arriving in the same bundle as the code it
authorizes.

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
binary — build it from the repo and point `--sandbox-cmd` at it.)

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
                      "max_calls": 64, "max_response_bytes": 1048576 },
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

⚠️ **Embedded backend only.** The read is lock-free precisely because it goes
to the `.blobs` sidecar without opening the database — and that sidecar is an
embedded-backend thing. On PostgreSQL a blob lives in-schema, so
`areev::blob_get` returns a `501` naming the limitation rather than reporting
the attachment as missing. On that backend a tool can open the memory
directly anyway (see [Backend divergence](#backend-divergence-reading-the-memory-mid-run-85)),
so the capability is closing an embedded-tier gap.

`headers` names the non-credential request headers the module may set, and is
deny-by-default like `credentials`: declaring none permits none. A name the
broker owns (`Authorization`, `Cookie`, `Host`, `Proxy-Authorization`) is
refused **at write time**, so a module that tries to declare the credential
channel is unwritable rather than writable-and-refused-later. Matching is
case-insensitive, because HTTP field names are.

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
| more than `max_calls`, or a response over `max_response_bytes` | broker, per call | 403 + a journaled refusal — an overrun is an error, never a truncation |

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
  local console, the hub, or `169.254.169.254` takes an explicit
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
the storage tier, and it silently decides whether an agent design is portable:

- **Embedded (Turso file)**: no — the file lock is exclusive, so even a pure
  `RECALL` from inside a tool is refused (`STO-E001`). Use the doors that
  exist: `areev blob get` reads CAS attachments lock-free, a **capability
  tool** reads them with `areev::blob_get` through the broker (#106, above),
  and a **trigger's `--context-query`** has the evaluator assemble a saved
  query's result into the run input before the run starts
  ([triggers](triggers.md)). All three are lock-free by construction, which is
  why they work while the run holds the file.
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

| Record | Grain | Namespace | When |
|---|---|---|---|
| Intent | Tool grain, `status = pending` | session `--ns` | **before** every effect dispatch |
| Result | supersession of the intent, re-stating its identity + usage | session `--ns` | when the effect settles |
| Checkpoint | State grain (scheduler state + the superstep's decision record), chained by `derived_from` | session `--ns` | every superstep |
| Manifest | the frozen plan resolution, budgets, principal | `agent:harness` | at start |
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
areev run list                            # recent runs, newest first
areev runs-touching --hash <GRAIN>        # the reverse join: which runs produced/refined this grain
```

## Crash recovery and resume

`areev run resume --run-id ID` picks up from the latest checkpoint. On
resume:

- answered asks settle; expired asks are journaled and fail their node;
- a **dangling intent** (crash between intent and result) is adopted —
  looked up before written, so there is only ever one intent grain per key —
  and re-dispatched under the same idempotency key, with the redelivery
  recorded as an Observation (`on_dangling = fail` turns this into
  `RUN-E008` instead, if your tools can't tolerate redelivery);
- a run whose manifest exists but whose first checkpoint was lost
  reconstructs from the manifest's input.

## Human-in-the-loop, precisely

- Asks are addressed by `tool_call_id`, **never by index** — an index is a
  race with the scheduler.
- An approval ask **structurally refuses** `responder == the principal that
  triggered it`. This is not a policy toggle; it is checked before anything
  else and there is no flag to disable it.
- On governed sessions (grants present in the file), the responder's own
  grants must cover `run.respond`. Grants are ordinary `mg:permits` Facts —
  they live in the file, sync with it, and are granted in CAL:

  ```
  GRANT run.respond ON ops TO "user:officer" WITH because("refund approvals")
  ```

- Rejected and expired responses are **journaled as Observations before the
  error returns** — a losing approval attempt is audit evidence, not a
  silent 4xx.
- `--ask-ttl <sec>` on start bounds how long an ask may sit unanswered.
- Refusing an ask is a first-class answer: `--is-error true` journals the
  refusal and fails the node as user-aborted.

The web console (`areev ui`) surfaces pending asks in its **Runs tab**, which
groups runs as *Waiting on you* / *In flight* / *Finished* so an ask cannot be
buried under finished history. Each card carries a per-step strip in plan order,
tinted by what that step did — the same join the Workflows canvas draws as a
status rail, read from one shared index so the two surfaces cannot disagree.
`run.respond` over HTTP refuses shared-token and anonymous callers outright:
only a per-principal credential may approve, because the approver's identity
*is* the audit record, and the Approve/Refuse buttons say so when they are
unavailable. Cancel deliberately keeps the low bar.

## Budgets

`--max-tokens`, `--max-usd`, `--max-wall-ms`, `--max-supersteps`. Spend is
accounted from the journal (usage rides every result grain), so a resumed
run's accounting equals a live run's. Exhaustion is a **parked checkpoint**,
not a corrupted run: `areev run fork` re-opens a budget-exhausted terminal
under raised budgets, continuing exactly where it stopped.

## Verify and shadow

```bash
areev run verify --run-id demo-1     # one run: byte-compare replay
areev run shadow --runs a,b,c        # many runs: replay with ZERO effect dispatches
```

`verify` re-drives the run from the **manifest's input** with the clock
scripted from journaled readings and every effect answered from the journal,
writing nothing — then byte-compares each commanded checkpoint against the
stored chain. Divergence names the differing fields (`RUN-E009`). Canceled
runs verify too (the cancel is replayed from checkpoint state), and
checkpoints past the last verifiable point are reported honestly as
unverified rather than skipped.

`shadow` is the same machinery as a batch pre-flight: the replay path holds
no executor, so "re-execute these journaled runs with zero side effects" is
structural, not a promise. It is also how [Areev Loop](loop.md) evaluates
proposed changes against history before anything is applied.

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
one.

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
- The tools *offered* to the model are exactly the manifest's pinned host
  Definitions. Tool arguments are validated against the pinned schemas —
  strictly, with one re-prompt on violation; an unknown tool name gets one
  re-prompt, then fails the node.
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
make the merged results order-independent. Spawn targets are host-bound
nodes in v1.

## Watching a run

- `--events` streams structured run events (JSON lines) to stderr while
  stdout stays the machine surface.
- `--otel-endpoint http://collector:4318` exports one OTLP/HTTP trace batch
  per run at completion; resumes join the same trace (the trace id derives
  from the run id).
- Streaming is **observational only**: journals are byte-identical with no
  subscriber, a normal one, or a slow one — pinned by test.

## Surfaces

The same runtime on every surface — one journal, one set of rules:

| Surface | Shape |
|---|---|
| CLI | `areev run start/resume/respond/cancel/list/inspect/verify/fork/shadow/oversight-report/demo`, plus `areev run-trace` / `areev runs-touching` |
| MCP | the six `areev_run_*` tools ([reference](mcp-reference.md)); host tools only via `$AREEV_RUN_TOOL_CMD`; the acting principal is server-bound — `principal`/`responder` are never client-supplied |
| Python | `db.run_start(workflow, run_id, input_json, tool_cmd, …, allow_executor=…, executor_cache=…, sandbox_cmd=…)`, `run_resume`, `run_respond(…, responder=…)`, `run_cancel`, `run_verify`, `run_shadow`, `run_fork`, `run_list`, `run_inspect`, `run_oversight_report(run_id=…, plan=…)`, `changes_since` — JSON strings out |
| Node | `await m.runStart(…)` and the same set (`runRespond`, `runFork`, `runInspect`, `runOversightReport`, …) — promises, JSON strings out |
| HTTP / console | `GET /api/run/list`, `GET /api/run/inspect`, `POST /api/run/respond` (per-principal credential required), `POST /api/run/cancel`; the console's Runs tab is the approval queue. The console's **Workflows** tab visualizes and edits plans themselves — an editable node/edge graph over the same Workflow grains, built entirely on `/api/browse` and `/api/cal` (`ADD workflow`), no dedicated route. It also draws what a plan does *not* contain: the Trigger grains that point at it (read-only, in their own lane) and, when a run is selected, a status rail per step from that run's journal grains — a client-side join on `mg:step_action:<node>`, not a new endpoint. The **Tools** tab is the other half of that picture: the Tool definitions a node can bind to, each with its schema, locked params and the plans that bind it, plus every execution grain grouped by run. A plan with a bounded-cycle edge or a per-node retry count opens view-only: `ADD`/`SUPERSEDE workflow` has no surface syntax yet to author either (`* N` populates `retries`, not `max_cycles`) — and for the same reason, connecting an edge that would close a cycle in an editable plan is refused rather than silently saved as an unbounded one |

Authorization uses three verbs, granted like any other
([CAL DCL](cal-reference.md)): `run.execute` (start/resume), `run.respond`
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
  spent $4.10"* — with the run grains cited as evidence.

## Error codes

All runtime errors lead with a stable `RUN-Ennn` code. The ones you'll
actually meet: `RUN-E002` unbounded cycle (add `max_cycles`), `RUN-E004`
unresolvable binding, `RUN-E005` bad condition, `RUN-E006` abstract node
without an LLM, `RUN-E007` budget exhausted (fork to raise), `RUN-E009`
replay divergence (names the differing fields), `RUN-E011` response names no
pending ask, `RUN-E012` missing grant, `RUN-E013` canceled. The full
registry is [`ERROR_CODES.md`](../ERROR_CODES.md).

## Bounds, stated

- Subgraphs run inline on the driver thread; a child that parks on a human
  gate fails its parent node (ask *bubbling* is not in v1), and parallel
  subgraph siblings serialize.
- `Send` targets host-bound nodes only in v1.
- The condition grammar is frozen; there is no expression language beyond
  it, deliberately.
- One memory = one writer: while a driver holds the file, other writers
  (including a second `areev run` on the same file) are refused with
  `STO-E002`. Respond-then-resume across processes works because each verb
  opens, works, and closes.

## Run leases

A run is leased while a driver advances it. The lease is taken at `start` /
`resume`, renewed at each superstep boundary, and released when the run reaches
a terminal outcome.

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
