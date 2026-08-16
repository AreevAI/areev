# areev-loop-adapter

The **Areev substrate adapter** for the [`areev-loop`](../areev-loop) engine: it
implements `areev_loop::OmsSubstrate` over `areev_cal::AreevFacade`, so the
governed self-improvement loop runs against real Areev `.mg`/Turso memory
files.

`areev-loop` itself has zero Areev dependencies (it talks to the `OmsSubstrate`
trait). This crate is the glue that binds the two — and, per proposal §10, it
stays in the Areev repo even after the engine is lifted to its own repo, so
Areev remains the reference substrate. The CLI (`areev loop`), server, and
bindings all sit on top of this adapter.

Not published during the churn phase (`publish = false`).

Licensed under MIT OR Apache-2.0.
