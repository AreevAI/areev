# Areev Loop policy variants

`loop-policy.json` is the **only** place auto-apply is granted, and it is
host config — never persisted in a memory file. Pass it with
`areev loop --policy <file>` (or `$AREEV_LOOP_POLICY`). It rejects unknown keys,
so a stolen or committed policy file is inert (it cannot register an
executable). `areev loop policy` prints the effective policy.

> The schema rejects unknown keys, so these files carry **no comment keys** —
> the explanation lives here in prose.

### `solo.json` — the shape-teacher

Grants nothing. Auto-apply is off; every recommendation waits for review. This
is the right default for a solo developer: the value is the receipts
(evidence, reasons, audit, undo), not unattended changes.

### `team.json` — auto-apply structural curation

Turns auto-apply on and grants it for two **structural** analyzers on
**memory** targets: `duplicate_sweep` (up to `low` severity) and
`fork_surfacing` (up to `medium`). Both propose SUPERSEDE-only curation — no
new text, no destruction — so the engine's shape check passes. The team still
reviews everything else (contradictions, tool-failure lessons, staleness).

Note what you **cannot** grant, by construction: anything carrying
evidence-derived free text (tool-failure lessons — they are `ADD`s), prompt or
host targets, or destructive `FORGET`s (staleness). The engine rejects those
even if you name them.

### `locked-down.json` — production floor

Auto-apply off; `staleness` denied entirely (no automatic expiry proposals);
severity floors raise the bar so only high-severity contradictions and
medium+ tool failures surface; telemetry off. The policy file lives in your
repo, so changes to agent autonomy go through code review and git history.
