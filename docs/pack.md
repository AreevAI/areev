# Packs — `areev pack validate | install | export`

A **pack** is an installable agent: a manifest, the grains it seeds, and the
code blobs those grains name. It exists because everyone deploying an agent was
otherwise writing the same installer — put the blobs in the CAS, rewrite each
`executor_uri` to the address the bytes turned out to have, seed the grains in
dependency order, and check that the plan came out at the hash the deployment
expected. That installer is now one verb, and self-hosters get the same door a
managed deployment does ([#178](https://github.com/AreevAI/areev/issues/178)).

```bash
areev pack validate ./pack                     # opens no memory at all
areev pack install  ./pack --db agent.db       # refuses on a hash mismatch
areev pack export   --db agent.db --ns ops --out ./pack
```

## The manifest

```json
{
  "pack": "invoice-to-accounting",
  "version": "1.0.0",
  "description": "Reads an AP mailbox, extracts, posts, asks a human.",
  "namespace": "ap",
  "blobs": { "poll": "blobs/mailbox.poll.wasm", "feed": "fixtures/mail.json" },
  "grains": [
    "grains/010-tool-poll.json",
    { "file": "grains/020-workflow.json", "expected_hash": "8f2c…" }
  ],
  "queries":   { "triage_ctx": { "body": "RECALL facts WHERE …" } },
  "templates": { "brief": { "body": "…" } },
  "host":      { "config_schema": { "type": "object" }, "fixtures": ["…"] }
}
```

A grain file is one JSON object in the same shape `ADD … WITH JSON` and the
bindings' `add` take, built by the **same builder**
(`areev_cal::json_build::build_grain_from_json`). That is deliberate: a pack
that authored grains its own way would be a fourth set of semantics for what a
Workflow is, and the one that drifts is always the one only a deployment path
exercises.

`namespace` fills in for any grain that does not name one. `queries` and
`templates` are the file-truths that are not grains — a pack without them
installs a trigger whose `context_query` names a query that is not there.

**Unknown top-level keys are warned, not dropped in silence** (1.9.0, #316).
A misspelled `"templats"` used to validate with `"ok": true, "warnings": []`
and install nothing — the exact failure the paragraph above warns about, with
no symptom. `validate` and `install` now list every key this build does not
read. They warn rather than refuse, so an existing pack keeps installing.

**`"host"` is reserved for the host and never interpreted.** It comes back
verbatim in the validate/install report, and no future manifest key will be
added inside it — so a product's per-install configuration schema and its
fixture metadata live in the pack instead of in a second artifact with its
own version number. (A first-class `config_schema` that Areev interprets is
deliberately NOT offered: install-time configuration cannot flow into hashed
grains without breaking `expected_hash`.)

**An evalset shipped in a pack is checked.** A `fact` grain with relation
`mg:evalset`, referenced from a Tool Definition as
`"evalset_hash": "grain:<id>"`, is how a pack carries its own gating set.
`pack validate` used to check only tools, so an evalset whose cases had
neither `input` nor `expect` addressed cleanly although `areev eval create`
would have refused it. Both now share ONE validator, so they cannot drift.

## From Rust — `areev::pack`

`areev pack validate|install` are **printers** over a library (1.9.0,
#315). A Rust service
that provisions a memory per tenant and installs versioned agent packs no
longer has to ship the binary into its image, spawn
`areev pack install … --format json`, parse the stdout and map string errors
back to causes:

```rust
use areev::pack::{validate_pack, install_pack, InstallOptions, PackError};

// CI: no store at all — content addressing needs no memory.
let report = validate_pack(Path::new("packs/invoice-to-accounting"))?;

// Provisioning: through the CALLER's facade, so the install runs under
// whatever principal the service bound.
let report = install_pack(&facade, dir, &InstallOptions::default())?;
```

`export` is deliberately **not** in the library: it is an authoring step a
person runs against a memory they own, not something a provisioning path
does per tenant, and it writes a directory tree rather than returning a
value. `areev pack export` stays the way to build one.

Two properties the subprocess form could not give:

- **It runs under the caller's bound principal.** The previous entry point
  consumed an owner `Areev`, which a host with an already-open handle could
  not supply at all. A principal without `write` on the pack's namespace now
  gets `AUT-E001` and the memory's op-log is unchanged.
- **The grains are written all-or-nothing** (`cal_add_batch`). Writing one
  at a time meant a pack refused halfway had already seeded the tools of an
  agent whose plan never arrived.

Refusals are typed (`PCK-E001`–`PCK-E004`; see
[`ERROR_CODES.md`](../ERROR_CODES.md)), so a host branches on the CAUSE — an
`expected_hash` mismatch is a deployment decision, a dangling reference is an
authoring bug, and a store refusal is neither. Store and authorization errors
pass through unchanged. `PackReport` carries exactly the fields
`--format json` prints.

## References are symbolic, because an address is a measurement

Two kinds, resolved at install:

| Reference | Resolves to | Why it cannot be written by hand |
|---|---|---|
| `"blob:<name>"` | `cas://sha256:<hex>` of that file's bytes | the address IS the bytes; anything typed is a claim that can be wrong |
| `"grain:<id>"` | the content address the named grain builds to | a plan binds tools by hash and a trigger names a plan by hash, and none of those hashes exist until the grain is built |

A grain's `id` defaults to its file stem, so most packs never write one.
Both rewrites are recursive over the whole document — one rule that reaches an
`executor_uri` at the top level, a `bindings` map, and a `config` value alike,
rather than a table of field names to keep in step with the grain types.

Grains are addressed **in manifest order**, and each one's address becomes
available to the ones after it. A forward reference is refused rather than
guessed at, which is why `grains` is an ordered list and not a set.

## `expected_hash` is refused, not warned

A pack that installs a plan at a different address than the deployment expects
has changed what runs — and everything pointing at the old hash, **triggers
above all** (they do not follow heads), is now pointing somewhere else. Install
stops before writing anything.

```
$ areev pack install ./pack --db agent.db
areev: this pack does not build what its manifest says it builds:
  grains/020-workflow.json (workflow): expected 8f2c…, builds to 41ab…
A content address covers the whole grain, so a mismatch means the declaration
changed — including any code blob it names, since the address of the bytes is
part of the plan. Nothing was written.
```

Note the second half of that sentence: because a Definition names its code by
address and a plan binds that Definition by address, **editing the connector's
wasm changes the plan's hash**. That is the property that makes the check worth
having.

## Install is validate plus a destination

Every grain is built and addressed with **no store at all** — content
addressing is a pure function of the serialized grain — the expectations are
checked, and only then is anything written. So:

- `pack validate` opens no memory, creates no file, and can run in CI on a pull
  request that only touched a pack;
- `pack install` cannot install something `validate` called invalid;
- a refused pack leaves the memory exactly as it found it. A half-installed
  agent is worse than an uninstalled one.

Install then re-asserts, per grain, that the address the pure build predicted
is the address the store recorded. It cannot differ — both are SHA-256 of the
same bytes — and it is checked anyway, because the day it does differ is the
day `validate` stopped speaking for `install`.

## Nothing code-carrying runs until the host pins it

Install prints the pins and so does validate:

```
Nothing code-carrying runs until this host pins it:
  --allow-executor 2a8f2d40…
```

The list is exactly the addresses some Definition names as its `executor_uri` —
never the data blobs a pack also carries (a fixture feed, a rule table, a
document a tool reads by address). Asking an operator to authorize data as
executable would be asking for the wrong permission.

The pin lives on the host, never in the pack, for the reason it never lives in
a bundle: a permission arriving with the code it authorizes is not a
permission. See `docs/run.md`, "Code-carrying tools".

## Export

```bash
areev pack export --db agent.db --ns ops --out ./pack   # --pack-format source (default)
areev pack export --db agent.db --ns ops --out ./pack --pack-format bundle
```

**`source`** writes reviewable grain JSON, one file per grain, ordered
tool → skill → fact → workflow → trigger (the order a reader follows, and the
order references resolve in). It exports what an agent **is** — definitions,
plans, standing rules, seed knowledge — and deliberately not the record of
anything that ran: journal Tool grains, Events, Observations and run State
belong to the memory that produced them.

Before writing the manifest it **proves the round trip**: each grain is rebuilt
from the document it just wrote and must address identically. Anything that
does not is refused, with the advice to export a bundle instead — finding that
out at install time on someone else's machine is the failure this check exists
to prevent.

**`bundle`** writes `pack.mgb` (the ordinary replication artifact — it carries
blobs, saved queries and retention policies) plus a manifest of the plans and
triggers it must contain. Install imports it and asserts each one is present:
a bundle replicates ops, an expectation names a content address, and the two
disagreeing means the bundle was cut from a different memory than the manifest
describes.

## The ten example agents ship one

Every agent under `examples/agents/` carries a `pack/` exported from its own
freshly seeded memory (`examples/agents/export-packs.sh` regenerates them), and
`run-smokes.sh` installs each one and asserts it carries the same plan the
language stacks mint. That is what goes stale when a seeder changes and the
packs were not regenerated — and it fails loudly rather than installing last
week's agent.

Two hand-written packs are the counterpart: `examples/grain-connector/pack/`
(four grains, two blobs, a trigger whose connector is one of them) and
`examples/blessed-tools/pack/` (a Definition binding the shipped `http.call`
blob with declared hosts). Neither contains a hash typed by a human.
