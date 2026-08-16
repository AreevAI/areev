# Contributing to Areev

Thanks for your interest in Areev — an embedded memory engine for AI agents.
Contributions of all kinds are welcome: bug reports, docs, tests, and code.

By participating you agree to abide by our
[Code of Conduct](CODE_OF_CONDUCT.md).

## Ways to contribute

- **Report a bug** or **request a feature** via the
  [issue templates](https://github.com/AreevAI/areev/issues/new/choose).
- **Ask a question** or share an idea in
  [Discussions](https://github.com/AreevAI/areev/discussions).
- **Report a vulnerability** privately — see [SECURITY.md](SECURITY.md). Do not
  file security issues in public.
- **Send a pull request** — see below.

## Developer Certificate of Origin (DCO)

We use the [Developer Certificate of Origin](https://developercertificate.org/)
instead of a CLA. It is a lightweight statement that you have the right to
submit your contribution under the project's license. To certify it, **sign off
every commit**:

```bash
git commit -s -m "your message"
```

This appends a `Signed-off-by: Your Name <your@email>` trailer. Use your real
name and an email you can be reached at. PRs whose commits are not signed off
will be asked to amend (`git commit --amend -s` / `git rebase --signoff`).

## Licensing

Areev is dual-licensed under **MIT OR Apache-2.0**. Unless you state otherwise,
any contribution you submit is licensed under those same terms
(inbound = outbound), with no additional conditions — as stated in the Apache-2.0
license, section 5.

## Getting started

You need a recent stable Rust toolchain (see `rust-version` in the workspace
`Cargo.toml` for the minimum supported version).

```bash
git clone https://github.com/AreevAI/areev
cd areev
cargo build --workspace
cargo test  --workspace        # full suite, fast
```

Per-crate iteration:

```bash
cargo test -p areev-cal        # one crate
cargo run -p areev -- --help
cargo run --release -p areev-store --example bench       # latency gates
cargo run --release -p areev-store --example voice_loop  # 50ms-cadence gate
```

The workspace is 9 crates in dependency order — see the table in
[README.md](README.md) and the design overview in
[`ARCHITECTURE.md`](ARCHITECTURE.md). Agent- and LLM-oriented
orientation lives in [AGENTS.md](AGENTS.md).

## Coding guidelines

- **Match the surrounding style.** The tree is not uniformly `rustfmt`-formatted;
  please **do not run blanket `cargo fmt`** — format only the lines you touch.
- Keep it warning-clean: `cargo clippy --workspace` should not add new warnings.
- Add or update tests for behavior you change; keep the suite green.
- Prefer clear names and small, reviewable commits with descriptive messages.
- **Dependency-light by policy.** Areev deliberately avoids heavy frameworks
  (no `clap`, no HTTP framework, no MCP SDK, no workspace-wide async runtime).
  Think twice before adding a dependency, and explain the trade-off in your PR.

### Invariants you must not break

Areev has a few load-bearing invariants. Changes that violate these will not be
merged without a design discussion first:

1. **Grains are immutable and content-addressed** (SHA-256 over the whole `.mg`
   blob). Never edit a stored blob — every edit is a supersession, every removal
   a tombstone or crypto-erasure.
2. **Canonical serialization is frozen** (NFC, sorted keys, compact keys,
   omit-defaults). Changing it silently changes every content address and breaks
   spec conformance. See [`crates/areev-core`](crates/areev-core).
3. **CAL's destructive surface is narrow and gated** — the only destructive
   statement is `FORGET <hash>`, gated by `allow_destructive_ops` (default on,
   `--no-destructive-ops` turns it off). `DELETE`/`DROP` are not grammar tokens
   and there is no bulk erasure. Don't widen the destructive surface (bulk
   PURGE, user/scope erasure, new verbs); new CAL syntax requires a spec-level
   decision.
4. **Error codes are append-only.** Every user-facing error carries a stable
   `DOMAIN-Ennn` code; never renumber or reuse one. See
   [ERROR_CODES.md](ERROR_CODES.md).

## Pull request process

1. Fork, create a topic branch, and make your change with signed-off commits.
2. Ensure `cargo test --workspace` passes and you have not introduced new clippy
   warnings.
3. Update docs/tests as needed. Open a PR against `main` using the PR template.
4. A maintainer will review. Please be responsive to feedback; we aim to keep
   the review loop short.

By submitting a pull request, you confirm your commits are signed off (DCO) and
your work is your own (or you have the right to contribute it).

## Questions?

Open a [Discussion](https://github.com/AreevAI/areev/discussions) or read
[SUPPORT.md](SUPPORT.md). Thank you for helping make Areev better!
