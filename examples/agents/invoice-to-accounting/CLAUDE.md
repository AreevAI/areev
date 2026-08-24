# invoice-to-accounting — working rules

Three parallel implementations of ONE agent. The contract that keeps them
honest:

1. **The act scripts are the spec.** `smoke.sh` and `improve.sh` at this
   level are language-neutral; each language dir has 3-line wrappers that
   export `AGENT` + `AGENT_OUT` and exec them. A behavior change starts in
   the act scripts (assert it), then lands in **all three** agents —
   `python/agent.py`, `typescript/agent.mts`, `rust/src/main.rs` — in the
   same commit. One stack drifting is a broken example, and the smoke's
   workflow-hash comparison (via `../run-smokes.sh`) is designed to catch
   the seeder half of that drift.
2. **The plan hash is load-bearing.** Every seeder pins
   `created_at = 1756000000000` on the workflow, tool-definition, and skill
   grains so all three languages mint identical content addresses. Change
   any seeded grain field in one language and you must change it in all
   three, or `run-smokes.sh` fails on hash mismatch. (Facts and triggers are
   not pinned; only the plan lineage is.)
3. **Keyless floor, always** (`.claude/skills/areev-examples`): the smokes
   run with no credentials, no network, no model key. `connectors/*.py` are
   the live, env-gated exceptions — they must never be invoked by the act
   scripts or CI. Fixtures are synthetic; every vendor, client, address and
   invoice is fictional.
4. **The fixture clock is `MAIL_UPTO`** (default `03`): the mock connector
   only serves fixture files whose 2-digit prefix is ≤ it. Week-one fixtures
   are `01–03` in each mailbox dir, week-two are `04+`. A new fixture's
   prefix decides which act it arrives in — remember both mailboxes share
   the clock.
5. **Reply fixtures carry markers.** The `[areev:ap/<ref>]` in each reply's
   subject is `sha256(message_id)[:12]` of the mail it answers. Change a
   fixture's `message_id` and you must recompute the marker in every reply
   that targets it (and the marker-count assert in `smoke.sh`).
6. **Subcommands `tools` and `connector` never open the memory.** The
   runtime spawns them while the evaluator/runner holds the file (embedded
   backend = one writer). Driver subcommands open per-invocation; the Node
   agent must `close()`.
7. **Namespace rules the seeds encode:** ops grains (plan, tools, triggers,
   journals) live in `org.ops` and that namespace never gets an anonymize
   policy; client knowledge lives in `org.<client>` / `org.<client>.vendors`
   (exact ns on writes); reads use the `"org.*"` prefix.
8. **Correction cluster keys stay short.** `record_tool_call` result strings
   like `corr:vendor:brightco` are the loop's cluster keys, normalized and
   truncated at 80 chars — never free prose, never JSON that can truncate
   mid-structure.
9. Rust stack: detached crate (`[workspace]` in its Cargo.toml), path deps
   into `../../../../crates/` so releases fix it atomically; `Cargo.lock`
   and `target/` are gitignored. TypeScript: `$AREEV_JS` points the wrapper
   at a built `crates/areev-js`; end users get `@areev/areev` from npm via
   `package.json`.
10. **Indexes to sweep with any change here**: `../README.md` (agent index),
    `../../README.md` (examples table), the repo README's Examples table,
    and `.github/workflows/ci.yml` (`agent-example` job). Shared how-to
    material lives in `../docs/` — extend it there, don't fork it into this
    README.
