# appworld — how this bench uses Areev

A coding agent completes everyday tasks by writing Python against the APIs of
nine apps. It is the one track whose domain has natural parts, so it is the
reference for **prefix namespaces**. Start from
[`../BENCH-TEMPLATE.md`](../BENCH-TEMPLATE.md).

The published result is a **pre-registered null** (`../APPWORLD.md`). Nothing
here changes that; what changed is the memory's shape, and the reason is
recorded in that document.

## Namespaces — the part worth copying

```
appworld              the base: rules, episode outcomes, unattributed errors
appworld.phone        one child per app
appworld.spotify
appworld.venmo        … amazon, file_system, gmail, simple_note, splitwise, todoist
agent:harness         eval-run scores only
```

Every read uses `NS_SCOPE = "appworld.*"`, which selects the base namespace
**and** its descendants. That is what makes the change safe: a memory written
flat — every run before 2026-09-08, including run 1's — still reads correctly
through the same query.

This is the defect `APPWORLD.md` records. The gate approved a rule that scoped
itself to `phone.*`, and it then sat in an undifferentiated pile where the
scope it named meant nothing at recall time. Now it means a namespace, and a
task about the phone can recall the phone's evidence.

`ns_for(app)` sends an **unattributed** error to the base namespace rather than
minting one from unvalidated text: a write is what mints a namespace, so a typo
there is accepted by every surface and found by none.

Migrating an existing memory into this shape:

```bash
python3 appworld/migrate_ns.py --src OLD.db --dry-run   # report the plan
python3 appworld/migrate_ns.py --src OLD.db --swap      # migrate and take its name
```

It writes a **new** memory and never writes into the source: grains are
immutable and content-addressed, so re-namespacing mints new addresses and
there is no in-place move. `--swap` then gives the migrated memory the
source's name and moves the flat original to `<name>.flat.db` — nothing is
deleted, and every sibling travels together (`-wal`, `.blobs/`,
`.telemetry.db`), because a memory whose WAL was left behind has a torn tail.

Run 1's four memories were migrated this way and checked to render identical
prompt blocks either side of the swap. **Recommendation grains do not
migrate** — they are engine-authored and query-only — so a migrated memory
starts with an empty review queue and run 1's ledger stays in the `.flat.db`.

## Grains

| grain | shape |
|---|---|
| Tool | `record_tool_call`, one per API error, into `appworld.<app>` |
| `fact` | `lesson` / `fails_with` — a supervisor-approved rule |
| `fact` | `outcome` on `episode_<task_id>` — error counts, apps touched, steps, how it ended |
| `fact` | `mg:eval_run` in `agent:harness` |

Deliberately **not** recorded: the task's ground-truth solution, its evaluation
code, or its score. AppWorld hides the unit tests from the agent and so does
this. The score reaches memory in exactly one place, written by the harness
under `agent:harness`, because the Verify gate needs an outcome series and an
agent that could read its own grade would be a different experiment.

`failure_cause` is a closed enum, so the harness's own taxonomy (`http_401`,
`TypeError`) rides in the call's `input`, where it stays queryable.

## The prompt

Two arms, two blocks.

| arm | saved query | assembled by |
|---|---|---|
| governed | `appworld_rules($scope)` | CAL, end to end |
| passive | `appworld_errors($scope)` | CAL selects; **the harness tallies** |

The governed block is a plain `ASSEMBLE` with `ORDER BY object ASC`,
`WITH dedup(object)` and a `{{#if assembly.grain_count}}`-guarded heading.

The passive block **moved into CAL on 2026-09-09**, once #209 landed
`GROUP BY … COUNT` and the `group.*` template variables:

```sql
ASSEMBLE "past API errors" FOR "the AppWorld coding agent" FROM
  ranked: (RECALL tools WHERE namespace = $scope AND is_error = true
           LIMIT 400 GROUP BY tool_name, tool_content COUNT LIMIT 12)
BUDGET 16000 tokens
FORMAT TEMPLATE appworld_errors_tpl
--  - ({{group.count}}x) {{group.key.0}}: {{group.key.1}}
```

### What the block ranks, and why that is the whole question

`(endpoint, message)` pairs, most frequent first, cut at twelve — the same
ranking the harness used to compute in Python, and the one runs 1 and 2 were
produced under:

```
- (6x) phone.login: Response status code is 401
```

For one day it ranked **endpoints only**. #209 gave CAL a per-group count but
not a composite key, and three engine gaps stood between that block and this
one — a template could not render a Tool grain's body (`tool_content` was
`CAL-E042` and `content` resolved empty, though `FORMAT markdown` printed it
happily), `GROUP BY` took one field, and a `LIMIT` after `COUNT` did not bind,
so the block listed every endpoint rather than the top twelve. All three were
filed as [#217](https://github.com/AreevAI/areev/issues/217) and all three are
closed; the block is CAL end to end and ranks what it always ranked.

**Say this wherever a number produced under it is published.** The passive arm
is the BASELINE the governed arm has to beat, so the direction of any change
to it matters more than its size: dropping the error message would make that
baseline *weaker*, which flatters the governed arm — a bias in exactly the
direction that would make the loop look better. No run was produced under the
endpoint-only block. If one ever is, it is not comparable to runs 1 and 2 and
must say so.

The selection had been a saved `RECALL` rather than an `ASSEMBLE` for a while,
which is worth keeping in the record: `ASSEMBLE` applies a token budget whether
or not one is asked for (default 4000), and wrapping a pure selection in one
returned **79 of 229** grains. Since #208 that drop announces itself as
`CAL-W017`, and `cal.section` raises on it.


## The governed pass, and what the reviewer reads

`memory.learn()` delegates to `bench_run.learn()`: `propose → review → apply`,
the review node a client executor the run parks on, answered by
`user:supervisor` through `run_respond` (`RUN-E012` refuses self-approval).

`review_pending` receives `pending`, `prior` (what this supervisor already
ruled on in the last 90 days, from the `bench_review_history` saved query) and
`outcomes` (the held-out series). `prior` matters here more than anywhere: run
1 put 100 proposals to the gate and 44 were refused with reasons, so a second
run under a rewording proposer would otherwise re-litigate decisions already
made. The engine's cooldown is keyed on `dedup_key` and cannot catch a
reworded restatement.

## Skipped, and why

| surface | status |
|---|---|
| ASSEMBLE | adopted for both arms — the passive arm's frequency ranking moved once #209 and #217 landed, and lost nothing on the way |
| saved queries | adopted, both arms |
| CAL rendering | adopted for both, via `DEFINE TEMPLATE` |
| tool-call lifecycle | adopted |
| prefix namespaces | **adopted — this is the reference track for it** |
