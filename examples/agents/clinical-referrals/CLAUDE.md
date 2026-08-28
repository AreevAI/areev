# clinical-referrals — working rules

One agent, one Python stack, two act scripts. The contract that keeps it
honest:

1. **The act scripts are the spec.** `smoke.sh` and `improve.sh` at this
   level are language-neutral and hold every assertion; `python/*.sh` are
   3-line wrappers that export `AGENT` + `AGENT_OUT` and exec them. A
   behavior change starts in the act scripts (assert it), then lands in
   `python/agent.py` in the same commit. If a second stack is added, it must
   mint the identical plan hash — `../run-smokes.sh` compares
   `out/workflow.hash` across stacks.
2. **The headline is pseudonymization on egress, and nothing may dilute it.**
   `set_anon_policy("org.clinic.referrals", {"mode":"egress","scope":"session"})`
   is declared once in `seed()`. **No tool in this example may anonymize
   anything itself.** The whole demonstration is that the tools contain no
   privacy logic and the store rewrote the text at the read exit. If you find
   yourself calling `anonymize_text` inside `tool_main`, the example is
   broken, not the store.
3. **Namespace policy is exact, and the operational ones never get one.**
   `org.clinic.referrals` has the policy. `org.clinic.protocol` (the desk's
   own rules, read back as INPUT) and `org.ops` (plan, tools, trigger,
   journal) have none, ever. `policy-drill` exists to demonstrate the hazard
   and `smoke.sh` step 10 asserts it — a fact's `subject` is an identity
   field by construction, so a rewriter turns every rule into `[PERSON_n]`
   and the desk stops finding its own protocol. Namespace **prefixes**
   (`"org.clinic.*"`) work on reads only; policy, writes and erasure take
   exact namespaces.
4. **The valid modes are `off` / `egress` / `ingress` / `both` / `audit`.**
   There is no `"rewrite"` mode. `invoice-to-accounting` uses `audit`
   (measure, change nothing); this one is the `egress` example. Don't
   introduce a third posture without a reason and a README paragraph.
5. **The wire log is the centrepiece assertion.** `out/egress.jsonl` is the
   verbatim exchange with `agent.py service`. `smoke.sh` step 5 walks *every
   fixture that has actually gone out* and checks its patient name, DOB, MRN,
   phone, email and referring GP against those bytes — derived from the
   fixtures, never hardcoded, so a new fixture strengthens the check instead
   of slipping past it. Never weaken this into a spot check.
6. **The honesty case is load-bearing, not a bug.** `improve.sh` step 9
   asserts that `Anneke Vos` **IS** in the wire log. Tier-0 has no name
   model: it catches shapes (date/phone/email/MRN/IBAN/card/secret) and
   identities the memory holds as grain **subjects**. A relative named once
   in prose is neither. If you "fix" that fixture, you delete the reason
   step 10–12 exist.
7. **The fixture clock is `REF_UPTO`** (default `03`): the mock connector and
   `intake` both serve only fixtures whose 2-digit prefix is ≤ it. Week one
   is `01–03`, week two is `04–09`. A new fixture's prefix decides which act
   it arrives in, and both readers share the clock.
8. **Review fixtures carry markers.** `marker` is `sha256(referral_id)[:12]`.
   Change a fixture's `referral_id` and you must recompute the marker in
   every review that targets it.
9. **The detector chain is a FILE truth; the detector is a HOST capability.**
   `detectors: ["tier0","ner"]` lives in the policy and replicates.
   `set_anonymizer_command` is per-process and never persisted, gated here by
   `CLINIC_NER`. A host that cannot honour the policy fails the read closed
   (`VAL-E001`) — keep it that way; serving raw would be the wrong failure.
   Same posture as `set_anonymize_egress_floor`, which is a restrictive cap
   that covers policy-less namespaces and is forgotten on reopen.
10. **Subcommands `tools`, `connector`, `service` and `ner` never open the
    memory.** The runtime (or the store, for `ner`) holds the single writer
    while they run. The driver writes grains after the run returns.
    `agent.py service` additionally stands OUTSIDE the trust boundary by
    construction: it must never read `fixtures/`, and reviewers should be
    able to see that from the file.
11. **Pin `created_at = 1756000000000`** on the workflow, tool-definition and
    skill grains so the plan hash is stable. Facts, events and triggers are
    not pinned; only the plan lineage. `smoke.sh` writes the hash to
    `out/workflow.hash`.
12. **Keyless floor, always** (`.claude/skills/areev-examples`): no
    credential, no network, no model key in anything the act scripts touch.
    The Tier-1 detector stand-in is a regex on purpose. Fixtures are
    synthetic — invented patients and clinicians, reserved `555-01xx` phone
    numbers, `example.com` emails, MRNs matching no real scheme. Nothing that
    could be mistaken for real PHI.
13. **Model judgment, never a rubber stamp.** Three structural refusals are
    asserted (self-signature `RUN-E012`, the loop applying its own advice, a
    decision with no reason) plus one fail-closed read (`VAL-E001`). Every
    clinician decision and every governance decision carries a written
    reason, and one of them is a *correction* that becomes a rule.
14. **The README must keep the legal caveat.** Pseudonymization is not
    anonymisation: reversible pseudonymized data is still personal data
    (GDPR Recital 26 / Art. 4(5)), the practice name is a quasi-identifier
    left in deliberately, and Tier-0 is a floor. If a change makes any of
    those less true, the README section moves with it.
15. **`improve.sh` reuses `out/` from `smoke.sh`** and hardens the policy at
    the end, so it is not idempotent — re-run `smoke.sh` first (it does
    `rm -rf "$AGENT_OUT"`). Never call `improve` (the loop) after `harden`
    without `CLINIC_NER=1`: the loop reads broadly and would fail closed.
16. **Indexes to sweep with any change here**: `../README.md` (agent index),
    `../../README.md` (examples table), the repo README's Examples table, and
    `.github/workflows/ci.yml` (`agent-example` job — usually nothing,
    `run-smokes.sh` discovers this directory automatically). Shared how-to
    material lives in `../docs/` — extend it there, don't fork it here.
