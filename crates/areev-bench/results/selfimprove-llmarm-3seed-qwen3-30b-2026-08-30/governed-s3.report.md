# selfimprove report

*config:* `{"agent_cmd":"python3 crates/areev-bench/scripts/openrouter_toolcall.py qwen/qwen3-30b-a3b-instruct-2507","arms":[],"assert_shape":false,"bench":"selfimprove_aba","context_cmd":null,"db":"/private/tmp/claude-501/-Users-sathish-mg-products-areev/ba8b88d8-5a30-46d9-8e5d-969b3b311aa2/scratchpad/pub3/governed-s3/bench.db","eval":100,"experience":300,"git_rev":"39fc35f6ee8e617a42dfdbe234282d614a3a6075","ground_cmd":"python3 crates/areev-bench/scripts/openrouter_loop.py deepseek/deepseek-chat","llm_cmd":"python3 crates/areev-bench/scripts/openrouter_loop.py qwen/qwen3-30b-a3b-instruct-2507","llm_lessons":false,"max_turns":24,"mllm_cmd":null,"mock":false,"mock_llm":false,"phase_base_ms":1700000000000,"reviewer_actor":"user:bench-reviewer","runner_actor":"agent:bench-runner","seed":3,"workdir":"/private/tmp/claude-501/-Users-sathish-mg-products-areev/ba8b88d8-5a30-46d9-8e5d-969b3b311aa2/scratchpad/pub3/governed-s3","workers":4}`

## Held-out eval by state

| state | n | success | tool errors | mean steps | tokens |
|---|---|---|---|---|---|
| A0 | 100 | 42.0% (42/100) | 107 | 6.5 | 728266 |
| B | 100 | 67.0% (67/100) | 95 | 6.9 | 845205 |
| A1 | 100 | 44.0% (44/100) | 114 | 6.7 | 751398 |
| B2 | 100 | 66.0% (66/100) | 95 | 6.8 | 834425 |

## Per-rule mishandling recurrence (mishandled/opportunities)

| rule | A0 | B | A1 | B2 |
|---|---|---|---|---|
| R1 | 0/42 | 0/42 | 0/42 | 0/42 |
| R2 | 0/100 | 0/100 | 0/100 | 0/100 |
| R3 | 0/50 | 0/50 | 0/50 | 0/50 |
| R4 | 16/25 | 6/25 | 16/25 | 6/25 |
| R5 | 11/25 | 6/25 | 11/25 | 6/25 |
| R6 | 33/41 | 22/41 | 31/41 | 22/41 |

## Governance ledger

| hash | source | disposition | summary | because |
|---|---|---|---|---|
| 69040cbe | llm | advisory | Timestamps in log_case calls are consistently malformed, suggesting a systemic issue with timestamp formatting in the agent's code or configuration. — record lesson: "Format all timestamps as UTC ISO-8601 (YYYY-MM-DDTHH:MM:SSZ) before passing them to log_case." | bench: advisory finding, not executable |
| 137c3e1c | loop.tool_failure/1 | applied | Tool "log_case" failed 68 times (38% of the calls that could fail this way): {"error":{"code":"invalid_timestamp","message":"timestamp must be utc iso-#: | bench: recurring failure evidence |
| 6378059c | loop.tool_failure/1 | applied | Tool "refund" failed 77 times (37% of the calls that could fail this way): {"error":{"code":"approval_required","message":"refunds over $# require a vali | bench: recurring failure evidence |
| 69040cbe | llm | advisory | Timestamps in log_case calls are consistently malformed, suggesting a systemic issue with timestamp formatting in the agent's code or configuration. — record lesson: "Format all timestamps as UTC ISO-8601 (YYYY-MM-DDTHH:MM:SSZ) before passing them to log_case." | bench: advisory finding, not executable |
| 41e1594d | loop.tool_failure/1 | applied | Tool "log_case" failed 68 times (38% of the calls that could fail this way): {"error":{"code":"invalid_timestamp","message":"timestamp must be utc iso-#: | bench: recurring failure evidence |
| 3687eecb | loop.tool_failure/1 | applied | Tool "refund" failed 77 times (37% of the calls that could fail this way): {"error":{"code":"approval_required","message":"refunds over $# require a vali | bench: recurring failure evidence |

*6 proposed — 4 applied, 0 rejected, 2 advisory, 0 apply_failed.*
