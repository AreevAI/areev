# tau2-ceiling-2026-09-04 — the native-agent control

`tau2-native-agent-control.json` is τ²-bench's **own** shipped agent
(`tau2 run`) on retail tasks 20, 21 and 22 with
`qwen/qwen3-30b-a3b-instruct-2507` as both agent and customer, temperature
0, max 50 steps. Average reward **0.0000**: the database check fails on
every scored task while the natural-language assertions pass, and one task
ends at `max_steps`. That measurement stands.

**What it does not support.** It was briefly cited as evidence that the
domain is out of reach for this model, alongside a FULL arm of our own that
scored 0 of 25. That arm turned out to be a parsing bug in the bridge —
every tool call was dropped, so the agent never acted — and the conclusion
was withdrawn (`../../tau2/README.md`, "Retracted"). Three tasks were never
enough to carry a claim that broad; they were leaned on because they agreed
with a broken arm, which is exactly how a control stops being one.

The corrected ceiling measurement will be published here when it has run.
