# areev-core

Core `.mg` format, canonical serialization, content addressing, and grain types for Areev.

`areev-core` is the foundation of the Areev workspace. It defines the OMS
grain types (Fact, Event, Observation, and the rest), the `.mg` binary format,
and the canonical serialization used to compute each grain's SHA-256 content
address. Canonical serialization is frozen — NFC normalized, sorted compact
keys, defaults omitted — so an identical grain always hashes to the same
address, which is what makes memories immutable and content-addressed. Every
crate above it in the stack (store, CAL, context) builds on these types.

Part of [Areev](https://github.com/AreevAI/areev) — an embedded memory engine for AI agents. See the [architecture overview](https://github.com/AreevAI/areev/blob/main/ARCHITECTURE.md).

Licensed under MIT OR Apache-2.0.
