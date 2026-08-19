# OMS 1.6 amendments — the trigger batch

**Status:** amendment proposal, written 2026-08-19. Shipped in Areev ahead of
ratification; this is the record of what an implementation must do to
interoperate.

Companion to [`oms-1.6-amendments.md`](oms-1.6-amendments.md) (the compliance
batch). Filed against `openmemoryspec/oms#12 §4`, which asked for a trigger
execution contract. This proposes something larger than that issue did, and
§T1 explains why.

| # | Amendment | Shipped? | Conformance impact |
|---|---|---|---|
| T1 | New grain type: `Trigger` (0x0D), §8.13 | **Yes** (1.3.0) | New type byte; `min_reader_version` |
| T2 | Delete §27.6 (triggers as Observations) | **Yes** | Removes a convention |
| T3 | Remove `Workflow.trigger` from §8.4 | **Yes** | Breaking field removal |
| T4 | A.7 `int:allowed_outbound_hosts` | **Yes** | Additive profile key |
| T5 | §27.8 trigger execution contract | Proposed | New normative section |
| T6 | A `triggers` conformance module | Proposed | New declaration |

---

## T1 — `Trigger` becomes a grain type (0x0D)

§27.6 says *"triggers are observers. No new grain type is required."* That
claim rests on an assertion — *"existing Observation fields accommodate trigger
definitions"* — which is false three ways.

**1. The convention is non-conformant against the spec's own closed enums.**
§27.6 puts `"periodic"` / `"continuous"` / `"scheduled"` in `observation_mode`,
which §25 declares **closed** over `passive | active | reflective | real_time`.
It puts a watch target like `"repos/{owner}/{repo}/issues"` in
`observation_scope`, which §26 declares **closed** over the *temporal breadth*
values `point | interval | session | longitudinal`. Every example in §27.6 would
be rejected by a Level 2 implementation enforcing §25 and §26.

The reference implementation then defines a third vocabulary again
(`realtime | batch | streaming`, `private | shared | public`). No two of the
three agree.

**2. Configuration in `context` cannot be queried.** `context` is not among
Observation's queryable fields, and a conformant field lookup is top-level.
`RECALL observations WHERE int:connector = "gmail"` does not merely fail — in
the reference implementation it **fails open**: an unrecognised `WHERE` field is
dropped from push-down with a warning and the query silently returns everything.
Moving the keys top-level would make them queryable but places them outside
`context`, which is where A.7 says `int:` fields live. The two requirements are
mutually exclusive.

**3. A trigger is not an observation.** §24 splits observers into Physical
("measurements of the material world") and Cognitive ("observations of the
information space"). A standing rule is neither, and registering `trigger:*`
would require inventing a third domain purely to make the classification work.

### §8.13 Trigger (type = 0x0D)

**Required:** `type` = "trigger", `kind`, `workflow`, `created_at`.

| Field | Type | Notes |
|---|---|---|
| `kind` | string | `interval\|schedule\|once\|polling\|memory\|webhook\|manual\|composite` |
| `workflow` | string | content address of the Workflow to start |
| `connector` | string | connector name; half of the firing identity |
| `scope` | string | what is watched |
| `enabled` | bool | omitted when true |
| `dedup_key` | string[] | JSON pointers, joined in order |
| `interval_secs` | int | |
| `cron` | string | |
| `at_ms` | int64 | |
| `predicate` | map | a serialized filter expression |
| `members` | map[string→string] | alias → trigger address, for `composite` |
| `correlate` | string | JSON pointer |
| `window_ms` | int64 | |
| `concurrency` | string | `forbid\|allow\|replace` |
| `catchup` | string | `last\|none\|all` |
| `config` | map | A.7 `int:` connector transport config |

**Binding direction is normative: a trigger names its workflow, never the
reverse.** A Workflow is content-addressed; a plan carrying a list of triggers
would change address whenever one was added, invalidating every reference to it.

**Members carry aliases** because a gate expression references them by name and
a 64-hex content address is not a legal identifier in any expression grammar.

## T2 — delete §27.6

Replaced by §8.13. Not marked `[DEPRECATED]`: preserving a documented convention
that is non-conformant against §25 and §26 is worse than removing it, because
the surviving text would keep telling implementers to write invalid grains.

## T3 — remove `Workflow.trigger` from §8.4

A free-text "activation condition" that no implementation ever evaluated. In the
reference implementation neither the scheduler nor the driver read it, so it
described an activation condition that could not activate anything while the
console offered to set it. §8.13 replaces it, in the only direction that works.

Breaking. Old blobs still deserialize — an unknown field is preserved and
ignored — so this costs a vestigial key in grains already written, and nothing
else.

## T4 — A.7 `int:allowed_outbound_hosts`

| Field | Type | Used by | Description |
|---|---|---|---|
| `int:allowed_outbound_hosts` | string[] | all | URL prefixes a connector may reach |

Fermyon Spin's semantics: scheme, host and port all take part in the match;
`*.example.com` covers subdomains but not the apex; an absent key is
unrestricted; an empty array denies everything. A bare `*` SHOULD be refused —
it lets a declaration appear policed while permitting the whole internet.

The existing nine `int:` trigger keys (`int:cron_expression`,
`int:poll_interval_secs`, `int:cursor_field`, `int:cursor_type`,
`int:webhook_path`, `int:webhook_secret_header`, `int:timezone`,
`int:config_schema`, `int:event_schema`) stay valid and keep their compact keys.
Their scope narrows: connector transport config, not the primary fields of a
trigger.

## T5 — §27.8, the execution contract

§27.6 defined a declaration format and stopped. Nothing normative said what
evaluates a declaration, how a firing becomes a grain, or what idempotency a
twice-delivered item gets. Proposed minimum:

1. **A firing MUST be journaled** — what fired, what it produced, what it
   skipped. A trigger that has never fired MUST be distinguishable from one with
   nothing to do.
2. **Item ingestion MUST be idempotent on the declared dedup key.** An
   implementation SHOULD derive the work identity from
   `(trigger, connector, dedup value)` so a re-delivered item is a recorded skip
   rather than duplicated work. Following CloudEvents, the producer identity is
   part of the key: `id` alone is unique only within a producer.
3. **First contact seeds and MUST NOT replay history.** A newly declared polling
   trigger records the source's current position and fires nothing.
4. **Evaluation state MUST NOT replicate.** Cursor, lease and watermark are
   per-host usage. Replicating them lets synced hosts ping-pong on each other's
   watermark, and lets a restored memory inherit a cursor and silently skip work.
5. **Multi-writer stores MUST arbitrate through a conditional write**, and a
   holder whose lease expired MUST NOT be able to write behind its successor.

## T6 — a `triggers` conformance module

Mirroring `cal_tiers`: declared, implemented whole or not at all, and explicitly
not a portability requirement. Not every OMS store runs triggers; one that does
should implement the whole contract and say so.

```json
{ "oms_conformance": 3, "oms_modules": ["triggers"] }
```
