# areev-trigger

Standing rules that start workflows for Areev.

`areev-trigger` makes an agent's cadence *data in the memory* rather than a fact
buried in someone's crontab. A rule is declared as a `Trigger` grain and
evaluated by `areev trigger run` — a one-shot, idempotent command that is safe to
invoke concurrently. There is no daemon and no scheduler: the host already runs
something that ticks, and `areev trigger render` emits config for it
(cron/launchd/systemd/k8s-cronjob) without creating anything.

Eight kinds sit on four primitives — `interval`/`schedule`/`once` (time),
`polling` (time + poll), `memory` (a state predicate), `webhook`/`manual` (push),
and `composite` (gates with correlation windows). Idempotency is structural: the
run id is derived from `(trigger, connector, dedup value)`, so a redelivered item
is one run and one recorded skip, and correctness does not rest on the lease.
The first poll seeds the cursor and fires nothing, so declaring a mailbox trigger
does not replay history. Connectors reuse the `--tool-cmd` subprocess contract,
inheriting its timeout, output cap and secret scrub; outbound requests pass
through the credential broker and host allowlist, so a connector is handed a
brokered URL rather than a token. Cron is UTC only — a non-UTC timezone is
refused with `TRG-E006` rather than mishandled across a DST boundary.

Part of [Areev](https://github.com/AreevAI/areev) — an embedded memory engine for AI agents. See the [trigger reference](https://github.com/AreevAI/areev/blob/main/docs/triggers.md) and the [architecture overview](https://github.com/AreevAI/areev/blob/main/ARCHITECTURE.md).

Licensed under MIT OR Apache-2.0.
