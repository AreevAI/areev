# rcm-optimization — working rules

This example exists for ONE mechanic: **`$send` fan-out with declared
reducers** — dynamic width the plan did not enumerate. Everything else here
is scaffolding around that. If a change makes the fan-out less legible, it
is the wrong change.

1. **The act scripts are the spec.** `smoke.sh` and `improve.sh` are
   language-neutral; a language dir holds a 3-line wrapper that exports
   `AGENT` + `AGENT_OUT` and execs them. A behavior change starts in the
   act scripts (assert it), then lands in every stack in the same commit.
   Assert outcomes, never log text.

2. **Reducers are STRING values** — `lww`, `append`, `sum`, `max`, `min` —
   and `reducers` is an **untyped passthrough** on the Workflow grain. An
   object, a typo, a nested shape: all store cleanly, mint a content
   address, and replicate, then refuse at **every future run start** with
   `RUN-E019`. `smoke.sh` step 1 asserts the table AS STORED and step 2
   proves the refusal on purpose (`agent.py reducer-check`). Never delete
   either assertion — they are the only thing standing between a typo and
   a plan that silently stops merging.

3. **A `$send` target must be a Host-executed node** (v1). Not a client
   gate, not a subgraph, not an abstract node. The human gate lives in the
   PARENT plan, downstream of the join — `lead_review` here.

4. **A spawned task's input is exactly what the spawn decision named**, not
   the merged run state. `split_denials` is therefore the only node that
   sees the whole remittance, and it is where the approved mappings are
   packed into each task. A static node's input is the whole context; the
   asymmetry is deliberate, and code that assumes otherwise breaks silently
   (the key is just absent).

5. **The fan-out edge is `split_denials -> classify_denial`, and it stays.**
   The spawn preempts the target's *static* activation, so the classifier
   runs N times and not N+1, and `classify_denial -> cluster` does not fire
   until the batch drains. Removing the edge does not remove the node from
   the graph — it makes it unreachable (`RUN-E003`).

6. **An empty `$send` list is not exercised and not supported here.** With
   zero spawns the target's static activation is *not* preempted and it
   runs once against the whole context. Every remittance fixture has at
   least one denial; keep it that way, or add the guard and the assertion
   together.

7. **`file_report` is the only terminal.** Every other node has an
   out-edge; a plan whose every node has one stalls (`RUN-E001`). The three
   paths — no proposal, rejected, approved — all resolve into it, and the
   AND-join is what makes that legal: `file_report`'s in-edges must all
   *resolve*, with at least one fired.

8. **Telemetry is recorded under `denial_root_cause` / `denial_fix`, NOT
   under the node names.** The run journal already writes an execution Tool
   grain per dispatch under the node's own tool name, and the loop's rate
   gate divides a failure cluster by that tool's opportunities — telemetry
   sharing a name with a node is diluted by the journal's own volume and
   quietly stops firing. Distinct names, distinct denominators.

9. **Loop signatures must be DIGIT-FREE.** `normalize_signature` collapses
   digit runs to `#`, so `DN-517` and `DN-622` would cluster together and
   the desk would learn the wrong thing. Signatures here are
   `"unmapped <payer-slug> <denial text>"` / `"mapped <payer-slug>
   <root_cause>"`, capped under 80 chars (the analyzer truncates there).
   Keep any new signature short, digit-free and normalized.

10. **The loop gate this example asserts is the auto-apply ceiling, not
    `apply`.** `loop.tool_failure`'s manifest is `auto_apply: Never`, so
    `improve --grant-auto-apply` hands the engine a host policy granting
    the family auto-apply and it still applies nothing. A *human* `apply`
    is allowed and expected — do not assert that it refuses. Note also that
    `apply_recommendation` fuses approve+apply: calling `approve` and then
    `apply` is `LOP-E020` (`approved -> approved`). Use one or the other.

11. **Subcommands `tools` and `connector` never open the memory.** The
    runtime spawns them while the evaluator holds the file (embedded
    backend = one writer). They write JSONL; the DRIVER turns those rows
    into grains afterwards, behind `out/telemetry.cursor` so a re-run does
    not double-count.

12. **`created_at` is pinned** (`EPOCH_MS = 1756000000000`) on the
    workflow, tool-definition and skill grains so every stack mints one
    plan hash; `smoke.sh` writes it to `out/workflow.hash` and
    `../run-smokes.sh` compares across stacks. Change a seeded field in one
    language and you must change it in all of them.

13. **The fixture clock is `REMIT_UPTO`** (default `02`): the feed serves
    remittance files whose 2-digit prefix is ≤ it. Week one is `01–02`,
    week two is `03–04`. A new fixture's prefix decides which act it
    arrives in, and both payers share the clock.

14. **Decision fixtures carry markers.** `marker` is
    `sha256(remit_id)[:12]`. Change a remittance's `remit_id` and you must
    recompute the marker in every decision that targets it.

15. **Namespaces:** ops grains (plan, tools, trigger, journals) in
    `org.rcm`; the desk's thresholds in `org.rcm.policy`; the mappings a
    lead approved in `org.rcm.denials`. Writes use the exact namespace;
    reads use the `"org.rcm.*"` prefix. `min_cluster_size` is a Fact, not a
    constant — that is what lets the loop propose moving it.

16. **Fixtures are synthetic and must stay that way.** The denial codes are
    invented and are NOT CARC/RARC; the payers, claim ids, CPT-shaped
    strings and `patient_ref`s are fictional. **No PHI, ever** — not a
    name, not a date of birth, not a member id. The README says so out
    loud; keep that paragraph accurate.

17. **Indexes to sweep with any change here**: `../README.md` (the agent
    index), `../../README.md` (the examples table), the repo README's
    Examples table, and `.github/workflows/ci.yml` (`agent-example` job —
    usually nothing, since `run-smokes.sh` discovers this directory once
    `smoke.sh` exists). Shared how-to material lives in `../docs/`.
