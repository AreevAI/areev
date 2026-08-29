# trade-surveillance — working rules

One agent, one language stack so far. The contract that keeps it honest:

1. **The act scripts are the spec.** `smoke.sh` and `improve.sh` at this
   level are language-neutral; `python/` holds 3-line wrappers that export
   `AGENT` + `AGENT_OUT` and exec them. A behaviour change starts in the act
   scripts (assert it), then lands in every stack in the same commit. Assert
   **outcomes** — case counts, ledger rows, journal tallies — never log text.

2. **This is a teaching example of a mechanism.** It is not a compliant
   surveillance system and the README says so twice. Do not add anything
   that reads as a calibrated abuse model, a threshold with regulatory
   meaning, or a real venue/issuer/ticker. Every instrument (`MRDN:VNTG`,
   `MRDN:ORLN`, `MRDN:PDRA`), issuer, index (`Meridian 40`), desk and analyst
   is invented and must stay invented. Symbols are **venue-qualified on
   purpose** — a bare four-letter code would eventually collide with a real
   listing, and `MRDN:` cannot.

3. **The headline capability is the composite gate.** If a change makes the
   example demonstrate something else better than it demonstrates
   `members` + `predicate` + `correlate` + `window_ms`, it is the wrong
   change. The three assertions that carry it are: a single signal starts
   nothing (smoke step 4), the correlated pair starts exactly one case (step
   5), and a pair outside the window starts nothing (step 7).

4. **Never pace an act script with `sleep`.** This example was flaky
   exactly once, and it was this: `sleep 1.2` against `interval_secs: 1`
   left ~350ms of slack, which interpreter-startup jitter on a loaded
   runner ate — the tick landed `skipped_not_due: 2` and the failure read
   as "the gate didn't fire". Two structural waits replaced it and must
   stay:
   - `agent.py await-due` polls `trigger_status()` until every trigger
     reports `due` — the same predicate `trigger_run` gates on — with a
     60s bounded timeout that fails loudly. It also ends at the *earliest*
     legal moment, which is what gives the in-window pairs the tightest
     spacing the machine can manage.
   - `agent.py await-window` blocks until the evaluator's own
     `last_fired_at` is more than `window_ms + margin` in the past, so the
     near-miss gets *longer* under load, never shorter.
   `GATE_WINDOW_MS` is 15000 for headroom: two ticks must fit inside it on
   a loaded box. Raising `FEED_POLL_SECS` or lowering the window narrows
   that margin — if you touch either, run the smoke five times and once
   under `yes > /dev/null` spinners.

5. **The fixture clock is `FEED_UPTO`** (default `06`). Both feeds share ONE
   2-digit sequence — `orders/` uses 01/04/05/07, `news/` uses 02/03/06/08 —
   because the prefix is the order the desk *saw* things in, and the act
   scripts advance it one tick at a time. Prefixes within a directory must
   stay ascending: the connector slices by cursor, so inserting a
   lower-numbered fixture into a directory shifts every cursor after it.

6. **The correlation value is the run identity.** One composite run per
   correlation value, ever — a second correlated pair on the same symbol is
   a duplicate and starts nothing. That is why session two reuses MRDN:PDRA
   (whose session-one pair never correlated) instead of re-firing MRDN:VNTG. Any
   new fixture pair must use a symbol that has not opened a case yet, or
   carry an episode in the correlate key.

7. **`tools` and `connector` never open the memory.** The runtime spawns
   them while the evaluator holds the file (embedded backend = one writer).
   Grains written as a consequence of a run — the precedents, the
   `record_tool_call` rows — are written by the DRIVER after `run_resume`
   returns.

8. **The plan hash is load-bearing.** The seeder pins
   `created_at = 1756000000000` on the workflow, tool-definition and skill
   grains so every stack mints identical content addresses;
   `smoke.sh` writes it to `out/workflow.hash` and `../run-smokes.sh`
   compares. Triggers and facts are deliberately not pinned.

9. **Precedents are keyed on the pattern signature**
   (`<order_pattern>+<news_category>`), never on the instrument. That is the
   whole session-two payoff. `assemble_case` computes the signature and
   `prior_art` matches it against the precedents the trigger's declared
   context delivered — which is also why `case_ctx` recalls precedents
   *unfiltered*: the signature does not exist yet when the evaluator runs
   the query.

10. **Every case parks.** There is no edge from `analyst_review` that does
    not require a response, and there must never be one. The refusals the
    acts assert — `RUN-E012` separation of duties, a disposition with no
    written reason, an advisory loop finding, `approved → rejected` — are
    the point of the example, not decoration.

11. **The loop tuning is a recorded act.** `improve()` sets
    `loop.tool_failure/1` to `{"min_count": 2, "min_rate": 0.25}` because
    this desk judges a handful of cases a week, not hundreds. The measured
    rate on these fixtures is 33% (2 benign dismissals out of 6
    `analyst_review` tool grains); adding an `analyst_review` call without
    adding a dismissal moves that number, so re-check step 6 of
    `improve.sh` if you touch the disposition path.

12. **Keyless floor, always** (`.claude/skills/areev-examples`): no
    credential, no network, no model key in anything the act scripts touch.
    `LOOP_LLM_CMD` is read by `improve()` and must stay unset in CI.

13. **Indexes to sweep with any change here**: `../README.md` (agent index),
    `../../README.md` (examples table), the repo README's Examples table,
    and `.github/workflows/ci.yml` (`agent-example` job — `run-smokes.sh`
    discovers this agent automatically once `smoke.sh` exists, so the CI
    edit is usually nothing). Shared how-to material lives in `../docs/` —
    extend it there, don't fork it into this README.
