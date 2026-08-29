# data-subject requests

**The problem.** Answering a GDPR request is easy to claim and hard to
prove. In most stacks the export and the deletion are separate code paths,
so nothing stops a desk disclosing one set of data and erasing another —
which is a compliance failure, not a rounding error. And the proof
machinery itself is a hazard: an audit log that names the person it erased
has just undone the erasure it records.

**What you get.** A desk where the disclosure and the erasure are one
selection — the acts assert report count == erasure count, namespace by
namespace, and refuse to erase at all if they ever diverge — an intake that
refuses to act for anyone who has not proven who they are, and a register
that proves every act with an approver, a written reason and a fingerprint,
never the person.

The desk itself: access, portability and erasure requests arrive; the agent
works out **who** is asking, prices the request against what is actually
stored, and parks for a Data Protection Officer — because an erasure is
irreversible and the approver's identity *is* the audit record. On approval
it discloses or erases, and writes down what it did **without** writing down
who it did it to.

Every other agent in this directory writes to memory. This one is about
taking things **out** of it, provably.

```
 data subject                the areev privacy desk                        DPO
──────────►  intake  ──────►  identify ─► build ────nothing on file───►  close
 "send me    (a case,         subject     report              ▲          (register
  my data"    logged)            │           │                │           entry)
  "delete     redacted           │ can't     │ something         ▲
   my data"   for the run        │ resolve   │ on file           │
                                 ▼           ▼                   │
                            FAILED,      dpo_review ─────► [ a person ]
                            loudly         (the gate)   "disclose | erase
                                                          + because: ..."
                                                                │
                                    ┌───────────────────────────┴──────┐
                                    ▼                                  ▼
                                  erase                          disclose_only
                                    └──────────► close ◄──────────────┘

  the run ORDERS; the driver EXECUTES, holding the memory:
     subject_report(ns) ──── must equal ────► forget_subject(ns)
                    └──► pack.json / bundle.mgb (Art. 15 / Art. 20)
```

And the second loop — the one that makes it *self-improving*. Every refused
request is already a grain in the desk's own journal, so improvement is
analyzers over its own record, decided by a person, and applied as **one
declared Fact** rather than a code change:

```
 run journals, tool calls ──► areev loop (deterministic analyzers)
                                     │  Recommendation + evidence, by hash
                                     ▼
                               [ a person ] ──approve, in writing──► one Fact in
                                     ▲                                org.privacy
   requests that failed ◄── the desk reads its rules out of memory ◄──────┘
      can now be re-run
```

Nothing here needs a credential, a network, or a model key: the whole thing
runs from committed fixtures. Every person, address, invoice and case
reference is fictional.

## Run it

| Stack | Needs | Run |
|---|---|---|
| [`python/`](python/) | `pip install areev` | `python/smoke.sh` then `python/improve.sh` |

A few seconds later:

```
OK -- 1 access, 1 portability, 1 erasure (report == erasure),
     1 unverified request refused, 2 approvals refused, 5 guards held.
OK -- 1 declared retention rule applied and 1 refused, 1 loop finding
     decided in writing, 1 rule declared, 3 revived requests answered.
```

## Week one — `smoke.sh`

Four requests arrive at a desk holding three namespaces of personal data
about eight fictional people.

| Step | What happens | What is asserted |
|---|---|---|
| 1 | Seed the plan, tool definitions, declared rules and a synthetic memory | 30 grains across `org.crm` / `org.support` / `org.billing` |
| 2 | Point every destructive surface at `"org.*"`, and at `""` | 5 refusals, all `VAL-E001` |
| 3 | Intake: access, erasure, portability, and one unverified sender | 3 parked, **1 refused before anything was read or removed** |
| 4 | The desk approves its own erasure | refused — `RUN-E012`, separation of duties |
| 5 | An erasure with no written reason | refused |
| 6 | The DPO discloses to Ines Bakker (Art. 15) | the pack **is** the report — same 6 grains, not a re-query |
| 7 | The DPO grants Nadia Okonkwo's erasure (Art. 17) | **report count == erasure count, per namespace**; the consent withdrawal joined the set; a different subject is untouched |
| 8 | The DPO grants Tomas Vetter's portability (Art. 20) | an MGB1 bundle, importable into any OMS store |
| 9 | Look for her afterwards | selector 0, structural 0, prose 0, journal 0; the certificate holds a **fingerprint**; no telemetry sidecar exists |
| 10 | `verify()` | every content address still verifies after the erasures |

## Weeks two and three — `improve.sh`

| Step | What happens | What is asserted |
|---|---|---|
| 1 | Three requests arrive naming only an email address; one asks whether last week's erasure happened | 3 refused, 1 closed **without troubling a human** |
| 2 | The erasure is still an erasure | report empty, nothing resurfaced |
| 3 | The declared retention rules run | `org.support` sweeps 3 grains past 365 days; the `org.*` rule is **refused**, not widened |
| 4 | `areev loop` reads the journals | ≥2 findings, one targeting `identify_subject` |
| 5 | Try to let the engine apply its own advice; try to decide with no reason | both refused |
| 6 | The DPO approves, in writing | the reason is on the record |
| 7 | The operator declares the approved rule — **one Fact** | the 3 dead requests now resolve and park; **the unverified one is still refused** |
| 8 | The DPO clears the backlog | 1 erasure (report == erasure), 2 disclosures, queue empty |
| 9 | The register | 6 certificates, each naming a case, an approver, a reason and a fingerprint — and no person |
| 10 | Run the loop again | deduped: the same evidence is not proposed twice |

## What each fixture exercises

| Fixture | Exercises |
|---|---|
| `fixtures/seed/subjects.json` | The synthetic memory: 8 people across 3 namespaces, facts, thread events and **consent** grains. `age_days` is what the retention sweep reads back, so the fixture — not the wall clock — decides what is old. It also carries the **processing register** (which namespaces hold personal data) and the **retention rules**, including one deliberately mis-declared as `org.*` |
| `requests/01-bakker-access.json` | Art. 15 access, DID claim, verified — the happy path |
| `requests/02-okonkwo-erasure.json` | Art. 17 erasure with an explicit consent withdrawal — the irreversible path |
| `requests/03-vetter-portability.json` | Art. 20 portability — produces an MGB1 bundle, not a JSON dump we invented |
| `requests/04-raman-unverified.json` | An erasure demanded by someone who never proved who they are. Art. 12(6): **verify, or do not act** |
| `requests/05..07-*.json` | Requests that name only an email address. The CRM keys on DIDs, so identity resolution fails — three times, which is what the loop finds |
| `requests/08-okonkwo-again.json` | A follow-up after the erasure: nothing on file, so it closes without a human |
| `decisions/00-desk-approves-itself.json` | Separation of duties — the principal that started the run may not answer its gate |
| `decisions/02-okonkwo-no-reason.json` | An irreversible act with no written reason |
| `decisions/01,03..07-*.json` | Real DPO decisions, each signed by name and carrying a reason |

## What this example is teaching

**1. Disclosure and erasure are ONE selection.** `subject_report(ns,
subject)` is the erasure selector in show-me mode — same partition-key
matching, same full supersession history. The desk measures with it, and at
execution time asserts that what the report disclosed is what
`forget_subject` removed, namespace by namespace, refusing to erase at all
if they ever diverge. A DSAR that discloses one set and deletes another is a
compliance failure, not a rounding error. (`docs/erasure.md`, REQ-ERASE-9.)

**2. A wildcard is a reading convention, never a destruction target.**
`"org.*"` is how you scope a *recall*. Pointing an erasure, a DSAR read or a
retention sweep at one is refused with `VAL-E001` — because a wildcard that
widened destruction would be indistinguishable from a typo. The desk asserts
this five ways in step 2, and again in the *declarative* path: a retention
rule someone wrote as `org.*` is refused at execution, and the correctly
declared rule beside it still runs.

**3. The record of an erasure must not be a copy of what was erased.**
`forget_subject` deliberately writes no audit grain of its own — an
immutable, replicating grain naming the subject would undo the erasure it
records. The host decides what to log, so this desk logs a **certificate**:
the case reference, the approver, the counts, the reason, and the subject as
a fingerprint (`sha256(identity)[:16]`, the shape
`areev_core::authz::subject_fingerprint` uses). Given a candidate identity
you can recompute it and verify the certificate concerns them; you cannot
read the person out of it, and you cannot enumerate the log.

**4. So the run journal never sees an identity either.** Run journals are
grains. If the desk had passed the request into the run, the person's name
would be sitting in `agent:harness` — outside every namespace an erasure is
scoped to, replicating happily. So `run_start` gets a **redacted intake
record**: case reference, request type, verification outcome, fingerprint,
and per-namespace counts. No sender, no name, no request body. `trace`
asserts it: zero journal mentions, after the fact.

**5. And no query log.** The recall-telemetry sidecar records query *text*,
and a privacy desk spends its day searching for people it is about to erase
— then again afterwards, to prove they are gone. Left on, those searches
leave the erased name in a sidecar the erasure has already run past, and the
loop's coverage-gap analyzer proposes *"recurring question with no matching
memory: <erased name>"* — writing the identity back into the memory as a
recommendation grain. This desk opens with `telemetry="off"`, and the smoke
asserts no sidecar exists.

**6. A consent withdrawal is itself personal data.** The withdrawal is
recorded before the erasure runs — and joins the set the erasure removes,
because it names the person. That is why the certificate reports one grain
*more* than intake priced; the act that authorised the erasure was in scope
for it. What survives is the fingerprinted certificate.

**7. Structured references are what erasure can find.** The scope contract
is dictionary-indexed references only. A Consent grain that names the person
only in `subject_did` is invisible to both the report and the erasure —
disclosed by neither, removed by neither. The seeder writes `subject` as
well, which is the indexed position (and the one `docs/gdpr.md` §6 recalls
on: `RECALL consents WHERE subject = "…"`).

**8. Verify, or do not act.** The desk refuses on two distinct grounds, and
keeps them distinct. The unverified request fails in week one; the
unresolvable ones fail in week two; the improvement in week three fixes
*resolution* — and the unverified request is **still refused**, which
`improve.sh` asserts. A desk that quietly conflated the two would be a desk
that erased people because someone knew their email address.

**9. The improvement is a Fact, not a deploy.** The desk reads its
processing register, its retention rules, its response deadline and its
identity-resolution rules out of `org.privacy`. The loop finds the failure
cluster; a DPO approves in writing; the operator declares one Fact; requests
that could not be answered are re-run and answered. No code changed.

**10. An irreversible act is not performed by a subprocess.** The runtime
holds the single writer, so a host tool can never open the memory — which is
the right architecture anyway. The `erase` node *orders* the erasure; the
driver executes it after the run returns, holding the memory, after a named
human approved it.

## Why there is no trigger here

Every other agent in this directory has a standing rule that starts runs.
This one does not, deliberately: screening every payment automatically is
the point of a screening desk, but starting an **erasure** run off an
unauthenticated mailbox is not. Intake is an explicit, logged act by the
controller — `agent.py intake`, which you would put behind whatever your
case-management system calls "accept this request". Everything downstream is
identical; `docs/triggers.md` is there when you want the polling leg.

## Going live

| Leg | Here | Live |
|---|---|---|
| Intake | `fixtures/requests/*.json` | your privacy@ mailbox / web form, parsed into the same shape. Keep the redaction: only the case reference, type, verification outcome and counts go into `run_start` |
| Identity verification | a `verification` block in the fixture | your account session, ID check, or challenge-reply. This is the field the desk refuses on |
| Identity resolution | the `mg:resolve_*` Facts in `org.privacy` | the same Facts, declaring which relations may match a claim in your schema |
| The processing register | Facts seeded from `fixtures/seed/subjects.json` | your Art. 30 record, as Facts. Every namespace listed is a destruction target |
| The DPO gate | `fixtures/decisions/*.json` | your case manager posting into `run_respond`. The responder must be a **named principal** — with `areev ui --auth`, a per-principal credential; never a shared token |
| The disclosure pack | `out/packs/*.json` and `*.mgb` | whatever you hand the data subject. The `.mgb` bundle is the Art. 20 artifact |
| The certificate | Facts in `agent:privacy` | the same. Keep the fingerprint; never the identity |

Two things to keep when you take this to production: **the count assertion**
in `execute_orders` (report == erasure, or stop), and **`telemetry="off"`**.
Both are one line, and both are load-bearing.

## The pieces

| File | What |
|---|---|
| [`python/agent.py`](python/agent.py) | The whole agent: the host-tool seam, the driver, the plan, the certificate |
| [`smoke.sh`](smoke.sh) / [`improve.sh`](improve.sh) | The act scripts — language-neutral, they hold every assertion |
| [`fixtures/`](fixtures/) | The synthetic memory, the requests, the DPO decisions |
| [`CLAUDE.md`](CLAUDE.md) | The working rules for changing anything here |

## Where to go next

- [`docs/erasure.md`](../../../docs/erasure.md) — the requirement record:
  REQ-ERASE-1..9, the scope contract, and why the audit names a fingerprint
- [`docs/gdpr.md`](../../../docs/gdpr.md) — the article → capability map
- [`docs/run.md`](../../../docs/run.md) — the plan, the journal, the gate
- [`docs/loop.md`](../../../docs/loop.md) — the analyzers and the four gates
- [`../invoice-to-accounting/`](../invoice-to-accounting/) — the same shape
  with an inbound connector and three language stacks
