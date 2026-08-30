# selfimprove report

*config:* `{"agent_cmd":"python3 crates/areev-bench/scripts/openrouter_toolcall.py qwen/qwen3-30b-a3b-instruct-2507","arms":[],"assert_shape":false,"bench":"selfimprove_aba","context_cmd":null,"db":"/private/tmp/claude-501/-Users-sathish-mg-products-areev/ba8b88d8-5a30-46d9-8e5d-969b3b311aa2/scratchpad/pub3/llm-s3/bench.db","eval":100,"experience":300,"git_rev":"39fc35f6ee8e617a42dfdbe234282d614a3a6075","ground_cmd":"python3 crates/areev-bench/scripts/openrouter_loop.py deepseek/deepseek-chat","llm_cmd":"python3 crates/areev-bench/scripts/openrouter_loop.py qwen/qwen3-30b-a3b-instruct-2507","llm_lessons":true,"max_turns":24,"mllm_cmd":null,"mock":false,"mock_llm":false,"phase_base_ms":1700000000000,"reviewer_actor":"user:bench-reviewer","runner_actor":"agent:bench-runner","seed":3,"workdir":"/private/tmp/claude-501/-Users-sathish-mg-products-areev/ba8b88d8-5a30-46d9-8e5d-969b3b311aa2/scratchpad/pub3/llm-s3","workers":4}`

## Held-out eval by state

| state | n | success | tool errors | mean steps | tokens |
|---|---|---|---|---|---|
| A0 | 100 | 51.0% (51/100) | 105 | 6.6 | 738510 |
| B | 100 | 70.0% (70/100) | 94 | 6.9 | 848021 |
| A1 | 100 | 51.0% (51/100) | 108 | 6.6 | 743957 |
| B2 | 100 | 65.0% (65/100) | 79 | 7.0 | 891819 |

## Per-rule mishandling recurrence (mishandled/opportunities)

| rule | A0 | B | A1 | B2 |
|---|---|---|---|---|
| R1 | 0/42 | 0/42 | 0/42 | 0/42 |
| R2 | 0/100 | 0/100 | 0/100 | 0/100 |
| R3 | 0/50 | 0/50 | 0/50 | 0/50 |
| R4 | 11/25 | 4/25 | 13/25 | 10/25 |
| R5 | 11/25 | 7/25 | 12/25 | 0/25 |
| R6 | 29/41 | 18/41 | 27/41 | 22/41 |

## Governance ledger

| hash | source | disposition | summary | because |
|---|---|---|---|---|
| 3ddea9da | loop.tool_failure/1 | applied | Tool "log_case" failed 68 times (36% of the calls that could fail this way): {"error":{"code":"invalid_timestamp","message":"timestamp must be utc iso-#: | bench: recurring failure evidence |
| 1c5d06e6 | loop.tool_failure/1 | applied | Tool "refund" failed 75 times (36% of the calls that could fail this way): {"error":{"code":"approval_required","message":"refunds over $# require a vali | bench: recurring failure evidence |
| 9f40c76e | llm | applied | Timestamps in log_case calls are consistently malformed, suggesting a systemic issue with timestamp formatting in the agent's data pipeline. — record lesson: "Format all timestamps as UTC ISO-8601 (YYYY-MM-DDTHH:MM:SSZ) before calling log_case." | bench: recurring failure evidence |
| 294c1212 | loop.tool_failure/1 | applied | Tool "log_case" failed 68 times (36% of the calls that could fail this way): {"error":{"code":"invalid_timestamp","message":"timestamp must be utc iso-#: | bench: recurring failure evidence |
| 1fd075b0 | loop.tool_failure/1 | applied | Tool "refund" failed 75 times (36% of the calls that could fail this way): {"error":{"code":"approval_required","message":"refunds over $# require a vali | bench: recurring failure evidence |

*5 proposed — 5 applied, 0 rejected, 0 advisory, 0 apply_failed.*
