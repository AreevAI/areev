# selfimprove — how this bench uses Areev

The A/B/A/B causal proof (`../../SELFIMPROVE.md`), and the only harness in this
crate written in Rust. It talks to the store through
`areev_loop::OmsSubstrate` rather than through CAL text, which is deliberate:
this track exists to measure the *loop*, and the substrate seam is what the
loop itself runs on. Start from
[`../../BENCH-TEMPLATE.md`](../../BENCH-TEMPLATE.md).

## What it already does right

- **Tool calls are calls.** `record_tool_call`, with the input round-tripping
  as parsed JSON. This file and PAST-Bench were the two that did it properly
  before the 2026-09-08 sweep.
- **The prompt is assembled from LIVE grains on every read**, with
  `ReadOpts::default()` (live-only), so a rolled-back lesson is tombstoned and
  stops rendering. That is the honesty lever the whole A/B/A/B rests on: the
  arms differ by what apply/rollback did to the memory, not by a harness flag.
- **Deterministic prompt bytes**: lessons sorted by `(subject, object)` and
  deduped, tool grains ordered `created_at_ms` then `hash`. Two arms that
  reordered between runs would not be comparable.

## Why the prompt is NOT an ASSEMBLE

The `fails_with` section renders a stored failure signature by **parsing
`object` as JSON**, pulling `error.code` out, removing that key, and
re-serialising the remainder:

```text
- `search_api` repeatedly failed with error code `rate_limited` — {"retry_after":30}
```

A CAL template can now *read into* a JSON payload — `{{grain.object |
get("error.code")}}` ([#211](https://github.com/AreevAI/areev/issues/211),
1.7.4) — and `WHERE object.error.code = "rate_limited"` filters on one. What it
still cannot do is **reshape**: parse a payload, remove a key, and re-serialise
the remainder. That was declined deliberately rather than left undone — it is a
transformation, it needs its own sandbox and spec, and its main use is better
served by writing the right shape in (see
`docs/oms-1.7-amendments-cal-expressiveness.md`,
"What is refused").

So this section still cannot move, for the narrower reason, and moving only the
`rules` section would leave one renderer split across two mechanisms — the
parallel system this sweep exists to remove.

The clean fix is to store the code and the detail as **separate fields** at
write time, which a template could then render. That changes the stored fields,
therefore the content addresses, therefore the evidence behind the published
A/B/A/B result. Not a change to make mid-programme; a note for whoever designs
the next signature format.

**Recorded as the fifth CAL gap this sweep found**, beside the four in
`../../persist/pastbench/AREEV.md` and `../../appworld/AREEV.md`. All five were
filed; four are closed in 1.7.4 and the fifth was declined on purpose. Naming a
gap precisely enough to quote its error code is what got them fixed — that is
the transferable lesson, not the table:

| gap | where | status |
|---|---|---|
| no per-group count to render (`GROUP BY` reorders only) | appworld's passive arm | **closed** — [#209](https://github.com/AreevAI/areev/issues/209) |
| `valid_to` not queryable on facts (`CAL-E060`) or a template variable (`CAL-E042`) | persist notes | **closed** — [#206](https://github.com/AreevAI/areev/issues/206) |
| `description` not filterable on skills (`CAL-E060`); filtering on `object` silently matches everything | persist skills | **closed** — [#207](https://github.com/AreevAI/areev/issues/207), both halves |
| no text extraction, no one-row-per-group projection | persist session titles | **partly** — extraction landed ([#210](https://github.com/AreevAI/areev/issues/210)); first-of-group is still open |
| **no reshaping of a JSON payload stored in a field** | selfimprove `fails_with` | **open by design** — reading a path landed ([#211](https://github.com/AreevAI/areev/issues/211)); reshaping was declined |

A sixth, found while fixing them rather than by this sweep:
`ASSEMBLE` dropped grains to its default budget in silence
([#208](https://github.com/AreevAI/areev/issues/208)) — closed, and the reason
`../../appworld/AREEV.md` gives for keeping its passive selection a saved
`RECALL` is updated there.

## Skipped, and why

| surface | status |
|---|---|
| ASSEMBLE | not adopted — the renderer reshapes JSON (above), which #211 deliberately does not cover. Any token or truncation figure in `SELFIMPROVE.md` therefore means a harness bound, not `BUDGET n tokens` |
| saved queries | not adopted, for the same reason: there is no query to register that produces this block |
| CAL rendering | not adopted |
| tool-call lifecycle | **adopted from the start** |
| prefix namespaces | not applicable — one synthetic domain |
