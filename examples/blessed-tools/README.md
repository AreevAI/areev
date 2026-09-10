# The gateway is a declaration, not a program

**The problem.** Every agent needs an outbound HTTP leg, and everyone writes
one. That code then decides *where a request may go* — which makes it a policy
engine that has to be read, versioned and trusted by everyone who installs it,
and that has to be re-audited every time it changes.

**What this shows.** The blessed `http.call` blob
([#179](https://github.com/AreevAI/areev/issues/179)) makes no such decision.
It hands the request to the broker and hands the answer back, verbatim, in both
directions. Where it may go, which method it may use, which credential it may
spend and which headers it may set are the Definition's `capabilities` and the
host's grant — **data, in the memory**, auditable without reading any code.

```
run input ──▶ areev-sandbox ──areev::fetch──▶ broker ────────▶ upstream
{"url": …}     no socket, no env              holds the token
               2.6 KB, no deps                enforces declared ∩ granted
                                              journals every call and refusal
```

## Run it

```bash
cargo build -p areev
cargo build --manifest-path areev-sandbox/Cargo.toml
examples/blessed-tools/run.sh
```

Offline: `stub-vendor.py` is a nine-line upstream on loopback that **401s
anything without the exact bearer token**, so the 200 in step 2 is the proof
that the broker attached a credential the tool never held.

Four steps, and each one is an assertion:

1. **install** — the pack's Definition binds the published `http.call` address
   (the script checks it against `areev-tools/dist/blessed.json`, so a rebuilt
   blob fails here rather than silently running different code);
2. **a permitted call** — 200, with the invoice, through the declared host,
   method, path prefix and credential;
3. **an undeclared host** — refused by the **broker** with `RUN-E022`, nothing
   sent, the refusal journaled in the memory;
4. **provenance** — `areev tool provenance <definition>` chains the blob:
   present, 2,598 bytes, at the address the Definition names.

## The declaration is the whole policy

```json
"capabilities": [
  { "http": { "hosts": ["http://127.0.0.1:7788"],
              "methods": ["GET"],
              "path_prefixes": ["/v1/invoices/"],
              "credentials": ["vendor"],
              "headers": ["X-Api-Version"] } }
]
```

Read it as a **pairing**, not five independent lists: a call must be admitted
by a single block in full. That is what stops a two-service tool from sending
one service's secret to the other. Several blocks are alternatives, so writing
more can only narrow.

And it only ever narrows what the host already allowed. The effective reach is
`declared ∩ granted`, checked per call:

```bash
areev run start … \
  --allow-executor 6c088ed0…          # this host will run this code
  --sandbox-cmd areev-sandbox         # under these limits
  --credential vendor=VENDOR_TOKEN    # holding this secret, which the tool never sees
  --allow-host http://127.0.0.1:7788  # and may reach only here
  --tool-egress 'vendor_api:vendor:GET'
```

Drop `--allow-host` and the declaration alone does not grant reach. Drop
`--allow-executor` and nothing runs at all. **A declaration replicates with the
memory; the authority does not** — which is the point: a bundle can hand you a
tool, and it cannot hand you permission to run it.

## Two Definitions, one blob

The same address can be bound twice with different declarations — one per
service — so a fleet reviews *one* blob and writes *N* policies. That is the
shape the tool-gateway pattern wanted all along, with the part that changes
often (policy) in data and the part that changes rarely (transport) pinned by
address.

## Where to go next

- [`docs/blessed-tools.md`](../../docs/blessed-tools.md) — the three tools, their contracts and addresses
- [`docs/pack.md`](../../docs/pack.md) — packs: `blob:`/`grain:` references, `expected_hash`
- [`docs/run.md`](../../docs/run.md) — capability tools: the full table of what is enforced where
- [`examples/grain-connector/`](../grain-connector/) — the same tier on the trigger path
