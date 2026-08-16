# Areev Loop — the explainer (slide source)

> Purpose: the narrative source for a slide deck explaining Areev Loop. **One H2 =
> one slide**: a headline, the bullets that go on the slide, a suggested
> visual, and speaker notes. Claims follow the discipline in
> `loop-proposal.md` §18 — mechanisms, not vibes; measured numbers only;
> competitors cited by *design* (their own docs/papers), never popularity.
> Landscape items dated 2026 come from the linked sources — re-verify before
> external use. Deep dives: [`loop.md`](loop.md) (user guide),
> [`loop-reflection.md`](loop-reflection.md) (the verified LLM path),
> [`loop-proposal.md`](loop-proposal.md) (design of record).

---

## 1. Title

**Areev Loop** — governed self-improvement for AI agents, built into Areev.

- Your agent's history → evidence-cited recommendations → human-governed
  changes → measured outcomes.
- Deterministic core (zero model calls). LLM optional — and verified when used.
- One binary, local-first, no daemon, works air-gapped.

*Visual: the word "Areev Loop" over the loop diagram from slide 5, greyed.*

*Speaker notes: Areev Loop ships inside Areev — the `areev loop` CLI family,
Python/Node methods, two MCP tools, `/api/loop/*`, and a tab in the web
console. It is not a separate install.*

---

## 2. The problem

**An agent that edits its own memory is an unreviewed production deploy that
happens continuously.**

- Every agent team wants an agent that learns from its own history: stop
  repeating the tool call that always fails, stop holding two contradictory
  beliefs, prune what went stale.
- Every team that tries hits the same wall: *What changed? Based on what
  evidence? Who approved it? Did it actually help? How do we undo it?*
- No memory product answers those five questions — so agent self-improvement
  stays a demo.

*Visual: five unanswered questions as red stamps over a "memory write" diff.*

*Speaker notes: this failure mode is documented in the field, not
hypothetical. A production team audited 10,134 entries their LLM-extraction
memory layer had accumulated in 32 days and found 97.8% junk — including 668
copies of one hallucination — and upgrading the extraction model didn't fix it
(mem0 issue #4573). Their conclusion: nothing should persist unless promoted —
which is a review gate, i.e., the thing Areev Loop makes native. Reflection loops
also carry real cost: background memory-rewriting agents have been reported at
up to 15× token burn (Letta sleep-time reviews).*

---

## 3. Why now

**The market just learned the ungoverned version of this feature is a
liability.**

- The big labs shipped background "dreaming" memory synthesis in mid-2026 —
  and drew immediate criticism for *reducing* user visibility and audit
  trails. The category is validated; the trust model is the open question.
- **EU AI Act Article 12** (record-keeping: reconstruct behavior, prove why a
  system acted) applies to high-risk systems from **August 2026** — audit-trail
  requirements with real penalties.
- Memory poisoning is a live attack class; published defenses call for
  provenance, validation before persistence, and rapid rollback — the exact
  shape of Areev Loop's gates.
- Users switching off ungoverned memory layers are switching *because of*
  junk accumulation and opacity — the mem0 audit above is the buyer's story.

*Visual: timeline of 2026 events converging on "governed" (lab launches →
backlash → regulation date).*

*Speaker notes: sources — OpenAI "Dreaming V3" memory update (June 2026) and
the audit-trail criticism it drew (TechTimes, June 5 2026); Anthropic Managed
Agents "Dreaming + Outcomes" (May 2026) — the closest big-lab analog, cloud-only;
EU AI Act Art. 12 (artificialintelligenceact.eu/article/12); MINJA-class
memory-injection research (>95% success against production agents,
christian-schneider.net). Timing claims are July-2026 web-sourced — re-verify
before external use.*

---

## 4. The bet

**Self-improvement is a governance problem before it is an intelligence
problem.**

- Make every change to the agent's memory a first-class object:
  **evidence-cited, reviewable, undoable, measured**.
- Then self-improvement stops being scary and becomes a habit.
- Corollary from the research: improvement is reliable exactly when an
  *external verifier* grades the change — and degrades when a model judges
  itself.

*Visual: "intelligence problem" crossed out, "governance problem" underneath.*

*Speaker notes: the research anchor (full ledger in `loop-reflection.md`):
self-judging planners hit 84% false-positive rates as their own verifiers
(Valmeekam et al.); GPT-4's GSM8K score drops 95.5 → 89.0 after unassisted
self-review (Huang et al., ICLR'24); verifier-anchored systems (Voyager,
DeepSeek-R1's rule-based rewards) compound. Areev Loop is built on that asymmetry.*

---

## 5. What Areev Loop is — the loop

**History in, governed improvements out.**

```
capture   tool calls, facts, events        record_tool_call / hooks / importers
  → analyze    eleven deterministic analyzers over typed grains
  → recommend  recommendation + cited evidence hashes, dedup'd
  → govern     four gates, hash-chained audit
  → apply      undoable supersession
  → measure    re-run the metric at 1d/7d/30d; regression → revert proposal
```

- The loop closes with **no LLM**.
- Runs on Areev's typed grains (facts, events, skills, tool calls) — never
  raw prose.
- Scope, stated plainly: Areev Loop improves the agent's **memory** — what it
  knows — not its prompt, code, or outputs.

*Visual: the six-stage loop as a cycle; "no LLM required" badge on it.*

*Speaker notes: the substrate matters — because Areev memories are immutable,
content-addressed, typed grains, analyzers compute over declared semantics
(`is_error` flags, supersession chains, `valid_to` windows), which is what
makes zero-LLM analysis possible and every citation stable forever. The engine
itself is substrate-agnostic (an `OmsSubstrate` trait); Areev is the shipped
adapter.*

---

## 6. The four gates

**Nothing changes behind your back.**

1. **Propose** — only structured recommendation objects enter the queue:
   versioned analyzer id, template-rendered summary, evidence hashes,
   severity. Analyzers cannot emit free prose.
2. **Review** — separation of duties; **mandatory reason** on every decision;
   self-approval blocked (creator ≠ approver).
3. **Apply** — scope-checked; destructive applies need `admin` +
   `allow_destructive`; every apply stores its inverse.
4. **Verify** — the metric is re-measured after apply; regression proposes a
   revert.

- The audit trail is *grains*: one immutable, hash-chained record per
  transition, with actor and reason. It syncs with the file and is queryable.
- Auto-apply is **off by default** — granted only by a host policy file, only
  for built-in analyzers, only non-destructive SUPERSEDE-shaped curation,
  never for LLM findings. Rejected proposals get a 7-day cooldown, so
  dismissing something once means it stays dismissed.

*Visual: four gates as a pipeline with a human icon at gate 2 and an undo
arrow at gate 3.*

*Speaker notes: how locked-down is "locked down"? In the shipped engine
exactly one analyzer (`duplicate_sweep`) is even *eligible* for auto-apply,
and only with an explicit host policy grant. The policy file rejects unknown
keys and is never stored in the memory file — a synced or stolen file can't
arrive pre-armed.*

---

## 7. The analyzers — hygiene, then utility

**Twelve built-in analyzers, all deterministic, ten on by default.**

| Hygiene (is memory correct?) | Utility (is memory used — does it help?) |
|---|---|
| `tool_failure` — recurring failure clusters → a lesson | `cold_grains` — facts never recalled |
| `duplicate_sweep` — exact + near dupes → consolidate | `coverage_gap` — questions that keep returning empty |
| `contradiction_sweep` — two live values, one relation | `budget_pressure` — context budget overflowing |
| `staleness` — past `valid_to` → forget (human-gated) | *(fed by the recall-telemetry sidecar)* |
| `fork_surfacing`, `skill_stall`, `goal_stagnation`, `outcome_review` | |

- The utility column is the differentiator: a disposable, never-syncing
  telemetry sidecar records what recall actually surfaced — so Areev Loop sees
  *whether memory earns its place*, not just whether it's internally clean.
- Telemetry capture is buffered and non-blocking: voice-loop recall stays
  ~82µs p50 with it on.
- Extendable without recompiling: `--analyzer-cmd 'CMD'` runs your own check
  (PII, house style, compliance) in any language — advisory-only, can never
  mutate memory.

*Visual: two-column table; sidecar drawn as a small satellite file feeding the
right column.*

*Speaker notes: staleness answers a pain users literally build plugins for —
memory layers with "no built-in mechanism for expiration or decay" (mem0
issue #5330 spawned a community forgetting-curve plugin). Here decay is a
governed, declared-`valid_to` sweep, not a bolt-on.*

---

## 8. Measured, not asserted

**Every quality claim has a harness.**

- Analyzer precision: **1.00 precision / 1.00 recall** on the labeled fixture
  (planted positives + look-alike decoys), for all seven fixture-covered
  default-on analyzers. CI fails if any default-on analyzer drops below
  **0.90**. (`cargo run -p areev-bench --bin loop_precision`)
- The verifier lifts the LLM path's **Effective Reliability from +0.00 to
  +1.00** on the reference corpus by filtering decoys
  (`loop_reflection` bench).
- Live: `areev loop` reports the **approval rate** of LLM findings from the
  audit chain — a field-quality signal that accrues with real use.

*Visual: the precision table screenshot or a bar chart; a CI-gate badge.*

*Speaker notes: honest caveat that belongs in the room — the fixture is a
synthetic floor, not a field number. It proves analyzers don't fire on
look-alikes; real-world precision accrues from the approval-rate metric. We
never headline numbers we didn't measure, and we never inherit vendor
benchmarks — that discipline is itself part of the pitch (the same system has
been publicly scored 58/66/75/84 on one benchmark depending on who held the
pen — see the LoCoMo dispute cited in `loop-reflection.md`).*

---

## 9. The LLM layer — verified, never trusted

**Determinism finds; the LLM extends; a verifier decides what you see.**

```
DISCOVER → GROUND → VERIFY → ROUTE → (human review)
```

- **DISCOVER** under an abstention-legitimate objective: "NOTHING TO REPORT"
  scores zero penalty, so the model isn't pushed to invent findings.
- **GROUND**: the finding's factual premises must be present in the cited,
  content-addressed grains (anti-fabrication) — genuine inference is allowed.
- **VERIFY**: an *independent call* judges soundness — the proposer never
  grades itself; grounding can even run on a different model
  (`--ground-model` / `--ground-cmd`). A confidence floor gates the queue.
- Survivors carry `origin = llm`: they reach the review queue, and can
  **never auto-apply**.
- Out of the box: `--model claude-sonnet`, `openai:gpt-5`, `ollama:llama3.1`,
  any OpenAI-compatible endpoint — or `--llm-cmd` for any subprocess. No
  backend configured → the stages are the identity; the deterministic loop is
  unchanged.

*Visual: pipeline with a skeptic icon at VERIFY; a dropped draft falling out
between GROUND and ROUTE.*

*Speaker notes: live validation (real model, gpt-4o-mini): seeded
`acme HQ = San Francisco` + `acme country = Germany` — no deterministic
analyzer can flag it (each fact is individually well-formed); the model
surfaces the geographic inconsistency, it grounds, verifies, and reaches the
queue. Seed a *consistent* corpus and the verifier rejects the model's vague
"potential inconsistency" — the run stores zero. Finds what determinism
misses; abstains when there's nothing.*

---

## 10. It proves its own advice — the Verify gate

**"Did it make a change" is the wrong question. "Did the change help" is the
product.**

```
$ areev loop outcomes
a6f8133  tool_error_recurrence  @1d    baseline 0 → current 0  [held]
a6f8133  tool_error_recurrence  @7d    baseline 0 → current 0  [held]
a6f8133  tool_error_recurrence  @30d   baseline 0 → current 2  [regressed] → revert proposed
```

- Recommendations that carry a metric are re-measured after apply on a
  **checkpoint schedule (1d / 7d / 30d)** — a fix that looks fine early and
  rots later is caught.
- Re-measurement is a typed read over subsequent history — no LLM, no
  guessing — recorded as a file-truth that syncs.
- **The honest boundary**: this works for internal, bounded, attributable
  outcomes (did that failure recur; does that duplicate still exist). It does
  *not* score open-ended world outcomes (was the post good) — those stay a
  monitored trend a human judges. Areev Loop improves the agent's *memory*, not
  its *outputs*.

*Visual: the terminal output above, with the @30d line highlighted.*

*Speaker notes: two metrics ship end-to-end — tool-error recurrence (did the
failure come back after the lesson?) and contradiction recurrence (did the
subject grow two live values again after the resolution?). The plumbing is
generic (`MetricSnapshot` + multi-horizon checkpoints); more kinds are
roadmap, and we say so rather than implying every apply is measured. What's
differentiated even now: measurement is attributed to a *specific change*,
not an aggregate dashboard — task-level graders elsewhere score the task,
not the change.*

---

## 11. Where it runs

**No daemon. No scheduler. No cloud.**

- **Triggers**: a cheap idempotent command with watermark gates
  (`--min-new 20 --min-new-errors 3 --if-stale 6h`) — Claude Code
  `SessionEnd` hook, cron, CI, an MCP call at session start.
- **CI gate**: `areev loop list --fail-on high` exits 2 when a pending
  high-severity recommendation exists — build-blocking memory review.
- **Surfaces**: `areev loop` CLI · Python/Node (`loop_run`,
  `recommendations`, `apply_recommendation`) · two MCP tools (reviewer and
  worker processes get different scopes, so no agent approves its own
  proposals) · `/api/loop/*` HTTP · the console's Areev Loop tab (queue,
  sessions, outcomes, setup — token-less UI is read-only).
- **Local-first**: one binary; everything works air-gapped; telemetry sidecar
  is encrypted under the file's key and never syncs.

*Visual: hub-and-spoke — one memory file in the middle; hooks/cron/CI/MCP/
console around it.*

*Speaker notes: parity note for honest Q&A — the host policy file attaches on
every run surface (CLI, bindings, `areev ui --policy`, `areev serve --mcp
--policy`); LLM reflection and external analyzers attach on the CLI and the
bindings, while the console/MCP "run" executes the deterministic engine (the
console says so on the page). Bindings carry the full lifecycle: run /
reflect / list / apply / dismiss / rollback / outcomes.*

---

## 12. The 60-second demo

**No agent, no API key, no waiting.**

```python
import areev, json
db = areev.Areev("proof.db", actor="user:me")

for _ in range(5): db.record_tool_call("stripe_refund", '{"error":"rate_limited"}', is_error=True)
for _ in range(2): db.record_tool_call("stripe_refund", '{"ok":true}', is_error=False)
db.add_fact("acme", "deploy_target", "us-east-1", 0.9)
db.add_fact("acme", "deploy_target", "eu-west-1", 0.9)

db.loop_run()
pending = json.loads(db.recommendations('{"status":"pending"}'))
for r in pending:
    print(r["severity"], r["summary"])
# → high  Tool "stripe_refund" failed 5 times (71% of calls): rate_limited
# → …     (contradiction: 2 live values for acme.deploy_target)

db.apply_recommendation(pending[0]["hash"], because="retries belong in the client")
db.dismiss_recommendation(pending[1]["hash"], "eu migration is in flight")
```

- Alternative from the CLI: `areev init --db demo.db --template demo` →
  `areev loop run` → `areev ui` (the Areev Loop tab shows the governed queue).
- Every action above left an immutable, hash-chained audit grain with an
  actor and a reason.

*Visual: live terminal or the console queue screenshot.*

---

## 13. The landscape — capability matrix

**Everyone is building the loop. Nobody else ships all seven properties.**

Columns: **proposes** improvements automatically · **verifies** proposals
against evidence · **governs** (human review workflow) · **measures**
outcomes after apply · **undo** · runs **LLM-free** · runs **local/embedded**.

| System | proposes | verifies | governs | measures | undo | LLM-free | local |
|---|---|---|---|---|---|---|---|
| **Areev Loop** | ✔ | ✔ | ✔ | ✔ | ✔ | ✔ | ✔ |
| Hermes Agent (Nous) | ✔ | — | ~ | — | ~ | — | ✔ |
| Mem0 | ~ | — | — | — | ~ | — | ✔ |
| Zep / Graphiti | ~ | ~ | — | — | ~ | — | ~ |
| Letta (sleep-time, context repos) | ✔ | — | — | — | ✔ | — | ✔ |
| LangMem | ✔ | — | — | — | — | — | ✔ |
| DSPy / GEPA (prompt optimization) | ✔ | ~ | — | ~ | ~ | — | ✔ |
| Evals platforms (LangSmith, Braintrust) | ✔ | ~ | ✔ | ~ | ✔ | — | — |
| Big-lab "dreaming" memory (2026) | ✔ | — | ~ | ~ | — | — | — |
| RELAI (verifiable continual learning) | ✔ | ✔ | ~ | ✔ | ~ | — | — |

- The defensible sentence: **the only agent-memory self-improvement layer
  that is governed, verified, and measured — and the only one that runs
  LLM-free and embedded.**

*Visual: the matrix itself, Areev Loop's row highlighted; or a 2×2 (x: proposes
automatically, y: governed+verified+measured) with Areev Loop alone top-right.*

*Speaker notes: ratings summarize each system's own docs/papers (ledger in
`loop-reflection.md` §3 + appendix; 2026 rows are web-sourced July 2026 —
re-verify before external use). Nearest neighbors, honestly stated: Anthropic's
Managed-Agents "Dreaming + Outcomes" proposes, optionally reviews, and
measures — but cloud-only, grader-based (scores tasks, not changes), no
rollback story, no evidence citation. RELAI verifies by *replaying* the
failure environment — genuinely stronger pre-apply validation than our
grounding, and on our roadmap — but it's a cloud enterprise platform with an
LLM in the loop. Letta's git-backed context repos are a real rollback
substrate but with no verification or governance workflow on top.*

---

## 14. The landscape — what we took from each

**The field's failures are design inputs, not talking points.**

| System | Mechanism | The lesson |
|---|---|---|
| Hermes Agent | learns skills/memory from experience, write-approval **off by default** | same loop, governed — the wedge |
| Reflexion / Self-Refine | verbal self-critique | self-judging degrades; verifier must be external |
| Mem0 | LLM picks ADD/UPDATE/**DELETE**/NOOP | destructive deletes lose info silently; we supersede, never delete |
| Zep / Graphiti | bi-temporal invalidation (expire, don't delete) | non-lossy is right — we add the governed loop on top |
| ACE | incremental playbook deltas | never let an LLM monolithically rewrite memory (their case: 18k tokens @ 66.7% → one rewrite → 122 tokens @ 57.1%) |
| GEPA | reflective evolution, Pareto pool | reflective non-weight improvement works; keep competing fixes |
| Voyager | self-verified executable skills | improvement compounds when an external check grades it |
| SEAL | self-generated finetuning | weight updates admit catastrophic forgetting; learn in the memory layer |

*Visual: pick 4 rows max for the slide; full table in the leave-behind.*

---

## 15. What Areev Loop does NOT do (say it before they ask)

**Scoped claims survive contact with smart audiences.**

- It improves the agent's **memory**, not its **outputs** — no claim that it
  "makes your agent better at tasks." Clean, current, contradiction-free
  memory is the mechanism; capability claims wait for the measurement.
- It does not write or rewrite your **system prompt, skills, or code**
  autonomously — prompt/instruction docs are versioned grains a human edits
  with diffs and rollback.
- Context injection is opt-in, not ambient: `recall-hook --with-loop` rides
  the pending queue into the injected context on Claude Code; every other
  host **pulls** (an MCP call, a binding query, the console). Areev Loop never
  inserts itself into a prompt you didn't wire it into.
- It is not "AI-powered" at the core — the deterministic layer is the
  product's floor; the LLM is an optional, bounded extension.
- The grounding checker is right ~3 in 4 on hard cases — a strong filter,
  not an oracle; the human gate stays.
- No scheduler, no daemon — it rides your hooks, cron, or CI by design.

*Visual: a "non-goals" checklist, each with a one-line why.*

---

## 16. Roadmap — shipped vs. next (ranked by user demand)

**Shipped (PR #20 + the follow-up pass, on main):** the engine + 12 analyzers ·
four gates + policy auto-apply (value-identity-checked, attachable to CLI /
console / MCP) · recall-telemetry sidecar + 3 utility analyzers ·
multi-horizon Verify gate (tool-error + contradiction recurrence) · LLM
reflection (DISCOVER→GROUND→VERIFY) with out-of-box providers · external
command analyzers (+ ready-to-run example) · full-memory sweep
(`areev loop reflect`) · `recall-hook --with-loop` context injection ·
console (queue / sessions / outcomes / writable setup) · CLI / Python / Node /
MCP / HTTP parity on the full lifecycle incl. rollback + outcomes · precision
+ Effective-Reliability benches.

**Next (ordered by what users ask for):**

1. **Pre-apply replay validation** — answer "will this change make my agent
   worse?" *before* apply by replaying recorded sessions against the proposed
   change; today Areev Loop verifies claims before and measures after.
2. **Substrate adapters beyond Areev** — the engine is substrate-agnostic
   (`OmsSubstrate`); adapters for existing stores/trace logs let today's
   mem0/Letta/LangGraph users run Areev Loop without migrating first
   (`areev migrate` remains the paved road in).
3. **Recommendation-aware recall everywhere** — the Claude Code hook path
   shipped (`recall-hook --with-loop` injects the pending queue); next is
   the general form: approved lessons and flags surfaced through ASSEMBLE for
   any host, without polling.
4. **Prompt/skill artifacts as governed apply targets** — the type system
   already models `doc:`/prompt targets; make instruction edits first-class,
   evaluated, undoable applies. Skill trajectories follow.
5. **More outcome metrics** — tool-error and contradiction recurrence ship;
   duplicate recurrence needs a supersede-by-existing substrate primitive
   first. Publish recommendation-quality numbers (precision, approval rate,
   regression rate) at corpus scale.
6. **Provenance / poisoning analyzers** — injection-pattern and trust-score
   sweeps; the audit substrate is already in place.
7. **Fleet rollups** — cross-file/cross-agent aggregation in the console.
8. **Migrate onto the OMS `0x0C` recommendation grain** — the native type now
   exists (OMS 1.5); moving the live queue onto it is a data migration.

*Visual: two-lane timeline, "shipped" lane visibly heavier; next lane
numbered.*

---

## 17. Close

**Self-improvement you can put in front of a compliance team.**

- Every recommendation cites content-addressed evidence.
- Every decision carries an actor and a written reason, hash-chained.
- Every apply is undoable; outcomes are re-measured on a schedule.
- Starts at zero model calls and zero dollars; scales to verified LLM
  reflection when you choose.
- `pip install areev` · `areev init` · `areev loop run` — the loop is
  running before the coffee is done.

*Visual: the five audit questions from slide 2, now all stamped green.*
