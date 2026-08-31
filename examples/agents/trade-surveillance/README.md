# trade surveillance — the co-occurrence gate

> **A teaching example of a mechanism, not a compliant surveillance system.**
> Two correlated signals are not a market-abuse model, and nothing here is
> calibrated, validated, or fit for a regulated deployment. Every instrument,
> issuer, desk, order, headline and analyst below is invented. Symbols are
> venue-qualified (`MRDN:VNTG` on the fictional Meridian Exchange) precisely
> so they cannot be mistaken for, or collide with, a listed instrument.

**The problem.** A market-abuse desk watches two feeds that know nothing
about each other, and neither is interesting alone — a block order is
Tuesday, a rebalance notice is Tuesday. Alert on each and the desk drowns
in false positives. What an analyst must look at is a **block order and a
material event on the same instrument, close together in time** — and
correlating that yourself means building and operating a correlation
service before the first case ever opens.

**What you get.** A standing rule that fires on the co-occurrence itself,
declared as data rather than built as infrastructure. Across two sessions,
**eight signals become three correlated cases — zero auto-closed** — each
parked for an analyst, and each arriving with how this *pattern* was
dispositioned before, on a different instrument.

The mechanism is a `composite` Trigger — two member
triggers under aliases, a boolean gate over those aliases, a `correlate`
pointer naming the field the two signals must agree on, and a `window_ms`
past which a half-match expires.

```
 order book feed ──┐                                              analyst
 (block orders)    │  member trigger: order_burst                     ▲
                   ├──────────────────────────┐                       │
 disclosure feed ──┘                          │                       │
 (material events) │  member trigger:         ▼                       │
                   │  material_event    ┌───────────────┐             │
                   └───────────────────►│  COMPOSITE    │             │
                                        │  order_burst  │             │
                                        │      AND      │             │
                                        │ material_event│             │
                                        │ correlate     │             │
                                        │   /symbol     │             │
                                        │ window 15000ms│             │
                                        └───────┬───────┘             │
                                                │ one firing per correlated set
                                                ▼                     │
                        assemble_case ─► prior_art ─► analyst_review ─┘
                                              ▲            │
                        precedents from ──────┘            ├─ escalate ─► file_alert
                        the desk's memory                  └─ benign ───► record_dismissal
                                                                              │
                        the precedent for NEXT time ◄────── a written reason ─┘
```

There is deliberately **no edge that closes a case without a person**. The
memory makes a case cheaper to judge; it never makes it automatic.

And the second loop, the one that reads the desk's own record back:

```
 dispositions, run journals ──► areev loop (deterministic analyzers)
        (the case record)               │ Recommendation + evidence, by hash
                                        ▼
                                  [ a person ] ──approve, signed, in writing
```

Nothing here needs a credential, a network, or a model key: the whole thing
runs from committed fixtures, so CI proves it on every release.

## Run it

```bash
# python 3.9+, `pip install areev` (or a maturin-built crates/areev-py)
examples/agents/trade-surveillance/python/smoke.sh      # one trading session
examples/agents/trade-surveillance/python/improve.sh    # the next one, plus the loop
```

Both exit non-zero on any drift, and neither paces itself with `sleep`:
`agent.py await-due` blocks until `trigger_status()` reports every trigger
due (the same predicate `trigger_run` gates on) and `agent.py await-window`
blocks until the evaluator's own record of the last firing is past the
correlation window. A timing assertion that depends on how loaded the
machine is is not an assertion.

Both exit non-zero on any drift. `python/{smoke,improve}.sh` are three-line
wrappers: the assertions live once, in the language-neutral
[`smoke.sh`](smoke.sh) and [`improve.sh`](improve.sh) at this level, so a
second language stack is a wrapper plus an agent file.

## Session one — `smoke.sh`

| Step | What happens |
|---|---|
| 1 | Seed two plans, six tool definitions, the instrument book, two saved CAL queries, two member triggers and the gate |
| 2 | Three composites that **must be refused when written**: one member; no predicate; a gate naming an undeclared alias (`TRG-E008`) |
| 3 | First pass seeds the feed cursors and fires nothing |
| 4 | A 480,000-share block buy in MRDN:VNTG arrives **alone** → 1 intake run, **0 cases** |
| 5 | The Meridian 40 rebalance notice for MRDN:VNTG, one tick later → **CASE MRDN:VNTG** |
| 6 | MRDN:ORLN: the take-private wire lands **first**, the order follows → **CASE MRDN:ORLN** (the gate is a co-occurrence, not an ordering) |
| 7 | MRDN:PDRA: an order, then the notice delivered **past the correlation window** → **no case** |
| 8 | The desk tries to dispose of its own case → refused (`RUN-E012`) |
| 9 | A benign dismissal with no written reason → refused |
| 10 | `user:nadia` dismisses MRDN:VNTG as benign; `user:oren` escalates MRDN:ORLN — both signed |
| 11 | Nadia's reasoning becomes a precedent about the **pattern**, not the instrument |
| 12 | Another pass: the same feed items start nothing (dedup) |
| 13 | The evaluator's own journal: 8 gate evaluations, 6 signals, 2 cases |

## Session two — `improve.sh`

| Step | What happens |
|---|---|
| 1 | MRDN:PDRA gets another block order, alone — last session's expired half-match is **gone**, so this starts a new partial match rather than completing an old one |
| 2 | The notice follows one tick later → **CASE MRDN:PDRA**, opening with nadia's MRDN:VNTG reasoning already attached |
| 3 | And it **still parks** for an analyst — a precedent is not a disposition |
| 4 | Nadia dismisses it on the precedent, saying what she checked anyway |
| 5 | The desk briefs itself out of its own memory (one saved query, one budget) |
| 6 | `areev loop`: two of three cases were one shape, both benign — 0 auto-applied |
| 7 | A decision with no written reason is refused |
| 8 | `user:oren` approves with a reason — and the reason is *record the lesson, do not narrow the rule* |
| 8b | The approval **cannot be quietly walked back**: `approved` has no exit but `applied` or `expired` |
| 9 | Run the loop again — the same evidence does not become a second recommendation |
| 10 | 8 signals across two feeds → 3 correlated cases, **0 auto-closed** |

## What each fixture exercises

| Fixture | Feed | Exercises |
|---|---|---|
| `orders/01-vntg-block-buy.json` | order book | a signal alone: the gate stays shut |
| `news/02-vntg-index-rebalance.json` | disclosures | completes the pair **inside** the window → CASE MRDN:VNTG |
| `news/03-orln-merger-talks.json` | disclosures | the material event arriving **first** |
| `orders/04-orln-block-buy.json` | order book | completes it from the other side → CASE MRDN:ORLN |
| `orders/05-pdra-block-buy.json` | order book | arms a partial match that is about to expire |
| `news/06-pdra-index-rebalance.json` | disclosures | delivered past the window → **no case** |
| `orders/07-pdra-block-buy.json` | order book | session two: the expired half-match did not linger |
| `news/08-pdra-index-rebalance.json` | disclosures | the same pattern on a new instrument → CASE MRDN:PDRA **with a precedent** |
| `decisions/00-desk-clears-its-own-case.json` | — | separation of duties (`RUN-E012`) |
| `decisions/01-vntg-no-reason.json` | — | a disposition with no written reason |
| `decisions/02-vntg-benign.json` | — | the dismissal that becomes the precedent |
| `decisions/03-orln-escalate.json` | — | the escalation, signed by a different analyst |
| `decisions/04-pdra-benign.json` | — | a dismissal *on* a precedent, still by a human |

The 2-digit prefix is the desk's clock: both feeds share one sequence, and
`FEED_UPTO` is how the act scripts advance it. A fixture whose prefix is
above the clock has not happened yet.

## What this example is teaching

### 1. A gate is a data structure, not an expression string

`areev-run-core`'s condition grammar is frozen, and new CAL syntax is an OMS
conformance decision — so a composite's gate is a serialized `Condition`
tree, which is why it can be authored straight from Python with no parser:

```python
{"kind": "and",
 "left":  {"kind": "comparison", "field": "order_burst",
           "comparator": "eq", "value": {"kind": "boolean", "value": True}},
 "right": {"kind": "comparison", "field": "material_event",
           "comparator": "eq", "value": {"kind": "boolean", "value": True}}}
```

The field names are the member **aliases**. A 64-hex content address is not
a legal identifier in any expression grammar, and an alias survives a member
being re-declared at a new address.

### 2. A gate that could never fire is refused when it is *written*

A dead trigger's only symptom is nothing happening — which looks exactly
like a healthy trigger on a quiet day. So all three of these are refused at
authoring time, before anything is stored:

| Declaration | Refusal |
|---|---|
| one member | `a composite trigger needs at least two members` |
| no predicate | `a composite trigger needs a predicate over its members` |
| a gate naming an alias the declaration does not carry | **`TRG-E008`** |

### 3. Correlation and windowing are one mechanism

Partial matches are keyed by `(trigger, correlation value)`, and each match
carries its own expiry from the moment its **first** member fired. That is
what stops Monday's order pairing with Tuesday's news, and it removes the
need for a wall-clock reset job entirely.

Two things worth knowing before you build on it:

- **The window is evaluation wall-clock, not event time.** It measures the
  gap between when the desk *saw* each signal, not the timestamps inside
  them. The `at` fields in the fixtures are illustrative; the window in
  `smoke.sh` is real elapsed milliseconds. If you need event-time
  correlation, put the bucket in the correlation key itself (see §5).
- **An item that cannot be correlated is dropped, not guessed.** If the
  `correlate` pointer does not resolve against a member's payload, that
  firing simply does not join the gate — attributing it to an arbitrary key
  would pair unrelated work.

### 4. The gate counts *firings*, not conclusions

A member arms the gate the moment its connector hands over an identifiable
item — before the run it starts has done anything, and regardless of what
that run concludes. So **whatever your connector emits is a signal**. In
this example the filtering ("what counts as a block order at all") lives in
the connector, which is where a real desk's feed adapter puts it. You cannot
put it in the member's workflow and expect the gate to notice.

### 5. The correlation value *is* the firing identity

A composite fires one item per satisfied key, and that item is just
`{"correlation": "<value>"}`. Two consequences:

- The run input carries the correlation value and nothing else — no member
  payloads. This example rebuilds the case from the desk's own tape
  (`out/tape.jsonl`, the raw feed archive) and gets everything it *knows*
  from declared context.
- Because the run id is derived from that value, **one composite run per
  correlation value per trigger chain**. A second correlated pair on the
  same symbol comes back as a duplicate and starts nothing. If you need
  repeat episodes, correlate on a key that already carries the episode —
  `MRDN:VNTG@2026-09-04` rather than `MRDN:VNTG`.

### 6. Declared context is assembled from what the *firing* knows

The gate declares `context_query: "case_ctx($symbol = /correlation)"`. The
**evaluator** runs that saved query — it is the one party already holding
the memory, since on the embedded backend a tool inside a run cannot open
the file its own run is locking.

Which means the query can only be parameterized on what the firing carries:
the instrument. The case's *signature* (`block_buy+index_rebalance`) is
computed by the run, after the query has already run — so precedents come
back unfiltered and `prior_art` matches them in the tool. That is not a
workaround; it is the shape of the seam.

### 7. The precedent is about the pattern, not the instrument

A benign dismissal writes two facts into `org.surv.precedents`:

```
block_buy+index_rebalance  mg:dismissed_benign  "<the analyst's reasoning>"
block_buy+index_rebalance  mg:dismissed_by      "user:nadia"
```

Keyed on the **shape**, so in session two the same shape on a completely
different instrument arrives pre-annotated. Nothing about MRDN:VNTG carries over
to MRDN:PDRA — only the reasoning about that kind of pair does.

And the case still parks. The payoff of memory here is a better-prepared
case, not a closed one: surveillance dispositions are a regulated judgment,
and an agent that closes its own alerts is the thing this example argues
against.

### 8. Where a person is structurally required

- `run.respond` refuses the principal that started the run — the desk
  cannot dispose of its own case (`RUN-E012`).
- The driver refuses a disposition with no written reason: a benign
  dismissal that explains nothing teaches the desk nothing and tells an
  examiner nothing.
- The loop's finding is advisory: `auto_applied: 0`, and it sits `pending`
  until a named human acts on it in writing.
- An approval is a state transition, not a note. `approved` has no exit but
  `applied` or `expired`, so a second reviewer cannot erase the first one's
  decision — only act on it.

## Going live

Three seams, one shape — JSON on stdin, JSON on stdout, one process per
invocation. Replace the processes; the gate, the window, the journal, the
approval and the audit trail do not change.

| Leg | Replace | With |
|---|---|---|
| **Inbound** | `agent.py connector` | a process that reads your OMS / venue drop and your news or disclosure vendor, paging on their cursor. Contract: [`docs/triggers.md`](../../../docs/triggers.md) |
| **Outbound** | `agent.py tools` | a process that writes your case manager and your alert queue. Contract: [`docs/run.md`](../../../docs/run.md) |
| **The human** | the `decide` fixtures | `areev run respond --as <principal>`, or the console's Runs tab — the approver's identity *is* the audit record |

Two things to change deliberately when you do:

1. **The window.** Fifteen seconds is a teaching number picked for
   *headroom* — the act scripts have to fit two ticks inside it on a loaded
   CI runner. A real desk correlates over minutes to hours, and the number
   is a surveillance-policy decision, not a tuning knob.
2. **What the member triggers consider a signal.** The gate only sees what
   the connectors emit (§4). "Block order" and "material event" are your
   thresholds, and they belong in the connector, versioned like any other
   surveillance parameter.

Deployment, scheduling and the embedded-vs-Postgres decision:
[`../docs/deploy.md`](../docs/deploy.md).

## The pieces

| File | What |
|---|---|
| [`python/agent.py`](python/agent.py) | the whole agent — driver, tool seam, connector seam |
| [`smoke.sh`](smoke.sh) / [`improve.sh`](improve.sh) | the language-neutral act scripts; **these are the spec** |
| [`fixtures/orders/`](fixtures/orders/) | the order-book feed |
| [`fixtures/news/`](fixtures/news/) | the disclosure feed |
| [`fixtures/decisions/`](fixtures/decisions/) | what analysts decided, and why |
| [`CLAUDE.md`](CLAUDE.md) | the working rules for changing any of it |

## Taking the model leg live

`improve.sh` here runs the deterministic analyzers — no key, no network, and
that is the floor CI holds. Attach a model and the loop gains
DISCOVER → GROUND → VERIFY, and a draft may carry a **proposal**: a lesson, a
fact, a rewrite of a saved CAL query, field-level edits to this plan, or new
source for one of its tools.

```bash
LOOP_LLM_CMD='./examples/llm/claude.sh' ./python/improve.sh
```

Nothing about the gates changes. An `origin = llm` finding can never
auto-apply, it still needs a human review with a written reason and an
explicit apply, and every kind records the inverse that rolls it back. The
vocabulary and its per-kind rules are in [`../../llm/`](../../llm/) and
[`docs/loop.md`](../../../docs/loop.md); two agents
([`../rcm-optimization/`](../rcm-optimization/) and
[`../sanctions-screening/`](../sanctions-screening/)) exercise the whole
governed path keyless in CI against a committed draft.

## Where to go next

- [`docs/triggers.md`](../../../docs/triggers.md) — the eight trigger kinds,
  the connector contract, composites and correlation windows
- [`docs/run.md`](../../../docs/run.md) — plans, the journal, HITL asks
- [`docs/loop.md`](../../../docs/loop.md) — the analyzers and the four gates
- [`../invoice-to-accounting/`](../invoice-to-accounting/) — the same shape
  driven by a mailbox, in three languages
