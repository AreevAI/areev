# A connector that is a grain

**The problem.** A polling trigger's connector — the code that actually reads
the mailbox, the ledger, the ticket queue — used to be a host command
(`--connector-cmd ./gmail.sh`). That put the one piece of code most likely to
be wrong *outside* the memory: it did not travel in a bundle, `areev tool
provenance` could not chase it, and the loop could not propose a
`code_revision` against it. Cursor bugs, pagination edge cases and provider
quirks are exactly the failures a gated code revision is built to fix, and they
were invisible to a loop that could not see the code.

**What this shows.** A trigger that names its connector by **content address**
(`connector_tool`, [#185](https://github.com/AreevAI/areev/issues/185)): the
connector is a Tool Definition whose `executor_uri` carries a
`wasm32-areev-io` blob, pinned by the host, sandboxed, and answered by the same
credential broker a run's tools use. There is no host connector script in this
directory.

```
areev trigger run                       the memory
      │                                 ┌──────────────────────────────────┐
      │  claim, decide due              │ Trigger  connector_tool ─────────┼──┐
      ├────────────────────────────────▶│ Tool     executor_uri cas://…    │◀─┘
      │                                 │ blob     the wasm module         │
      │  pinned? --allow-executor       │ blob     the filed mailbox       │
      ├── no ──▶ TRG-E012, nothing ran  └──────────────────────────────────┘
      │                                              ▲
      │  yes: areev-sandbox ──areev::blob_get──▶ broker (holds the memory)
      │                                          journals every read
      └── items ──▶ dedup fence ──▶ one run per message
```

## Run it

```bash
cargo build -p areev
cargo build --manifest-path areev-sandbox/Cargo.toml
examples/grain-connector/run.sh
```

Keyless and offline. The connector reads its mailbox from a **filed blob**
rather than an API, through the same broker on the same token a network
connector calls out through — so the pin, the sandbox, the capability check,
the cursor, the dedup fence and the run start are all exercised with no
credential and nothing to reach. A production connector differs in one line: it
declares `{"http": {...}}` and calls `areev::fetch` instead of
`areev::blob_get`. Everything around it is the same.

## What the pack installs

`pack/` is an installable pack ([#178](https://github.com/AreevAI/areev/issues/178)) —
`areev pack install pack --db mailbox.db` — carrying four grains and two blobs:

| Grain | Why |
|---|---|
| `mailbox.poll` (Tool Definition) | the connector: `runtime: "wasm32-areev-io"`, `capabilities: [{"blob": {"read": true}}]`, `executor_uri: blob:mailbox_poll` |
| `triage` (Tool Definition) | `executor_kind: "client"` — a person answers it, so the example needs no tool command either |
| `mailbox-triage` (Workflow) | one node, bound to `triage` |
| `ap-mailbox` (Trigger) | `kind: "polling"`, `connector: "mailbox"`, **`connector_tool`** naming the Definition above |

Note what the manifest can and cannot say. Blob and grain references are
**symbolic** — `blob:mailbox_poll`, `grain:poll` — because an address is a
measurement of bytes, not something an author can write down: install stores
the blob, learns its address, rewrites the Definition, addresses *that*, and
rewrites the trigger. `expected_hash` is how a deployment pins the result.

## The two authorizations, and why they are separate

The connector's code travels **in the memory**. The permission to execute it
deliberately does not:

```bash
areev trigger run --db mailbox.db --ns demo \
  --allow-executor 2a8f2d40…  --sandbox-cmd areev-sandbox
```

Without `--allow-executor`, the trigger refuses with `TRG-E012` naming the
address to pin — step 2 of `run.sh` shows it. A bundle carries the connector,
so a permission arriving in the same bundle as the code it authorizes would not
be a permission at all. This is the same split `--allow-executor` makes for a
plan's nodes; the trigger path takes the same flags, and means the same thing
by them.

`connector` (the name) stays beside `connector_tool` (the code) because the run
id is derived from `(trigger, connector, dedup value)`: revising the code must
not renumber the runs.

## Where to go next

- [`docs/triggers.md`](../../docs/triggers.md) — the connector contract, both kinds
- [`docs/run.md`](../../docs/run.md) — capability tools, the pin, and the broker
- [`docs/blessed-tools.md`](../../docs/blessed-tools.md) — the shared `http.call` / `mcp.call` / `a2a.call` blobs, and the module this example's connector was built beside
- [`areev-tools/`](../../areev-tools/) — the source of that wasm module (`mailbox-poll`), and how to build your own
