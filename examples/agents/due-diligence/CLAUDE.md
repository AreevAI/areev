# due-diligence — working rules

The example that exists to show **a budget as a control, not an error**, plus
the three replay/audit verbs. Everything below is load-bearing; breaking one
of these silently turns the example into a lie.

1. **The act scripts are the spec.** `smoke.sh` and `improve.sh` at this
   level are language-neutral and hold every assertion; each language dir is
   a three-line wrapper exporting `AGENT` + `AGENT_OUT`. A behavior change
   starts by asserting it in the act script, then lands in every agent
   implementation in the same commit. Assert **outcomes**, never log text.
2. **`finished` is a Rust `Debug` string.** `BudgetExhausted { axis: WallMs }`,
   `Failed { node: "research", detail: "…" }`. Match a SUBSTRING. An equality
   check against it will break the first time the enum's `Debug` formatting
   moves, and nothing about that formatting is a stability promise.
3. **The wall-clock arithmetic is deliberate — do not retune it casually.**
   The scheduler charges wall time per superstep and checks every budget axis
   in a pre-flight *before* opening the next one. With a per-leg cost `L`
   and a per-superstep spawn overhead `s`, the run stops after exactly two
   legs when `4s + L < ceiling ≤ 5s + 2L`. At `L = 1500 ms` (`DD_LEG_MS`) and
   `ceiling = 2900 ms` (`DD_CEILING_MS`) that holds for any `s` under
   ~350 ms, which is the margin that keeps this deterministic on a loaded CI
   box. Change either constant and redo that arithmetic, or act two's
   like-for-like comparison stops being like-for-like.
4. **The two compared targets must keep the same yield shape.** Act two's
   claim is "the same ceiling, three times the material findings". It only
   means anything because `TGT-4401` and `TGT-4402` have identical per-leg
   material counts (media 0, filings 1, financials 2, litigation 1) in the
   same sector. The comparison is strict for any prefix length 1–3; only a
   full four-leg read would tie, and the ceiling makes that unreachable.
   Change one target's counts and you must change the other's.
5. **Memory only ever DEMOTES a leg.** `mg:low_yield_leg` moves a leg to the
   back of the queue; it never removes it. A leg that stops being read is a
   leg nobody signed off on not looking at — and the fork still reads it once
   the ceiling comes off, which `improve.sh` asserts.
6. **Run state comes from the JOURNAL, not from `findings.jsonl`.**
   `legs_of` reads the run's last checkpoint (a State grain whose `context`
   holds the serialized scheduler state) via `run_grains`. Deriving it from
   the effects ledger instead couples every assertion to file-append
   attribution across a fork, and it broke exactly that way once: a run
   whose context clearly held four legs reported three, because one leg's
   ledger rows were attributed elsewhere. The ledger is the EFFECTS record
   and nothing else — which is what makes the `run_shadow` line-count
   assertion mean something. Do not reintroduce the coupling.
7. **The fork parks; it does not complete.** The partner gate is mandatory,
   so `resume` on the fork ends `parked`, not `Completed`. Act one asserts
   both halves: the fork's legs strictly contain the exhausted run's (it
   CONTINUED the work), and the run only reaches `Completed` after the
   partner answers. An assertion that the fork completes on its own is
   wrong about the plan, not about the runtime.
8. **A held lock is waited on.** `open_db` retries `STO-E001`/`STO-E002` for
   ~10s. One memory is one writer, and a subcommand starting while the
   previous one's handle is still being torn down is a load-dependent flake,
   not a contract violation.
9. **Both acts are idempotent, and that is tested.** `smoke.sh` wipes `out/`;
   `improve.sh` re-runs act one when it sees act two already ran there.
   Verify with: smoke, improve, improve, smoke — all four exit 0.
10. **The host passes `--run <id>` to the tool seam.** A fork inherits the
   base run's context verbatim, so anything the driver stamped into the run
   input (a run id included) is *stale* in the fork. The run id therefore
   travels on the tool command line, not in the state. Keep it that way.
11. **`tools` never opens the memory.** The runtime holds the single writer
   while it spawns them. The driver writes grains after the run returns.
   Same reason `book` opens ONE handle for a whole batch: a second handle on
   one file fails at open (`STO-E002`) by design.
12. **`created_at` is pinned to `1756000000000`** on the workflow, tool
   definition and skill grains so the plan hash is stable across
   implementations. `smoke.sh` writes it to `out/workflow.hash`. Facts and
   observations are not pinned — only the plan lineage.
13. **Both refusals in `sign` are real, and in this order:** the desk refuses
   an empty `because` before it asks the runtime anything; the runtime
   refuses the triggering principal (`RUN-E012`) because every client ask is
   an approval boundary. Note that a FORK's triggering principal is the
   forker — that is what makes "the analyst who raised the ceiling cannot
   sign the result off" true rather than conventional.
14. **`sign` responds, resumes, and only THEN writes the partner's note.**
    Writing memory between `run_respond` and `run_resume` leaves the ask
    settled and the run un-resumed if the write throws. Ordering, not taste.
15. **`adopt` refuses a finding that is not `approved`.** The desk rule and
    the reason for it are one record. `recommendations()` defaults to
    pending-only — pass `{"status": "all"}` when resolving a hash, or an
    approved finding vanishes from the lookup.
16. **Do not dramatize crash recovery.** A hard-crashed run is held by a
    hardcoded 10-minute lease (`RUN-E021`) with no override. A *parked* run
    releases its lease deliberately, which is why HITL resumes instantly.
    The README states the lease as an operational fact; there is no act
    built on it, and there should not be.
17. **Keyless floor, always.** No credential, no network, no model key in
    anything the act scripts touch. Every company, person, court,
    publication and reference number in `fixtures/` is fictional; keep it
    that way — this is diligence material, and a real name in a synthetic
    adverse-media snippet is a defamation-shaped problem, not a style one.
18. **Namespaces:** ops grains (plan, tool definitions, run journals) in
    `org.ops`; the desk's standing rules in `org.diligence`; what partners
    taught it in `org.diligence.learned`. Reads use the `org.diligence.*`
    prefix; writes and policy take the exact namespace.
19. **Indexes to sweep with any change here**: `../README.md` (agent index),
    `../../README.md` (examples table), the repo README's Examples table,
    and `.github/workflows/ci.yml` (the keyless smoke job). Shared how-to
    material lives in `../docs/` — extend it there rather than forking it
    into this README.
