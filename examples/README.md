# examples

Copy-paste-runnable material for Areev + Areev Loop. These are docs-with-files,
not a package — clone the repo (they are not shipped in `pip`/`npm`/`cargo`
installs). See [`docs/loop.md`](../docs/loop.md) for the full guide.

Building your own? Start with
[`how-to-create-an-areev-agent.md`](how-to-create-an-areev-agent.md) — the
architecture, which grain to use when, the autonomy spectrum up to dynamic
planning, and the do/don't list these examples follow.

Two tiers: [`agents/`](agents/) are **vertical agents** — a whole job, end to
end — and everything else teaches **one seam** (a protocol, a policy file, a
contract). Agent examples must run keyless against committed fixtures, with
live credentials opt-in, so CI can prove them on every release.

| Dir | What it solves |
|---|---|
| [`agents/`](agents/) | **A whole job your team recognizes, run under governance** — ten vertical desks (accounts payable, sanctions screening, incident response, hiring, insurance claims, denial management, surveillance, diligence, clinical referrals, GDPR requests), each one starting from a real problem and proving its payoff in a keyless smoke: work in (polled from a mailbox, or **pushed as a webhook your own listener delivers**), workflow, a named human approval, system of record out — then the loop turns the desk's own record into signed improvements. Zero repo dependencies: every vendor leg is a JSON-on-stdio connector or host tool |
| [`colab/`](colab/) | **See the self-improvement loop pay off before wiring anything** — runnable Colab/Jupyter notebooks: the full loop plus five business-scenario walkthroughs (wrong-lesson rollback, detect/review/govern, Hermes comparison, enterprise architecture); keyless deterministic floor, optional LLM discovery |
| [`policy/`](policy/) | **How much must a person sign?** Three `loop-policy.json` variants — solo prototype, team, locked-down prod — to start from instead of authoring policy from scratch |
| [`import/`](import/) | **Improvement without changing your agent** — your existing tool-call logs (JSONL) become Tool grains, and the loop clusters the failures you already had |
| [`ci/`](ci/) | **An unreviewed lesson blocks the merge, not the postmortem** — a GitHub Actions job that fails the build on pending high-severity recommendations |
| [`mcp/`](mcp/) | **One agent proposes, another approves** — the multi-agent supervisor pattern (separation of duties over MCP) |
| [`llm/`](llm/) | **Plug in any model without an SDK** — ready-to-run `--llm-cmd` backends (`claude -p`, OpenAI, Ollama, a dependency-free mock) + the stdin/stdout protocol, including the five-kind **proposal vocabulary** a draft may carry (a lesson, a fact, a CAL query rewrite, workflow field edits, new tool source) and the fixture mode two agent examples use to exercise the whole governed path keyless |
| [`analyzers/`](analyzers/) | **Your own detection logic, advisory-only** — a bring-your-own command analyzer (`--analyzer-cmd`) with the probe/analyze protocol |
| [`hermes/`](hermes/) | **Memory for an agent you did not build** — Areev as a [Hermes Agent](https://github.com/NousResearch/hermes-agent) memory provider: budgeted per-turn assembly (p50 0.83 ms), `MEMORY.md`/`USER.md` edits mirrored as immutable grains, Areev Loop at session end |

Every example models **judgment** — approve one recommendation, dismiss one
with a reason. Never a rubber-stamp loop.
