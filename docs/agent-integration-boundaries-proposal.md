# Agent integration boundaries — code, models, and credentials

**Status:** proposal. Written 2026-08-19, from a design review that modelled
three real workloads onto the existing primitives: invoice intake from an
Outlook mailbox into Zoho Books, NDA review conducted over email, and
contact-form lead enrichment into a CRM.

**Part 1 (`executor_uri`) is BUILT** and landed with this document. Parts 2 and
3 are decided-in-principle and unbuilt; each names what it costs.

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
3. **Per-tool scoping — the RBAC part.** Declare reach on the **Tool Definition
   grain**: which credentials it may use, which hosts it may reach, which
   methods it may issue. The run manifest pins that at start, so a plan cannot
   widen its own reach mid-run.

   ```
   zoho_post   → credential: zoho,  hosts: books.zoho.com,      methods: [POST]
   send_email  → credential: graph, hosts: graph.microsoft.com, methods: [POST]
   parse_pdf   → no credential, no egress
   ```

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

1. **Part 1 (`executor_uri`)** — built, landed with this document.
2. **Part 3 (egress)** — mechanism exists, mostly relocation, closes the
   largest blast-radius gap. Smallest remaining risk.
3. **Part 2 (LLM boundary)** — mechanism exists; needs two wiring points, the
   fail-closed rule, and care with verify and replay.

That order optimizes for risk. **If enterprise or compliance work is driving,
invert 2 and 3**: the LLM boundary is what makes [`gdpr.md`](gdpr.md) and
[`eu-ai-act.md`](eu-ai-act.md) true for agent workloads, and it is the thing a
procurement reviewer asks about first.

## 5. What this document does not propose

- **A sandbox.** Isolation does not constrain the failure mode that actually
  occurs (see §1), and claiming otherwise would be worse than not having one.
- **New CAL syntax.** Every part above is expressible in existing grammar or in
  host configuration; new syntax is an OMS conformance decision.
- **A resident process.** Nothing here needs a daemon, and the no-daemon
  decision holds.
