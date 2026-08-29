# incident-response — working rules

An on-call desk woken by **webhook deliveries**, not by polling. The contract
that keeps it honest:

1. **The act scripts are the spec.** `smoke.sh` and `improve.sh` at this level
   are language-neutral; `python/` holds 3-line wrappers that export `AGENT` +
   `AGENT_OUT` and exec them. A behavior change starts in the act scripts
   (assert it), then lands in every stack in the same commit. Assert
   **outcomes** — ledger rows, run outcomes, report counts — never log text.
2. **The plan hash is load-bearing.** The seeder pins
   `created_at = 1756000000000` on the workflow, tool-definition and skill
   grains so any second stack mints the identical content address
   (`run-smokes.sh` compares `out/workflow.hash` across stacks). Facts,
   triggers and journals are deliberately not pinned. Change a seeded grain
   field in one language and you must change it in all of them.
3. **Keyless floor, always** (`.claude/skills/areev-examples`): no credential,
   no network, no model key in anything the act scripts touch. Every service,
   engineer, monitoring vendor and identifier is fictional — `beacon`,
   `checkout-api`, `ledger-sync`, `notify-worker`, `user:rhea`, `user:tobin`,
   `user:imara`, and every URL is `.invalid`.
4. **`listen` is the only fake host.** It replays `fixtures/alerts/` in place
   of an HTTP endpoint; in production that subcommand is a request handler
   calling `trigger_deliver` with the raw body. Nothing else about the example
   changes when you swap it. **Areev never opens a port** — do not add one.
5. **The fixture clock is `ALERT_UPTO`** (default `03`): `listen` delivers
   only alert files whose 2-digit prefix is ≤ it. Week-one fixtures are
   `01–03`, week-two `04–08`. A new fixture's prefix decides which act it
   arrives in, and adding one changes the loop's failure ratio (see rule 8).
6. **Deliveries are spaced by ~2 ms in `listen`.** Two firings whose journal
   Observations are byte-identical *and* land in the same millisecond collide
   on their content address (`STO-E001: UNIQUE constraint failed: grains.hash`),
   which is reachable only from a tight batch replay — consecutive
   `duplicates: 1` deliveries. A listener handling one request at a time never
   hits it. Do not remove the spacing without re-testing a batch replay.
7. **Decisions are matched by `(alert_id, channel)`, not by a hash marker.**
   `channel` comes from the trigger's `scope` (`beacon:alerts` → `webhook`,
   `oncall:replay` → `replay`) and is computed in `classify`, so a fixture
   never carries a hand-computed digest. One alert can have two live runs —
   one per standing rule — and that pair is exactly what step 5 of `smoke.sh`
   asserts.
8. **The loop's arithmetic is a fixture invariant.** `improve.sh` needs
   `loop.run_outcome/1` to fire at `min_failure_ratio: 0.3`: nine terminal
   runs, three failed (33%). Adding an alert that completes, or removing a
   ledger-sync failure, silently drops it under the floor and the act fails at
   step 5. Count before you add a fixture.
9. **Subcommand `tools` never opens the memory.** The runtime spawns it while
   the evaluator holds the file (embedded backend = one writer). Driver
   subcommands open per-invocation. Facts learned from a decision are written
   by the **driver**, after `run_resume` returns — and only when the run
   finished `Completed`, because a failed remediation has taught the desk
   nothing about the incident yet.
10. **T18: `close` is the only terminal** — the one node with no out-edges. It
    is a join below both `apply_remediation` and `record_only`; a node whose
    in-edges all resolve with none fired dies, and its out-edges propagate
    death, so the untaken branch resolves rather than pending forever. A plan
    whose every node has an out-edge never completes, it stalls (`RUN-E001`).
11. **Namespace rules the seeds encode:** ops grains (plan, tool definitions,
    triggers, run journals) live in `org.ops` and that namespace never gets an
    anonymize policy; the desk's own rules live in `org.sre` and the service
    catalog plus learned causes in `org.sre.services` (exact ns on writes).
    Reads use the `"org.sre.*"` prefix.
12. **Never remove the human from the production-action path.** The memory
    payoff is a *better proposal at the same gate*. If a change makes an
    approved remediation apply itself because the desk has seen the alert
    before, the change is wrong regardless of what the tests say.
13. **Indexes to sweep with any change here**: `../README.md` (agent index),
    `../../README.md` (examples table), the repo README's Examples table, and
    `.github/workflows/ci.yml` (`agent-example` job — `run-smokes.sh`
    discovers this agent automatically once `smoke.sh` exists, so that edit is
    usually nothing). Shared how-to material lives in `../docs/` — extend it
    there, don't fork it into this README.
