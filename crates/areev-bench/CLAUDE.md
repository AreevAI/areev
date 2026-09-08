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

## Use the product's own surfaces, or say which one you measured

A harness exists to show what Areev does. Every step it hand-rolls in Python
is a step the published number does not demonstrate — and any claim about
that step then belongs to the harness, not to the engine. The five below are
the ones benches here keep reimplementing. Apply the ones that fit; where one
does not fit, say why in the track's design document.

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

*Adherence today: **none**.* No harness in this crate uses `ASSEMBLE`; it
appears only in `RESULTS.md`'s latency table and `src/bin/cal_validate.rs`.

### 2. Register the read as a saved query

```sql
DEFINE QUERY "agent_block"($ns) AS { ASSEMBLE … }
RUN "agent_block"($ns = "appworld")
```

Saved queries persist as `qry:<name>` meta rows: they **travel with the
memory file** and **replicate through bundles**. A read that lives as a
Python string literal means a memory handed to someone else does not carry
how to read it — the knowledge is stranded in the harness.

*Adherence today: **none**.* `persist/pastbench/areev_backend.py` mentions
`DEFINE QUERY` only to classify a loop proposal as a `query_revision`; it
defines none.

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

*Adherence today: **partial**.* `receipts/structure.py` is the reference —
it renders the same grains through `markdown`, `sml`, `toon` and `json` as a
position/format ablation, and its docstring is honest that the *published*
prompt is hand-assembled elsewhere. Every production prompt path
(`receipts/memory.py`, `tau2/memory.py`, `appworld/memory.py`,
`persist/…/areev_backend.py`) uses `FORMAT json` plus host formatting.

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

*Adherence today: **partial**.* `persist/pastbench/areev_backend.py` and
`persist/horizon/areev_agent/agent.py` do it properly, as does
`src/selfimprove/memory.rs`. `tau2/memory.py`, `receipts/memory.py` and
`appworld/memory.py` flatten it.

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

*Adherence today: **partial**.* Every harness separates the harness journal
from the agent memory. **None** uses prefix scope, and at least one paid for
it: in `APPWORLD.md` the gate approved a rule scoped to `phone.*` which then
sits in an undifferentiated pile, where per-app namespaces would have made
its scope mean something at recall time.

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
