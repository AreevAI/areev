# MCP Server Reference

Areev ships a built-in **Model Context Protocol (MCP)** server that exposes
memory-semantic tools to any MCP client (Claude Code, Claude Desktop, Cursor,
and others). It is *not* SQL-over-MCP: the tools speak grains, recall ranking,
supersession, and CAL — the vocabulary of agent memory.

Run it over stdio:

```bash
areev serve --mcp --db <memory.db> [--ns <namespace>]
```

`--db` is optional (falls back to `$AREEV_DB`, then `~/.areev/default.db`).
One server serves exactly **one writable memory file**. To read across files in
a single `areev_cal` `ASSEMBLE`, add read-only mounts:

```bash
areev serve --mcp --db user.db --mount org=~/.areev/org.db,team=~/.areev/team.db
```

Each mount is exposed under a namespace prefix (`org.<inner>`), is **read-only**
(writes always land on the primary `--db`), and lets one statement pull the
user's memory plus shared org/team knowledge:

```
ASSEMBLE "prompt" FROM
  policy:  (RECALL facts WHERE namespace = "org.policies"),
  profile: (RECALL facts WHERE subject = "john")
```

For where the MCP server sits in the system, see
[ARCHITECTURE.md](../ARCHITECTURE.md#8-crate-layout). For trust boundaries, see
the [security model](security-model.md#trust-model-at-a-glance).

---

## Protocol

| Property | Value |
|---|---|
| Transport | **stdio** — one JSON-RPC message per line (newline-delimited) |
| RPC | **JSON-RPC 2.0** |
| Protocol revision | **`2025-06-18`** |
| Server name / version | `areev` / the crate version |

The server reads one JSON object per line from stdin and writes one JSON object
per line to stdout. It handles these methods:

| Method | Behavior |
|---|---|
| `initialize` | Returns `protocolVersion`, `capabilities.tools`, and `serverInfo` |
| `ping` | Returns an empty result |
| `tools/list` | Returns the twenty-three tool definitions (with input schemas) |
| `tools/call` | Invokes a tool by `name` with `arguments` |

Conventions:

- **Notifications get no response.** A message with no `id` (or `id: null`) is a
  notification and produces no reply.
- **Protocol errors are JSON-RPC errors.** A malformed line returns
  `-32700` (parse error); an unknown method returns `-32601` (method not found).
- **Tool failures are results, not errors.** A `tools/call` whose tool fails
  (missing argument, bad hash, store error) returns a normal JSON-RPC **result**
  with `isError: true` and the error text in the content. This is per the MCP
  spec — the model sees the failure as tool output it can react to, not as a
  transport-level fault. Only protocol-level problems become JSON-RPC errors.

A successful tool call returns:

```json
{
  "jsonrpc": "2.0",
  "id": 3,
  "result": {
    "content": [{ "type": "text", "text": "<tool output, usually JSON>" }],
    "isError": false
  }
}
```

A failed tool call returns the same shape with `"isError": true` and the error
message as the text.

### Namespace resolution

Every tool accepts an optional `namespace` argument. When omitted, the server
uses its **session namespace**, resolved once as: explicit `--ns` flag → the
facade's capability default → `"shared"`. This scopes an MCP session to a
namespace by default so a client need not repeat it on every call.

By default a `namespace` argument is a *filter*, not a boundary — a client may
name any namespace. To make it a boundary, start the server with
`--lock-ns <NS>`: per-call `namespace` arguments (and `namespace` set inside
`fields`) are ignored, and `areev_cal` queries are namespace-overridden, so an
agent handed the session cannot read or write outside `<NS>`. Use this when a
multi-tenant host gives an agent a session it must not escape.

---

## The twenty-three tools

### `areev_recall`

Recall current memories about a subject (structural, microsecond-class). Returns
grains newest-first.

| Parameter | Type | Required | Description |
|---|---|---|---|
| `subject` | string | **yes** | Entity to recall about, e.g. `"caller:john"` |
| `relation` | string | no | Optional relation filter, e.g. `"prefers"` |
| `namespace` | string | no | Defaults to the session namespace |
| `k` | integer | no | Max results (default 16) |
| `run_id` | string | no | Ambient trajectory run id recorded on recall telemetry |

Returns a JSON array of `{ hash, type, fields }` objects. Under an active
anonymization policy (`anon:<ns>` row or the host's `--anonymize-egress`
floor) the response becomes
`{ "grains": [...], "anonymized": {...} }`: fields arrive pseudonymized
(`[PERSON_1]`) and `anonymized` carries the namespaces, floor state, and
mapping *ids* only — the placeholder→value mappings never ride an MCP
payload; the host process rehydrates via `anon_mappings()`.

```json
{ "name": "areev_recall",
  "arguments": { "subject": "john", "relation": "prefers", "k": 8 } }
```

### `areev_add`

Add a durable memory grain (append-only, content-addressed). Use `type: "fact"`
with `subject`/`relation`/`object` fields for structured knowledge.

| Parameter | Type | Required | Description |
|---|---|---|---|
| `fields` | object | **yes** | Grain fields, e.g. `{subject, relation, object, confidence}` |
| `type` | string | no | Grain type: `fact` (default), `event`, `state`, `goal`, `observation`, … |
| `namespace` | string | no | Optional namespace (injected into `fields` if absent) |

Returns `{ "hash": "<content address>" }`.

```json
{ "name": "areev_add",
  "arguments": { "type": "fact",
    "fields": { "subject": "john", "relation": "prefers",
                "object": "window seat", "confidence": 0.9 } } }
```

### `areev_supersede`

Evolve a memory: write a new version that supersedes `old_hash`. The old version
is preserved as append-only history — never deleted.

| Parameter | Type | Required | Description |
|---|---|---|---|
| `old_hash` | string | **yes** | Content address (64-hex) of the version to supersede |
| `fields` | object | **yes** | Fields of the new version |
| `type` | string | no | Grain type of the new version (default `fact`) |
| `namespace` | string | no | Optional namespace |

Returns `{ "hash": "<new>", "supersedes": "<old>" }`.

### `areev_forget`

Erase a grain from the hot store (tombstoned in the op-log). This is the same
single-grain tombstone as CAL `FORGET <hash>`; both paths are gated by the
server's destructive-ops flag (on by default — disable with
`--no-destructive-ops`).

| Parameter | Type | Required | Description |
|---|---|---|---|
| `hash` | string | **yes** | Content address (64-hex) to forget |

Returns `{ "forgotten": "<hash>" }`.

### `areev_subject_report`

The DSAR read (GDPR Art. 15 access / Art. 20 portability): everything
`FORGET SUBJECT` **would** erase for one identity — the exact subject, its
partition-style keys (`pat`, `pat#visit1` — never `patricia`), and the full
history — hydrated instead of erased. Read-only: it is **not** behind the
destructive-ops flag, needs only the session's `read` grant, and writes no
audit grain (the audit obligation is on destruction, not access). The CAL
spelling is `REPORT SUBJECT "<id>" [WITH text_mentions]`.

| Parameter | Type | Required | Description |
|---|---|---|---|
| `subject` | string | **yes** | The identity to report on |
| `text_mentions` | boolean | no | Also include grains whose indexed text mentions the identity (needs the text index on and fully built) |

Returns `{ "subject", "identity_names": [...], "count", "grains": [{hash, type, fields}, ...] }`.

The report and `FORGET SUBJECT` run one selector, so "show me everything,
then delete it" is two calls over exactly the same set.

### `areev_remember`

Store raw conversational content as an **Event** grain (a transcript entry).
Distill durable knowledge from it afterwards with `areev_add`.

| Parameter | Type | Required | Description |
|---|---|---|---|
| `content` | string | **yes** | The utterance/observation text |
| `session_id` | string | no | Session/thread id |
| `role` | string | no | `user` \| `assistant` \| `system` \| `tool` |
| `observer` | string | no | Id of the agent capturing this |
| `namespace` | string | no | Optional namespace |

Returns `{ "hash": "<hash>", "event": "<hash>", "stored_as": "event", "note": "distill durable facts with areev_add" }`
(`event` and `hash` are the same value; `event` matches the key the CLI and
bindings return, `hash` is kept for compatibility).

This is the **same write path** as `areev remember`, the Python/Node bindings,
and `capture-stop` — the same input produces the same grain on every surface.

What this tool deliberately does *not* have is the LLM extraction knobs
(`--model` / `--llm-cmd`): over MCP the client already *is* a model, so
extraction here would be a model calling a model. Distill with `areev_add`
instead.

### `areev_cal`

Execute a CAL statement (`RECALL` / `ASSEMBLE` / `EXISTS` / `HISTORY` / `ADD` /
`SUPERSEDE` / …). **CAL destruction is shaped and authorization-gated** — it
takes a hash, an identity, or an age, never a predicate; can never rewrite
history; and is refused without the session's `delete`/`erase` grant — see
[the destruction model](cal-reference.md#8-destruction-shaped-and-authorization-gated).
`--no-destructive-ops` still switches the whole surface off per-process.

| Parameter | Type | Required | Description |
|---|---|---|---|
| `query` | string | **yes** | CAL text, e.g. `RECALL facts WHERE subject = "alice" \| COUNT` |

Returns the CAL result payload as JSON. Bulk-destructive tokens (`DELETE`,
`DROP`, …) are rejected — the call returns `isError: true` with the parse
error, not a crash. `FORGET <hash>` parses and, by default, **executes** (the
server runs with destructive ops enabled); launch with `--no-destructive-ops`
to gate it off for both this tool and `areev_forget`.

```json
{ "name": "areev_cal",
  "arguments": { "query": "RECALL facts WHERE subject = \"john\" | COUNT" } }
```

**Saved queries and templates ride this tool.** The memory file can carry
named assembly logic (`qry:`/`tpl:` registry rows — they replicate with the
file), and an agent should prefer a deployment-provided query over composing
its own:

```json
{ "name": "areev_cal", "arguments": { "query": "DESCRIBE QUERIES" } }
{ "name": "areev_cal",
  "arguments": { "query": "RUN \"session_prompt\"($user = \"john\", $session = \"call-42\")" } }
{ "name": "areev_cal",
  "arguments": { "query": "RECALL facts WHERE subject = \"john\" FORMAT TEMPLATE brief" } }
```

Saved-query bodies are read-only by construction, so `RUN` never widens the
session's authority. `DEFINE QUERY`/`DEFINE TEMPLATE` and `DROP` are
governance: they need the `admin` verb on the session's principal, and `DROP`
additionally sits under the destructive-ops cap. See cookbook recipe 15 for
the end-to-end pattern.

---

### `areev_loop`

Runs one governed self-improvement pass and returns the run outcome plus the
pending recommendation queue. The engine is the deterministic analyzer set;
auto-apply happens only under the host policy the server was started with
(`areev serve --mcp --policy loop-policy.json` or `$AREEV_LOOP_POLICY` — never
controllable by the client), and LLM reflection attaches on the CLI, not
here. Call it at session start; review pending recommendations before
acting. See [loop.md](loop.md).

| Parameter | Type | Required | Description |
|---|---|---|---|
| `min_new` | integer | no | only run if at least this many new grains since the last run |
| `min_new_errors` | integer | no | …or this many new tool failures |
| `full_sweep` | boolean | no | re-analyze the whole memory (the `areev loop reflect` semantics) instead of the incremental watermark |

Result: `{ "run": <run-outcome>, "pending": [ <recommendation>, … ] }`.

### `areev_recommendations`

Lists recommendations, or acts on one. Without `action`, lists by status
(default `pending`). With `action` + a `hash` + a mandatory `because` reason,
performs the audited transition. An agent approving **its own** proposal is
blocked (self-approval, `LOP-E021`) — run a reviewer process with distinct
`--scopes`/`--actor` for separation of duties.

| Parameter | Type | Required | Description |
|---|---|---|---|
| `status` | string | no | filter: `pending` \| `approved` \| `applied` \| `all` (default `pending`) |
| `action` | string | no | `apply` \| `approve` \| `reject` (omit to list) |
| `hash` | string | for an action | recommendation hash |
| `because` | string | for an action | mandatory written reason |

### The graph, time, and run↔memory reads

Five read-only tools expose the graph walk, the as-of axis, and the join
between execution history and semantic memory. All take an optional
`namespace`.

#### `areev_related`

Walk the entity graph from a starting term (bounded k-hop, breadth-first).
`in`/`both` directions only see relations the file declares entity-valued.

| Parameter | Type | Required | Description |
|---|---|---|---|
| `start` | string | **yes** | entity term to start from, e.g. `"alice"` |
| `relations` | string | **yes** | comma-separated, e.g. `"reports_to,mg:knows"` |
| `direction` | string | no | `out` (default) \| `in` \| `both` |
| `depth` | integer | no | hops to walk, 1–4 (default 2) |
| `limit` | integer | no | max entities returned (default 64) |

#### `areev_entity_at`

As-of read on two axes: what was true in the world at T (`world`), or what
the agent knew at T (`knowledge`).

| Parameter | Type | Required | Description |
|---|---|---|---|
| `subject` | string | **yes** | entity, e.g. `"alice"` |
| `relation` | string | **yes** | relation, e.g. `"employer"` |
| `at` | integer | **yes** | point in time, epoch milliseconds |
| `axis` | string | no | `world` (default) \| `knowledge` |

#### `areev_step_actions`

Execution records for a workflow: which grains ran which of its nodes.
Retries appear as several records for one node.

| Parameter | Type | Required | Description |
|---|---|---|---|
| `workflow` | string | **yes** | content address (64-hex) of the Workflow grain |
| `node` | string | no | narrow to one node id |
| `limit` | integer | no | max records returned (default 64) |

#### `areev_run_trace`

Everything recorded during a run, plus what the run produced downstream
(facts/lessons derived from it).

| Parameter | Type | Required | Description |
|---|---|---|---|
| `run_id` | string | **yes** | the run identifier recorded on the run's grains |
| `include_yield` | boolean | no | also return derived grains (default true) |
| `limit` | integer | no | max grains per section (default 64) |

#### `areev_runs_touching`

Which runs produced or refined a grain — the reverse join. Runs that merely
read the grain are not recorded: a read leaves no grain.

| Parameter | Type | Required | Description |
|---|---|---|---|
| `hash` | string | **yes** | content address (64-hex) of the grain |
| `depth` | integer | no | provenance hops to walk, max 8 (default 4) |

### The runtime six (`areev_run_*`)

The governed workflow runtime over MCP: journaled, checkpointed, budgeted,
resumable runs of Workflow grains. Two rules distinguish this surface from
the CLI:

- **Host tools execute only via `$AREEV_RUN_TOOL_CMD`** (the same subprocess
  seam as the CLI's `--tool-cmd`: input JSON on stdin, result JSON on
  stdout). Without it, host-tool nodes fail loudly rather than silently.
- **The acting principal is server-bound** — the identity the server was
  started with (`--as`, else `agent:mcp`). `principal`/`responder` are
  **not** parameters; a client-supplied name would let an agent approve
  gates on runs it triggered itself. Approvals from a different principal
  go through a server bound to that principal, the console, or the CLI.

| Tool | What it does |
|---|---|
| `areev_run_start` | Start a run — `workflow` (64-hex), fresh `run_id`, optional `input` (any JSON) and budgets (`max_tokens`, `max_usd_micros`, `max_wall_ms`, `max_supersteps`). Client-gated nodes park the run and return a `requires_action` envelope. |
| `areev_run_resume` | Resume a parked/interrupted run from its latest checkpoint — settles answered asks, expires stale ones, re-delivers crash-window intents under the same idempotency key (recorded). |
| `areev_run_respond` | Answer one pending ask by `tool_call_id` (never an index) with `result` / `is_error`. Separation of duties is structural; rejected and late responses are journaled as audit evidence before the error returns. |
| `areev_run_cancel` | Write the kill-switch marker (`run_id`, optional `because`) — deliberately the lowest-privilege run verb. |
| `areev_run_verify` | Journal-consistent replay: re-derives every checkpoint and byte-compares against the stored chain, writing nothing; divergences name the differing fields. |
| `areev_run_list` | Recent run ids, newest first (`limit`, default 20). |

#### `areev_tool_provenance`

One-command forensics for a piece of governed code: the loop recommendations
targeting this tool hash (status, summary, pinned evalset) plus which runs
touched it — the full chain from code to approval to execution.

| Parameter | Type | Required | Description |
|---|---|---|---|
| `hash` | string | **yes** | content address (64-hex) of the tool/code grain |
| `depth` | integer | no | provenance hops for the runs join (default 2) |

---

### `areev_record_tool_call`

Record one tool invocation as a trajectory Tool grain, keeping JSON arguments
separate from the result and preserving failures and provider call ids.

| Parameter | Type | Required | Description |
|---|---|---|---|
| `tool_name` | string | **yes** | Tool/function name |
| `result` | string | **yes** | Tool result text or JSON string |
| `input` | any JSON | no | Tool arguments |
| `is_error` | boolean | no | Whether the invocation failed (default `false`) |
| `thread` | string | no | Session/thread id |
| `call_id` | string | no | Provider call id; synthesized when absent |
| `namespace` | string | no | Defaults to the session namespace |

### `areev_run_manifest`

Persist a reproducible run configuration as a content-addressed State grain
plus the run-to-config Fact link. The `config` object should pin the model
build/adapter/quantization/runtime, sampling parameters, prompt, and tools.
This tool is unavailable in a namespace-locked MCP session because its records
live in reserved namespace `agent:harness`.

| Parameter | Type | Required | Description |
|---|---|---|---|
| `run_id` | string | **yes** | Run identifier used by trajectory grains |
| `config` | object | **yes** | Reproducible run configuration |

Only the stdio (`--mcp`) transport is available; the server refuses any other
`serve` transport. Because the server inherits the trust boundary of the process
that spawns it (see the [security model](security-model.md)), run it under a
parent you trust.

---

---

## Wiring it into a client

### Claude Code (one line)

```bash
claude mcp add areev -- areev serve --mcp --db ~/.areev/code.db --ns claude-code
```

This registers a stdio MCP server named `areev` that Claude Code spawns on
demand. The `--db` path is the memory file (created if absent); `--ns` scopes
the session namespace.

### Generic MCP client config

Any MCP client that speaks stdio can launch the server with a command entry.
The typical `mcpServers` config block:

```json
{
  "mcpServers": {
    "areev": {
      "command": "areev",
      "args": ["serve", "--mcp", "--db", "/path/to/memory.db", "--ns", "myagent"]
    }
  }
}
```

## Example session

A minimal scripted session over stdio (requests in, responses out; one JSON
object per line):

```jsonc
// → initialize
{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18"}}
// ← {"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2025-06-18",
//     "capabilities":{"tools":{}},"serverInfo":{"name":"areev","version":"..."}}}

// → list tools
{"jsonrpc":"2.0","id":2,"method":"tools/list"}
// ← 6 tool definitions

// → add a fact
{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"areev_add",
  "arguments":{"fields":{"subject":"john","relation":"prefers","object":"window seat"}}}}
// ← {"...":"...","result":{"content":[{"type":"text","text":"{\"hash\":\"...\"}"}],"isError":false}}

// → recall
{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"areev_recall",
  "arguments":{"subject":"john"}}}
// ← result: JSON array of grains

// → a destructive CAL query is rejected as a tool error, not a crash
{"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"areev_cal",
  "arguments":{"query":"DELETE facts WHERE subject = \"john\""}}}
// ← result with "isError": true and the parse error text
```
</content>
