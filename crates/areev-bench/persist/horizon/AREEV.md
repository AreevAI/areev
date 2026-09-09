# persist/horizon — how this bench uses Areev

The Horizon leg of the persist track: an assistant whose only thing outliving a
session is an Areev file, ingesting a trace and learning from it under review.
Start from [`../../BENCH-TEMPLATE.md`](../../BENCH-TEMPLATE.md).

## Namespaces

| namespace | what lives there |
|---|---|
| `desk:horizon` | the trace as grains, the approved lessons, the durable facts |
| `areev-loop` | the loop's own recommendation records (read for the ledger) |

Flat. Harbor tasks are not partitioned in a way a child namespace would make
recallable.

## Grains

| grain | shape |
|---|---|
| Tool | `record_tool_call`, one per call/output pair in the ingested trace — this track did it properly from the start |
| `event` | everything else in the trace, one session per UTC day |
| `fact` | `lesson` / `note` — approved by the reviewer |
| `fact` | any other relation — a durable fact the loop stored, rendered `subject relation: object` |
| `fact` | `mg:eval_run` — an eval score, and it must never render into the agent's prompt |

## The prompt

Two saved queries and two templates, registered in the memory by `REGISTRY` in
`areev_agent/agent.py` and installed on every writable open.

| saved query | selects | renders |
|---|---|---|
| `horizon_lessons($ns)` | `relation IN ("lesson", "note")` | `  - {{grain.object}}` |
| `horizon_other($ns)` | `relation NOT IN ("lesson", "mg:eval_run", "note")` | `  - {{grain.subject}} {{grain.relation}}: {{grain.object}}` |

`lessons_block(db)` frames them with the header sentence and returns the empty
string when nothing is approved — both templates guard on
`assembly.grain_count`, so a memory with no lessons contributes *nothing* to
the system prompt rather than a header promising lessons that are not there.

Neither query orders: recall order (newest first) is what this block has always
rendered in, and the move is byte-for-byte.
`scripts/parity_check.py horizon` gates it, including that an `mg:eval_run`
score never reaches the agent's own prompt.

`live_lessons` stays a list, because the **reviewer** needs the individual
strings for its dedup check. It excludes the same relation the block excludes —
if the two ever diverge, a reviewer would be judging against a set the agent
cannot see.

## Skipped, and why

| surface | status |
|---|---|
| ASSEMBLE | adopted |
| saved queries | adopted |
| CAL rendering | adopted, via `DEFINE TEMPLATE` |
| tool-call lifecycle | adopted before this change |
| prefix namespaces | not applicable |
