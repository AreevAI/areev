# Areev Loop — governed self-improvement for AI agents

Areev Loop turns an agent's own history into **recommendations** — evidence-cited,
reviewable, undoable, measured — and governs every change to the agent's
memory through four gates. The core is **deterministic**: it produces useful
recommendations with **zero model calls** by computing over Areev's typed
grains, never over raw prose.

Areev Loop ships inside Areev: the `areev loop` verb family, the `areev.*`
binding methods, two MCP tools, the `/api/loop/*` HTTP routes, and an Areev Loop
tab in `areev ui`. It is not a separate install.

- Design & rationale: [`loop-proposal.md`](loop-proposal.md)
- Analyzer precision numbers: `crates/areev-bench/RESULTS.md`
- Trust model: [`security-model.md`](security-model.md)

## The 60-second proof (no agent, no LLM, no waiting)

The fastest way to see the loop is a REPL and ~15 lines — five failing tool
calls and a couple of contradictory facts light up the analyzers
deterministically:

```python
import areev, json

db = areev.Areev("proof.db", actor="user:me")   # actor labels the audit chain

# tool-failure clustering: 5 failures + 2 successes for one tool
for _ in range(5): db.record_tool_call("stripe_refund", '{"error":"rate_limited"}', is_error=True)
for _ in range(2): db.record_tool_call("stripe_refund", '{"ok":true}', is_error=False)

# contradiction sweep: two live values under a functional relation
db.add_fact("acme", "deploy_target", "us-east-1", 0.9)
db.add_fact("acme", "deploy_target", "eu-west-1", 0.9)

health = db.loop_run()                             # explicit call: never gated
for rec in json.loads(db.recommendations('{"status":"pending"}')):
    print(rec["severity"], rec["summary"])

# review with judgment — never rubber-stamp
pending = json.loads(db.recommendations('{"status":"pending"}'))
db.apply_recommendation(pending[0]["hash"], because="rate-limit retries belong in the client")
db.dismiss_recommendation(pending[1]["hash"], "those were one expired key")
```

Or from a fresh install with the CLI, using the seeded demo corpus:

```bash
areev init --db demo.db --template demo    # plants dupes, a contradiction, a stale grain
areev loop run --db demo.db              # ~3 recommendations across analyzers
# code changes are their own class: a code_revision pins the evalset it was
# gated against (Rule E1) and applies ONLY with the recorded gate run —
#   areev eval run --evalset <hash> --tool-cmd '…'      # records the edge
#   areev loop apply <rec> --because "…" --gating-run <eval-run-id>
#   areev tool provenance <code-hash>                   # the whole chain
areev loop list --db demo.db
areev ui --db demo.db --token-env AREEV_TOKEN   # the Areev Loop tab shows the queue
```

## The loop

```
capture  (tool calls, facts, events)        — record_tool_call / add / import
  → analyze   (deterministic, typed)         — thirteen analyzers over grain semantics
  → recommend (recommendation + evidence)    — dedup'd, template-rendered, cited
  → govern    (review / policy auto-apply)   — four gates, hash-chained audit
  → apply     (undoable supersession)        — scope-checked at execution
  → measure   (outcome review)               — re-run the metric, revert on regression
```

The loop closes **without requiring an LLM** — a floor that always runs, not
a ceiling, and not a boast: deterministic signature lessons are also what
measured strongest on the self-improvement benchmark
([RESULTS.md](../crates/areev-bench/RESULTS.md#areev-loop-self-improvement--the-abab-causal-proof)),
and `--llm-cmd` adds verified reflection on top where judgement needs
language. Whichever runs, the guarantees are the same: every recommendation
cites the grains it was computed from; every apply stores its inverse (or is
marked non-rollbackable up front); every decision carries a written reason.

## The four gates

1. **Propose** — only recommendation objects enter the queue, each carrying a
   versioned analyzer id + params, a deterministic template-rendered summary,
   bounded evidence hashes, a severity, and (where applicable) a reproducible
   metric snapshot. Analyzers cannot emit free prose.
2. **Review** — separation of duties (`write` grants neither `review` nor
   `apply`); a **mandatory reason** (BECAUSE) on every decision; self-approval
   is blocked against the recommendation's creating actor — and, for LLM and
   external-command findings, against the principal that triggered the run
   that authored them (an LLM draft is authored *via* its trigger; a
   deterministic finding is computed, so the engine stays its only creator).
3. **Apply** — requires the `apply` scope; destructive applies additionally
   require `admin` + `allow_destructive`; every apply records its inverse.
   Advisory Edit/Data findings have no executable engine primitive: make the
   change in the host, then dismiss the recommendation to close its lifecycle.
4. **Verify** — outcome review re-runs the stored metric after `review_after`
   and proposes a revert on regression.

### Definition rewrites (`DEFINE QUERY` / `DEFINE TEMPLATE`)

A proposal may rewrite a saved query or template — which is where a
self-improving agent's *prompt-assembly* lives, so without it the loop could
evolve an agent's memories but never the CAL that turns them into a prompt.
Two rules make it safe, both enforced in the engine rather than by convention:

- **Never auto-applied, regardless of policy.** The `query` target class is
  excluded from `grants_auto_apply` by name, exactly as `code` and `evalset`
  are. A grain edit changes one remembered value; a definition rewrite changes
  what **every future context** contains, so it always requires a human
  `APPROVE` + `APPLY` with `BECAUSE`.
- **The inverse is recorded at apply, or the apply is refused.** A `DEFINE`
  writes a `qry:`/`tpl:` registry row, not a grain, so the ordinary rollback
  (retract `created_hashes`) would undo nothing while reporting success. The
  substrate supplies `definition_inverse` — the statement restoring the
  previous definition, or a `DROP` when there was none — and `ROLLBACK` runs
  it. A substrate that cannot produce one (the default) refuses the apply:
  a definition change `ROLLBACK` could not undo must not be applied at all.
  Built-in definitions are immutable and therefore yield no inverse, so a
  proposal to redefine one is refused.

`DROP` is never proposable, and saved-query bodies keep their read-only
verification pass, so a definition rewrite can no more smuggle in a write than
a hand-written one can.

Definition rewrites are also what a DISCOVER `query_revision` proposal emits
(above). The engine will not stamp one as applicable unless the substrate
returns an inverse for it, so the "refuse rather than apply" rule holds a
statement earlier for a model-authored rewrite than for a hand-authored one:
a reviewer is never offered a rewrite whose rollback would be a no-op.

The **audit trail is grains**: one immutable Observation per transition,
hash-chained per recommendation, carrying the actor label and the reason. It
syncs with the file and is queryable.

## The analyzers

Fourteen built-in analyzers, all deterministic (T0/T1), computing over typed
grains — never raw prose. Twelve are default-on; goal stagnation and retention
sweep are opt-in (see the table). Three are **telemetry-fed** —
they read the recall-telemetry sidecar (below) and move Areev Loop from *hygiene*
(is memory internally correct?) to *utility* (is memory used, and does it
help?):

| Analyzer | Fires on | Proposes |
|---|---|---|
| `tool_failure` | ≥N Tool-grain errors clustered by (tool, normalized signature), at ≥40% of that signature's **opportunities** — the tool's successful calls plus this cluster, *not* every call to the tool, because a call that failed some other way never reached this failure — **or** a large absolute count (so high-volume, moderate-rate failures aren't hidden) | a memory lesson (never auto-applies — evidence-derived text) |
| `duplicate_sweep` | exact-duplicate facts (NFC + case-fold) and near-duplicate observations (Jaccard) | consolidation (SUPERSEDE the extras) |
| `contradiction_sweep` | ≥2 live values under a functional relation (seeded list: `deploy_target`, `lives_in`, `tier`, … — extendable per domain via `extra_relations`) | resolve to the latest value |
| `fork_surfacing` | an entity with >1 live head | a merge (approval-required — a merge is lossy, never auto-applies) |
| `staleness` | a grain past its declared `valid_to` | a single-grain `FORGET` (destructive, never auto-applies) |
| `skill_stall` | a Skill practiced ≥N times whose proficiency stays low — doing it, not getting better at it | an advisory flag (never auto-applies) |
| `goal_stagnation` | an active Goal with little progress that's gone stale (**opt-in** — "stalled" is ambiguous; enable per file) | an advisory flag |
| `cold_grains` *(telemetry)* | a live fact never recalled past a grace window — memory not earning its place | a retire-candidate flag (advisory; cold ≠ wrong) |
| `coverage_gap` *(telemetry)* | a recurring recall question that keeps returning nothing — knowledge the memory should hold | a gap flag (advisory; the fix is to *add* memory) |
| `budget_pressure` *(telemetry)* | context assembly repeatedly overflowing its token budget (fed by the ASSEMBLE allocator) | a flag: raise the budget or curate |
| `retention_sweep` | grains older than a declared `max_age_days` (**opt-in** — a deletion policy is stated, never inferred; 0 = disabled) | one `FORGET` per over-age grain, batched per namespace (destructive, never auto-applies). The proposal names every grain it would remove, and states how many exceed the per-proposal cap rather than truncating silently. The cron equivalent is `areev retention sweep` — see [`gdpr.md`](gdpr.md) §2a |
| `outcome_review` | an applied recommendation past `review_after` that regressed | a revert |
| `run_outcome` | `areev run` workflows whose terminal runs keep failing/stalling/exhausting budgets (≥50% of ≥3 runs), or whose aggregate spend crosses a floor — fed by the run-outcome Observations the driver writes at every terminal run | an advisory flag per workflow (failure cluster and/or cost attribution) |
| `adapter_intake` | an unpromoted adapter registered by [`areev tune`](#the-tuning-seam-adapter_revision) (an `mg:adapter` Fact in `agent:harness`) — one candidate per served model, the newest | an `adapter_revision` pinned to its evalset (Rule E1; never auto-applies) |

Precision is measured, never asserted: `cargo run -p areev-bench --bin
loop_precision` scores each analyzer against a labeled fixture and exits
non-zero below 0.90 when invoked. The binary is an explicit evaluation command,
not a workflow step; reusable metric arithmetic plus the loop/golden tests run
under `cargo test --workspace`. On the current fixture the seven default-on analyzers it covers —
contradiction, duplicate, staleness, tool-failure, skill-stall, **cold-grains,
and coverage-gap** — each score **1.00** precision and recall; `fork_surfacing`
and `outcome_review` need concurrent heads / applied history, and
`budget_pressure` is a global signal, so those three are covered by the crate
tests instead. See `crates/areev-bench/RESULTS.md` for the table.

Both ASSEMBLE paths feed budget telemetry. Multi-source assemblies allocate
token budgets; the legacy single-source path interprets the numeric limit as a
grain count and reports `budget.unit = "grains"`. In either case, dropping any
candidate records an overflow sample for `budget_pressure`.

## The tuning seam — `adapter_revision`

The corpus path's last mile. `areev tune` hands a governed corpus to a
**host-supplied** trainer (Areev never trains, ships no trainer, takes no
training dependency) and registers the returned adapter as an `mg:adapter`
Fact in `agent:harness` — base model + adapter + quantization pinned as one
tuple, `derived_from` naming the corpus export manifest, and the Rule E1
evalset pin embedded. From there the loop governs it exactly like a code
revision:

```bash
areev tune --select '<READ CAL>' --out train.jsonl \
           --evalset <PIN> --cmd 'my-trainer --base qwen3-4b'
areev loop run                          # adapter_intake proposes the promotion
areev eval run --evalset <PIN> --model openai-compat:<serves_as>   # the gate
areev loop approve <rec> --because "…"
areev loop apply <rec> --gating-run <eval-run-id>
```

The apply writes an immutable `(model:<name>, mg:adapter_promotion)` Fact in
`areev-loop` carrying the payload plus the recorded gating edge. **That Fact
is the host contract**: serve whatever a live promotion names (query
`(model:X, mg:adapter_promotion)`), and treat a retracted promotion as the
stop-serving signal — `areev loop rollback` is the memory-side inverse; the
serving side is the host's move, and runs answered since promotion are not
reverted.

The lifecycle is deliberately **one candidate per served model**: while a
promotion is live the analyzer proposes nothing for that model — replacing a
promoted adapter starts with rolling the promotion back, after which the
newest unpromoted candidate is proposed. A rolled-back candidate re-proposes
while its registry grain stays live ("the situation returned"); retiring the
`mg:adapter` grain — a supersession or `FORGET` — is how a host silences it.
Multiple live candidates under one model are registry rows, not competing
values; don't "resolve" them as a fork.

Verify stays closed for adapters: when a baseline run of the pinned evalset
exists, the recommendation carries an `evalset:<pin>:failed` metric — re-run
`areev eval run --evalset <pin> --model …` after promotion and a recorded
regression makes `outcome_review` propose the revert. An adapter promotion is
auto-apply-impossible three independent ways (the `model:` class is excluded
by name, the analyzer is `AutoApplyClass::Never`, and origin rules still
apply). Erasure reaches the seam too: `forget-subject` reports which corpus
exports went stale **and which adapters derive from them** — auditable
suppression and re-derivation, never a claim that a subject left the weights.

## Recall telemetry (the utility signal)

Telemetry is what lets the last three analyzers exist. A disposable
`<file>.telemetry.db` sidecar records what recall actually surfaced — which
grains were retrieved, which questions came back empty, how often — so Areev Loop
can see memory *utility*, not just internal consistency.

- **Host-only; off in the library, `aggregate` for agent hosts.** The `areev`
  CLI (`--telemetry off|aggregate|full`, default aggregate) and the Python/Node
  constructors (`telemetry="aggregate"`) turn it on; a bare library `open()`
  records nothing. It is never a file-truth.
- **Buffered and non-blocking.** The recall hot path only pushes an in-memory
  event — no SQLite I/O touches the ~136µs recall / 50ms voice budgets (proven:
  voice-loop recall p50 stays ~82µs with telemetry on). The buffer drains
  off-path.
- **Encrypted under the same key** as the main file (crypto-erasure covers it),
  **never syncs** (bundles carry the memory file only), **rebuildable** —
  losing it costs evidence detail, never state. `FORGET` synchronously scrubs
  it. Modes: `off` | `aggregate` (rollups) | `full` (+ a per-recall ring log).
  A host-scoped `run_id` may be attached to full rows for trajectory joins; it
  is deliberately excluded from intent-rollup keys.

The console **Sessions** view visualizes it; `GET /api/loop/telemetry` serves it.

## LLM enrichment (optional)

The deterministic loop closes with no model. Attach one out of the box with
`areev loop run --model claude-sonnet` (the key comes from
`$ANTHROPIC_API_KEY`/`$OPENAI_API_KEY`/`$OLLAMA_HOST`; `--model openai:gpt-5`,
`--model ollama:llama3.1`, `--llm-base-url` for any gateway) — or
`--llm-cmd 'CMD'` for a subprocess backend. The built-in adapters
(OpenAI-compatible, Anthropic, Ollama) live in `areev-llm` over a small
blocking HTTP client, so the core crates stay dependency-light. Either way the
pipeline gains **strictly additive** stages —
`ANALYZE → DISCOVER → GROUND → VERIFY → ENRICH → VALIDATE+DEDUP → STORE` — that
are the identity when no backend is set:

- **DISCOVER** — the model proposes *additional* findings determinism can't see
  (a semantic contradiction, a stale assumption), under an **abstention-legitimate
  objective**: "nothing to report" is a first-class, zero-penalty answer, so it
  isn't pushed to over-generate. Every draft must **cite evidence** (uncited →
  dropped) and name a `target`; `origin = llm` so it can **never auto-apply**.
  A draft may also carry a **proposal** — a specific change it asks a reviewer
  to make. That is a **closed vocabulary of five kinds**, each mapping onto an
  apply path that already records an inverse:

  | kind | target | what an apply runs |
  |---|---|---|
  | `lesson` | `entity:<ns>/<subject>` | `ADD` a Fact, `relation = "lesson"` — one imperative line (≤240 chars) |
  | `fact` | `entity:<ns>/<subject>` | `ADD` a Fact under a model-chosen `relation` (an identifier, ≤64 chars) |
  | `query_revision` | `query:<name>` / `template:<name>` | `DEFINE QUERY`/`DEFINE TEMPLATE` — the agent revising how it assembles its own context |
  | `plan_revision` | `grain:<workflow hash>` | `SUPERSEDE … WITH workflow` from ≤8 field-level edits |
  | `code_revision` | `tool:<name>` | §7.4's promotion grain, behind the Rule E1 evalset gate |
  | `skill` | `entity:<ns>/<skill-name>` | `ADD skill` — a reusable procedure (description, `when_to_use`, ordered steps) from a trajectory that succeeded; `SUPERSEDE … WITH skill` when a live skill of that name exists. Offered only under `skills.enabled` |
  | `plan` | `entity:<ns>/<plan-name>` | one batch: `ADD workflow` (steps bound to tools the evidence shows were called, edges with conditions in the runtime's frozen grammar — handed to the substrate's plan validator first) **and** `ADD skill` of the same name (the prose). `SUPERSEDE` both when a live pair of that name exists. Offered only under `plans.enabled` |

  A draft with no proposal — or one the engine cannot resolve — stays an
  advisory flag, exactly as every DISCOVER finding used to. What resolves
  becomes an *applicable*, rollbackable recommendation, with the exact change
  an apply would make shown in the review summary.

  Four rules bound the surface, all enforced in the engine:

  - **The model never names its own scope.** Subject, query name, plan hash and
    tool name all come from the draft's `target`; a Fact's namespace comes from
    the cited evidence; and a `code_revision`'s evalset pin is read from the
    tool's own definition grain (`evalset_hash`) — a proposer that could choose
    its own grader is not gated.
  - **Resolution happens before the gates.** The draft becomes the exact
    statement an apply would run *before* GROUND and VERIFY see it, and that
    statement is folded into the claim they judge — so a malformed proposal
    costs no model call, and no gate ever judges a summary standing in for the
    payload.
  - **A `query_revision` body cannot restructure its own statement.** The body
    is the one place model text lands *inside* a statement rather than beside
    it, so it is refused outright if it contains a brace (closing the `AS { … }`
    block early is the injection shape) or a `FORGET`/`PURGE`/`DROP`/`DEFINE`
    token anywhere — a per-token scan, because the ordinary destructive check
    reads each line's *leading* keyword and a one-line injection passes it by
    construction. The substrate's `validate_cal` and the saved-query read-only
    verification still run after this; the engine simply does not assume either
    is strict.
  - **`plan_revision` is edits, not a replacement plan.** Only
    `edges.<i>.cond`, `edges.<i>.max_cycles` and `retries.<node>` are editable,
    so node topology cannot be expressed at all; each edit declares a `from`
    that must equal what the live plan holds (a proposal authored against a
    superseded plan does not apply to a newer one); values are type-checked
    (a string in `max_cycles` would be dropped by the grain deserializer and
    an "applied" tightening would silently mean *unlimited*); and the candidate
    body must pass the runtime's own plan validation before it is ever offered.
  - **Auto-apply is unchanged, twice over.** `origin = llm` is categorically
    ineligible, and independently `grants_auto_apply` admits only the `memory`
    target class — so a query, code or plan proposal cannot auto-apply even
    under a policy that names it. Every kind takes a human review with a
    BECAUSE plus an explicit apply.

  **Outcome records are first-class evidence.** DISCOVER is told that the
  bundle may contain records of whether a run was *accepted*, that restating
  a deterministic finding earns nothing, and that comparing rejected against
  accepted outcomes is how a problem with no error attached gets found — with
  a two-observation floor, because one rejection is an anecdote. Without that,
  a model handed both kinds of evidence reliably writes about the error text
  and ignores the rest (observed live: four authored lessons, all restating
  clusters the analyzers had already produced).

  **The evidence bundle budgets its sources.** DISCOVER sees at most 64
  grains, drawn from four places that answer different questions: what the
  deterministic findings CITED (≤24 — what clustering already caught), recent
  tool ERRORS (≤16 — what clustering could have caught and did not),
  human-authored **Observations** (≤8, taken before the rest), and recent
  facts (the remainder — the model's own lens).

  The Observation reserve exists for a different reason from the others.
  Learning does not only come from what went wrong: a person saying "from now
  on, do X" is a complete rule stated **once**, and recency or frequency
  seeding buries it under the thousands of routine grains a working desk
  produces. The rarest evidence is usually the most valuable, and ordering by
  volume is exactly the wrong instinct for it. Each share is
  reserved rather than served first-come, because one `tool_failure` finding
  may cite up to 64 grains on its own: without the reservation the lens is
  starved by the very determinism it exists to look past, and the symptom is
  indistinguishable from a model that simply found nothing.

  Two kinds need substrate support: `plan_revision` requires the `plans`
  capability (structural plan validation) and `code_revision` the `code`
  capability (the blob seam plus evalset resolution). A substrate declaring
  neither degrades those kinds to advisory rather than pretending to have
  checked them.
- **GROUND → VERIFY** — before a draft is ever queued it must pass an
  independent **grounding** check (are the finding's factual *premises* present
  in the cited evidence? — this guards against fabrication while still allowing a
  genuine *inference*, e.g. "HQ=San Francisco and country=Germany conflict") and
  an adversarial **verification** pass (is the finding sound and specific, not
  vague or spurious — abstention is legitimate). An authored lesson is folded
  into the claim both gates judge, so what survives is exactly what an apply
  would write — never a summary standing in for it. **Each is a separate call, so
  the proposer never grades itself**; grounding can even run on a different model
  (`--ground-model` / `--ground-cmd`) to take the generator out of the loop. Only
  findings that survive, above a confidence floor, reach review. This is what
  turns "generates something" into "generates something that survived a skeptic."
  Quality is measured, not asserted: the `loop_reflection` bench scores
  **Effective Reliability**, and `areev loop` reports the live approval-rate of
  LLM findings. Full design + evidence: [`loop-reflection.md`](loop-reflection.md).
- **ENRICH** — a whitelisted one-line `guidance` note on a deterministic
  finding; the engine-templated summary is always kept.
- **Fail-soft**: a failed/garbled/slow backend drops the contribution, never
  the run. Instructions never interleave with (untrusted) evidence text.

`CommandLlm` mirrors `--embed-cmd`: a JSON request on stdin → a JSON response on
stdout, one process per call, probed at construction. CLI-only, never persisted.
Ready-to-run backends live in `examples/llm/` (`claude -p`, OpenAI, ollama, and
a dependency-free mock) with the protocol documented.

## External analyzers (optional)

Determinism you can extend without recompiling: `areev loop run --analyzer-cmd
'CMD'` registers a subprocess analyzer. It receives a live-grain snapshot on
stdin and returns advisory findings on stdout (`{op:analyze,grains:[…]}` →
`{findings:[{target,summary,severity,evidence}]}`, self-describing via a probe).
It runs at **trust class `command`, auto-apply `never`** — a domain-specific
check (PII, a house style rule, a compliance sweep) can *surface* an issue a
human then reviews, but can never mutate memory. A failure skips that analyzer
for the run, never the pass. This is also the only custom-analyzer path from
Python/Node (which can't implement the Rust `Analyzer` trait): `loop_run(…,
analyzer_cmd="…")`. A ready-to-run sample (a PII scan, protocol documented
inline) lives in `examples/analyzers/`.

## Surfaces

### CLI — `areev loop`

```
areev init   [--template blank|demo|coding-agent] [--ns NS]   seed a backend + print hooks
areev loop run     [--min-new N --min-new-errors N --if-stale 6h --format json --quiet]
                    [--model P:N | --llm-cmd 'CMD'] [--ground-model P:N | --ground-cmd 'CMD']
                    [--analyzer-cmd 'CMD']
areev loop reflect  like run, but re-analyzes the WHOLE memory (ignores the incremental
                    watermark) — a full sweep; same flags as run
areev loop list    [--status pending|applied|all] [--fail-on high]   (exit 2 on match → CI gate)
areev loop show <hash>
areev loop approve|reject|apply|rollback <hash> --because "…" [--actor A] [--allow-destructive]
areev loop outcomes     the Verify gate — did applied advice hold or regress?
areev loop analyzers | policy
areev loop              (bare: a health summary)
```

`run` returns the **run-outcome contract** — `{outcome, skip_reason,
new_grains, new_error_events, proposed, deduped, stored, auto_applied,
analyzers_run, analyzers_skipped}`. Exit 0 on ran *or* clean skip (cron never
pages on a healthy no-op), 1 on error. Hashes accept git-style unique
prefixes.

### Bindings — Python & Node

Same methods in both (scalars in, JSON strings out):

```python
db = areev.Areev("agent.db", actor="user:alice")
db.record_tool_call("stripe_refund", result_json, is_error=True, thread="sess-42",
                    call_id="toolu_01A", input=args_json)
db.loop_run(min_new=20, min_new_errors=3, if_stale="6h")   # gated; bare call never gates
db.loop_run(full_sweep=True)                 # the `reflect` semantics: whole memory
db.loop_run(policy="loop-policy.json")     # host policy file — the only auto-apply path
db.loop_run()   # the returned JSON carries `llm_funnel` when a backend is
#   attached: evidence → proposed → cited (with `dropped_uncited` and
#   `dropped_target` split out) → grounded → kept → stored. "The model
#   contributed nothing" has five causes that need opposite fixes and all
#   render as an empty queue; this is how you tell them apart.
db.recommendations('{"status":"pending"}')
#   rows carry hash/status/severity/analyzer/summary/target_ref/destructive,
#   plus `rollbackable` and `evalset_hash` — Rule E1's pin, so a reviewer can
#   see which gate a code or adapter revision will be held to BEFORE they
#   approve it (null on every other kind; the engine refuses a pin elsewhere)
db.apply_recommendation(hash, because="…")     # audited approve+apply
db.apply_recommendation(hash, because="…", gating_run="eval-…")  # a gated
#   (code/adapter) revision: evidence loads from the recorded eval summary,
#   and an ungated attempt refuses BEFORE the approval lands
db.dismiss_recommendation(hash, "…")           # audited reject
db.rollback_recommendation(hash, because="…")  # retract what an apply created
db.loop_outcomes()                           # the Verify gate's held/regressed record
# The tuning seam for hosts that train in-process (the CLI stays the paved road):
db.record_corpus_export(selector, destination, source_hashes=json.dumps([...]))
db.record_adapter(reply_json, manifest_hash, evalset_hash)
```

Node mirrors these as `recordToolCall`, `loopRun` (incl. `fullSweep` /
`policy`), `recommendations`, `applyRecommendation` (incl. `gatingRun`),
`dismissRecommendation`, `rollbackRecommendation`, `loopOutcomes`,
`recordCorpusExport`, and `recordAdapter`, plus the `actor` constructor
argument.

### MCP — two tools

`areev_loop` runs a pass and returns the pending queue (call it at session
start). `areev_recommendations` lists, or acts (`apply`/`approve`/`reject`
with a mandatory `because`). Launch a reviewer process and worker processes
with different `--scopes`/`--actor` so no agent can approve its own proposals.

### HTTP — `/api/loop/*`

`GET recommendations|health|analyzers` (reads) and `POST run|review|apply|
rollback|config` (writes). `POST /api/loop/apply` takes an optional
`gating_run` — the `eval-…` run id a **code or adapter revision** requires;
the evidence is loaded server-side from the journaled `mg:eval_run` summary,
never from the request. The console's Areev Loop tab renders the queue with
severity dots, evidence, and approve/apply/reject actions gated behind a
mandatory reason; a gated recommendation (its row carries `evalset_hash`)
additionally asks for the gate run id before it will apply. The **Setup**
tab is writable — click an analyzer on/off to persist an enable/disable to
the file's config (`POST /api/loop/config`). Auto-apply is never grantable
from the console — only via a host policy file.

## Does it actually work? — the Verify gate

The honest test of self-improvement is not "did it make a change" but "did the
change help." Areev Loop answers that for itself. When you apply a recommendation
that carries a metric, the engine re-measures it after the review window and
records a **measured outcome** — `held` or `regressed`:

- A tool-failure lesson's metric is **recurrence**: after you apply the lesson,
  does that exact tool failure happen again? Baseline is zero — the fix is
  supposed to stop it. If the failure recurs, the outcome is `regressed` and
  outcome review proposes a **revert**; if it doesn't, the outcome is `held`.
- A contradiction resolution's metric is **recurrence** too: after resolving
  to the latest value, does the subject again hold two live values under that
  functional relation? A returned conflict regresses the checkpoint and
  proposes a revert for human judgment. (Duplicate consolidation carries no
  metric yet: a supersession creates a replacement grain, so a live-grain
  count can't honestly measure it — that needs a supersede-by-existing
  substrate primitive first.)

A revert the gate proposed and a reviewer applied is a verdict on the
finding, not only on that apply: the lesson was tried and it hurt. So the
reverted finding goes on the same doubling cooldown a rejection earns (7d,
14d, … capped at 90d) and the next pass does not re-propose it, even though
the situation that produced it is still there. A rollback an operator runs by
hand (`areev loop rollback`) earns no cooldown — the finding may come back on
the next pass, which is what lets a lesson be restored through the governed
path after a deliberate retraction.

Crucially, it re-measures on a **schedule of checkpoints**, not once — so an
outcome that looked fine early can be caught regressing later. A single fixed
window would freeze a false "held"; the time series doesn't. The schedule is
in whatever unit the deployment counts: elapsed time (the 1d / 7d / 30d
default), evalset runs since the apply, or grains written since it — a
checkpoint counted in runs renders as `@1 run`, one in grains as `@50 grains`:

```bash
areev loop outcomes --db agent.db
#   a6f8133  tool_error_recurrence  @1d    baseline 0 → current 0  [held]
#   a6f8133  tool_error_recurrence  @7d    baseline 0 → current 0  [held]
#   a6f8133  tool_error_recurrence  @30d   baseline 0 → current 2  [regressed]  ← late recurrence caught; revert proposed
```

The re-measurement is a typed read over subsequent history (no LLM, no
guessing), recorded as a file-truth so it syncs and accumulates. That is the
difference between "governed memory hygiene" and self-improvement that proves
its own advice — the record is the evidence.

**The honest boundary.** This works for **internal, bounded, attributable**
outcomes — facts about data Areev Loop owns (did this tool fail again, does this
duplicate still exist). It does **not** measure open-ended, confounded,
world-facing outcomes (was a generated post good, is a patient happier). Those
depend on signals outside Areev and on a hundred factors that aren't the
change, so the honest output is a **monitored trend a human judges**, never a
machine verdict — the design suppresses causal claims at low sample sizes on
purpose. Areev Loop improves the agent's *memory*, not its *outputs* (§2.4).

### Evalset-backed outcomes — where that boundary legitimately moves

If you have a **labeled ground-truth set**, external correctness stops being
open-ended: an evalset run is itself internal, bounded and attributable. So a
recommendation may carry a metric naming one:

```
metric = "evalset:<EVALSET_HASH>:<field>"
```

`<field>` is read from the summary `areev eval run` journals. Four names are
promoted and work against any evalset — `passed`, `failed`, `total`,
`error_rate` (`failed/total`) — and anything else is read from the summary your
harness wrote, e.g. `evalset:abc123:category_accuracy`.

**State the direction.** The built-in metrics are recurrence counts where lower
is better; an accuracy is the opposite. `MetricSnapshot.higher_is_better` says
which, and it is not cosmetic: read the wrong way, the Verify gate sees a rule
that *improved* accuracy and proposes reverting it. The comparison lives in one
function (`recommendation::is_regression`) that both the engine's recorded
verdict and `outcome_review`'s revert draft call.

**A run from before the apply is never evidence.** The lookup is scoped to
summaries journaled at or after the apply. If no eval run has happened since,
the metric is *not yet measurable* and the checkpoint stays due — the engine
does not fall back to the baseline run. Scoring the baseline against itself
would report `held` forever, which is a fabricated receipt and worse than none.

**Authored lessons are measured the same way.** With `outcome_evalset` in
the host policy, an applied LLM-authored lesson is re-measured against the
evalset at every checkpoint; a run after the apply that scores worse than
the newest run **before the apply** is `regressed`, `outcome_review`
proposes the revert, and applying it retracts the lesson and puts it on the
rejection cooldown so the next pass does not re-propose what the gate just
removed. The proposal freezes the newest run of its day as the snapshot;
the verdict reads the newest run before the apply when one exists, so a
deployment that measures before each apply judges each rule against the
state it changed, and one that measured only on day one judges every rule
against day one (below).
That is the whole "verify the change improved, otherwise revert" arc, on
the one kind of change a human approves from prose alone.

A revert's identity is the recommendation it retracts: two lessons on one
entity that both regress get two reverts (a deterministic finding's dedup
key is analyzer + target + action, and keyed that way the second revert was
dropped as a duplicate of the first until 2026-09-06). And the verdict has
**no noise floor**: any drop past `1e-9` is a regression, so a 359 → 355
dip on 387 trials — within what one adapter read twice can differ by —
proposes a revert. That is a limit, stated here; a policy-level minimum
effect size is not implemented.

No scheduler is implied: run `areev eval run` from cron or CI exactly as you run
`areev loop run`; outcomes only ever **read** what it journaled. The apply gate
(`areev loop apply --gating-run <id>`) and the outcome edge deliberately read
those summaries through **one** function (`areev_loop::eval`), so a rule cannot
be admitted on one reading of an evalset and judged on another.

So for a learned vendor-alias rule, the receipt becomes exactly what it should
be: *canonical-vendor accuracy on the 184-row ground truth went up, and stayed
up at 1d, 7d and 30d.*
Outcomes accrue over real calendar time as checkpoints elapse; the loop is
exercised end-to-end by the engine test suite, which controls the clock.

### What the gate does not catch — measured, not hypothesised

Both of these were found by running the loop on a public corpus, not by
reading the code. The evidence is
[`crates/areev-bench/ADBUY.md`](../crates/areev-bench/ADBUY.md), seed 3.

**Outcome measurement catches damage, not lost opportunity.** The comparison
is against the newest run journaled before the apply. An agent that
climbed to 238 of 280, then fell to 128 as later rules landed, is still four
times better than the day-one run of 35 — so when day one is the only run
journaled before the apply, the gate reports `held`, correctly by its own
definition, and no revert is proposed. It has no way to see the 238. The
per-rule marginal measurement is now what the verdict does *when the host
journals a run before each apply* (`crates/areev-bench/CURVE.md`, seed 1: a
rule that contradicted an earlier one took the agent from 86% to 66% and
measured as `held` against day one's 26% until the checkpoint reads were
journaled onto the timeline). A high-water mark carried forward is not
implemented; a deployment that does not measure between applies still sees
a rising-then-falling agent as a rising one.

**Dedup is by content, not by meaning.** `authored_dedup_key` fingerprints
the proposal text, so it collapses a rule proposed twice verbatim. It cannot
collapse the same instruction rephrased — which is what an LLM proposer
emits, pass after pass, from the same recurring evidence. In that run ten
approved rules stated four distinct facts, each true and well-formed enough
that a reviewer approved it alone, and the agent stopped emitting the very
fields the rules most insistently named. Every rule in the prompt is a rule
competing for the model's attention; a reviewer judging one at a time cannot
see the pile. Semantic near-duplicate suppression is not in the engine.

Neither is a bug in the four gates. Both are limits of what the gates
measure, and a host running the loop unattended over many passes should
know them.

## Triggers — no daemon, anywhere

A loop run is a cheap, idempotent command that hosts trigger however they
already trigger things (hooks, cron, CI, MCP calls). Gates make repeat runs
free:

- `--min-new N` / `--min-new-errors N` — run only after enough new grains /
  tool failures since the last run (a file-truth watermark).
- `--if-stale 6h` — run only if the last run is older than the interval.

The SessionEnd Claude Code hook runs `areev loop run --min-new 20
--min-new-errors 3 --quiet`, so most session ends are a watermark check that
exits immediately. There is no scheduler in the product.

The loop also closes **into** the agent's context: the UserPromptSubmit hook
`areev recall-hook --with-loop` appends a compact block of pending
recommendations (severity + summary, capped at 3, `origin=llm` entries
labeled) to the memory it injects — so the agent sees its own pending queue
instead of waiting to be asked. `areev init` and `areev hook claude-code` print
the flag in their snippets.

## Where the loop's own state lives, and why it is a grain

The loop persists one JSON blob — analyzer config, recommendation lifecycle,
audit-chain heads, creators, cooldowns, and the run watermark — as a **Fact
grain** in namespace `areev-loop`, subject `__loop_state__`, superseded on each
write.

`areev trigger` stores *its* state the opposite way: `trg:` rows in the store's
`meta` table that deliberately never replicate. The asymmetry is a decision, not
drift, and it is worth writing down so nobody harmonises one to match the other.

**The loop's state must replicate, because most of it is governance.**
`creators` and `co_creators` are what the self-approval block reads to refuse an
approval by the principal who authored the recommendation; `audit_heads` chains
the audit records; `status_index` carries the lifecycle. If those stopped
travelling with the file, a replica would find no creator recorded and let
someone approve their own recommendation — a separation-of-duties bypass, and a
silent one.

**A trigger's state must not, because it is a cursor.** A dev memory restored
from prod that inherited prod's Gmail cursor would skip real mail while
reporting success.

The replication hazard also lands differently. Two hosts sharing a memory, where
one skips because the other ran the loop five minutes ago, is **correct** — the
loop analyses shared memory and its recommendations replicate too, so the work
genuinely was done. The same reasoning does not transfer to a poll of an
external system.

The cost of the grain form is growth: one grain and two op-log entries per run,
each carrying a full copy of the blob, with the superseded chain retained. What
grows is the *history*, not the live grain, so the remedy if it bites is
compaction — not restructuring, which would trade the atomicity of a single
supersession for disk. That is not a trade a governance subsystem should make.

## Auto-apply & the policy file

Auto-apply is **off by default** and is granted **only** by an optional
host policy file — `areev loop --policy loop-policy.json` (or
`$AREEV_LOOP_POLICY`):

```json
{
  "auto_apply_enabled": true,
  "auto_apply": [
    { "analyzer": "loop.duplicate_sweep", "targets": ["memory"], "max_severity": "low" }
  ],
  "deny": [],
  "severity_floors": { "loop.staleness": "medium" },
  "telemetry": "aggregate",
  "discover_objective": "review_queue",
  "outcome_evalset": {
    "hash": "<evalset hash>", "field": "passed", "higher_is_better": true,
    "checkpoints": [{ "after_runs": 1 }, { "after_ms": 604800000 }]
  },
  "evidence_attribution": "named",
  "cadence": { "every_events": 10 },
  "skills": { "enabled": true, "min_steps": 2 },
  "plans": { "enabled": true, "min_nodes": 2 },
  "premise_drift": true,
  "min_evidence": 1
}
```

Three of those blocks decide **when** the loop acts and **what** it may
author, and each defaults to what every deployment had before it existed:

`cadence` (default: none — a pass is due whenever it is called) is the
per-call gate (`--min-new`, `--min-new-errors`, `--if-stale`) written once,
in the policy, so the CLI, the MCP tool and the console all keep the same
rhythm — and with the units a chat deployment counts in: `every_ms`,
`every_grains`, `every_events` (turns) and `every_sessions` (distinct
conversations). Any threshold met makes the pass due. Explicit flags still
override the block (host CLI flags > policy file), and `areev loop reflect`
always runs: a sweep is a command, not a tick. A skipped pass reports
`cadence_not_due`.

`skills` (default: on, two steps minimum) lets DISCOVER propose a **Skill**
— a reusable procedure with a description, a `when_to_use` cue and ordered
steps — from a trajectory that succeeded. The name comes from the target
(`entity:<ns>/<skill-name>`), the namespace from the evidence, and a live
skill of that name is superseded rather than duplicated. It is a draft like
any other: GROUND, VERIFY, the confidence floor, a review with a BECAUSE,
never auto-applied. When it is on, successful tool calls join the evidence
bundle after the failures, inside the same reserved share; off, the bundle
is exactly what it was. It exists because, measured on PAST-Bench, the agent
performed a procedure correctly and then answered "nothing to save" when
asked — every skill in the memory had depended on the model volunteering one
mid-task (`crates/areev-bench/PERSIST.md`).

`plans` (default: on, two steps minimum) lets DISCOVER propose the same
procedure as a **plan** — a Workflow grain the runtime validates before a
reviewer sees it (unique, reachable steps; edges whose conditions parse;
bounded cycles), each step bound to a tool the cited evidence shows was
called, beside a Skill of the same name carrying the prose. A skill is what a
model reads; a plan is what `areev run`, the run journal and `run_outcome`
can reach, and what `plan_revision` can patch by field. The benchmark's own
label for every procedural family — "ordered steps, tools, conditions; a
patched v2 supersedes v1" — is a Workflow.

`premise_drift` (default: on) is the Verify gate's second question. Every
applied recommendation cites the grains it was derived from; when one is
later superseded by a **different** value, or retracted, the premise the
reviewer approved no longer holds, the gate records `drifted`, and
`outcome_review` proposes the revert. A value-identical supersession (what
consolidation does) is not drift. It exists because a lesson that outlives
its premise is measured harm: on PAST-Bench a rule encoding the old regime's
flag cost the governed arm 0.32 on the very migration family it was learned
in.

`min_evidence` (default 1) is the fewest distinct grains a draft must cite
to be offered as a change; under it the draft is stored and reviewable but
applies as nothing. An independent audit of 88 governed decisions found 15 of
28 approvals had made one instance into standing policy; `2` is what that
audit argues for. The funnel reports the demotions as
`advisory_thin_evidence`.

`evidence_attribution` (default `named`) decides whether an Observation
reaches the LLM with its observer named — `<observer> (a person) said of
<subject>: <text>` — or as bare text. It is host policy for two independent
reasons. An observer id can be a person's name or account, and whether that
belongs in a model prompt is a privacy decision only the host can make. And
attribution changes what gets proposed: a bare correction ("Vendor Name is
ACME") is ambiguous about direction, and a model given a run of them
concluded the *agent* had been asking for data it already had, proposing
rules to stop it asking (`crates/areev-bench/RECEIPTS.md`). `anonymous`
restores the pre-2026-09-04 rendering exactly, which is what makes it usable
as an ablation switch.

`outcome_evalset` (optional, default none) gives every **applicable
LLM-authored proposal** — a lesson, a fact, a query or plan revision — the
host's evalset as its outcome metric: baseline from the newest
`mg:eval_run` summary journaled before the **apply** (the proposal freezes
the newest run of its day; a run journaled between proposal and apply
replaces it at verdict time), current from summaries journaled after the
apply, at the **checkpoints** the host sets — `horizons_ms` (default 1d /
7d / 30d) or, in the deployment's own unit, `checkpoints`: `{"after_ms": n}`,
`{"after_runs": n}` (evalset runs journaled since the apply) or
`{"after_grains": n}` (grains written since it); a bare integer is ms. A
benchmark or CI harness wants `[{"after_runs": 1}]` — measure at the next
graded run, however soon — because a schedule counted in days is inert on a
deployment that finishes in minutes: on PAST-Bench the day-long default fired
zero verdicts across 78 governed runs. It exists because an authored lesson carries no recurrence metric —
nothing errors when a lesson is merely useless or quietly harmful — so
without it the Verify gate had nothing to re-measure for exactly the
proposals a reviewer was least able to judge from the text. No run
journaled yet → no metric, never a fabricated one; the direction is
mandatory because a guessed one would revert an improvement.

`discover_objective` picks the scoring rule the LLM proposer is given
(`docs/loop-reflection.md` §5.1) and nothing else — the gates behind it are
the same either way. `review_queue` (default) makes "nothing to report" a
zero-penalty answer and a wrong finding cost twice a right one: the rule for
a queue a person triages. `learner` is for an agent that has to improve from
this pass: abstaining while the evidence holds a recurring failure, two or
more rejected outcomes, or a person's instruction is penalized like a wrong
lesson, and the model is told to prefer the one proposal that addresses the
most frequent failure. It exists because, measured live, a cheap model under
the review-queue rule authored a lesson on fewer than half of its passes over
evidence that plainly held one (`crates/areev-bench/RESULTS.md`, the 2x2).
Every draft under either objective still passes GROUND, VERIFY, the
confidence floor and a human review with a BECAUSE; the objective changes
what the proposer is asked to optimize, not what may reach the memory.

A recommendation auto-applies **only if all** hold (proposal §6.3): host
opt-in + a matching grant, a built-in analyzer (never command/LLM), a
`memory`/`query` target (never prompt/host), non-destructive, and
engine-side shape verification — the batch must be SUPERSEDE-only **and
value-identical**: every replacement field is checked against the grain it
supersedes (case-fold/trim; `namespace` against the grain's own), so only
consolidation that provably changes no value qualifies. An ADD that
introduces evidence-derived text, a FORGET, or a near-duplicate consolidation
that rewrites an observation body all stay pending. Anything failing stays
pending. The policy file rejects unknown keys, so it can never arrive
pre-armed; it is host config and is never persisted in a memory file.
`areev loop policy` prints the effective policy.

The same policy file attaches to the other run surfaces — `areev ui --policy`
(console-triggered runs) and `areev serve --mcp --policy` (the `areev_loop`
tool) — so every surface honors one set of grants, set at process start and
never controllable by a client.

## Read-only console (breaking change)

Token-less `areev ui` is **read-only**: it browses the queue but cannot act.
Every write — any loop mutation, an `ADD`/`SUPERSEDE`/`FORGET` CAL batch —
requires `areev ui --token-env VAR`. This closes the path where a local
process could execute a proposal's CAL directly and skip the review queue.
Existing write callers add `--token-env`; a token unlocks review + apply.

## Compatibility notes

- **Interim grain mapping.** The OMS 1.5 `0x0C` Recommendation type **is** now
  realized in areev-core, but Areev Loop has not migrated to it: recommendation
  and audit grains still ride as Facts in the `areev-loop` namespace with the
  field-map carried as JSON. They are real, content-addressed, syncable
  grains. Moving the queue to the native type is a data migration, not a
  format change — existing content addresses stay valid either way (additive,
  per OMS §4.5) — and it is sequenced separately so landing the type does not
  rewrite anyone's live queue. Note that a file containing `0x0C` grains
  stamps `min_reader_version`: `deserialize_blob` errors on an unknown type
  byte rather than skipping it, so such a file is unreadable to a pre-1.5
  build.
- **Tool grains.** The flagship analyzer reads Tool grains (0x05), which
  carry `tool_name`/`is_error`/`content` natively. `record_tool_call` and
  `areev migrate --from tool-log` both produce them.
- **Authored proposals dedup on content.** An analyzer finding keys on
  `family ⟂ target ⟂ action` and deliberately not on content, so a growing
  cluster does not re-propose as novel. An LLM-authored executable proposal
  keys on a fingerprint of its content as well: two different lessons on one
  entity are two findings and both reach the queue, while the same lesson
  re-authored (different confidence, spacing, case) is one and is deduped
  against the pending or applied original. Rejection and measured-revert
  cooldowns therefore apply to *that lesson*, not to every lesson on the
  entity. Found by the receipts harness: the second rule the accountant
  asked for was silently dropped as a duplicate of the first.
- **Occurrences, not values.** Content-addressed dedup is right for a fact —
  a fact restated is the same fact — and wrong for a tool call: a tool that
  failed five times is a different state of the world from one that failed
  once, and that count is the entire input to `loop.tool_failure`. So
  `record_tool_call` stamps each call with an identity (`call_id`, or a
  synthesized one) and recording is append-only. Pass the provider's real
  `tool_call_id` when you have it — it is stored as the grain's
  `tool_call_id` and is queryable, so a recommendation's evidence links back
  to the transcript that produced it. Adding a Tool grain through the raw
  `add()` path keeps ordinary value semantics.
- **Determinism.** A loop run's *deterministic* recommendations are a pure
  function of (store state, params, now) — the same finding yields the same
  `dedup_key` on any host, so a synced file behaves identically on its next
  host. The optional LLM layer only *adds* `origin = llm` drafts; it never
  changes the deterministic set.

## Status

Built and tested: the engine (thirteen analyzers, lifecycle, dedup, gating,
auto-apply, the multi-horizon Verify gate, the optional LLM DISCOVER/ENRICH
stages), the recall-telemetry sidecar and its three telemetry-fed analyzers,
the Areev adapter, the `areev loop` CLI + `areev init` (incl. `--telemetry`
and `--llm-cmd`), the Python/Node bindings (telemetry-enabled), the MCP tools,
the tool-log importer, the policy file, the `/api/loop/*` API (incl.
`/telemetry`), the read-only-token-less auth, the Areev Loop console tab (queue /
analyzers / **sessions** / outcomes / **setup**), the `examples/llm/` backends,
and the precision bench.

Also shipped since: `budget_pressure` reads the live ASSEMBLE overflow signal
(default-on); the LLM operator-taste history (recent approvals/rejections) is
passed to DISCOVER so the model learns this reviewer's taste; the bindings carry
`model`/`llm_cmd`/`ground_*`/`analyzer_cmd`; a **pluggable grounding backend**
(`--ground-cmd`), **external command analyzers** (`--analyzer-cmd`), a
**full-memory sweep** (`areev loop reflect`), and a **writable console Setup**.

And in the post-merge follow-up pass: the auto-apply **value-identity check**
(near-duplicate consolidations stay pending, as §6.3 always intended);
analyzer writes now **carry their namespace** (a consolidation or lesson can
no longer drift to the store default namespace — the tool-failure lesson lands
in the dominant namespace of its evidence); a **contradiction-recurrence
metric** (the Verify gate now measures resolutions, not just tool lessons);
**`recall-hook --with-loop`** (pending recommendations ride into the
injected context); bindings parity (`rollback_recommendation`,
`loop_outcomes`, `full_sweep`, `policy`); the **host policy attaches to
`areev ui` and `areev serve --mcp`**; and an `examples/analyzers/` sample.

The whole loop is now pinned end to end by a **golden E2E suite**
(`crates/areev-cli/tests/golden_loop_tests.rs`): a committed dataset in
which every deterministic analyzer has a seeded target, driven through the
real `areev` binary with the engine clock pinned via **`AREEV_LOOP_NOW_MS`** (a
simulation seam honored by the CLI, MCP serve, and the console — with it, and
with recommendation/audit grains stamped from engine time, a run is a pure
function of (file, policy, now), so queue listings are byte-pinned including
content addresses, and outcome horizons / rejection cooldowns are tested by
stepping time instead of sleeping). The same pass made `areev-loop show` carry
the reviewable proposal (the CAL that will execute), the outcome metric, and
guidance; stamped external-analyzer findings `origin = command` (they were
mislabeled `builtin` — the `[external]` badge could never render); and added
the trust class to `areev-loop analyzers`.

Remaining follow-ups (documented, not blockers): **migrating Areev Loop onto the
native OMS `0x0C` Recommendation grain**, which now exists in `areev-core`
(OMS 1.5 landed it, resolving the spec-level decision that had deferred it).
Recommendations still ride as Facts with a distinguishing relation
(`loop_recommendation`) until that migration is sequenced. And a labeled
non-parasitic
corpus for a published Effective-Reliability number. See `loop-proposal.md`
for the full plan.
