# areev-context

Budget-aware, provider-optimal rendering of recall results into model-ready
context. Input: `&[SearchHit]` (from areev-cal); output: `FormattedContext`
text in SML / TOON / Markdown / PlainText / JSON.

## Module map

- `policy.rs` — config types: `OutputFormat`, `MetadataLevel`, `Ordering`,
  and the `FormatPolicy` builder (`.metadata/.ordering/.token_budget/
  .group_by_type/.grain_override/.query_text/.grain_type_diversity`).
- `presets.rs` — `FormatPolicy::claude()` (SML, grouped), `gpt4()` /
  `gemini()` (Markdown), `local_small()` (PlainText), `json_api()` (JSON).
  **Presets never set `token_budget`** — the caller owns that.
- `budget.rs` — `Allocation{Full,Summary,Omit}` and two allocators:
  `allocate()` (pure priority order) and `allocate_with_diversity()`
  (5-phase: group by grain type → reserve `min_per_type` → cap trim → Full →
  fill remainder). Progressive disclosure: Full up to ~70% of budget, then
  Summary, then Omit at ~95%.
- `render.rs` — `GrainRenderer` trait (`render`, `render_summary`,
  `token_estimate`, `context_priority`) + `RendererRegistry` with 13 per-type
  renderers (12 grain types + default). `toon_columns()` defines the TOON
  tabular columns per grain type.
- `assembly.rs` — `ContextAssembler` (`format()`, `format_with_hints()`),
  `RenderingHints`, `FormattedContext{text, estimated_tokens, included_count,
  omitted_count, truncated}`.

## Rendering modes

`format_with_hints` picks exactly one mode, in priority order:
aggregation > timeline (chronological; needs ≥2 hits + temporal intent) >
census (80/20 budget split, keyed on `RecallSource::Census`) >
relevance-highlight (>10 grains) > default. **JSON output bypasses all
modes** — it is a plain structured dump.

## Provider-optimal means

Format matched to the consuming model: SML tags for Claude (XML-ish),
Markdown for GPT/Gemini, TOON compact tables / JSON for machines, PlainText
for small local models.

## Gotchas

- **Token estimation is `chars / 4`** — a heuristic, no real tokenizer.
  `estimated_tokens` is approximate; don't treat budgets as exact.
- Budget pressure sets `truncated: true` and bumps `omitted_count` — check
  those instead of guessing from output length.
- Tests are inline `#[cfg(test)]` per module; there is no `tests/` dir.
  Run with `cargo test -p areev-context`.
