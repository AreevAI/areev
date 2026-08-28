# data-subject-requests — working rules

A privacy desk: DSAR intake, disclosure, portability and erasure under a DPO
gate. The rules that keep it honest, in the order they will bite you:

1. **The act scripts are the spec.** `smoke.sh` and `improve.sh` at this
   level are language-neutral; `python/` holds 3-line wrappers that export
   `AGENT` + `AGENT_OUT` and exec them. A behavior change starts in the act
   scripts (assert it), then lands in every stack under this directory in
   the same commit. Assert outcomes, never log text.

2. **Report count == erasure count, or stop.** `execute_orders` takes a
   fresh `subject_report` per namespace immediately before `forget_subject`
   and raises if the two disagree. That check is the reason this example
   exists (REQ-ERASE-9 — one selector, two modes). Never "optimize" it away
   by trusting the count measured at intake: intake is a snapshot, and the
   consent withdrawal lands between the two.

3. **Nothing that names a data subject may be written into a grain the
   erasure cannot reach.** Concretely: `run_start`'s `input_json` gets the
   REDACTED intake record (case reference, type, verification outcome,
   fingerprint, per-namespace counts) — never the sender, the name or the
   request body, because run-journal grains live in `agent:harness` and no
   namespace-scoped erasure will ever touch them. Same rule for the
   certificate (`certify`) and for `record_tool_call` result strings, which
   are cluster keys: `unresolved-identity`, never `unresolved-<email>`.
   `agent.py trace` asserts this; keep it in the acts.

4. **`telemetry="off"` in `open_db` is load-bearing, not a tuning knob.**
   The recall sidecar logs query text, and this desk searches for people it
   is about to erase. Turn it on and `loop.coverage_gap` will propose
   *"recurring question with no matching memory: <erased name>"* — writing
   the identity back in as a recommendation grain. `smoke.sh` asserts no
   `*.telemetry.db` exists.

5. **Destruction takes an exact namespace and a non-empty subject.** Reads
   use `"org.*"`; writes, erasure, DSAR reads and retention sweeps take one
   exact namespace and refuse a pattern with `VAL-E001`. `agent.py guards`
   proves all five refusals, and the deliberately mis-declared `org.*`
   retention rule in `fixtures/seed/subjects.json` proves the same thing on
   the declarative path — do not "fix" that fixture, it is the test.

6. **A Consent grain must carry `subject` as well as `subject_did`.** The
   DSAR selector matches DICTIONARY-INDEXED references, and `subject` is the
   indexed position. A consent grain naming the person only in `subject_did`
   is invisible to both the report and the erasure. (`docs/gdpr.md` §6
   recalls on `subject` too.) Same trap for any grain type you add here:
   check that `subject_report` sees it before you believe it is in scope.

7. **`tools` never opens the memory.** The runtime holds the single writer
   while it spawns host tools, so the `erase` and `disclose_only` nodes
   *order* their acts into `out/orders.jsonl`; the driver executes them
   after the run returns. This is also the correct governance: an
   irreversible act is performed by the party holding the memory, after a
   named human approved it — not by a subprocess.

8. **T18 — `close` is the only node with no out-edge.** Every path ends
   there, including `build_report -> close` when nothing is on file. Adding
   an out-edge to `close` turns every run into a Stall (`RUN-E001`).

9. **Two refusal grounds, kept apart.** `identify_subject` fails when the
   requester was not verified AND when the claim did not resolve to exactly
   one subject. Week three fixes *resolution* only, and `improve.sh` asserts
   the unverified request is still refused. Never let a resolution rule
   double as a verification rule.

10. **Policy is data.** The processing register, the retention rules, the
    response deadline and the identity-resolution rules are Facts in
    `org.privacy`, read back through `declared()`. That is what makes
    `agent.py teach` a real change of behaviour. `add_fact` heads are keyed
    on `(subject, relation)`, so two values for one rule need two relations
    (`mg:resolve_did`, `mg:resolve_contact_email`) — not two facts on one
    relation, which supersede each other silently.

11. **The fixture clock is `REQ_UPTO`** (default `04`): the intake only
    reads request files whose 2-digit prefix is ≤ it. Week one is `01–04`,
    week two `05–08`. `age_days` in `fixtures/seed/subjects.json` is the
    other clock — stamped relative to `now` at seed time so the retention
    sweep's arithmetic never drifts with the calendar. Changing an
    `age_days` past/under 365 changes the sweep count `improve.sh` asserts.

12. **Keyless floor, always** (`.claude/skills/areev-examples`): no
    credentials, no network, no model key, committed synthetic fixtures.
    Every person, address, case reference and DID is fictional
    (`did:example:…`, `*.example.test`). A real name in a fixture would be
    the one bug this example cannot afford.

13. **Pinned `created_at`** (`EPOCH_MS = 1756000000000`) on the workflow,
    tool-definition and skill grains keeps the plan hash stable across
    stacks; `smoke.sh` writes it to `out/workflow.hash`. Personal-data
    grains are deliberately NOT pinned — they are stamped relative to now,
    per rule 11.

14. **Indexes to sweep with any change here**: `../README.md` (agent
    index), `../../README.md` (examples table), the repo README's Examples
    table, and `.github/workflows/ci.yml` (the keyless smoke job). Shared
    how-to material lives in `../docs/` — extend it there, don't fork it
    into this README.
