# areev-server

Web console server for Areev.

`areev-server` is the opt-in HTTP surface for Areev. It powers the local
inspection console (`areev ui`) — a JSON API plus a single embedded HTML page
for browsing memories, exploring the graph, and running queries — built on a
deliberately minimal std-only HTTP/1.1 server that binds loopback with no auth.
`areev ui --token-env VAR` requires a shared token on every request, and
`--auth FILE` binds a per-principal credential instead. It is an inspection
surface, not part of the recall hot path.

Part of [Areev](https://github.com/AreevAI/areev) — an embedded memory engine for AI agents. See the [architecture overview](https://github.com/AreevAI/areev/blob/main/ARCHITECTURE.md).

Licensed under MIT OR Apache-2.0.
