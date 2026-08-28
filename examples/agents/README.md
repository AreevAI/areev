# examples/agents

Vertical agents built on Areev — end-to-end workflows where Areev is the memory
and the runtime, and the business system on either end is somebody else's.

The rest of [`examples/`](../) teaches **one seam at a time**: a policy file, the
`--llm-cmd` protocol, the analyzer contract. These teach a **job**: mail arrives,
a workflow runs, a human approves (and corrects, by reply, until they do), a
system of record gets written, and what happened is recallable afterwards —
then the loop reads those runs back and proposes a fix a person signs.

## The index

| Agent | Job | Status |
|---|---|---|
| [`invoice-to-accounting/`](invoice-to-accounting/) | Poll AP mailboxes for two clients, extract invoices, park on human approval, take corrections by email reply around a bounded cycle, post to the expense sheet — then brief itself out of its own memory and propose fixes a person approves | **runnable ×3** — the same agent in Python, TypeScript, and Rust, one file each, minting one content-addressed plan; `<lang>/smoke.sh` then `<lang>/improve.sh`, all keyless |
| [`sanctions-screening/`](sanctions-screening/) | A payment screening desk whose **screening rule lives in the memory as content-addressed code**. Outbound payments are matched against a watchlist by a CAS blob the host must pin before it will execute; possible hits park for a compliance officer; a false positive an officer signs clears that counterparty next time. Then the rule itself changes under governance — the loop finds a cluster of refusals, a person approves a fix, and the revision walks the whole chain (blob → tool → plan → trigger) while the desk refuses to run at all until the operator syncs the pin | **runnable** — Python; `python/smoke.sh` then `python/improve.sh`, keyless. The one agent here with **code-carrying tools**: unpinned execution refuses with `RUN-E018`, the pin is derived from the checkout rather than read from the memory, and every ledger row names the exact rule bytes that decided it |
| [`incident-response/`](incident-response/) | An on-call desk **woken by webhooks, not polling**: a monitoring system POSTs an alert, the desk correlates it to the service, recalls what past incidents on it turned out to be, proposes a remediation and parks for a human before anything touches production — then writes down the cause so the next identical page arrives with its history attached | **runnable** — Python; `python/smoke.sh` then `python/improve.sh`, keyless. The one agent here with **no connector at all** (`trigger_deliver`), plus a `manual` trigger for replaying an incident by hand |
| [`hiring-screening/`](hiring-screening/) | Screen candidates against one requisition's published criteria and park **every** advance/reject for a named recruiter — then produce the EU AI Act Article 14 human-oversight report *measured from the run journal*: the gate, the authorized approvers, the budget ceilings, and the kill switch's real cancel-to-drain time | **runnable** — Python; `python/smoke.sh` then `python/improve.sh`, keyless |
| [`insurance-documents/`](insurance-documents/) | A policy servicing desk: read inbound endorsements, corrections, claim notices and cancellations, keep the coverage picture current on **two clocks**, and assess a claim against the cover that was in force on the date of loss — not the cover the file holds today. Bi-temporal as-of reads (`world` vs `knowledge`), the entity graph for accumulation, an underwriter gate, then a loop finding a person approves and a retention proposal a person rejects | **runnable** — `python/smoke.sh` then `python/improve.sh`, keyless |
| [`rcm-optimization/`](rcm-optimization/) | Healthcare revenue cycle: a payer remittance lands carrying a wall of denied claims, and **the plan does not know how many** — one node returns a `$send` list, the runtime spawns a screening task per denial, joins the batch, and folds the results through declared reducers (`append`/`sum`). Then it clusters the causes per payer, parks a proposed coding fix for a billing lead who approves one and rejects another with a written reason, and next week's remittance classifies itself from what was signed | **runnable** — Python; `python/smoke.sh` then `python/improve.sh`, keyless. The one agent here with **dynamic fan-out**: 6 nodes, 11 tasks |
| [`trade-surveillance/`](trade-surveillance/) | A market-abuse desk watching two feeds that know nothing about each other. Neither is interesting alone: what opens a case is a block order **and** a material event on the SAME instrument inside a correlation window — a `composite` trigger over two member triggers, correlated on `/symbol` with a `window_ms` past which a half-match expires. The case assembles with how this *pattern* was dispositioned before (on a different instrument), and still parks for an analyst | **runnable** — Python; `python/smoke.sh` then `python/improve.sh`, keyless. The one agent here with a **composite trigger**: a single signal starts nothing, the correlated pair starts exactly one case, a pair outside the window starts nothing. A teaching example of the mechanism, **not** a compliant surveillance system |
| [`due-diligence/`](due-diligence/) | Vendor/M&A diligence where **the budget is the control**: a request works a checklist of research legs under a wall-clock ceiling, and reaching it finishes the run `BudgetExhausted` — a terminal state with a checkpoint behind it. The analyst reads what the ceiling bought and what is still unread, then forks it under a raised ceiling (or ships the partial report); a partner signs the result out, and it cannot be the person who raised the ceiling. Then a low-yield leg a partner flagged is demoted, and the same ceiling buys three times the material findings | **runnable** — Python; `python/smoke.sh` then `python/improve.sh`, keyless. The one agent here where **`BudgetExhausted` is resumable, not an error** — plus the three replay verbs: `run_verify`, `run_shadow` (zero effect dispatches, asserted against the ledgers) and `run_oversight_report` |
| [`clinical-referrals/`](clinical-referrals/) | A specialist clinic's referral desk where **what leaves the memory is not what is in it**: referral letters are filed identified — name, DOB, MRN, phone, email — and every model-facing read of that namespace comes back as typed placeholders, so the outside coding service the desk consults receives `[PERSON_1] (DOB [DATE_1], MRN [MRN_1])` and nothing more. A clinician signs each acceptance or redirection, their correction becomes the clinic's own triage rule, and `rehydrate_text` puts the real values back for the acknowledgement letter | **runnable** — Python; `python/smoke.sh` then `python/improve.sh`, keyless. The **pseudonymization-on-egress** agent: one `set_anon_policy` declaration and no privacy logic in any tool; the wire log audited against every fixture's identifiers; the `org.ops` hazard *demonstrated* rather than described; and Tier-0's limit shown by letting one relative's name through before extending the chain with `set_anonymizer_command` — which fails the read closed until the host installs it |

| [`data-subject-requests/`](data-subject-requests/) | A privacy desk that answers GDPR requests — access, portability, erasure — across a multi-namespace memory. It identifies the subject, prices the disclosure, and parks for a Data Protection Officer, because an erasure is irreversible and the approver's identity *is* the audit record. On approval it discloses or erases, then proves it: the report and the erasure share **one selector**, so what was disclosed is exactly what was removed, and a neighbouring subject is untouched | **runnable** — Python; `python/smoke.sh` then `python/improve.sh`, keyless. The **erasure** agent: `subject_report` (Art. 15), `subject_bundle` (Art. 20), `forget_subject` (Art. 17), declarative retention sweeps, consent withdrawal — and five refusals, including a wildcard namespace, because a pattern never widens a destructive surface. Deliberately has **no trigger**: starting an irreversible erasure from an unauthenticated mailbox is the wrong default |

Ten agents, and **each one is here to teach a different piece of Areev** —
the capability named in its Status column is one no other example exercises.
Read them for the mechanism, not the vertical: the invoice desk for the shape
of an agent, [`sanctions-screening/`](sanctions-screening/) for governed code,
[`data-subject-requests/`](data-subject-requests/) for erasure.

New agents follow the same shape. Start from the invoice agent — its
[`CLAUDE.md`](invoice-to-accounting/CLAUDE.md) is the working contract — and
[`sanctions-screening/`](sanctions-screening/) for the newer conventions
(code as a grain, a pin derived from the workshop, a revision that walks its
whole reference chain).

## One agent, one or more languages

An agent ships as parallel single-file stacks — `python/agent.py`,
`typescript/agent.mts`, `rust/src/main.rs` — each embedding Areev through
its own binding and all exposing the same subcommands. The agent-level
`smoke.sh`/`improve.sh` hold the assertions **once**; per-language wrappers
are three lines, so adding a language is a wrapper plus an agent file.

`invoice-to-accounting` is the one that ships in all three today; the other
nine are Python, which is why their act scripts are written
language-neutrally — porting one adds a stack without touching an assertion.
Because every seeder pins `created_at`, all stacks of one agent must mint the
identical plan hash, and [`run-smokes.sh`](run-smokes.sh) asserts it: a stack
cannot silently drift from its siblings.

## What every agent here is made of

Three seams, one shape — JSON on stdin, JSON on stdout, one process per
invocation:

| Leg | Seam | Contract |
|---|---|---|
| **Inbound** — what wakes it up | a `polling` Trigger + a connector, or a `webhook` / `manual` Trigger and **no connector at all** (your listener calls `trigger deliver`) | [`docs/triggers.md`](../../docs/triggers.md), providers: [`docs/email-providers.md`](docs/email-providers.md) |
| **Work** — what it does | a Workflow grain run by `areev run`, host tools via `--tool-cmd` | [`docs/run.md`](../../docs/run.md) |
| **Model** — where judgment needs language | `--llm-cmd`, optional | [`../llm/`](../llm/) |

So an agent example adds **no dependencies to this repo**. A vendor SDK, if you
need one, lives in your copy of the connector script — never here. That is the
same posture the core takes (no clap, no HTTP framework, no MCP SDK), applied
to examples.

## Two paths, always

- **Keyless** — mock connector, mock tools, committed fixtures. No credentials,
  no network, no model key. This is what the smokes and CI run, and it is the
  reason these examples stay correct across releases instead of quietly
  rotting. Run them all: [`run-smokes.sh`](run-smokes.sh); how it works:
  [`docs/testing.md`](docs/testing.md).
- **Live** — real connectors and real tools, opt-in behind env vars
  ([`docs/email-providers.md`](docs/email-providers.md)); deployment,
  scheduling, and the embedded-vs-Postgres decision:
  [`docs/deploy.md`](docs/deploy.md).

## Where the human goes

Each one parks on a `client` node — an approval a person answers with
`areev run respond --as <principal>` (here: by replying to an email) —
because these are agents that spend money or sign things. The principal who
triggered the ask structurally cannot answer it (separation of duties), and
the approver's identity *is* the audit record. A correction the human makes
on the way to "yes" is recorded as memory, which is where self-improvement
actually starts. An agent example that approves its own work is teaching the
wrong lesson.

## Adding one

Layout, the keyless-floor rule, the no-new-dependency rule, and the indexes
that need updating in the same commit are in the `areev-examples` skill
(`.claude/skills/areev-examples/`); the test contract is
[`docs/testing.md`](docs/testing.md).
