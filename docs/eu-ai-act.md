# EU AI Act — article → capability map

The same shape as [`gdpr.md`](gdpr.md): each row names the obligation, the
Areev capability that discharges it, and the command that demonstrates it.
Scope note: these obligations bind PROVIDERS/DEPLOYERS of high-risk AI
systems; Areev is infrastructure that makes the obligations demonstrable
for systems built on it. Enforcement for high-risk obligations began
2026-08-02.

| Article | Obligation | Capability | Demonstrate |
|---|---|---|---|
| Art. 12 (record-keeping) | Automatic recording of events over the system's lifetime; logs identify situations that may result in risk | Every run is a journal of immutable, content-addressed grains: intent before dispatch, result as supersession, per-superstep checkpoints and decision records; run-outcome Observations record every terminal state with spend figures. Retention floors + legal holds keep the logs destructible only by stated policy. Only the **guaranteed** mirror/listener mode may be cited (best-effort sheds under load, honestly counted). | `areev run-trace --run-id R`, `areev audit export`, `areev retention floors` |
| Art. 14 (human oversight) | Systems designed so natural persons can effectively oversee them: understand capabilities, decide not to use, intervene or interrupt | Client-gated nodes park the run and hand a `requires_action` envelope to a human; approval separation of duties is structural (responder ≠ triggering principal, refused not documented); the kill switch (`areev run cancel`) is deliberately the LOWEST-privilege verb; every response, including losing/rejected ones, is journaled. | `areev run oversight-report [--plan H \| --run-id R]` — Client-gated nodes, authorized responders, expiry/budget config, measured cancel→drain time |
| Art. 13 (transparency) | Operation sufficiently transparent for deployers to interpret output | Replay is a pure function of (plan, manifest, input, journal); `verify` re-derives every checkpoint and names divergences; `areev tool provenance` chains code → approval → gate → runs. | `areev run verify --run-id R`, `areev tool provenance <hash>` |
| Art. 15 (accuracy, robustness) | Appropriate accuracy and resilience to errors | Evalset gating on code changes (Rule E1: pinned evalset, recorded gating run, failing gate admits nothing), shadow evaluation over journaled runs with zero effect dispatches, outcome measurement with auto-revert proposals. | `areev eval run`, `areev run shadow`, `areev loop outcomes` |

What this map does NOT claim: Areev does not classify your system's risk
tier, does not write your technical documentation (Art. 11), and does not
make an unsafe model safe. It makes the runtime obligations *evidenced* —
each row ends in a command a reviewer can run.
