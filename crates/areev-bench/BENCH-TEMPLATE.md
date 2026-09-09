# Starting a new benchmark track

Copy this. Every track in this crate follows it, and each one records its own
answers in a `AREEV.md` beside its harness code — `receipts/AREEV.md`,
`tau2/AREEV.md`, `appworld/AREEV.md`, `persist/pastbench/AREEV.md`. Read the
closest one to what you are building before writing anything.

The point of the shape is that a harness should **demonstrate the engine**, not
reimplement it. Every step hand-rolled in Python is a step the published number
does not demonstrate, and any claim about that step then belongs to the harness
rather than to Areev. `CLAUDE.md` in this directory states the five surfaces;
this file is how you wire them.

---

## 1. What a track is made of

```
<track>/
  AREEV.md      how THIS bench uses Areev: namespaces, grains, saved queries,
                the plan, and what it deliberately does not use and why
  memory.py     the bridge: REGISTRY (templates + saved queries), the write
                path, the read path, the reviewer's rubric
  agent.py      the model call; knows nothing about Areev
  run.py        the experience phase and the learn cadence
  evaluate.py   the paired held-out evaluation
  selftest.py   the keyless floor (no API key), run by CI
<TRACK>.md      the design document, at crate level, written BEFORE the run
                and never revised to match the result
```

Shared machinery you should be **using, not rewriting** (`scripts/`):

| file | what it gives you |
|---|---|
| `cal_assemble.py` | `install` / `section` / `block` / `rows`, and the `guarded_template` + `saved_query` builders |
| `bench_run.py` | the governed-learning Workflow, its Trigger, and `govern()` to drive it |
| `bench_govern.py` | the host-tool seam `areev run` invokes (`--harness <track>`) |
| `parity_check.py` | the byte-parity gate — add a case for your track |
| `selftest_run.py` | the keyless proof that the governed pass really parks and refuses self-approval |
| `aba_stats.py` | `mcnemar_exact`. Import it. Do not reimplement a statistic. |
| `openrouter_loop.py`, `openrouter_toolcall.py`, `openai_chat.py`, `tee_llm.py` | the model legs |

---

## 2. Namespaces, before anything else

Two separations are load-bearing:

```python
NS         = "yourdomain"        # the agent's own memory
NS_SCOPE   = "yourdomain.*"      # base + every child, for reads
HARNESS_NS = "agent:harness"     # the harness's journals — NEVER the agent's
```

An eval score written into the agent's namespace is an agent that can read its
own grade. And `agent:*` is skipped by the loop's all-namespace evidence scan,
so a memory that lives there is invisible to DISCOVER — which is why the
agent's memory must **not** be `agent:<x>` (PAST-Bench's first smoke saw zero
evidence for exactly this reason).

If the domain has parts — apps, tenants, document types — give them child
namespaces and read the prefix:

```python
def ns_for(part):
    return "%s.%s" % (NS, part) if part in PARTS else NS   # never mint from unvalidated text
```

`"yourdomain.*"` matches `yourdomain` **and** its descendants, so a memory
written flat still reads correctly through the same query — which is what makes
this safe to adopt mid-programme.

---

## 3. The prompt is a saved query in the file

Not a Python f-string. A `DEFINE QUERY` persists as a `qry:<name>` meta row: it
travels with the `.db`, replicates through bundles, and is visible from the
CLI, MCP and the console. A read that lives in the harness means a memory
handed to someone else does not carry how to read it.

```python
import cal_assemble as cal

SECTION_CAP = 500

REGISTRY = [
    cal.guarded_template("yourdomain_rules_tpl", "## RULES", "- {{grain.object}}"),
    cal.saved_query(
        "yourdomain_rules", ["scope"],
        '  ASSEMBLE "operating rules" FOR "the agent" FROM\n'
        '    rules: (RECALL facts WHERE namespace = $scope\n'
        '            AND relation IN ("lesson", "fails_with")\n'
        '            ORDER BY object ASC LIMIT %d)\n'
        '  FORMAT TEMPLATE yourdomain_rules_tpl\n'
        '  WITH dedup(object)' % SECTION_CAP,
        "the approved rules, as the agent reads them"),
]

def rules_block(db):
    return cal.section(db, "yourdomain_rules", {"scope": NS_SCOPE}, cap=SECTION_CAP)
```

Four things in that snippet are doing real work:

- **`{{#if assembly.grain_count}}`** (inside `guarded_template`) makes an empty
  section render to the **empty string**. A governed arm whose rules were rolled
  back must show *nothing* — not a heading announcing rules that are gone —
  or the paired evaluation stops being causal.
- **`ORDER BY object ASC` inside the source parentheses** is the `sorted(...)`
  you would have written in Python. It orders *within* a section; a
  statement-level stage cannot reorder an assembly and is skipped with
  `CAL-W016`.
- **`WITH dedup(object)`** is the `set(...)`. It keeps the first occurrence, so
  it preserves whatever order the source produced.
- **`cap=`** turns "the scan hit its LIMIT" into a raise. A prompt silently
  missing its oldest rules is the failure `receipts` hit at 300 and would have
  hit again at 1000.

Install the registry on the **write** path only — `DEFINE` is a write, and a
frozen arm reads a copy read-only (`STO-E004`):

```python
db = areev.Areev(db_path, ns=NS, actor=actor, read_only=read_only)
if not read_only:
    cal.install(db, REGISTRY, db_path=db_path, ns=NS)
```

The queries travel with the file, so a read-only arm finds them already there.

### State the budget

`ASSEMBLE` applies a token budget whether or not you ask for one — default
4000, ceiling 16000 — so write `BUDGET n tokens` rather than inheriting the
default. A budget that binds announces itself as `CAL-W017` (since 1.7.4), and
`cal.section` / `cal.rows` **raise** on it: a prompt section that lost rows is
a wrong prompt, not a warning.

And use `ASSEMBLE` only for text a model reads. A pure selection — one your
harness will post-process anyway — belongs in a saved `RECALL`, which carries
no budget semantics. Wrapping one in an `ASSEMBLE` is how AppWorld's error
read returned 79 of 229 grains.

### Pick the format deliberately

`markdown`, `sml`, `toon`, `json`, or a registered `TEMPLATE`. Which is
cheapest depends on how many rows there are — `toon`'s tabular header starts
paying at roughly four same-shaped rows. Measure on your own memory
(`python3 scripts/cal_assemble.py --db PATH:NS`), do not assume. `FORMAT json`
followed by rebuilding the text in Python is the anti-pattern; if you must do
it, say so where the number is published and use `cal.rows()`, which at least
keeps the selection in the query.

---

## 4. A tool call is a call, not a string

```python
db.record_tool_call(
    name, result, is_error,
    thread=task_id, call_id=tc.id, input=json.dumps(args),
    status="failed" if error else "completed",     # enum: pending|completed|failed
    failure_cause="executor_error",                # enum: timeout|executor_error|
                                                   # schema_validation_failed|user_aborted|unknown
    executor_kind="host",
    ns=ns_for(app),                                # targets a child namespace
)
```

`add("tool", {...})` flattens the lifecycle: it keeps the result text and
discards the input that produced it, the call/result correlation, the status and
the failure cause — exactly the fields `areev_tool_provenance` and
`step_actions` exist to answer with. Correlate results to calls **by the id the
environment echoes back**, never by position: a turn issuing several calls
otherwise attributes every result to the last one.

Note the two enums are closed. Your own taxonomy (`http_401`, `TypeError`)
rides in `input`, where it stays queryable.

---

## 5. Grain types, by what the thing IS

| you have | grain |
|---|---|
| a rule, a convention, a learned value | `fact` (`relation = "lesson"` / `"fails_with"` / your own) |
| something a person said, in their words | `observation` (`observer_type = "human"`) |
| a tool call and its result | `record_tool_call` (never `add("tool", …)`) |
| a multi-step procedure the agent should follow | `skill` |
| the plan a run executes | `workflow` |
| the standing rule that starts a plan | `trigger` |
| one episode's outcome record | `fact`, on an `episode_<id>` subject |
| an eval score | `fact` `mg:eval_run`, in **`agent:harness`** |

Keep the episode record to the episode's *observable shape* plus
accepted/rejected. Never the gold actions, never the reward's reason, never
which withheld rule it needed. Those name the answer.

---

## 6. The governed pass is a run, not a function

```python
import bench_run

TRACK = "yourtrack"          # the directory `bench_govern.py --harness` loads

def learn(db_path, llm_cmd, ground_cmd, judge=None, policy=None, verbose=True,
          full_sweep=False):
    return bench_run.learn(
        sys.modules[__name__], db_path, llm_cmd, ground_cmd,
        decide=lambda ask: review_pending(db_path, ask, judge),
        policy=policy, verbose=verbose, full_sweep=full_sweep)
```

`propose → review → apply`, where `review` is a **client** node: the run parks,
and answering it is `run_respond`, which **structurally refuses a responder
equal to the principal that triggered the ask** (`RUN-E012`). That is the
separation of duties every track used to implement by convention. The pass is
journaled, so `run_trace` shows what ran and `run_verify` byte-compares a
replay — a learning claim can be checked against a journal instead of against
your own log lines.

Make this the **only** path. If your `learn()` keeps its own review loop
beside this one, you have two ways to approve a rule and only one of them is
audited.

The edge is `decided == true`, not `approved == true`: `apply` records
rejections too, and the ledger of what was turned down is half the evidence.

**Your reviewer judges against history, not just the batch.** `decide` is
handed the whole ask — the pending proposals, `prior` (what this reviewer
ruled on in the last 90 days) and `outcomes` (the held-out series). Consult
`prior` *before* spending a judge call:

```python
earlier = cal.restates_a_decision(text, declined, normalize_rule, _content_words)
if earlier is not None:
    ok, why = False, "already declined: %s" % earlier["text"][:110]
```

The engine already does the generation half — it dedupes on `dedup_key` and
starts an exponential rejection cooldown (7d, 14d, 28d, capped at 90) — but a
**reworded** restatement carries a different key, so the cooldown never sees
it. That is the case this check exists for.

The held-out evaluation is deliberately *not* a node: it is a separate phase,
on its own cadence, and a node that skipped itself every pass is furniture.

**Two files.** The journal lives in `runs.db`, the agent's memory in its own
file. The driver holds the journal's writer handle for the whole run, and the
host tools are subprocesses that open the agent memory; one file for both is
`STO-E002`. When the agent memory is a Postgres schema (`AREEV_BENCH_DB`), the
journal stays a local file — the provisioned role has no CREATE for a second
schema — and `learn(policy_dir=…)` names the directory it and the pass's
policy file go to, since a DSN has no "beside".

Declare the cadence as a Trigger grain (`bench_run.author_trigger`) even when
the harness is its evaluator: a reader of the memory can then see what was
supposed to start the plan without reading your Python.

---

## 7. The rules that produced every result here

Restated because they are not optional. The long form is in `CLAUDE.md`.

1. **A positive control runs before any model is asked to score.** A zero from a
   broken harness and a zero from a weak agent are identical in a reward column.
   This programme published a wrong conclusion for want of that check once.
2. **A ceiling probe runs before any learning run.**
3. **Pin every model leg** — model, provider, seed — and *verify the pin binds*.
4. **Never change a harness after seeing its result.** If you must, record the
   reason and which runs precede it.
5. **A score over a smaller denominator than the dataset is refused, not
   averaged.**
6. **A null publishes as a null**, with the ledger, the gate's reasons and the
   spend.
7. **Keyless floor.** `selftest.py` with no API key, run by CI. It proves
   plumbing, never learning — say so where it prints.

---

## 8. Before you run anything

- [ ] `<TRACK>.md` written, at crate level, **before** the run
- [ ] `<track>/AREEV.md` written: namespaces, grains, saved queries, plan, and
      every surface you skipped **with its reason**
- [ ] `REGISTRY` installed on the write path; read-only arms verified
- [ ] a case added to `scripts/parity_check.py` if you replaced a renderer
- [ ] `python3 scripts/selftest_run.py` still passes
- [ ] `python3 <track>/selftest.py` passes with no API key
- [ ] runs land outside the repo (`~/mg/local/areev-runs/…`); only counts travel
