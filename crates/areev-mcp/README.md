# areev-mcp

Model Context Protocol (MCP) stdio server for Areev.

`areev-mcp` exposes Areev to MCP-capable agents and hosts. It serves a small,
memory-semantic tool set — `areev_recall`, `areev_remember`, `areev_add`,
`areev_supersede`, `areev_forget`, and `areev_cal` — over newline-delimited
JSON-RPC 2.0 on stdio, rather than exposing raw SQL. Following the MCP
convention, protocol-level problems are returned as JSON-RPC errors while
tool-execution failures come back as `isError: true` tool results. This lets an
agent read and write durable memory using the same tool-calling interface it
uses for everything else.

Part of [Areev](https://github.com/AreevAI/areev) — an embedded memory engine for AI agents. See the [architecture overview](https://github.com/AreevAI/areev/blob/main/ARCHITECTURE.md).

Licensed under MIT OR Apache-2.0.
