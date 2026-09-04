# tau2-ceiling-2026-09-04 — why no τ² learning number is published

`tau2-native-agent-control.json` is τ²-bench's **own** shipped agent
(`tau2 run`) on retail tasks 20, 21 and 22 with
`qwen/qwen3-30b-a3b-instruct-2507` as both agent and customer, temperature
0, max 50 steps. Average reward **0.0000**: the database check fails on
every scored task while the natural-language assertions pass, and one task
ends at `max_steps`.

That is the control for the bridge in `../../tau2/`. Its FULL arm — the
whole policy, the whole tool descriptions, no lessons — matched this at
zero, so the zero is the model against the domain, not the harness against
itself. With the ceiling at zero there is no headroom a governed loop could
close, so no learning number from this domain is published at this model.

`../../tau2/README.md` has the reasoning and what would make the domain
measurable.
