# Agent integration boundaries — code, models, and credentials

**Status:** proposal. Written 2026-08-19, from a design review that modelled
three real workloads onto the existing primitives: invoice intake from an
Outlook mailbox into Zoho Books, NDA review conducted over email, and
contact-form lead enrichment into a CRM.

**All three parts are BUILT.** Each section records what the implementation
changed about the design, because two of the three changed materially.

The review's conclusion was that the primitives compose — `Trigger` for
ingress, `Workflow` for the plan, `Tool` Definitions for effects, the journal
for the record — and that everything genuinely missing sits at three
boundaries, all of them where Areev meets something it does not control:

| # | Boundary | Question it answers |
|---|---|---|
| 1 | **Code** | May a tool that arrived in a memory execute? |
| 2 | **Models** | What may leave for a model provider? |
| 3 | **Credentials** | What may a tool reach, holding whose secret? |

They share one shape, and it is the thesis of this document:

> **A declaration replicates. An authorization never does.**

The codebase already reached that split twice, for reasons that had nothing to
do with each other. Trigger evaluation state lives in non-replicating `trg:`
rows, because a dev memory restored from prod must not inherit prod's cursor.
Host config — embedder capability, executor limits, the destructive-ops cap —
is per-process and never persisted, because a file that carried its own
permissions would grant them to whoever imported it. Both are the same rule,
and all three boundaries below are that rule applied again.

---

## 1. `executor_uri` — connectors that travel, code that does not auto-run

### The defect

`executor_uri` on a `Tool` Definition was serialized, deserialized,
CAL-buildable, template-renderable, strictly type-validated — and read by zero
lines of `areev-run` and `areev-trigger`. A Definition declaring
`executor://crm.lookup@v3` executed whatever `--tool-cmd` happened to be.

That is the same defect as the `Workflow.trigger` field removed in `5699cbc`: a
field describing an activation it cannot cause. It was also the first field a
connector-pack author would reach for, which made it worse than inert — it
looked like the answer to a problem it did not touch.

### Why wiring it beats removing it

Nothing ships for Outlook, Zoho, or any CRM, and connectors are the adoption
blocker. Areev cannot win a connector-count race against n8n or Zapier and
should not enter one. What it has instead is content-addressed, replicating,
provenance-tracked storage.

The decisive argument is that the loop already built most of the loop. Rule E1
requires a `code_revision` recommendation to target a `tool:` and pin the
evalset it was gated on — machinery that exists and does nothing, because there
is no code for it to revise. Wiring `executor_uri` completes it, and the
resulting claim is one no workflow product can make:

> Integrations live in the memory, content-addressed. Areev proposes
> improvements to them, gates each on a pinned evalset, and the journal records
> which version ran on every execution.

Removing the field would have been defensible. It would also have closed the
only door to that.

### The danger, stated plainly

Bundles carry blobs. So importing a peer's memory imports their connector code,
and auto-executing it is remote code execution by design.

This is not hypothetical, and the failure mode is already cited in
[`triggers.md`](triggers.md): the January 2026 n8n community-node compromise
exfiltrated decrypted OAuth tokens, and the malicious node never violated a
sandbox. It read a credential it was given and made a request it was allowed to
make. Zapier runs each task in a Firecracker microVM and that attack still
works. **Isolation does not constrain what actually goes wrong here**, because
a connector legitimately needs the network and the credential.

### The design

The declaration replicates; the authorization does not.

- A Definition may set `executor_uri: "cas://sha256:<64 hex>"`. The scheme is
  the store's existing CAS vocabulary, so `get_blob` verifies the digest on
  every read and the integrity check costs nothing extra.
- **Every value now either dispatches or is refused by name.** A scheme this
  build cannot execute is `RUN-E018` at resolve; so is an `executor_uri` on a
  `client` tool, which is answered by a person and has no executor. There is no
  third path where a value is quietly ignored — that was the original bug.
- Execution requires the address in a **host-side allowlist**
  (`areev run start --allow-executor <addr>`). Absent it, the run is refused at
  start, before it takes a lease or writes a manifest, with a message naming
  the address so pinning it is a copy-paste.
- **There is deliberately no grant form.** `mg:permits` Facts live in the file
  and replicate; a permission arriving in the same bundle as the code it
  authorizes is not a permission.
- The address is pinned into the run manifest at start, so a Definition
  superseded mid-run cannot change what executes — the resolution freeze the
  bindings already get.
- The blob is read and hash-verified on the **driver thread** and handed to the
  pool as bytes, because pool workers touch no store handle. This mirrors how
  LLM effects resolve their pinned Definitions.
- Materialized to `<cache>/<hex>`, mode 0700, write-then-rename. The path *is*
  the content address, so a poisoned cache entry cannot impersonate another
  executor.
- Purely additive: a Definition with no `executor_uri` behaves exactly as
  before, and `--tool-cmd` is untouched.

An importer therefore gets a connector pack that replicates with the memory and
**refuses to run until a human pins its hash**. That is strictly better than the
model that got n8n compromised, and it is defensible in a procurement review.

### What it does not do

It does not sandbox. A pinned executor runs as you, with your privileges,
exactly like `--tool-cmd` — see [`security-model.md`](security-model.md). The
pin is a decision about *provenance*, not a container. And the operator pins per
platform: a blob is bytes, and a shell script pinned on Linux is not a Windows
executor.

---

## 2. Anonymization — the boundary is the model, not the tool

**Built.** Two things the sketch below got wrong, both found by writing the
tests:

- **The prompt is not `input["messages"]`.** An abstract node's turn is built
  by the *scheduler* as `{instruction, state}` from the run context, so
  pseudonymizing the seam translation would have missed it entirely. The
  transform belongs on the effect input, before the seam. The first draft of
  the round-trip test failed for exactly this reason, which is the only reason
  it was caught.
- **A bare personal name is not detected.** Tier-0 catches `email` and `phone`
  by pattern; `person` matches only interned known identities and the policy's
  `custom_terms`. A test written with a plausible-looking name passed
  vacuously against a memory with no interned identities. The honest statement
  — now in both `run.md` and `security-model.md` — is that the boundary
  replaces what the detectors catch, and callers should declare the terms they
  care about rather than assume a name is covered.

The `egress`-extension question below was resolved as recommended: extending
the existing mode, because the current behavior was the defect and a second
knob would have meant two ways to be half-protected.

### The correction

An earlier draft of this review said the trigger → run path meant "the thing
actually sent to a provider is not protected", without saying which provider.
That conflated two seams that must behave differently, and the distinction is
the whole design:

- A **host tool** must receive real values. A tool that posts a pseudonymized
  vendor name to Zoho Books writes a corrupt invoice.
- A **model** should not need them. Extraction, classification, and drafting
  work as well on `[ORG_7C1A]` as on the real name.

So the boundary is the LLM, and exactly one seam crosses it: `seam_messages()`
in `runner.rs`, which builds an abstract node's model turn from run state.
Today that state arrives from the trigger's payload in process, never passing a
read exit, so an egress policy an operator reasonably believes covers
model-facing data does not cover the one place a model is called.

### The mechanism already exists

This is not a feature to design so much as two wires to connect:

| Piece | Where | Does |
|---|---|---|
| `anonymize` / `transform_text` | `core/anon/mod.rs` | real → `[PERSON_A4F2]` |
| sealed vault | `vault:<ns>:<token>` meta rows | token → value, HKDF subkey off the page key |
| `rehydrate` | `core/anon/mod.rs` | reverse — and **reports unmatched rather than guessing** |
| `mapping_id` | D11 | keyed round-trip handle |

Crypto-erasure already reaches the vault: destroy the page key and the mappings
die with it.

### The design

**Out** — `seam_messages` pseudonymizes before the model turn.
**Back** — the model's tool-call arguments are rehydrated *before* dispatch.

The LLM sees `[ORG_7C1A]`; the tool that posts to Zoho sees the real name.

Four constraints make it correct rather than merely present:

1. **Rehydration fails closed.** `rehydrate` leaves unresolvable placeholders
   intact and lists them in `unmatched`. A hallucinated `[PERSON_9999]` in a
   tool-call argument must fail the node and journal why — dispatching a
   partially rehydrated call would post a literal placeholder to a vendor.
2. **The journal stores the pseudonymized form**, plus the `mapping_id`.
   Otherwise `run verify`'s byte-compare replay diverges the moment a replay
   rehydrates differently.
3. **This forces `scope: memory`, which forces an encrypted memory.** Session
   scope numbers tokens by appearance order, so the same input yields different
   tokens on replay and verify breaks. Only value-derived (HMAC) tokens are
   replay-stable — that is D8 — and the token key is HKDF-derived from the page
   key. **A plaintext memory cannot have a replay-safe LLM boundary**, and that
   belongs in the docs rather than being discovered.
4. **Not every field.** The policy's per-category actions already express this:
   `pseudonym` for person/org/email/phone, `allow` for amounts and dates the
   model must reason about, `GenBucket` where a coarser date will do.

### The open question

`egress` currently means "model-facing reads *from the store*". Extending it to
cover the run path is what makes an operator's belief true, but it silently
changes behavior for anyone already pairing `egress` with abstract nodes.

**Recommendation: extend `egress` and record it as a bugfix.** A second knob
means two ways to be half-protected, and the current behavior is the defect.

### Honest limit

This is a model-provider boundary, not DLP. The tool gets real data, so a
compromised tool exfiltrates real data. Part 3 is what constrains that, and it
constrains it only somewhat.

---

## 3. Egress — brokering the write side

**Built.** What shipped differs from the sketch below in one way worth
recording: the per-tool grant is **host configuration, not a field on the Tool
Definition**. Writing it on the grain would have made it a permission arriving
in the same bundle as the code it authorizes — the exact thing §1 refuses for
code — so the tool names which credential it wants *at call time* and the host
decides whether it may have it. Intent travels; authority does not. That also
avoided touching canonical serialization, which was a bonus rather than the
reason.

Two things the sketch missed and the implementation needed:

- **The broker had to start authenticating its callers.** It binds loopback,
  and loopback is not an authorization — any process on the box could post to
  it and spend the credentials it holds. Each caller now presents an
  unguessable per-caller capability token. That token is also the only way one
  port can serve N pool workers and still tell them apart, so per-tool scoping
  was impossible without it. The connector path gained the same protection for
  free.
- **`TRG-E009` needed a sibling, not a rename.** The same condition reported by
  two subsystems takes one code each, exactly as a storage failure is
  `TRG-E010` in one and `RUN-E020` in the other. Egress refusal is now
  `TRG-E009` from the evaluator and `RUN-E022` from the driver.

### The gap

`AREEV_EGRESS_URL` credential brokering and the `int:allowed_outbound_hosts`
allowlist live entirely in `areev-trigger`. The run's `--tool-cmd` gets spawn
hardening but **no broker and no allowlist**.

So the polling side — which reads — is credential-brokered, while the side that
posts to a vendor API and sends mail under your company's name holds its own
token and can reach anything. That is inverted relative to blast radius.

### The design

Mostly relocation. The mechanism is 721 lines that already work.

1. **Move the broker down.** `areev-trigger` depends on `areev-run`, so the
   broker cannot be shared where it sits. Move `broker.rs` and `egress.rs` into
   `areev-run`, which owns the host-command executor; `areev-trigger` keeps
   using them. No cycle, no new crate.
2. **Same seam for tools.** The driver spawns `--tool-cmd` with
   `AREEV_EGRESS_URL` set and `--credential name=ENV_VAR` mappings. The tool
   posts a description of the call it wants; the broker attaches the
   credential. Identical contract to the connector — one contract to learn,
   which was the original argument for the connector using the tool's shape.
3. **Per-tool scoping — the RBAC part.** Declared host-side, per tool name:
   which credentials it may spend and which methods it may issue, against a
   run-wide host allowlist.

   ```bash
   --allow-host 'https://books.zoho.com,https://graph.microsoft.com' \
   --tool-egress 'zoho_post:zoho:POST,send_email:graph:POST,parse_pdf::'
   ```

   A tool with no grant never receives the broker's address at all.

4. **Deny writes by default.** Connectors read; tools write. A definition that
   does not declare `POST` cannot POST. Deny-by-default on the write verb is
   the entire point.
5. **Journal the brokered call** — host, method, credential *name*, never the
   value. "What did this run actually touch" then answers from the journal.

### On whose credentials

A declaration names a credential; it never carries one. Values are read from
host-named environment variables in the broker's process and never enter a
grain, a bundle, or the connector's environment. A customer deployment supplies
its own `ZOHO_TOKEN`. That is already true for connectors and carries over
unchanged — the gap is only that tools cannot reach it yet.

### Honest limits

Restating what [`triggers.md`](triggers.md) already says, because it applies
identically: exfiltration *through* an allowed host still works (encode data
into a draft, a label, a filename); hostname allowlisting cannot see through
DNS tricks or domain fronting; and a brokered tool cannot use a vendor SDK,
because the SDK wants its own sockets. This raises the bar. It is not a
boundary.

---

## 4. Sequencing

All three parts are built. Two follow-ups remain, both recorded rather than
left implicit.

**`unmatched` detection is silhouette-based.** `rehydrate` recognizes leftover
tokens in the default `[CATEGORY_ID]` shape, so a policy with a custom
`placeholder` template weakens the fail-closed check. Either narrow the
template to a validated shape when the run boundary is active, or teach
`rehydrate` the policy's own template.

**Egress refusals are still not journaled.** refusals are reported to the caller
as a `403` and printed to stderr when a run ends, but they are **not journaled
into the run record**. The executor holds the broker and runs on a pool thread,
so journaling from there would need driver plumbing the rest of this did not.
Worth closing, because a refusal a tool swallowed should be recoverable from
the memory rather than from a terminal that has scrolled.

## 5. What this document does not propose

- **A sandbox.** Isolation does not constrain the failure mode that actually
  occurs (see §1), and claiming otherwise would be worse than not having one.
- **New CAL syntax.** Every part above is expressible in existing grammar or in
  host configuration; new syntax is an OMS conformance decision.
- **A resident process.** Nothing here needs a daemon, and the no-daemon
  decision holds.
