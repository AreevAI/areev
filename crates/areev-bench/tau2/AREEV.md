# tau2 — how this bench uses Areev

A retail desk agent deployed on an incomplete understanding of its own domain.
The environment, customer, orchestrator and reward are τ²-bench's, unchanged
and not vendored; this directory is a bridge. Start from
[`../BENCH-TEMPLATE.md`](../BENCH-TEMPLATE.md).

## Namespaces

| namespace | what lives there |
|---|---|
| `retail` | the desk's rules, conventions, tool calls, customer pushback, episode outcomes |
| `agent:harness` | eval-run scores only |

Flat. τ²-retail is one domain with one tool catalogue; there is nothing to make
child namespaces out of.

## Grains

| grain | shape | note |
|---|---|---|
| `fact` | `lesson` / `fails_with` on `retail_desk` | the approved rules |
| `fact` | any other relation on a non-episode subject | a learned convention (`refund_window: 30 days`) |
| `fact` | `outcome` on `episode_<task_id>` | the episode's observable shape plus accepted/rejected — **never** the gold actions, the reward's reason, or which withheld clause it needed |
| `observation` | the customer's own sentence, when it was pushback rather than a request | the rarest evidence a memory holds |
| Tool | `record_tool_call`, one per call | see below |

## The prompt

Two saved queries, one template each, joined with a single newline (this block
is one flat list of lines, not headed sections).

| saved query | selects |
|---|---|
| `retail_rules($ns)` | `relation IN ("lesson", "fails_with")`, `ORDER BY object ASC`, `WITH dedup(object)` |
| `retail_conventions($ns)` | everything else on a non-episode subject, `NOT subject STARTS WITH "episode_"`, `ORDER BY relation ASC` |

**One deliberate change from the renderer this replaced.** The retired version
sorted rules and conventions into ONE alphabetical list, interleaving "always
confirm before cancelling" with "refund window: 30 days". CAL orders *within* a
section, not across sections, so the two shapes are now two runs of lines. The
same grains reach the model in the same per-section order; only the
interleaving changed. `scripts/parity_check.py tau2` asserts the line SET is
identical and that each section is ordered. **No τ² learning number is
published** (see `README.md`, "Result"), so nothing published moves with it.

## Tool calls

`record_tool_call`, with the input, the call/result id, the status and the
failure cause — not `add("tool", …)`, which kept only the result text.

Correlation was also fixed here, and it was a real defect: `episode.py` paired
a `ToolMessage` with the **last** call in the turn, so a turn issuing several
calls attributed every result to one of them. It now joins on the
`tool_call_id` the environment echoes back. Recorded under rule 4 of
`CLAUDE.md`: the change is a correctness fix, it precedes any published τ²
learning number (there is none), and it is why the ceiling-probe numbers in
`README.md` should not be compared byte-for-byte against a re-run.

`failure_cause` is a closed enum, so the environment's refusal text is the
*result*; `executor_error` is the cause.

## The governed pass, and what the reviewer reads

`memory.learn()` delegates to `bench_run.learn()`: `propose → review → apply`,
the review node a client executor the run parks on, answered by
`user:supervisor` through `run_respond` (`RUN-E012` refuses self-approval).
There is no second, unjournaled review loop.

`review_pending` receives `pending`, `prior` (what this supervisor already
ruled on in the last 90 days, from the `bench_review_history` saved query) and
`outcomes` (the held-out series). `prior` is consulted before the judge: the
engine's rejection cooldown is keyed on `dedup_key`, so it cannot catch a
reworded restatement of a declined rule.

## Skipped, and why

| surface | status |
|---|---|
| ASSEMBLE | adopted |
| saved queries | adopted |
| CAL rendering | adopted, via `DEFINE TEMPLATE` |
| tool-call lifecycle | adopted |
| prefix namespaces | not applicable — one domain |
