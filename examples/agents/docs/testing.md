# Testing the agent examples

An untested example in the flagship repo rots invisibly — and it is wrong at
exactly the moment somebody is evaluating. So every agent example is a
**test**: a keyless, deterministic, fixture-driven smoke that CI executes on
every push, and that you run locally before pushing to main.

## The shape

Each agent directory carries two **language-neutral act scripts** —
`smoke.sh` (the job, under governance) and `improve.sh` (the loop, under
governance) — whose assertions are the example's spec. Each language stack
(`python/`, `typescript/`, `rust/`) has 3-line wrappers that export

- `AGENT` — how to invoke that stack's agent (all stacks expose identical
  subcommands), and
- `AGENT_OUT` — where its artifacts land (`out/`, gitignored),

then `exec` the shared act script. One set of assertions proves three
implementations; a fourth language is a wrapper plus an agent file.

The act scripts additionally write `out/workflow.hash`, because every
stack's seeder pins `created_at`: **all stacks of one agent must mint the
identical plan hash**. That turns "did a stack drift?" into a string
comparison.

## Run locally (before pushing to main)

```bash
examples/agents/run-smokes.sh                 # every agent × every stack you can run
REQUIRE="python typescript rust" examples/agents/run-smokes.sh   # skips become failures
```

The runner skips a stack loudly when its toolchain is missing. Getting each
one ready, in-tree:

| Stack | One-time setup |
|---|---|
| python | `python3 -m venv .venv && . .venv/bin/activate && pip install maturin && maturin develop --release -m crates/areev-py/Cargo.toml` — or just `pip install areev` to test against the released binding. Pass `PYTHON=` to point the runner at a specific interpreter. |
| typescript | node ≥ 22.6 and a built binding: `cd crates/areev-js && npm ci && npm run build` (the wrappers find it via `$AREEV_JS` automatically) — or `npm i @areev/areev` inside the agent's `typescript/`. |
| rust | nothing beyond the repo's toolchain; the stack builds against the sibling crates by path. |

## What CI runs

The `agent-example` job in [`.github/workflows/ci.yml`](../../../.github/workflows/ci.yml)
builds all three surfaces from the tree (the `areev` binary is not needed —
the stacks embed the crates/bindings directly), then runs exactly the local
entry point with skips forbidden:

```yaml
REQUIRE="python typescript rust" examples/agents/run-smokes.sh
```

Local and CI share one script on purpose: a green local run predicts a
green job, and there is no second harness to drift.

## Determinism — the two ways an act script goes flaky

Both of these have actually bitten agents in this directory, and both pass
locally before they fail in CI.

**Clocked triggers versus guessed sleeps.** A `polling` trigger declaring
`interval_secs: 1` is only eligible once its `next_due_at` has passed, so an
act script that paces with `sleep 1.2` is betting that the *previous* step
took under 200 ms. That bet holds on an idle laptop and loses in a job that
has already run ten other agents — the tick reports
`skipped_not_due`, and the assertion fails with a confusing "expected 1 run,
got 0". Prefer **waiting for the condition** over sleeping a guessed amount:
poll `trigger_status()` until the trigger reports `due: true`, under a
bounded overall timeout that fails loudly. Where a sleep is unavoidable,
assert `due` *before* the tick, so a timing failure says "not due yet"
rather than pointing at your business logic. The mirror image is worse: an
assertion that something did **not** happen (a correlation window expiring,
a backoff elapsing) will start passing *spuriously* under load, quietly
testing nothing.

**Exit codes through a pipe.** `run-smokes.sh | tail -60` reports **`tail`'s**
exit status, not the harness's — a red run reads as green. Redirect to a file
and check the status, or run the harness bare. This is how a genuinely
failing agent was first mistaken for a passing one.

## Adding an agent (or a language stack)

1. Follow the layout in the `areev-examples` skill
   (`.claude/skills/areev-examples/`): `README.md`, `fixtures/` (synthetic
   only), the two act scripts, per-language stacks with wrappers.
2. Make the act scripts exit non-zero on any drift — assert outcomes, not
   logs. Model judgment: at least one refusal, at least one decision with a
   written reason.
3. Keep the keyless floor absolute: no credential, no network, no model
   key in anything the act scripts touch. Live legs are env-gated scripts
   CI never invokes.
4. Update the four indexes in the same commit (`examples/agents/README.md`,
   `examples/README.md`, the repo README's Examples table, the CI job) —
   `run-smokes.sh` discovers your agent automatically once `smoke.sh`
   exists, so the CI edit is usually nothing.
