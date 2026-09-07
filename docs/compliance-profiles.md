# Compliance profiles — host-config presets

Three deployment presets — **GDPR**, **healthcare**, **financial** — written as
the exact commands that configure them. Nothing here is a new feature; every
flag already exists. The page exists because assembling the right ones from
five reference docs, under time pressure, is where deployments go wrong.

A preset is a starting posture, not a certification —
[what none of them gives you](#what-no-preset-gives-you) is the last section.

---

## First: which controls survive a copy

Two kinds of control, and mixing them up is the failure this page exists to
prevent. An auditor asking "does this still hold on the replica?" is asking
which column a control is in.

| | **File-truth** | **Host config** |
|---|---|---|
| Lives in | the memory's `meta` table | the process |
| Travels with a copy, sync, restore | **yes** | **no** |
| Examples | `anonymize set`, `retention set`, `retention floor`, `hold set`, saved queries, triggers | `--anonymize-egress`, `--anonymize-cmd`, `--no-destructive-ops`, `--read-only`, `--passphrase-env`, `--anon-key-env`, `--allow-host` |

The rule that follows: **anything you must still be true after the file is
copied has to be a file-truth.** A host floor is a cap for *this* process — it
narrows what a policy-less namespace can leak here, and it is forgotten on the
next open. Both are useful; only one is a guarantee.

- A **detector chain** is a file-truth (`detectors: ["tier0","ner"]` in the
  policy), but the **detector** is a host capability
  (`--anonymize-cmd`). A host that cannot honour the declared chain fails the
  read closed with `VAL-E001`. That is deliberate: serving raw text because the
  NER model was missing would be the wrong failure.
- An **unknown policy field is refused**, not ignored. The field a newer build
  added could be the one that *strengthens* the policy.

---

## GDPR

For a deployment processing personal data of people in the EU/EEA.
[`gdpr.md`](gdpr.md) is the article→capability map; this is the configuration.

```bash
# 1. Storage limitation (Art. 5(1)(e)) — declare, then enforce separately.
areev retention set   --db memory.db --ns support --days 90 --type event \
  --because "support transcripts age out at 90d"
areev retention sweep --db memory.db --yes          # from cron; audited

# 2. Pseudonymize on the way to any model or third party (Art. 32).
areev anonymize set --db memory.db --ns support \
  --policy '{"mode":"egress","scope":"session"}'

# 3. Encryption at rest.
areev … --passphrase-env AREEV_PASSPHRASE

# 4. Attributable console access; approvals name a person.
areev auth mint --auth creds.json --id alice-laptop --principal user:alice --expires 90d
areev ui --db memory.db --auth creds.json
```

Why each line:

- **`retention set` is declarative and `sweep` is the act.** Declaring never
  deletes. The policy travels with the memory, so a restore on another host
  still says what the data's lifetime is — which is the thing a DPIA asserts.
- **`scope: session`** makes a pseudonym stable across one session's reads, so
  a model can reason about "the same person" without ever seeing who. `context`
  (the default) renumbers per call; `memory` is stable for the life of the
  memory and **requires key material** (below).
- **`--auth`, not `--token-env`.** A shared token is implied-admin and
  unattributable, and it can never answer a human-in-the-loop approval. Per
  principal credentials mean a write or an approval names *who*, one credential
  can be revoked without disturbing the principal's others, and `expires_at`
  bounds a leak.

The subject-rights verbs are on-demand rather than configured:

```bash
areev subject-report  "user:pat" --db memory.db                          # Art. 15 / 20
areev forget-subject  "user:pat" --db memory.db --text-mentions --yes   # Art. 17
```

They share one selector, so a disclosure reports exactly what an erasure
removes. Audit records name a subject **fingerprint**, never the identity — an
immutable, replicating grain naming the erased subject would undo the erasure
it records.

**Also required, and not a flag:** one memory per trust domain, a
TLS-terminating proxy for anything non-loopback, and a documented
archive-retention window. Those are [`gdpr.md` §2](gdpr.md), and a deployment
that skips one has a finding waiting.

---

## Healthcare

For clinical text — the profile where a name *beside a condition* is the
problem, not the name.

```bash
# Key material first: scope "memory" and the mapping vault are keyed from it.
areev … --anon-key-env AREEV_ANON_KEY        # 64 hex chars, from a KMS
#   (on the file backend --passphrase-env's page key also works)

areev anonymize set --db clinic.db --ns org.clinic.referrals \
  --policy-file clinic-policy.json

# Host floor: covers namespaces that have no policy of their own, this process
# only. A cap, never a substitute for declaring the policy.
areev … --anonymize-egress
```

`clinic-policy.json`:

```json
{
  "mode": "egress",
  "scope": "session",
  "default_action": "pseudonym",
  "categories": {
    "person": "allow",
    "condition": "allow",
    "phi": "pseudonym",
    "mrn": "redact",
    "date": "pseudonym",
    "phone": "pseudonym",
    "email": "pseudonym"
  },
  "term_sets": { "condition": ["type 2 diabetes", "hypertension"] },
  "co_occurrence": [
    { "when": "person", "near": "condition", "within_chars": 120, "as_category": "phi" }
  ],
  "detectors": ["tier0"],
  "because": "PHI leaves the clinic pseudonymized"
}
```

**Read this before copying it: where `person` comes from.** Tier-0 has no name
model. It detects a person from the identities the memory already holds as
grain **subjects**, from the policy's own `known` list, or from a Tier-1 NER
detector you install. A name appearing only in prose is detected by none of
those, and every rule below that keys on `person` is silent for it:

```console
$ echo "Marion Delacroix has type 2 diabetes." | areev anonymize scan --policy-file clinic-policy.json
{
  "text": "Marion Delacroix has type 2 diabetes.\n",
  "detections": []
}
```

Nothing: not a grain subject, not in `known`. Add the identity and the rule
fires, escalated to `phi` by the co-occurrence:

```console
$ # with "known": [{"value": "Marion Delacroix", "category": "person"}]
$ echo "Marion Delacroix has type 2 diabetes." | areev anonymize scan --policy-file clinic-policy.json
{
  "text": "Marion Delacroix has type 2 diabetes.\n",
  "detections": [
    { "start": 0, "end": 16, "category": "phi", "confidence": 1.0, "detector": "tier0.policy_known" }
  ]
}

$ echo "Marion Delacroix called about parking." | areev anonymize scan --policy-file clinic-policy.json
{
  "text": "Marion Delacroix called about parking.\n",
  "detections": []
}
```

(One detection abridged onto a line; the binary prints a field per line.) The
last one is empty because no condition is nearby and `"person"` is `allow`.

Why this shape:

- **`co_occurrence` is the rule per-category actions cannot express.** A name
  alone may be acceptable in a prompt; a name *together with* a condition,
  medication or procedure is health data. That is a property of the pair, not
  of either detection. It is written as a re-categorization rather than an
  action override so the output explains itself — the reader sees `[PHI_1]`,
  not `[PERSON_1]`, and can tell *why* the span was treated more strictly.
- **`"person": "allow"` is what makes the pair rule meaningful**, and is also
  the line to reconsider first. It says a bare name may pass; only a name near
  a condition is escalated. If your setting cannot allow a bare name, set it to
  `pseudonym` — but note that this still only covers names the detector can
  see, per the box above.
- **`mrn` is `redact`, not `pseudonym`.** A stable pseudonym for a record
  number is still a per-patient key.
- **Add `"detectors": ["tier0","ner"]` only with `--anonymize-cmd` installed.**
  Declaring a chain the host cannot serve fails the read closed (`VAL-E001`).
- **`"vault": true` needs `scope` `session`/`memory` and key material.** It
  persists the mapping to the file's sealed vault so a pseudonym can be
  reversed later by someone entitled to; set `vault_ttl_days` alongside it, or
  the mapping outlives the reason for keeping it.
- **Declare the policy on the clinical namespace only.** A fact's `subject` is
  an identity field by construction, so a policy on the namespace holding your
  *own operational rules* rewrites every rule into `[PERSON_n]` and the agent
  stops finding its own protocol. Policy, writes and erasure take exact
  namespaces; only reads accept a `"org.clinic.*"` prefix.

### Verifying it

The worked example is [`examples/agents/clinical-referrals`](../examples/agents/clinical-referrals),
and its `smoke.sh` step 5 is the assertion to copy. It captures the verbatim
bytes an outside service received (`out/egress.jsonl`) and, for **every fixture
that actually went out**, checks the patient name, date of birth, MRN, phone,
email and referring clinician against those bytes — derived from the fixtures,
never hardcoded, so adding a fixture strengthens the check instead of slipping
past it:

```bash
cd examples/agents/clinical-referrals && ./smoke.sh
#   5. what the outside service actually received
#      3 referrals audited: REF-2201, REF-2202, REF-2203
#      0 names, 0 dates of birth, 0 MRNs, 0 phone numbers, 0 emails
```

It also asserts the limit from the box above rather than hiding it: a relative
named once in prose **is** in the wire log, because Tier-0 saw no identity
there. That assertion is deliberate — read it before treating Tier-0 as a name
scrubber.

> Pseudonymization is not anonymisation. Reversible pseudonymized data is still
> personal data (GDPR Recital 26 / Art. 4(5)).

---

## Financial

For records that must be **kept** for a stated period, and destroyed only by
stated policy — the inverse of the GDPR default.

```bash
# 1. A floor: destruction younger than this is refused, whatever a sweep says.
areev retention floor --db ledger.db --ns accounting --min-days 2555 \
  --because "7-year records retention"
areev retention floors --db ledger.db

# 2. Legal hold: suspends ALL age-based destruction on a namespace. Both ends
#    take a mandatory --because and land in `areev audit export`.
areev hold set     --db ledger.db --ns accounting --because "litigation 2026-114"
areev hold list    --db ledger.db
areev hold release --db ledger.db --ns accounting --because "matter closed"

# 3. Approvals name a person, and cannot be the person who asked.
areev ui --db ledger.db --auth creds.json          # never --token-env here

# 4. A read-only console/reporting role holds no write authority.
areev ui --db ledger.db --read-only --auth readers.json

# 5. Cap destruction for any surface that should not have it at all.
areev serve --mcp --db ledger.db --no-destructive-ops
```

Why each line:

- **A floor outranks a policy.** `retention set --days 30` under this floor
  does not quietly win — the sweep skips it, on the record:

  ```
  accounting: SKIPPED — VAL-E001: validation error: cutoff is younger than the
  2555-day retention floor on 'accounting' (7-year records retention) —
  destruction refused
  ```

- **A hold refuses rather than skips silently.** While a hold is live, age-based
  destruction on that namespace refuses with the hold's reason and the principal
  who placed it. The hold row is deleted on release, so both transitions are
  also written to the Tier-2 trail — releasing is the act an auditor asks
  about, and it carries the same mandatory `--because` as placing.
- **`--no-destructive-ops` is a process-wide cap over any grant.** It sits above
  the file's own `mg:permits` grants, so it narrows even a principal the file
  authorizes. It covers `cal`, `repl`, `serve --mcp`, `ui`, `forget-subject`,
  `purge-older-than` and `retention sweep` — the last two being the age-based
  destruction this profile's floor and hold exist to govern.
- **Approvals: `--auth`, and never a shared token.** `run.respond` accepts an
  identity in proportion to how it was proven — a per-principal credential or
  native OIDC may approve; a proxy-asserted SSO identity may not unless the
  operator passes `--sso-approvals allow`; a **group-derived** principal may
  never approve, under any flag, because a role identifies nobody who can be
  asked why. Separation of duties is structural: the responder cannot be the
  principal who started the run.

Outbound calls, when a tool must reach a payment or ledger API:

```bash
areev run start … \
  --credential stripe=STRIPE_KEY@user:treasury \
  --allow-host https://api.stripe.com \
  --tool-egress refund:stripe@api.stripe.com:POST
```

The tool receives the broker's address and a capability token, **never the
secret**. `CRED@HOST` pins a credential to the one hostname it may be sent to,
so a tool holding two services' secrets cannot send one to the other; a grant
naming no method may only read. Prefer a minted credential
(`NAME=cmd:…` or `NAME=vault:PATH#FIELD`) over a long-lived environment
variable — it is re-minted on a TTL and refused rather than sent
unauthenticated if the resolver fails.

---

## Gate the policy in CI

A fixture file is the first artifact an auditor asks for, and the
`must_not_redact` half is the load-bearing one — a policy that redacts
everything passes `must_redact` trivially:

```bash
areev anonymize test --fixtures policy-fixtures.json
# → 5 fixtures: 5 passed, 0 failed (0 missed, 0 false positive)
# non-zero on any miss or false positive

areev audit export --db memory.db --since <ms> --out audit.jsonl
```

Fixture format and worked negatives:
[`cookbook.md`](cookbook.md#gate-the-policy-in-ci).

---

## What no preset gives you

- **A certification.** These are configurations. Your obligations depend on
  your data, your jurisdiction and your processing, and no flag decides them.
- **Coverage of unstructured references.** Erasure and access reach what the
  *indexes* reach. An identifier buried in a free-text blob with no structured
  reference is neither reportable nor erasable; `--text-mentions` widens the
  reach, it does not replace keeping identity references in structured fields.
- **Retroactive effect.** `anonymize set` transforms reads from the moment it
  is declared. Text already sent to a model is already sent.
- **Protection against a host that ignores the file.** Host floors and caps are
  per-process. A different operator opening the same memory gets the
  file-truths and none of the host config — which is exactly why the table at
  the top of this page matters.

## See also

[`gdpr.md`](gdpr.md) · [`eu-ai-act.md`](eu-ai-act.md) ·
[`erasure.md`](erasure.md) · [`security-model.md`](security-model.md) ·
[`deployment-profile.md`](deployment-profile.md) · [`cookbook.md`](cookbook.md)
