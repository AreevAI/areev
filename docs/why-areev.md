# Why Areev — the full argument

The [README](../README.md) makes the case in one screen. This page is the
long form: the problem in detail, what "self-improving" does and does not
mean here, the three systems and how they are designed against each other,
what a governed run actually is, why erasure is an operation rather than a
project, and the storage story.

## The problem

Agent memory today is a vector store plus an extraction pipeline. Audited
deployments keep finding the same failures, and none of them announce
themselves — the agent just gets quietly worse.

| What usually happens | What happens here |
|---|---|
| **The store fills with near-duplicates.** The same fact, written eleven slightly different ways, all ranked, all in the prompt. | Memories are content-addressed. A byte-identical rewrite collapses to **one** grain — not a duplicate, not an error. Even a *paraphrased* re-learning is caught: `areev novelty` reports the nearest existing lesson so the harness supersedes it instead of adding a near-duplicate. |
| **Stale values out-rank current ones.** The agent learned the price twice and cites the old number, and nothing in the store says which is which. | An edit is a **supersession**. Recall returns 1 current value and 0 stale ones; the old value stays in history, retrievable on purpose. |
| **Nobody can say where a belief came from.** "Why does it think that?" ends in a log grep, if the log still exists. | **100%** of grains trace to when and how they entered — and to the run that produced them. |
| **A crash pays the invoice twice.** Retry logic hopes the vendor deduplicates. | Intent is journaled **before** the effect. A crash-window redelivery reuses the same idempotency key and is journaled as a redelivery. |
| **"Delete this person's data" is a project.** | `FORGET SUBJECT` is one operation, it replicates as ordinary tombstones so replicas delete too, and a disclosure shares **one selector** with it. |

None of that is a promise you have to take on faith. It is a deterministic
benchmark with no LLM in the loop:
`cargo run -p areev-bench --bin honesty_metrics`. Measured on this tree:
byte-identical writes settle to **one grain**; after 20 updates recall
returns **1 current value, 0 stale** with full history kept; writes cost
**~136 µs and 0 LLM calls**; **100%** of grains trace to when/how they
entered ([RESULTS.md §3](../crates/areev-bench/RESULTS.md)).

## What "self-improving" does and does not mean here

Three limits, stated before the features, because they are the reason this is
deployable:

| It does | It does not |
|---|---|
| Improve the agent's **memory** — lessons, facts, routing rules, flagged plans | Touch model weights. Areev never trains, ships no trainer, and takes no training dependency |
| Propose, with the grains it computed from cited by hash | Change anything on its own authority. Every apply goes through the gates; auto-apply is **off** unless a host policy file grants it, and never for destructive or LLM-originated changes |
| Run when you run it: a hook, a cron, CI, an MCP call | Run a daemon. There is no scheduler and no background process |

The scary version of self-improvement is an agent that changes itself between
your reviews. This one cannot: **autonomy is never earned by a metric, it stays
an explicit grant from the host.**

Weights stay outside the engine — but not outside the governance story: see
[the tuning seam](#the-tuning-seam-governed-corpora-for-your-own-slm) below.

## Three systems, designed against each other

The record the agent reads from, the loop that improves it, and the authority
that decides what it is allowed to change — designed *against* each other,
not merely alongside: the graph is shaped so an erasure can reach every copy
of a fact, the loop is shaped so it cannot apply its own advice, and
governance is shaped so neither of them can be talked out of it. That is why
the record can be erased, the turn can be budgeted, and the change can be
undone.

> **git for your agent's memory**: log, diff, time-travel, forks with explicit
> merges, and encrypted incremental sync — built into the data model, because
> grains *are* content-addressed immutable objects.

### 1. The record — and the turn

Every grain is an immutable, content-addressed `(subject, relation, object)`
assertion with a reverse index, so the store is a **provenance graph**, not a
pile of embeddings — and the same system decides what reaches the model:

- **Traverse it**: `areev related` walks edges, `WITH multi_hop` follows them
  in reverse — *what points at this?*, not just *what does this point to?*
- **Time-travel it**: `areev entity-at` reconstructs what the agent believed at
  a timestamp. Updates are supersessions, so history is never overwritten and
  the current value is unambiguous.
- **Assemble it into the prompt**: retrieval returns candidates; a turn needs a
  *budget-shaped* context. Grains render Full up to ~70% of the token budget,
  degrade to **Summary** to ~95%, then **Omit** — so the tail of a large result
  set costs a summary rather than the whole grain or nothing at all. A per-type
  priority table (consent > state > goal > fact) with a reserved minimum per
  type means one loud grain type cannot crowd out the consent record or the
  open goal. ([`areev-context`](../crates/areev-context/), not a `top_k` and a prayer.)
- **Reproduce it**: same inputs, same context — allocation is deterministic,
  which is what makes a replay comparable to the original run. One renderer
  backs both this and CAL's `FORMAT`, pinned by a byte-parity test, so the two
  surfaces cannot drift.
- **Annotations stay annotations**: a cross-link between grains never alters its
  target's supersession state (OMS §15.3), so enriching the graph cannot
  silently rewrite what the agent currently believes.
- **Join execution to memory**: `areev run-trace` gives a run's transcript and
  the durable knowledge it produced; `areev runs-touching` answers the blast-radius
  question — *this fact is wrong, which runs produced or refined it?* Stated with
  its limit: a run that merely **read** a grain leaves no grain behind, so an
  append-only store cannot attest to it.
- **Concurrent edits become branches**, surfaced with a deterministic
  provisional head and merged explicitly — never silently lost.
- **Hybrid recall**: structural + BM25 + vector legs fused with RRF;
  multilingual by construction (Arabic and English ride every leg; unspaced
  CJK rides the vector leg). Bring any embedder: the `EmbedBackend` trait in
  Rust, a callback in Python (`set_embedder`), or a command on every surface
  (`--embed-cmd 'my-embedder'` — text on stdin, JSON vector on stdout).

### 2. The learning — Areev Loop

Areev Loop reads the agent's own history back as evidence and proposes
changes — *"this tool failed 40% of its calls"*, *"these two facts
contradict"* — each citing the grains it was computed from. Thirteen
deterministic analyzers, **zero model calls required**.

- **Your agent stops repeating what fails.** The analyzers cluster recurring
  tool failures into lessons, catch duplicate and contradictory facts, flag
  stale grains, and surface forks — computed over typed grains, never raw
  prose. With the recall-telemetry sidecar on, three of them see memory
  *utility*, not just hygiene: facts never recalled (`cold_grains`),
  questions that keep coming back empty (`coverage_gap`), context budgets
  overflowing (`budget_pressure`). And with `areev run` journaling executions
  into the same file, `run_outcome` reads run terminals per plan — *"this
  workflow failed 4 of 6 runs (last error: …)"*, *"this plan has spent
  $4.10 across 6 runs"* — as analyzer findings with the run grains cited,
  not dashboard archaeology. Precision is measured, not asserted: 1.00 on
  the labeled fixture, with a 0.90 failure floor when the fixture runner is
  invoked (`cargo run -p areev-bench --bin loop_precision`).
- **Nothing changes behind your back.** Four gates — propose → review →
  apply → verify — with separation of duties, a **mandatory reason** on every
  decision, a hash-chained audit grain per transition, and a stored inverse
  for every apply. Auto-apply is off unless a host policy file explicitly
  grants it, and never for destructive or LLM-originated changes.
- **It proves whether its own advice worked.** A recommendation that carries
  a metric is re-measured after you apply it — at 1d / 7d / 30d checkpoints,
  against what actually happened (did that tool failure recur?); a late
  regression proposes a revert. `areev loop outcomes` is the receipt.
- **Add an LLM for what determinism can't see — verified, never trusted.**
  `areev loop run --model claude-sonnet` (or `openai:gpt-5`,
  `ollama:llama3.1`, any OpenAI-compatible endpoint, or `--llm-cmd 'CMD'`)
  lets a model discover cross-fact issues like a semantic contradiction — but
  every draft must ground against the cited grains and survive an
  **independent verifier** (the proposer never grades itself) before it
  reaches the queue, and `origin = llm` can never auto-apply. "Nothing to
  report" is a first-class answer, so it doesn't invent findings to look busy.
- **It runs where you already run things — no daemon.** A cheap, idempotent
  command with watermark gates (`--min-new`, `--if-stale`): a Claude Code
  `SessionEnd` hook, cron, CI (`areev loop list --fail-on high` exits 2 —
  a build gate), or the `areev_loop` MCP tool. And the loop closes *into*
  the agent: `areev recall-hook --with-loop` rides the pending queue into
  the context Claude Code injects, so the agent sees its own recommendations
  without polling.

One property is worth the whole design: **the gate cannot be weakened by the
thing it gates** — a `code_revision` recommendation is pinned to the evalset
it was judged against, and can be applied only through the gating edge that
judged it (Rule E1). Full guide: [`loop.md`](loop.md) · why the LLM layer is
verified, never trusted: [`loop-reflection.md`](loop-reflection.md).

### 3. The authority — governance

Governance here is not access control bolted onto a store. It is the **shape of
the operations**:

- **Destruction takes a hash, an identity, or an age — never a predicate.**
  `DELETE` is not a token in the grammar. The three destructive statements each
  require an authorization grant plus a recorded reason, write an audit record,
  and can be capped off entirely per process.
- **The audit names a fingerprint, not the person.** An immutable, replicating
  grain naming the erased subject would undo the erasure it records.
- **One selector, two directions**: `REPORT SUBJECT` (disclose) and
  `FORGET SUBJECT` (erase) share it, so a DSAR cannot describe more or less than
  the erasure removes.
- **Effects are journaled before they happen.** `areev run` writes intent before
  every dispatch; a crash-window effect is redelivered under the **same
  idempotency key** — journaled as a redelivery — never minted as a duplicate.
  `areev run verify` re-drives the run with every effect answered from the
  journal, writing nothing, and byte-compares every checkpoint — a
  **journal-consistent** replay, which is what it is called rather than a bare
  "verified".
- **Separation of duties is structural**: the principal who triggered an
  approval gate cannot approve it.

## What a governed run actually is

Every agent framework executes graphs. Almost none can *prove* an execution
afterwards. Here the plan is a grain, the run is a journal in the same file,
and the person who has to say yes is a node in the graph — not a Slack
message somebody remembered to send.

An effect is written down **before** it is allowed to happen: intent is
journaled, the tool runs, the result supersedes the intent. That ordering is
what makes a crash-window effect redeliverable under the same idempotency key
instead of paid twice, and what lets `areev run verify` re-drive the whole run
from its journal and byte-compare every checkpoint.

`areev run` gives you LangGraph-grade execution (Send fan-out, subgraphs,
typed reducers, streaming, time-travel forks) with an audit story the
frameworks don't have: budgets enforced per-superstep, and a kill switch whose
cancel-to-drain time is **measured** into the oversight report rather than
asserted ([EU AI Act Art. 12/14 map](eu-ai-act.md)). Full guide:
[`run.md`](run.md) · hands-on: [the quickstart](quickstart.md#run-a-governed-workflow-areev-run).

### Triggers — the cadence is data, not a daemon

An agent that only runs when you type a command is a demo. Making it a product
usually means a scheduler service: something else to deploy, monitor, and keep
in sync with what the agent is supposed to be watching.

Areev makes the standing rule a **grain**. What to watch, how often, and what to
start live in the memory, so they replicate with it and survive a restore:

```bash
areev trigger add --db accounting.db --ns accounting \
  --type polling --observer mailbox --scope 'mailbox:ap@northwind.example' \
  --interval 120 --workflow <WF_HASH> --dedup-key /message_id \
  --because "poll the accounts-payable mailbox for invoices"

areev trigger run --db accounting.db --dry-run   # touches nothing — the safe first command
areev trigger run --db accounting.db --connector-cmd ./mailbox.sh --tool-cmd ./tools.py
```

Then put `trigger run` on whatever heartbeat you already have — cron, launchd,
systemd, a Kubernetes CronJob. It can be much **coarser** than your shortest
interval, because the command is cheap and the *memory* decides what is actually
due. Claims are leased, cursors are local (a dev memory restored from prod does
not inherit prod's watermark and skip real work), and `--dedup-key` means the
same invoice does not get processed twice.

**The loop rides the same posture.** `areev loop run` is likewise a cheap,
idempotent command with watermark gates — put it on the same heartbeat, a
Claude Code `SessionEnd` hook, or a CI job where `areev loop list --fail-on
high` exits 2 and turns governance into a merge gate. Neither of them is a
background process that can drift while you aren't looking.

Eight kinds over four primitives, the connector contract, composite gates and
correlation windows: [`triggers.md`](triggers.md).

## Erasure is a real operation

Because the record is content-addressed and subject-indexed rather than baked
into weights, removing a person is an operation rather than a project.
`FORGET SUBJECT` removes them from the live store and replicates as ordinary
tombstones, which delete on replicas too — and the data-subject report shares
**one selector** with it, so a disclosure describes exactly what an erasure
removes. The reach, and the archive-retention window it does *not* cover, are
stated plainly in [`gdpr.md`](gdpr.md) and [`erasure.md`](erasure.md).

That is the property model-side memory cannot offer, and it is where blast
radius is actually controlled.

## Storage: a plain SQLite file or a Postgres schema

The memory is not a proprietary blob. On the embedded backend — the
[Turso](https://github.com/tursodatabase/turso) engine, SQLite rewritten in Rust
— **a memory is a SQLite file**, and you can prove it against the demo memory
committed to this repo without installing anything of ours:

```console
$ file data/demo.db
data/demo.db: SQLite 3.x database, ...
$ sqlite3 data/demo.db "SELECT count(*) FROM grains"
466
```

That is the anti-lock-in property. Your agent's memory opens in every SQLite
tool on earth, backs up with `cp`, and outlives this engine — and the `.mg`
format inside it is documented and
[OMS](https://github.com/openmemoryspec/oms)-conformant with byte-exact test
vectors, so the *contents* outlive it too. Importers exist for the common
stores if you are bringing history with you ([`migrate.md`](migrate.md)).

Because it is in-process, recall is **microseconds, not milliseconds** — there
is no server in the recall path. The same engine runs on a **$35 Raspberry Pi 3
from 2016** at ~361 µs, flat from 500 to 8,000 grains
([RESULTS.md](../crates/areev-bench/RESULTS.md)).

Where there is no durable disk — Cloud Run, autoscaled containers, a
multi-instance service — the same store runs over **one PostgreSQL schema per
memory** instead, behind a cargo feature, with multiple concurrent writers and
pgvector. The two backends are held to identical semantics by **one
conformance suite executed against both**, so the choice cannot quietly change
behaviour: [the quickstart](quickstart.md#postgresql-backend-server-tier) has
the commands and the deliberate differences.

## The tuning seam: governed corpora for your own SLM

The trajectory path keeps the typed evidence needed to replay or train from a
run: `record-tool-call` stores JSON arguments separately from results,
`capture-stop` preserves every ordered chat/content block, `run-manifest`
binds a run to a content-addressed configuration, and sampled ASSEMBLE
manifests record the exact included/dropped hashes plus the rendered digest.
Set `--run-id` to join full-mode recall telemetry to the same trajectory.

`areev corpus --select '<READ CAL>' [--out train.jsonl] [--recipient ID]`
reuses CAL as the authorized selector and streams OpenAI chat JSONL with tool
definitions, step-level loss weights/quality labels, elision records, and
trace/model/policy/subject-fingerprint bindings. Each export writes a
replicating manifest grain whose `related_to` edges name every source hash;
`--recipient` records the downstream trainer/model owner that must act on a
stale-export notice. Later identity or retention erasure reports which
exported corpora are stale and must be retired or re-derived; this is
auditable suppression/re-derivation, not a claim that a subject has been
removed from model weights.

That corpus is the hard half of tuning a small language model on your agent's
own history: on-policy trajectories with step-level labels (the harmful steps
of a failed run are masked, not the whole run discarded), a gating harness,
and lineage that survives an erasure. `areev tune --cmd ...` is the last
mile: hand the corpus to **your** trainer exactly as `--embed-cmd` hands text
to your embedder, take back an adapter reference, and it registers as a grain
pinning base model + adapter + quantization as one unit, with `derived_from`
naming the corpus manifest. Promotion is then what every other change here
already is: `areev loop run` proposes the candidate, `areev eval run
--evalset <pin> --model openai-compat:<name>` grades it against your serving
endpoint (vLLM/SGLang; `ollama:` for GGUF), a **clean recorded run** admits
it through `approve`/`apply --gating-run` — writing the
`mg:adapter_promotion` grain your host serves from — and rollback retracts
it. Works from the CLI, Python, Node, MCP, or the console alike. Areev still
never trains, ships no trainer, and takes no training dependency: it supplies
the corpus, grades the result, and owns the lineage — and no accuracy or
context-savings claim ships until the harness has measured one. Full guide:
[`loop.md`](loop.md) · recipe:
[cookbook §14](cookbook.md#14-capture-a-reproducible-run-and-export-a-governed-corpus).

## The rest of the case

- **Model-native**: built-in MCP server, [Anthropic memory-tool backend
  adapter](memory-tool.md), budget-aware context rendering (SML / Markdown /
  TOON / JSON), tool-schema rendering for 9 provider formats, Python and Node
  bindings.
- **Distributed the git way**: op-log streaming with generations and
  point-in-time restore; pull subscriptions for fleet-wide knowledge
  distribution; concurrent edits become **branches with a deterministic
  provisional head** — surfaced, merged explicitly, never silently lost.
- **Private by design**: local-first, no telemetry; optional **AES-256-GCM
  encryption at rest** with an Argon2id-derived key; deletion is a tombstone or
  **crypto-erasure** (destroy the key, destroy the memory). See
  [`security-model.md`](security-model.md).
- **Keep your stack**: [LangGraph and CrewAI
  adapters](quickstart.md#keep-your-langgraph-or-crewai-stack--govern-its-state)
  put your existing agent's state in a memory file you can diff, sync, and erase.
- **A format you keep**: the `.mg` format and CAL are stable and documented,
  conformant with the Open Memory Spec (OMS) — byte-exact test vectors — so
  the record outlives this engine, and outlives us.
