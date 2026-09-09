# areev-bench — how a benchmark harness is built here

Reproducible harnesses that produce published numbers. The evidence lives in
[`AreevAI/areev-benchmark`](https://github.com/AreevAI/areev-benchmark); only
the harness and the design documents live here, because the harness is a
workspace member that gates CI and has to move with the engine.

Each track owns a design document at this level (`RECEIPTS.md`, `PERSIST.md`,
`APPWORLD.md`, `CURVE.md`, `DRIFT.md`, `FOURWAY.md`, `EXPENSE.md`,
`SELFIMPROVE.md`) and a directory of harness code beside it. The design
document is written **before** the run and is not revised to match the
result.

**Starting a new track: read [`BENCH-TEMPLATE.md`](BENCH-TEMPLATE.md) first.**
It is the shape every track here follows, with the copy-paste form of the five
surfaces below. Each track then records its own answers in an `AREEV.md` beside
its harness code — [`receipts/AREEV.md`](receipts/AREEV.md),
[`tau2/AREEV.md`](tau2/AREEV.md), [`appworld/AREEV.md`](appworld/AREEV.md),
[`persist/pastbench/AREEV.md`](persist/pastbench/AREEV.md),
[`persist/horizon/AREEV.md`](persist/horizon/AREEV.md),
[`src/selfimprove/AREEV.md`](src/selfimprove/AREEV.md) — including every
surface it skipped **and why**. Read the one closest to what you are building
before writing anything: the six together already record **five reads that do
not express in CAL** (tabulated in `src/selfimprove/AREEV.md`), which is worth
more to the next harness than rediscovering them one at a time.

## Use the product's own surfaces, or say which one you measured

A harness exists to show what Areev does. Every step it hand-rolls in Python
is a step the published number does not demonstrate — and any claim about
that step then belongs to the harness, not to the engine. The six below are
the ones benches here keep reimplementing. Apply the ones that fit; where one
does not fit, say why in the track's `AREEV.md`.

### 1. Compose context with `ASSEMBLE`, under a token budget

`RECALL` reads grains; `ASSEMBLE` composes them into context. Anything that
becomes model-facing text goes through `ASSEMBLE`.

```sql
ASSEMBLE "operating rules" FOR "the agent" FROM
  rules: (RECALL facts WHERE relation = "lesson")
BUDGET 300 tokens
FORMAT markdown
WITH dedup(object)
```

A hand-rolled sort-and-join forfeits the shared token estimator (a cap
counted in *lines* or *characters* is not a token budget, and two arms capped
that way are comparable only by coincidence), progressive disclosure
(Full→Summary→Omit), `PRIORITY` weighting, and `PIN` for non-degradable
sections.

**State the budget, and only use `ASSEMBLE` for text a model reads.**
`ASSEMBLE` applies a token budget whether or not you ask for one — default
4000, ceiling 16000 (`CAL-E033`). It used to drop grains to that budget in
silence, which is how wrapping AppWorld's error SELECTION in an `ASSEMBLE`
returned 79 of 229 grains on run 1's own memory; since 1.7.4 the drop
announces itself as `CAL-W017` (#208).

Two rules follow. A pure selection belongs in a saved `RECALL` — it is not
composing model-facing text, so it should not carry budget semantics at all.
A rendered block states `BUDGET n tokens` (`cal_assemble.MAX_BUDGET_TOKENS` is
the ceiling, used as a stated bound rather than a squeeze, because a binding
budget would change published prompt bytes) — and the ceiling is not a
guarantee of no trimming, so `cal.section` / `cal.rows` **raise** on
`CAL-W017`: a prompt section that lost rows is a wrong prompt, not a warning.
`scripts/parity_check.py` seeds a section past the default and asserts both
halves.

*Adherence today: **adopted** (2026-09-08; revisited 2026-09-09).* Every
model-facing block in `receipts`, `tau2` and `appworld` is an `ASSEMBLE`;
`persist` is two sections of four. The gaps this sweep found were filed and
**four of them were fixed in 1.7.4** (#206–#209), plus the three #217 names,
so what is still harness-composed is a much shorter list, and for reasons that
are now about the PROMPT rather than about CAL:

- AppWorld's passive arm **has moved** (2026-09-09), and its block ranks the
  same `(endpoint, message)` pairs the harness used to tally in Python — which
  took a second round of engine work, [#217](https://github.com/AreevAI/areev/issues/217):
  a composite `GROUP BY` key, a Tool body a template can render, and a `LIMIT`
  that binds after `COUNT`. Moving it without those would have changed the
  prompt an arm was measured under, which is rule 4's case for
  pre-registration rather than a refactor.
- persist's notes are *expressible* and deliberately **not moved**, for that
  same reason. The query is written out in its `AREEV.md`, ready.
- persist's session titles need FIRST-of-group, which #209 did not land.
- `selfimprove` needs to reshape a JSON payload — parse, remove a key,
  re-serialise — which was declined on purpose (#211), not left undone.

The shared helpers are `scripts/cal_assemble.py`.

Ordering *within* a section is a stage inside the source parentheses --
`(RECALL facts WHERE relation = "lesson" ORDER BY object ASC LIMIT 50)`. That
position did not parse before this work; `CAL-W016` named it as the fix while
the parser refused it (`CAL-E002`), and the engine was fixed rather than the
harnesses worked around it.

### 2. Register the read as a saved query

```sql
DEFINE QUERY "agent_block"($ns) AS { ASSEMBLE … }
RUN "agent_block"($ns = "appworld")
```

Saved queries persist as `qry:<name>` meta rows: they **travel with the
memory file** and **replicate through bundles**. A read that lives as a
Python string literal means a memory handed to someone else does not carry
how to read it — the knowledge is stranded in the harness.

*Adherence today: **adopted** (2026-09-08).* Each track's `REGISTRY` is
installed on every writable open and travels with the file --
`scripts/parity_check.py` asserts a copied memory still carries its queries,
which is what lets a frozen read-only arm (`STO-E004`) assemble its block at
all.

### 3. Render through CAL, and pick the format deliberately

`FORMAT` offers `sml`, `markdown`, `text`, `toon` and `json`. `FORMAT json`
followed by rebuilding the text in Python is the anti-pattern: it asks CAL
for data and then does the rendering CAL exists to do. `DEFINE TEMPLATE`
registers a reusable renderer as a `tpl:<name>` row, which travels with the
file the same way.

**Which format is cheapest depends on how many rows there are** — measured by
`scripts/cal_assemble.py` over five real memories from four tracks, in
characters:

| grains | markdown | sml | toon | json |
|---:|---:|---:|---:|---:|
| 1 | **88** | 130 | 103 | 395 |
| 4 | 647 | 761 | **603** | 1718 |

`toon`'s tabular header costs more than it saves on a short block and starts
paying at roughly four same-shaped rows; `json` is never the right choice for
a *rendered* block. Measure on the track's own memory rather than assuming —
that is what the smoke is for.

*Adherence today: **adopted via `DEFINE TEMPLATE`** (2026-09-08).* The
default renderers were not adopted, and the reason is measured rather than
assumed: `receipts/structure.py` found a seven-rule memory scores **35** under
CAL's default `FORMAT markdown` against **141** hand-assembled, because that
renderer prefixes each rule with its subject and relation and suffixes it with
a date. So each track registers a template that emits its published bytes
exactly, and `scripts/parity_check.py` gates the equality against the retired
renderers (kept verbatim there, imported by nothing).

Two template shapes carry the whole crate. `{{#if assembly.grain_count}}` in
`HEADER` makes an empty section render to the EMPTY STRING -- what a
rolled-back rule set must look like, or the paired evaluation stops being
causal. `{{^assembly.grain_count}}` in `FOOTER` gives the opposite: a heading
that always renders, with `- (none yet)` under it.

### 4. Record a tool call as a call, not as a string

```python
db.record_tool_call(name, result, is_error=…, thread=…, call_id=…,
                    input=json.dumps(args), status=…, failure_cause=…)
```

A Tool grain has a **call/result lifecycle**. `add("tool", {tool_name,
is_error, content})` flattens it, discarding the input that produced the
result, the call/result correlation, the status and the failure cause — and
those are exactly the fields a later forensic query (`areev_tool_provenance`,
`step_actions`) needs.

*Adherence today: **adopted** (2026-09-08).* `tau2` and `appworld` moved to
`record_tool_call`; `persist`, `persist/horizon` and `src/selfimprove/memory.rs`
already did it. `receipts` is not applicable -- that agent calls no tools.

Two things bit on the way: `status` and `failure_cause` are **closed enums**
(`pending|completed|failed`, and `timeout|executor_error|
schema_validation_failed|user_aborted|unknown`), so a harness's own taxonomy
(`http_401`) rides in `input`; and correlating a result to its call **by
position** is wrong -- `tau2/episode.py` attributed every result in a
multi-call turn to the last call until it was joined on the `tool_call_id` the
environment echoes back.

The Python and Node bindings gained `ns=` on `record_tool_call` for this: it
was the only write on either surface that could not leave the session
namespace, so per-app namespaces were unreachable without a second handle,
which the single-writer registry refuses (`STO-E002`).

### 5. Namespace deliberately, and use prefix scope when the domain has parts

Two separations are load-bearing and every harness should have them:

- the **agent's own memory** (`retail`, `desk:persist`, `appworld`, …) versus
  the **harness's journals** (`agent:harness`). An eval score written into
  the agent's namespace is an agent that can read its own grade;
- **prefix scope** (`"org.*"` = `org` plus its `.`-descendants, resolved
  against `ns_reg`) where the domain has natural parts. A benchmark spanning
  several apps, tenants or document types should put them in child
  namespaces (`appworld.phone`, `appworld.spotify`, …) so a task can recall
  the part it is about, or `"appworld.*"` for all of it.

*Adherence today: **adopted where the domain has parts** (2026-09-08).*
`appworld` is the reference: nine child namespaces (`appworld.phone`, …), every
read through `"appworld.*"`. Because a prefix scope selects the base namespace
AND its descendants, a memory written flat still reads correctly through the
same query -- which is what made this safe to adopt mid-programme, and is
verified against run 1's own memories. `appworld/migrate_ns.py --swap` writes a
migrated COPY (a namespace is part of the content address, so there is no
in-place move), gives it the source's name, and moves the flat original to
`<name>.flat.db` -- nothing deleted, every sibling travelling together. Run 1's
four memories went through it and render identical blocks either side.
Recommendation grains do not migrate (engine-authored, query-only), so a
migrated memory starts with an empty review queue.

`receipts`, `tau2` and `persist` stay flat, each for a stated reason in its
`AREEV.md` -- for persist the reason is a real hazard: a family-scoped recall
would leak which family a task belongs to.

Every harness separates the harness journal (`agent:harness`) from the agent's
memory. Note the agent's memory must NOT be `agent:<x>`: the loop's
all-namespace evidence scan skips `agent:*` as governance metadata, so a memory
living there is invisible to DISCOVER.

### 6. Run the governed pass as a run, not as a function

```python
bench_run.learn(mem, db_path, llm_cmd, ground_cmd, decide, …)
#   propose -> review (client: the run PARKS) -> apply
```

Every track does the same four things when it learns, and every track drove
them from a Python function — so the one part of the loop that is a
GOVERNANCE claim, a person approving a rule as a different identity from the
one that proposed it, was a convention of the harness rather than something
the engine enforced and journaled.

As a Workflow grain the `review` node is a **client** executor: the run parks,
and answering it is `run_respond`, which **structurally refuses a responder
equal to the principal that triggered the ask** (`RUN-E012`) before any policy
check. The pass is journaled, so `run_trace` shows what ran and `run_verify`
byte-compares a replay — a learning claim can be checked against a journal
rather than against the harness's own log lines.

The edge is `decided == true`, not `approved == true`: `apply` records
rejections as well as approvals, and gating on approval would drop the
dismissals — the ledger showing what was turned down is half of what makes the
gate evidence.

**The reviewer is handed history, not just the batch.** GENERATION already
reads it: the engine dedupes a candidate against every recommendation already
recorded (`dedup_key`) and starts an exponential cooldown on rejection — 7d,
14d, 28d, capped at 90 — so a harness that dismisses through
`dismiss_recommendation` gets that for free. REVIEW did not. A reviewer
judging each proposal against only the rules in force will approve a
**rewording** of something it declined last month, because the reword carries
a different `dedup_key` and nothing else was looking. So `propose` also
returns `prior` (what this reviewer ruled on in the last 90 days) and
`outcomes` (the held-out series the Verify gate reads), both from saved
queries — `cal.REVIEW_REGISTRY`, registered by every track — and each
reviewer consults `prior` **before** spending a judge call.

The window is a literal in the query body: `SINCE $window` is refused
(`CAL-E059` → `CAL-E002`), so a different window is a different saved query.

**Two files, not one.** The journal lives in its own memory; the agent's stays
where it was. The driver holds the journal's writer handle for the whole run
and the host tools are subprocesses that open the agent memory — one file for
both is `STO-E002`.

*Adherence today: **adopted, and it is the only path** (2026-09-08).*
`memory.learn()` in every track delegates to `bench_run.learn()`; the
unjournaled review loop each one used to carry is gone, so there is no second
way to approve a rule. `scripts/bench_run.py` (the plan, its Trigger, and the
driver), `scripts/bench_govern.py` (the `--tool-cmd` seam),
`scripts/selftest_run.py` (the keyless proof: it parks, it refuses
self-approval, it applies, it replays).

The agent loops themselves are **not** runs and will not be: AppWorld's ReAct
loop, τ²'s conversation driver and PAST-Bench's runtime-adapter protocol each
own their control flow and call us. Making those `areev run` workflows would
mean maintaining forks of three upstream benchmarks, and the published numbers
would stop coming from stock harnesses.

The plan's Tool and Workflow grains carry a **pinned `created_at`**. Without
it, authoring the plan twice mints two Tool hashes, hence two `bindings`,
hence two Workflow hashes — and a Trigger points at a plan BY HASH and does not
follow heads, so the second authoring silently orphans it.

### If you skip one, say so where the number is published

A result produced by hand-rolled assembly still measures what it measures.
But when a track reports a **cost, token or truncation** effect, name which
budget produced it: CAL's `BUDGET n tokens`, or a character cap in the
harness. `PERSIST.md` attributes its token win to "the budgeted assembly";
that budget is `MAX_INJECT_CHARS` and a 6000-**character** loop in
`areev_backend.py`. The measurement stands; the mechanism is harness code,
and a reader should not have to grep to learn that.

## The rules that produced every result here

1. **A positive control runs before any model is asked to score.** Drive
   known-good input through the whole scoring path and confirm it scores. A
   zero from a broken harness and a zero from a weak agent are identical in a
   reward column, and this program has published a wrong conclusion for want
   of that check exactly once (`tau2/README.md`, the retraction).
2. **A ceiling probe runs before any learning run.** If the agent cannot do
   the task with everything in front of it, no loop can teach it the missing
   piece, and the arms would measure the model's ceiling instead.
3. **Pin every model leg** — model, provider and seed — and *verify the pin
   binds*. An unpinned provider can serve a different quantisation between
   arms, which is a harder confound to spot than a noisy agent because the
   ledger still looks plausible. An unverified pin is a comment.
4. **Never change a harness after seeing its result.** Record the reason for
   any change and which runs precede it.
5. **A score over a smaller denominator than the dataset is refused, not
   averaged.** A dead worker that silently drops its share of the tasks
   produces a real-looking number over the wrong base; check the count and
   fail loudly.
6. **A null publishes as a null**, with the ledger, the gate's reasons and
   the spend. No figure is published that nobody measured.
7. **Keyless floor.** Every harness carries a `selftest.py` (or a `--mock`
   path) that exercises the plumbing with no API key, so CI catches a harness
   that rotted between paid runs. It proves plumbing, never learning — say
   so where it prints.

## Practical notes

- **Runs live outside the repo** (`~/mg/local/areev-runs/…`); only counts
  travel. `.gitignore` keeps `*.db` out — `data/demo.db` is the one exception
  and belongs to the README, not to a bench.
- **The store is single-writer per file.** A second handle fails `STO-E002`
  even for a reader, so parallel workers each need their own copy. Open a
  memory the arm must not change with `read_only=True`: writes then fail
  `STO-E004` and the freeze is the store's guarantee rather than the
  harness's intention.
- **Statistics have one source of truth.** `scripts/aba_stats.py`'s
  `mcnemar_exact` is imported, not reimplemented; `aba_arm_stats.py
  --selftest` gates it in CI.
- **`scripts/` holds the shared adapters** — `openrouter_loop.py` (loop LLM
  legs), `openrouter_toolcall.py` (tool-calling chat; `--base-url`/`--key-env`
  send an OpenAI model to api.openai.com directly, which is cheaper than
  routing it through OpenRouter), `openai_chat.py`, `tee_llm.py` (metering).
  Add to these rather than writing a fourth adapter.
- **The Areev surfaces are shared too**, and are not to be re-derived per
  track: `cal_assemble.py` (`install` / `section` / `block` / `rows`, plus the
  `guarded_template` and `saved_query` builders), `bench_run.py` +
  `bench_govern.py` (the governed pass as a run), `parity_check.py` (the
  byte-parity gate — add a case when you replace a renderer) and
  `selftest_run.py`. Both gates are keyless:

  ```bash
  python3 scripts/parity_check.py     # every block CAL assembles == the block it replaced
  python3 scripts/selftest_run.py     # the governed pass parks, refuses self-approval, replays
  ```
