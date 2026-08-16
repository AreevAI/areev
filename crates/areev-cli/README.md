# areev

Command-line interface for Areev, the embedded memory engine for AI agents.

`areev` builds the `areev` binary — a thin shell over the store and CAL
layers where one memory is one file. It offers verbs for the full lifecycle:
adding grains, structural and hybrid recall, running CAL queries, inspecting
history and the op-log, moving data with bundles, streaming, and restore, and
migrating in from other memory systems (`areev migrate` — mem0, Zep, Letta,
LangMem, Basic Memory, generic JSONL). It also drives the MCP server and the
local web console, so it is the primary hands-on entry point to an Areev
memory.

```sh
# add a fact, then recall it as model-ready context
areev add    --db demo.db --ns caller --subject john --relation prefers --object tea
areev recall --db demo.db --ns caller --subject john --render sml
```

Part of [Areev](https://github.com/AreevAI/areev) — an embedded memory engine for AI agents. See the [architecture overview](https://github.com/AreevAI/areev/blob/main/ARCHITECTURE.md).

Licensed under MIT OR Apache-2.0.
