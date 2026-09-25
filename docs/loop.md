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
  → withdraw  (premise drift)                — the evidence moved; leave the queue
```

The loop closes **without requiring an LLM** — a floor that always runs, not
a ceiling, and not a boast: deterministic signature lessons are also what
measured strongest on the self-improvement benchmark
([RESULTS.md](../crates/areev-bench/RESULTS.md#areev-loop-self-improvement--the-abab-causal-proof)),
and `--llm-cmd` adds verified reflection on top where judgement needs
language. Whichever runs, the guarantees are the same: every recommendation
cites the grains it was computed from; every apply stores its inverse (or is
marked non-rollbackable up front); every decision carries a written reason.

### The lifecycle, and `withdrawn`

`pending → approved → applied → rolled_back` are human (or policy)
decisions; `expired` says time ran out. Since 1.9.0 there is one more, and
the ENGINE is its only author:

**`withdrawn` (#317)** — every grain the recommendation cited has moved:
retracted, or superseded by a different value. Premise drift used to be
checked on APPLIED recommendations only, so a pending finding whose entire
evidence had been destroyed stayed pending and could still be approved — a
reviewer was offered, and could act on, a finding with nothing left behind
it, and applying it produced a recommendation to revert it on the next pass.

- Governed by the existing `premise_drift` policy switch, with the same
  definition of "moved" and the same `same_value` comparison, so a
  value-identical consolidation is not drift.
- `premise_drift_open_all` (default `true`) requires **every** cited grain to
  have moved. A finding derived from six grains of which one changed is
  weakened, not baseless — that is a reviewer's judgement, not the engine's.
  `false` is the stricter sweep.
- A withdrawal writes a hash-chained audit record with a templated reason
  ("N of M cited grains were superseded by a different value or retracted"),
  observer `System`, actor `engine:loop.premise_drift`.
- It **strikes no cooldown** and is excluded from the dedup keys, so the same
  finding on NEW evidence is proposed normally on the next pass. The engine
  withdrew it because the evidence moved, not because anyone decided against
  the finding.
- `RunResult.withdrawn` counts them; `loop list --status withdrawn` shows
  them; `review` of one returns `LOP-E020`.

`expired` remains **reserved**: the state machine admits it, and nothing
assigns it. It says "time ran out", which is a different claim from "the
premise moved".

### The queue is namespace-scoped (#312)

The loop's INPUTS were namespace-grant-gated and its OUTPUTS were not: every
recommendation went to one namespace, `areev-loop`, and rights were checked
against that one namespace. A recommendation's summary, proposal content,
guidance and evidence hashes are **derived content**, so one
`read ON areev-loop` grant disclosed all of it for every namespace in the
memory, one `loop.review` grant decided all of it, and a reject struck a
memory-wide cooldown keyed on the finding.

Since 1.9.0 a `Recommendation` carries **`scope`**: the normalized, sorted
namespace list the producing analyzer was run over, stamped by the engine
where `dedup_key` and `origin` are — so an analyzer, an external command or
a model draft cannot set it.

A principal **covers** a recommendation when its grants allow the verb on
**every** namespace in that scope: `read` to list or show, `loop.review` to
approve or reject, `loop.apply` to apply or roll back. Two deliberate escape
hatches keep existing deployments working unchanged:

- a grant on `areev-loop` itself (or `*`) still means the **whole queue**, so
  owner sessions and today's operator grants behave exactly as before;
- an **empty** scope — an unscoped pass, and every recommendation written
  before this existed — is covered only by such a whole-queue grant. Fail
  closed: "derived from we-don't-know-where" must not be readable by someone
  holding one namespace.

A recommendation the caller does not cover answers **`LOP-E040` not found**,
not "not authorized", so its existence is not disclosed. Listing goes through
ONE filtered read (`areev_loop_adapter::visible_recommendations`) that the
CLI, the server, MCP, both bindings and `DESCRIBE LOOP` all call, so a
surface added later cannot forget the check.

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

Fifteen built-in analyzers, all deterministic (T0/T1), computing over typed
grains — never raw prose. Twelve are default-on; goal stagnation, retention
sweep and the lesson pile are opt-in (see the table). Three are **telemetry-fed** —
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
| `cold_grains` *(telemetry)* | a live fact never recalled past a grace window — memory not earning its place. **Skips a fact a live Tool Definition pins as its `evalset_hash`**: Rule E1 reads that gate at review time rather than through recall, so it would look cold forever, and retiring it would remove the gate a `code_revision` has to pass | a retire-candidate flag (advisory; cold ≠ wrong) |
| `coverage_gap` *(telemetry)* | a recurring recall question that keeps returning nothing — knowledge the memory should hold | a gap flag (advisory; the fix is to *add* memory) |
| `budget_pressure` *(telemetry)* | context assembly repeatedly overflowing its token budget (fed by the ASSEMBLE allocator) | a flag: raise the budget or curate |
| `retention_sweep` | grains older than a declared `max_age_days` (**opt-in** — a deletion policy is stated, never inferred; 0 = disabled) | one `FORGET` per over-age grain, batched per namespace (destructive, never auto-applies). The proposal names every grain it would remove, and states how many exceed the per-proposal cap rather than truncating silently. The cron equivalent is `areev retention sweep` — see [`gdpr.md`](gdpr.md) §2a |
| `outcome_review` | an applied recommendation past `review_after` that regressed | a revert |
| `run_outcome` | `areev run` workflows whose terminal runs keep failing/stalling/exhausting budgets (≥50% of ≥3 runs), whose aggregate spend crosses a floor, or whose transcripts keep outgrowing the model's window (≥1 fold per run) — fed by the run-outcome Observations the driver writes at every terminal run | an advisory flag per workflow (failure cluster, cost attribution and/or context pressure) |
| `adapter_intake` | an unpromoted adapter registered by [`areev tune`](#the-tuning-seam-adapter_revision) (an `mg:adapter` Fact in `agent:harness`) — one candidate per served model, the newest | an `adapter_revision` pinned to its evalset (Rule E1; never auto-applies) |
| `lesson_pile` | more than `max_active` (default 8) live lessons on one entity (**opt-in** — a budget is stated, never inferred). Measured need: on the ad-buy corpus ten approved rules stating four facts took the agent from 238 to 128, and a reviewer judging one card at a time could not see the pile | an advisory flag listing the pile with each member's latest Verify-gate verdict (`held` / `regressed` / `drifted` / `unmeasured`), so the reviewer can retire what measured badly; with an LLM attached it is also DISCOVER's cue to draft **one consolidating lesson** (`kind: consolidation`) through GROUND → VERIFY — gate-judged, human-applied, never auto-applied; its apply supersedes every member and `rollback` restores them all |

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
  it. Modes: `off` | `aggregate` (rollups) | `aggregate-hashed` | `full`
  (+ a per-recall ring log). A host-scoped `run_id` may be attached to full
  rows for trajectory joins; it is deliberately excluded from intent-rollup
  keys.
- **`aggregate-hashed` keeps the rollups and none of the query TEXT** (1.9.0,
  #306). What a person types while working in one namespace is content:
  under `aggregate` it is retained memory-wide in `telem_query_stat.qkey` and
  `.sample`, and copied into `areev-loop` recommendations by `coverage_gap`.
  Here the key is the hex digest of the same intent key — HMAC'd under a key
  derived from the memory's own AEAD key, because query strings are
  low-entropy and a bare digest would hand an attacker an offline guessing
  oracle — the sample is empty, and the ring log is not written.
  `cold_grains`, `coverage_gap` and `budget_pressure` keep working: they need
  counts and distinctness, not the text.
- **`areev telemetry scrub --ns NS --yes`** (and
  `Areev::telemetry_scrub_namespace`) drops every `telem_*` row for one exact
  namespace, plus the buffered events that have not reached them. It reaches
  the row nothing else could: a **zero-result free-text query** names no
  grain hash, so the per-hash scrub cannot find it, and the per-subject scrub
  only reaches it if the erased identity happens to appear in the text.
  Erasing a namespace should take its recall evidence with it, and every
  `telem_*` row carries `ns`, so it can.

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
  to make. That is a **closed vocabulary of eight kinds**, each mapping onto an
  apply path that already records an inverse:

  | kind | target | what an apply runs |
  |---|---|---|
  | `lesson` | `entity:<ns>/<subject>` | `ADD` a Fact, `relation = "lesson"` — one imperative line (≤240 chars) |
  | `fact` | `entity:<ns>/<subject>` | `ADD` a Fact under a model-chosen `relation` (an identifier, ≤64 chars) |
  | `query_revision` | `query:<name>` / `template:<name>` | `DEFINE QUERY`/`DEFINE TEMPLATE` — the agent revising how it assembles its own context |
  | `plan_revision` | `grain:<workflow hash>` | `SUPERSEDE … WITH workflow` from ≤8 field-level edits — **rehearsed first**: when the substrate has journaled runs of the plan, the candidate is re-driven through the runtime's scheduler over them with every effect answered from the journal (`areev run shadow --plan-file`), and the report rides on the recommendation as `replay` (`areev loop show`, the console card); a `plan_replay` policy refuses to stamp it applicable when it is worse than the incumbent on the same runs or too many runs fall outside the journal's support |
  | `code_revision` | `tool:<name>` | §7.4's promotion grain, behind the Rule E1 evalset gate |
  | `skill` | `entity:<ns>/<skill-name>` | `ADD skill` — a reusable procedure (description, `when_to_use`, ordered steps) from a trajectory that succeeded; `SUPERSEDE … WITH skill` when a live skill of that name exists. Offered only under `skills.enabled` |
  | `plan` | `entity:<ns>/<plan-name>` | one batch: `ADD workflow` (steps bound to tools the evidence shows were called, edges with conditions in the runtime's frozen grammar — handed to the substrate's plan validator first) **and** `ADD skill` of the same name (the prose). `SUPERSEDE` both when a live pair of that name exists. Offered only under `plans.enabled` |
  | `consolidation` | `entity:<ns>/<subject>` | one batch: `ADD` the one lesson (carrying `consolidates: [hashes]`) and `SUPERSEDE` each member of the pile with a marker (`relation = "mg:lesson_consolidated"`), so the prompt holds one rule, not N copies; `rollback` retracts the markers and the line and every member is a head again. Offered only in answer to a `lesson_pile` finding, and `supersedes` must be exactly the live lessons that finding lists — the model cannot pick a pile of its own |

  A draft with no proposal — or one the engine cannot resolve — stays an
  advisory flag, exactly as every DISCOVER finding used to. What resolves
  becomes an *applicable*, rollbackable recommendation, with the exact change
  an apply would make shown in the review summary.

  **A lesson that restates a live one is marked.** `authored_dedup_key`
  collapses the same *text* proposed twice; it cannot see the same
  instruction in different words, which is exactly what a proposer emits,
  pass after pass, from the same recurring evidence. So at ROUTE an authored
  lesson is compared with every live lesson on its entity: by **cosine over
  the substrate's embedder** when one is installed (`Capabilities.embeddings`,
  threshold 0.90), else by **normalized token-set Jaccard** (threshold 0.60 —
  a weak floor, and honest about it; the record says which method spoke).
  Under the default policy, `near_duplicate: "flag"`, the draft still
  reaches the queue — the reviewer's call stays theirs — carrying
  `near_duplicate_of: [{hash, score, method}]`, its summary says
  NEAR-DUPLICATE and names the closest rule, and the console card shows the
  existing rule beside it. `near_duplicate: "suppress"` drops it before the
  queue and the funnel counts it as `dropped_near_duplicate`, beside
  `dropped_uncited` and `dropped_target`, so "the model contributed nothing"
  keeps its distinct causes. A `consolidation` is exempt — superseding the
  pile is its whole point.

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
  grains, drawn from five places that answer different questions: what the
  deterministic findings CITED (≤24 — what clustering already caught), recent
  tool ERRORS (≤16 — what clustering could have caught and did not), named
  **harness records** (≤6 — see below), human-authored **Observations** (≤8,
  taken before the rest), and recent facts (the remainder — the model's own
  lens).

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

  **The harness reserve is a deliberate carve-out, not an exception waiting to
  widen.** An all-namespace scan hides every `agent:` namespace: those hold the
  file's own grant Facts and its Tier-2 audit Observations, and an analyzer
  that swept them as ordinary memory once proposed tombstoning the grants —
  which locks every non-owner out of the file. That exclusion stays.

  But not everything the harness records is governance. A **fold summary** is
  the agent's own account of what a long run had worked out, written when its
  transcript outgrew the model's window ([`run.md`](run.md)); it sits in
  `agent:harness` because it is evidence about a run rather than memory the
  agent asserts. Invisible to the lens, it may as well not have been written —
  measured against a live model, the bundle came back empty and DISCOVER was
  never called. So the loop reads those rows by **explicit namespace** and by
  an explicit list of `observation_kind` values (`HARNESS_EVIDENCE_KINDS`,
  currently `fold_summary` alone), with its own small reserve. Adding a kind is
  a reviewed one-line decision; un-hiding `agent:*` is not on the table.

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

### Decision backend (optional)

A **decision** model takes a `state` plus named, typed questions and returns
probabilities — no text ([`decision-model-proposal.md`](decision-model-proposal.md)).
A host installs one with `Engine::with_decider(Box::new(LoopDecider(chain)))`
(`LoopDecider` is the `areev-loop-adapter` bridge over any
`areev_core::decide::DecisionBackend`; the loop crate itself only sees the
wire JSON through its own `DecideBackend` trait, so it keeps zero Areev
dependencies). Nothing is default-on, and it touches four places:

- **GROUND → VERIFY (E1)** — with an LLM attached and a **calibrated**
  backend, the LLM GROUND call is replaced by one request per draft: a `noul`
  per cited evidence grain (`ev_<bundle id>` — "does evidence item … contain
  the premise the recommendation relies on?") plus a `sound` `noul` ("Given
  only this evidence, is the recommendation sound?"), over `state =
  {recommendation: {summary, guidance}, evidence: [{id, grain_type, text}]}`.
  A draft is grounded when any cited grain reaches **0.75**. The LLM's
  adversarial keep/kill still runs; the routing number at the 0.75 floor is
  the decision's `p(sound)`, not the verifier's self-report. The
  recommendation records both — `confidence` is the decision,
  `llm_confidence` the self-report — so a reviewer sees them disagree.
- **Duplicate sweep (E2)** — observation pairs in one namespace with token
  Jaccard in `[0.5, jaccard)` that the ≥ `jaccard` rule left unclustered are
  asked "do these two state the same claim?" (16 pairs per request, at most
  **200 pairs per run**, `Engine::with_decider_pair_cap`). A calibrated
  `p ≥ 0.75` proposes the same supersede-into-the-earliest draft the Jaccard
  path does (`duplicate.judged`, the probability in the summary).
- **Contradiction sweep (E2)** — for a (namespace, subject, relation) OUTSIDE
  the functional set holding ≥ 2 distinct live values, each pair of values is
  asked "can both be true at the same time?" (same batching and cap). A
  calibrated `1 − p ≥ 0.75` proposes the same supersede-older draft as the
  seeded path, under `contradiction.judged`, whose summary says the relation
  was not seeded. It carries no recurrence metric: a relation nobody declared
  single-valued may legitimately gain values later.
- **Tool-failure cause (E3)** — with a backend installed, a `tool_failure`
  cluster names its majority cause (`tool_failure.cluster_cause`): a
  `failure_cause` in the closed vocabulary is used as recorded; free text (a
  `failure_cause` outside it, else `failure_detail` — settable from every
  surface: MCP `areev_record_tool_call`, CLI `record-tool-call
  --failure-detail`, Python `failure_detail=`, Node `failureDetail`) is classified once per
  distinct string per run with a `choice` over `timeout`, `executor_error`,
  `schema_validation_failed`, `user_aborted`, `context_overflow`, `unknown`,
  and the argmax is used at **≥ 0.6**; otherwise `unknown`. Without a backend
  the draft is exactly as before.

**Uncalibrated never omits.** When the backend (or a given response) is not
calibrated, no probability drops or proposes anything: GROUND/VERIFY run the
LLM exactly as without a backend, the sweeps propose nothing new (they do not
even ask), and free-text causes stay `unknown`.

**Fail-soft, fail-open.** A backend error or malformed answer is `LOP-E051`:
that stage's decision contribution is dropped for the run — GROUND falls back
to the LLM GROUND call for every draft, a sweep batch proposes nothing, a
cause stays `unknown` — and the run continues.

**Provenance.** Every recommendation a decision shaped carries `judged_by:
{backend, provider, model, calibrated, latency_ms, stage, answers}` (shown by
`areev loop show`), and the run result carries `decider: {backend,
calibrated, calls, failed_calls, last_error?}` beside `llm_funnel`. Both are
absent when no backend is installed, so a run without one reads exactly as
before. A replay never consults the backend.

**The four gates are unchanged.** A decision model scores; it never approves,
applies or rolls back. A recommendation carrying `judged_by` is **never
auto-applied**, whatever the policy grants and whatever its payload's shape —
it takes a human review with a BECAUSE plus an explicit apply, like any
`origin = llm` draft.

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
areev loop show <hash>   the review surface: proposal + action_kind (= the bindings' recommendation(hash))
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
db.loop_run(model="openai:gpt-4o-mini",     # the model leg (reflection AND grounding)
            base_url="http://127.0.0.1:4000/v1", key_env="TENANT_7_KEY")
#   through a gateway with a key named by variable — the CLI's --llm-base-url /
#   --llm-api-key-env and run_start's pair; with key_env given, the provider's
#   default variable (OPENAI_API_KEY) is never read
db.loop_run()   # the returned JSON carries `llm_funnel` when a backend is
#   attached: evidence → proposed → cited (with `dropped_uncited` and
#   `dropped_target` split out) → grounded → kept → stored (with
#   `dropped_near_duplicate` split out under `near_duplicate: "suppress"`).
#   "The model contributed nothing" has six causes that need opposite fixes
#   and all render as an empty queue; this is how you tell them apart.
db.recommendations('{"status":"pending"}')
#   rows carry hash/status/severity/analyzer/summary/target_ref/destructive,
#   plus `rollbackable` and `evalset_hash` — Rule E1's pin, so a reviewer can
#   see which gate a code or adapter revision will be held to BEFORE they
#   approve it (null on every other kind; the engine refuses a pin elsewhere)
#   — and `near_duplicate_of`, the live lessons an authored lesson restates
db.recommendations('{"status":"pending","include":"proposal"}')
#   the same rows plus `action_kind` and the flattened proposal (`proposal`
#   kind tag + `cal` / `format,base_digest,diff` / `data`), so a host gate can
#   measure a batch of proposals in one call; without `include` the row shape
#   is unchanged
db.recommendation(hash)   # the object `areev loop show <hash>` prints — the
#   review surface, proposal included (a hash prefix resolves). The proposal
#   is derived content governed by the same grants as the summary: the call is
#   coverage-filtered exactly like the listing (#312), so a recommendation
#   the principal does not cover answers "no recommendation matches", never a
#   denial that confirms it exists
db.apply_recommendation(hash, because="…")     # audited approve+apply
db.apply_recommendation(hash, because="…", gating_run="eval-…")  # a gated
#   (code/adapter) revision: evidence loads from the recorded eval summary,
#   and an ungated attempt refuses BEFORE the approval lands
db.dismiss_recommendation(hash, "…")           # audited reject
db.rollback_recommendation(hash, because="…")  # retract what an apply created
db.loop_replay('{"config": {"loop.staleness/1": {"severity_floor": "medium"}}, "window": "90d"}')
#   score a candidate config against the recorded past, beside the incumbent
#   (zero writes; the model and external analyzers reported `not_replayed`)
db.loop_outcomes()   # the Verify gate's held/regressed record; each row names
#   the run it compared against (`baseline_kind`, `baseline_run_id`) and, on an
#   evalset metric, `best_before` — the peak before the apply — plus, under a
#   policy cost bound, the `cost` read and the `held_costlier` verdict
# The tuning seam for hosts that train in-process (the CLI stays the paved road):
db.record_corpus_export(selector, destination, source_hashes=json.dumps([...]))
db.record_adapter(reply_json, manifest_hash, evalset_hash)
```

Node mirrors these as `recordToolCall`, `loopRun` (incl. `fullSweep` /
`policy`, and `baseUrl` / `keyEnv` as its last two parameters),
`recommendations`, `recommendation`, `applyRecommendation` (incl. `gatingRun`),
`dismissRecommendation`, `rollbackRecommendation`, `loopOutcomes`,
`loopReplay`, `recordCorpusExport`, and `recordAdapter`, plus the `actor`
constructor argument.

### MCP — two tools

`areev_loop` runs a pass and returns the pending queue (call it at session
start). `areev_recommendations` lists, or acts (`apply`/`approve`/`reject`
with a mandatory `because`). Launch a reviewer process and worker processes
with different `--scopes`/`--actor` so no agent can approve its own proposals.

### HTTP — `/api/loop/*`

`GET recommendations|health|analyzers` (reads) and `POST run|review|apply|
rollback|config|replay` (writes, though `replay` writes nothing — it is
guarded because it runs every analyzer over the whole history on request). `POST /api/loop/apply` takes an optional
`gating_run` — the `eval-…` run id a **code or adapter revision** requires;
the evidence is loaded server-side from the journaled `mg:eval_run` summary,
never from the request. The console's Areev Loop tab renders the queue with
severity dots, evidence, and approve/apply/reject actions gated behind a
mandatory reason; a gated recommendation (its row carries `evalset_hash`)
additionally asks for the gate run id before it will apply. The **Setup**
tab is writable — click an analyzer on/off to persist an enable/disable to
the file's config (`POST /api/loop/config`). Auto-apply is never grantable
from the console — only via a host policy file.

## Replay — score a configuration against the past

Every threshold in a policy file used to be a guess validated in
production: the only way to evaluate a change was to run it live and watch
the queue for weeks. The engine is a pure function of (file, policy, now)
— `run` never reads the clock, and the golden suite byte-pins queues
because of it — so a candidate configuration has a measurable quality on
the recorded past. `docs/loop-proposal.md` §17 named this rung 1 of the
escalation ladder: *explore in the past, not in production.*

```bash
areev loop replay --db agent.db --config candidate.json                 # per recorded pass
areev loop replay --db agent.db --config candidate.json --window 90d --step 1d
areev loop replay --db agent.db --config candidate.json --format json
```

`candidate.json` is a per-analyzer overlay in the shape the Setup view
saves — `{"config": {"loop.run_outcome/1": {"params": {"min_failure_ratio":
0.3}}, "loop.staleness/1": {"severity_floor": "medium"}}}` — plus an
optional `"policy"` to replay under (severity floors, the deny list,
`near_duplicate`, …; auto-apply grants are irrelevant, a replay applies
nothing). The report always carries two arms, **incumbent** (the file's
config under the host's policy) and **candidate**, so it is a comparison,
never a bare number:

- **Steps.** `--step per-pass` (default) steps `now` through the moments
  the loop actually ran, reconstructed from the audit trail (every stored
  finding's first transition is stamped with its pass's `now`; a pass that
  stored nothing left no trace and is not a step). `--step 1d` is a fixed
  stride from `--since <epoch-ms>` or `--window 90d`.
- **Prefix only.** Each step reads through a view that hides every grain
  created after that step's `now` — the paper's prefix rule, no leakage
  from the future.
- **State fidelity.** The watermark advances per step; a recorded
  rejection (or a measured revert) of a dedup key puts it on the same
  doubling cooldown a live pass would have; a rollback frees it; and the
  queue the rehearsal itself produced is what dedups its next step.
- **Zero writes.** The view refuses every mutating call by type and the
  engine holds an immutable borrow; the CLI additionally reads the op-log
  length before and after and prints it (`op-log 212 → 212 (unchanged)`),
  refusing the report if it moved.
- **The columns.** Per analyzer and in total: `findings`; the overlap with
  recorded decisions, matched by dedup key — `approved` (incl. applied and
  rolled back), `rejected`, `never_reviewed` (stored, still pending),
  `never_proposed` (the incumbent never produced it); the overlap with
  outcomes on the approved ones — `regressed`, `drifted`, `held`; and the
  queue volume per step. When the substrate can compute a content address
  without writing (Areev can), each would-be finding names the exact grain a
  live pass would have stored — the identity test pins that replaying the
  golden memory under its own config reproduces the golden queue byte for
  byte.
- **Scope, stated in the output.** `origin = llm` proposals and
  `origin = command` analyzers are not replayed — a model is not a pure
  function of the evidence and a command is out of process — and appear as
  `not_replayed` with the reason, as do the telemetry-fed analyzers (the
  rollups are not time-indexed). The deterministic rows are unaffected.

No auto-adoption: replay informs; adopting the configuration remains the
policy file or `POST /api/loop/config`, by a human. `POST /api/loop/replay`
takes the same request (`{config, policy, window | since_ms, step}`,
token-guarded like `/api/loop/config`), the console's Setup view has a
**Preview** beside each analyzer's On/Off that shows the would-be queue
delta before Save, and the bindings expose `loop_replay(request_json,
policy=…)` / `loopReplay(request, policy)`. MCP deliberately has no replay
tool: host policy is not client-controllable.

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
#   3c91e0a  evalset:9f…:passed     @1 run baseline 128 → current 133  [held]  baseline=newest_before_apply (eval-1041)  best_before 238
#   7d20b4c  evalset:9f…:passed     @1 run baseline 102 → current 104  [held_costlier]  baseline=newest_before_apply (eval-1042)  best_before 102  cost tokens 1000 → 1600 ×1.60 [breached, bound ×1.5]
```

The last row is an evalset-backed verdict: it names the run it compared
against (`baseline_run_id`, and `baseline_kind` says which rule picked it)
and carries `best_before`, the best value the field reached on any run
before the apply. That column is advisory and always present for an evalset
metric: a `held` against 128 with a `best_before` of 238 is the lost
opportunity the marginal comparison cannot see, shown so a reviewer sees it
even when no revert is proposed. The JSON form (`--format json`,
`GET /api/loop/outcomes`, `loop_outcomes()` in the bindings) carries the
same three fields; a verdict on a metric that is not evalset-backed, or made
with no run before the apply, says `baseline_kind: "snapshot"` — the number
the proposal froze — and names no run. Under a policy cost bound the row
also carries `cost` (`field`, `baseline`, `current`, `max_increase_ratio`,
`status` ∈ `within` / `breached` / `not_measurable`) and `current_run_id`;
`held_costlier` is the verdict when the score held and the cost did not.

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

`<field>` is read from the summary `areev eval run` journals. Four quality
names are promoted and work against any evalset — `passed`, `failed`,
`total`, `error_rate` (`failed/total`) — and five **cost** names beside them
— `effects` (executor calls the cases made), `tokens` (`input_tokens +
output_tokens`), `usd` (`usd_micros / 1e6`), `wall_ms`, and the derived
`cost_per_pass` (`usd / passed`, undefined when nothing passed — never a
division by zero, never zero); anything else is read from the summary your
harness wrote, e.g. `evalset:abc123:category_accuracy`. `areev eval run`
writes `effects` and `wall_ms` on every run and `input_tokens` /
`output_tokens` on the `--model` path from the provider's reported usage.

**A `--tool-cmd` grader can report its own field metrics and usage** (1.9.0,
#313). `areev eval run` names a scratch file in `$AREEV_EVAL_REPORT` beside
`$AREEV_EVALSET` and `$AREEV_EVAL_CASE`; the command may write one JSON
object there:

```json
{"metrics": {"unit_ok": 1, "period_ok": 0},
 "usage": {"input_tokens": 1200, "output_tokens": 300, "usd_micros": 900}}
```

Off stdout on purpose — `equals` / `contains` scoring reads stdout, and a
reserved trailer would change what every existing grader prints. Each
metric's **mean** across the cases that reported it lands in the summary as
`<name>` beside `<name>_n`, so a 0/1 field result reads as a ratio, which is
what `min_effect.points` assumes of a host-defined field; usage sums into
`input_tokens` / `output_tokens` / `usd_micros`. `outcome_evalset.field` can
then name a critical field directly instead of `passed`.

Read **fail-closed**, like the cost keys: a report that is present and
malformed FAILS the case with the reason, a non-integer usage value is an
error rather than a zero, and a metric name colliding with a key the verb
owns (`run_id`, `passed`, `failed`, `effects`, `wall_ms`, `input_tokens`,
`output_tokens`, `usd_micros`, `model`, `case_ns`) refuses the run before
anything is journaled. A command that writes nothing produces a summary
byte-identical to before.

**Where the cases live** (#314). `areev eval create|run --case-ns NS` puts
the evalset Fact and every case's Tool grain — its input and the executor's
output — in `NS`, while the `mg:eval_run` summary and the re-acceptance Fact
stay in `agent:harness` carrying only ids, counts and cost (plus `case_ns`,
so `areev run-trace --ns NS --run-id eval-…` finds the cases). Evaluation
cases are built from a firm's own confidential documents; without the flag
they went to the memory-wide harness, outside the grants, retention and
erasure of the namespace they came from. It is an explicit flag rather than
the global `--ns` so no existing invocation changes placement. This is the
split `areev run` already makes: effect grains in the run's namespace,
evidence ABOUT the run in the harness.
**A harness that journals its own runs must write `passed` and `failed` as
integer counts** (`1`/`0` for a single graded task): the reader is
fail-closed and drops a summary whose counts are missing or non-integer — a
boolean `passed` is not a count — so a run journaled that way is invisible
to the gate and no metric ever attaches. Measured: one benchmark harness did
exactly this for three runs and recorded zero verdicts
(`crates/areev-bench/PERSIST.md` §11 #24). The cost keys read the same way:
a summary that carries `input_tokens: "1200"` (a string) makes `tokens`
*not measurable* on that run — never zero. A cost key the summary does not
carry at all is read from the runtime: a harness that journals its evalset
run under the run id `areev run` executed writes no cost keys, and the gate
takes `spent_*` from that run's terminal `run_outcome` Observation — the
same grain the `run_outcome` analyzer attributes spend from, so a run that
is both quotes one number to both readers.

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
against day one (below). That is the default, `"baseline":
"newest_before_apply"`. The policy may instead say `"baseline":
"high_water"`: the baseline is then the **best** run journaled before the
apply (max for a higher-is-better field, min otherwise), so an agent that
fell from its own peak reads `regressed` even when the run just before the
apply was already down. It is a choice and not the default because it
confounds: on a noisy evalset, or in a deployment that does not journal a
run between applies, the whole fall from the peak is charged to whichever
rule was applied last, and the revert it proposes may retract a rule that
did nothing wrong. A revert applied under either baseline earns the same
doubling cooldown. Either way the receipt names the run it compared against
and carries `best_before`, so the peak is visible on a `held` as well.

**A cost bound beside the quality metric.** `"cost": {"field": "tokens",
"max_increase_ratio": 1.5}` on `outcome_evalset` reads a second column at
every checkpoint: the cost field on the baseline run and on the run after
the apply. The quality verdict is unchanged by it. When the score held but
the cost exceeded the bound, the checkpoint records **`held_costlier`** and
`outcome_review` emits an **advisory Flag** citing both runs — never a
revert draft, because a cost/quality trade is a human decision; a lesson
like *"exhaust every page of `search_customers` before acting"* can raise
accuracy two points while tripling tool calls, and until this it measured
`held`. `regressed` dominates — one verdict per checkpoint — and a revert
drafted on a run that also breached the bound names the cost delta. A cost
that is not measurable on either run (absent or malformed key) leaves the
verdict alone and says `not_measurable` on the record. `areev loop
outcomes`, `GET /api/loop/outcomes` and the console card show the quality
column and the cost column with the delta.
That is the whole "verify the change improved, otherwise revert" arc, on
the one kind of change a human approves from prose alone.

A revert's identity is the recommendation it retracts: two lessons on one
entity that both regress get two reverts (a deterministic finding's dedup
key is analyzer + target + action, and keyed that way the second revert was
dropped as a duplicate of the first until 2026-09-06). And by default the
verdict has **no noise floor**: any drop is a regression, so a 359 → 355
dip on 387 trials — within what one adapter read twice can differ by —
proposes a revert. The policy can set a **minimum effect size**,
`outcome_evalset.min_effect`: `{"count": 5}` (absolute, in the field's own
unit) or `{"points": 1.0}` (percentage points — scaled by the baseline run's
`total` for `passed`/`failed`/`total`, read as `p/100` for `error_rate` and
for a host-written field, which `points` assumes is a ratio in `0..1`; a
count-valued host field wants `count`). A worsening of at most the floor is
`held`, and the receipt records the floor it held under (`tolerance`), so a
`held` under a floor is distinguishable from a `held` at zero. Name it for
what it is: a **floor, not a significance test** — no p-value, no interval;
a builder who needs statistics has the run counts to compute them. The
floor is resolved once from the policy and travels with the measurement, so
the recorded verdict and `outcome_review`'s revert draft cannot disagree on
it, and `areev eval run --baseline RUN --tolerance N` (model-swap
re-acceptance) reads the same `is_regression` — one reader of "did it get
worse" for both edges.

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
journaled onto the timeline). A high-water mark carried forward is now a
policy choice, `outcome_evalset.baseline: "high_water"` — off by default,
because it charges the whole fall from the peak to the last rule applied,
which on a noisy evalset proposes reverts of rules that did not cause the
drop; a deployment that does not measure between applies and does not opt
in still sees a rising-then-falling agent as a rising one, though the
receipt now shows the peak (`best_before`) beside the `held`.

**Dedup is by content, not by meaning.** `authored_dedup_key` fingerprints
the proposal text, so it collapses a rule proposed twice verbatim. It cannot
collapse the same instruction rephrased — which is what an LLM proposer
emits, pass after pass, from the same recurring evidence. In that run ten
approved rules stated four distinct facts, each true and well-formed enough
that a reviewer approved it alone, and the agent stopped emitting the very
fields the rules most insistently named. Every rule in the prompt is a rule
competing for the model's attention; a reviewer judging one at a time cannot
see the pile. Two things now see it: a near-duplicate check at proposal time
(cosine with an embedder, token-set Jaccard without — `near_duplicate:
"flag" | "suppress"` in the policy), and the opt-in `lesson_pile` analyzer,
which flags an entity over its lesson budget with each member's latest
verdict and cues one consolidating lesson through the ordinary gates. What
neither does is judge whether the pile is *harmful* — that stays the Verify
gate's question, and the reviewer's.

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
    "checkpoints": [{ "after_runs": 1 }, { "after_ms": 604800000 }],
    "baseline": "newest_before_apply",
    "min_effect": { "count": 5 },
    "cost": { "field": "tokens", "max_increase_ratio": 1.5 }
  },
  "plan_replay": { "min_runs": 3, "require_no_worse": true, "max_out_of_support": 0.5 },
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

`plan_replay` (default none) is the pre-apply gate on a `plan_revision`:
`{"min_runs": 3, "require_no_worse": true, "max_out_of_support": 0.5}`.
When the substrate could rehearse the candidate against the live plan's
journaled runs (`docs/run.md`, "Verify and shadow"), the revision is stored
as advisory — never offered to apply — when it completes fewer of the same
runs than the incumbent did, or when more than the stated fraction of runs
could not be scored because the candidate asked for effects the journal
never recorded; the summary names the runs. Fewer than `min_runs` rehearsed
runs and the gate abstains (two runs are an anecdote). Dream-RSI's monotone
selection as a gate rather than an auto-deploy: applying stays human, with a
BECAUSE. Without the policy the rehearsal still rides on the card; nothing
is refused. The loop's rehearsal answers **every** effect from the journal:
it holds no executor, so a `plan_revision` is scored on the plan's shape —
edges, conditions, retries, cycle bounds — and never on a tool's code. A
candidate that rebinds a node to a different module is invisible to it and
rehearses as `same`; scoring that takes `areev run shadow --reexecute pure`
with the host's own executor pins (`docs/run.md`, "Rehearsing a candidate
version"), which is a human's command and not the loop's.

`near_duplicate` (default `flag`) decides what DISCOVER does with an
authored lesson that says, in other words, what a live lesson on the same
entity already says: `flag` queues it marked with `near_duplicate_of`,
`suppress` drops it before the queue and counts it in the funnel. Measured
need: on the ad-buy corpus ten approved rules stated four distinct facts.

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
zero verdicts across 78 governed runs. `baseline` picks the run the verdict
compares against: `newest_before_apply` (default — the marginal question,
never blames a rule for an earlier rule's drop) or `high_water` (the best
run before the apply — catches a fall from the peak, at the cost of
charging the whole fall to the last apply; see "What the gate does not
catch"). `min_effect` is the verdict's floor — `{"count": n}` in the
field's unit or `{"points": p}` of the pass rate — below which a dip is
`held` (default none: any drop regresses; see "Evalset-backed outcomes").
`cost` reads a cost field (`effects`, `tokens`, `usd`, `wall_ms`,
`cost_per_pass`, or a harness key) beside the quality field and records
`held_costlier` with an advisory Flag when the score held but the cost rose
past `max_increase_ratio` × the baseline run's (default none).
It exists because an authored lesson carries no recurrence metric —
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
