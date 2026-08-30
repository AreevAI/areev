# selfimprove report

*config:* `{"agent_cmd":"python3 crates/areev-bench/scripts/openrouter_toolcall.py qwen/qwen3-30b-a3b-instruct-2507","arms":[],"assert_shape":false,"bench":"selfimprove_aba","context_cmd":null,"db":"/private/tmp/claude-501/-Users-sathish-mg-products-areev/ba8b88d8-5a30-46d9-8e5d-969b3b311aa2/scratchpad/pub3/llm-s1/bench.db","eval":100,"experience":300,"git_rev":"39fc35f6ee8e617a42dfdbe234282d614a3a6075","ground_cmd":"python3 crates/areev-bench/scripts/openrouter_loop.py deepseek/deepseek-chat","llm_cmd":"python3 crates/areev-bench/scripts/openrouter_loop.py qwen/qwen3-30b-a3b-instruct-2507","llm_lessons":true,"max_turns":24,"mllm_cmd":null,"mock":false,"mock_llm":false,"phase_base_ms":1700000000000,"reviewer_actor":"user:bench-reviewer","runner_actor":"agent:bench-runner","seed":1,"workdir":"/private/tmp/claude-501/-Users-sathish-mg-products-areev/ba8b88d8-5a30-46d9-8e5d-969b3b311aa2/scratchpad/pub3/llm-s1","workers":4}`

## Held-out eval by state

| state | n | success | tool errors | mean steps | tokens |
|---|---|---|---|---|---|
| A0 | 100 | 44.0% (44/100) | 107 | 6.7 | 757160 |
| B | 100 | 52.0% (52/100) | 76 | 6.8 | 874815 |
| A1 | 100 | 41.0% (41/100) | 107 | 6.7 | 755124 |
| B2 | 100 | 55.0% (55/100) | 73 | 6.7 | 856233 |

## Per-rule mishandling recurrence (mishandled/opportunities)

| rule | A0 | B | A1 | B2 |
|---|---|---|---|---|
| R1 | 0/45 | 0/45 | 0/45 | 0/45 |
| R2 | 0/100 | 0/100 | 0/100 | 0/100 |
| R3 | 0/50 | 0/50 | 0/50 | 0/50 |
| R4 | 11/25 | 10/25 | 13/25 | 8/25 |
| R5 | 7/25 | 0/25 | 9/25 | 0/25 |
| R6 | 39/51 | 40/51 | 42/51 | 37/51 |

## Governance ledger

| hash | source | disposition | summary | because |
|---|---|---|---|---|
| 5e0cc8f4 | llm | applied | Multiple log_case failures due to non-UTC ISO-8601 timestamps suggest a systemic issue with timestamp formatting in the agent's workflow. — record lesson: "Format timestamps as UTC ISO-8601 (YYYY-MM-DDTHH:MM:SSZ) before calling log_case; validate with a standard library or regex." | bench: recurring failure evidence |
| a5551ec0 | llm | applied | Repeated refund failures due to missing approval_token indicate a missing pre-approval step for high-value refunds. — record lesson: "Obtain an approval_token via request_approval before initiating any refund over $100." | bench: recurring failure evidence |
| ddc4a320 | loop.tool_failure/1 | applied | Tool "log_case" failed 60 times (35% of the calls that could fail this way): {"error":{"code":"invalid_timestamp","message":"timestamp must be utc iso-#: | bench: recurring failure evidence |
| 90af56fe | loop.tool_failure/1 | applied | Tool "refund" failed 77 times (35% of the calls that could fail this way): {"error":{"code":"approval_required","message":"refunds over $# require a vali | bench: recurring failure evidence |
| 001b33e0 | llm | applied | The agent repeatedly attempts to log cases with invalid timestamps, but no evidence shows that the agent validates or converts local timestamps to UTC ISO-8601 format before calling log_case. — record lesson: "Convert all timestamps to UTC ISO-8601 (YYYY-MM-DDTHH:MM:SSZ) before calling log_case; validate with a standard library or regex." | bench: recurring failure evidence |
| 6dbd7783 | llm | applied | The agent attempts refunds over $100 without first obtaining an approval_token, and no evidence shows a retry mechanism or fallback when approval is missing. — record lesson: "Obtain an approval_token via request_approval before initiating any refund over $100." | bench: recurring failure evidence |
| cf631650 | loop.tool_failure/1 | applied | Tool "log_case" failed 60 times (35% of the calls that could fail this way): {"error":{"code":"invalid_timestamp","message":"timestamp must be utc iso-#: | bench: recurring failure evidence |
| 4473d94c | loop.tool_failure/1 | applied | Tool "refund" failed 77 times (35% of the calls that could fail this way): {"error":{"code":"approval_required","message":"refunds over $# require a vali | bench: recurring failure evidence |

*8 proposed — 8 applied, 0 rejected, 0 advisory, 0 apply_failed.*
