---
name: areev-examples
description: How `examples/` is structured and how to add one — the two tiers (primitive demos vs `examples/agents/` vertical agents), the keyless-deterministic-floor rule that keeps every example CI-testable without secrets, the no-new-dependency rule (vendor integrations are JSON-on-stdio connectors, never SDKs in the tree), and the four indexes that go stale. Use before adding, moving, or restructuring anything under `examples/`, and whenever a release commit changes a surface an example demonstrates.
---

# examples/

Runnable material for Areev. **Docs-with-files, not a package** — cloned, never
installed.

## Why they live in this repo

Nothing under `examples/` ships in any artifact: the directory sits outside
every crate directory, so `cargo package` never sees it; maturin packages
`crates/areev-py` (whose `include` is the two licenses); napi packages
`crates/areev-js`. Examples cost the published artifacts nothing.

What they buy is **atomicity**: a release that changes a CLI verb, a CAL
result payload, or a binding signature can fix the examples that use it *in the
same commit*. That is the whole reason they are not a separate repo, and it is
load-bearing — an example that lags the release by a week is worse than no
example, because it is wrong at exactly the moment someone is evaluating.

**Do not put an example under `crates/*/examples/`.** That is cargo's namespace:
those are compiled example targets that ship with the crate
(`crates/areev-store/examples/{bench,voice_loop,seed_*}.rs` are the only
legitimate residents, and they are perf gates and seeders, not teaching material).

## The two tiers

| Tier | Where | Teaches | Shape |
|---|---|---|---|
| **Primitive** | `examples/<name>/` | one Areev seam — a protocol, a policy file, a contract | a script or two, a README, usually no deps |
| **Agent** | `examples/agents/<name>/` | a business workflow end-to-end, with Areev as one of several parts | a directory with the layout below |

`colab/`, `policy/`, `import/`, `ci/`, `mcp/`, `llm/`, `analyzers/`, `hermes/`
are tier one. Anything that needs a mailbox, a vendor API, or a model key is
tier two and belongs under `agents/`.

## Rule 1 — the keyless deterministic floor (non-negotiable)

Every example must run end-to-end **with no credentials, no network, and no
model key**, against committed fixtures. The live path is opt-in behind an env
var. This is the existing precedent, not a new invention: the colab notebooks
run on a "keyless deterministic floor, optional LLM discovery", and
`examples/llm/mock.py` is a dependency-free mock backend.

The reason is CI. An example that needs a secret cannot be tested here, and an
untested example in the flagship repo rots invisibly and silently. The keyless
floor is what makes an example a *test* rather than a liability — so it can ride
the release commit without anyone hand-verifying it.

## Rule 2 — no new dependencies; vendor integrations are connectors

Invariant #6 (dependency-light by policy) applies to what the repo *signals*,
not just what it links. A vendor integration enters as a **JSON-on-stdio
connector**, never an SDK in the tree:

- **inbound** (a mailbox, a queue, a webhook source) → the connector contract in
  `docs/triggers.md` — stdin `{trigger, connector,
  scope, cursor, max_items, config}`, stdout `{items, cursor, more}`;
- **outbound** (write to an accounting system, a CRM, a ticket tracker) → the
  host-tool contract `--tool-cmd` in `docs/run.md` — input
  JSON on stdin, result JSON on stdout, one process per effect;
- **the model leg** → `--llm-cmd` / `--embed-cmd`, per `examples/llm/`.

Three seams, one shape. The user's heavy dependencies live in *their* connector
script. An example that adds a vendor SDK to this repo is authored wrong.

## Layout for an agent example

```
examples/agents/<name>/
  README.md          what it does, what it demonstrates, how to run both paths
  trigger.sh         the `areev trigger add` declaration — the standing rule
  plan.py            the Workflow grain: nodes, edges, bindings (see docs/run.md)
  connectors/        inbound connector(s), JSON on stdio
    <source>-mock.sh   reads fixtures/, no network      (the keyless floor)
    <source>.sh        the real one, env-gated          (opt-in)
  tools/             outbound host tool(s), JSON on stdio, same mock/real pair
  fixtures/          committed sample inputs + expected outputs
  smoke.sh           the keyless end-to-end run CI invokes; exits non-zero on drift
  requirements.txt   only if unavoidable; installed in the CI job, never the workspace
```

Not every example needs every file. `smoke.sh`, `README.md`, and `fixtures/`
are mandatory — those three are what stop it from rotting.

Fixtures must be **synthetic**: no real invoice, contract, or mailbox content,
no third-party names implying an endorsement. If you commit a PDF, add
`*.pdf binary` to `.gitattributes` (every other binary type is listed
explicitly there; don't rely on `text=auto` inferring it).

## The four indexes — update in the same commit

An example nobody can find is dead weight, and these go stale silently:

1. `examples/README.md` — the top-level table
2. `examples/agents/README.md` — the agent index (tier two only)
3. `README.md` — the "## Examples" table
4. `.github/workflows/ci.yml` — the job that runs the keyless smoke

Also sweep the CLAUDE.md docs-contract table: if the example demonstrates a
surface you changed, the example *is* one of the docs that must move with it.

## CI wiring

Model the job on `python` (`ci.yml`, the `python` job, whose last step is the
Hermes provider smoke): a throwaway venv, per-example deps installed inside
the job, run the smoke. Root `examples/` is not a cargo workspace member —
the same posture as `areev-js` — so nothing leaks into the workspace build.

Add the job **when the example actually runs**. A CI job pointing at a
placeholder is worse than no job: it goes green over nothing and trains everyone
to trust it.

## Release coupling

`.claude/skills/areev-release` does not currently mention examples. If a release
changes a surface an example demonstrates, fixing the example is part of that
release commit — that is the reason the examples are in this tree at all. See
[[areev-release]].

## Checklist for a new example

- [ ] Right tier, right directory (**not** `crates/*/examples/`)
- [ ] Runs keyless against committed fixtures; live path env-gated
- [ ] Vendor integration is a stdio connector or host tool — zero new repo deps
- [ ] Fixtures synthetic and license-clean
- [ ] `smoke.sh` exits non-zero on drift
- [ ] All four indexes updated in the same commit
- [ ] Models **judgment** where a human belongs in the loop — approve one thing,
      reject another with a reason. Never a rubber-stamp path.
