# Fact sheet — how CAL assembles context

*Verified against the code on 2026-07-16 (branch `main`, near commit `002a0bc`);
§1, §3 and §4 re-verified 2026-08-16 after the render unification changed
what is true.
Every claim below is anchored to a `file:line` you can open. This sheet backs the
assembly claims in the (out-of-repo) `VIDEO_CONCEPT.md` and any marketing/UI copy
about "assembling the prompt." If the code moves, fix the citations here first —
this is the honesty source of truth for the assembly story.*

The one-line reframe it exists to justify: **CAL doesn't hand you a prompt string —
it assembles a token-accounted context block, and the tokens that reach the model
are joined from many grains, not typed by a human.**

---

## 1. What `ASSEMBLE` actually produces

`ASSEMBLE` returns a **structured, token-accounted payload**, not a bare string:

- `CalResultPayload::Assembled { grains, sources, total_tokens: u32, budget_limit:
  Option<u32>, progressive, total_available }` —
  `crates/areev-cal/src/executor.rs:172` (destructured at `:2072`).
- Per-source accounting rides along in `SourceMeta { tokens_used, tokens_allocated,
  … }` — `crates/areev-cal/src/assemble.rs:93`.
- It becomes **one joined text block only when a `FORMAT` clause is present** →
  `CalResultPayload::Formatted { text, format, grain_count }`
  (`executor.rs:197`, applied via `apply_format_clause_to_grains` at `:3528`);
  without `FORMAT` it returns raw `Grains` (`:3533`).
- The actual "render N grains into one string" lives in
  `ContextAssembler::format()` → `FormattedContext { text, estimated_tokens, … }`
  (`crates/areev-context/src/assembly.rs:51`, `:273`).

**Honesty caveat:** the token count is an **estimate — `chars / 4`, not a provider
tokenizer**. Since the render unification there is exactly ONE estimator —
`areev_cal::render::estimate_tokens` — shared by `ASSEMBLE … BUDGET`
(`assemble.rs::estimate_grain_tokens`) and the areev-context allocators, so a
budget means the same thing on every path. Honest to say "one token-budgeted /
token-accounted block"; **not** honest to imply exact GPT/Claude token counts
on screen.

## 2. The streams → 11 grain types (unified Tool grain)

Exactly **11** grain types:
`enum GrainType { Fact, Event, State, Workflow, Tool, Observation, Goal, Reasoning,
Consensus, Consent, Skill }` — `crates/areev-core/src/types/grain.rs:12` (bytes
`0x01`–`0x0B`; coverage test at `.../types/registry.rs:242`).

The six on-screen streams and the footer add up to 11 with no overlap:

| On-screen stream | Grain type | Note |
|---|---|---|
| FACTS | `Fact` | |
| MESSAGES | `Event` | |
| SKILLS | `Skill` | |
| WORKFLOWS | `Workflow` | |
| TOOLS | `Tool` (`Definition` kind) | one Tool grain, discriminated by `kind` |
| TOOL RESULTS | `Tool` (`Execution` kind) | same grain type — the invocation record |
| `…` footer | `State · Goal · Reasoning · Observation · Consensus · Consent` | the other 6 |

`enum ToolKind { Definition, Execution }` on `Tool.kind` —
`crates/areev-core/src/types/tool.rs:22` / `:176` (default when absent =
`Execution`). So "TOOLS" and "TOOL RESULTS" are **two kinds of one grain type**, not
two grain types — 5 distinct types cover the 6 streams, + 6 named in the footer = 11.

## 3. Output formats — `markdown`, not `md`; the set is larger than four

- CAL `enum FormatSpec { Sml, Toon, Markdown, Json, Yaml, Text, Triples, Csv, Table,
  Preset{name}, Template{…} }` — `crates/areev-cal/src/ast.rs:1221`. Canonical key
  for markdown is `"markdown"` (`ast.rs:1249`), **not** `md`.
- Context render `enum OutputFormat { Sml, Toon, Markdown, PlainText, Json }` —
  `crates/areev-context/src/policy.rs:13`.
- `sml / markdown / toon / json` is a fair **representative subset** — do not claim
  it's the whole set.
- Since the render unification, both enums dispatch into ONE per-grain
  implementation — `areev_cal::render` — so `FORMAT sml` from CAL and
  `recall --render sml` produce the same per-grain bytes (pinned by
  `crates/areev-context/tests/render_parity.rs`). Honest to say "a grain
  renders identically on every surface"; envelopes (grouping, sections,
  budget) remain per-surface.

## 4. Budget-aware assembly — progressive disclosure is now REAL (2026-08-16)

This section previously documented the gap between the claim and the code.
Phase 3 of the render unification closed it; what is true now:

- Per-source `BUDGET` and `PRIORITY` are CAL keyword tokens; budget applied in
  `assemble.rs` (`budget_prefix`), priority (`PrioritySpec { label, weight }`)
  alongside. `BUDGET <n> [tokens|grains]` is token- or grain-denominated.
- **The context allocators emit all three tiers.** `allocate()` and
  `allocate_with_diversity()` (`crates/areev-context/src/budget.rs`) admit
  Full while under **70%** of the budget, degrade to **Summary** while under
  **95%**, then Omit — both thresholds are in the code now, and
  `render_summary` renders through the shared per-type summaries
  (`areev_cal::render::render_grain_summary`).
- **Structured formats bypass the fade on purpose** —
  `strip_summaries_for_structured_formats` (`assembly.rs`): a JSON array or a
  TOON table gets whole entries or nothing, because a prose summary inside
  either would corrupt the format. The fade applies to `sml` / `markdown` /
  `plaintext`.
- **CAL template renders are tier-driven**: `ASSEMBLE … BUDGET n tokens
  FORMAT TEMPLATE x` computes the disclosure tier via
  `templates::select_tier(budget, grain_count)` (`executor.rs::template_tier`),
  so `ELEMENT_SUMMARY` fires when the budget squeezes and `ELEMENT_OMIT`
  accounts for the grains the budget dropped — pinned by
  `element_omit_renders_grains_the_budget_dropped`
  (`crates/areev-cal/tests/cal_integration.rs`).
- The `Assembled` payload's `progressive: false` field remains a legacy flag
  of the OLD (pre-removal) mechanism and is still always false; disclosure
  now lives in the render path, not the assembly payload.

**Honest phrasing:** priority-ranked, token-budgeted assembly with progressive
disclosure in prose renders — grains degrade full → summary → omitted rather
than being cut mid-token; structured outputs (JSON/TOON) stay whole-entry.

## 5. Hybrid recall + RRF fusion

- Three legs fused with Reciprocal Rank Fusion: structural (`recall_seqs`), BM25
  full-text (`search_text`, Turso FTS), and vector (`search_vector`,
  `vector_distance_cos`). Doc at `crates/areev-store/src/lib.rs:2334` ("with
  Reciprocal Rank Fusion; optional deadline makes it fail-open").
- RRF constant: `pub const RRF_K0: f64 = 60.0;` (`lib.rs:393`); each leg contributes
  `1.0 / (RRF_K0 + rank)` (`lib.rs:2429`). Deadline-bounded / fail-open.

## 6. Cross-file context — read-only facade mounts

`AreevFacade::mount(alias, store)` (`crates/areev-cal/src/areev_facade.rs:49`);
mounts are "Read-only mounted memories" (`:25`), routed by `"alias.inner"` namespace
(`:146`) and surfaced to `ASSEMBLE` as cross-file sources (`mount_aliases`, `:70`).
Writes only ever hit the session store — mounts can't be written through.

## 7. Immutability · content addressing · versioning

- Content address = **SHA-256 over the whole `.mg` blob**: `content_address(blob) =
  Sha256::digest(blob)` — `crates/areev-core/src/format/header.rs:150`.
- `SUPERSEDE` (token `lexer.rs:355`, AST `ast.rs:128`) and `HISTORY OF <hash>` (token
  `lexer.rs:246`, AST `ast.rs:99`) exist. Grains are never edited in place; supersede
  mutates the **index layer only** (store invariant).

## 8. Dedup by content address ("808 → 1")

- Hard uniqueness: `CREATE UNIQUE INDEX IF NOT EXISTS idx_grains_hash ON
  grains(hash)` — `crates/areev-store/src/lib.rs:455`. N byte-identical writes
  therefore collapse to **1** grain by construction (`has(hash)` probe at `:2180`;
  "content addressing dedupes by construction" at `:3101`).
- Distinct, higher-level mechanism: value-level idempotent add keyed on
  `(namespace, subject, relation, object)` — `add_if_novel` at `lib.rs:1259`. Don't
  conflate the two: content-address dedup is byte-exact; `add_if_novel` is by triple.

## 9. Tool schema → 9 provider formats

`enum ProviderKind { OpenAiTools, OpenAiResponses, AnthropicTools, GeminiTools,
McpTools, Hermes, Llama31, MarkdownTools, SmlTools }` = **9**, enumerated in `ALL`
— `crates/areev-core/src/format/tool_schema/mod.rs:34` (`:86`). Split: 5 JSON-shaped
(OpenAI-tools/-responses, Anthropic, Gemini, MCP) + 4 text-shaped (Hermes, Llama3.1,
Markdown, SML).

## 10. The latency / write / dedup numbers (from `crates/areev-bench/RESULTS.md`)

All measured on Apple M4 Max, `--release`:

| claim | number | source |
|---|---|---|
| in-process recall | p50 33.1µs / p99 60.2µs = **0.12%** of a 50ms frame | RESULTS.md §1 |
| full voice loop (edge profile) | p50 79.0µs / p99 151.9µs, target <200µs → **PASS** | RESULTS.md voice_loop |
| write | **136µs** amortized (single-add p50 117µs), **0 LLM calls · 0 tokens · $0** | RESULTS.md §3 (M3) |
| idempotency | **808** byte-identical writes → **1** grain | RESULTS.md §3 (M1) |

## 11. Self-improvement loop — shipped vs. roadmap

The differentiator is "memory that improves." Be precise about what runs today.

**Shipped (host-driven):**

- Capture: `capture-stop` reads Claude Code hook JSON on stdin and stores the last
  exchange as thread-indexed Events — `crates/areev-cli/src/main.rs:814` (the hook
  snippet it prints is at `:377`). This is invoked *by the host*, not autonomously.
- Correction: `SUPERSEDE` links a new grain over a stale one (append-only), walkable
  via `HISTORY OF <hash>` — see §7. Host/agent decides when.
- Event stream substrate: an **op-log** table (`oplog`) with
  `changes_since(after_op_seq, limit) -> Vec<OpRecord>` —
  `crates/areev-store/src/lib.rs:3069` (`OpRecord` at `:38`). Real, and tailable.
  Its current consumers are **sync and observability, not learning**: the hub segment
  push/pull (`crates/areev-server/src/lib.rs:352`), the CLI `.log`/follow
  (`crates/areev-cli/src/main.rs:613`), plus tests/benches.
- Derived-intelligence *data model* exists: skill proficiency
  (`Skill::with_proficiency`, `practice_count`, `strategies` —
  `crates/areev-core/src/types/skill.rs:153`) and provenance values `"inferred"` /
  `"consolidated"` / `"llm_generated"` (`crates/areev-core/src/format/serialize.rs:59`).

**Roadmap (NOT wired — must be marked `[roadmap]` on screen):**

- An **autonomous engine** that tails the op-log and consolidates. The result types
  are defined — `ConsolidationResult` / `ConsolidationGroupInfo`
  (`crates/areev-cal/src/store_types.rs:299`, filed under "H3: Multi-Agent +
  Intelligence types") — but there is **no producer**: no `consolidate()` function,
  no CAL/CLI/MCP surface reaches it, and `ConsolidationResult` has zero non-defining
  usages in the tree (grep). So "listens to events → prepares improvements → writes
  back, unattended" is a design target, not shipped behavior.

Honest framing for copy: **today** the loop is capture → `SUPERSEDE` → sharper next
context, driven by the host; the **autonomous** consolidation/learning engine is
roadmap with the substrate (op-log, consolidation types, proficiency, provenance)
already in place.

## 12. The executed sample query (2026-07-16)

The video's on-screen `ASSEMBLE` was **run, not composed on paper** — against a
store built with the published `areev` PyPI package (generic
`add(grain_type, fields_json)`: 2 Facts, 2 Events, 1 Skill, 1 Tool definition,
1 Tool execution), then executed via both the Python `cal()` binding and the
`areev cal` CLI:

```sql
CAL/1 ASSEMBLE "turn context"
  FOR "voice agent rebooking priya's flight"
  FROM
    messages: (RECALL events WHERE session_id = "call-42" RECENT 10),
    profile:  (RECALL facts  WHERE subject = "priya"),
    tools:    (RECALL tools  RECENT 5),
    skills:   (RECALL skills RECENT 3)
  BUDGET 900 tokens
  PRIORITY messages > profile > tools > skills
  FORMAT markdown
```

Observed: `type: "formatted"`, `format: "markdown"`, 6 grains rendered (the
900-token budget dropped the tool *definition* whole, kept the execution record);
without `FORMAT`, the same statement returns the `assembled` token ledger
(alloc 360/270/180/90 per the ordering-form priority; `total_tokens: 410`,
`budget_limit: 900`).

Syntax facts this exercise pinned down (all against the parser):

- **Clause order is fixed and matches OMS §8.2**: topic, `FOR`, `FROM`, `BUDGET`,
  `PRIORITY`, `FORMAT`, `WITH dedup` — `parse_assemble` in
  `crates/areev-cal/src/parser.rs`. **Fixed 2026-08-19 (issue #42):**
  out-of-order clauses used to detach silently — `BUDGET` after `FORMAT` was
  dropped and the assembly ran at the 4000-token default. They are now a parse
  error naming the clause and the canonical order.
- **Source labels are plain identifiers** — dotted labels (`org.policies:`) are a
  parse error (`CAL-E002`); mounts are addressed by the namespace *string* inside
  the source (`WHERE namespace = "org.policies"`), per the §8 acceptance test
  (`crates/areev-cal/tests/assemble_mount_tests.rs:37`,
  `crates/areev-cli/tests/mcp_smoke.rs:257`).
- **Both `PRIORITY` forms work**: weighted (`a: 0.5`) and ordering (`a > b > c` →
  evenly spaced weights) — `parser.rs:3307`.
- **CAL text `ADD` covers only fact / observation / goal / skill** (executor
  message, verified live); Events and Tools are written via the bindings' generic
  `add` (`crates/areev-py/src/lib.rs:186`), `capture-stop`, or the store API.
- Mixed subject filters across sources emit lint `CAL-W009` (real, observed).
- `FORMAT json` on ASSEMBLE returns a `grains` payload, not rendered text
  (golden-test note) — use `markdown`/`sml`/`toon` when the video needs visible
  rendered output.

**Doc bugs found & fixed during this check**: `docs/cal-reference.md` documented
the ASSEMBLE grammar in a non-parsing order (`FOR` after `PRIORITY`, `FORMAT`
after `WITH dedup`) and its multi-source example used a dotted `org.policies:`
label — invalid as written (`CAL-E002`), and its `PRIORITY`-before-`BUDGET` order
detaches the budget. `ARCHITECTURE.md` §5.4 carried the same invalid
`org.policies BUDGET 800, …` sketch. All three were corrected and each fixed
example was re-executed against a live store.

## Corrections applied to `VIDEO_CONCEPT.md` (2026-07-16)

1. **Grains row → column.** The six streams are now a vertical column funneling into
   CAL (matches "streams converge into CAL").
2. **Output relabeled.** "THE PROMPT" → "THE ASSEMBLED CONTEXT · one budgeted token
   block"; copy now states CAL returns a token-accounted block (§1), not a bare
   prompt string.
3. **Progressive-disclosure claim removed.** Dropped "full → summary → omitted" and
   the "~70% / ~95%" thresholds; replaced with the accurate priority-ranked
   full → omitted behavior (§4). Added honesty-guardrail bullets: summary tier isn't
   emitted; on-screen token numbers are `chars/4` estimates.
4. **FORMAT list fixed.** `md` → `markdown`, and the wider format set noted (§3).
5. **Tool rows corrected** to the real `Definition` / `Execution` kinds (§2).
6. **Diagram reworked into a clockwise cycle** (2nd pass): grain column labeled
   **STORAGE**; "the assembled context" moved to the CAL→LLM arrow as a *label*, not
   a box (it isn't a processing stage); added an **ACTIONS** block (host executes tool
   calls) with results captured back to storage; added a **LEARNING ENGINE** block
   marked **`[roadmap]`** with the shipped-substrate-vs-unwired-engine split spelled
   out here in §11 and in the honesty guardrails.
7. **On-screen query added, executed** (3rd pass): a verified `ASSEMBLE` over
   messages/facts/tools/skills with `FORMAT markdown` plus its real output and token
   ledger (§12). The §1 example — `org.policies BUDGET 800, …` — was **invalid
   syntax** (`CAL-E002`) and was replaced with an executed one; the same label/order
   bugs were also fixed in `docs/cal-reference.md`.
