# areev-store

Content-addressed embedded store with hybrid recall (Turso/SQLite) for Areev.

`areev-store` is the persistence and retrieval layer. It stores grains as
immutable content-addressed blobs and maintains the index layer around them:
dictionary-encoded triple permutations (SPO/POS, with selective OSP),
`entity_latest` materialization, an op-log with HLC ordering and tombstones, a
thread index, and CAS blobs. On top of that it provides the core memory
operations — add, recall, batch, supersede, forget — plus hybrid recall (BM25
text search fused with structural lookup) and bounded graph traversal. It wraps
the async Turso engine behind a synchronous API using a private current-thread
runtime, so callers get a simple blocking interface.

Part of [Areev](https://github.com/AreevAI/areev) — an embedded memory engine for AI agents. See the [architecture overview](https://github.com/AreevAI/areev/blob/main/ARCHITECTURE.md).

Licensed under MIT OR Apache-2.0.
