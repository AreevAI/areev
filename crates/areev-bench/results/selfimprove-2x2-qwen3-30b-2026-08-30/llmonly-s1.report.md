# selfimprove report

*config:* `{"agent_cmd":"python3 crates/areev-bench/scripts/openrouter_toolcall.py qwen/qwen3-30b-a3b-instruct-2507","arms":[],"assert_shape":false,"bench":"selfimprove_aba","context_cmd":null,"db":"/private/tmp/claude-501/-Users-sathish-mg-products-areev/ba8b88d8-5a30-46d9-8e5d-969b3b311aa2/scratchpad/pub4/llmonly-s1/bench.db","eval":100,"experience":300,"git_rev":"81965f6913bfc0a2fc5442c48792d8286448f7b4","ground_cmd":"python3 crates/areev-bench/scripts/openrouter_loop.py deepseek/deepseek-chat","llm_cmd":"python3 crates/areev-bench/scripts/openrouter_loop.py qwen/qwen3-30b-a3b-instruct-2507","llm_lessons":true,"max_turns":24,"mllm_cmd":null,"mock":false,"mock_llm":false,"no_analyzer_lessons":true,"phase_base_ms":1700000000000,"reviewer_actor":"user:bench-reviewer","runner_actor":"agent:bench-runner","seed":1,"workdir":"/private/tmp/claude-501/-Users-sathish-mg-products-areev/ba8b88d8-5a30-46d9-8e5d-969b3b311aa2/scratchpad/pub4/llmonly-s1","workers":4}`

## Held-out eval by state

| state | n | success | tool errors | mean steps | tokens |
|---|---|---|---|---|---|
| A0 | 100 | 49.0% (49/100) | 109 | 6.7 | 755170 |
| B | 100 | 46.0% (46/100) | 112 | 6.7 | 756166 |
| A1 | 100 | 49.0% (49/100) | 109 | 6.7 | 753784 |
| B2 | 100 | 49.0% (49/100) | 95 | 6.9 | 813360 |

## Per-rule mishandling recurrence (mishandled/opportunities)

| rule | A0 | B | A1 | B2 |
|---|---|---|---|---|
| R1 | 0/45 | 0/45 | 0/45 | 0/45 |
| R2 | 0/100 | 0/100 | 0/100 | 0/100 |
| R3 | 0/50 | 0/50 | 0/50 | 0/50 |
| R4 | 14/25 | 18/25 | 12/25 | 18/25 |
| R5 | 10/25 | 8/25 | 9/25 | 0/25 |
| R6 | 32/51 | 35/51 | 34/51 | 35/51 |

## Governance ledger

| hash | source | disposition | summary | because |
|---|---|---|---|---|
| d73678a5 | loop.tool_failure/1 | advisory | Tool "log_case" failed 72 times (40% of the calls that could fail this way): {"error":{"code":"invalid_timestamp","message":"timestamp must be utc iso-#: | bench: advisory finding, not executable |
| 52194ce1 | loop.tool_failure/1 | advisory | Tool "refund" failed 83 times (37% of the calls that could fail this way): {"error":{"code":"approval_required","message":"refunds over $# require a vali | bench: advisory finding, not executable |
| d73678a5 | loop.tool_failure/1 | advisory | Tool "log_case" failed 72 times (40% of the calls that could fail this way): {"error":{"code":"invalid_timestamp","message":"timestamp must be utc iso-#: | bench: advisory finding, not executable |
| 52194ce1 | loop.tool_failure/1 | advisory | Tool "refund" failed 83 times (37% of the calls that could fail this way): {"error":{"code":"approval_required","message":"refunds over $# require a vali | bench: advisory finding, not executable |
| 71ec8271 | llm | applied | Timestamps passed to log_case are consistently malformed, suggesting a systemic issue in timestamp formatting before tool invocation. — record lesson: "Format timestamps as UTC ISO-8601 (YYYY-MM-DDTHH:MM:SSZ) before calling log_case." | bench: recurring failure evidence |

*5 proposed — 1 applied, 0 rejected, 4 advisory, 0 apply_failed.*
