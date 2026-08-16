# examples

Copy-paste-runnable material for Areev + Areev Loop. These are docs-with-files,
not a package — clone the repo (they are not shipped in `pip`/`npm`/`cargo`
installs). See [`docs/loop.md`](../docs/loop.md) for the full guide.

| Dir | What |
|---|---|
| [`colab/`](colab/) | Runnable Colab/Jupyter notebooks: the full self-improving-agent loop plus five business-scenario walkthroughs (wrong-lesson rollback, detect/review/govern, Hermes comparison, enterprise architecture) — keyless deterministic floor, optional LLM discovery |
| [`policy/`](policy/) | Three `loop-policy.json` variants (solo / team / locked-down prod) |
| [`import/`](import/) | A tool-call JSONL sample + walkthrough → Tool grains → tool-failure clustering |
| [`ci/`](ci/) | A GitHub Actions job that fails the build on pending high-severity recommendations |
| [`mcp/`](mcp/) | The multi-agent supervisor pattern (separation of duties over MCP) |
| [`llm/`](llm/) | Ready-to-run `--llm-cmd` backends (`claude -p`, OpenAI, Ollama, a dependency-free mock) + the stdin/stdout protocol |
| [`analyzers/`](analyzers/) | A bring-your-own command analyzer (`--analyzer-cmd`, advisory-only) with the probe/analyze protocol |
| [`hermes/`](hermes/) | Areev as a [Hermes Agent](https://github.com/NousResearch/hermes-agent) memory provider — budgeted per-turn assembly (p50 0.83 ms), `MEMORY.md`/`USER.md` edits mirrored as immutable grains, Areev Loop at session end |

Every example models **judgment** — approve one recommendation, dismiss one
with a reason. Never a rubber-stamp loop.
