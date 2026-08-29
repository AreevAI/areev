# Areev

**Self-improving agents, governed.** Areev is the substrate for **adaptive
agents** — agents that get better from their own history, under human
authority, in steps you can inspect, undo, and re-measure.

[![CI](https://github.com/AreevAI/areev/actions/workflows/ci.yml/badge.svg)](https://github.com/AreevAI/areev/actions/workflows/ci.yml)
[![License: MIT OR Apache-2.0](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#license)
[![MSRV](https://img.shields.io/badge/rustc-1.90%2B-blue.svg)](#getting-started)

<p align="center">
  <img src="docs/assets/areev-hero.png" width="900"
       alt="How Areev works: typed grains — facts, goals, skills, workflows, tools, events, recommendations — in one store, assembled by the context graph (hybrid recall, rerank, knowledge graph) into a budget-shaped pseudonymized context for the LLM provider; the host executes the actions under human-in-the-loop governance, and the Areev learning loop (propose, review, apply, verify, plus the governed model-tuning seam) feeds what happened back into the store">
</p>

<p align="center">
  <em>One substrate: typed grains in, a budget-shaped pseudonymized context out, a person on the gate —<br>
  and what the agent did comes back as the evidence its next improvement is proposed from.</em>
</p>

Every team that ships an agent wants the same next thing: an agent that
**gets better from its own experience**. Almost none ship one — because an
agent that rewrites its own memory unsupervised fails every security review
on the same four questions: **what changed, on what evidence, on whose
authority, and can we take it back?**

Areev makes all four mechanical. The agent **proposes** from its own
recorded history, citing evidence by hash; a named person **approves**, with
a written reason; every apply **stores its inverse**; and the change is
**re-measured** afterwards — a late regression proposes its own revert. This
is enforceable rather than aspirational because the agent's knowledge *and*
its execution history live in one content-addressed store, queried with one
language ([CAL](docs/cal-reference.md)).

Three honest limits, by design: it improves **memory, never model weights** ·
**nothing applies itself** without an explicit host grant · **no daemon** —
everything runs when you run it.
[The full argument →](docs/why-areev.md)

---

## What's in the box

| | What it is | Why it's unusual |
|---|---|---|
| **[Areev Loop](#the-learning-loop--self-improvement-with-a-gate-on-it)** — the learning | Thirteen deterministic analyzers read the agent's history and propose changes, each citing its evidence by hash | It **proposes; a named person disposes**. Four gates, a written reason, a stored inverse, re-measurement after apply. Starts at **zero model calls** |
| **[Areev Run](#the-execution-graph--runs-a-person-can-gate)** — the execution graph | Plans as content-addressed grains, runs as journals, humans as nodes in the graph | Intent is journaled **before** the effect; `verify` replays the journal and byte-compares every checkpoint |
| **[Areev Trigger](docs/triggers.md)** — the cadence | Standing rules that start workflows — eight kinds, from cron to memory-predicates | The rule is a **grain**, so the cadence travels with the memory. **No daemon** — evaluation is a cheap idempotent command |
| **[CAL](docs/cal-reference.md)** — the context | A query language that **assembles**, not just retrieves: budget-aware rendering, Full → Summary → Omit | A turn needs a *budget-shaped* prompt, and deterministic allocation is what makes a replay comparable |
| **[The store](docs/why-areev.md#storage-a-plain-sqlite-file-or-a-postgres-schema)** — the record | A provenance graph in a plain **SQLite file** ([Turso](https://github.com/tursodatabase/turso)), or a **PostgreSQL** schema for the server tier | **~30 µs** recall in-process; one conformance suite pins both backends to identical semantics |

Every screen below is the real console over the demo memory committed to
this repo — click through:

<table>
  <tr>
    <td align="center" width="33%">
      <a href="#see-it-yourself--the-knowledge-graph">
        <picture>
          <source media="(prefers-color-scheme: dark)" srcset="demo/screens/graph-dark.png">
          <img src="demo/screens/graph-light.png" alt="The knowledge graph — entities in the agent's memory drawn as a provenance graph with a rewind scrubber">
        </picture>
      </a>
      <br><sub><b>The knowledge graph</b><br>walk and rewind what it knows</sub>
    </td>
    <td align="center" width="33%">
      <a href="#the-learning-loop--self-improvement-with-a-gate-on-it">
        <picture>
          <source media="(prefers-color-scheme: dark)" srcset="demo/screens/suggestions-dark.png">
          <img src="demo/screens/suggestions-light.png" alt="The review queue — findings from the learning loop, each with evidence and an Apply or Dismiss decision">
        </picture>
      </a>
      <br><sub><b>The learning loop</b><br>it proposes, you dispose</sub>
    </td>
    <td align="center" width="33%">
      <a href="#the-execution-graph--runs-a-person-can-gate">
        <picture>
          <source media="(prefers-color-scheme: dark)" srcset="demo/screens/runs-dark.png">
          <img src="demo/screens/runs-light.png" alt="The Runs page — governed runs grouped as waiting on you and finished, with Approve and Refuse buttons">
        </picture>
      </a>
      <br><sub><b>Governed runs</b><br>a person on the gate</sub>
    </td>
  </tr>
</table>

### Does the learning actually work? — take it away and see

Anyone can show a number going up. The honest test is whether the *learning*
caused it — so we remove what was learned and watch the gain leave, then put
it back and watch it return. One frozen model at temperature 0, three seeded
runs over 100 held-out tasks each (n=300) the agent has never seen; the only
thing that changes between bars is whether the lessons Areev learned from
the agent's own failures are in its prompt:

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="docs/assets/aba-selfimprove-dark.svg">
  <img src="docs/assets/aba-selfimprove-light.svg" width="860"
       alt="A/B/A/B self-improvement bar chart: 39.0% of held-out tasks passed before learning, 59.7% with lessons applied, 37.7% with lessons rolled back, 56.3% re-applied — the gain follows the lessons in both directions, every transition paired McNemar p < 0.0001">
</picture>

| what the memory holds | tasks passed | |
|---|:---:|---|
| nothing learned yet | 39.0% | the agent keeps repeating its mistakes |
| the lessons, applied | **59.7%** ▲ | it stops |
| the lessons, rolled back | 37.7% ▼ | **the proof** — take them away and the gain leaves with them |
| the lessons, re-applied | **56.3%** ▲ | put them back and it returns |

Every transition is significant at **p < 0.0001** (paired McNemar, n=300);
the two lessons-off states are statistically indistinguishable. There is no
benchmark mode: the prompt is assembled from live memory on every run, so
rolling the lessons back empties it structurally. Three results behind the
chart:

- **It learned for free.** The lessons come from deterministic clustering
  over the agent's own tool calls — **zero model calls** — and each one
  cites the failures it was computed from, by hash.
- **It improved without regressing.** Every hidden rule the loop touched got
  better, none got worse.
  [The per-rule numbers →](crates/areev-bench/RESULTS.md#it-improves-without-regressing)
- **It matches heavyweight retrieval at a fraction of the cost.** The same
  store with the loop *off* — raw failure history retrieved into the prompt
  three different ways — never significantly outscored the lessons, paid
  **1.3–6.2× the prompt tokens** every turn doing it, and its best-scoring
  variant doubled a class of mistakes the loop had eliminated.
  [The baselines →](crates/areev-bench/RESULTS.md#does-the-loop-beat-the-store--the-passive-memory-arms)

One synthetic workload, built to make learning *measurable* rather than to
mimic production traffic — and we wrote the test, so re-run it yourself: the
whole three-seed comparison costs about **$2.30**.
[Full results & caveats →](crates/areev-bench/RESULTS.md#areev-loop-self-improvement--the-abab-causal-proof) ·
[Reproduce it →](crates/areev-bench/SELFIMPROVE.md#reproduce) ·
[How the loop works →](docs/loop.md)

---

| **~30 µs** recall, in-process | runs on a **$35 Raspberry Pi** | **2,504 tests · 81.0% coverage** | **`FORGET SUBJECT`** is one operation |
|:---:|:---:|:---:|:---:|
| [benchmarks →](crates/areev-bench/RESULTS.md) | [edge results →](crates/areev-bench/RESULTS.md) | [quality, measured →](docs/quality.md) | [GDPR map →](docs/gdpr.md) |

---

## See it yourself — the knowledge graph

The demo memory behind every screenshot is committed to this repo — 466
grains, 9 governed runs, 13 real recommendations, one open fork:

```bash
areev ui --db data/demo.db --ns accounting     # → http://127.0.0.1:7437
```

<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="demo/screens/graph-dark.png">
    <img src="demo/screens/graph-light.png" width="900"
         alt="The Areev console: the entities in an agent's memory — people, vendors, invoices, processes — drawn as a provenance graph around a focused person, with a rewind scrubber along the bottom">
  </picture>
</p>

<p align="center"><em>The people, vendors, invoices and processes this agent knows — as a graph you can walk, and rewind.<br>
A real console over the real <a href="data/demo.db"><code>demo.db</code></a> in this repo. Nothing here is a mockup.</em></p>

Rebuild it from scratch with [`scripts/build_demo.sh`](scripts/build_demo.sh):
every run in it is a real journal and every recommendation is a real analyzer
output, not rows written to look convincing.

---

## The learning loop — self-improvement with a gate on it

Areev Loop reads the agent's own history back as evidence — *"this tool
failed 40% of its calls"*, *"these two facts contradict"*, *"this workflow
failed 4 of its last 8 runs"* — and turns it into recommendations that are
evidence-cited, reviewable, undoable, and re-measured after apply. Thirteen
deterministic analyzers, **zero model calls required**; attach an LLM for
what determinism can't see and its findings are grounded against the cited
grains and independently verified before a human ever sees them.

<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="demo/screens/suggestions-dark.png">
    <img src="demo/screens/suggestions-light.png" width="900"
         alt="The review queue in the Areev console: findings surfaced by the learning loop in plain language, each with its evidence and an Apply or Dismiss decision — nothing applies itself">
  </picture>
</p>

<p align="center"><em>Thirteen findings from the demo memory, in plain language, each undoable. Nothing here applies itself.</em></p>

Every recommendation passes **propose → review → apply → verify** with
separation of duties, a mandatory written reason, a hash-chained audit grain
per transition, and a stored inverse. Applied advice is re-measured at
1d / 7d / 30d — a late regression proposes its own revert. It runs where you
already run things: a Claude Code `SessionEnd` hook, cron, or CI, where
`areev loop list --fail-on high` exits 2 and turns governance into a merge
gate. Full guide: [docs/loop.md](docs/loop.md) · analyzers, gates, and
policy in depth: [why-areev](docs/why-areev.md#2-the-learning--areev-loop).

### Tuning your own SLM — the governed corpus

The same history is a training asset. `areev corpus` exports on-policy
trajectories as chat JSONL with step-level loss weights and lineage that
survives an erasure; `areev tune --cmd` hands that corpus to **your** trainer
and registers the returned adapter as a grain. Promotion is then what every
other change here already is: proposed by the loop, graded against a pinned
evalset, admitted through a clean recorded gating run, and revocable — the
gate cannot be weakened by the thing it gates (Rule E1). Areev still never
trains and ships no trainer: it supplies the corpus, grades the result, and
owns the lineage. [The tuning seam →](docs/why-areev.md#the-tuning-seam-governed-corpora-for-your-own-slm)

---

## The execution graph — runs a person can gate

<p align="center">
  <img src="docs/assets/run-lifecycle.png" width="900"
       alt="The governed run lifecycle: a trigger fires (cron, webhook, or poll), intent is journaled before the effect, the host executes the tool, a person decides under separation of duties, and the run is provable afterwards with areev run verify — every step landing as a grain in the journal, replayable and byte-compared">
</p>

Every agent framework executes graphs; almost none can *prove* an execution
afterwards. Here the plan is a grain, the run is a journal in the same file,
and the approver is a node in the graph. An effect is written down **before**
it is allowed to happen, so a crash-window effect is redelivered under the
same idempotency key instead of paid twice — and `areev run verify` re-drives
the whole run from its journal and byte-compares every checkpoint.

<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="demo/screens/runs-dark.png">
    <img src="demo/screens/runs-light.png" width="900"
         alt="The Runs page of the Areev console: nine governed runs grouped as Waiting on you and Finished — six posted, one refused by a person, one failed honestly, one waiting with Approve and Refuse buttons">
  </picture>
</p>

<p align="center"><em>Nine real runs: six posted, one a person <strong>refused</strong>, one that failed honestly, one still waiting.<br>
Approving requires <strong>your own</strong> sign-in — the approver's identity <em>is</em> the audit record.</em></p>

LangGraph-grade control flow (Send fan-out, subgraphs, typed reducers,
streaming, time-travel forks) with budgets that actually stop the run and a
kill switch whose drain time is **measured** into the oversight report
([EU AI Act Art. 12/14 map](docs/eu-ai-act.md)). Standing rules start runs on
a schedule or an event with **no daemon** — the cadence is data
([triggers](docs/triggers.md)). Full guide: [docs/run.md](docs/run.md) ·
hands-on: [quickstart](docs/quickstart.md#run-a-governed-workflow-areev-run).

---

## Five ways agent memory rots

<p align="center">
  <img src="docs/assets/problem-solution.png" width="900"
       alt="Five ways agent memory rots and Areev's structural answer to each: duplicates collapse to one content-addressed grain; edits supersede with history kept (1 current, 0 stale); every grain traces to the run that made it (100% provenance); intent is journaled before the effect under the same idempotency key; and FORGET SUBJECT makes erasure one operation that reaches replicas">
</p>

Vector-store memory fails quietly — duplicates crowd the prompt, stale values
outrank current ones, provenance is a log grep, a crash pays twice, and
erasure is a project. Areev makes each failure structurally impossible, and
proves it with a deterministic benchmark, no LLM in the loop:
`cargo run -p areev-bench --bin honesty_metrics`.
[Each failure, in detail →](docs/why-areev.md#the-problem)

---

## Getting started

```bash
cargo install areev          # the CLI    (prebuilt binaries: see the quickstart)
pip install areev            # Python
npm install @areev/areev     # Node (unscoped `areev` is pending an npm exception)
```

Store a fact, recall it, hand it to a model:

```bash
areev add    john prefers "window seat"
areev recall john --render sml              # → a model-ready context block
areev ui                                    # → the web console
```

Give Claude Code (or any MCP client) persistent memory in one line:

```bash
claude mcp add areev -- areev serve --mcp --db ~/.areev/code.db --ns claude-code
```

Or skip the toolchains entirely — the repo's [`Dockerfile`](Dockerfile)
builds the same binary with the Postgres and TLS features already on:

```bash
docker build -t areev .
docker run --rm -v areev-data:/data areev add john prefers "window seat"
AREEV_UI_TOKEN=$(openssl rand -hex 16) docker compose --profile console up
```

The image serves every role — console and a trigger heartbeat —
and one box runs a whole fleet of agents, one memory each. Containers,
compose files, and the AWS / GCP / Azure / Kubernetes mappings:
[docs/docker.md](docs/docker.md).

Want a complete agent, not a snippet?
[`examples/agents/invoice-to-accounting`](examples/agents/invoice-to-accounting/)
is an accounts-payable agent that takes corrections **by email reply** — no
credentials, no network, no model key. `python/smoke.sh` runs week one (it
does the job, under governance); `python/improve.sh` runs week two (it
proposes its own fix from its run journals, and you decide). The same agent
ships in **Python, TypeScript, and Rust** — one file each, and all three mint
the *identical* content-addressed plan.

Rust / Python / Node embedding, the `areev run` walkthrough, the PostgreSQL
backend, encryption at rest, migration from other stores, and fleet sync:
**[docs/quickstart.md](docs/quickstart.md)**. Task recipes:
[cookbook](docs/cookbook.md). Keep your LangGraph or CrewAI stack and govern
its state with the [pip adapters](docs/quickstart.md#keep-your-langgraph-or-crewai-stack--govern-its-state).

---

## Fast enough for the edge

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="docs/assets/bench-latency-dark.svg">
  <img src="docs/assets/bench-latency-light.svg" width="820"
       alt="Recall latency measured at p50: 9 to 158 microseconds across every surface on an Apple M4 Max, 361 microseconds in-process on a 2016 Raspberry Pi 3, and flat latency from 500 to 8,000 grains on both a Pi 3 and a 2018 Intel NUC">
</picture>

Recall is **microseconds, not milliseconds**, because there is no server in
the recall path — fast enough inside a real-time voice agent's 50 ms frame,
where a network call cannot go. The same engine, installed with
`pip install areev` in 16 seconds, serves recall on a **$35 Raspberry Pi 3
from 2016** at ~361 µs — **flat from 500 to 8,000 grains**, so a device can
accumulate memory for months and answer as fast on day 200 as on day 1.
Measured on the devices themselves, clock-certified:
[RESULTS.md](crates/areev-bench/RESULTS.md).

---

## Privacy & erasure

Areev is local-first and collects no telemetry. Optional **AES-256-GCM
encryption at rest** (Argon2id-derived key) covers the database and its
attachment sidecar; deleting a memory is a tombstone or **crypto-erasure**.
Destruction is authorization-gated and takes a hash, an identity, or an age —
never a predicate: `DELETE` is not even a token in the query grammar.

Handling a data-subject request is three commands:

```bash
areev subject-report "pat" --db memory.db --ns caller --out pat.jsonl --bundle pat.mgb
areev forget-subject  "pat" --db memory.db --ns caller --yes --because "Art. 17 request #42"
areev audit export --db memory.db --out evidence.jsonl
```

The report and the erasure run **one selector**, so a disclosure describes
exactly what an erasure removes; the audit names a *fingerprint*, never the
identity. [GDPR article→capability map](docs/gdpr.md) ·
[erasure scope](docs/erasure.md) · [threat model](docs/security-model.md) ·
report vulnerabilities per [SECURITY.md](SECURITY.md).

---

## Quality, measured

The numbers below are regenerated from the tree on every CI run, which fails
the build if they drift — they cannot go stale without turning the build red.

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="docs/assets/repo-stats-dark.svg">
  <img src="docs/assets/repo-stats-light.svg" width="760"
       alt="Areev repository quality metrics — source and test line counts, test count, line coverage, and stable error codes, generated from the tree">
</picture>

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="docs/assets/coverage-dark.svg">
  <img src="docs/assets/coverage-light.svg" width="760"
       alt="Line coverage per crate, each bar shown against its own CI floor — coverage is enforced per crate, not as one workspace average">
</picture>

- Tests are about a third of the codebase; roughly half of that drives the
  **real binary over real stdio**, not mocks.
- Coverage counts **source lines only** (no test code scoring itself) — the
  lowest of the three numbers we could have quoted — and is **floored per
  crate** in CI, so one crate's regression cannot hide behind another's gain.
- Every user-facing error carries a stable, **append-only** `DOMAIN-Ennn`
  code ([ERROR_CODES.md](ERROR_CODES.md)); both storage backends run one
  conformance suite; the CAL examples in the reference are **executable** and
  fail CI when stale.

How each number is produced, and the benchmark receipts:
**[docs/quality.md](docs/quality.md)** · per-crate table:
[docs/repo-stats.md](docs/repo-stats.md) ·
[LoCoMo accuracy + honesty metrics](crates/areev-bench/RESULTS.md).

---

## Documentation

| Doc | For |
|---|---|
| [`docs/quickstart.md`](docs/quickstart.md) | Install, CLI, MCP, Rust/Python/Node, Postgres, encryption, fleets |
| [`docs/why-areev.md`](docs/why-areev.md) | The full argument: the problem, the three systems, the honest limits |
| [`docs/quality.md`](docs/quality.md) | How every published number is produced and gated |
| [`ARCHITECTURE.md`](ARCHITECTURE.md) | How Areev works: grains, `.mg` format, CAL, recall, sync |
| [`docs/loop.md`](docs/loop.md) | Areev Loop — governed self-improvement (analyzers, four gates, policy, every surface) |
| [`docs/run.md`](docs/run.md) | `areev run` — the governed runtime: plans, the journal, verify, HITL, budgets, forks |
| [`examples/how-to-create-an-areev-agent.md`](examples/how-to-create-an-areev-agent.md) | Building an agent on Areev: architecture, grain selection, the autonomy spectrum, dynamic planning, do/don't |
| [`docs/triggers.md`](docs/triggers.md) | Standing rules that start workflows — the cadence as data |
| [`docs/eu-ai-act.md`](docs/eu-ai-act.md) · [`docs/procurement.md`](docs/procurement.md) | EU AI Act article→capability→command map; procurement questionnaire answers |
| [`docs/cal-reference.md`](docs/cal-reference.md) | The CAL query language reference |
| [`docs/mcp-reference.md`](docs/mcp-reference.md) | The MCP server + its 25 tools |
| [`docs/migrate.md`](docs/migrate.md) | Importing an existing corpus, with its edit history |
| [`docs/memory-tool.md`](docs/memory-tool.md) | The Anthropic memory-tool backend (Python / Node / CLI) |
| [`docs/cookbook.md`](docs/cookbook.md) | Task-oriented recipes |
| [`docs/deployment-profile.md`](docs/deployment-profile.md) | Deploying the runtime + adapters: modes, auth, SSO |
| [`docs/docker.md`](docs/docker.md) | The container image: compose, the trigger heartbeat, cloud deploys, multi-agent fleets |
| [`docs/scale-and-tenancy.md`](docs/scale-and-tenancy.md) | Laying out a large shared corpus: why the partition boundary IS the permission boundary, and what each boundary costs |
| [`FAQ.md`](FAQ.md) | Questions & answers (also LLM-friendly) |
| [`SECURITY.md`](SECURITY.md) · [`docs/security-model.md`](docs/security-model.md) | Security policy & threat model |
| [`docs/gdpr.md`](docs/gdpr.md) · [`docs/erasure.md`](docs/erasure.md) | GDPR obligations → capabilities (for a DPIA); the erasure requirement record |
| [`AGENTS.md`](AGENTS.md) · [`llms.txt`](llms.txt) | For AI agents working in / with this repo |
| [`CONTRIBUTING.md`](CONTRIBUTING.md) | How to contribute (DCO sign-off) |

Runnable material lives in [`examples/`](examples/) — vertical agents,
notebooks, CI gates, policy variants, custom analyzers — every one keyless
and deterministic at its floor; the guide to assembling your own agent is
[`examples/how-to-create-an-areev-agent.md`](examples/how-to-create-an-areev-agent.md). The workspace layout and crate map are in
[`ARCHITECTURE.md`](ARCHITECTURE.md); Areev is built on
[Turso Database](https://github.com/tursodatabase/turso) (MIT — see
`THIRD-PARTY-NOTICES.md`). The `.mg` format and CAL are stable, documented,
and [OMS](https://github.com/openmemoryspec/oms)-conformant;
[`CHANGELOG.md`](CHANGELOG.md) records each release.

## Contributing

Contributions are welcome under the [DCO](https://developercertificate.org/) — see
[CONTRIBUTING.md](CONTRIBUTING.md) and our [Code of Conduct](CODE_OF_CONDUCT.md).
Questions and ideas: [GitHub Discussions](https://github.com/AreevAI/areev/discussions).

## License

Licensed under either of [Apache License 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT) at your option. Unless you explicitly state otherwise,
any contribution you intentionally submit for inclusion is dual-licensed as
above, with no additional terms. The OMS specification itself is CC0.

---

<p align="center">
  Areev is built and backed by
  <strong><a href="https://mindgryd.com">MindGryd Software Private Limited</a></strong>.
</p>
