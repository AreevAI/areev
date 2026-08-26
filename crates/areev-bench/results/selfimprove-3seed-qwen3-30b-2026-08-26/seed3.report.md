# selfimprove report

*config:* `{"agent_cmd":"python3 crates/areev-bench/scripts/openrouter_toolcall.py qwen/qwen3-30b-a3b-instruct-2507","arms":["m-steel","m-all","m-llm"],"assert_shape":false,"bench":"selfimprove_aba","context_cmd":null,"db":"/private/tmp/claude-501/-Users-sathish-mg-products-areev/948e5ede-40ed-422a-8911-525405666586/scratchpad/run3-s3/bench.db","eval":100,"experience":300,"git_rev":"5aac29b167be9af558050da725785f529b8ce52e","ground_cmd":"python3 crates/areev-bench/scripts/openrouter_loop.py deepseek/deepseek-chat","llm_cmd":"python3 crates/areev-bench/scripts/openrouter_loop.py qwen/qwen3-30b-a3b-instruct-2507","max_turns":24,"mllm_cmd":"python3 crates/areev-bench/scripts/openrouter_toolcall.py qwen/qwen3-30b-a3b-instruct-2507","mock":false,"phase_base_ms":1700000000000,"reviewer_actor":"user:bench-reviewer","runner_actor":"agent:bench-runner","seed":3,"workdir":"/private/tmp/claude-501/-Users-sathish-mg-products-areev/948e5ede-40ed-422a-8911-525405666586/scratchpad/run3-s3","workers":6}`

## Held-out eval by state

| state | n | success | tool errors | mean steps | tokens |
|---|---|---|---|---|---|
| A0 | 100 | 40.0% (40/100) | 99 | 6.5 | 737933 |
| B | 100 | 59.0% (59/100) | 83 | 6.8 | 829764 |
| A1 | 100 | 40.0% (40/100) | 102 | 6.5 | 734967 |
| B2 | 100 | 57.0% (57/100) | 88 | 6.8 | 834942 |
| M-steel | 100 | 63.0% (63/100) | 119 | 6.7 | 1077487 |
| M-all | 100 | 58.0% (58/100) | 74 | 6.9 | 5116265 |
| M-llm | 100 | 57.0% (57/100) | 65 | 6.2 | 1052883 |

## Per-rule mishandling recurrence (mishandled/opportunities)

| rule | A0 | B | A1 | B2 | M-steel | M-all | M-llm |
|---|---|---|---|---|---|---|---|
| R1 | 0/42 | 0/42 | 0/42 | 0/42 | 0/42 | 0/42 | 0/42 |
| R2 | 0/100 | 0/100 | 0/100 | 0/100 | 0/100 | 0/100 | 0/100 |
| R3 | 0/50 | 0/50 | 0/50 | 0/50 | 0/50 | 0/50 | 0/50 |
| R4 | 13/25 | 2/25 | 14/25 | 4/25 | 23/25 | 0/25 | 0/25 |
| R5 | 10/25 | 5/25 | 10/25 | 7/25 | 9/25 | 5/25 | 0/25 |
| R6 | 37/41 | 34/41 | 36/41 | 32/41 | 15/41 | 36/41 | 36/41 |

## Governance ledger

| hash | source | disposition | summary | because |
|---|---|---|---|---|
| 1a8e54fb | loop.tool_failure/1 | applied | Tool "log_case" failed 70 times (36% of the calls that could fail this way): {"error":{"code":"invalid_timestamp","message":"timestamp must be utc iso-#: | bench: recurring failure evidence |
| 52293b7a | loop.tool_failure/1 | applied | Tool "refund" failed 78 times (38% of the calls that could fail this way): {"error":{"code":"approval_required","message":"refunds over $# require a vali | bench: recurring failure evidence |
| fe3d63dc | loop.tool_failure/1 | applied | Tool "log_case" failed 70 times (36% of the calls that could fail this way): {"error":{"code":"invalid_timestamp","message":"timestamp must be utc iso-#: | bench: recurring failure evidence |
| 790cb907 | loop.tool_failure/1 | applied | Tool "refund" failed 78 times (38% of the calls that could fail this way): {"error":{"code":"approval_required","message":"refunds over $# require a vali | bench: recurring failure evidence |

*4 proposed — 4 applied, 0 rejected, 0 advisory, 0 apply_failed.*
