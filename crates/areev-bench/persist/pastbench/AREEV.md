# persist — how this bench uses Areev

An assistant on PAST-Bench, whose only thing that outlives a session is an
Areev file. This was the **least CAL-expressible** prompt of the five tracks,
and that is the useful thing about it: the three reads that could not move into
CAL were named here with their error codes — and two of the three were fixed in
the engine as a result (1.7.4, #206 and #207). One is still open. A gap named
precisely enough to quote an error code is a gap someone can close. Start from [`../../BENCH-TEMPLATE.md`](../../BENCH-TEMPLATE.md).

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
| `### Notes` | the harness | `valid_to` — **closed in 1.7.4 (#206)**, movable |
| `### Skills` | the harness | `description` — **closed in 1.7.4 (#207)**, movable |
| `### Earlier sessions` | the harness | first-of-group — still open |

The three, precisely — **two of them are closed as of 1.7.4**, and the table
above is what the harness did before that. Filing them is what got them fixed;
they are kept here because the reasoning is the record of why the harness looks
the way it does, and because the third is still open.

- **`valid_to` — CLOSED ([#206](https://github.com/AreevAI/areev/issues/206)).**
  A note may declare an expiry, and an expired note must not render. `valid_to`
  *was* neither a queryable field on facts (`CAL-E060`) nor a template variable
  (`CAL-E042`), so neither the filter nor the `(until 2026-10-01)` label could
  move into the query. All four world-time fields now filter, sort and render
  on every type, so the section is expressible:

  ```sql
  RECALL facts WHERE namespace = "desk:persist"
    AND (valid_to IS NULL OR valid_to > $now)
  ```

  The `IS NULL` leg is load-bearing — a note with no declared expiry never
  lapses, and dropping it would return only the notes that *do* expire.
- **`description` on skills — CLOSED ([#207](https://github.com/AreevAI/areev/issues/207)).**
  A retired skill is marked by writing `description: "retired"`, and
  `description` was not filterable on skills (`CAL-E060`) despite being
  required on every Skill — a registry gap, now fixed. The *second* half was
  worse and is the reason this one was worth filing: filtering on `object`
  instead did not work **and did not warn**, returning every row, so pushing
  the filter down would have put retired skills back in the prompt silently.
  Negations now fail closed on a field the grain does not carry, so that
  cannot happen on any type or any field.
- **Session titles — STILL OPEN.** One line per *session*, whose title is a
  regex over that session's first event. The reason narrowed but did not
  vanish. CAL now has text extraction
  ([#210](https://github.com/AreevAI/areev/issues/210):
  `match`, `between`, `split`, `first_line`, …) and per-group *counts*
  ([#209](https://github.com/AreevAI/areev/issues/209): `GROUP BY <field>
  COUNT`). What is still missing is **first-of-group** — one row per group
  carrying that group's earliest member — so the read remains the harness's.
  Recorded as a separate decision in
  `docs/oms-1.7-amendments-cal-expressiveness.md`.

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
