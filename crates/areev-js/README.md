# areev-js

Node.js (napi-rs) bindings for Areev, the embedded memory engine for AI agents.

`areev-js` is the napi-rs native addon that exposes Areev to Node.js as the
`areev` package. It mirrors the Python binding with the same thin,
version-stable FFI convention: scalar arguments in, JSON strings out for
anything structured, and errors thrown as JavaScript `Error`s. Because the
underlying engine is native, this is a compiled Node addon rather than WASM. One
memory is one file, opened with a namespace, giving JavaScript agents durable
add / recall / supersede / forget over content-addressed memory.

```js
const { Areev } = require('areev')

const mem = new Areev('caller.db', 'caller') // 3rd arg: passphrase for AES-256 at rest
const h = await mem.addFact('john', 'prefers', 'tea', 0.95)
console.log(await mem.recall('john')) // JSON string, newest-first

await mem.setEmbedderCommand('python3 embed.py')   // vector recall via a host command
await mem.migrate('mem0', exportJson, historyJson) // import an existing corpus (docs/migrate.md)
await mem.memoryTool('{"command": "view", "path": "/memories"}') // Anthropic memory-tool backend
```

## Every method returns a promise

Store calls run on libuv's thread pool, not on the thread running your
application. They used to run inline: a single `migrate` or `importBundle` held
the event loop for its whole duration, so timers, sockets and any HTTP server in
the same process stopped until it finished. Measured on a 400-record `migrate`,
a 5 ms timer now fires **263 times** during the call; before, it could not fire
at all.

Two things follow, and the second is the one that bites:

- **The constructor stays synchronous.** Opening a file should fail loudly, at
  the line that opened it.
- **Await your writes.** Promises settle in completion order, not call order,
  and concurrent calls contend for one lock inside the store. This is a race:

  ```js
  mem.addFact('john', 'prefers', 'tea')  // not awaited
  const rows = await mem.recall('john')  // may or may not see it
  ```

  Awaiting the write fixes it. If you want several independent writes to
  overlap, `Promise.all` them and await the group.

Errors arrive as rejections, so `try`/`catch` around `await`, or `.catch()`.
An un-awaited call that fails becomes an unhandled rejection, which recent Node
versions treat as fatal.

Part of [Areev](https://github.com/AreevAI/areev) — an embedded memory engine for AI agents. See the [architecture overview](https://github.com/AreevAI/areev/blob/main/ARCHITECTURE.md).

Licensed under MIT OR Apache-2.0.
