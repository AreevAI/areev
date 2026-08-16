# Anonymization — prompt-safe context, PII-aware storage

**Status:** P0–P2 BUILT (2026-08-16, branch `anonymization-p0`). P2: ingress
mode (value-derived pseudonyms at `capture`/`remember`/`attach_facts`, the
facade's structured writes, and the memory-tool adapter — all before
serialization, D8), `memory` scope (HKDF-keyed tokens stable across
handles), REQ-ANON-7 (erasure and DSAR select under both the real name and
the recomputed pseudonym), the encrypted-memory requirement for
value-derived features (Q5 resolved conservatively), and the
`PseudonymizingBackend` LLM decorator wired into `areev remember`'s
extraction. Migrate importers stay exempt (historical data rides in raw;
declare the policy before importing if imports must transform — their
content-address dedup probe is incompatible with in-flight rewriting).
P0: the
`areev_core::anon` engine (Tier-0 chain, placeholder codec, keyed
`mapping_id`), the explicit `scan_text`/`anonymize_text`/`rehydrate_text`
APIs on the facade and both bindings, and `areev anonymize scan` — gated by
the golden detector corpus and the round-trip property test. P1: the
`anon:<ns>` policy rows (replicating write-if-absent, conformance-pinned),
the store read boundary over every grain-returning read (with the
documented exemptions below), gate-level known-identity propagation
(seeded from the file's subjects + fed by the write path),
`context`/`session` scopes with in-process mapping custody, the
fail-closed poisoned-row contract, the `--anonymize-egress` floor, `audit`
mode, payload flags on CAL/MCP, the `/api/config` block, `areev anonymize
set|list|clear|mappings`, and policy/mapping methods on both bindings —
latency gates re-run green. §11's P2–P5 remain proposal. Three
implementation truths recorded below where they diverge from the first
draft: `min_reader_version` warns loudly at open (it has never been a hard
refusal), P1 `audit` counters are in-memory on the handle (the sidecar
route comes later), and the DSAR/replay/authz reads are exempt until P3's
reveal grants exist.

Companions: [gdpr.md](gdpr.md) (esp. "Content addressing is not
anonymization"), [erasure.md](erasure.md) (REQ-ERASE-*),
[security-model.md](security-model.md),
[areev-governed-agents-proposal.md](areev-governed-agents-proposal.md) §7.5 +
D9 (the results-only, fail-closed redactor precedent),
[cal-all-you-need-proposal.md](cal-all-you-need-proposal.md) (authz grants,
Tier-2 audit).

---

## 1. What this is

When Areev assembles context, it can produce a **prompt-safe rendering**:
sensitive values are detected, replaced with typed placeholder tokens
(`[PERSON_1]`, `[EMAIL_2]`), and the placeholder↔value mapping is kept so
that when the model's reply comes back, the real values are restored before
the reply is shown, stored, or acted on. The external model works on
placeholders; the identities never leave the process. Optionally, the same
detection can run at **insertion**, so a memory never stores the sensitive
value at all.

Commercial anonymisation gateways sell this as a standalone proxy that sits
between an application and its LLM. Areev is better placed than a proxy to do
it, for one structural reason: a proxy sees an opaque string and must find
PII by pattern-matching prose, while Areev sits *inside* the memory engine
and already knows the schema — `subject` **is** an identity by construction,
every identity the memory has interned is known to the engine, and the
renderer is a single shared implementation.
Detection here is schema-aware first, regex second, model third.

Everything below follows the house rules: dependency-light (the built-in
tier is pure std + what we already ship; models arrive through command
seams, never as dependencies), host config never persisted in the file,
declarative policy travels with the file, destruction and re-identification
are authorization-gated and audited.

## 2. Decision record

| # | Decision |
|---|---|
| D1 *(new)* | **Two pipelines, one engine.** *Egress* pseudonymization (render/recall-time, reversible via a mapping) is the headline feature; *ingress* transforms (insertion-time, irreversible or vaulted) are the optional stricter mode. Both share one detector chain and one policy. |
| D2 *(new)* | **Detection is layered and pluggable.** Tier 0 (structural fields + regex + checksum validators + user dictionaries) ships in-tree with zero new dependencies. Tier 1 is an external NER detector over a command seam (`--anonymize-cmd`, modeled on `CommandEmbed`/`CommandAnalyzer`). Tier 2 is LLM-based via the existing `LlmBackend` trait, local-first. The engine ships no model. |
| D3 *(new)* | **Policy is a file-truth; detectors are host capabilities.** What to protect and how (`anon:<ns>` meta row) travels with the file and replicates write-if-absent, like `retention:<ns>`. *How to detect* (the installed backends) is per-process host config, never persisted, like the embedder. An unparseable policy row is a hard `VAL` error, not a skip — a policy this build cannot read must not silently mean "no policy". (This follows the `retention:` precedent — `retention_policies()` already hard-fails on an unparseable row — and deliberately diverges by prefix from the `qry:`/`tpl:` skip-and-warn contract; the bundle-import side keeps its conservative write-if-absent behavior either way.) |
| D4 *(new)* | **Placeholders are category-typed and context-scoped by default.** `[PERSON_1]` tells the model *what kind* of thing it is (utility) without *which* one (privacy). Numbering restarts per assembled context; the opt-in `session` and `memory` scopes give cross-prompt consistency via HMAC-keyed derivation, with the joinability risk stated (§3). |
| D5 *(new)* | **The mapping is ephemeral by default; the vault is opt-in; custody is per-surface.** By default the placeholder↔value map lives in process memory and dies with the session. It is returned only to in-process Rust/binding callers, who already hold the raw file; MCP and server payloads carry the `mapping_id` alone — on those surfaces the caller is the model harness §3 names as the egress channel, so rehydration there is a host-side, grant-gated call, never a model-callable echo of the mapping. The persistent vault is `vault:` meta rows — **excluded from `REPLICABLE_META_PREFIXES`**, sealed under a dedicated HKDF subkey (`areev.vault.v1`), and therefore only available on an encrypted memory (a plaintext file has no key material; vault persistence on one is a hard refusal, not a silent downgrade). |
| D6 *(new)* | **Egress fails closed.** If the effective policy requires anonymized egress and any detector in the chain fails (spawn error, timeout, garbled output), the render **fails**; raw content never leaves as a fallback. This follows the governed-agents redactor precedent (D9 there) and deliberately inverts `CommandAnalyzer`'s skip-on-failure posture, which is right for advisory analysis and wrong for a privacy gate. Advisory scan-only modes may skip. |
| D7 *(new)* | **No new CAL syntax in v1.** CAL syntax is an OMS conformance contract, and CAL's own forward-compatibility rule (unknown `WITH` options warn-and-skip, I-6) makes a per-query `WITH anonymize` structurally unsafe as the *gate* — on an older build it would silently return raw text. The file-declared policy is the gate; a future `WITH anonymize(...)` (spec-level proposal, §9) may only *strengthen* the effective policy, never weaken it. |
| D8 *(new)* | **Ingress transforms happen before serialization.** The content address must commit to the transformed text — anonymize-then-hash, never hash-then-mask. Transforming only the index projection (`projected_text` / BM25 / embeddings) is explicitly rejected: it stores the original in the blob, replicates it, and re-identifies on `rebuild_text_index()`. It is masking, not anonymization, and we will not ship it under this feature's name. Ingress pseudonyms must additionally be **value-derived** (`memory`-scope), never counter-based: add-idempotency and `cal_add_if_novel` dedup survive only if the same raw text always transforms identically, and erasure stays addressable only if `FORGET SUBJECT` can recompute the stored pseudonym from the real identity (REQ-ANON-7). |
| D9 *(new)* | **Re-identification is a privileged, audited act.** Reading the persistent vault (`reveal`) requires an authz grant and writes a Tier-2 audit Observation in `agent:authz` naming the *subject fingerprint*, never the identity — same rule as erasure audits. Ephemeral in-process rehydration by the caller that created the mapping is not gated (they already hold both sides). |
| D10 *(new)* | **We claim pseudonymization, not anonymity.** Robust de-anonymization of sparse data (Narayanan & Shmatikov 2008, [arXiv:cs/0610105](https://arxiv.org/abs/cs/0610105)) shows identifier removal does not anonymize rich behavioral data. The docs, the console, and the API vocabulary say *pseudonymize*; the threat model (§3) states what remains linkable; no k-anonymity or differential-privacy claim is made anywhere. |
| D11 *(new)* | **The round trip is keyed, and deterministic where determinism is honest.** One detection pass feeds every format rendered from one collected result set, all sharing one mapping. `mapping_id` is a truncated **HMAC** under the session/vault key over (canonicalized policy, scope, sorted placeholder→value pairs) — never a bare digest, which would hand the egress channel an offline-guessing oracle for low-entropy values like a 4-digit PIN. Under `context`/`session` scope, tokens are *not* reproducible across re-assemblies (recall is deadline-bounded and budget-truncated; the id names one exact mapping, and rehydrating against the wrong one fails loudly); cross-assembly reproducibility comes from `scope: memory` (value-derived tokens) or the vault. Rehydration is exact-token and never guesses. Anonymized and original renderings may be produced side by side; when a policy is active, the original arm is D9's gated reveal, not a second pipeline. |

## 3. Threat model, stated honestly

**What this defends against.** The adversary is the egress channel: the
external model provider, the routing/logging layers between the process and
the model, and retention of prompt logs. With egress pseudonymization on,
that channel sees typed placeholders instead of names, emails, phone
numbers, card numbers, secrets, and any custom-dictionary terms — for every
rendered format on every surface, because the hook is below the renderer
(§4.3).

**What it cannot defend against.** Four ceilings, each of which the feature
must state rather than hide:

1. **Linkage attacks over rich content.** The Netflix-prize result:
   with 8 movie ratings (two of them wrong) and dates known to ±14 days,
   99% of subscribers were uniquely identifiable; 6 obscure ratings alone
   identified 84%. The lesson generalizes: a memory's *combination* of
   facts is sparse and distinctive. A fully pseudonymized context that says
   `[PERSON_1]` prefers X, met [PERSON_2] on a date, and complained about Y
   can still be linked to a real person by anyone holding auxiliary
   knowledge. Pseudonymization narrows the egress channel; it does not
   anonymize the data. The mitigations we offer — `generalize` actions for
   dates/ages/locations and per-context placeholder scope — reduce the
   quasi-identifier surface; they do not eliminate it.
2. **Placeholder co-occurrence structure.** Within one prompt, consistent
   placeholders are what make the context usable ("[PERSON_1] said…
   [PERSON_1] prefers…"), and that relational structure is itself
   quasi-identifying. The `memory` scope (stable pseudonyms across all
   prompts) extends that structure across the provider's entire log — which
   is exactly the joinability the default per-context scope exists to
   prevent. The scope knob is a privacy/utility dial and is documented as
   one.
3. **The model itself.** A model can paraphrase a placeholder ("the person
   you mentioned"), breaking exact-match rehydration for that span, and a
   frontier model can sometimes *infer* an identity from context plus its
   training data. Rehydration is exact-token, best-effort; nothing can make
   a model un-know what it can infer.
4. **Detector recall.** No detector chain catches 100%. Tier 0 is
   deterministic and precise but pattern-bound; NER adds recall and
   false positives. The `audit` mode (§8.1) exists so a deployment can
   measure leakage candidates before trusting the gate.

**Relation to GDPR.** Per [gdpr.md](gdpr.md), pseudonymized data with a
retained mapping is still personal data (Art. 4(5)); only the ingress
`redact`/`generalize` actions even approach Recital 26 territory, and we do
not certify that they reach it. The vault *is* the "additional information
kept separately" of Art. 4(5), which is why D5/D9 gate it the way they do.
The compliance contract is the house one: the deployment holds the
obligations, and this feature's job is to make pseudonymization
(Art. 25(1), 32(1)(a)), erasure reach (Art. 17, REQ-ANON-1), vault custody
and lifetime (Art. 4(5), 5(1)(e), REQ-ANON-6), and DSAR honesty (Art. 15,
REQ-ANON-4) mechanically easy to satisfy and to *evidence* — the Tier-2
audit trail, the policy `because` fields, and `/api/config` observability
are the evidence surfaces a DPIA can lift.

## 4. The two pipelines

### 4.1 Egress: prompt-safe context

```
recall/ASSEMBLE ──► SearchHits ──► [detect ▸ substitute] ──► render ──► prompt
                                        │                                 │
                                        ▼                                 ▼
                                  mapping (ephemeral                 external LLM
                                  or vault)                               │
                                        │                                 ▼
caller/decorator ◄── [rehydrate] ◄──────┴──────────────────────────── response
```

The substitution runs at the **store read boundary**: `Areev::recall` /
`recall_hybrid` and every other public read that returns grain fields —
`get`, `get_history`, `open_forks`, the graph/time reads, the run traces,
and the DSAR `subject_report` (whose raw form is the reveal-granted
variant) — funneled through one internal chokepoint behind a wrapper
type, so a future read method cannot skip the substituter by omission.
Hash re-resolution sits inside the boundary too: anonymized results still
carry grain hashes, and a raw `get(hash)` would otherwise be a one-call
bypass. That placement — below the facade, not on it — is forced by three
facts found in the audits for this proposal:

- `areev_cal::render` is pinned pure ("no store access, no clock, no host
  config") — a config-carrying redactor inside it would break a stated
  invariant;
- **MCP's `areev_recall` bypasses the renderer entirely** (it hand-rolls
  JSON from `SearchHit`), so a render-layer hook would leave the single
  most agent-facing surface unredacted; and
- **the CLI and both bindings bypass the facade**: they call store-level
  `recall`/`recall_hybrid` directly (`with_store`, and the CLI's own
  assembly path), so a facade-level hook would leave three shipped
  surfaces raw.

ASSEMBLE inherits the hook — its recalls funnel through the same store
reads — and nothing substitutes twice: results carry the anonymized
marker, and the substituter treats already-tokenized text as settled.
Three read families are deliberately exempt, each stated in code at the
exempt method: `subject_report` (the DSAR disclosure must stay faithful —
REQ-ERASE-9 — until P3's reveal grants gate it), `run_grains` (the
machine replay read the runtime reconstructs state from; the model-facing
`run_trace`/`run_yield` views ARE covered), and the authz engine's
internal grant recall (a pseudonymized principal would fail every check;
it reads through a private raw variant that no public surface exposes).

Anonymized results are marked (`anonymized: true` plus the `mapping_id`
of §6 on the CAL result payload and MCP tool results — the mapping itself
never rides those surfaces, per D5's custody rule), token budgeting runs
over the substituted text (the estimate must reflect what is actually
sent), and hashes in results remain what they are — references, not
content commitments about the substituted view. Substitution happens
once, above the renderer, so every `FORMAT` rendered from one collected
result set — markdown, json, sml, toon — shares the same tokens and one
`mapping_id` (D11); separate budget-truncated assemblies can select
different grain subsets, which is what `session`/`memory` scope and the
render-many-formats-from-one-result-set pattern are for (§6).

For in-process LLM flows the reconstruction is automatic:
`PseudonymizingBackend<L: LlmBackend>` is a decorator that anonymizes the
request and rehydrates the response. Wrapping the one `LlmBackend` seam
covers `remember()` fact extraction, the loop's DISCOVER→GROUND→VERIFY
verifier, and the runtime's tool-calling LLM in a single stroke — today all
three send raw grain content to whatever model the host configured. For
out-of-process flows (the host app calls its own LLM), the API returns the
mapping alongside the rendered context and offers `rehydrate(text, mapping)`.

### 4.2 Ingress: store less

Ingress mode runs the same detector chain **before serialization**, at
the write chokepoints — and the list, not a single hook, is the contract:

- `Areev::capture()` — "the one place raw remembered text is written",
  shared by `areev remember`, both bindings, MCP `areev_remember`, and
  `capture-stop`;
- `Areev::remember()` — the transform runs on `content` **before** the
  extractor sees it, so the derived Fact drafts (the semantically richest
  output) inherit the transform instead of leaking around it; the same
  hook covers host-supplied drafts entering `attach_facts`;
- `AreevFacade::build_grain_from_json` (+ `write_ns`) — the structured
  write chokepoint behind `cal_add`, `cal_add_if_novel`, `cal_add_batch`,
  `cal_supersede`, and the CAL ADD family; and
- the memory-tool adapter and the `migrate` importers, which write grains
  directly and each get the hook explicitly.

Raw `Areev::add()` of a pre-built grain stays below the policy line
(§4.3, last row).

The consequence of D8 is worth spelling out: the grain's hash commits to
the *transformed* content. What replicates, what BM25 and the vector leg
index, what a DSAR `subject_report` discloses, and what erasure must later
find are all the same anonymized text — there is no second, secret copy.
(The seductive alternative — transform only inside `prep_from_blob`'s index
projection — stores the original blob verbatim, ships it in every bundle,
and silently restores it to the index on `rebuild_text_index()`. Rejected.)

Ingress actions are `redact`/`generalize` (destructive; the value is gone)
or `pseudonym` with the vault (reversible; and then the file is
pseudonymized-at-rest, which composes with encryption rather than replacing
it). One asymmetry to respect: term-dictionary interning happens for
subject/relation/object *fields* independently of prose, so a policy that
anonymizes text but leaves `subject: "caller:john"` intact has anonymized
nothing — the structural-field detector (§5.1) exists precisely for this.

Two more properties hold only because ingress pseudonyms are value-derived
(D8). *Idempotency:* `add` deduplicates on the content address and
`cal_add_if_novel` depends on it — counter-based tokens would make the
same raw text hash differently on every write, silently breaking both.
*Erasure addressability:* `FORGET SUBJECT "caller:john"` and `REPORT
SUBJECT` match the interned identity string, so the engine recomputes the
stored pseudonym from the real identity and selects under **both** names;
an ingress policy whose pseudonyms cannot be recomputed from the identity
is refused at `set` time (REQ-ANON-7).

### 4.3 What each surface gets

| Surface | Egress coverage | Ingress coverage |
|---|---|---|
| CAL (`RECALL`/`ASSEMBLE`/`FORMAT`, any format) | automatic via store read boundary | ADD/REMEMBER via facade/`capture` hooks |
| MCP (`areev_recall`, `areev_cal`, run tools) | automatic via store read boundary | `areev_add`/`areev_remember` via same hooks |
| Server/console API | automatic (executor path) | same |
| CLI (`recall`, `search`, `cal`, context assembly) | automatic | `add`, `remember` |
| Python / Node bindings | automatic + explicit `anonymize_text`/`rehydrate_text` | automatic + explicit APIs |
| In-process LLM calls (extract, loop, run) | `PseudonymizingBackend` decorator | n/a |
| Raw `Areev::add()` of a pre-built grain | n/a | **below the policy line** — the caller built the grain; explicit `scan`/`anonymize` APIs are offered, mirroring how `with_store` is the documented raw escape hatch under authz |

## 5. Detection: layered and pluggable

Detections are spans: `{start, end, category, confidence, detector_id}` —
offsets are **UTF-8 byte positions over NFC-normalized text** (the same
normalization canonical serialization applies), so a span always slices
exactly what would be stored or hashed; ingress detection runs after NFC,
before serialization. Detectors run as a chain; overlapping spans
coalesce by **action severity** first (`redact` > `mask` > `generalize` >
`pseudonym` > `allow`), span length only as a tiebreak within one action
— a long `person` span must never swallow a `credit_card` into the
reversible mapping. Policy maps category → action; a category emitted by
any detector but absent from the map takes the policy's `default_action`
(default `pseudonym`), so the chain fails closed on categories it didn't
anticipate, not open. Confidence below the policy's `min_confidence`
drops the span (Tier 0 detections are confidence 1.0).

### 5.1 Tier 0 — built-in, deterministic, zero new dependencies

| Detector | Categories | Notes |
|---|---|---|
| Structural fields + known identities | `person` | Schema-aware: `subject`/`object`/`user_id`/`observer`/`session_id` are identities by construction, and every identity already interned in the memory's dictionary becomes a prose match term — `subject: "caller:john"` makes a bare "john" in any grain's text detectable with no NER. This is the detector a proxy product cannot have. |
| Regex + validator | `email`, `phone` (E.164 + common formats), `ipv4`/`ipv6`, `mac`, `url_userinfo`, `date` | Pattern plus structural validation where one exists. |
| Checksummed IDs | `credit_card` (Luhn), `iban` (mod-97) | Checksum validation kills most false positives; a 16-digit string that fails Luhn is not a card. |
| National IDs | `national_id` | Locale-tagged pattern set, off by default except the deployment's declared locales. |
| Secrets | `secret` | Known prefixes (`sk-`, `AKIA`, `ghp_`, PEM headers) + high-entropy heuristic. Always-on even in otherwise-off policies is worth considering (§13). |
| Keyword-proximity | `pin`, `password`, `otp`, `account_number` | Contextual patterns: a nearby cue word plus a value shape (`pin number is 1462`); cue lists are locale-extensible per policy. A bare digit run with no cue never matches — Tier 0 buys precision, Tiers 1–2 buy recall. |
| Dictionary | `custom` | User-supplied terms per policy (client names, project codenames). Case/NFC-normalized exact and word-boundary match. |

This tier lands where the dead stub already is: `detect_pii` in
`areev-core` becomes real, `scan_for_pii`/`ContainsPii` (CR-F1) comes alive
for tool-schema grains, and the module grows into `areev_core::anon`
(categories, span type, validators, placeholder codec, HMAC pseudonym
derivation) so every crate above it shares one implementation.

### 5.2 Tier 1 — external NER over the command seam

`AnonymizeBackend` follows the four-seam contract already in the tree
(`CommandEmbed`, `CommandLlm`, `CommandAnalyzer`, the run executor):
whitespace-split argv, never a shell; construction-time probe so a broken
command fails at setup; JSON over stdin/stdout; the host owns the model.

```
probe:   {"areev_anonymize":1,"op":"probe"}
      →  {"id":"presidio/2.2","categories":["person","location","org",…]}
analyze: {"areev_anonymize":1,"op":"detect","text":"…"}
      →  {"detections":[{"start":10,"end":18,"category":"person","confidence":0.92}]}
```

Offsets in `detect` responses are UTF-8 byte positions over the
NFC-normalized input, as §5 pins — byte-vs-code-point-vs-UTF-16 ambiguity
is a silent mis-slicing bug on exactly the non-ASCII text where the
stakes are highest, so the protocol names its unit.

Installed via `set_anonymizer(Box<dyn AnonymizeBackend>)` /
`--anonymize-cmd 'CMD'` (global CLI flag, one paragraph in the help text,
one pre-verb application block — same shape as `--embed-cmd`). This is how
Presidio, GLiNER, spaCy, or a local ONNX model plug in without any of them
becoming a dependency. Per D6, when the effective policy requires egress
anonymization, a Tier-1 failure fails the render — the posture is the
governed-agents redactor's, not `CommandAnalyzer`'s.

### 5.3 Tier 2 — LLM-based detection

For prose where NER models underperform (nicknames, indirect references),
the existing `LlmBackend` trait can host a detect call. Two rules: **local
first** (Ollama et al.) — shipping text to a remote model in order to
anonymize it inverts the feature — and a remote detector therefore requires
an explicit opt-in flag whose name says the quiet part
(`--anonymize-llm-remote-ok`). Grounding reuses the extract pipeline's
span-verification habit: a detection whose span text does not appear in the
source is discarded.

### 5.4 Why layered

Tier 0 alone already covers the structured majority of what a memory engine
holds (identities live in `subject`, contact data in typed fields) at zero
latency and perfect determinism. Tiers 1–2 add prose recall at increasing
latency and decreasing determinism. Policies can run `egress: tier0` for
the 50 ms voice loop and `egress: tier0+ner` for batch assembly — the tier
selection is part of the policy, so it travels with the file, while the
binaries that satisfy it are host capabilities.

## 6. Actions, placeholders, reconstruction

| Action | Reversible | What it does | Best for |
|---|---|---|---|
| `pseudonym` | yes (mapping) | `[PERSON_1]` typed token | egress default for identities |
| `mask` | no | format-preserving fake (`j***@d***.com`, `+1-555-***-**41`) | fields whose *shape* downstream logic parses |
| `generalize` | no | bucket: age→decade, date→month, postcode→district | quasi-identifier damping (§3.1) |
| `redact` | no | `[REDACTED:secret]` | secrets, cards — things no model should see even typed |
| `allow` | — | pass through | explicit exemptions |

**Placeholder format.** `[{CATEGORY}_{ID}]`, uppercase, e.g. `[PERSON_1]`,
`[EMAIL_2]` — bracketed single tokens survive LLM round-trips well, and the
category keeps the context legible to the model. `{ID}` is an
appearance-order counter under `context`/`session` scopes and a truncated
keyed-hash fragment under `memory` scope (`[PERSON_7F3A]`) — a counter
cannot be stable across assemblies; a keyed hash is. Template is
configurable per policy; the codec (parse + substitute + collision
handling when the source text already contains something
placeholder-shaped) lives in `areev_core::anon` and is shared by both
directions.

**Scope** (D4): `context` (default — numbering and mapping restart per
assembled context), `session` (stable across one session), `memory`
(stable per file; pseudonym = HMAC-SHA256 over the value, keyed by a
derived subkey, truncated — deterministic without storing a table, same
family as `subject_fingerprint`). Wider scope = more utility = more
joinability; the console copy says exactly that. A leaked HMAC key turns
low-entropy values (phone numbers) into a dictionary attack, which is one
more reason the key derives from the file's encryption key on encrypted
files rather than living anywhere else (the unencrypted case is open
question 5).

**Reconstruction.** `rehydrate(text, mapping_or_id)` replaces exact
placeholder tokens in a model response with the originals. In-process, the
`PseudonymizingBackend` decorator does this transparently. Out-of-process,
the mapping rides the API result (egress call returns
`{context, mapping_id, mapping}`) and the host passes either back.
Unmatched placeholders in a response are left intact and reported, never
guessed. One rule decides recoverability: **only `pseudonym` spans enter
the mapping.** `mask`, `generalize`, and `redact` are one-way by
definition — a value the policy redacts (a card number) is *gone* from the
round trip on purpose, so a value that must come back to the end user (a
username, a PIN in a support flow) must be mapped to `pseudonym`.

**The round trip, end to end.** Grain content, stored raw under egress
mode with `subject: "caller:john"`: `my user name is john, and pin number
is 1462`.

1. Recall/ASSEMBLE under a policy `{person: pseudonym, pin: pseudonym}`
   yields context `my user name is [PERSON_1], and pin number is
   [PIN_1]`, mapping `{"[PERSON_1]":"john","[PIN_1]":"1462"}`, and its
   `mapping_id`. (Tier 0 grounds both detections: "john" in the prose via
   known-identity propagation from the `subject`, "1462" via the
   keyword-proximity detector — no NER required for this example.)
2. The prompt built from that context goes to the external model, which
   answers in kind: `Hi [PERSON_1], I've verified pin [PIN_1].`
3. `rehydrate(response, mapping_id)` → `Hi john, I've verified pin
   1462.` — what reaches the end user is meaningful data, not tokens.

**Keyed and deterministic (D11).** `mapping_id` is a truncated
HMAC-SHA256 — keyed under the session key (the vault subkey when the
vault is on) — over the canonicalized policy, the scope, and the sorted
placeholder→value pairs. *Keyed* matters: a bare digest over the values
would be a confirmation oracle — this section's own worked example
contains a 4-digit PIN an adversary could brute-force against an unkeyed
hash in microseconds, and the id travels on exactly the surfaces the
adversary sees. *Determinism* has an honest boundary: the same collected
results under the same policy and key reproduce the same tokens and id,
but under `context`/`session` scope a **re-assembly** is not guaranteed
the same results — recall is deadline-bounded fail-open, RRF-fused,
recency-weighted, and budget-truncated — so tokens renumber and the id
changes with them, deliberately: the id names one exact mapping, and
rehydrating against the wrong one must fail loudly rather than substitute
wrong values. Reproducibility *across* assemblies comes from `scope:
memory` (value-derived tokens, at the joinability cost §3.2 describes) or
from the vault: within one process the mapping lives on the session (the
ephemeral default); resolving a *bare* `mapping_id` later, or from
another process, is exactly what the opt-in vault (§7) exists for, with
REQ-ANON-3 gating the read.

**Both versions, many formats, one pass.** Substitution happens on result
fields above the renderer (§4.1), so a single detection pass over **one
collected result set** feeds every output format: anonymized markdown,
anonymized json, and toon all carry the same tokens and one `mapping_id`.
(Running a separate budget-truncated assembly per format can select
different grain subsets — render the formats from one result set, or use
`session`/`memory` scope for cross-assembly token consistency.) The
*original* rendering of the same
results is not a second pipeline — it is the same recall with the
substitution step skipped, which under an active policy is precisely the
privileged read D9 gates. A principal holding the reveal grant can request
anonymized-markdown and original-json side by side (one audit
Observation); a caller without the grant gets placeholders in every
format.

## 7. The mapping: ephemeral by default, vault by choice

**Ephemeral (default).** The mapping lives on the facade session and/or is
returned to the caller; nothing touches disk. A long-running host (MCP
server, `areev ui`, an embedded app) rehydrates responses across many tool
calls; when the process exits the mapping is gone. The session table is
bounded — an LRU with a TTL, never an unbounded re-identification table on
the longest-lived process we ship — and joins the facade's existing
interior-mutability state (`recall` takes `&self`). This is the correct
default because a persisted placeholder↔value table is a re-identification
asset, and the safest vault is the one that doesn't exist.

**Vault (opt-in), for flows that must reconstruct later** (one-shot CLI
invocations, resumed runs, HITL approvals that arrive tomorrow):
`vault:<mapping_id>:<placeholder>` rows in the `meta` table (keyed by
`mapping_id` so two assemblies' `[PERSON_1]`s can never collide and
silently rehydrate the wrong identity), values sealed with AES-256-GCM
under `HKDF(page_key, info=b"areev.vault.v1")` with the row key as AAD —
the exact `blobcrypt` construction with its own domain-separation string.
Consequences, each inherited deliberately:

- an unencrypted memory has no page key, so **vault persistence on a
  plaintext file is a hard `CRY` refusal** — not a silent plaintext table;
- **crypto-erasure kills the vault by construction** — destroying the page
  key destroys the derived vault key with it;
- `vault:` stays **out of `REPLICABLE_META_PREFIXES`**, so bundles/segments
  never carry the re-identification table to replicas or the hub. (The
  import path already refuses non-allowlisted prefixes, so a crafted bundle
  cannot inject vault rows either; a conformance case pins both directions.)
- the vault does not exist on the `postgres` backend in v1 — there is no
  page key to derive from; a policy requesting vault persistence there is
  refused as loudly as on a plaintext file.

**Requirements** (erasure interactions are hard invariants, so they get
REQ IDs in the erasure.md style):

- **REQ-ANON-1 (erasure reaches the vault).** `erase_where` today never
  touches the `meta` table — a vault row would survive `forget_subject` and
  re-identify a subject the store believes erased. The erasure path gains a
  vault sweep over the same identity list telemetry scrubbing iterates, but
  run **inside the erasure transaction** — the telemetry scrub's
  best-effort post-commit placement is not good enough for a hard
  invariant — deleting every mapping whose plaintext names an erased
  identity: by direct key lookup where the pseudonym is recomputable from
  the identity (`scope: memory`), and by a decrypt-and-compare scan of the
  small, file-local vault otherwise. Identity erasure is the selector that
  reaches the vault; age reaches it through REQ-ANON-6's TTL; single-grain
  `forget(hash)` deliberately does not (a mapping is per-assembly, not
  per-grain) — stated so nobody assumes otherwise. Ships in the same phase
  as the vault itself, gated by the same test.
- **REQ-ANON-2 (the vault never replicates).** `vault:` never joins
  `REPLICABLE_META_PREFIXES`; conformance cases assert export omits and
  import refuses.
- **REQ-ANON-3 (reveal is audited).** Reading a vault mapping through any
  surface requires an authz grant and writes a Tier-2 Observation in
  `agent:authz` carrying the subject *fingerprint* (never the identity) —
  the same non-re-identifying audit rule erasure uses.
- **REQ-ANON-4 (reports disclose the pipeline).** `REPORT SUBJECT` /
  `subject_report` output states whether anonymization policies were active
  in the namespaces it covers, so a DSAR answer doesn't overclaim what is
  stored in cleartext.
- **REQ-ANON-5 (no policy is loud).** Every surface that renders context
  reports the effective anonymization mode (`off` included) in its
  config/observability output, so "we thought it was on" is diagnosable
  from `GET /api/config` alone.
- **REQ-ANON-6 (the vault has a lifetime).** Persisted mappings are
  subject to storage limitation (GDPR Art. 5(1)(e)) like any other
  personal data: vault rows carry a written-at timestamp, the policy may
  declare a vault TTL, and `sweep_retention` includes expired vault rows
  in its pass. A re-identification table must not be the one thing in the
  file that lives forever by default.
- **REQ-ANON-7 (ingress stays erasable).** An ingress-pseudonymized
  identity remains addressable by its real name: `FORGET SUBJECT` and
  `REPORT SUBJECT` recompute the value-derived pseudonym and select under
  both names, so pseudonymized-at-rest never means erasure-proof. An
  ingress policy whose pseudonyms cannot be recomputed from the identity
  is refused at `set` time.

## 8. Configuration

### 8.1 Per-file policy: `anon:<ns>`

One JSON meta row per namespace, `retention:<ns>`'s skeleton (set/list/
clear triple, mandatory-ish `because`, declared-vs-enforced separation):

```json
{
  "mode": "egress",              // off | egress | ingress | both | audit
  "detectors": ["tier0", "ner"], // which chain links the policy demands
  "categories": {
    "person":      "pseudonym",
    "email":       "pseudonym",
    "phone":       "mask",
    "credit_card": "redact",
    "secret":      "redact",
    "date":        "generalize:month",
    "custom":      "pseudonym"
  },
  "default_action": "pseudonym",   // fail-closed for unmapped categories
  "custom_terms": ["Project Nightingale"],
  "scope": "context",            // context | session | memory
  "placeholder": "[{CATEGORY}_{N}]",
  "min_confidence": 0.6,
  "because": "EU deployment; prompts leave the boundary via provider X"
}
```

`audit` mode detects and *reports* (counts + categories, never the spans'
plaintext) without transforming — the measurement mode §3.4 calls for, and
the recommended first step of any rollout. As built in P1 the counters are
in-memory on the handle (`anon_audit_counts`, surfaced by `/api/config`),
which needs no sidecar and works on every backend; durable sidecar
persistence is a later refinement. Merge on sync is `retention:`'s conservative
rule: write-if-absent, never silently swap a live policy. Parse failure is
a hard `VAL` error (D3). Enforcement mismatch is loud: a policy that
demands `ner` in a process with no `--anonymize-cmd` installed fails
egress closed (D6) and shows up in `open_warnings()`/`/api/config`
warnings, exactly like the "vector leg dormant" warning does today.

Version skew fails loudly where we control it and is named where we
don't. Setting any `anon:` policy stamps `min_reader_version`; the shipped
semantic of that stamp is a **loud open warning** (never a hard refusal —
correcting this proposal's first draft), so an older build opening the
file is told it cannot honor everything the file declares before it serves
a single read (note the file's bundles also start exporting as MGB2 once
replicable rows exist). What stamping cannot fix is an old
*replica*: an anonymization-unaware build importing a bundle drops the
unknown `anon:` row silently, because the importer filters on its own
allowlist — so a fleet upgrades its readers before relying on replicated
policy, and the conformance suite pins this skew case so the gap stays
documented by a test rather than discovered as a surprise.

### 8.2 Host capabilities

- `Areev::set_anonymizer(Box<dyn AnonymizeBackend>)` (+
  `set_anonymizer_command` in the bindings), CLI `--anonymize-cmd` /
  `--anonymize-model` — per-process, never persisted, embedder-style.
- The process-wide floor is **not** an executor-config field: MCP's
  `areev_recall` never passes through the executor, and the CLI has no
  `rebuild_executor` (there are five separate `CalExecutorConfig`
  construction sites across CLI, MCP, and the server). The floor lives
  where the hook lives — `anonymize_egress_floor` is installed on the
  store handle at open time, embedder-style — a restrictive cap in the
  `allow_destructive_ops` mold: it can force egress anonymization on (a
  hub exposing a shared file) but can never switch a file-declared policy
  off.
- The LLM decorator: `PseudonymizingBackend::new(inner, chain)` in
  `areev-llm`, applied by the CLI/loop/run wiring whenever both an LLM
  backend and an active egress policy are present. To keep the dependency
  graph acyclic, the policy type, `Detection`, and the chain runner live
  in `areev_core::anon` (both `areev-llm` and `areev-store` already sit
  above core); detector backends are injected by the host, so neither
  leaf crate ever depends on the other.

### 8.3 Console and API

- **Connect → Settings** gains an "Anonymization" card next to the analyzer
  toggles it mirrors: mode selector (Off / Audit / Egress / Both), the
  category→action list as rows with plain-language labels, detector status
  ("Built-in ✓ · NER — not installed on this host"), and the `because`
  string. Developer mode reveals the raw policy JSON and per-category
  detector IDs.
- `GET /api/config` gains an `anonymization` block: effective mode per
  namespace, installed detectors, and reconciliation warnings (policy
  demands NER / host has none; vault requested / file not encrypted).
- The memory browser gets a per-grain "as the model sees it" preview toggle
  when a policy is active — the single most persuasive UI element this
  feature can have, and cheap once the store read boundary exists (phase P4).
- CLI verb, `retention`-shaped: `areev anonymize
  <set|list|clear|scan|reveal>` — `scan` runs the chain over stdin or a
  namespace in audit mode; `reveal` is the vault read (REQ-ANON-3 applies).
- MCP: existing tools inherit egress automatically (the store read
  boundary — no per-tool work). One new tool at most (`areev_reveal`, principal-
  required like `areev_run_respond`); the tool count is pinned in
  `docs/mcp-reference.md` prose and must be bumped with it.

## 9. CAL: nothing new in v1, and why

CAL syntax is an OMS conformance contract (invariant #4); a `WITH
anonymize` spelling is spec-level work, not a product patch. But there is
also a *mechanical* reason to keep the gate out of the query text: CAL's
own compatibility rule is that unknown `WITH` options warn and skip. A
query carrying `WITH anonymize` into an older build would silently return
raw content — the exact failure a privacy feature cannot have. Gates
belong in the file (policy row) and the process (the store-handle floor),
which
both fail closed on version skew; per-query text may only tighten.

What CAL users see in v1: results from a policy-covered namespace arrive
anonymized regardless of format (D1/§4.3), the result payload carries
`anonymized: true` and the `mapping_id` (§6), and `classify` is untouched
(no new statements; scan, reveal, and rehydrate are API/CLI/MCP
operations, and reveal's audit rides the existing Tier-2 machinery). The
v2 spec proposal — `WITH anonymize("strict")` as a recall-tier option,
ASSEMBLE per-source overrides, an `anonymized` conformance assertion, and
a rehydrate spelling (`REHYDRATE "<text>" WITH mapping("<id>")` or
similar — a new statement, hence a `classify` decision and a spec-level
one) — goes to OMS as an amendment in the `oms-1.6-amendments.md` mold
once v1 has field mileage.

## 10. Where it lands (implementation inventory)

| Crate | Change |
|---|---|
| `areev-core` | `anon` module: categories, `Detection`, policy type + chain runner (keeps `areev-llm`/`areev-store` acyclic), Tier-0 detectors + validators, placeholder codec, keyed pseudonym/`mapping_id` derivation; `detect_pii` stub becomes real and `scan_for_pii` gains its production call site in the tool-schema write path (CR-F1 revived) |
| `areev-store` | `AnonymizeBackend` trait + `CommandAnonymize`; **egress substitution at the store read boundary** (one wrapper-typed chokepoint over `recall`/`get`/history/graph/trace reads); `anon:<ns>` policy CRUD (`set_/get_/clear_anon_policy`, `min_reader_version` stamp on set); ingress hooks in `capture`/`remember`/`attach_facts`/memory-tool/migrate; vault + `areev.vault.v1` subkey in `finish_open`; in-txn REQ-ANON-1 sweep in `erase_where`; vault TTL in `sweep_retention` (REQ-ANON-6); policy prefix into `REPLICABLE_META_PREFIXES`, vault kept out |
| `areev-cal` | egress inherited from the store boundary (the facade adds the reveal-grant arm and session mapping table); ingress at `build_grain_from_json`; `anonymized` + `mapping_id` on result payloads |
| `areev-context` | none structural — hits arrive pre-anonymized; budget math already runs over what it receives |
| `areev-llm` | `PseudonymizingBackend<L>` decorator; Tier-2 detect op |
| `areev-mcp` | inherits via the store boundary; optional `areev_reveal` |
| `areev-cli` | `--anonymize-cmd` global flag; `anonymize` verb family; extract-pipeline decorator wiring |
| `areev-server` | `/api/config` block; console Settings card |
| `areev-py` / `areev-js` | `set_anonymizer_command`, `set_anon_policy`, `anonymize_text`, `rehydrate_text` — scalars in, JSON strings out, both bindings in lockstep |
| `areev-conformance` | REQ-ANON-2 cases (vault never rides/loads); policy-row merge cases |
| tests | detector golden corpus (per category, positives + near-miss negatives); round-trip property test `rehydrate(anonymize(t)) == t` for pseudonym-only policies; egress fail-closed test; erasure-reaches-vault test; render-parity stays green because anonymization happens above the renderer |

## 11. Phasing

| Phase | Deliverable | Gate |
|---|---|---|
| P0 | `areev_core::anon` (Tier 0 + codec + HMAC), explicit `scan`/`anonymize_text`/`rehydrate_text` on facade + both bindings, `areev anonymize scan` | golden corpus green; round-trip property test |
| P1 | Egress pipeline: `anon:<ns>` policy rows, store read boundary, ephemeral session mapping, payload flags, `/api/config` block, store-handle floor | fail-closed test; every grain-returning read covered; docs examples parse |
| P2 | `PseudonymizingBackend` + extract/loop/run wiring; ingress via `capture` + `build_grain_from_json` | content-address commits to transformed text (D8 test) |
| P3 | `CommandAnonymize` NER seam; vault + subkey + REQ-ANON-1..3; `reveal` + audit | conformance cases on both backends |
| P4 | Console card + per-grain preview; Tier-2 LLM detector; generalization library | usability pass on the Paper design file first |
| P5 | OMS amendment proposal for `WITH anonymize` | spec decision, not ours alone |

## 12. What we deliberately will not do

- **No bundled NER model, no Python runtime, no ONNX dependency.** The
  command seam exists so the heavy lifting stays out of the dependency
  tree (invariant #6). An `areev-anonymizer` companion binary can ship
  separately if demand exists.
- **No "anonymize the index, keep the blob" mode** (D8). It is a leak with
  extra steps.
- **No silent fallback to raw on detector failure** (D6).
- **No anonymity claims.** The word in the API is `pseudonymize` wherever a
  mapping exists; marketing copy inherits the docs' vocabulary, not the
  reverse (D10).
- **No CAL syntax in v1** (D7), and no new destructive-adjacent CAL verbs
  ever for the vault — reveal is not a query-language concern.

## 13. Open questions

1. Should the `secret` category be detected-and-redacted even when no
   policy is declared (a safety floor, like the body cap), or is
   zero-surprise ("no policy, no transform") worth more?
2. Placeholder collision policy: if source text already contains
   `[PERSON_1]`, escape it, renumber around it, or fail? (Leaning: escape
   on egress, assert-and-refuse on rehydrate ambiguity.)
3. Does `audit` mode's telemetry row (category counts per namespace) need
   its own retention treatment, given counts alone can be a weak
   quasi-identifier for tiny namespaces?
4. Is the store-handle floor per-mode (`egress` only) or a full policy
   override? A hub forcing `both` would change ingress semantics for every
   caller, which smells too powerful for a process flag.
5. For `scope: memory` pseudonyms on an *unencrypted* file there is no key
   to derive the HMAC from — refuse the scope, or derive from a
   host-supplied secret with the joinability warning?
