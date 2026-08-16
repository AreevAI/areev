# areev-cal

CAL (Context Assembly Language) lexer, parser, and executor for Areev.

`areev-cal` implements CAL, the query language for reading memories. It
provides the lexer, parser, AST, and executor, along with the ASSEMBLE engine,
templates, and saved queries, all executed against the embedded store through
`AreevFacade`. CAL is primarily a read and assembly language: its only
destructive statement is `FORGET <hash>` (a single-grain tombstone), gated by
`allow_destructive_ops` (on by default, disable with `--no-destructive-ops`);
`DELETE`/`DROP` are not grammar tokens and there is no bulk erasure — so queries
are safe to run against durable memory, and can be made fully read-only for
untrusted input. The facade also supports cross-file queries via mounts, letting
a single ASSEMBLE draw from several one-file memories.

Part of [Areev](https://github.com/AreevAI/areev) — an embedded memory engine for AI agents. See the [architecture overview](https://github.com/AreevAI/areev/blob/main/ARCHITECTURE.md).

Licensed under MIT OR Apache-2.0.
