# Migrating to Areev

`areev migrate` imports another memory system's export into an Areev file —
with original timestamps, provenance, and (where the source has one) the full
edit history preserved. Everything is file-based: you dump your data with the
one-liners below, then point `areev migrate` at the file. Areev never calls
your old provider's API.

```bash
areev migrate --from <source> --file <path> [--history <path>] --db mine.db [--ns NS]
```

Sources: `mem0` · `mem0-history` · `langgraph` (alias `langmem`) · `letta` ·
`letta-archival` · `zep` (alias `graphiti`) · `basic-memory` · `jsonl`.

The same importers are available in the bindings —
`m.migrate(source, payload, history=None)` in Python,
`m.migrate(source, payload)` in Node — except `basic-memory`, whose
directory walk is CLI-only.

**What every import guarantees:**

- `created_at` keeps the **source's original timestamp**; Areev's op-log
  separately records when this store learned each grain. Both truths survive.
- Every imported grain carries `source_type: "import"` and a `context.import`
  block with the source name and original ids — auditable back to where it
  came from.
- Prose is indexed for BM25 (and for vectors, if an embedder is installed —
  see [Embeddings](#embeddings-during-import)).
- **Re-running an import is a no-op**: chains that already exist and grains
  whose content address is already present are counted as `skipped`, never
  duplicated, never an error. (Exact re-run dedup needs source timestamps —
  real exports have them.)
- Bulk speed: the importer defers the BM25 index for the duration and rebuilds
  it once at the end, so a 10k-record import takes seconds.

The report is JSON: `{"added": n, "superseded": n, "forgotten": n,
"skipped": n, "notes": [...]}` — per-record anomalies land in `notes` (on
stderr for the CLI) instead of failing the run.

---

## mem0

The one importer that keeps **history**. mem0's `history()` events replay as
real Areev supersession chains — `ADD` → add, `UPDATE` → supersede, `DELETE`
→ forget — with their original timestamps, so `areev history --subject
mem0/<id> --relation mem0_memory` shows your memory's pre-import evolution.
(The official mem0→Zep and mem0→Supermemory guides keep only final state.)

Each memory becomes a Fact chain: subject `mem0/<memory-id>`, relation
`mem0_memory`, the text in `context.content`, `user_id`/`categories` mapped
to Areev's `user_id`/`tags`, `metadata` preserved under `context.import`.

**Consequence, stated plainly:** because the subject is the opaque
`mem0/<uuid>`, **structural recall matches nothing you carry over** — a
`areev recall --subject john` finds no imported memories. Imported prose is
reachable via the **text and vector legs** (`areev search --query "..."`,
best with `--embed-cmd`), which is how mem0 stored it too. That is fine for
retrieval; it just means the microsecond structural point-reads apply to
memories you write *after* import, not to the UUID-keyed ones you brought.

To join the structurally-keyed world, **re-key** during a one-time pass: run
your extractor over each imported `context.content` to get real
`(subject, relation, object)` triples, then `SUPERSEDE sha256:<imported-hash>
SET subject = "..." SET relation = "..." SET object = "..."` — the supersession
keeps the imported grain as history and the new tip carries structural keys.
Areev has no LLM, so the extraction is your model call (same seam as the
reflection harness in [`cookbook.md`](cookbook.md#10-build-an-agent-that-learns-and-can-unlearn)).

### mem0 Platform (hosted)

Page through the get-all endpoint and, if you want history, the per-memory
history endpoint:

```python
# dump_mem0.py — writes export.json + history.json
import json, os, requests
H = {"Authorization": f"Token {os.environ['MEM0_API_KEY']}"}
memories, page = [], "https://api.mem0.ai/v3/memories/"
body = {"filters": {"user_id": "YOUR_USER_ID"}, "page_size": 200}
while page:
    r = requests.post(page, headers=H, json=body).json()
    memories += r["results"]; page = r.get("next")
json.dump({"results": memories}, open("export.json", "w"))

events = []
for m in memories:
    r = requests.get(f"https://api.mem0.ai/v1/memories/{m['id']}/history/", headers=H)
    events += r.json()
json.dump(events, open("history.json", "w"))
```

```bash
areev migrate --from mem0 --file export.json --history history.json --db mine.db
```

### mem0 OSS (self-hosted)

```python
# from your mem0 environment
import json
from mem0 import Memory
m = Memory()  # your existing config
json.dump(m.get_all(user_id="YOUR_USER_ID"), open("export.json", "w"))
```

If you configured a history database (`history_db_path`), dump it too — the
importer accepts the raw table rows:

```bash
sqlite3 history.db -json "SELECT memory_id, old_memory, new_memory, event, created_at, updated_at FROM history" > history.json
areev migrate --from mem0 --file export.json --history history.json --db mine.db
```

History-only (no get-all): `areev migrate --from mem0-history --file history.json`.

## LangMem / LangGraph store

LangMem persists through the LangGraph `BaseStore`; on Postgres that is the
`store` table. Dump it as JSONL:

```bash
psql "$DATABASE_URL" -Atc \
  "SELECT row_to_json(t) FROM store t" > store.jsonl
areev migrate --from langgraph --file store.jsonl --db mine.db
```

Each item becomes a Fact `langgraph/<prefix>/<key>` with the value's prose
(or compact JSON) in `context.content` and the full structured value
preserved.

## Letta

Two feeds, because Letta's own `.af` export **does not include archival
memory**:

```bash
# 1) agent file: core-memory blocks + message history
curl -H "Authorization: Bearer $LETTA_API_KEY" \
  "$LETTA_BASE_URL/v1/agents/$AGENT_ID/export" > agent.af
areev migrate --from letta --file agent.af --db mine.db

# 2) archival passages (paginated) → JSONL
python3 - <<'EOF'
import json, os, requests
base, agent = os.environ["LETTA_BASE_URL"], os.environ["AGENT_ID"]
H = {"Authorization": f"Bearer {os.environ['LETTA_API_KEY']}"}
after, out = None, open("archival.jsonl", "w")
while True:
    params = {"limit": 100, **({"after": after} if after else {})}
    page = requests.get(f"{base}/v1/agents/{agent}/archival-memory", headers=H, params=params).json()
    if not page: break
    for p in page: out.write(json.dumps(p) + "\n")
    after = page[-1]["id"]
EOF
areev migrate --from letta-archival --file archival.jsonl --db mine.db
```

Core-memory blocks land as **live memory-tool files** at
`/memories/letta/<agent>/<label>` (see [memory-tool.md](memory-tool.md));
user/assistant messages become thread-indexed Events; archival passages
become Events with their tags.

## Zep / Graphiti

Zep has no export endpoint; enumerate edges and episodes with the SDK (Cloud)
or Cypher (self-hosted Graphiti) into one JSON file:

```python
# dump_zep.py — Zep Cloud SDK
import json
from zep_cloud.client import Zep
client = Zep(api_key="...")
edges = [e.dict() for e in client.graph.edge.get_by_user_id("USER_ID")]
episodes = [ep.dict() for ep in client.graph.episode.get_by_user_id("USER_ID", lastn=1000)]
json.dump({"edges": edges, "episodes": episodes}, open("zep.json", "w"), default=str)
```

```bash
areev migrate --from zep --file zep.json --db mine.db
```

Fidelity note: Zep's bi-temporal `valid_at`/`invalid_at` maps onto Areev's
world-time validity axis (`valid_from`/`valid_to`) — invalidated facts import
as *no longer valid* instead of polluting current recall. Episodes become
thread-indexed Events.

## Basic Memory

Point at the vault directory — every markdown note becomes a live
memory-tool file at `/memories/<permalink>` (frontmatter `title`, `tags`,
`permalink`, and `created`/`date` are honored; file mtime is the timestamp
fallback):

```bash
areev migrate --from basic-memory --file ~/basic-memory --db mine.db
```

Re-imports never clobber a note the agent has since edited.

## Anything else (pgvector, Chroma, homegrown)

Dump your table to JSONL with one object per line. `subject` + `relation` +
`object` makes a Fact; `content` (or `text`/`memory`) alone makes an Event.
Optional per line: `created_at` (ISO-8601 or epoch), `confidence`, `tags`,
`user_id`, `session_id`, `embedding_text`.

```bash
# pgvector-ish example
psql "$DATABASE_URL" -Atc \
  "SELECT json_build_object('content', body, 'created_at', created_at) FROM memories" > memories.jsonl
areev migrate --from jsonl --file memories.jsonl --db mine.db
```

```python
# Chroma example
import json
col = client.get_collection("memories")
data = col.get(include=["documents", "metadatas"])
with open("memories.jsonl", "w") as f:
    for doc, md in zip(data["documents"], data["metadatas"]):
        f.write(json.dumps({"content": doc, **(md or {})}) + "\n")
```

---

## Embeddings during import

Vectors are host-supplied (Areev ships no model). To embed while importing,
install an embedder first:

```bash
areev migrate --from mem0 --file export.json --db mine.db \
  --embed-cmd 'python3 my_embedder.py' --embed-model bge-m3
```

`my_embedder.py` reads the text from stdin and prints a JSON array of floats.
One process is spawned per grain — fine for thousands of records, slow for
millions; without an embedder the import still lands with structural + BM25
recall, and you can re-import into an embedder-equipped file later. In
Python, pass a callback instead: `m.set_embedder(fn)` before `m.migrate(...)`.

## After the import

```bash
areev stats   --db mine.db                    # grains / triples / ops
areev verify  --db mine.db                    # full content-address recheck
areev search  --query "anything" --db mine.db # BM25 (+ vectors if embedded)
areev memtool '{"command": "view", "path": "/memories"}' --db mine.db
```

Reach for `areev search`, not `areev recall --subject`, to confirm mem0 imports
landed — they are keyed by `mem0/<uuid>` and only the text/vector legs find
them (see the note under [mem0](#mem0) on re-keying).
