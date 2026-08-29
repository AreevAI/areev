# clinical referrals

**The problem.** A referral desk cannot do its job without an outside
service — coding, triage, increasingly a model — and it cannot use one
without disclosing patient records to it. Hand-redacting every letter does
not scale, and privacy logic written into each integration is the version
that drifts: one missed MRN in one tool is a reportable breach.

**What you get.** One policy declared on one namespace, and every
model-facing read of it leaves as typed placeholders — with no privacy code
in any tool. The acts prove it the hard way: three referrals triaged, and
the verbatim wire log checked byte-by-byte against every identifier in
every fixture — **zero on the wire** — while the clinician's letter comes
back fully rehydrated. And the desk gets better at the job: a correction a
clinician signs in week one becomes the clinic's own triage rule, which
fires in week two without waking anyone.

The scene: the referral desk of a specialist chest-pain clinic. GP practices send
referral letters; the desk files them, asks an outside clinical-coding
service for a code and a triage suggestion, and parks every referral for a
clinician to accept or redirect — and **what it sends to that outside
service carries no patient identifier at all**, while the clinic's own copy
stays fully identified.

That last clause is why this example exists.

```
 GP practice                    the areev referral desk                        clinic
───────────►  inbox ──intake──►  the memory  ──recall──►  extract ─► code_lookup ─► triage ─► clinician_review ─┬─► accept
  referral    (polling           identified                  │            │                    [ a person ]     │
   letter      trigger)          at rest                     ▼            ▼                                     └─► redirect
                                                        no DOB/MRN?   the outside
                                                        FAILED,       (a clinical coding
                                                        loudly         + triage service)
```

The boundary, which is the whole point:

```
        what the file holds                     what leaves the file
  ┌──────────────────────────────┐        ┌──────────────────────────────┐
  │ Marion Delacroix-Bell        │        │ [PERSON_1]                   │
  │ DOB 1971-04-02               │  ────► │ DOB [DATE_1]                 │
  │ MRN 4471902                  │        │ MRN [MRN_1]                  │
  │ 202-555-0142                 │        │ [PHONE_1]                    │
  │ marion.delacroix@example.com │        │ [EMAIL_1]                    │
  └──────────────────────────────┘        └──────────────────────────────┘
             ▲                                          │
             │        db.set_anon_policy(               │  the mapping never
             │          "org.clinic.referrals",         │  leaves the process
             │          '{"mode":"egress"}')            ▼
             └────────── rehydrate_text() ◄──── the clinician's letter
```

Nothing here rewrites text on the way out. One policy is declared once, and
after that **every model-facing read of that namespace** — `recall`, CAL,
the context a trigger assembles for a run, the run journal that records it —
comes back pseudonymized. Writes are untouched: `egress` rewrites reads.

And the second loop, the one that makes it self-improving:

```
 runs, corrections, tool calls ──► areev loop (deterministic analyzers)
        (the journal)                     │ Recommendation + evidence, by hash
                                          ▼
                                    [ a person ] ──approve, with a reason──►  new rules
                                          ▲                                   in org.clinic.protocol
   next run's context ◄── saved CAL queries assemble the lessons back ◄────────┘
```

Nothing needs a credential, a network, or a model key: the whole thing runs
from committed fixtures.

## Run it

| Stack | Needs | Run |
|---|---|---|
| [`python/`](python/) | `pip install areev` | `python/smoke.sh` then `python/improve.sh` |

Or everything at once (what CI runs): [`../run-smokes.sh`](../run-smokes.sh).

A few seconds later:

```
OK -- 3 referrals triaged, 3 outbound requests with 0 identifiers,
     1 self-signature refused, 1 correction signed, 1 letter rehydrated.
OK -- 1 signed correction became a rule, 3 incomplete referrals refused,
     1 loop finding approved with a reason, 1 detector chain extended.
```

## Week one — `smoke.sh`

Three referral letters arrive:

| Referral | What it is | What the run does |
|---|---|---|
| REF-2201 / exertional chest tightness, six weeks | the clinic's core presentation | The coding service suggests **routine**. A clinician **corrects it to urgent** and writes why — and that reason becomes a rule in `org.clinic.protocol`. |
| REF-2202 / palpitations at rest | in scope, unremarkable | Parks, accepted as routine. Nobody has to change anything. |
| REF-2203 / incidental asymptomatic murmur | wrong service | Parks, **redirected** to general cardiology with a written reason. |

Twelve properties are asserted, not narrated. The load-bearing ones:

- **The wire log holds no identifier.** `out/egress.jsonl` is the verbatim
  exchange with the outside service. The act script walks *every fixture
  that actually went out* and checks its patient name, date of birth, MRN,
  phone, email and referring GP against those bytes. A new fixture cannot
  quietly weaken the check.
- **The same memory still resolves them.** `reveal_tokens` — admin-gated,
  Tier-2 audited — hands the clinician back the real values, and the audit
  Observation it writes names subject *fingerprints*, never the identity.
  (An immutable grain naming the person it un-masked would defeat the point.)
- **The desk cannot sign its own triage.** The runtime refuses the principal
  that started the run (`RUN-E012`). Separation of duties is structural, not
  a policy document.
- **The clinician's letter is rehydrated in-process.** `rehydrate_text` puts
  six values back from the mapping this handle is holding; nothing was
  looked up remotely, because the mapping never travelled.
- **The operational namespaces have no policy, and step 10 shows why** — see
  below.
- **The host floor is a cap, not a policy.**
  `set_anonymize_egress_floor(True)` covers every namespace *without* a
  declared policy and can never weaken one that has one — and reopening the
  file forgets it. A cap you can forget to set is not a policy.

## Week two — `improve.sh`

Six more letters. Three of them are from one practice whose referral
template has no date-of-birth field.

| Referral | What it is | What the run does |
|---|---|---|
| REF-2204 / exertional chest tightness again | the same complaint as week one | **Proposed urgent, by the clinic's own rule.** The coding service still says routine; the clinic overrules it without waking anyone. The clinician confirms, and corrects nothing. |
| REF-2205, REF-2206, REF-2207 / Bramblewood, no DOB | missing a required identifier | **Fail, loudly.** Nothing triaged, nothing sent. A referral triaged without an identifier is the expensive failure; one stopped at the desk is the cheap one. |
| REF-2208 / exertional chest tightness, patient attends with her daughter | complete — and carrying a third party | Triaged and accepted. **And her daughter's name went out**, because Tier-0 could not see it. |
| REF-2209 / palpitations after caffeine | in scope, routine | Accepted routine. |

Then the desk reads its own record:

- **It briefs itself without touching patient data.** `desk_pulse` — a saved
  CAL query stored *in the memory file* — assembles the plan, the tool
  definitions, recent activity and the protocol under a token budget. The
  act script asserts no patient identifier appears in it, which is true for
  a structural reason: the briefing query never reads the clinical
  namespace.
- **`areev loop run` finds the cluster**: `HIGH — Workflow e9cb3f56c7b8
  failed 3/9 recent runs (33%): extract: … REF-2205 is missing required
  identifier(s): date`. Deterministic analyzers; no model key involved.
- **The gate holds**: the engine refuses to apply its own advisory finding,
  and refuses any decision with no written reason. A clinician approves it,
  signing name and reasoning — and the reasoning is *don't relax the check*.
- **The honest part.** The act script then asserts that `Anneke Vos` **is**
  in the wire log. She is not a date, a phone or an email, and the memory
  has never interned her as a subject, so nothing was there to detect her.
  Tier-0 is a floor.
- **Extending the floor.** The policy is tightened to
  `detectors: ["tier0","ner"]` with the reason recorded in the policy
  itself. The very next read **fails closed** (`VAL-E001`) because this host
  has no Tier-1 backend — the policy is a property of the *file* and travels
  with it; the detector is a capability of the *host* and does not. Install
  one with `set_anonymizer_command` and the relative becomes `[PERSON_2]`
  too — same policy, better chain. The identified record never moved.

## What this example is teaching

**Pseudonymization is a property of the namespace, not of your code.** The
desk's tools contain no anonymization logic whatsoever. They read the run's
context and act on it. The context arrived pseudonymized because the trigger's
declared CAL query read a namespace with a policy, and the store rewrote it at
the read exit. Change the policy and every surface changes with it: `recall`,
CAL, MCP, the console, the run journal.

**`egress` rewrites reads, never writes.** The clinic's system of record is
inside the trust boundary and stores the truth. That is what makes
`reveal_tokens` and `rehydrate_text` possible, and it is what makes a DSAR
answerable. There is no `"rewrite"` mode, whatever some prose calls it — the
five valid modes are `off`, `egress`, `ingress`, `both`, `audit`.
[`invoice-to-accounting`](../invoice-to-accounting/) uses `audit`: count the
detections, change nothing. This one rewrites.

**The run journal never held an identifier in the first place.** The journal
lives in `org.ops`, and what it recorded was already pseudonymized on its way
out of the clinical namespace. So the review queue, the run trace, the
`step_actions` history and the loop's evidence are all safe to read, forward
and archive — without a second policy anywhere.

**The operational namespaces must NEVER get an anonymization policy.** This
is the lesson [`invoice-to-accounting`](../invoice-to-accounting/) states and
this example *demonstrates* (`agent.py policy-drill`, asserted in step 10 of
`smoke.sh`). An egress rewriter is for what LEAVES. `org.clinic.protocol`
does not leave — the desk reads it back as **input** on every run. Point a
rewriter at it and:

```
  triage  mg:complaint_term  chest tightness on exertion
    ↓ with a policy on the namespace
  [PERSON_2]  mg:complaint_term  [PERSON_1]
```

Every fact's `subject` is an identity field *by construction*, so it becomes
`[PERSON_n]` unconditionally; the protocol version `2026-07-01` becomes
`[DATE_1]`; and because `triage` is a known identity in that namespace it is
substituted even *inside the relation name* — `mg:[PERSON_2]_urgency`. The
desk would not find a single one of its own rules. The same argument covers
`org.ops`, where the plan hash, the tool bindings, the trigger cursors and
the run journal live. Keep the policy on the namespaces that hold people.

The drill also proves the other half: the **file** is never harmed. Same
grains, same content addresses, before, during and after — `egress` rewrote a
read, not a byte on disk.

**Namespaces, and what each one is for:**

| Namespace | Holds | Policy |
|---|---|---|
| `org.clinic.referrals` | patients and referring GPs: name, DOB, MRN, phone, email, the letter | **`{"mode":"egress","scope":"session"}`** |
| `org.clinic.protocol` | the clinic's own triage rules, including the ones clinicians wrote | none — read back as input |
| `org.ops` | the plan, the tool definitions, the trigger, the run journal | none — never needed one |

`scope: "session"` keeps a token stable for the life of a handle, so a
mapping a read hands back still resolves tokens an earlier read in the same
process produced. (`context`, the default, renumbers per call; `memory`
derives tokens from the value itself and needs an encrypted memory.)

**Three seams, one shape** — JSON on stdin, JSON on stdout, one process per
invocation, none of them ever opening the memory:

| Leg | Subcommand | Contract |
|---|---|---|
| inbound | `agent.py connector` | [`docs/triggers.md`](../../../docs/triggers.md) |
| work | `agent.py tools` | [`docs/run.md`](../../../docs/run.md) |
| the outside | `agent.py service` | the same host-tool shape — it just runs on the other side of the boundary |
| Tier-1 detection | `agent.py ner` | `{"areev_anonymize":1,"op":"probe"\|"detect"}` |

## Pseudonymization is not anonymisation

Say this out loud before you ship anything built on this example.

**A pseudonym is still personal data.** The whole design goal here is that
the mapping is *recoverable* — that is what `rehydrate_text` and
`reveal_tokens` are for, and it is why the clinician gets a usable letter.
Under GDPR (Recital 26 and Article 4(5)) reversible pseudonymized data
remains personal data, and under HIPAA a re-identification key held by the
covered entity keeps the data identifiable. Pseudonymization reduces the blast
radius of a disclosure; it does not take you outside the regulation. If you
need de-identification, you need the mapping to not exist — `mask` and
`redact` are the one-way actions, and `generalize:month|year|decade` coarsens
instead of replacing.

**A placeholder is not the only identifier in a letter.** This example leaves
the referring practice name (`Willowbrook Surgery`) in the outbound text on
purpose: it is an organization, not one of the Tier-0 categories, and whether
it is a re-identification risk depends on how rare it is in your population.
Rare diagnoses, rare postcodes, an admission date plus a hospital — these are
quasi-identifiers no detector catches for you. The policy has the levers
(`custom_terms`, `term_sets`, `co_occurrence` to escalate a person seen near
a condition, `generalize:*` to coarsen a date) but choosing them is a
judgment, not a default.

**Tier-0 is a floor you extend, and the example proves it by failing.**
Tier-0 detects checksummed or cue-gated *shapes* — dates, phones, emails,
IBANs, card numbers, MRNs, NRIC/EID, secrets — plus identities the memory
already holds as grain subjects. It has no name model. A relative named once
in prose walks straight past it, which is exactly what `improve.sh` step 9
asserts. `set_anonymizer_command` is the Tier-1 NER seam (JSON on stdio, one
spawn per call, probed at install); a policy that lists `"ner"` in
`detectors` **fails the read closed** on a host that has not installed one.
The stand-in in `agent.py ner` is a deliberately crude regex so the keyless
floor stays keyless — in production that is a model.

**Detection is not the only control.** The gate below it still matters: who
may read the namespace at all, who may call `reveal_tokens`, and whether the
audit trail is somewhere the reader cannot edit.

## Fixtures — what each one exercises

Entirely synthetic. Every patient, clinician, practice and identifier is
invented; phone numbers are in the reserved `555-01xx` range, emails are
`example.com`, and the MRNs match no real numbering scheme. Nothing here
could be mistaken for real PHI.

| Fixture | Exercises |
|---|---|
| `referrals/01-delacroix-bell.json` | the full Tier-0 sweep in one letter: person (via interned subject), date, MRN, phone, email — and the correction that becomes a rule |
| `referrals/02-okonkwo.json` | a second patient and a second referring GP, so pseudonym numbering is visibly per-identity and stable across a handle |
| `referrals/03-thanachart.json` | two complaint terms in one letter (the earlier one wins, deterministically) and the out-of-scope redirect route |
| `referrals/04-vasquez-rey.json` | the memory payoff: same complaint, the clinician's week-one rule applied with no clinician involved |
| `referrals/05-oyelaran.json`, `06-fitzwilliam.json`, `07-lindqvist.json` | a missing required identifier, three times from one practice — the failure cluster the loop finds |
| `referrals/08-nakamura-oyelowo.json` | the Tier-0 miss: a relative named once in prose, and the Tier-1 detector that catches her |
| `referrals/09-perreault.json` | an unremarkable routine acceptance — the control case |
| `reviews/00-desk-signs-its-own.json` | separation of duties: the starting principal is refused |
| `reviews/01-…-urgent.json` | a correction with a written reason (becomes `org.clinic.protocol`) |
| `reviews/03-…-redirect.json` | the redirect branch, signed by a different clinician |
| `reviews/04-…-confirm.json` | a confirmation that corrects nothing — the payoff, asserted |
| `protocol.json` | the clinic's own rules: required identifiers, in-scope complaints, the out-of-scope route |

## Going live

Nothing about the governance changes. Three things do:

1. **`agent.py connector`** becomes your document intake — a scanning
   mailbox, an NHS e-Referral feed, a DMS webhook. Same JSON-on-stdio
   contract ([`docs/triggers.md`](../../../docs/triggers.md)); your vendor
   SDK lives in *your* connector, never in this repo.
2. **`agent.py service`** becomes the coding or triage API you actually
   call. It already receives nothing but placeholders, so pointing it at a
   real endpoint does not change what you are disclosing. If that endpoint
   is an LLM, this is the same seam `--llm-cmd` uses ([`../../llm/`](../../llm/)).
3. **`agent.py ner`** becomes a real model. Keep the fail-closed behaviour:
   declaring `"ner"` in the policy and *not* installing a backend is the
   safe failure, and you want to find that out in staging.

Then the things this example deliberately does not do:

- **Encrypt the memory.** The file here is plaintext so the smoke stays
  dependency-free. A real clinical memory is opened with a passphrase; that
  is also what unlocks `scope: "memory"` (value-derived, stable tokens
  across processes) and the sealed vault (`vault: true`), which persists the
  mapping instead of holding it in RAM.
- **Authenticate the clinicians.** `run_respond` here takes a principal
  string. In deployment the approver's identity has to be *proven* — see the
  approval ladder in [`docs/auth-proposal.md`](../../../docs/auth-proposal.md)
  and [`docs/security-model.md`](../../../docs/security-model.md): a
  per-principal credential or OIDC may approve; a shared token or a group
  role may not.
- **Answer a DSAR.** `subject_report` and `forget_subject` are the read and
  the erasure over one identity, and they share one selector so a disclosure
  discloses exactly what an erasure removes
  ([`docs/erasure.md`](../../../docs/erasure.md),
  [`docs/gdpr.md`](../../../docs/gdpr.md)).

Reference: [`docs/security-model.md`](../../../docs/security-model.md) for the
anonymization surface, [`docs/run.md`](../../../docs/run.md) for the runtime,
[`docs/loop.md`](../../../docs/loop.md) for the improvement pass.
