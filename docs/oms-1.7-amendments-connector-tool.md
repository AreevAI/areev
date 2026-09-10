# OMS 1.7 amendment — `connector_tool` on the Trigger grain

> Companion to [`oms-1.6-amendments-triggers.md`](oms-1.6-amendments-triggers.md),
> which introduced the Trigger grain, and
> [`oms-1.7-amendments-cal-expressiveness.md`](oms-1.7-amendments-cal-expressiveness.md).

**Status:** amendment proposal, written 2026-09-09
([#185](https://github.com/AreevAI/areev/issues/185)). One field.

| # | Amendment | Issue | Conformance impact |
|---|---|---|---|
| C1 | Trigger gains an optional `connector_tool` (compact key `tct`): a grain reference to the Tool **Definition** whose `executor_uri` carries the connector's code | [#185](https://github.com/AreevAI/areev/issues/185) | Trigger field set (§8.13 / §A.7); **no new CAL syntax** |

## What it is

```json
{ "kind": "polling", "workflow": "<plan hash>",
  "connector": "gmail",
  "connector_tool": "<Tool Definition hash>",
  "scope": "mailbox:accounts@example.com",
  "interval_secs": 900, "dedup_key": ["/message_id"] }
```

Read through the same `strip_grain_scheme` rule every grain reference in this
type already uses (`<64 hex>`, `sha256:<hex>` or `grain:sha256:<hex>`), like
`members`. **Omit-default**: absent means the host command polls, which is what
every pre-1.7.4 trigger means, so no existing declaration changes address.

## Why the field is on the Trigger and not somewhere else

The alternative was to leave the connector entirely to host configuration,
which is where it was. That put the code most likely to be wrong — cursors,
pagination, a provider's quirks — outside the memory, so it did not replicate,
`tool provenance` could not chase it, and a governed improvement loop could not
propose a revision against code it cannot see. The declaration is the natural
home: a trigger already says *what* it watches and *how often*; saying *with
which code* is the same kind of statement, and it is the statement that makes a
synced memory self-describing.

## Why it names a Definition and not a blob

A `cas://` blob is bytes. The runtime it executes under, its limits, and above
all the `capabilities` declaration that decides where it may reach are Tool
fields — so a Trigger naming a blob directly would need to carry a parallel
copy of all three, which is a second place for them to disagree. Naming the
Definition keeps one description of what a piece of code is and what it may do,
and lets a connector be superseded and gated exactly like any other tool.

## Why `connector` remains required beside it

`connector` is half the firing identity: the run id is derived from
`(trigger, connector, dedup value)`. If the code's address were the identity,
revising the connector would renumber every run and every dedup fence with it —
turning a code fix into a replay of the source. The name is what a person and
the dedup fence both call it; the reference is what runs.

## What an implementation must do

1. **Serialize and read the field** under compact key `tct`, omitted when
   absent.
2. **Resolve it to a Tool Definition** and reject anything else — a Trigger
   naming an Execution record, or a grain of another type, is a declaration
   that cannot fire.
3. **Require a host-side authorization to execute the code.** This is the part
   that must not be inferred from the file: bundles carry blobs, so a
   declaration that arrived by import brings its connector's code with it, and
   a permission travelling beside the code it authorizes is not a permission.
   Areev spells this `--allow-executor <address>` and refuses with `TRG-E012`.
4. **Apply the Definition's own gates** — runtime, limits, capability
   declaration — exactly as when a workflow node binds that Definition. An
   implementation that ran a connector under looser rules than a node would
   make the trigger path the weak one, which is the path nobody is watching.

Nothing here changes the connector's wire contract: the request and response
JSON are the same for a host command and a grain, which is what lets a
deployment move from one to the other without touching anything else.
