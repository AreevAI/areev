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

## The corrected ceiling measurement

`ceiling.summary.json` is the probe over 25 held-out tasks under each
condition, with the bridge fixed: **FULL 7/25 solved, REDACTED 9/25**,
paired 2 against 4 discordant at p = 0.69. The domain is reachable and the
withheld clauses cost nothing the reward can see — though they do cost
behaviour, with 42 tool errors against 23 and two runs dying of too many
errors where the full policy had none.

So no τ² learning number is published from this clause set.
`../../tau2/README.md` has the reading and what a working design needs.
