# areev-langgraph

Areev persistence for LangGraph — checkpoints and long-term memory as
content-addressed, governable grains.

```python
from areev_langgraph import AreevCheckpointSaver, AreevStore

saver = AreevCheckpointSaver("./threads")     # one thread = one memory file
store = AreevStore("./memories.db")           # shared long-term memory

graph = builder.compile(checkpointer=saver, store=store)
```

Why this backend:

- **Every edit is history.** `put` supersedes, never overwrites; `delete`
  is a tombstone. Time-travel that LangGraph promises at the API level is
  what the storage actually does.
- **One thread = one memory file.** `delete_thread` erases a file — the
  cleanest possible right-to-erasure story — and a thread's history
  replicates, forks, and exports as one unit.
- **Governance for free.** The same file works with `areev` CLI tooling:
  DSAR subject reports, audited destruction, retention policies with
  floors and legal holds, run-aware provenance joins.

Notes and honest bounds:

- TTL is not supported (`supports_ttl = False`): retention is declarative
  and audited (`areev retention`), never a per-item timer. `ttl` /
  `refresh_ttl` arguments are accepted and ignored.
- `search` post-filters operator queries over a bounded candidate pool
  (1000 items per call, documented in `store.py`).
- Pickled payloads from other checkpointers are not imported; migration
  skips and reports them.
