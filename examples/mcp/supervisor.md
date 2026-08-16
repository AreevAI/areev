# Multi-agent supervisor pattern (separation of duties)

Areev Loop's self-approval block bites when actors are distinct. Run **two MCP
processes over the same backend file** with different scopes and actors, so a
worker agent can propose but only a reviewer can approve.

> The scope model is per-process host policy: whoever launches an `areev serve
> --mcp` process grants it its scopes. (Per-process `--scopes`/`--actor` flags
> on `areev serve --mcp` are the mechanism; wire them from your MCP client's
> server config.)

### Worker — proposes, cannot approve

```jsonc
// mcp server config (worker)
{ "command": "areev",
  "args": ["serve", "--mcp", "--db", "agent.db", "--ns", "caller",
           "--scopes", "read,write", "--actor", "agent:worker"] }
```

The worker captures tool calls (`areev_add` / a `record_tool_call` loop) and
may run `areev_loop` to surface recommendations — but a `write`-scoped
actor holds neither `review` nor `apply`.

### Reviewer — approves and applies

```jsonc
// mcp server config (reviewer)
{ "command": "areev",
  "args": ["serve", "--mcp", "--db", "agent.db", "--ns", "caller",
           "--scopes", "review,apply", "--actor", "agent:reviewer"] }
```

### The money shot

If the worker tries to approve its own proposal, the engine refuses:

```
areev_recommendations { "action": "approve", "hash": "…", "because": "lgtm" }
→ LOP-E021 self-approval blocked: agent:worker created this recommendation
```

No agent can rubber-stamp its own memory edits. The reviewer approves with a
written reason; the transition is recorded as a hash-chained audit grain that
names both actors.
