# The Anthropic memory-tool backend

Anthropic's [memory tool](https://platform.claude.com/docs/en/agents-and-tools/tool-use/memory-tool)
(`memory_20250818`) is client-side: Claude issues `view` / `create` /
`str_replace` / `insert` / `delete` / `rename` commands against a `/memories`
path space, and **you** implement the storage backend. Areev ships that
backend — in Python, Node, and the CLI — with grains instead of naive files:

| naive file backend | Areev backend |
|---|---|
| edits overwrite; history is gone | every edit is a **supersession** — full version history stays queryable |
| rename loses the file's past | rename links provenance (`derived_from`) across the move |
| delete = hope the bytes are gone | delete = auditable tombstones; with an encrypted file, key destruction is **crypto-erasure** |
| files are invisible to search | file bodies ride the BM25 (and vector) recall legs like any other memory |
| separate storage from the rest of agent memory | memory files live in the same file as facts/events — one recall surface, one sync/backup story |

Each file is a supersession chain of Fact grains: subject = the path,
relation = `memory_file`, body in `context.content` (and mirrored into the
indexed `embedding_text`). Path traversal outside `/memories` is rejected.

## Wire it up

### Python (Anthropic SDK tool loop)

```python
import json, areev
from anthropic import Anthropic

m = areev.Areev("agent.db", ns="assistant")
client = Anthropic()

messages = [{"role": "user", "content": "Remember: I'm vegetarian."}]
while True:
    resp = client.messages.create(
        model="claude-sonnet-5",
        max_tokens=1024,
        tools=[{"type": "memory_20250818", "name": "memory"}],
        messages=messages,
    )
    if resp.stop_reason != "tool_use":
        break
    results = []
    for block in resp.content:
        if block.type == "tool_use" and block.name == "memory":
            try:
                out = m.memory_tool(json.dumps(block.input))   # ← Areev backend
            except ValueError as e:
                out = f"Error: {e}"
            results.append({"type": "tool_result", "tool_use_id": block.id,
                            "content": out})
    messages.append({"role": "assistant", "content": resp.content})
    messages.append({"role": "user", "content": results})
```

### Node

```js
const { Areev } = require('@areev/areev')
const m = new Areev('agent.db', 'assistant')

// inside your tool loop (every method returns a promise):
const out = await m.memoryTool(JSON.stringify(toolUse.input))
```

### CLI (inspect / script / test)

```bash
areev memtool '{"command": "view", "path": "/memories"}' --db agent.db
areev memtool '{"command": "create", "path": "/memories/prefs.md", "file_text": "Vegetarian."}' --db agent.db
```

## What each command does here

| command | behavior on grains |
|---|---|
| `view` | directory listing (`/memories` or any `.../` prefix), or the file numbered line-by-line — an `entity_latest` point read (~µs) |
| `create` | new chain, or a **new version** of an existing file (old version stays in history) |
| `str_replace` | validates uniqueness of `old_str`, writes a superseding version |
| `insert` | line insert, superseding version |
| `delete` | forgets the whole chain — tombstoned in the op-log, auditable |
| `rename` | new chain at the new path with `derived_from` provenance to the old head; old chain is forgotten (v1: version history does not carry across renames — the provenance link preserves the connection) |

Because versions are ordinary grains, everything else applies to them: `areev
history --subject /memories/prefs.md --relation memory_file` shows every
edit, `areev search` finds file bodies, bundles/streaming replicate them, and
encryption at rest covers them.

## Interop notes

- The command set is the Messages-API memory tool (GA, no beta header). Any
  agent framework that emits the same JSON commands can use this backend —
  it's just a dict in, string out.
- Imports can pre-populate the space: `areev migrate --from basic-memory`
  lands each note at `/memories/<permalink>`, and Letta core-memory blocks
  land at `/memories/letta/<agent>/<label>` — immediately editable by the
  tool. See [migrate.md](migrate.md).
- Namespacing: pass `ns` to isolate one agent's `/memories` from another's
  inside the same file, or use one file per agent (one memory = one file).
