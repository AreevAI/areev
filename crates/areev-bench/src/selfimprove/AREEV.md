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

A CAL template can substitute a field and filter on truthiness; it cannot parse
a JSON payload out of a field, remove a key from it and re-serialise the rest.
So this section cannot move, and moving only the `rules` section would leave
one renderer split across two mechanisms — the parallel system this sweep
exists to remove.

The clean fix is to store the code and the detail as **separate fields** at
write time, which a template could then render. That changes the stored fields,
therefore the content addresses, therefore the evidence behind the published
A/B/A/B result. Not a change to make mid-programme; a note for whoever designs
the next signature format.

**Recorded as the fifth CAL gap this sweep found**, beside the four in
`../../persist/pastbench/AREEV.md` and `../../appworld/AREEV.md`:

| gap | where |
|---|---|
| no per-group count to render (`GROUP BY` reorders only) | appworld's passive arm |
| `valid_to` not queryable on facts (`CAL-E060`) or a template variable (`CAL-E042`) | persist notes |
| `description` not filterable on skills (`CAL-E060`); filtering on `object` silently matches everything | persist skills |
| no text extraction, no one-row-per-group projection | persist session titles |
| **no reshaping of a JSON payload stored in a field** | selfimprove `fails_with` |

## Skipped, and why

| surface | status |
|---|---|
| ASSEMBLE | not adopted — the renderer reshapes JSON (above). Any token or truncation figure in `SELFIMPROVE.md` therefore means a harness bound, not `BUDGET n tokens` |
| saved queries | not adopted, for the same reason: there is no query to register that produces this block |
| CAL rendering | not adopted |
| tool-call lifecycle | **adopted from the start** |
| prefix namespaces | not applicable — one synthetic domain |
