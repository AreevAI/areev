# selfimprove report

*config:* `{"agent_cmd":"python3 crates/areev-bench/scripts/openrouter_toolcall.py qwen/qwen3-30b-a3b-instruct-2507","assert_shape":false,"bench":"selfimprove_aba","db":"/private/tmp/claude-501/-Users-sathish-mg-products-areev/948e5ede-40ed-422a-8911-525405666586/scratchpad/aba-live2/bench.db","eval":60,"experience":150,"git_rev":"23a1990ed6b70545afe5192baeff980cbb8cf87a","ground_cmd":"python3 crates/areev-bench/scripts/openrouter_loop.py deepseek/deepseek-chat","llm_cmd":"python3 crates/areev-bench/scripts/openrouter_loop.py qwen/qwen3-30b-a3b-instruct-2507","max_turns":24,"mock":false,"phase_base_ms":1700000000000,"reviewer_actor":"user:bench-reviewer","runner_actor":"agent:bench-runner","seed":1,"workdir":"/private/tmp/claude-501/-Users-sathish-mg-products-areev/948e5ede-40ed-422a-8911-525405666586/scratchpad/aba-live2"}`

## Held-out eval by state

| state | n | success | tool errors | mean steps | tokens |
|---|---|---|---|---|---|
| A0 | 60 | 40.0% (24/60) | 64 | 6.6 | 451912 |
| B | 60 | 51.7% (31/60) | 61 | 7.0 | 510221 |
| A1 | 60 | 38.3% (23/60) | 66 | 6.7 | 459095 |
| B2 | 60 | 53.3% (32/60) | 59 | 6.9 | 509397 |

## Per-rule mishandling recurrence (mishandled/opportunities)

| rule | A0 | B | A1 | B2 |
|---|---|---|---|---|
| R1 | 0/30 | 0/30 | 0/30 | 0/30 |
| R2 | 0/60 | 0/60 | 0/60 | 0/60 |
| R3 | 0/30 | 0/30 | 0/30 | 0/30 |
| R4 | 5/15 | 3/15 | 7/15 | 2/15 |
| R5 | 4/15 | 3/15 | 4/15 | 3/15 |
| R6 | 26/30 | 24/30 | 27/30 | 23/30 |

## Governance ledger

| hash | source | disposition | summary | because |
|---|---|---|---|---|
| 35313eab | loop.tool_failure/1 | applied | Tool "refund" failed 46 times (44% of the calls that could fail this way): {"error":{"code":"rate_limited","message":"rate limited, try again later","retry | bench: recurring failure evidence |
| ceb314bb | loop.tool_failure/1 | applied | Tool "refund" failed 46 times (44% of the calls that could fail this way): {"error":{"code":"rate_limited","message":"rate limited, try again later","retry | bench: recurring failure evidence |

*2 proposed — 2 applied, 0 rejected, 0 advisory, 0 apply_failed.*
