# Areev Loop — self-improving agents with governance and guardrails, on Areev

**Status**: proposal — for team review. **Date**: 2026-07-17. *(Historical:
written under the engine's original name; the text was updated when the
engine was renamed Areev Loop in v1.1.1.)*
**Product**: Areev Loop — the self-improvement layer of the Areev
agent backend.
**Tagline**: *"Areev Loop makes your agent wiser — self-improvement with
receipts."*

**How to read this**: §1–5 are the product (what we build and why it wins).
§6–14 are the design (governance, data model, analyzers, architecture,
extension / integration / configuration, spec). §15–19 are execution
(settled decisions, build order, growth path, claims discipline, open
questions). §2.5 and §15 record decisions that are settled, each with its
reason — reopen them with new evidence against the reason; §19 lists what
is genuinely open for this review. One breaking change ships with this
proposal: token-less `areev ui` becomes read-only (§5.7). Appendix A grounds
every "ships today" claim in the code, verified 2026-07-17.

---

## 1. The pitch

Every agent team eventually wants the same thing: an agent that learns from
its own history — stops repeating the tool call that fails every time, stops
holding two contradictory beliefs about the same subject, prunes what went
stale, sharpens its own instructions. And every team that tries it hits the
same wall: **an agent that edits its own memory and prompt is an unreviewed
production deploy that happens continuously.** What changed? Based on what
evidence? Who approved it? Did it actually help? How do we undo it? No
memory product on the market answers those questions, so agent
self-improvement stays a demo.

**Areev Loop's bet: self-improvement is a governance problem before it is an
intelligence problem.** Make every change to the agent's operating state a
first-class object — evidence-cited, reviewable, undoable, measured — and
self-improvement stops being scary and becomes a habit.

What we ship: **Areev + Areev Loop, the governed backend for self-improving
agents.** Bring your own agent framework; the backend is what persists and
improves. Four properties carry the product:

1. **Deterministic core; LLM optional.** The engine produces useful
   recommendations with zero model calls, because Areev memories are typed
   grains, not text blobs — analyzers compute over declared semantics
   (`Event.is_error`, Fact subject/relation/object, supersession chains,
   `valid_to`, `derived_from`), never over raw prose. Plug an LLM in
   (`--llm-cmd`) and recommendations get richer; the LLM can never gate,
   approve, or apply anything.
2. **Governance is native, not bolted on.** Every change passes four gates —
   propose, review, apply, verify (§2.3): separation of duties, mandatory
   reasons, hash-chained audit records, undoable applies, and a trust floor
   that no file config can weaken.
3. **Receipts everywhere.** Every recommendation cites the evidence grains
   it was computed from; every apply stores its inverse (or is labeled
   non-rollbackable up front); every outcome is re-measured against what
   actually happened afterward.
4. **A console in the box.** `areev ui` opens a local, zero-install workbench
   for the whole loop: create the backend, debug what the agent knew and
   did, review and apply improvements, watch measured outcomes. One binary,
   no build step, no cloud, works air-gapped.

Three design axes run through every surface:

- **Extension** — analyzers in three rungs (built-in Rust, external command
  in any language, statically linked private crates); pluggable embedder,
  LLM, and even storage substrate; a manifest contract that drives CLI,
  API, and console rendering from one source.
- **Flexibility** — every layer optional (no LLM → add an embedder → add an
  LLM), config layered file-vs-host under a hard precedence rule, no daemon
  to operate, per-analyzer parameters, host policy as one JSON file.
- **Easy integration** — hooks for Claude Code, library capture calls for
  any codebase, MCP tools for any MCP client, importers for existing memory
  products and tool logs, a documented HTTP API, Python/Node/Rust parity.

The loop in one line: **history data + action results + pending/past
recommendations ⇒ new recommendations** — governed on the way in, measured
on the way out.

## 2. Product definition

### 2.1 The agent backend — the thing users create

The unit a user creates, debugs, and manages is the **agent backend**: one
or more Areev memory files (one file per subject or user — Areev's core
invariant "one memory = one file": everything known about one subject lives
in a single file, which makes the file the unit of erasure, sync,
portability, and write parallelism) holding:

- **Memory grains** — facts, observations, entities, events;
  content-addressed, immutable, supersession-versioned.
- **Instruction docs** — system prompt / CLAUDE.md-style documents stored
  through Areev's shipped memory-tool adapter, which represents a document
  as a chain of superseding grains (`doc:` targets); every prompt edit is a
  diffable, revertible grain.
- **Recall configuration** — saved queries and templates (QueryRegistry),
  assembly budgets, embedding provenance. These live as *file-truths*:
  self-describing settings stored in the file's `meta` table that any host
  honors on open.
- **Skills** — OMS 0x0B grains carrying `proficiency` (aliases
  `common.confidence`) and `practice_count`; improvement only by supersession
  (OMS §28.8), so the chain *is* the learning history.
- **Event history** — captured tool calls, results, `is_error` flags,
  thread-indexed exchanges: the raw material for improvement.
- **Improvement state** — recommendation grains, audit chains, and
  `loop_config`/`loop_state` rows, all in-file — plus one deliberate
  exception, a disposable telemetry sidecar file (§8).

Everything in-file syncs, forks, and erases as a unit because it is one
file. The sidecar is local, rebuildable evidence that never syncs; it is
encrypted under the same key, so erasure covers it too (§8). The backend is
**state + retrieval + improvement loop** — deliberately not the agent
runtime.

### 2.2 The improvement loop

```
capture (events, results, docs)          — hooks, library calls, importers
  → analyze (deterministic, typed)       — T0/T1 analyzers (§8) over grain semantics
  → recommend (0x0C grain + evidence)    — dedup'd, template-rendered, cited (§7)
  → govern (review / policy auto-apply)  — four gates, audit grains
  → apply (undoable supersession)        — scope-checked at execution
  → measure (outcome review)             — metric re-run, revert on regression
  → recall (next-session context)        — approved lessons reach the prompt
```

The loop closes without any LLM. With `--llm-cmd` it gets richer
(DISCOVER/ENRICH stages, §9); with none it is still genuinely useful. That
asymmetry — deterministic value first, model value optional — is the core
product bet.

### 2.3 Governance: the four gates

Every change to the backend passes four gates:

1. **Propose** — only recommendation grains enter the queue. Each carries a
   versioned analyzer id + params snapshot, a deterministic
   template-rendered summary, bounded evidence hashes + a CAL query that
   regenerates the full evidence set, a severity, and a reproducible metric
   snapshot. Free prose from analyzers is impossible by construction.
2. **Review** — separation of duties (`write` grants neither `review` nor
   `apply`); a mandatory BECAUSE on every decision (a written reason —
   `BECAUSE` is the literal keyword of the review statement, §14);
   self-approval blocked fail-closed against the creating actor (§6.1);
   policy can pre-approve only structural, engine-verified, non-destructive
   changes to memory/query targets.
3. **Apply** — requires the `apply` scope plus every scope the payload
   itself needs, evaluated at execution time under the caller's live scopes
   (no privilege amplification, no time-of-check races); every apply
   records, at apply time, the inverse needed to undo it; destructive
   applies are triple-gated and marked non-rollbackable (§6.4).
4. **Verify** — after `review_after`, the stored metric query re-runs;
   changed/unchanged is recorded as facts; regressions propose revert.
   Approvals are accountable to measured history, not vibes.

The **audit trail is grains**: one immutable Observation per transition,
hash-chained per recommendation, carrying a host-asserted actor label, an
observer type, and the reviewer's BECAUSE. It syncs with the file, survives
forks, is tamper-evident (`chain_verified` in the UI), is queryable in CAL,
and is erased exactly when the file it governs is erased (§6.5).

**Guardrails** = a non-configurable trust floor (§6.3 has all seven items):
auto-apply can never touch free text, destruction, prompts, or LLM-drafted
content; analyzers execute read-only; no payload can amplify scopes; no
file can raise a cap the host set. The rule of one sentence: **"the file
selects and restricts; only the host grants."** A synced or hostile memory
file can never arrive pre-armed.

### 2.4 Non-goals (stated so the pitch stays honest)

- **Not an agent framework or runtime.** No planning, no tool execution, no
  orchestration. Bring LangChain/LangGraph, the OpenAI Agents SDK, Claude
  Code, or a bare loop; the backend persists and improves underneath.
- **Not output moderation.** "Guardrails" here govern *changes to the
  agent's backend* (memory, prompts, queries), never the agent's runtime
  outputs. We never claim content-safety filtering.
- **No autonomous prompt rewriting.** Prompt/instruction and host targets
  are never auto-applied — always human-reviewed (trust floor).
- **Not a hosted control plane.** Embedded-first; the hub syncs segments,
  it does not govern. No daemon, no scheduler anywhere.

### 2.5 Binding product decisions

1. Deterministic recommendations are the core product — genuinely useful
   with no LLM. LLM = optional enrichment, never required.
2. Recommendation targets, all in scope: memories (curation), saved
   queries/recall config, agent system prompt / instruction files, generic
   host artifacts.
3. Lifecycle: approval-or-auto-apply per policy; the engine stores
   lifecycle state + the full audit trail; the host enforces identity
   (scope model — no user model inside the engine).
4. We control the OMS spec (CC0) and may revise it — but deliberately
   spec-later, after real usage (§14).
5. The engine must be claimable as working with any OMS-compatible store
   (CAL + grains) — hence the separate-engine architecture (§10).
6. MindGryd/Areev (our company; "AreevAI"/"areevai" is its GitHub/npm org)
   build proprietary in-house analyzers on the OSS core; OSS users must be
   able to customize without forking.
7. Product goal: users create **self-improving agents with governance and
   guardrails**; the agent backend is the unit of creation, debugging, and
   management.
8. The console is a **first-class deliverable** — best-in-class
   out-of-the-box UI for creating, debugging, and managing the agent
   backend, not an admin afterthought. ("Best-in-class" is the internal
   bar, never a printed claim — §18.)
9. **Extension, flexibility, easy integration** are design axes on every
   surface: analyzers, capture, policy, UI, bindings, protocol.
10. Discipline: this scope launched with the six analyzers defined in §8
    (peer review later added the field-based `skill_stall` default-on and
    `goal_stagnation` opt-in — §8), no daemon/scheduler, no new dependencies,
    and no weakening of the trust floor. Growth is UI, governance ergonomics, and
    integration.

### 2.6 Naming: two words

User surfaces use exactly two words, everywhere. **Areev Loop** names the
engine and every surface it owns: the `areev loop` verb family, the
Areev Loop console tab, `/api/loop/*`, `--with-loop`,
`loop_config`/`loop_state`, `loop-policy.json`, the `areev-loop` crate.
**Recommendation** names the object — locked in by the OMS spec (the 0x0C
grain; the REVIEW/APPLY statements) and never abbreviated to "recs" or
"reco" on a user surface. The analysis pass needs no third word: users
*run the loop*. Descriptive flags stay descriptive: `--analyzer-cmd` for
bring-your-own analyzers, `--llm-cmd` for the optional LLM. Exempt: the
OMS spec's own vocabulary (its §24.2 names a "reflective" observer mode)
and engine-internal identifiers.

## 3. Baseline: what ships today vs. what this proposal builds

Areev 1.0.1 is published (crates.io, PyPI, npm) and public at
`AreevAI/areev`. Everything in the left column exists and is tested today
(Appendix A has the code grounding); the right column is this proposal.

| Ships today (Areev 1.0.1) | Built by this proposal |
|---|---|
| Immutable content-addressed grains, 11 OMS types, canonical serialization, supersession + tombstones, encryption at rest | Recommendation grain (OMS 0x0C), audit-grain lifecycle, rollback inverses |
| CAL query language incl. `EXPLAIN` with structured plans; saved queries (QueryRegistry); templates; ASSEMBLE with SML/TOON/Markdown/JSON renderers (SML and TOON are Areev's two compact, model-facing context formats) | `REVIEW` / `APPLY` statements (spec drafts), `recommendation` as an ADD-able type, review/apply scopes |
| Hybrid recall; pluggable EmbedBackend / CommandEmbed, RerankBackend, QueryExpander | Areev Loop engine crate: OmsSubstrate trait, reference substrate, Analyzer SDK, six analyzers, validate/dedup/store pipeline |
| Console `areev ui` (Memories / Graph / Query tabs, one embedded HTML file); HTTP API (`/api/cal`, stats, log, config, browse, grain, verify, segments) | Console pillars: Overview, Sessions, Areev Loop (Queue/Analyzers/Outcomes), Setup; `/api/loop/*`; **breaking auth change: token-less UI becomes read-only** |
| MCP server (6 tools); Python + Node bindings; ~24 CLI verbs incl. `migrate` (8 sources), `provenance`, `forks`/`merge`, `restore` | the `areev loop` verb family, `areev init`; MCP +2 tools; py/js `loop_run` + recommendation methods + `record_tool_call`; generic tool-log importer |
| Claude Code hooks: recall-hook (context in), capture-stop (tool calls + errors out as Event grains) | Third hook (SessionEnd → `areev loop run`); recall-hook `--with-loop` |
| Hub segment sync (multi-channel acceptance-tested); bench harnesses: latency, honesty, LoCoMo accuracy | `loop_precision` bench (fixture-measured analyzer precision); telemetry sidecar; host policy file |

## 4. Golden paths

### 4.1 Claude Code agent (ten minutes)

```bash
cargo install areev            # or: pip install areev / npm install @areev/areev

areev init --db agent.db --template coding-agent --framework claude-code
#   (new verb) seeds namespaces, an instruction doc, starter saved queries,
#   and default analyzer config, then PRINTS the hook lines to paste (with
#   the ABSOLUTE areev path baked in, so a venv/pipx binary off the hook
#   PATH doesn't fail silently) — areev never edits your Claude Code settings:
#     UserPromptSubmit → /abs/areev recall-hook …  (ships today; gains
#                                                  --with-loop)
#     Stop             → /abs/areev capture-stop   (ships today)
#     SessionEnd       → /abs/areev loop run --min-new 20 --min-new-errors 3 --quiet  (new)

# … work normally for a few sessions …

areev ui --db agent.db --token-env AREEV_TOKEN
#   Overview shows backend health; the Areev Loop tab shows the pending queue.
```

Set expectations so the wait doesn't read as breakage: **after one
session** the Sessions tab and Overview already show real captured
exchanges — same-day debug value; **after a few sessions** the watermark
clears and the first recommendations land. An impatient evaluator doesn't
have to wait at all — `areev init --template demo` (§4.5) or the 60-second
proof (§4.2) shows the full loop immediately on a sandbox file.

### 4.2 Library agent — the mem0/zep/letta switcher path

Switchers are library users with no Claude Code hooks, and the flagship
analyzer (tool-failure clustering) feeds on tool events — so capture is a
first-class library call, not a hook-only feature.

**The 60-second proof (docs and the README Areev Loop section open with this).**
The fastest honest path to a first recommendation needs no agent, no LLM,
and no waiting — a REPL and ~15 lines light up four of six analyzers
deterministically, so an evaluator converts in one sitting:

```python
import areev, json

db = areev.Areev("proof.db", actor="user:me")   # actor labels the audit chain

# tool-failure clustering: 5 failures + 2 successes for one tool trips n≥3, ≥40%
for _ in range(5):
    db.record_tool_call("stripe_refund", '{"error":"rate_limited"}', is_error=True)
for _ in range(2):
    db.record_tool_call("stripe_refund", '{"ok":true}', is_error=False)

# contradiction sweep: two live objects under a seeded-functional relation
db.add_fact("acme", "deploy_target", "us-east-1", 0.9)
db.add_fact("acme", "deploy_target", "eu-west-1", 0.9)

# duplicate sweep: a case-variant re-add of the same triple
db.add_fact("acme", "tier", "Enterprise", 0.9)
db.add_fact("acme", "tier", "enterprise", 0.9)

# staleness: a fact whose declared valid_to has already elapsed
db.add('{"subject":"promo","relation":"active","object":"true","valid_to":"2020-01-01T00:00:00Z"}')

health = db.loop_run()                            # explicit call: no min-new/if-stale gate
pending = json.loads(db.recommendations('{"status": "pending"}'))
for rec in pending:
    print(rec["severity"], rec["summary"])
```

Then in the agent's own tool loop and at session end, the same surface
closes the governed loop:

```python
db = areev.Areev("support-agent.db", actor="user:alice")

# in the agent's tool loop — one line per call; thread groups a session
db.record_tool_call("stripe_refund", result_json, is_error=True, thread="sess-42", input=args_json)

# at session end — gated so it no-ops cheaply off the file-truth watermark
health = db.loop_run(min_new=20, min_new_errors=3, if_stale="6h")

# review with judgment — never rubber-stamp
pending = json.loads(db.recommendations('{"status": "pending"}'))
db.apply_recommendation(pending[0]["hash"],        # audited approve+apply, undoable
                        because="rate-limit retries belong in the client")
db.dismiss_recommendation(pending[1]["hash"],
                          "those 8 failures were one expired key")
```

Binding governance semantics (§6.6), so the four gates hold in-process:
the constructor takes an `actor="…"` kwarg (default `user:local`) that
labels every audit grain; embedded callers are the local root of trust and
hold all scopes, the same posture as the CLI; `apply_recommendation(hash,
because=…)` is an **audited approve+apply** (two chained audit grains under
one reason — the mandatory BECAUSE is a required argument, not optional);
`dismiss_recommendation` is the audited `rejected` transition. Explicit
`loop_run()` calls apply **no** `min-new`/`if-stale` gating unless the
caller passes those params — gating is a hook/loop ergonomic, so an
evaluator's first bare call always runs. `record_tool_call` takes an
optional `thread=` (default: a stable per-process id) so the Sessions
timeline stays thread-structured.

Same surface in Node. All methods are new in this proposal, on the existing
FFI convention (scalars in, JSON strings out). `areev migrate` (8 sources)
plus a new generic tool-log importer (OpenAI-style tool-call JSONL) closes
the loop for history that predates Areev.

### 4.3 MCP-native and multi-agent

`areev mcp` gains two tools: `areev_loop_adapter` and `areev_recommendations`.
Docs ship a system-prompt line — *"At session start call areev_loop_adapter;
review pending recommendations before acting"* — and the supervisor
pattern: a reviewer agent holds `review`+`apply` scopes while worker agents
hold `write`, so no agent can approve its own proposals (§6.1).

### 4.4 Ops without a daemon

There is no scheduler anywhere in the product. A loop run is a cheap,
idempotent command that hosts trigger however they already trigger things
(hooks, cron, CI, MCP calls). The command **reports what it did** (the
run-outcome contract, §13) so cron mail, CI logs, and polling agents can
tell ran-and-proposed from a healthy no-op:

```bash
areev loop run --db agent.db --min-new 20 --min-new-errors 3 --if-stale 6h
#   cheap no-op off a file-truth watermark; --min-new-errors gates on NEW
#   is_error Events since the watermark, so the flagship analyzer's own
#   signal — not just total grain count — can wake a run. --format json
#   prints {outcome, skip_reason, proposed, deduped, duration_ms}; exit 0
#   on ran OR clean skip (cron never pages on a healthy no-op), 1 on error.

areev loop list --db agent.db --status pending --fail-on high --format json
#   CI gate: exit 0 = none match, 2 = pending high-severity exists
#   (build-blocking), 1 = error. JSON envelope is append-only like /api.
```

Time triggers are the OS's job, not ours — `areev loop schedule --print`
emits a correct, absolute-path snippet for the host's scheduler (there is
no `watch` verb, §15):

```bash
areev loop schedule --print --every 6h --os cron|launchd|systemd|windows
#   prints a crontab line / launchd plist / systemd timer+service pair /
#   schtasks command — print-only, never installs (the `hook claude-code`
#   posture). The Setup tab renders the same snippet per detected OS.
```

Two more triggers close common gaps without a daemon. After an import,
run in the same command so migrated duplicates become the first demo:

```bash
areev migrate --from mem0 export.json --db agent.db --loop   # analyze after import
```

And long-lived server agents that never end a session make the library the
timer — `loop_run()` is a microsecond watermark check when gated, so
calling it every turn is safe:

```python
db.loop_run(min_new=20, min_new_errors=3, if_stale="6h")   # idle tick / turn-end
```

### 4.5 The three-minute demo

The demo runs on a **seeded corpus anyone can reproduce**, not on
hand-built or waited-for data:

```bash
areev init --db demo.db --template demo    # a literal, reviewable CAL batch
areev loop run --db demo.db              # ~5 recommendations across analyzers
areev ui --db demo.db                      # console opens on a full queue
```

The `demo` template (a template-as-CAL constant like the others, §5.5)
plants exact + near-duplicate observations, two live objects under the
seeded-functional `deploy_target` relation, grains with an elapsed
`valid_to`, and ~6 failing tool Events for one tool (with two contrasting
successes) — so the first run fires the duplicate, contradiction,
staleness, and tool-failure analyzers at once. From there the scripted
beats: console Overview (health) → Queue → detail view (evidence chips →
provenance walk) → approve with BECAUSE → apply → undo → next session's
context contains the lesson → Outcomes shows the measurement. No API key
appears on screen. Close: *"Evidence for every recommendation. Undo for
every apply. Measurement for every outcome."*

The same seeded file doubles as a pinned integration test (all default-on
analyzers fire) and seeds the first `loop_precision` fixture (§8); its
literal CAL batch, shown under `areev init --dry-run`, is itself a
governance teaching moment. Two mechanics to settle when it is built:
whether CAL `ADD` can express typed Event grains with `is_error` (else the
template needs a small engine-side seeding shim), and whether one linear
batch can plant the two concurrent heads fork surfacing needs (if not, the
demo fires four analyzers, not five — still plenty).

Day-1 cold start is now covered three ways, so no persona waits: the
**demo template** yields recommendations in one command on a fresh install;
`areev migrate` makes an old memory layer's accumulated duplicates and
contradictions the first demo (the switcher who brings a store); and the
generic tool-log importer does the same from OpenAI-style JSONL (the team
who brings logs, not a memory product). The first `areev loop run` on any
file always emits the **Memory Health Report** (same content as the
Overview tab — one source). Time-to-first-recommendation is the activation
metric, computed locally from grain timestamps and shown in the Health
Report ("first recommendation: 2h 14m after creation"), **never
transmitted** — there is no phone-home.

Rule for every quickstart and recipe: **model judgment** — print evidence,
apply one recommendation, dismiss one with a reason. Never a rubber-stamp
loop.

## 5. The console — create, debug, manage

The console is where governance becomes tangible: the queue with literal
diffs, evidence chips, and mandatory reasons is the product's screenshot
moment. It is also our structural differentiator: memory competitors ship a
dashboard-as-a-SaaS; we ship a workbench inside the binary.

(This section is about surfaces. The machinery it renders — scope names,
the recommendation grain's fields like `proposal_edit` and `review_after`,
the lifecycle — is specified in §6–§7; forward references are marked.)

### 5.1 Quality bars (the internal "best-in-class" bar, made falsifiable)

- **Zero-install**: `areev ui agent.db` → the full console. No node_modules,
  no build step, no CDN, works air-gapped. Keep it absolute.
- **Honest by construction**: every screen shows measured data with
  provenance one click away; no invented percentages (suppressed under
  n=10); empty states teach the next step — the activation-ladder pattern:
  name the exact prerequisite and the command that satisfies it.
- **Fast on real files**: panels paged; target <100ms server time per panel
  on a 100k-grain file. The server is serial (one request per connection) —
  no endpoint may hold the loop; inline analyzer runs keep a ~5s budget,
  T0 built-ins only, and point at `areev loop run` for the rest.
- **Keyboard-first where it matters**: queue navigation j/k/Enter; deep
  links for every entity (`?tab=areev-loop&rec=<hash>`,
  `?tab=sessions&thread=<id>`).
- **Light + dark** via the existing CSS-var system; a11y basics: focus
  order, contrast on severity colors, all dynamic text escaped.

### 5.2 Architecture: a few embedded files, still no framework

Today's console is one 1362-line embedded HTML file. Three pillars don't
fit in one file that grows past 3000 lines, so it splits into a handful of
embedded static assets (`console.html` + ~4 JS modules + 1 CSS), each
`include_str!`-ed and served from a static route table. Still: no
framework, no build step, no external assets, vanilla JS, existing
components (jsonTree, grain drawer, chainView, CAL highlighter, force
graph) reused across pillars. Size budget ≤ ~250KB total embedded.

**The console speaks CAL.** New read panels are CAL statements against the
existing `POST /api/cal` (RECALL / ASSEMBLE / EXPLAIN / HISTORY are reads),
not new endpoints. New endpoints exist only where CAL cannot express the
operation (recommendation lifecycle, analyzer config). This keeps the server small
and makes every console view reproducible from the CLI — every panel gets a
"copy as CAL" affordance, which is a debugging feature in itself.

**XSS rule (absolute)**: every dynamic string crosses `esc()`/`textContent`
— no "engine-rendered, trusted" exemptions. Template args, manifest titles,
diff lines, and target refs all carry grain-derived attacker text; one
innerHTML miss in a console holding the apply token = silent approve+apply.
New components (diffView, timelineView, planView) build DOM via
createElement + textContent. Review gate: grep new render functions for
innerHTML concatenation of unescaped variables.

**One console, ever.** The console lives in areev-server and stays there
after the engine repo split (§10): `areev` depends on the `areev-loop` crate
(a first-party dependency — the dependency-light rule guards against
third-party deps), so every `areev` binary carries the engine and every
console build carries the Areev Loop tab. A backend that has never run the loop
costs nothing — no daemon, and `loop_config`/`loop_state` are lazily
created — and renders as activation-ladder empty states, never as errors.
The areev-loop repo ships no UI; its CI runs against the reference substrate
(§10), and third-party substrates embedding the engine build their own
surfaces on the documented engine API. The console is deliberately part
of Areev's substrate advantage.

### 5.3 Information architecture

Tabs: **Overview | Memories | Graph | Query | Sessions | Areev Loop | Setup**.
Pending-recommendation count chips on the Areev Loop tab.

| Tab | Pillar | Contents | Status |
|---|---|---|---|
| Overview | manage | Memory Health Report rendered: grain counts by type, verification breakdown, open heads/forks, staleness counts, activation ladder ("duplicate sweep — needs ≥50 grains, you have 12"), last loop run + watermark, pending recommendations, effective-policy panel (read-only) | new |
| Memories | debug | grain browser, inspector drawer, supersession chains; adds a "recommendations citing this grain" cross-link (CAL over `derived_from`) | ships today (+1 link) |
| Graph | debug | entity force-graph | ships today |
| Query | debug | CAL console + highlighter; gains Explain mode and context preview | ships today (+ §5.6 additions) |
| Sessions | debug | thread-indexed Event timeline: exchanges, tool calls, `is_error` badges, failure-signature chips linking to derived recommendations | new |
| Areev Loop | manage | sub-nav Queue \| Analyzers \| Outcomes + the detail view | new |
| Setup | create | live readiness checklist + editors + wiring snippets | new |

### 5.4 Manage pillar (the Areev Loop tab)

- **Queue**: table — severity dot, analyzer badge, origin badge
  (`command:` distinct; `llm:` amber), scheme-prefixed target linked to the
  grain drawer, age, evidence count, status chips; filters default to
  pending; keyboard j/k/Enter only. **No bulk approve** — one copy-pasted
  BECAUSE across 25 audit grains is an audit chain faithfully recording
  that nobody looked. If structural recommendations pile up, the empty
  state points at the host auto-apply policy instead.
- **Detail view — the screenshot moment**: fact header (severity, analyzer
  + params snapshot, deterministic summary, target, metric baseline) → the
  proposal rendered LITERALLY (CAL through the existing highlighter;
  `proposal_edit` via a ~60-line hand-rolled diffView; red banner on
  destructive) → LLM prose only inside a collapsed amber "Model-written —
  unverified" fence, escaped plain text, no links or markdown → evidence
  hash chips → grain drawer + provenance walk → **Dry run** (per-statement
  effects; UX-advisory — the server tracks no preview state) →
  approve/reject with mandatory BECAUSE → hash-chained audit timeline with
  a `chain_verified` check → outcome as FACTS (baseline→current values with
  timestamps; changed/unchanged — no causal "improved/regressed" chips at
  n=1; percentages suppressed under n=10).
- **Analyzers**: one card per analyzer, rendered from its manifest
  (publisher badge, tier, trust-class badge, enabled toggle, activation
  status, last-run watermark, run-now). The six built-ins get hand-written
  param panels; auto-generated forms from ParamSpec are deferred until a
  third-party analyzer actually renders in the console — but command
  analyzers already *appear* as cards with read-only params via the
  manifest, at zero form-generation cost.
- **Outcomes**: per-analyzer counts (proposed / approved / rejected /
  applied / rolled back; zeros shown as zeros), captioned "Measured history
  from audit records". No charts, no trust scores.
- **API** (new routes under `/api/loop/*`): GET recommendations; GET
  recommendation?hash (+audit, chain_verified); POST dryrun; POST review;
  POST apply; POST
  rollback; GET analyzers; POST analyzers (config); POST run (~5s inline
  budget, T0 built-ins only); GET health; GET outcomes. Plus the existing
  `GET /api/config` extended with areev-loop/policy reporting. `POST run`
  returns the same run-outcome shape as the CLI (§13) plus, when the inline
  budget truncates a large file,
  `{completed, analyzers_run, analyzers_skipped: [{id, reason:
  budget|tier|capability}], hint: "areev loop run"}` — the console renders
  a partial-run banner with that copyable CLI line. Deliberately **no
  202-plus-poll async job queue**: a job registry inside a serial,
  one-request-per-connection server is scheduler-state by the back door
  (§15). The honest split is API/MCP for light runs and status, CLI/cron
  for heavy runs.

### 5.5 Create pillar (`areev init` + Setup)

Two halves, one mental model — scaffolding is CLI, guidance is console,
both render the same checklist:

- **`areev init`** (new verb; the name is unclaimed in today's dispatch
  table) — creates a memory file and seeds a working backend:
  `areev init --db agent.db --template
  coding-agent|support-agent|blank|demo
  [--framework claude-code|openai|python|node] [--dry-run]`. Seeds
  namespaces, an instruction doc (memory-tool chain), starter saved
  queries, and `loop_config` rows for the default-on analyzers; then
  prints the wiring snippet for the chosen framework (hook lines /
  `record_tool_call` stanza / MCP config). The printed **hook lines embed
  the absolute `areev` binary path** and name the settings.json location for
  the chosen scope — a venv/`pipx` install whose binary is off Claude
  Code's hook PATH otherwise fails silently, leaving Event capture at zero
  with no signal. The `demo` template (§4.5) seeds a planted corpus so the
  loop is visible in one command with no agent history. **Templates are
  literal, reviewable CAL batches embedded as constants** — shown before
  apply with `--dry-run`, and documented as copy-paste docs pages. There is
  no pack format, no pack files, no new config language.
- **Setup tab** — a live checklist against the open file; each row = status
  + copyable command + inline action where safe (token-gated writes):
  namespaces present · instruction doc seeded (opens the doc editor) ·
  saved queries defined (opens the query editor) · capture wired (detected
  via Event-grain count; if zero, shows the per-framework snippet — and
  because zero can mean "not pasted" or "pasted but binary off PATH," the
  empty state names the absolute-path fix) · embeddings configured (from
  `meta`; shows the `--embed-cmd` recipe if absent) · analyzers enabled
  (links to Analyzers) · policy active (host-reported, read-only; shows a
  starter `loop-policy.json` to copy) · loop run scheduled (shows the
  per-OS `schedule --print` snippet; detects the last-run watermark).
  The top empty-state card offers the demo path (`areev init --db demo.db
  --template demo && areev ui --db demo.db`) so the console teaches with
  real data without seeding anything into the user's own file, and the
  Areev Loop-tab empty state shows the watermark math ("needs ≥20 new grains
  since last run; you have 3").
- **Instruction-doc editor**: the memory-tool chain rendered with
  chainView; edit = supersession with diff preview (reuses diffView); full
  history browsable; every save is an audited, revertible grain — the
  "your system prompt has git-grade history" moment.
- **Saved-query editor**: CAL editor with the existing highlighter, EXPLAIN
  preview, DEFINE QUERY on save (token-gated; saved-query-body rules apply
  — bodies stay read-only, no destructive ops). **Prerequisite fix**: wire
  the QueryRegistry hooks into AreevFacade first — today `RUN` of builtin
  saved queries fails with QueryNotFound on the embedded path (Appendix A);
  that gap becomes user-visible the moment a query editor exists.
- Explicit non-goal: the console creates and edits *backends*, not agents.
  No code generation, no framework project scaffolding beyond printed
  snippets.
- Scope note: `areev ui` serves one facade (with its mounts). Multi-file
  fleet management is deferred — the hub is sync, not a control plane;
  revisit on real multi-file demand.

### 5.6 Debug pillar (context X-ray + sessions)

The debugging story answers the three questions every agent developer
actually asks: *what did my agent know, why did it know that, and what did
it do?*

- **Explain mode** (Query tab): any RECALL/ASSEMBLE runs with EXPLAIN and
  renders the structured plan the executor already returns — stages,
  chosen indexes/strategies, candidate and budget information — side by
  side with the executed result. **Rule: render exactly what EXPLAIN
  returns; any gap between what developers need and what the plan carries
  is an engine work item (extend the plan payload), never UI fabrication.**
  No synthetic scores, no invented "relevance %".
- **Context preview** (Query tab): ASSEMBLE output rendered verbatim
  (escaped) in the format the model would receive (SML, TOON, Markdown, or
  JSON), with renderer-reported size/budget figures and a budget bar. This
  is "view source" for your agent's context — the recall path renders
  in-process today, so this displays real output, not a simulation.
- **Sessions tab**: thread-indexed timeline of captured Events (exchanges,
  tool calls, results, `is_error` badges) via CAL over the events
  namespace; failure-signature chips link to the tool-failure cluster and
  any recommendations derived from it (evidence-hash intersection); every
  event links into the grain drawer and provenance walk. Empty state
  teaches capture wiring (links to Setup).
- **Provenance everywhere** (mostly exists): grain drawer + chainView +
  `derived_from` walks, plus the new grain→recommendations cross-link — so
  you can go grain → recommendation → audit → outcome without leaving the
  console.
- All debug surfaces are **read-only CAL**, so they work token-less under
  the new auth model. Debugging never requires granting write.

The debug pillar consumes data that already exists (events, plans,
provenance); it creates no new analyzer or capture obligations.

### 5.7 Auth model (breaking change, ships with the first console increment)

Today, token-less `areev ui` exposes `POST /api/cal` with writes enabled and
destructive ops on — any local process could execute a proposal's CAL
directly and bypass the review queue entirely, which would void the whole
governance story. The fix is deliberate and load-bearing for everything in
this proposal:

- **Token-less `areev ui` = read-only.** CAL writes rejected server-side
  (statement classification via the existing validate path; a batch
  containing any write is a write); all Areev Loop mutations and Setup writes
  return 401 with a "restart with `--token-env`" hint. Overview, Memories,
  Graph, Query (reads), Sessions, Areev Loop (reads), and the Setup checklist
  with its copyable snippets stay fully functional; only Setup's inline
  write actions are token-gated.
- Token unlocks review + config + Setup writes; `--allow-apply` adds apply;
  `admin` + `allow_destructive_ops` gates destructive applies. The 401
  carries `WWW-Authenticate: Basic` so browsers prompt natively (any
  username, password = token; scripts use `Authorization: Bearer`) — the
  existing mechanism.
- The Origin check tightens to exact own-origin for token-less POSTs
  (today's check passes *any* loopback port, so every local dev server
  could POST).
- The console is honestly documented as ONE principal (single shared
  token); per-actor granularity comes from CLI/MCP `--actor` labels.
- Shipping mechanics: lands with console increment 1 in a minor release,
  with a prominent CHANGELOG entry and a one-time stderr notice at
  `areev ui` startup. Deliberately no opt-out flag — an opt-out would
  re-open the bypass this closes. Existing write callers add
  `--token-env VAR` (a one-line change); token-less API callers get 401s
  carrying the same hint.

### 5.8 Design process & size

- The "Areev" design file in Paper (our design tool; the console's design
  source of truth) gets artboards **before** console code — eight:
  Overview, Queue, Rec Detail, Analyzers, Outcomes, Sessions,
  Explain/Context preview, Setup.
- Size estimate: Areev Loop tab ~+850 lines; Overview ~150; Sessions ~200;
  Explain/preview ~250; Setup + editors ~350; asset-split overhead ~100 ≈
  **+1900 lines over today's 1362 → ~3300 lines across ~6 embedded files,
  ~180–220KB total** (inside the 250KB budget). Four new CSS vars
  (severity ×3, LLM amber).

## 6. Governance in depth

### 6.1 Scopes, RBAC, multi-agent safety

- Scope vocabulary: `read / write / review / apply / admin`. `admin` ⊇ all;
  **`write` ⊉ `review`; `write` ⊉ `apply`** — separation of duties. (We
  rejected the tempting shortcut "apply = write + payload scopes": it would
  let a write-scoped proposer apply its own recommendations.)
- APPLY requires `apply` PLUS every scope the payload itself needs,
  evaluated **at execution time under the caller's live scopes** — no
  privilege amplification, no time-of-check/time-of-use gap on authority.
- Self-approval block: fail-closed; enforced against the recommendation's
  **creating actor** — it bites on MCP/CLI surfaces with distinct
  `--actor` labels. The console is one principal (§5.7) and documented as
  such. **Scope the claim accordingly:** separation of duties is enforced on
  the CLI/MCP surfaces (distinct per-process actors); the console is
  single-principal, so two people sharing its token are indistinguishable and
  propose/approve separation is *not* enforced there. Do not claim
  "separation of duties" for the console UI — per-user tokens / SSO is a
  prerequisite for that, and a deferred item (§5.5 scope note).
- Creating-actor semantics: a deterministic analyzer run's recommendations
  are created by the *engine* — the propose transition records the
  engine/run identity, not the human who happened to invoke
  `areev loop run`. The block compares the approving actor to the creating
  actor, so it never locks a solo developer out of their own queue; it
  bites when an actor that *authored* a recommendation (an agent
  submitting drafts over MCP, or the LLM DISCOVER path) tries to approve
  it.
- The solo-developer case (the §4.1 golden path) is governed too, just not
  by separation of duties: with one human holding every scope, the value
  is the receipts — evidence on every recommendation, a written reason on
  every decision, a hash-chained audit trail, an undo for every apply —
  plus the trust floor, which binds no matter who approves.
- Hosts map their real RBAC onto scopes per connection; the engine stores
  no users; actor labels are host-asserted strings.
- How scopes are granted, per surface: the CLI is the local root of trust
  (whoever can run `areev` against the file holds all scopes — the same
  posture as every existing verb); an `areev mcp` process is launched with
  `--scopes` and `--actor`, so each MCP client holds exactly its process
  grant (the supervisor pattern = two MCP processes with different
  grants); the web console derives its tier from the token flags (§5.7).
  Scopes are per-connection host policy, never stored in the file.
- Quorum (N-of-M approval via Consensus 0x09 grains): the schema is
  forward-built — every approval is a vote grain — but **enforcement is
  deferred** until real multi-party deployments exist.
- We **rejected "earned autonomy"** (auto-apply unlocked by an analyzer's
  track record): it is reward-hackable (inflate early outcomes to unlock
  autonomy) and statistically meaningless at single-digit n. Autonomy is
  an explicit host grant, below.

### 6.2 Policy-as-code

Host grants are three-dimensional (analyzer × target class × severity), and
CLI flags encode that badly. So the grant gets an artifact:

- **One optional host-side JSON policy file**: `areev … --policy
  loop-policy.json` (env `AREEV_LOOP_POLICY`). serde_json only; no new deps;
  deliberately one flat JSON file, not a multi-file config format (§15).
- Contents (all default-closed): auto-apply grants
  `[{analyzer, targets: [memory|query], max_severity}]`; analyzer
  deny-list; per-analyzer severity floors; telemetry mode. **Explicitly
  excluded**: anything that registers executables (`--analyzer-cmd` /
  `--llm-cmd` stay CLI-only — a stolen or committed policy file must be
  inert), anything on the trust floor (those fields don't exist in *any*
  schema), anything that raises engine ceilings.
- **Precedence**: engine ceilings > host CLI flags > policy file >
  memory-file config. The one-sentence rule is unchanged by this: *"the
  file selects and restricts; only the host grants"* — the policy file
  counts as host.
- Why it earns its existence: the policy file is **reviewable governance**.
  It lives in the user's repo, changes to agent autonomy go through code
  review and git history, and the console renders effective policy
  read-only (§5.3). This is the governance pitch made concrete, at ~1–2
  days of cost.
- Public nouns are exactly three: **analyzer, recommendation, policy**.
  (We considered and rejected a fourth — a named, multi-instance
  "reflector" configuration object wrapping analyzers — see §15.)

### 6.3 Auto-apply and the trust floor

Auto-apply preconditions (ALL must hold):

1. host/process opt-in (default off — same posture as
   `allow_destructive_ops`), AND
2. the analyzer is allowlisted by host policy for this target class and
   severity, AND
3. the target class is eligible — **memory/query only; prompt and external
   targets never**, AND
4. the payload is non-destructive, AND
5. **engine-side per-draft shape verification** passes: the engine
   re-checks the structural claim itself (re-hash both grains to prove
   exact-duplicate supersede equality; re-verify the shared parent for a
   fork merge). An analyzer manifest's self-declared "structural curation"
   flag alone is NEVER trusted.

Anything failing verification silently downgrades to pending review.

**Trust floor — not configurable anywhere** (the fields simply don't exist
in any file or policy schema; unknown keys are rejected at load):

- free-text auto-apply block (no auto-apply for payloads carrying
  evidence-derived prose)
- destructive gating (§6.4)
- self-approval block
- llm-origin restrictions (never auto-apply; never prompt/host targets)
- read-only evidence execution (analyzers cannot write)
- no scope amplification
- engine ceilings (file/policy may lower caps, never raise them)

The threat that shapes this floor: **the deterministic path can launder
attacker text.** Tool-failure clustering derives an error pattern from
attacker-controlled tool output; if a recommendation embedding that pattern
auto-applied, memory would be poisoned with no LLM and no human in the
loop. Hence auto-apply is restricted to structural curation with ZERO
attacker-influenced free text; recommendations that introduce
evidence-derived text are always approval-required; and the approval UI
shows the literal diff with untrusted prose fenced (§5.4).

### 6.4 Destructive proposals

Proposals MAY contain FORGET — staleness sweeps need tombstones. A
write-time validator stamps such a recommendation `destructive=true`.
Destructive recommendations: never auto-apply; require prior explicit
approval + `admin` scope + `allow_destructive_ops` at apply time; are
`rollbackable=false` (FORGET has no inverse). Documented fallback for
non-rollbackable mistakes: the shipped `restore --until-hlc` verb rewinds
the whole file to a hybrid-logical-clock timestamp (admin-gated, audited).
The destructive surface does not widen beyond single-grain FORGET — that
is a standing Areev invariant, and Areev Loop inherits it.

### 6.5 Audit & compliance surface

Audit chains are grains, so they are already queryable in CAL and travel
with existing bundle/segment machinery. The only sugar planned (deferred to
a post-1.0 minor): `areev loop audit --export jsonl` for compliance
pipelines. The console shows `chain_verified` per recommendation (§5.4).
There is deliberately no separate retention machinery: audit grains live
and die with the file they govern — erasing a subject's file erases its
audit, which is the correct GDPR-shaped behavior and costs nothing.

### 6.6 Library / embedded governance semantics

The bindings are a full governance surface, not a bypass — the four gates
hold in-process exactly as they do over CLI/MCP:

- **Actor labels.** `Areev(path, actor="…")` takes an actor kwarg
  (default `user:local`) that stamps every audit grain. Without it, a
  production team's whole audit chain records one anonymous actor — the
  accountability spine hollowed out for the very persona (§4.2) that needs
  it most. Node's constructor takes the same kwarg.
- **Scopes.** An embedded caller is the local root of trust and holds all
  scopes — the same posture as every existing `areev` verb against the file.
  Separation of duties is a multi-process property (the supervisor pattern,
  §4.3), enforced where actors are distinct; a solo in-process caller is
  governed by receipts (evidence, reasons, audit, undo), not by SoD.
- **Apply carries its reason.** `apply_recommendation(hash, because=…)` is
  an **audited approve+apply**: it writes the approve transition and the
  apply transition as two chained audit grains under one mandatory reason.
  The `because` argument is required — the binding cannot rubber-stamp a
  `pending → applied` jump without it (the §7.2 state machine reserves the
  reasonless `pending → applied` edge for `actor=policy` auto-apply alone).
  `dismiss_recommendation(hash, why)` is the audited `rejected` transition
  (the library-friendly name for `areev loop reject`).
- **No gating on explicit calls.** A bare `loop_run()` applies no
  `min-new`/`if-stale` watermark gate — gating is a hook/loop ergonomic
  passed explicitly (`loop_run(min_new=…, if_stale=…)`), so an
  evaluator's first call always runs instead of silently no-opping.

## 7. Data model

### 7.1 The recommendation grain (OMS type 0x0C)

A new standard grain type from the OMS 0x0C–0xEF "future standard types"
reserve. Precedent: Skill 0x0B was realized from the same reserve in OMS
1.4; additive type bytes + optional fields are address-safe per OMS §4.5
(omit-absent) and the 1.4 changelog's on-record statement that existing
content addresses remain valid.

Fields (beyond the OMS §6.1 common fields — `confidence`, `importance`,
`derived_from`, `valid_to` are reused, never duplicated):

| Field | Req | Purpose |
|---|---|---|
| `target_ref` | MUST | `<scheme>:<opaque>`; schemes: `grain:sha256:<h>`, `entity:<ns>/<subject>`, `query:<ns>/<name>`, `template:<ns>/<name>`, `doc:<host-id>` (e.g. `doc:claude.md`), `host:<opaque>`. The scheme IS the target-kind discriminator. |
| `analyzer` | MUST | Versioned logic id `publisher.name/major` (e.g. `loop.duplicate_sweep/1`) + an inline params snapshot — full "why" provenance with no extra grain type. |
| `summary` | MUST | Deterministic template-rendered one-liner `(template_id, args)`. Analyzers can never emit free prose. |
| `dedup_key` | MUST | Engine-computed, never settable. Identity = `(analyzer-family, target_ref, action-kind)`, NFC/case-fold normalized. **`analyzer-family` = `publisher.name` *excluding* the `/major` version** — so bumping `loop.duplicate_sweep/1` → `/2` does not re-propose the whole queue as "novel" (the same failure §15 rejects for content-hashing); the grain's `analyzer` field still records the exact proposing version + params. Deliberately **excludes** proposal content and evidence: content-hashing defeats dedup — one new duplicate joining a cluster would change the proposal and re-propose it as "novel" forever. |
| `proposal_*` | one-of MUST | `proposal_cal` (a batch of CAL Tier-1 "evolve" writes — ADD/SUPERSEDE, the standard non-destructive write tier; MAY additionally contain FORGET — §6.4), `proposal_edit` (`{format, base_digest, diff}` for doc targets; `base_digest` enables a staleness check at apply), `proposal_data` (opaque map for host targets). |
| metric snapshot | SHOULD | `{metric, baseline, unit, n, window, query(CAL), review_after}` — reproducible by construction; powers outcome review. |
| `severity` | SHOULD | info / low / medium / high. |

Evidence = `derived_from` (the existing OMS provenance array — subtree
traversal per OMS §23.6 and the shipped `areev provenance` verb work for
free). Bounded at ≤64 representative hashes, plus an optional
`evidence_query` (CAL) that regenerates the full set.

### 7.2 Lifecycle

- **Authoritative record** = one immutable **audit Observation grain per
  transition**, hash-chained per recommendation
  (`derived_from = [rec_hash, previous_audit_hash, ...result_hashes]`),
  carrying a host-asserted actor label (`user:alice`, `agent:worker-3`,
  `policy:auto`), `observer_type ∈ human|agent|policy|system`, and a
  mandatory BECAUSE (≤500 chars). Portable, tamper-evident, syncs with the
  file, fork-mergeable.
- **`rec_status` = a rebuildable index-layer cache**, the same posture OMS
  §5.6 gives `superseded_by` and `verification_status`. The
  recommendation's hash stays **stable for its whole life** — a host holds
  one id from propose to rollback.
- Content changes (evidence refresh, a growing cluster) = supersession of
  the recommendation grain (dedup_key constant across the chain,
  validated). **Lifecycle ≠ content.**
- State machine: `pending → approved | rejected`; `approved → applied`;
  `applied → rolled_back`; `pending → applied` **only** with
  `actor=policy` (auto-apply); `expired` is computed from `valid_to`,
  never by a daemon. Rejections start doubling cooldowns keyed on
  dedup_key (engine-local, rebuildable).
- Rollback: the inverse is derived **at apply time** and stored on the
  applied record as a store-op plan (not CAL — no new syntax needed):
  SUPERSEDE → re-instate prior content; ADD → **tombstone the added grain**
  so it truly leaves the recall path (the implementation uses `forget`, an
  index-layer removal; a `verification_status=retracted` marker was rejected
  because retracted grains are only *demoted* in recall, not excluded, so it
  would not be a behaviorally real undo); DEFINE QUERY → restore the prior
  body; doc edit → supersede back to the prior head. **FORGET has no
  inverse** → `rollbackable=false`.
- Lifecycle fields can never be written via CAL (`SUPERSEDE … SET` on them
  is rejected) — plain `write` must not be able to forge `approved`.
  Transitions happen only through REVIEW/APPLY/engine API.

### 7.3 Compatibility: mixed fleets and version bumps

Areev Loop writes a new grain type (0x0C) and new file tables into files that
older Areev binaries may still read after a hub sync — so mixed-version
behavior is brand-critical for a trust product and must be verified, not
assumed:

- **Old reader, new grain.** OMS §4.5's "existing content addresses remain
  valid" is an *address*-stability claim, not a *reader* claim. Before any
  areev-loop release, a test must confirm that shipped Areev 1.0.x **skips**
  unknown grain type 0x0C on read rather than erroring — otherwise the
  first loop-touched file synced to a not-yet-upgraded host breaks recall
  for that whole subject (and "one memory = one file" makes the blast
  radius the entire subject). If 1.0.x errors, the posture is a
  `min_reader_version` file-truth stamped only when the first 0x0C grain
  lands, so an old host refuses cleanly with a message instead of
  corrupting reads.
- **Config tables.** `loop_config`/`loop_state` carry a schema-version
  row; older binaries ignore unknown keys, newer binaries default absent
  ones — the lazily-created, open-unchanged posture already in §13.
- **Analyzer version bumps.** Because `analyzer-family` excludes major
  (§7.1), an analyzer upgrade preserves open recommendations, cooldowns,
  and outcome history. Recommendations proposed by an uninstalled or older
  analyzer stay reviewable and applicable — apply executes the stored
  proposal, not the analyzer — so upgrading or removing an analyzer never
  strands its queue.

## 8. Analyzers

**Tier ladder**: T0 = pure statistics/counting/graph over typed grains
(zero models, always available). T1 = the same analyzers upgraded with
embedding space when an EmbedBackend is installed (an embedder is not an
LLM). T2 = LLM enrichment (§9), never required. The OMS type system is the
interpretation layer — analyzers compute over declared semantics
(`Tool.is_error`, Fact s/r/o, Skill `proficiency`/`practice_count`,
`valid_to`, `superseded_by`, `derived_from`), never raw text. This is why no-LLM
analysis works here and text-blob memory products cannot do it.

**The initial set (six; default-on pending precision measurement):**

1. **Tool failure clustering** (T0) — Event grains from capture
   (tool_use/tool_result + `is_error`), grouped by (tool_name, normalized
   error signature: first ~80 chars, digits/paths stripped), 30-day
   window; fires at n≥3–5 AND ≥40% of that tool's calls. Emits
   instruction/memory recommendations.
2. **Duplicate sweep** (T0 + T1) — exact triple duplicates after
   NFC + case-fold; observation near-duplicates via token-set Jaccard
   ≥0.9; with embeddings: cosine ≥0.95 connected components → a
   consolidation grain superseding the members
   (`derived_from` = the members).
3. **Contradiction sweep** (T0) — learns functional relations from the
   file itself (single-valued for ≥80% of subjects, min 5 subjects), flags
   subjects with ≥2 live objects under a functional relation. Ships with a
   seeded list of known-functional relations from the built-in `mg:`
   relation vocabulary (`lives_in`, `deploy_target`, …) so it fires on
   day 1 without the cold-start learner.
4. **Fork surfacing** (T0; requires the `forks` capability) — entities
   with >1 head from the heads table, ranked; proposes a merge_heads plan.
5. **Staleness** (T0) — declared `valid_to` elapsed, only ("expiry you
   declared" — the honest framing). A soft tier (low confidence + old +
   never recalled) is deferred and will be opt-in.
6. **Outcome review** (T0) — for applied recommendations past
   `review_after`: re-run the stored metric query, record
   changed/unchanged with values, propose revert on regression. Closes the
   honesty loop; makes approve and auto-apply accountable.

**Telemetry-fed (now built).** The recall-telemetry sidecar shipped, and with
it three of the deferred analyzers: `cold_grains` (stored but never recalled),
`coverage_gap` (recurring questions with no matching memory), and
`budget_pressure` (assembly budgets overflowing — opt-in until the ASSEMBLE
overflow signal is wired). `cold_grains`/`coverage_gap` are default-on at 1.00
fixture precision. **Still deferred**: dead queries (needs a saved-query
execution registry) and coverage-gap *clustering* (the T1 embedding-grouped
form; the exact-question form ships now). **Evaluated and cut**: fact churn
(facts superseded unusually often) — no reliable deterministic signal.

**Added post-review (peer review Q2/Q3).** Two grain-type analyzers were
reconsidered against their **field-based** (not event-based) form and shipped:
`skill_stall` (a Skill practiced ≥N times whose `proficiency` stays low —
computed from the grain's own fields, no telemetry; **default-on**, 1.00 on
the precision fixture) and `goal_stagnation` (an active Goal with low
`progress` gone stale — **default-off / opt-in**, because "stalled" vs.
legitimate long-running work is genuinely ambiguous). Both are advisory
(`Flag`) and never auto-apply. The earlier cut evaluated only the weaker
event-based forms ("failure counts trend upward" / "no progress events");
the field-based forms are cleaner. With the three telemetry-fed analyzers this
makes **eleven built-in analyzers (ten default-on**; goal-stagnation opt-in);
`skill_stall` is the first that distills a *capability*
signal, and `cold_grains`/`coverage_gap` are the first that measure *utility*
rather than pure memory hygiene.

**Precision rule (brand-critical)**: NO invented precision percentages
anywhere — design docs, spec, marketing. areev-bench gains
`loop_precision`: fixture = a corpus bundle + optional telemetry.jsonl +
labels.jsonl (`{dedup_key, expected}`); scores per analyzer; **measured
numbers decide the default-on set**, published RESULTS.md-style. The rule
applies identically to built-ins, command analyzers, and linked crates:
*no analyzer ships default-on without a fixture run — yours included.*

**Telemetry sidecar** `<file>.telemetry.db`: encrypted under the SAME key
as the main file, so crypto-erasure covers it (a plaintext sidecar holding
query text, vectors, and top-hits would outlive erased grains). Recall-path
writes are buffered and non-blocking with a bench gate — nothing lands
inside the benched recall (~136µs) and 50ms voice-cadence latency budgets
(the areev-bench gates that keep Areev viable inside real-time voice
loops). FORGET synchronously scrubs referencing telemetry rows. Placement
rule: **anything that changes engine behavior on another host is a
file-truth** (watermarks, cooldowns, run-state live in the file); the
sidecar holds only high-volume disposable evidence (recall-log ring
90d/64MiB, grain-access rollups). It never syncs — hub segments carry the
memory file only — and it is rebuildable, so losing it costs evidence
detail, never state. Mode is host-only: `off | aggregate (default) |
full`.

**Runtime posture**: `areev loop run` is a batch verb outside every latency
gate, but it must stay interactive. Design intent: analyzers scan
incrementally off the `loop_state` watermark (only grains since the
last run seed candidate generation); duplicate/contradiction candidates
are blocked by entity and type before any pairwise comparison — never
all-pairs over the whole file; per-analyzer time budgets ride on the
manifest's cadence class. Target: seconds on a 100k-grain file, with the
precision fixtures doubling as perf fixtures. The SessionEnd hook runs
`--min-new 20 --min-new-errors 3 --if-stale 6h`, so most session ends are a
watermark check that exits immediately.

**Degradation is a UI feature, not a failure**: analyzer manifests declare
`requires: [forks, telemetry, embeddings]`; a missing capability renders as
an activation-ladder entry ("substrate doesn't provide forks"), never a
silent no-op. This is a **CLI surface too, not console-only** — the Memory
Health Report (the CLI twin of Overview, printed by bare `areev loop`)
lists each analyzer's activation status *with its reason*: missing
capability, below threshold (`n=12 < 50`), in cooldown until `T`, or
disabled — naming which config layer disabled it (policy vs. file). That is
the CLI answer to "why didn't analyzer X fire," where the solo-dev and
analyzer-author personas live.

## 9. LLM enrichment (optional layer)

Pipeline: `ANALYZE (deterministic, always) → DISCOVER (LLM, optional) →
ENRICH (LLM, optional) → VALIDATE+DEDUP (deterministic, always) → STORE`.
The no-LLM path is the identical pipeline with the LLM stages as identity.
A SYNTHESIZE stage (cross-recommendation composition) is deferred. LLM
stages can only ADD draft recommendations or polish whitelisted text fields
— they never gate storage of deterministic recommendations.

- **LlmBackend** trait beside EmbedBackend; `CommandLlm` mirrors
  the shipped CommandEmbed exactly: whitespace-split argv (no shell),
  stdin/stdout JSON, one process per call, construction-time probe
  (`{"loop":1,"op":"probe"}` → declared kinds + model, fail-loud). CLI:
  `--llm-cmd` (+ timeout default 120s, budget default 8k tokens,
  max-calls default 8; a typical run = 2 spawns: one batched ENRICH + one
  DISCOVER). Never persisted in the file — and never in the policy file.
- Evidence bundle: reuses ASSEMBLE + the SML/TOON/JSON renderers under
  budget; each item wrapped with provenance
  `{hash, origin: user_message|tool_output|observation|distilled_fact,
  trust}`. A history section carries the last 20 rejected recommendations
  (with operator reasons) + the last 20 approved — the model learns what
  this operator rejects.
- Injection defenses (binding): instructions never interleave with
  evidence; output must parse to the recommendation schema (unknown fields
  dropped, string caps enforced); ENRICH merges whitelist-only fields
  (`guidance`, enriched summary — the deterministic summary is kept
  separately) against known candidate ids; embedded CAL is statically
  checked under saved-query-body rules before storage; DISCOVER drafts
  must cite evidence hashes present in the bundle (uncited → confidence
  capped, flagged); **origin=llm is engine-stamped → never auto-apply,
  never prompt/host targets**; and DISCOVER drafts enter through the
  ordinary ADD path rather than as analyzer output, so the determinism
  contract — *a loop run's own recommendations are a pure function of
  (store state, params, `now`)* — stays literally true even with an LLM
  attached. `now` is an explicit input, not the wall clock: the engine takes
  the timestamp as a parameter (`run(store, opts, now)`) and never reads the
  clock itself, so the tool-failure window and staleness's `valid_to`
  comparison are reproducible, and counterfactual replay (§17) simply supplies
  a historical `now`.
- Pending-recommendation prompt injection (recall hook
  `--with-loop`): only engine-templated deterministic text is
  injected; llm-origin recommendations appear as hash+count stubs until
  approved — otherwise attacker → tool output → LLM-drafted rec →
  auto-injected prompt would be a laundering channel *before* any
  approval.
- Recipes shipped: a `claude -p` one-liner, a ~15-line OpenAI script, an
  ollama script.

## 10. Engine architecture: a standalone engine over a substrate

- **`areev-loop` = a standalone engine crate with zero areev dependencies.**
  It talks to an **`OmsSubstrate` trait**: execute CAL text → JSON rows
  (CAL is the spec'd wire language; the engine contains a small CAL
  *writer*, never a parser — validation delegates to the substrate); get
  grain by hash; put grain; supersede with justification; validate a CAL
  batch. All within the OMS Level 2 store protocol (§28.4).
- **Optional capabilities** are declared per-analyzer in the manifest
  (`requires: [forks, telemetry, embeddings]`); absence degrades gracefully
  (§8). Portable core on CAL + grains alone: duplicate sweep,
  contradiction sweep, staleness, outcome review, tool-failure clustering
  (Event grains are spec'd). Areev-capability analyzers: fork surfacing
  (heads table) and the future telemetry-fed set.
- **Reference substrate in-repo** (an in-memory grain map + a naive CAL
  subset, a few hundred lines): engine CI runs the full suite against it
  with zero Areev — the portability claim stays *testable*, and the same
  harness doubles as the conformance kit for third-party substrates.
- Lifecycle is portable by construction: audit grains are just grains; the
  `rec_status` cache is substrate-local.
- **Repo timing**: develop as a workspace member in the areev repo with
  CI-enforced zero sibling deps; lift to `AreevAI/areev-loop` when semantics
  freeze (same trigger as the OMS 1.5 release, one cycle in). A
  solo-maintainer cross-repo release dance during the churn phase is the
  cost being avoided; the clean boundary makes the later split a cheap
  subtree move. After the split, `areev` consumes `areev-loop` from crates.io
  like any dependency — the engine moves; the console, CLI, and substrate
  adapter stay in areev (§5.2 "one console, ever").
- **Claim wording** until a second real substrate passes the kit: "built
  on the OMS interfaces — Areev is the first substrate; the repo ships a
  reference substrate any implementation can test against." NOT "works
  with any OMS store" at n=1. And state the one gap plainly: on a substrate
  without the `forks` capability, **fork surfacing** is the single analyzer
  that goes dark; the other five run on CAL + grains alone. "Portable" means
  five-of-six, not full parity — say so.
- **Strategy note**: our earlier OMS play was spec-first — publish the
  open spec, hope implementations follow — and it underperformed. This
  architecture retries it product-first: Areev Loop becomes the reason to be
  OMS-compatible (want the engine? speak CAL + grains). Sequencing
  discipline: winning switchers from existing memory products onto Areev
  ships first, with "self-improving agents with governance" as the
  message; store-portability is the second act, never the headline.

## 11. Extension: the analyzer SDK

A new crate at the context tier (`core ← store ← cal ← areev-loop`; consumed
by cli/mcp/server/py/js through the substrate adapter).

```rust
pub trait Analyzer: Send + Sync {
    fn manifest(&self) -> &AnalyzerManifest;
    fn analyze(&self, ctx: &mut AnalyzeCtx) -> Result<Vec<RecDraft>, areev_loop::Error>;
}
```

- `AnalyzeCtx` is a **struct, not a trait** — the engine can add methods
  without breaking implementors: `cal()` via a read-only executor
  (`allow_destructive_ops=false`, writes rejected), `view()` curated
  reads, `telemetry()`, validated `params`, `watermark()`, `now()`.
- `RecDraft`: non_exhaustive builder; summary = `(template_id, args)`;
  dedup_key / origin / params-snapshot are engine-stamped, never settable.
- `AnalyzerManifest`: id `publisher.name/major`, title, description, tier,
  cadence class, `requires` capabilities, target classes, auto-apply
  class, and a flat hand-rolled **ParamSpec** list (6 kinds: bool /
  int{min,max} / float{min,max} / str{max_len} / enum / duration —
  serde_json only, no JSON-Schema dependency). ONE source feeding the CLI
  listing, `/api/loop/analyzers`, MCP listing, param validation, docs —
  **and the console's analyzer cards**. The manifest is explicitly the UI
  contract: a third-party analyzer that registers correctly appears in the
  console with title, badges, activation status, and read-only params, at
  zero UI work.

**Three implementation rungs:**

1. **OSS built-ins** — one `builtin_analyzers()` function, count
   test-pinned, manifests test-gated. No per-analyzer cargo features.
2. **External command analyzers (any language)** — the CommandEmbed
   pattern: registration probes `{"loop":1,"op":"manifest"}` fail-loud;
   per run, the ENGINE executes the manifest's declared CAL evidence
   queries and pipes rows to the process; the process returns drafts; a
   timeout or bad JSON kills that analyzer's run only. The analyzer
   process never touches the store — evidence flows one way. Declared ONLY
   via `--analyzer-cmd 'id=cmd'` — never persisted in a file or policy
   (nothing that syncs or gets committed may cause command execution).
   `areev-py`/npm ship ~20-line helper libraries for writing one.
3. **Linked Rust (the in-house seam)** — embedding hosts call
   `areev_loop::Engine::register(Box::new(...))`; proprietary crates stay
   closed (MIT/Apache static linkage). Restructuring the CLI crate into
   library-plus-wrapper-binary form — so a private binary could link extra
   analyzers around the OSS CLI — is deferred until such a binary is
   actually scheduled (a mechanical refactor when needed). Python/JS
   *in-process* callbacks are deferred (pyo3 GIL + store-mutex reentrancy;
   the napi async surface is missing) — those hosts use command mode
   meanwhile.

**Trust classes**: builtin and statically-linked are the SAME class —
compiling it in IS the trust decision, and OSS docs name no privileged
in-house tier. `command:<id>` = a distinct badge, never auto-apply.
`llm:<model>` = an amber badge, never auto-apply, no prompt/host targets.
Auto-apply always additionally requires engine-side shape verification
(§6.3), whatever the class.

**Compat contract**: downstream imports via one re-export path; the real
promise is workspace-version coupling (all crates inherit the workspace
version); `Analyzer` gains methods only with default impls or at a major;
the external JSON envelope is versioned independently and append-only —
command analyzers outlive Rust ABI churn.

**The public OSS line, drawn now**: *every general-purpose memory-hygiene
analyzer is OSS, forever; proprietary analyzers are domain-specific to
MindGryd's own products* (e.g. churn risk — about their customers, not
about memory).

**Custom evidence queries, scoped honestly**: user-supplied CAL lives only
where the contract is real — command-analyzer manifests. Built-ins own
their data access (fork surfacing reads the heads table; clustering reads
telemetry) — a stored query cannot be injected into a hardcoded ctx call,
so built-in customization = params + namespaces + windows.

## 12. Integration surfaces

Positioning sentence: **"Bring your own agent. If it emits tool calls,
Areev Loop can learn from it."** Never claim "works with any framework" —
claim the mechanism.

### 12.1 The matrix

| Surface | What you get | Status |
|---|---|---|
| **Rust** (facade) | Full engine + `register()` for linked analyzers | facade ships today; engine API new |
| **Python / Node** | `Areev(path, actor=…)`; `db.loop_run([min_new, min_new_errors, if_stale])` (ungated when bare); `db.recommendations(filter)`; `db.apply_recommendation(id, because=…)` (audited approve+apply); `db.dismiss_recommendation(id, why)`; **`db.record_tool_call(name, result, is_error, thread=…, input=input_json)`** | new methods, both bindings, existing scalars-in/JSON-out convention; governance semantics §6.6 |
| **CLI** | the `areev loop` namespace (status, `run`, queue verbs, analyzer config) + `areev init` | new verbs |
| **Claude Code** | 3-hook loop: UserPromptSubmit recall-hook (gains `--with-loop`), Stop capture-stop, SessionEnd `areev loop run --min-new 20 --min-new-errors 3 --quiet` | two hooks ship today; third is a new printed line |
| **MCP** | `areev_loop_adapter`, `areev_recommendations` (+ the existing 6 tools); supervisor pattern (§4.3) | new tools |
| **HTTP** | documented `/api/loop/*` + `/api/cal` (the console's own API; versioned, append-only JSON) — any stack can build its own surface | new routes |
| **Importers** | 8 `areev migrate` sources + a **generic tool-log importer** (OpenAI-style tool-call JSONL) | migrate ships today; 1 new importer |
| **Hub** | segment sync of the whole backend incl. recommendations + audit (they're grains) | hub ships today (library mode, no CLI verb yet); analysis stays local-per-file (loop-over-hub deferred) |

### 12.2 Capture is a first-class library path

The reasoning is in §4.2 — switchers have no hooks, and the flagship
analyzer eats tool events — so the loop must be closable from any codebase
in ~3 lines: `record_tool_call()` in py/js/rust plus the tool-log
importer. Quickstarts lead with it.

Naming note: the bindings' `dismiss_recommendation` performs the same
audited `rejected` transition as `areev loop reject` — a library-friendly
verb for the identical state change.

### 12.3 The host-apply contract (generic artifacts)

The engine **never writes outside the memory file**. For `host:<opaque>`
and external `doc:` targets the flow is: the recommendation proposes
(`proposal_data` / `proposal_edit` with `base_digest`) → host reviews →
**host applies in its own world** → host reports back via `areev loop
mark-applied <hash> --actor … --because …` (audited like every other
transition) → outcome review measures as usual. Areev Loop recommends beyond
its blast radius but only ever *acts* inside it — that keeps the
governance story airtight.

### 12.4 Recipes (docs, not deps)

Each recipe is ~20 lines, ends in the same loop (capture → analyze →
review → apply → recall), and models judgment (approve one, dismiss one
with a reason). Recipes lead with the **60-second proof** (§4.2 — the
canonical first recipe), then: Claude Code (hooks) · Anthropic tool-runner
(`client.beta.messages.tool_runner`, our-ecosystem-native, Python) · OpenAI
Agents SDK / any tool-calling loop (Python) · LangChain callback (Python) ·
Vercel AI SDK middleware (Node) · plain-cron ops (§4.4). A `--notify-cmd`
push hook is deferred until demand — it would be a new
executable-registration surface, and the cron recipe already covers CI/chat
notification; the human channel is instead the one-line stderr notice
(§13).

Recipes and fixtures ship in a **top-level `examples/` dir** (docs-not-deps,
so it is repo-only, never packaged), each smoke-tested in CI where runnable:

- `examples/analyzers/` — a complete hello-world **command analyzer** in
  Python *and* Node (manifest probe + evidence-row consumption + `RecDraft`
  JSON), on a **structural** subject (e.g. same-subject-across-namespaces
  consolidation) so the example never models text laundering. CI asserts
  the handshake: `echo '{"loop":1,"op":"manifest"}' | python
  examples/analyzers/consolidate.py`.
- `examples/llm/` — the §9 `--llm-cmd` scripts as real, copy-paste-runnable
  files (OpenAI, ollama), probe-handshake-tested in CI; the `claude -p`
  one-liner stays a docs snippet.
- `examples/policy/` — three `loop-policy.json` variants: solo
  (grants nothing — the shape-teacher), team (auto-apply `duplicate_sweep`
  + `staleness` → memory targets, `max_severity: low`), locked-down prod
  (analyzer deny-list, high severity floors, telemetry off). **No `_comment`
  keys** — §6.3 rejects unknown keys, so explanation lives in surrounding
  prose; the solo file is the same one the Setup tab offers (one source).
- `examples/import/` — a ~30-line sample OpenAI-style tool-call JSONL + the
  walkthrough (`areev import --format tool-log … && areev loop run` →
  tool-failure clustering fires on *historical* data) — the third
  cold-start leg for teams who bring logs, not a memory product.
- `examples/mcp/` — the supervisor pattern as two literal `areev mcp` launch
  configs (worker `--scopes write --actor agent:worker`; reviewer
  `--scopes review,apply --actor agent:reviewer`) + a transcript whose
  money shot is the **self-approval block firing** when the worker tries to
  approve its own proposal. No runnable two-agent orchestrator — shipping
  orchestration blurs the §2.4 non-goal.
- `examples/ci/` — a GitHub Actions job: `areev loop run` on the synced
  file → `areev loop list --status pending --fail-on high` → exit 2 fails
  the build (or a PR comment). No pre-commit hook — memory files are
  runtime state, not source, so commit-time is the wrong trigger grain.

**Framework coverage is an adapter table, not one recipe each.** Beyond the
recipes above, CrewAI · AutoGen · Pydantic-AI · LlamaIndex each get one
table row (framework → its callback/middleware name → the same three
capture lines), never a dedicated recipe: none has a mechanism distinct
enough from the generic tool-calling loop to justify the maintenance, and
per §18 we claim the mechanism ("any agent that emits tool calls"), never
"works with any framework." A row graduates to a recipe on real demand.

## 13. Configuration & triggers

- **One record per analyzer id**: `{enabled, params, severity_floor,
  namespaces}` in a `loop_config` table inside the memory file
  (file-truth; lazily created, so existing files open unchanged). A
  `loop_state` sibling table holds watermarks/cooldowns — deliberately
  not `meta` key/values (meta stays the scalar open-time scan). Rows are
  stamped actor + source (local CRUD vs arrived-via-sync), so config that
  arrived through sync is visible provenance, not a surprise.
- **Layering** (one precedence rule): engine ceilings > host CLI flags >
  host policy file (§6.2) > memory-file config. The file may
  enable/disable analyzers, raise severity floors, and lower caps; the
  host holds global off, deny-lists, auto-apply grants,
  `--analyzer-cmd`/`--llm-cmd`, and telemetry mode. Escalation fields
  don't exist in the file schema.
- CLI — one namespace, `areev loop`: bare `areev loop` prints the Memory
  Health Report + pending count (the CLI twin of the console Overview, with
  per-analyzer activation status + reason, §8);
  `run [--min-new N --min-new-errors N --if-stale D --quiet --dry-run
  --only <id> --format json]` executes an analysis pass (`--dry-run`
  analyzes and prints drafts but stores nothing — analyzers are read-only
  by the trust floor, so this is pipeline-tail suppression; with
  `--only <id>` it is exactly the command-analyzer author's test loop, and
  the CLI twin of the console's run-now); queue verbs
  `list | show | approve | reject | apply | rollback | mark-applied`
  (`--because` required, `--actor` labels), where `list` takes
  `--fail-on <severity>` for CI gating and every hash argument accepts a
  **git-style unique short prefix** (ambiguity → a VAL error listing the
  matches; `list` prints 12-char forms); `schedule --print --os …`
  (§4.4); analyzer config
  `analyzers | enable <id> | disable <id> | set <id>.<param>=<value>`; and
  `config-export | config-import` (plain JSON, zero new deps). An
  interactive `review` walker (one pending recommendation at a time,
  prompted BECAUSE — the CLI embodiment of no-bulk-approve, reusing the
  `repl` precedent) is deferred until CLI-first users ask; the console
  covers launch.
- Config-change audit: a rebuildable index layer (queryable
  actor/because/old/new) — NOT immortal grains. Authoritative audit grains
  are reserved for the recommendation lifecycle, where they authorize
  mutations.
- **Run-outcome contract** (the reporting half of "hosts call a cheap
  idempotent command" — the command must *say what it did*, or cron mail,
  CI logs, and polling agents can't tell ran-and-proposed from a healthy
  no-op). This is the engine's run-result type from day 1, so CLI,
  `/api/loop/run`, MCP, and bindings all return one shape:
  - `areev loop run --format json` →
    `{outcome: "ran"|"skipped", skip_reason:
    "min_new_not_met"|"not_stale"|"lock_held"|null, new_since_watermark:
    {grains, error_events}, proposed, deduped, duration_ms}`.
  - **Exit taxonomy**: `run` exits 0 on *ran or clean skip* (cron must not
    page on a healthy no-op) and 1 on error; the CI gate `areev loop list
    --fail-on <severity>` exits 0 = none match, **2 = matches**
    (build-blocking), 1 = error. JSON output is declared append-only like
    the HTTP envelope.
  - **Human notification without a daemon or new surface**: a SessionEnd
    `--quiet` run stays silent on a no-op but still emits exactly one
    stderr line when new recommendations land (`areev-loop: 3 new (1 high) —
    areev loop list`); a non-quiet run prints the health report.
    `--quiet` suppresses the health report and the ran-nothing line, never
    the new-recommendations notice. Paired with recall-hook `--with-loop`
    (agent-facing), discovery is closed; `--notify-cmd` stays deferred.
- **Trigger architecture: no scheduler, no daemon, anywhere.**
  `areev loop run --min-new 20 --min-new-errors 3 --if-stale 6h` no-ops
  cheaply off a file-truth watermark (`--min-new-errors` gates on new
  `is_error` Events since the watermark — the flagship analyzer's own
  signal — OR-composed with `--min-new`); hosts wire hooks (SessionEnd),
  cron, or MCP calls. Deliberately typed gate flags, **not** a
  `--if <CAL>` predicate: arbitrary CAL in a hook line is an
  injection-adjacent surface and fake generality; per-namespace gates stay
  deferred until asked. We rejected an in-engine on-Nth-write trigger:
  unbounded work in the write path breaks the 50ms voice-cadence gate, and
  hidden side effects on write contradict the trust brand.
- **Concurrency & multi-host**: locally, `areev loop run` is an ordinary
  single-writer open — Areev's one-writer-per-file rule already
  serializes it against capture, and the SessionEnd hook orders it after
  the session's writes by construction; `areev ui` reads concurrently. When
  triggers stack (SessionEnd hook + user cron + console run-now within
  seconds), a second concurrent `run` attempts a **non-blocking** writer
  open; if the lock is held it exits 0 with `skip_reason: "lock_held"` —
  never blocks, never surfaces a raw busy error. Coalescing then holds by
  construction: the lock-holder advances the file-truth watermark at run
  *end*, so the next caller no-ops via `--min-new`/`--if-stale`.
  Watermark-at-end means a crashed run simply re-runs, which is correct
  because propose is idempotent under `dedup_key`. Concurrent lifecycle
  transitions (two reviewers/surfaces racing approve/apply) are
  compare-and-set on current status; the loser gets a typed error mapped to
  HTTP 409. (`--if-stale` compares wall-clock across hosts after sync —
  fine at hour granularity; sub-minute staleness is not a supported
  cadence.) Across hosts: recommendations and audit grains are ordinary
  grains, so hub sync carries them; if two hosts run the loop on copies of
  the same file, the same finding yields the same deterministic `dedup_key`
  on both, and the next run's validate/dedup stage collapses the pair into
  one live recommendation chain. Watermarks and cooldowns are file-truths
  precisely so a synced file behaves identically on its next host.
  Coordinated loop-over-hub (distributed review) stays deferred
  (§12.1).

## 14. Spec strategy (OMS 1.5 / CAL 1.2)

Don't freeze fresh semantics into a CC0 spec with append-only error codes
before one release of real usage — and a spec we control growing
Areev-shaped modules would weaken the conformance story. So:

- Implement 0x0C in Areev/areev-loop NOW as the reference implementation
  (address-safe, additive). Publish **OMS 1.5 + CAL 1.2 drafts under
  `proposals/`** in the oms repo (existing precedent:
  `proposals/embedding-text-field.md`); release after one Areev cycle.
- Spec surface = the MINIMAL durable interop layer only: the
  Recommendation grain, lifecycle semantics + a fail-closed transition
  table, `REVIEW <hash> APPROVE|REJECT BECAUSE "…"`,
  `APPLY <hash> [DRY RUN]` (DRY RUN desugars to EXPLAIN),
  `recommendation` joining the CAL ADD-able set, the `review`/`apply`
  scopes, and the no-amplification rule. Packaged as an OPTIONAL
  conformance module (MAY at every level; implementations declare a
  capability; non-implementers fail closed on the new keywords).
- **Never specced**: the analyzer/algorithm registry
  (embedder/telemetry-dependent analyzers can't have cross-implementation
  test vectors — determinism is defined per-implementation given (store
  state, local telemetry, params), reproducible via `evidence_query`),
  analyzer config objects, the console API, and the policy file (host
  config is per-implementation by definition, same posture as executor
  limits).
- A `DEFINE REFLECTOR`-style CAL config syntax: not planned — config is
  CLI/API surface. New error codes land in an append-only block. The CLI
  remains the local root of trust (scopes apply to server/MCP surfaces).

## 15. Decisions already settled

These were settled during design review, before this document; each row
records its reason so future debate can start from the reason instead of
from zero. Any of them can be reopened — with new evidence against the
recorded reason.

| Decision | Why |
|---|---|
| No scheduler/daemon; analysis is a host-triggered idempotent command | Unbounded work in the write path breaks the voice-cadence gate; hidden side effects on write contradict the trust brand. |
| Auto-apply never touches prompt/instruction or host targets, and never fires for llm- or command-origin recommendations | Prompt poisoning and text laundering (§6.3); blast radius must stay structural. |
| No "earned autonomy" (track-record-unlocked auto-apply) | Reward-hackable; statistically meaningless at single-digit n. Autonomy is an explicit host grant. |
| No bulk approve in the queue | One pasted BECAUSE across 25 audit grains is an audit chain faithfully recording that nobody looked. The pressure valve is host auto-apply policy for structural recommendations. |
| `dedup_key` excludes proposal content and evidence | Content-hashing defeats dedup: a growing cluster re-proposes as "novel" forever. |
| Lifecycle fields never writable via CAL | `write` scope must not forge `approved`; transitions only via REVIEW/APPLY/engine API. |
| Exactly three public nouns: analyzer, recommendation, policy | A named multi-instance "reflector" config object invited incompatible data models and ~10 extra CLI verbs for zero first-release value. |
| No config-package ("pack") format — no TOML bundles, manifests, or round-trip tooling | A package format for analyzer/config bundles would mean a second config language and a new dependency for zero first-release value. Docs pages with copy-paste commands plus `areev init` templates-as-literal-CAL cover the need. |
| Executable registration (`--analyzer-cmd`, `--llm-cmd`) is CLI-only, never file- or policy-persisted | A synced memory file or committed policy file must be inert — nothing that syncs may cause command execution. |
| Trust-floor fields absent from every schema (not "off by default") | A hostile or synced file can never arrive pre-armed; unknown keys are rejected at load. |
| FORGET allowed inside proposals, but triple-gated and non-rollbackable | Staleness sweeps need tombstones; destruction stays gated per the standing Areev invariant. |
| Built-in analyzers take params/namespaces/windows — not custom CAL | Built-ins read typed internals (heads table, telemetry); accepting stored queries there is fake generality. Custom CAL is real in command-analyzer manifests. |
| No invented precision numbers, ever | Default-on status is decided by measured `loop_precision` fixture runs, published RESULTS.md-style. |
| Analyzer count launched at six; +2 added on peer review | New value that cycle was UI + governance + integration, not more detectors. Peer review (Q2/Q3) then reconsidered the *field-based* Skill/Goal analyzers (vs. the weaker event-based forms): `skill_stall` ships default-on, `goal_stagnation` opt-in. Fact-churn stays cut (no reliable signal). |
| The engine never writes outside the memory file | Host-apply contract (§12.3) keeps recommendations-beyond-blast-radius safe. |
| Token-less console becomes read-only (breaking) | An unauthenticated write path would let any local process bypass the review queue; the auth change is load-bearing for governance. |
| No `areev loop watch` verb — time triggers are the OS scheduler's job; we print its config (`schedule --print`) | A foreground `watch` respects the letter of no-daemon but not the spirit — users `nohup` it and then want pidfiles, logs, restart-on-crash, i.e. scheduler creep with worse reliability than the OS scheduler already on every platform. Reopen on sustained Windows Task Scheduler friction. |
| `run` exits 0 on clean skip; a held lock is a clean skip, not an error | Cron must not page on a healthy no-op; a raw busy error on stacked triggers reads as "Areev Loop is flaky." Exit 1 is reserved for real errors; the CI gate `list --fail-on` uses exit 2. |
| No 202-plus-poll async job queue on the HTTP API | A job registry inside a serial one-request-per-connection server is scheduler-state by the back door. API/MCP handle light runs + status; CLI/cron handle heavy runs. |
| `dedup_key` analyzer-family excludes the `/major` version | Including major re-proposes the whole queue as "novel" on every analyzer upgrade — the same failure content-hashing causes; the exact proposing version still lives in the grain's `analyzer` field. |
| Bindings' bare `loop_run()` applies no watermark gating | Gating is a hook/loop ergonomic; an evaluator's first bare call must run, not silently no-op. |

## 16. Build order & estimates

0. **Prerequisite fixes** (~1 day): wire QueryRegistry hooks into
   AreevFacade (today `RUN` of builtin saved queries → QueryNotFound on
   the embedded path — user-visible the moment the query editor exists);
   extend `GET /api/config` with areev-loop/policy reporting.
1. **Engine crate** (`areev-loop`, workspace member, CI-enforced zero sibling
   deps): substrate trait + reference substrate + Analyzer
   trait/manifest + engine + six built-ins + validate/dedup/store
   pipeline + `loop_config`/`loop_state` + the **run-result type**
   (outcome/skip-reason/counts, §13) so every surface returns one shape +
   a `LOP` error-code domain (the engine has zero areev deps, so it needs
   its own `DOMAIN-Ennn` block registered in ERROR_CODES.md; REVIEW/APPLY
   *syntax* errors stay in the CAL domain) (~1 week).
2. **Areev adapter + CLI + bindings**: substrate impl over facade/store,
   the `areev loop` verb family (incl. `--format json`,
   `--min-new-errors`, `--fail-on`, `--dry-run [--only]`, short-hash
   prefixes, `schedule --print`), py/js methods + `record_tool_call`
   (`thread=`) + `Areev(actor=…)` + the tool-log importer, MCP tools, py/js
   analyzer helper libs (~3–4 days).
3. **Precision harness** + fixtures; measured numbers pick the default-on
   set (~2–3 days; blocks any default-on claim).
4. **Policy file** (`--policy`, JSON schema, precedence, config
   reporting) (~1–2 days).
5. **Console increment 1 — Manage + Overview + auth** (~1 week): Paper
   artboards first; the asset split; the read-only-token-less auth
   change; Areev Loop tab (Queue/Detail/Analyzers/Outcomes); Overview +
   health; the policy panel. Ships together — the auth change is
   load-bearing for everything after.
6. **Console increment 2 — Debug** (~1 week): Explain mode + context
   preview (any plan-payload extensions land in areev-cal here), Sessions
   timeline, recommendation cross-links.
7. **Console increment 3 — Create** (~1 week): `areev init` + templates
   (incl. the `demo` seeded corpus, §4.5 — which doubles as the analyzer
   integration test and the first `loop_precision` fixture), Setup tab,
   instruction-doc editor, saved-query editor.
8. **Docs** (~2–3 days): `docs/loop.md` (concepts, four gates, policy
   file, upgrade/compat); REVIEW/APPLY into `cal-reference.md`; +2 tools
   into `mcp-reference.md`; a README Areev Loop section (leading with the
   60-second proof); FAQ + `llms.txt` entries; and a
   **`docs/security-model.md` update** (trust floor, the laundering threat,
   the §5.7 auth change) — shipping a breaking auth change without touching
   the security-model doc undercuts the pitch.
9. **Examples + cookbook reconciliation** (~2–3 days): the top-level
   `examples/` tree (§12.4) with CI probe smoke-tests; and rewriting
   cookbook **§10** ("Build an agent that learns"), which today hand-rolls
   the exact loop Areev Loop governs (reflect-via-your-own-LLM, manual lessons,
   restore-as-undo) — reframed as "the substrate, by hand" with a header
   pointing at the governed loop, so the two don't ship as competing
   stories; a new canonical "Self-improve with Areev Loop" page carries the
   tour, recipes, policy, and review verbs.

Total ≈ **6.5–7.5 weeks solo**. Increments 5–7 are each shippable; if the
schedule compresses, cut 7 before 6 before 5 — manage-and-debug is the
trust core; create is first-run polish. Docs (8) and examples (9) interleave
with the increments they document rather than tailing them — the 60-second
proof and demo template in particular gate the activation metric, so they
land with the surfaces they exercise.

**Deferred by trigger**: repo split (semantics freeze / OMS 1.5) ·
`areev-cli [lib]`/Extensions wrapper binary (a private binary is actually
scheduled) · ParamSpec auto-forms (an external analyzer actually renders
in the console) · named multi-instance analyzer configs (a real user asks
to run one analyzer twice) · quorum enforcement (real multi-party
deployments) · js in-process analyzers (napi async surface lands) ·
telemetry-fed analyzers (2–4 weeks of telemetry exist) · `areev-loop replay`
(post-1.0 flagship, §17) · `--notify-cmd` (demand — the one-line stderr
notice covers launch) · interactive `areev loop review` walker (CLI-first
users ask) · per-namespace run gates (`--min-new-ns`, asked) ·
rollback-discovery sugar (`areev-loop list --status applied --since 7d`, plus
the doc paragraph ordering per-rec rollback vs. `restore --until-hlc` —
which also rewinds unrelated writes) · loop-over-hub + multi-file fleet
view (multi-host demand) · audit JSONL export sugar.

## 17. Growth path: learning from the record

The loop is structurally propose→apply→observe→adjust, but formally it is
a **contextual bandit with delayed, confounded, human-mediated rewards** —
not RL (no long-horizon credit assignment, no transition model). Rewards
are sparse (few applies), delayed (7–28-day windows), confounded (no
control group), non-stationary, and gameable — which is exactly why
"earned autonomy" was rejected (§6.1).

Strategy: **rewards are the scarce asset, not algorithms.** Every
approve/reject with BECAUSE, every outcome fact, every audit chain is
labeled training data accumulating from day 1. Bandit machinery becomes
justified at roughly 50–100 labeled decisions per analyzer; bolting it on
then is cheap, building it now would be learning theater on n=5.

Escalation ladder, in order:

1. **Counterfactual replay — the flagship** (post-1.0): immutable history
   + deterministic analyzers ⇒ replay any configuration against the full
   past offline (`areev-loop replay --set duplicate_sweep.threshold=0.85
   --window 90d`) — off-policy evaluation at zero live risk, unique to
   content-addressed immutable memory. Positioning line: *"explore in the
   past, not in production."*
2. **Ranking exploration**: Thompson-style ordering of the pending queue
   from per-analyzer approval history — the console queue is where this
   lands; the exploration cost is reviewer attention, never memory
   corruption.
3. **Recall challenger slots** (strictly opt-in, later): an ε-budgeted
   slot in assembled context surfacing a cold/low-confidence memory to
   gather usage signal — it injects noise into live context, hence opt-in
   and bounded.
4. **A/B lesson application** (deferred): OMS already carries
   `con:ab_variant`; needs session-level attribution machinery; wait for
   demand.

## 18. Claims discipline

CAN claim (with no LLM): detects contradictions · clusters recurring
failures · finds duplicate and stale memories · scores memories by
measured outcomes · every recommendation cites its evidence grains · every
apply is an audited, undoable supersession · outcomes verified against
subsequent history · every prompt/instruction edit is versioned with diffs
and rollback · effective policy is inspectable · the console runs offline
with zero dependencies.

MUST NOT claim: "writes better prompts than you" · "understands your
agent" · "learns like a human" · "gets smarter on its own" · "AI-powered"
for the deterministic layer · any unbenched accuracy number · "best-in-
class UI" in marketing copy (internal bar; show screenshots, let readers
conclude) · "no-code agent building" (we scaffold backends, not agents) ·
"full observability" (say "the sessions you capture") · "guardrails for
your agent's outputs" (our guardrails govern backend changes, not runtime
outputs) · "works with any framework" (name the mechanism: any agent that
emits tool calls can feed it).

Governance claims must name the mechanism, never the vibe: not
"enterprise-grade governance" but "separation of duties, mandatory
reasons, hash-chained audit, undoable applies, measured outcomes."

Areev Loop's honest sentence: *it measures which of its own advice works and
proves it — and is built to learn from that record when the record is deep
enough to mean something.*

**Framing (v1 is memory-weighted).** Five of the six analyzers are memory
*hygiene* (dedup, contradiction, staleness, fork, outcome); only tool-failure
distills net-new knowledge, and none yet improve Skills. So the accurate v1
claim is a **self-improving memory backend** — the governed loop plus clean,
coherent memory the agent reasons from — not "makes your agent better at its
tasks." Genuine capability improvement (skill trajectories, coverage-gap
learning) is the growth path (§8 deferred set, §17). Lead with the memory
framing; it is both true and defensible, and it is the discipline that keeps
the stronger future claim credible when it lands.

## 19. Naming, reservations, open questions

**Names were secured** (as of 2026-07-17) under the engine's original
name: crates.io and PyPI placeholders at 0.0.1, plus an `@areevai`-scoped
npm package. All placeholders were honest name-reservation stubs,
trivially re-creatable. *(Historical note: the engine was renamed to
`areev-loop` in v1.1.1, so these reservations are now moot — the
`areev-loop` / `areev-loop-adapter` crate names are the live ones.)*

**Resolved (2026-07-17) — branding and naming**: Areev Loop is adopted
outright as the user-facing name, under the two-word rule of §2.6: the
`areev loop` verb family, the Areev Loop tab inside the unchanged `areev ui`,
`/api/loop/*`, `--with-loop`. There is no separate Areev Loop console,
app, or "Studio" — one console, in Areev (§5.2).

**Open questions for the team:**

1. **Console increment order**: recommendation is Manage → Debug → Create
   (trust core first, daily-driver second, first-run polish last). If
   launch demos need the `areev init` golden path earlier, Create can swap
   ahead of Debug at the cost of demoing governance on hand-made files.
2. **Policy file schema sign-off** (§6.2) — specifically confirming that
   executable registration stays CLI-only.
3. **First framework recipes**: proposed Claude Code + OpenAI-style JSONL
   + LangChain (py) + Vercel AI SDK (js); which two lead the docs?
4. **MindGryd services' host language** (Rust/Python/TS): decides whether
   the py/js in-process analyzer surface jumps the priority queue; until
   then, in-house analyzers run via the command envelope like everyone
   else's.
5. **OMS 1.5 timing** relative to the areev-loop repo split (currently
   coupled: both at semantics freeze).
6. **`demo` template seeding mechanism** (§4.5): can CAL `ADD` express
   typed Event grains with `is_error` (keeping the template a pure literal
   CAL batch), or does the corpus need a small engine-side seeding shim?
   And can one linear batch plant the two concurrent heads fork surfacing
   needs, or does the demo fire four analyzers rather than five? Both are
   cheap either way, but they decide whether `demo` is purely
   templates-as-CAL or a hair more.

## Appendix A — grounding in the shipped code (verified 2026-07-17)

**Areev repo** (`AreevAI/areev`): 9-crate workspace + standalone napi
package; dependency order core ← store ← cal ← context; ~950 tests.

- **Server & console**: `areev-server/src/lib.rs` = 779 lines of
  std-only HTTP/1.1, serial one-request-per-connection; routes today:
  `POST /api/cal` (:305), `GET /api/stats|log|config|browse|grain|verify`
  (:337–:525), `POST/GET /api/segment(s)` (:532–:564). `console.html` =
  1362 lines, embedded via a single `include_str!` (lib.rs:15), vanilla
  JS; components: jsonTree, grain drawer, chainView, sortable tables, CAL
  highlighter, canvas force-graph, light/dark via CSS vars; `api()`
  wrapper with Bearer + 401→prompt→retry. `GET /api/config` already
  reports effective config + file-vs-host reconciliation warnings. The
  Origin check currently passes any loopback port (`origin_is_local`).
  `areev ui` currently wires `allow_destructive_ops` default-ON — the §5.7
  auth change targets exactly this.
- **CAL**: `EXPLAIN` is fully implemented and returns a structured plan
  (`ExplainStmt` ast.rs:104–105; `execute_explain` executor.rs:2819;
  `CalResultPayload::Explain { plan: CalQueryPlan }` executor.rs:166,
  :3083; tested executor.rs:5974). CAL 1.1 = a closed 12-variant statement
  enum (incl. Explain); `DROP` is a lexer non-token except
  TEMPLATE/QUERY; destructive surface = single-grain `FORGET`, gated by
  `CalExecutorConfig::allow_destructive_ops`.
- **CLI**: dispatch confirms ~24 verbs incl. add, recall, search, cal,
  history, log, stream, follow, restore, bundle, import, migrate,
  reindex, verify, stats, serve, capture-stop, recall-hook, remember,
  memtool, repl, ui, get, provenance, forks, merge, novelty. **No `init`
  verb exists** (name free). **`record_tool_call` does not exist
  anywhere** (proposed). Hooks: recall-hook (UserPromptSubmit, budget
  400) + capture-stop (Stop → Event grains incl. tool_use/is_error);
  `areev hook claude-code` only prints settings snippets, never writes
  user config.
- **QueryRegistry**: 18 builtin saved queries as inline literals,
  builtin-immutable flag, max 100/ns, 8KB body, 10 params,
  `agent/<slug>/<suffix>` names. **Known gap**: the shipped AreevFacade
  does not override the CalStoreFacade query hooks, so `RUN` of builtins
  → QueryNotFound on the embedded path (build-order item 0).
- **Plugin seams**: EmbedBackend/CommandEmbed (dim-probe at construction,
  whitespace-split argv, stdin text → stdout JSON f32), RerankBackend,
  QueryExpander; Python has in-process callable + command modes, Node is
  command-only (napi async surface deferred). CommandLlm (§9) copies
  this exact pattern.
- **File semantics**: `meta` table = scalar file-truths scanned at open
  (text_index, entity_relations, embedding model/dim); `open()` adopts,
  `open_with()` re-stamps + reports warnings. memory-tool adapter:
  a `/memories/*` file = a Fact supersession chain (subject=path,
  relation="memory_file", body in context, mirrored to embedding_text) —
  the substrate for `doc:` targets and the instruction-doc editor.
- **No scheduler/daemon** exists anywhere; the hub is a library-mode
  server (`into_hub`), exercised by the multichannel acceptance test; no
  hub CLI verb.
- **Error codes**: stable `DOMAIN-Ennn`, append-only, registry in
  ERROR_CODES.md, format/uniqueness test-enforced. Areev Loop adds codes in a
  new append-only block.
- **Invariants inherited**: immutable content-addressed grains; canonical
  serialization frozen; destruction gated (FORGET only); CAL syntax = an
  OMS conformance contract (no new syntax without a spec-level decision);
  one memory = one file; dependency-light (no clap, no HTTP framework, no
  MCP SDK, no workspace-wide async runtime).

**OMS spec** (CC0, `~/opensource/oms`, v1.4 2026-06-12): 11 types
0x01–0x0B; **0x0C–0xEF reserved "future standard types"** (Skill 0x0B was
realized from this reserve in 1.4 — the precedent); §4.5 omit-absent ⇒
additive optional fields/types are address-safe ("existing content
addresses remain valid", 1.4 changelog); §5.6 index layer mutable &
hash-excluded (`superseded_by`, `verification_status`
unverified|verified|contested|retracted, access counters); §11.7 index
manifest portable/local split; §28.4 store protocol (supersede atomic,
distinct from put); §28.8 Skill improvement only by supersession, chain =
learning history; §24.2 already names a "reflector" observer type and
"reflective" mode (spec vocabulary for observers — unrelated to the
"reflector" config object rejected in §15); consumer profile A.6 carries
`con:ab_variant`.
Versioning: Keep-a-Changelog, MAJOR/MINOR, `proposals/` directory
precedent; companion CAL/SML releases land in the same OMS changelog
entry.
