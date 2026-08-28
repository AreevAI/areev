# due diligence under a budget

A vendor/M&A diligence desk. An analyst files a request naming a target; the
agent works a checklist of research legs — registry filings, litigation,
adverse media, filed accounts — accumulating findings. Research is
open-ended and expensive, so **every request runs under a ceiling**. When the
ceiling is reached the run stops cleanly with its work journaled, the analyst
reads what the money bought and what is still unread, and decides: **raise
the ceiling, or ship the partial report.** No report leaves the building
without a partner's signature, and the partner may not be the person who
raised the ceiling.

```
 analyst                    the areev diligence agent                     partner
───────►  file request ──►  intake ─► next_leg ─► research ──┐
          (a ceiling         │           ▲                    │
           in ms)            │           └────── loop ────────┘  bounded, ≤8
                             │                   │ every leg read
                             │                   ▼
                             │               assemble ──►  partner_review  ──►  issue
                             │                                   ▲              report
                             │  ceiling reached                  │                │
                             ▼                             [ a person ]        shelve
                    BudgetExhausted { axis: WallMs }        "issue | shelve"
                    ── terminal, journaled, RESUMABLE ──     + a written reason
                             │
                             │  [ the analyst decides ]
                             ├──► ship the partial report
                             └──► fork ──► resume under a raised ceiling ──► ...
```

`BudgetExhausted` is **not an error**. It is a terminal outcome with a
checkpoint behind it, and `run_fork` re-opens exactly that checkpoint under
new budget knobs. The exhausted run is never touched — its journal stays as
the record of what the first ceiling bought.

And the second loop, the one that makes the desk better at spending the same
ceiling — every run outcome and every partner's note is already a grain, so
improvement is queries over the desk's own record:

```
 run journals, partners' notes ──► areev loop (deterministic analyzers)
        (what actually happened)          │ Recommendation + evidence, by hash
                                          ▼
                                    [ a person ] ── diagnoses it, approves
                                          │          with a written reason
                                          ▼
                                   a desk rule that CITES that approval
                                          │
   next request's leg order  ◄── saved CAL queries feed it back ◄───┘
```

Nothing here needs a credential, a network, or a model key: the whole thing
runs from committed fixtures. Every company, person, court, publication and
reference number in `fixtures/` is invented.

## Run it

```
python/smoke.sh          # act one  — the ceiling, the fork, the signature
python/improve.sh        # act two  — the payoff, the loop, the desk rule
```

Both together take about 16 seconds. They end with:

```
OK -- 1 ceiling reached, 1 fork, 1 report issued, 2 refusals, 2 runs replayed with 0 effects.
OK -- same ceiling, 3x the material findings; 1 cluster found, 1 diagnosis signed, 1 desk rule adopted.
```

| Stack | Needs | Run |
|---|---|---|
| [`python/`](python/) | `pip install areev` | `python/smoke.sh` then `python/improve.sh` |

The act scripts ([`smoke.sh`](smoke.sh), [`improve.sh`](improve.sh)) are
language-neutral and hold every assertion; each language directory is a
three-line wrapper that exports `AGENT` and `AGENT_OUT`.

## Act one — `smoke.sh`

One request, DD-2026-0114, against Vantridge Logistics Group, under the
desk's standing 2,900 ms ceiling.

| Step | What it exercises |
|---|---|
| the ceiling bites | `run_start(..., max_wall_ms=2900)` finishes `BudgetExhausted { axis: WallMs }` after two of four legs — **1 material finding**, two legs unread, no report issued |
| the journal survived | `run_inspect` shows the ceiling, the spend that exceeded it, the checkpoints and the journal entries — nothing was lost by stopping |
| asking again buys nothing | `run_resume` on the exhausted run returns the same `BudgetExhausted` and the findings ledger does not move: the ceiling is on the run's **frozen manifest**, and only a fork gets its own knobs |
| the analyst raises it | `run_fork(base, new)` returns a 64-hex seed checkpoint; `run_resume` on the fork **continues** the exhausted run — the act asserts the base's legs are a strict subset of the fork's — reads the rest, and then **parks on the partner gate**. Research finished is not file finished. `fork_of` records `(base_run, base_superstep)` |
| separation of duties | the analyst who forked signs her own report → **RUN-E012**, refused by the runtime. Every client ask is an approval boundary |
| no unsigned reports | a review fixture with an empty `because` → refused by the desk, before the runtime is asked |
| the signature | a partner signs with a written reason; only then does the run reach `Completed`, with no pending asks and the whole checklist read, and the report lands on the ledger under **her** name |
| the replay | `run_verify` re-derives and byte-compares every checkpoint of both runs; `run_shadow([both])` replays them with `effect_dispatches: 0` — and both output ledgers are asserted **unchanged**, so auditing the desk does not re-run it |
| the oversight report | `run_oversight_report` names `partner_review` as the client gate and states the separation-of-duties rule, measured from the journal rather than asserted |
| the note becomes memory | the partner's low-yield note on `adverse_media` lands as a Fact in `org.diligence.learned`, with her reason as an Observation beside it |

## Act two — `improve.sh`

| Step | What it exercises |
|---|---|
| the same ceiling, spent better | DD-2026-0231, same sector, **same 2,900 ms**. Memory demotes `adverse_media` to the back of the queue, so the ceiling buys `corporate_filings` + `financials` instead — 1 → **3 material findings** on a normal machine. The *assertion* is the invariant, not the number: same leg count, strictly more material findings (which holds for any prefix the ceiling can buy) |
| demote, never drop | the demoted leg is still read once the ceiling comes off. Nobody signed off on not looking |
| the routine book | three more requests run unbudgeted; two die on the `research` node because Ravensmoor publishes FY25 accounts on a Q3 calendar and the `financials` leg has nothing to read. They **fail loudly** rather than reporting "nothing found" |
| the loop finds the cluster | `loop.run_outcome/1` flags the plan: *failed 4/6 recent runs (67%)*. That is **all** it says — it found the cluster, it did not diagnose it |
| the gates | `apply` on an advisory finding → refused (LOP-E011). `approve` with no reason → refused |
| a rule must cite something | `adopt` against a still-pending finding → refused. A standing desk rule has to name a finding a person approved, so the rule and the reason for it stay one record |
| a person decides | a partner reads the journals, writes the diagnosis into the approval, and adopts `Ravensmoor: an unpublished financials is a gap, not a failure` |
| the payoff | the re-filed request reaches the partner's desk with the unread leg written into the report as a **GAP**, instead of dying on it |
| it does not nag | a second loop pass proposes no second copy of the same finding |

## Fixtures

| Fixture | What it exercises |
|---|---|
| `requests/01-vantridge.json` | act one's file. Ravensmoor, regional-logistics |
| `requests/02-halcyon.json` | act two's file — deliberately the **same sector and the same yield shape** as 01, so the two ceilings compare like with like |
| `requests/03-merrowgate.json`, `04-pallister.json` | two Ravensmoor targets whose `financials` leg is unpublished — the failure cluster the loop finds |
| `requests/05-brantwick.json` | Colgrave-registered, accounts published: the control that proves the pattern is the filing calendar, not the plan |
| `requests/06-merrowgate-refile.json` | the re-file after the desk rule is adopted |
| `records/TGT-44xx.json` | the synthetic record sets, per target, per leg. `material: true` marks a record that changes the conclusion; `unavailable` marks a leg with nothing to read |
| `reviews/00-analyst-signs-own.json` | the separation-of-duties refusal |
| `reviews/01-unsigned-issue.json` | the no-written-reason refusal |
| `reviews/02-vantridge-issue.json` | the signature that issues act one's report **and** carries the low-yield note act two reads back |
| `reviews/03-halcyon-issue.json` | act two's signature |

## What this example is teaching

**A budget is a first-class control with a resumable outcome.** Most agent
frameworks treat a spend cap as an exception: you catch it, you log it, you
lose the work. Here the ceiling is part of the run's frozen manifest, the
scheduler checks every axis *before* each superstep, and reaching one
terminalizes the run at a checkpoint like any other outcome. `run_inspect`
then answers the only two questions an operator has — what did it spend, and
what is still unread — and `run_fork` is how the answer becomes more
research. The four axes `run_start` takes are `max_tokens`,
`max_usd_micros`, `max_wall_ms` and `ask_ttl_sec`; this example uses the
wall-clock one because it is the axis a keyless demo can actually reach.

**The exhausted run is evidence, not garbage.** It keeps its journal, its
spend and its checkpoints. The fork descends from it (`fork_of` names the
base run and superstep), so "we looked at two legs for £X and then a person
decided to spend more" is one auditable chain rather than a story someone
tells afterwards.

**The record replays, and replaying it changes nothing.** `run_verify`
re-derives every checkpoint and byte-compares it against the stored chain.
`run_shadow` goes further: it replays whole runs with no executor attached
at all, so the report's `effect_dispatches: 0` is structural. The act script
does not take that on faith — it counts the lines in both output ledgers
before and after and asserts they did not move.

**Approval is proportional to identity.** `partner_review` is a *client*
node, which is what makes the run park rather than decide for itself. Every
client ask is an approval boundary, so `run_respond` refuses the principal
that triggered the run — and because a fork's triggering principal is the
**forker**, the analyst who chose to spend more is exactly the person who
cannot then sign the result off.

**Memory changes what the same money buys.** The partner's note is not a
config flag; it is a Fact in `org.diligence.learned` with the partner's name
and reason on it, written at the moment the decision took effect. The desk
reads it back through a saved CAL query that lives *in the memory file* and
replicates with it, and it only ever **demotes** a leg — a leg that stops
being read is a leg nobody signed off on not looking at.

**The loop finds clusters; people find causes.** `loop.run_outcome/1` says
"this plan failed 4 of 6 runs". It does not say why, and it is not allowed
to act. A person reads the journals, writes the diagnosis into the approval,
and adopts a desk rule that cites the approval by hash. That is three
separate acts, each with a name on it.

## Known operational facts

- **A hard-crashed run is held by a lease.** If the process holding a run
  dies, the run is unresumable for a hardcoded 10-minute lease window
  (`RUN-E021`), with no override. A **parked** run releases its lease
  deliberately, which is why the human-in-the-loop path in this example
  resumes instantly. Plan operational recovery around the lease; do not plan
  around defeating it.
- **`finished` is a Rust `Debug` rendering**, not a stable token — you get
  `BudgetExhausted { axis: WallMs }`, `Failed { node: "research", detail: … }`.
  Match a **substring**, never the whole string. The act scripts do.
- **`DD_LEG_MS` (default 1500) is the simulated cost of one research leg.**
  A real registry pull or docket search takes seconds; without a stand-in
  cost, a wall-clock ceiling means nothing on a laptop. The act scripts set
  it to 0 for the bulk runs, where the ceiling is not the point.
- **One memory is one writer.** The driver holds a single handle for a whole
  batch of runs; opening a second handle on the same file fails at open
  (`STO-E002`) by design. The `tools` subcommand never opens the memory at
  all — the runtime that spawned it is holding it.
- **A fork inherits its base run's context**, including anything the driver
  put in the run input. That is why the host passes `--run <id>` down to the
  tool seam: without it the fork's tools would file their findings under the
  base run's id.
- **Run state is read from the journal, never from the effects ledger.**
  `findings.jsonl` records what left the desk; the run's own last checkpoint
  (a State grain whose `context` carries the serialized scheduler state)
  records what the run *did*. A fork's seed checkpoint carries the base's
  context verbatim, so "what has this file read" is one journal read with no
  lineage walk — and it cannot drift from the runtime the way a side file
  can. This is also what makes the `run_shadow` assertion meaningful: the
  ledger is purely the effects record, so its line count not moving is a
  real statement about effects.
- **A lock is waited on, not treated as fatal.** One memory is one writer, so
  a driver subcommand starting while the previous one is still tearing its
  handle down would otherwise fail with `STO-E001` depending on machine load.
  `open_db` retries for up to 10 seconds.
- **Both acts are idempotent.** `smoke.sh` wipes `out/` first; `improve.sh`
  re-runs act one when it detects act two has already run there, because
  filing a request, adopting a desk rule and re-filing are not no-ops the
  second time.

## Going live

Replace one seam and nothing else changes:

- **`agent.py tools`** becomes processes that call your registry provider,
  your docket search and your media vendor — same contract, JSON on stdin,
  JSON on stdout, one process per effect, `$AREEV_TOOL_NAME` selecting the
  leg. No SDK enters this repo; the heavy dependencies live in your tool
  script.
- **`sign`** becomes your case-management webhook. Keep the two refusals:
  the responder must not be the triggering principal, and the reason must be
  written.
- **The ceiling** becomes a real one. `max_usd_micros` is the axis most
  desks actually want; `max_wall_ms` is here because it is the one a keyless
  fixture-backed demo can reach.
- **The requests** arrive from wherever they arrive. This example drives
  `run_start` directly because the budget knobs and explicit run ids are the
  subject; a standing rule that starts runs on a queue is
  [`docs/triggers.md`](../../../docs/triggers.md), and
  [`../sanctions-screening/`](../sanctions-screening/) shows that shape.

## The pieces

```
smoke.sh / improve.sh        the two acts — language-neutral, every assertion
python/agent.py              the whole agent: tool seam + driver
python/{smoke,improve}.sh    three-line wrappers
fixtures/requests/           the quarter's request book (2-digit prefix = the clock)
fixtures/records/            synthetic record sets, per target, per leg
fixtures/reviews/            partners' decisions, including the two refusals
out/                         gitignored: the memory file + both ledgers
```

## Where to go next

- [`../sanctions-screening/`](../sanctions-screening/) — the code an agent
  runs on as a governed, content-addressed grain, and a polling trigger.
- [`../invoice-to-accounting/`](../invoice-to-accounting/) — the same
  lifecycle in three languages, with mail connectors and bounded correction
  cycles.
- [`docs/run.md`](../../../docs/run.md) — budgets, journals, forks, the
  run ↔ memory join.
- [`docs/loop.md`](../../../docs/loop.md) — the analyzers, the four gates,
  the recommendation lifecycle.
