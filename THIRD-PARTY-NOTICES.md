# Third-Party Notices

Areev is licensed under MIT OR Apache-2.0 (see `LICENSE-MIT` and
`LICENSE-APACHE`). Binary distributions
statically link the third-party crates below. This file satisfies their
attribution requirements; regenerate with `cargo about generate` (or
`cargo license`) in CI before every release so it stays complete.

## Turso Database (MIT)

Areev's storage engine embeds [Turso Database](https://github.com/tursodatabase/turso)
(`turso`, `turso_core` and related crates).

> MIT License
>
> Copyright (c) 2023-2025 the Limbo authors
> Copyright (c) 2025-present Turso
>
> Permission is hereby granted, free of charge, to any person obtaining a copy
> of this software and associated documentation files (the "Software"), to deal
> in the Software without restriction, including without limitation the rights
> to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
> copies of the Software, and to permit persons to whom the Software is
> furnished to do so, subject to the following conditions:
>
> The above copyright notice and this permission notice shall be included in all
> copies or substantial portions of the Software.
>
> THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
> IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
> FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
> AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
> LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
> OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
> SOFTWARE.

Turso transitively includes [tantivy](https://github.com/quickwit-oss/tantivy)
(MIT) for its experimental full-text index.

## Other direct dependencies

| Crate | License |
|---|---|
| serde, serde_json | MIT OR Apache-2.0 |
| tokio | MIT |
| logos | MIT OR Apache-2.0 |
| thiserror | MIT OR Apache-2.0 |
| parking_lot | MIT OR Apache-2.0 |
| sha2, hex | MIT OR Apache-2.0 |
| chrono | MIT OR Apache-2.0 |
| unicode-normalization | MIT OR Apache-2.0 |
| regex | MIT OR Apache-2.0 |
| lru | MIT |
| tracing | MIT |
| rmpv / rmp | MIT |
| argon2, zeroize | MIT OR Apache-2.0 |
| aes-gcm, hkdf | MIT OR Apache-2.0 |
| getrandom | MIT OR Apache-2.0 |
| croner (areev-trigger only) | MIT |
| ureq (areev-llm, areev-run) | MIT OR Apache-2.0 |
| pyo3 (areev-py only) | MIT OR Apache-2.0 |
| napi, napi-derive (areev-js only) | MIT |
| tokio-postgres (areev-store, `postgres` feature) | MIT OR Apache-2.0 |
| rustls, rustls-pki-types (areev-server, `tls` feature) | Apache-2.0 OR ISC OR MIT |

"MIT OR Apache-2.0" dependencies are used under Apache-2.0. Full license
texts ship in each crate's source distribution via crates.io.
