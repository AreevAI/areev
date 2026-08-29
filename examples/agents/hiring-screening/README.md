# hiring screening — oversight you can *measure*

A candidate-screening desk for one job requisition. Applications arrive, the
agent reads each one, checks it against the criteria the requisition
published, and then **parks it for a named recruiter**. Every single one.
There is no auto-advance path in this plan — not a disabled one, an absent
one.

**The problem.** Screening candidates for employment is an **Annex III
high-risk** use under the EU AI Act — Regulation (EU) 2026/1744, published
in the Official Journal on 24 July 2026 and in force from 27 July — and its
Article 14 asks a question a policy document cannot answer: *can a natural
person actually oversee this thing, and can you show it?* Most teams answer
with prose. An auditor wants a record.

**What you get.** The record, as a command — and a desk where the record
cannot come apart from the practice: every outcome pairs with the named
recruiter who decided it, the counts must balance, and the oversight report
is measured from the run journal rather than asserted. This example exists
to answer Article 14 from the record:

```bash
python/smoke.sh          # the week, under governance -- ends by printing the report
```

```json
{
  "human_gates": {
    "client_gated_nodes": [{"node": "recruiter_review", "tool": "recruiter_review"}],
    "every_client_ask_is_an_approval": true,
    "separation_of_duties": "responder != triggering principal, refused structurally",
    "ask_ttl_sec": 172800
  },
  "authorized_responders": {"principals_granted_run_respond": ["user:ines", "user:mo"]},
  "budgets": {"max_tokens": 200000, "max_usd_micros": 1500000, "max_wall_ms": 120000},
  "kill_switch": {
    "verb": "run.cancel (deliberately the lowest-privilege run verb)",
    "measured_cancel_to_drain_ms": [2]
  },
  "plan_hash": "6adaafd898188707e3d61146bd02bc2f769b793b2aec401f4a2d0f2619aa4607"
}
```

Not one field of that was configured for the report's benefit. The gate is
read off the run's frozen manifest, the approvers off the grant Facts in the
memory file, the ceilings off the budgets the runs actually ran under, and
the kill-switch number is **measured** — journaled cancel Fact to terminal
checkpoint close — on a real cancel that happened earlier in the same week.

Nothing here needs a credential, a network, or a model key: the whole thing
runs from committed fixtures, so CI proves it on every release.

## What this example does — and does not — do

**Does**: demonstrate human oversight and record-keeping. A person decides
every outcome; every decision carries a name and a written reason; the
journal replays byte-identically afterwards; and the evidence of all of that
is a command, not a claim.

**Does not**: score candidates, rank them, or test anything for bias. The
criteria are three boring, job-related, published facts (years of backend
experience, one named certification, work authorisation for the posting
location) and the agent reports each one as **met**, **missed**, or **not
evidenced** — nothing else. Areev has no fairness-testing capability and
this example does not imply one. Deploying anything like this for real means
doing your own bias assessment, your own DPIA, and your own candidate
notice; what Areev gives you is the record you will need while doing it.

**And a human gate is not an exemption.** The European Commission's
guidelines on Article 6(5) (19 May 2026) are explicit that human involvement
does not change whether a system is classified as high-risk. Parking every
candidate for a recruiter is how you *discharge* Article 14, not how you
escape Annex III.

## What the law actually asks for

Worth being precise about, because the popular framing ("AI hiring laws
require a human in the loop") is mostly wrong:

| Regime | What it actually requires | Where this example fits |
|---|---|---|
| **GDPR Art. 22** | The one genuine **ex-ante human-decision gate** in hiring: a decision based solely on automated processing with legal or similarly significant effects needs a lawful basis, and the data subject may obtain human intervention. | This is the obligation the client gate discharges directly — no outcome is ever reached by automated processing alone. |
| **EU AI Act** (Reg. (EU) 2026/1744), Annex III §4 + Art. 14 | Employment screening is high-risk; oversight must be *designed in and demonstrable* — intervene, interrupt, understand. Not "a human somewhere". | `run_oversight_report`, measured from the journal; `run_verify`; `run_cancel` as the low-privilege brake. |
| **NYC Local Law 144** | An annual independent **bias audit**, published results, and candidate notice. It does **not** require human review: 6 RCNY § 5-304(a) — *"Nothing in this subchapter requires an employer or employment agency to provide an alternative selection process."* The US pattern is **audit → notify → explain → retain**. | Areev is the *retain* and *explain* leg. It does not perform the bias audit. |
| **US federal** | Title VII and the Uniform Guidelines on Employee Selection Procedures (29 C.F.R. Part 1607, the four-fifths rule at § 1607.4) remain in force. The EEOC's 2023 Title VII AI guidance and 2022 ADA guidance were withdrawn following E.O. 14179 and their URLs now 404 — do not cite them. | Adverse-impact analysis is yours. The journal gives you the per-decision record it needs as input. |
| **State ADS record-keeping** | California FEHA automated-decision-system records: **4 years**, and that duty extends to vendors. Colorado ADMT: **3 years**. | Grains are immutable and content-addressed, so the retention floor is a policy (`areev retention set --days 1460 --ns org.talent`), not a hope that nobody edited a log. |

## The shape

```
 applicant                    the areev screening desk                    recruiter
──────────►  ATS queue  ──poll──►  parse ─► check ────────────────►  [ a person ]
 CV + form    (Greenhouse   trigger   │      criteria                 "advance | reject"
              / Workday                │      met / missed /                │
              / your own)              │      NOT EVIDENCED       ┌─────────┴────────┐
                                       │                          ▼                  ▼
                                       ▼                       advance            reject
                                  FAILED, loudly                  └──────► close ◄──┘
                            (no text layer -- never
                             a rejection, always a
                             person picking it up)

                        there is NO edge from `check` to an outcome
```

And the evidence loop that makes it auditable — every one of these is read
back out of the same journal the work wrote:

```
 runs, asks, responses, cancels ──►  run oversight-report   Art. 14: gate, approvers,
        (the journal)             │                         TTL, budgets, measured
                                  │                         cancel-to-drain
                                  ├─►  run verify           every checkpoint re-derived
                                  │                         and byte-compared
                                  └─►  areev loop           a cluster in the record,
                                                            proposed to a person
```

## Run it

| Stack | Needs | Run |
|---|---|---|
| [`python/`](python/) | `pip install areev` | `python/smoke.sh` then `python/improve.sh` |

Or everything at once (what CI runs): [`../run-smokes.sh`](../run-smokes.sh).

A few seconds later:

```
OK -- 4 of 4 screened candidates went through a person, 2 advanced, 1 rejected, 1 canceled, 1 unreadable file refused.
OK -- 1 written reason became a precedent, 5 unreadable files refused rather than rejected, 1 finding approved by a person, 5 outcomes each signed by name.
```

## Week one — `smoke.sh`

Five applications arrive for **REQ-4417 — Senior Platform Engineer, Dublin**.

| Application | What it is | What the run does |
|---|---|---|
| APP-2001 Roan Vessik | meets all three published criteria | **Parks anyway.** mo advances it. Meeting the criteria is not a decision. |
| APP-2002 Imre Dalquist | two years against a stated four, no CPO-2 | **Parks.** mo rejects, in writing, on the published criteria — and that reason becomes memory. |
| APP-2003 Sunil Trevane | years and certification evidenced; work authorisation simply **not stated** | **Parks.** ines advances it: *a missing statement is a question for the first call, never a screen-out.* This is the case the "not evidenced" category exists for. |
| APP-2004 Neve Aldritch | withdraws mid-review | **Canceled.** The kill switch drains the run; no decision is ever recorded about them. |
| APP-2005 (scan, no text layer) | the ATS uploaded a page image | **Fails, loudly.** A candidate must never lose the process because our parser could not read their file. |

Five governance properties are asserted, not narrated:

- **No auto-advance path exists** — asserted against the *stored plan*, not
  the seeder: every edge into `advance` or `reject` has `recruiter_review`
  as its source, and `check_criteria` has none.
- **The desk cannot decide its own case.** It tries; `run_respond` refuses
  with `RUN-E012` because the responder may not be the triggering principal.
- **A verdict the plan has no edge for never reaches the runtime.**
- **The brake is the lowest-privilege verb.** `user:coordinator` — who was
  deliberately *not* granted `run.respond` — stops a run. The oversight
  report then shows exactly that asymmetry: two approvers, and a
  brake-puller who is not one of them.
- **Outcomes and humans are counted, and must be equal.** `agent.py
  gate-audit` reads the *run journal* — not this desk's own ledger — and
  pairs every completed `advance`/`reject` effect with the completed
  `recruiter_review` client effect whose `author_did` is the recruiter who
  answered. Three outcomes, three human reviews, none self-reviewed. This is
  the assertion that catches a regression which quietly drops the person,
  whatever caused it.
- **The record has not been edited after the fact.** `run_verify` re-derives
  every checkpoint of all five runs from their journals and byte-compares —
  including the failed one and the canceled one.

Then step 9 prints the Article 14 report and asserts its content.

## Week two — `improve.sh`

Two more applicants, and four more broken ATS exports.

- **A written reason becomes a precedent.** APP-2006 arrives with exactly
  the mismatch mo rejected in week one, so the review queue now carries
  mo's recorded reason next to it. It **still parks** — the precedent
  informs a person and decides nothing, and ines' own reason cites it
  rather than quietly inheriting it. Consistency you can read, not
  consistency you have to trust.
- **Five unreadable files, zero rejections.** The parse refusals stack up
  in the record as failed runs; not one of them became an outcome.
- **The desk briefs itself out of its own memory** — `desk_pulse`, a saved
  CAL query stored *in the file*, assembles the plan, the tool definitions,
  the criteria and the grants under a token budget.
- **`areev loop run` finds the cluster**: `Tool "parse_application" failed
  10 times (45% of the calls that could fail this way): unreadable: the
  uploaded file has no text layer`. Deterministic analyzers; no model key
  was involved.
- **The gates hold.** The pass proposed one finding and applied none; a
  blank `BECAUSE` is refused by the engine (`LOP-E011`); a recruiter
  approves with a reason; approving the same finding twice is refused
  (`LOP-E020`) — one finding carries exactly one signed decision. A second
  loop pass proposes nothing new.
- **Eleven runs later the numbers still balance**: five outcomes, five
  human reviews, six runs that reached no outcome at all — and the report
  still shows one client gate, the same two approvers, the same four
  ceilings, the kill switch still measured.

`improve.sh` checks its own precondition rather than assuming it: if the
memory is not in the exact post-`smoke.sh` state (five runs, none still
open), it re-runs `smoke.sh` first. Run it twice, run it alone on a fresh
checkout, run it against a half-finished state — it rebuilds week one and
proceeds.

Both scripts also take an exclusive lock on their `out/` before touching
it. Areev's single-writer guard is *process-wide*, so it refuses a second
handle inside one process but cannot see a second **OS process** opening
the same memory file; two act scripts sharing one `out/` would interleave
and answer each other's asks. That is a real hazard for anyone driving one
memory from two schedulers, and worth copying: one driver per memory file,
enforced outside the library.

## What this example is teaching

**Oversight is a property of the graph, not of the prose.** `recruiter_review`
is a node whose Tool definition carries `executor_kind: "client"`, which is
what makes the run park and hand a `requires_action` envelope to a person.
Because the plan is a content-addressed grain and the run's manifest freezes
its bindings at start, "was there a human gate on the run that decided this
candidate" is a lookup, not an argument.

**Budgets are a governance control, and they are reported from the record.**
Four ceilings ride every run: `max_tokens`, `max_usd_micros`, `max_wall_ms`
and `ask_ttl_sec`. They are journaled into the run manifest at start, which
is how the oversight report can quote them. A fifth axis,
`max_storage_bytes`, exists in the scheduler's `BudgetsSpec` but is not
reachable from the Python binding — the binding pins it to `None`, so this
example does not pretend to set it.

`max_wall_ms` is **compute** time, not calendar time: a run parked two days
on a recruiter accrues *elapsed*, never *wall*. A 48-hour ask TTL and a
two-minute wall ceiling are not in tension, which is what makes a wall
budget usable on a human-in-the-loop plan at all.

**Who may approve lives in the file.** `GRANT run.respond ON "org.talent"
TO "user:mo"` writes an `mg:permits` Fact in the reserved `agent:authz`
namespace. It replicates with the memory, `SHOW GRANTS` reads it back, and
so does the oversight report — the approver list is not host configuration
somebody can forget to copy.

**The kill switch is measured, not promised.** `run.cancel` writes a marker
Fact and is deliberately the lowest-privilege run verb: a brake must never
be blocked by missing privilege. The drive loop polls it every superstep, so
cancel→drain is two journaled timestamps, and the report subtracts them.

**"Not evidenced" is a first-class outcome.** Most screening code has two
buckets and silently sorts a CV that does not mention something into the
wrong one. Three buckets — met, missed, not evidenced — is the difference
between a question for the first call and a rejection nobody reviewed.

**A parse failure must never be a rejection.** The one thing this desk
refuses to do is screen an application it cannot read. The run fails, the
failure clusters in the record, the loop surfaces the cluster, and a person
decides what to do about the intake channel.

**The criteria are memory, not code.** They are Facts in `org.talent.reqs`,
delivered to the check node through the trigger's declared context query. A
different requisition screens differently with no code change — and the
criteria a decision was made under stay queryable long afterwards, which is
exactly what a candidate exercising a GDPR Article 15 right would be asking
for.

## Going live

The mock connector and tools are the only fake parts. To make it real:

1. **Intake** — replace the `connector` subcommand with a poller against
   your ATS (Greenhouse, Workday, Lever, an S3 drop, a mailbox). Same
   contract as every other Areev connector: stdin `{trigger, connector,
   scope, cursor, max_items, config}`, stdout `{items, cursor, more}` —
   [`docs/triggers.md`](../../../docs/triggers.md).
2. **Parsing** — replace the `parse_application` handler with a real CV
   parser. Keep its refusal: no text layer, no screening. Consider routing
   the refusals to a human transcription queue rather than retrying.
3. **The review UI** — the parked runs are already a queue
   (`areev run list` / `GET /api/run/*` / the console's Runs tab). Give
   recruiters per-principal credentials (`areev ui --auth`), where
   `run.respond` refuses shared-token and anonymous callers outright,
   because the approver's identity *is* the audit record.
4. **The record you will actually be asked for** — `areev run
   oversight-report --plan <HASH>` on a schedule, filed with the rest of
   your Article 14 evidence; `areev run verify` in the same pass;
   `areev subject-report` and `FORGET SUBJECT` for candidate DSARs
   ([`docs/gdpr.md`](../../../docs/gdpr.md),
   [`docs/erasure.md`](../../../docs/erasure.md)).
5. **Retention is a policy, not a habit** — California FEHA wants ADS
   records kept **4 years** (vendors included), Colorado ADMT **3 years**.
   Declare it: `areev retention set --days 1460 --ns org.talent`, and note
   that erasure of a specific candidate (`FORGET SUBJECT`) is a separate,
   authorization-gated act with its own audit record.
6. **The part Areev does not do** — the NYC LL144 bias audit, Title VII
   adverse-impact analysis under the Uniform Guidelines (29 C.F.R. Part
   1607), candidate notice, and the Annex IV technical documentation are
   all yours. Areev's contribution is that the underlying record is
   complete, immutable, and queryable while you do them.

## The pieces

```
python/agent.py               the whole agent, one file, embedded Areev
smoke.sh, improve.sh          the two acts -- language-neutral assertions,
                              driven through the stack's 3-line wrapper
fixtures/requisition.json     the criteria, published with the posting
fixtures/applications/        eleven synthetic applications ("APPS_UPTO" is the clock)
fixtures/decisions/           recruiter decisions, incl. the two refusals
out/gate-audit.json           outcomes vs. humans, counted from the journal
```

Every candidate, name, requisition and certification here is invented.

## Where to go next

- [`../../../docs/eu-ai-act.md`](../../../docs/eu-ai-act.md) — the
  article → capability → command map this example is one row of
- [`../../../docs/run.md`](../../../docs/run.md) — the runtime: journal,
  budgets, asks, cancel, `verify`, `oversight-report`
- [`../../../docs/triggers.md`](../../../docs/triggers.md) — the trigger
  model and the connector contract
- [`../../../docs/security-model.md`](../../../docs/security-model.md) —
  the approval ladder: which proven identities may answer an ask
- [`../invoice-to-accounting/`](../invoice-to-accounting/) — the same
  machinery on a different job, in three languages
