# sanctions-screening — working rules

The example whose point is **where the logic lives**. Everything below
protects that point.

1. **The rule is a grain, not a file.** `src/screen.py` is the *workshop*
   copy; the deployed artifact is the CAS blob `agent.py seed` stores from
   those exact bytes, named by the `screen` Tool definition's
   `executor_uri`. If you find yourself importing screening logic into
   `agent.py`, stop — you have just moved the agent's brain outside its own
   improvement loop, which is the thing this example exists to prevent.
2. **The pin is computed from the checkout, never read from the memory.**
   `rule_address()` is `sha256` of the file on disk. That is deliberate: the
   host authorizes precisely the code its operator can see. A memory whose
   rule has moved ahead refuses to run (`RUN-E018`) instead of executing
   something the operator never reviewed.
3. **`RULE_FILE` models the operator's checkout.** Default `screen.py`;
   `improve.sh` sets `screen_v2.py` at the moment it wants to say "the
   operator synced". Do not add a flag that reads the address out of the
   memory — that would defeat rule 2 entirely.
4. **A revision is a CHAIN, not a supersession.** `revise()` walks all four
   links: new blob → supersede the Tool definition → supersede the Workflow
   (bindings name tools *by hash*, so the plan must move too, minting a new
   plan hash) → re-point the Trigger (triggers do **not** follow supersession
   heads). Skip any link and the desk goes on running the rule you thought
   you replaced. This was a real bug during authoring, and the act script
   asserts the whole chain.
5. **A refused start holds the cursor and backs off.** Two consequences.
   The pin refusal in `smoke.sh` uses `pin-check` (a direct `run_start`)
   rather than a trigger tick, because a tick would leave the desk in
   backoff for the rest of the act. And `improve.sh` waits that backoff out
   with `await-due 120`, **never a fixed sleep** — the act scripts contain
   no `sleep` at all. A guessed sleep bets on how long the previous step
   took, which holds on an idle laptop and loses in CI behind ten other
   agents (`skipped_not_due`, surfacing as a confusing "expected N runs, got
   0"). `await-due` polls the evaluator's own predicate and fails loudly by
   timeout, naming the blocked trigger and its `last_error`. See
   `../docs/testing.md`, "Determinism". Watch
   `consecutive_failures`/`last_error` on `trigger-state`: a desk refusing
   every start is safe but doing nothing.
6. **Subcommands `tools` and `connector` never open the memory.** The
   runtime spawns them while it holds the single writer. The driver writes
   grains after `run_resume` returns — which is why the false-positive
   disposition is written in `decide()`, not in the `record_disposition`
   tool.
7. **The watchlist is external data; the dispositions are memory.** The list
   rides in on the item payload (real lists come from a publisher, not from
   your memory); what an officer signed off lives in
   `org.psp.counterparties` and reaches the rule through the trigger's
   declared `context_query`. Keep that split — it is the honest one.
8. **A disposition clears a counterparty, never a list entry.** `mg:screened_clear`
   is keyed on the counterparty name. Widening it to the list id would let one
   false positive clear every future match against that entry, which is the
   failure mode that gets screening desks fined.
9. **`created_at` is pinned at `1756000000000`** on the workflow, tool and
   skill grains so the plan hash is stable. Change a seeded field and the
   hash moves; `out/workflow.hash` is what `../run-smokes.sh` compares.
10. **Fixtures are synthetic and the mojibake is load-bearing.** `06`–`09`
    carry Cyrillic homoglyphs double-encoded as UTF-8 — that is what v1
    refuses and v2 repairs, and `09` is an exact list match that was *hiding*
    behind them. Do not "fix the encoding" in the fixtures.
11. **Indexes to sweep with any change here**: `../README.md` (agent index),
    `../../README.md` (examples table), the repo README's Examples table, and
    `.github/workflows/ci.yml`. Shared how-to material lives in `../docs/`.
