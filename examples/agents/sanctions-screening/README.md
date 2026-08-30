# sanctions screening — the rule is a grain

**The problem.** A screening desk lives and dies by one question an
examiner will eventually ask: *which exact version of the rule screened
this payment?* When the rule is a script on a box, the answer is a
changelog and a shrug — and nothing stops the desk quietly running last
quarter's rule after the list, or the rule, changed.

**What you get.** Every ledger row names the exact rule bytes that decided
it, answerable from the content address rather than from anyone's memory of
a deploy. A rule change is a signed, governed chain instead of an edit —
and a desk whose rule has moved ahead of its operator's pin **refuses to
run at all** rather than running stale. The acts prove the payoff both
ways: the revision is walked end to end under a human signature, and the
revised rule promptly catches an exact watchlist match the old rule was
blind to.

The desk itself: its **screening rule lives inside the memory as
content-addressed code**, not as a script on a box. It matches
outbound payments against a watchlist, parks possible hits for a compliance
officer, learns from the dispositions officers sign — and when the rule
itself needs to change, the change is governed end to end: new bytes, new
address, a human decision, and a host pin that must be synced before
anything executes.

```
 payment rail                 the areev screening desk                officer
 ──────────►  queue  ──poll──►  screen ─► triage ──no match──►  release ─► ledger
  "PMT-1004    (mock /         (a CAS       │
   250k USD"    real)           blob)       │ possible match
                                  ▲         ▼
                    host pin ─────┘    open case ──────────►  [ a person ]
                  --allow-executor              "release | block | false positive"
                  (RUN-E018 if absent)                │
                                                      ├── block  ─► ledger
                                                      └── false positive
                                                            ↓
                                                     disposition becomes memory
```

And the loop that makes it *self-improving* — where "improving" means the
**code** improves, under a gate:

```
 failed runs, dispositions ──► areev loop (deterministic analyzers)
       (the journal)                  │ Recommendation + evidence
                                      ▼
                                [ a person ] ──approve, signed──► revised rule
                                                                        │
   new blob address ─► supersede Tool ─► supersede Workflow ─► re-point Trigger
                                                                        │
                          RUN-E018 until the operator syncs the pin ◄────┘
```

Nothing here needs a credential, a network, or a model key. The whole thing
runs from committed fixtures, so CI proves it on every release.

## Run it

| Stack | Needs | Run |
|---|---|---|
| [`python/`](python/) | `pip install areev` | `python/smoke.sh` then `python/improve.sh` |

A few seconds later, the two acts end with what was promised:

```
OK -- 2 released, 2 blocked, 1 unpinned refusal, 3 decisions signed by name.
OK -- 1 disposition became memory, 1 rule revised under a pin, 2 rule versions on the ledger.
```

## Week one — `smoke.sh`

Four payments queue up behind one watchlist:

| Payment | What it is | What the run does |
|---|---|---|
| NorthWind Logistics, 4,200 USD | no list match | **Releases itself.** Nobody is woken up. |
| Volkov Trading OOO, 88,000 USD | exact match, SL-0002 | **Parks.** Mo blocks it; their name goes on the ledger row. |
| Aurora Metals Ltd (Sheffield), 12,500 GBP | 0.667 match to *Aurora Metals LLC* | **Parks.** Mo clears it as a false positive with a written reason — and that reason becomes a fact. |
| Sable Freight Holdings, 250,000 USD | exact match, SL-0001 | **Parks.** Ines blocks it and escalates. |

Four governance properties are asserted, not narrated:

- **Unpinned code does not run.** Before anything else, the smoke starts one
  payment with no `--allow-executor` and the runtime refuses with
  **`RUN-E018`** *before the first journal write*. The blob travels with the
  memory; the permission to execute it never does. Import someone's bundle
  and you import their rule — not the right to run it.
- **The pin is derived from the workshop.** `agent.py pin` hashes
  `src/screen.py` and the smoke asserts it equals the seeded blob's address.
  The host authorizes exactly the code its operator can read.
- **The desk cannot clear its own case.** It replies to its own ask and the
  runtime refuses — the responder structurally cannot be the principal that
  started the run.
- **Every decision names the rule that made it.** `agent.py provenance`
  answers the examiner's actual question — *which version of the rule
  screened this payment?* — from the content address, not from a changelog.

## Weeks two and three — `improve.sh`

More payments. The disposition Mo signed in week one now clears the same
counterparty automatically — no officer, no second review. Then three
payments arrive whose counterparty names are **Cyrillic homoglyphs double-
encoded as UTF-8**: `VÐ¾lkov TrÐ°ding OOO`. The rule *refuses them* rather
than screening a mangled string, because a false clear is the expensive
failure and a stopped payment is the cheap one. Those refusals cluster in
the journal.

Then the part no other example in this repo can show:

1. **`areev loop run` finds the cluster** — `failed 3/8 recent runs (38%):
   screen: … is not readable ASCII`. Found by deterministic analyzers — the
   lesson kind that measured stronger than LLM-authored ones on our benchmark
   ([RESULTS.md](../../../crates/areev-bench/RESULTS.md#areev-loop-self-improvement--the-abab-causal-proof)),
   and keyless besides; `LOOP_LLM_CMD` adds verified LLM reflection on top.
2. **The gate holds.** The engine refuses to apply its own advisory finding,
   and refuses any decision with no written reason.
3. **A person approves, signing name and reasoning.**
4. **The rule is revised** — and the revision is a *chain*, because
   everything downstream names its input by content address:

   ```
   new bytes → new blob address
             → supersede the Tool definition to name it
             → supersede the Workflow (bindings name tools BY HASH,
               so the plan moves too, minting a new plan hash)
             → re-point the Trigger (triggers do NOT follow heads)
   ```

   Skip any link and the desk quietly goes on running the old rule. The act
   script asserts the whole chain.
5. **The desk then refuses to run at all** (`RUN-E018`): the memory's rule
   has moved ahead of the operator's pin. The cursor is **held** —
   `consecutive_failures=1`, nothing silently dropped — and the payments
   wait. A desk refusing every start is safe but doing nothing, which is why
   `trigger-state` is the thing to watch.
6. **The operator syncs the checkout** and the same payments go through
   under v2 — which promptly catches an **exact list match that was hiding
   behind the homoglyphs**. v1 was loud but blind; v2 sees it. The ledger
   records which rule version decided each payment.

## What this example is teaching

**Code that is a grain can be governed; code that is a file cannot.** The
loop's `code_revision` class only works on content-addressed code: the
revision pins the evalset it was gated against, apply is refused without the
recorded gating run, and `areev tool provenance` chains code → recommendation
→ approver → the runs that executed it. A `--tool-cmd` script gets none of
that — the loop can still *flag* it, but the fix happens in your editor,
ungoverned and invisible.

**Declarations travel; authorizations do not.** The blob, the definition,
the plan and the trigger all replicate in a bundle. `--allow-executor` never
does. Effective permission is always *declared ∩ granted ∩ host-configured*,
and the pin is the host's half.

**A blob and a host tool have the identical contract** — run state as JSON on
stdin, `AREEV_TOOL_NAME` in the environment, result JSON on stdout. Moving
logic from `--tool-cmd` into a grain is a **packaging change, not a
rewrite**, which is what makes the migration in §4 of the
[how-to guide](../../how-to-create-an-areev-agent.md) practical.

**A disposition is narrower than the rule.** `mg:screened_clear` clears one
counterparty, never the list entry it matched. Widening that is how a single
false positive turns into a permanent blind spot.

**Keep operational grains out of policied namespaces.** Client knowledge
lives in `org.psp` / `org.psp.counterparties`; the plan, tool definitions,
triggers and journals live in `org.ops` and never get an anonymization
policy — a rewriter that turns 64-char hashes into `[PERSON_1]` breaks
bindings.

## The pieces

```
src/screen.py        the rule -- the WORKSHOP copy, seeded as a CAS blob
src/screen_v2.py     the revision: repairs homoglyphs instead of refusing
python/agent.py      the whole agent, one file, embedded Areev
smoke.sh, improve.sh the two acts -- language-neutral assertions
fixtures/payments/   ten synthetic payments ("PAY_UPTO" is the clock)
fixtures/watchlist.json  a synthetic screening list -- every entity fictional
fixtures/decisions/  officer decisions, each carrying a written reason
```

## Going live

The mock connector and the host tools are the only fake parts.

1. **Queue** — replace the `connector` subcommand with your payment rail's
   API (same JSON-on-stdio contract). Real screening lists come from a
   publisher; keep them arriving as data, not as memory.
2. **The rule** — keep it a blob. For stronger isolation compile it to
   `wasm32-areev` (fuel and memory ceilings, no I/O) or `wasm32-areev-io`
   (declared capabilities through the credential broker). Note the broker's
   flags are reachable from the CLI and `trigger run`, **not** from the
   Python/Node `run_start`.
3. **Pins in production** — derive `--allow-executor` in your deploy from the
   same artifacts you ship, and alert on `consecutive_failures`. A stale pin
   is safe but silent.
4. **Identity** — the acts map decision fixtures to principals; in
   production, approvals deserve per-principal credentials (`areev ui
   --auth`, where `run.respond` refuses shared-token callers) and grants in
   the file.
5. **Schedule both loops** — the ingest heartbeat and the nightly
   improvement pass. `LOOP_LLM_CMD` adds verified LLM reflection on top of
   the deterministic floor.

## Caveats

This is a **teaching example of the mechanism**, not a compliant sanctions
programme. The matching is a toy Jaccard score, the list is four invented
entities, and every payment, counterparty, officer and identifier is
fictional. Real screening involves fuzzy transliteration, secondary
identifiers, ownership resolution, and regulatory obligations this example
does not model.

## Where to go next

- [`../../how-to-create-an-areev-agent.md`](../../how-to-create-an-areev-agent.md)
  — §4 (where tool code lives) is the section this example implements
- [`../../../docs/run.md`](../../../docs/run.md) — the runtime
- [`../../../docs/loop.md`](../../../docs/loop.md) — analyzers and gates
- [`../../../docs/security-model.md`](../../../docs/security-model.md) — the
  declared ∩ granted ∩ host-configured split
