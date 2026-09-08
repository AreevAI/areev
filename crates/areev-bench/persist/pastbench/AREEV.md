# persist — how this bench uses Areev

An assistant on PAST-Bench, whose only thing that outlives a session is an
Areev file. This is the **least CAL-expressible** prompt of the five tracks,
and that is the useful thing about it: the three reads that cannot move into
CAL are named here with their error codes, so the next bench knows what not to
attempt. Start from [`../../BENCH-TEMPLATE.md`](../../BENCH-TEMPLATE.md).

## Namespaces

| namespace | what lives there |
|---|---|
| `desk:persist` | notes, user profile, skills, plans, session events |
| `agent:harness` | eval-run scores only |

**Not `agent:<x>` for the agent's own memory.** The loop's all-namespace
evidence scan deliberately skips every `agent:*` namespace as governance
metadata, so a memory living there is invisible to DISCOVER — the first smoke's
loop passes saw zero evidence for exactly this reason.

## Grains

| grain | shape |
|---|---|
| `fact` | `note` / `lesson` on `assistant`; `profile` on `user`; anything else renders as `subject relation: object` |
| `skill` | a procedure the assistant wrote for itself; retired by writing `description: "retired"` |
| `workflow` | a plan the loop authored, carrying a `name` |
| `event` | one per session turn, `session_id`-threaded |
| Tool | `record_tool_call` — already correct before this change |

## The prompt: one section of four is CAL

`prompt.py` holds the whole injected block and imports **no PAST-Bench**, so it
loads (and is parity-checked) without the benchmark installed.
`areev_backend.py` imports it.

| section | assembled by | why |
|---|---|---|
| `### User profile` | **CAL** — `persist_profile($ns)`, `BUDGET 1200 tokens` | expressible |
| `### Notes` | the harness | `valid_to` |
| `### Skills` | the harness | `description` |
| `### Earlier sessions` | the harness | grouping + extraction |

The three, precisely:

- **`valid_to`.** A note may declare an expiry, and an expired note must not
  render. `valid_to` is not a queryable field on facts (`CAL-E060`) and not a
  template variable (`CAL-E042`), so neither the filter nor the
  `(until 2026-10-01)` label can move into the query.
- **`description` on skills.** A retired skill is marked by writing
  `description: "retired"`, and `description` is not filterable on skills
  (`CAL-E060`; `DESCRIBE FIELDS skills` lists `instructions` and `when_to_use`
  but not `description`). Filtering on `object` instead **does not work and
  does not warn** — it returns every row — so pushing the filter down would
  silently put retired skills back in the prompt.
- **Session titles.** One line per *session*, whose title is a regex over that
  session's first event. CAL has `GROUP BY`, but it reorders rows rather than
  projecting one row per group, and it has no text extraction.

The profile section's template uses an **unguarded** `HEADER` plus a
`{{^assembly.grain_count}}` `FOOTER`, so the heading always renders and an
empty section says `- (none yet)` — the opposite of the other tracks, whose
sections must vanish when empty. Both shapes are in `cal_assemble.py`.

## The budget, stated honestly

`PERSIST.md` reports a token effect. Name the budget that produced it:

- the profile section is bounded by CAL's `BUDGET 1200 tokens`, counted by the
  engine's own estimator;
- **everything else is bounded by `MAX_INJECT_CHARS = 12_000`, a CHARACTER
  cap** applied to the composed block in `_inject_memory`.

A cap counted in characters is not a token budget, and two arms capped that way
are comparable only by coincidence. Any sentence in `PERSIST.md` about tokens
must say which of the two it means.

## Skipped, and why

| surface | status |
|---|---|
| ASSEMBLE | one of four sections; the other three are blocked by the CAL gaps above |
| saved queries | adopted for the section that is expressible |
| CAL rendering | adopted for that section, via `DEFINE TEMPLATE` |
| tool-call lifecycle | **already adopted** before this change — this track and `src/selfimprove/memory.rs` were the two that did it properly |
| prefix namespaces | not adopted. PAST-Bench families are a plausible split (`desk:persist.<family>`), but families are the *unit of evaluation*, and giving each its own namespace would let a family-scoped recall become a hint about which family a task belongs to |
