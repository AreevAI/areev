# areev-context

Budget-aware context rendering (SML/TOON/Markdown/JSON) for Areev.

`areev-context` turns recall results into model-ready context. It takes the
search hits produced by the store and renders them into budget-aware,
provider-optimal strings — SML, Markdown, TOON, or JSON — while respecting a
token budget through progressive disclosure (full, summary, or omit per grain)
and grain-type sectioning. This is the last hop in the recall path: it decides
what fits in the context window and how it is formatted for the target model,
so an agent receives compact, relevant memory rather than raw rows.

Part of [Areev](https://github.com/AreevAI/areev) — an embedded memory engine for AI agents. See the [architecture overview](https://github.com/AreevAI/areev/blob/main/ARCHITECTURE.md).

Licensed under MIT OR Apache-2.0.
