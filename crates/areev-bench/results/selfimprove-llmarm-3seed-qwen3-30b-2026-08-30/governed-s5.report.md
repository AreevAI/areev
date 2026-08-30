# selfimprove report

*config:* `{"agent_cmd":"python3 crates/areev-bench/scripts/openrouter_toolcall.py qwen/qwen3-30b-a3b-instruct-2507","arms":[],"assert_shape":false,"bench":"selfimprove_aba","context_cmd":null,"db":"/private/tmp/claude-501/-Users-sathish-mg-products-areev/ba8b88d8-5a30-46d9-8e5d-969b3b311aa2/scratchpad/pub3/governed-s5/bench.db","eval":100,"experience":300,"git_rev":"39fc35f6ee8e617a42dfdbe234282d614a3a6075","ground_cmd":"python3 crates/areev-bench/scripts/openrouter_loop.py deepseek/deepseek-chat","llm_cmd":"python3 crates/areev-bench/scripts/openrouter_loop.py qwen/qwen3-30b-a3b-instruct-2507","llm_lessons":false,"max_turns":24,"mllm_cmd":null,"mock":false,"mock_llm":false,"phase_base_ms":1700000000000,"reviewer_actor":"user:bench-reviewer","runner_actor":"agent:bench-runner","seed":5,"workdir":"/private/tmp/claude-501/-Users-sathish-mg-products-areev/ba8b88d8-5a30-46d9-8e5d-969b3b311aa2/scratchpad/pub3/governed-s5","workers":4}`

## Held-out eval by state

| state | n | success | tool errors | mean steps | tokens |
|---|---|---|---|---|---|
| A0 | 100 | 52.0% (52/100) | 101 | 6.6 | 740129 |
| B | 100 | 76.0% (76/100) | 90 | 6.9 | 848675 |
| A1 | 100 | 54.0% (54/100) | 104 | 6.5 | 734406 |
| B2 | 100 | 75.0% (75/100) | 98 | 6.9 | 844405 |

## Per-rule mishandling recurrence (mishandled/opportunities)

| rule | A0 | B | A1 | B2 |
|---|---|---|---|---|
| R1 | 0/44 | 0/44 | 0/44 | 0/44 |
| R2 | 0/100 | 0/100 | 0/100 | 0/100 |
| R3 | 0/50 | 0/50 | 0/50 | 0/50 |
| R4 | 16/25 | 5/25 | 17/25 | 10/25 |
| R5 | 7/25 | 4/25 | 8/25 | 5/25 |
| R6 | 29/40 | 16/40 | 26/40 | 14/40 |

## Governance ledger

| hash | source | disposition | summary | because |
|---|---|---|---|---|
| a270953b | llm | advisory | The agent repeatedly fails to format timestamps in UTC ISO-8601 (YYYY-MM-DDTHH:MM:SSZ) when calling log_case, indicating a systemic issue with timestamp generation or validation. — record lesson: "Format all timestamps as UTC ISO-8601 (YYYY-MM-DDTHH:MM:SSZ) before invoking log_case." | bench: advisory finding, not executable |
| 342e31f4 | loop.tool_failure/1 | applied | Tool "log_case" failed 71 times (39% of the calls that could fail this way): {"error":{"code":"invalid_timestamp","message":"timestamp must be utc iso-#: | bench: recurring failure evidence |
| 829e40e3 | loop.tool_failure/1 | applied | Tool "refund" failed 87 times (37% of the calls that could fail this way): {"error":{"code":"approval_required","message":"refunds over $# require a vali | bench: recurring failure evidence |
| a270953b | llm | advisory | The agent repeatedly fails to format timestamps in UTC ISO-8601 (YYYY-MM-DDTHH:MM:SSZ) when calling log_case, indicating a systemic issue with timestamp generation or validation. — record lesson: "Format all timestamps as UTC ISO-8601 (YYYY-MM-DDTHH:MM:SSZ) before invoking log_case." | bench: advisory finding, not executable |
| dd2206f5 | loop.tool_failure/1 | applied | Tool "log_case" failed 71 times (39% of the calls that could fail this way): {"error":{"code":"invalid_timestamp","message":"timestamp must be utc iso-#: | bench: recurring failure evidence |
| 39939446 | loop.tool_failure/1 | applied | Tool "refund" failed 87 times (37% of the calls that could fail this way): {"error":{"code":"approval_required","message":"refunds over $# require a vali | bench: recurring failure evidence |

*6 proposed — 4 applied, 0 rejected, 2 advisory, 0 apply_failed.*
