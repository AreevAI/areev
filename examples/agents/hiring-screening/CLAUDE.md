# hiring-screening — working rules

A high-risk-use example. The care here is not only technical: this
directory models screening people for a job, and everything about it — the
fixtures, the criteria, the prose — is a claim about what an agent may and
may not do to a candidate. Read these before changing anything.

1. **The headline is `run_oversight_report`.** This example exists to show
   EU AI Act Article 14 answered *from the run journal* rather than asserted
   in a policy document. `smoke.sh` step 8 prints it and asserts its
   content; `improve.sh` step 11 asserts it again after the desk has grown.
   If a change makes the report weaker, less measured, or optional, the
   change is wrong even when the tests still pass.
2. **There is no auto-advance path, and that is asserted structurally.**
   `smoke.sh` reads the *stored plan* (`agent.py plan`) and asserts every
   edge into `advance`/`reject` has `recruiter_review` as its source, and
   that `check_criteria` has none. Never add an edge that reaches an outcome
   without passing the client gate — not behind a flag, not "for the happy
   path". The whole example collapses if that edge exists. Rule 3 is the
   runtime half of the same invariant.
3. **The gate invariant is asserted as arithmetic, from the journal.**
   `agent.py gate-audit` pairs every completed `advance`/`reject` effect
   with the completed `recruiter_review` client effect that produced it
   (`author_did` is the responder), and both act scripts assert
   `decisions == human_reviews`, `decisions_with_no_human == []` and
   `self_reviewed == []`. It reads the **run journal**, deliberately not
   `out/decisions.jsonl`, so a bug in this example's own tool handlers
   cannot hide a bypass. Rule 2 checks the graph; this checks what actually
   happened. Keep both — a bypass that leaves the graph intact is exactly
   the failure mode this catches.
4. **`improve.sh` checks its precondition, it does not assume it.**
   `week_one_only` requires exactly five runs and none still open; anything
   else re-runs `smoke.sh` (which starts by `rm -rf`-ing `$AGENT_OUT`, so it
   is a clean reset). That makes the script idempotent — a second run, a
   run on a fresh checkout, a run against a half-finished state all pass.
   Do not replace it with a bare `[ -d "$AGENT_OUT" ]` test again.
5. **Both act scripts take an exclusive lock, and must keep it.**
   Areev's single-writer guard is *process-wide* (an in-process open-path
   registry raising `STO-E002`); it cannot see a second **OS process**
   opening the same memory file. Two act scripts on one `out/` therefore
   interleave and answer each other's asks — which presents as an empty
   review queue and runs that look as though they reached an outcome with
   nobody involved. This was observed, reproduced by racing two
   `improve.sh` runs, and fixed with an atomic `mkdir "$AGENT_OUT.lock"`
   at the top of each script. The lock sits BESIDE `$AGENT_OUT` because
   `smoke.sh` deletes `$AGENT_OUT` wholesale, and `AGENT_LOCK_HELD` makes
   it re-entrant so `improve.sh`'s act 0 can call `smoke.sh`. Keep
   `out.lock/` in `.gitignore`. If you need parallel runs, give each one
   its own `AGENT_OUT`.
6. **Never invent a protected characteristic.** The screening criteria are
   three job-related facts published on the requisition: years of
   professional backend engineering, one named certification (`CPO-2`), and
   work authorisation for the posting location. Do not add age, gender, name
   origin, photograph, school prestige, employment gaps, "culture fit", or
   any proxy for one — not as a criterion, not as a fixture field, not as a
   scoring input. And do not add a **score** or a **rank** at all:
   `smoke.sh` asserts no `score`/`rank`/`decision` key exists in the review
   queue, on purpose.
7. **"Not evidenced" is a third bucket and must stay one.** A criterion the
   application does not mention is neither met nor missed. Collapsing it
   into "missed" turns a silent CV into a silent rejection, which is exactly
   the failure this example is built to refuse (APP-2003 is the fixture that
   pins it).
8. **A parse failure is never a rejection.** `parse_application` exits
   non-zero when there is no text layer, the run fails, and no decision row
   is written. `improve.sh` asserts that none of the five unreadable
   applications ever produced an outcome. Do not "helpfully" fall back to an
   empty extraction.
9. **Fixtures are entirely fictional and stay minimal.** Invented candidate
   names, an invented certification (`CPO-2`), an invented requisition, no
   real ATS vendor, no addresses, no dates of birth, no protected data of
   any kind. Keep the CV summaries to one boring sentence.
10. **The plan hash is load-bearing.** The seeder pins
   `created_at = 1756000000000` on the workflow, tool-definition and skill
   grains so any second language stack mints the identical content address;
   `smoke.sh` writes it to `out/workflow.hash` and `../run-smokes.sh`
   compares across stacks. Changing a seeded grain field means changing it
   in *every* stack. (Facts, grants and triggers are not pinned; only the
   plan lineage is.)
11. **Subcommands `tools` and `connector` never open the memory.** The
   runtime holds the single writer while it spawns them. The `parse_application`
   handler therefore writes its audit line to `out/parse.jsonl` and the
   **driver** drains that file into `record_tool_call` grains after the tick
   returns (`drain_parse_log`, cursored on `out/parse.cursor`). Those grains
   are what `loop.tool_failure/1` clusters — if the drain breaks, `improve.sh`
   step 7 fails.
12. **The loop assertions are analyzer-selected, not index-selected.**
   `improve.sh` picks the recommendation whose analyzer is
   `loop.tool_failure/1` and whose summary names `parse_application`.
   `loop.run_outcome/1` fires or does not depending on the failure ratio at
   that moment (5/11 = 45%, just under its 0.5 default) — never assert on
   `pending[0]`.
13. **Only the gates that actually exist may be asserted.** Builtin
    analyzers record no human co-creator, so the loop's *self-approval*
    block does **not** fire for a deterministic finding — do not claim it
    does. What is real here and asserted: the pass applies nothing on its
    own (`auto_applied == 0`), a blank `BECAUSE` is refused (`LOP-E011`),
    and a second approval is refused (`LOP-E020`). `govern` resolves
    recommendation prefixes across `{"status": "all"}` precisely so a
    lifecycle violation surfaces as one instead of as "not found".
14. **The budget ceilings in the code and in the assertions are one set of
    numbers.** `MAX_TOKENS` / `MAX_USD_MICROS` / `MAX_WALL_MS` /
    `ASK_TTL_SEC` in `agent.py` are quoted literally by both act scripts and
    by `README.md`. Change one, change four places. `max_storage_bytes` is
    **not** reachable from the Python binding (it is pinned to `None` in
    `run_options`) — do not add an assertion that pretends otherwise.
15. **Grants are how the report knows the approvers.** `GRANT run.respond ON
    "org.talent" TO "user:mo"` — note the namespace must be a **quoted
    string**: the CAL grammar takes a bare ident there, and `org.talent`
    lexes as `org` `.` `talent`. `user:coordinator` is granted `run.cancel`
    and deliberately never `run.respond`; the asymmetry is asserted.
16. **Namespaces**: ops grains (plan, tools, triggers, run journals) live in
    `org.talent`; the requisition's criteria and the recorded precedents in
    `org.talent.reqs`. Reads use the `"org.talent.*"` prefix; writes take
    the exact namespace. Never put an anonymize policy on the ops lane.
17. **The fixture clock is `APPS_UPTO`** (default `05`): the mock connector
    serves only files whose 2-digit prefix is ≤ it. Week one is `01–05`,
    week two `06–11`. A new fixture's prefix decides which act it lands in,
    and the run/decision counts in both act scripts are exact.
18. **Decision fixtures carry markers.** `marker` is
    `sha256(application_id)[:12]`. Change an `application_id` and you must
    recompute every marker that targets it (and the literal in `smoke.sh`
    step 6's `stop`).
19. **Regulatory claims are load-bearing and get checked.** The README's
    "What the law actually asks for" table is deliberately narrow and
    corrects the common misreading. Do not soften these: GDPR **Art. 22** is
    the only genuine ex-ante human-decision gate in hiring; **NYC LL144 does
    not require human review** (6 RCNY § 5-304(a) — "Nothing in this
    subchapter requires an employer... to provide an alternative selection
    process"), it requires bias audit + notice; the EU AI Act is
    **Regulation (EU) 2026/1744** (OJ 24 July 2026, in force 27 July) and
    per the Commission's Art. 6(5) guidelines (19 May 2026) a human in the
    loop does **not** change the high-risk classification; the **EEOC's 2023
    Title VII AI guidance and 2022 ADA guidance were withdrawn after
    E.O. 14179 and now 404** — never cite them, while Title VII and the
    Uniform Guidelines (29 C.F.R. Part 1607, four-fifths rule § 1607.4)
    remain in force; retention is **4 years** for California FEHA ADS
    records (vendors included) and **3 years** for Colorado ADMT. And keep
    the standing honesty note: this example demonstrates oversight and
    record-keeping, never bias auditing.
20. **Indexes to sweep with any change here**: `../README.md` (the agent
    index). `run-smokes.sh` discovers this agent automatically once
    `smoke.sh` exists, so there is usually no CI edit. Shared how-to
    material lives in `../docs/` — extend it there, don't fork it into this
    README.
