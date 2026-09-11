# Blessed tools — shared `wasm32-areev-io` blobs

Five tools ship in this repository as content-addressed wasm blobs
([#179](https://github.com/AreevAI/areev/issues/179)). They are the ones every
agent needs and nobody should have to write: an HTTP call, an MCP tool call, an
A2A message, a paginated REST poll. A pack binds one by address, declares where
it may reach, and gets a connector or an outbound leg with **no code of its
own**.

| Tool | Address (`cas://sha256:`) | Bytes | Imports |
|---|---|---|---|
| `http.call` | `6c088ed0161d1254bdf669b978391d0403de5878233e3bd3f13b026be47c98a2` | 2,598 | `areev::emit`, `areev::fetch` |
| `mcp.call` | `e52f19ff68e9ab2184886f36790e8838cd864fd38f31a36ffdff6fc2d9aca428` | 7,748 | `areev::emit`, `areev::fetch` |
| `a2a.call` | `d6fe7e471c8f43999346781370ce711272eba578a01cfd6324be3996e26d25e3` | 11,067 | `areev::emit`, `areev::fetch` |
| `mailbox.poll` | `2a8f2d40014a9f3d9dea4ee6a17d80fea74b541a6d70db6ba714146ec6dd668d` | 9,268 | `areev::emit`, `areev::blob_get` |
| `rest.poll` | `e53d60bdddfb7b496952e4901fdfa6578a3d4ccdcba4ff6b2f40cd1b00869f79` | 21,321 | `areev::emit`, `areev::fetch` |

`areev-tools/dist/blessed.json` is the machine-readable copy, and CI asserts it
is the address of the file beside it. `mailbox.poll` is not a gateway tool: it
is the example connector [#185](https://github.com/AreevAI/areev/issues/185) is
demonstrated with, shipped here because it is built from the same source tree —
and it stays the keyless, network-free floor, unaffected by `rest.poll`.

**These addresses change when the blobs are rebuilt.** A blob is bytes and a
different rustc emits different bytes, so a rebuild is a deliberate commit that
moves the blob, this table and `dist/blessed.json` together — and re-points
every pack that pinned the old address. That is not a wart: it is the same
property that makes `expected_hash` meaningful.

## The idea: the gateway becomes configuration

A tool gateway that decides *where a request may go* is code — code that has to
be read, versioned, and trusted by everyone who installs it. `http.call` makes
no such decision. It hands the request to the broker and hands the answer back,
verbatim, in both directions. Where it may go, which method it may use, which
credential it may spend and which headers it may set are the Definition's
`capabilities` declaration and the host's grant: **data**, in the memory,
replicated with the tool, auditable without reading any code at all.

The consequence worth stating plainly: this blob has no opinion to get wrong,
and its behaviour cannot drift from its declaration, because there is nothing
between the two.

```json
{ "tool_name": "vendor_api", "kind": "definition",
  "executor_uri": "cas://sha256:6c088ed0…",
  "runtime": "wasm32-areev-io",
  "runtime_limits": { "fuel": 200000000, "max_pages": 256, "max_calls": 8 },
  "capabilities": [
    { "http": { "hosts": ["https://api.vendor.example"],
                "methods": ["GET", "POST"],
                "path_prefixes": ["/v2/invoices/"],
                "credentials": ["vendor"],
                "headers": ["X-Api-Version"] } } ] }
```

Two Definitions may name the **same blob** with different declarations — that
is the point. One address to pin, one blob to review, and a per-service policy
that a synced memory states out loud.

`examples/blessed-tools/` runs exactly this, offline: a pack binds `http.call`
with declared hosts, one call is permitted (200, with the broker attaching a
credential the tool never holds), one aimed at an undeclared host is refused by
the broker with `RUN-E022`, and `areev tool provenance` chains the blob.

## Contracts

### `http.call`

**In**: the broker's own request shape, forwarded rather than translated.

```json
{ "url": "https://api.vendor.example/v2/invoices/4471",
  "method": "GET", "credential": "vendor",
  "headers": { "X-Api-Version": "2026-01-01" }, "body": null }
```

**Out**: the broker's own answer, forwarded rather than translated —
`{"status": 200, "body": "…"}`, or `{"error": "…", "code": "RUN-E022"}` when
policy said no. The refusal keeps the code a reader can look up.

An input that is not a request is refused by the broker, which was going to
decide anyway; validating it in the guest would add a second opinion that can
drift.

### `mcp.call`

One envelope and one unwrap over `http.call`.

```json
{ "url": "https://mcp.example.com/mcp", "credential": "mcp",
  "tool": "search_docs", "arguments": { "q": "invoice 4471" }, "id": 1 }
```

builds `{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":…,"arguments":…}}`
and returns `{"status": 200, "result": {…}}` — or `{"status": …, "error": {…}}`
for a JSON-RPC error, or the broker's refusal verbatim.

`arguments` is **sliced out of the input and copied byte for byte**, never
reparsed and reserialized, so a server that cares about key order or number
formatting sees exactly what the caller wrote. `method` + `params` passed
explicitly reach any other MCP method without this blob carrying a table of
them.

`headers` defaults to `{"Content-Type": "application/json"}` — a JSON-RPC
endpoint receiving `text/plain` answers 415 — and a declaration must name every
header it sets. Not supported in v1: the SSE/streamable transport.

### `a2a.call`

The same envelope with `message/send` and the A2A `params` shape:

```json
{ "url": "https://partner.example.com/a2a", "credential": "partner",
  "text": "Invoice 4471 is approved.", "message_id": "m-4471",
  "context_id": "ctx-ap-4471" }
```

`text` builds a one-part user message; `message` passes a whole A2A message
verbatim (files, structured parts); `method` + `params` take over entirely.
Not supported in v1: `message/stream` and push notifications.

`a2a.call` is a separate blob from `mcp.call` rather than a mode flag on one,
because a pack pins an address and declares a capability **per tool**: an
agent-to-agent endpoint and an MCP server are different hosts with different
credentials, and one blob doing both would be one address to grant for both.

### `mailbox.poll`

The trigger-connector example: it reads its items from one filed CAS blob
named by the trigger's `config` (`int:feed_blob`), pages them with the
evaluator's cursor, and declares `{"blob": {"read": true}}` and nothing else.
See `examples/grain-connector/`.

### `rest.poll`

A paginated REST connector as a **declaration**
([#231](https://github.com/AreevAI/areev/issues/231)). `http.call` won this
argument one level down: a blob that forwards a request has no opinion to get
wrong. A poll is one request plus three decisions — where the items are, where
the cursor is, how the next page is asked for — and written in code those three
are invisible to the pack, to `areev tool provenance`, and to the loop's
`code_revision` gate. Written as JSON pointers in the Definition's `config`,
they are data a reviewer reads. Five providers become five declarations naming
one blob.

```json
{ "tool_name": "gmail_poll", "kind": "definition",
  "executor_uri": "cas://sha256:e53d60bd…",
  "runtime": "wasm32-areev-io",
  "runtime_limits": { "fuel": 200000000, "max_pages": 256, "max_calls": 2 },
  "capabilities": [
    { "http": { "hosts": ["https://gmail.googleapis.com"],
                "methods": ["GET"],
                "path_prefixes": ["/gmail/v1/users/me/"],
                "credentials": ["gmail"] } } ],
  "config": {
    "url": "https://gmail.googleapis.com/gmail/v1/users/me/messages",
    "query": { "q": "newer_than:1d" },
    "credential": "gmail",
    "page_size": "maxResults",
    "items": "/messages",
    "id": "/id",
    "order": "newest_first",
    "next_page": { "token_at": "/nextPageToken", "as_query": "pageToken" } } }
```

**In**: the evaluator's `PollRequest` (`cursor`, `max_items`, `config`).
**Out**: the connector contract's `{ items, cursor, more }`, or
`{"error": …}` — which fails the poll rather than reading as a quiet source.

| `config` key | meaning |
|---|---|
| `url` | **required** — the endpoint to poll |
| `method` | default `GET` |
| `credential` | the credential NAME the broker spends; the tool never holds a value |
| `headers` | non-credential request headers |
| `query` | static query parameters |
| `body` | a request body, for a source that polls by POST |
| `page_size` | query parameter the request's `max_items` rides on |
| `cursor_param` | query parameter the stored watermark rides on |
| `items` | JSON pointer to the array of items (`""` = the body IS the array) |
| `id` | pointer WITHIN an item to its id (default `/id`) |
| `cursor_from` | pointer into the RESPONSE to the new watermark; `-` selects an array's last element |
| `order` | `oldest_first` (default) or `newest_first` — which end of a page is newest |
| `next_page` | `{ "token_at": <pointer>, "as_query": <parameter> }` |

Pointers are RFC 6901 with one extension: **`-` selects an array's last
element**. The RFC gives `-` no resolvable meaning (it is JSON Patch's append
position), so nothing is overloaded, and "the id of the last message" becomes
writable as `/messages/-/id`.

**The four cursor rules, written once.** These are the ones every connector
implementer gets wrong, which is the argument for one reviewed copy:

1. **An absent cursor means "leave it where it is."** A poll that found
   nothing emits no `cursor` key — not `null`, which would rewind the source.
   The one exception is a drain that ended on an empty page: the stored cursor
   still carries an exhausted page token, so the watermark is written back
   without it.
2. **The watermark advances over everything LOOKED AT.** This tool emits every
   item on the page it fetched, so "looked at" and "delivered" coincide by
   construction — and `max_items` is expressed to the source as a page size
   (`page_size`), never used to slice a page afterwards. Slicing a page you
   cannot resume mid-way is how a poller loses its tail or re-reads its head
   forever. A page whose items are all filtered out downstream still advances,
   because the connector delivered them and the filtering happened after.
3. **The first poll seeds.** The evaluator keeps the cursor and fires nothing
   ([triggers.md](triggers.md)); what this tool must do is return a usable
   watermark on that poll.
4. **`more: true` drains without hammering.** A page token means the next
   invocation runs immediately instead of waiting out the interval.

**The cursor** is opaque and connector-defined, and written to be read by a
person looking at `areev trigger status`: `"m-1802611"` is an ordinary
watermark, and `{"w":"m-1802611","p":"CAUQAA"}` is a drain in flight — the
watermark this drain decided, plus the page to ask for next. A cursor set by
hand to a bare string is therefore always valid. While a drain is in flight the
watermark moves with every page under `oldest_first` (a crash resumes from the
last page actually delivered) and is held from the first page under
`newest_first` (where the newest item was on page one, so a crash re-reads
rather than skips). Both rules are "never lose an item"; neither can rewind.

Reach is still the declaration's business: a `url` the `capabilities` block
does not admit is refused by the broker, and the refusal is journaled.

## Running one

```bash
areev run start --db m.db --workflow <PLAN> --run-id r1 \
  --allow-executor 6c088ed0… --sandbox-cmd areev-sandbox \
  --credential vendor=VENDOR_TOKEN \
  --allow-host https://api.vendor.example \
  --tool-egress 'vendor_api:vendor:GET,POST'
```

Nothing runs that the host did not pin, and the effective reach is
`declared ∩ host-granted`, checked per call. `docs/run.md` has the full table
of what is enforced where.

## Building them

```bash
areev-tools/build.sh            # build, refresh dist/, print the addresses
areev-tools/build.sh --check    # what CI asserts about the committed blobs
```

`areev-tools/` is a standalone cargo workspace — `no_std`, no dependencies, one
shared crate for the guest ABI and JSON slicing. It is not a member of the root
workspace for the same reason `areev-sandbox` is not: it builds for
`wasm32-unknown-unknown`, and `cargo test --workspace` has no business trying
to run a cdylib that has no host.

The blobs are tested where it matters — `areev-sandbox/tests/blessed_tools.rs`
runs the **committed bytes** under the real engine against a loopback broker
stand-in, asserting the request each one builds, the answer each one returns,
and that the import gate holds: a tool that declared no network does not get
`areev::fetch` linked, and one that declared no blob read does not get
`areev::blob_get`.
