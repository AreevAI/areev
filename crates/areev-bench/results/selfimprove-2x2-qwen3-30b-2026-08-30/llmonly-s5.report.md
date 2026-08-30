# selfimprove report

*config:* `{"agent_cmd":"python3 crates/areev-bench/scripts/openrouter_toolcall.py qwen/qwen3-30b-a3b-instruct-2507","arms":[],"assert_shape":false,"bench":"selfimprove_aba","context_cmd":null,"db":"/private/tmp/claude-501/-Users-sathish-mg-products-areev/ba8b88d8-5a30-46d9-8e5d-969b3b311aa2/scratchpad/pub4/llmonly-s5/bench.db","eval":100,"experience":300,"git_rev":"81965f6913bfc0a2fc5442c48792d8286448f7b4","ground_cmd":"python3 crates/areev-bench/scripts/openrouter_loop.py deepseek/deepseek-chat","llm_cmd":"python3 crates/areev-bench/scripts/openrouter_loop.py qwen/qwen3-30b-a3b-instruct-2507","llm_lessons":true,"max_turns":24,"mllm_cmd":null,"mock":false,"mock_llm":false,"no_analyzer_lessons":true,"phase_base_ms":1700000000000,"reviewer_actor":"user:bench-reviewer","runner_actor":"agent:bench-runner","seed":5,"workdir":"/private/tmp/claude-501/-Users-sathish-mg-products-areev/ba8b88d8-5a30-46d9-8e5d-969b3b311aa2/scratchpad/pub4/llmonly-s5","workers":4}`

## Held-out eval by state

| state | n | success | tool errors | mean steps | tokens |
|---|---|---|---|---|---|
| A0 | 100 | 52.0% (52/100) | 101 | 6.5 | 722088 |
| B | 100 | 50.0% (50/100) | 103 | 6.6 | 745340 |
| A1 | 100 | 50.0% (50/100) | 100 | 6.5 | 734615 |
| B2 | 100 | 57.0% (57/100) | 87 | 6.8 | 806596 |

## Per-rule mishandling recurrence (mishandled/opportunities)

| rule | A0 | B | A1 | B2 |
|---|---|---|---|---|
| R1 | 0/44 | 0/44 | 0/44 | 0/44 |
| R2 | 0/100 | 0/100 | 0/100 | 0/100 |
| R3 | 0/50 | 0/50 | 0/50 | 0/50 |
| R4 | 15/25 | 16/25 | 14/25 | 19/25 |
| R5 | 9/25 | 10/25 | 9/25 | 0/25 |
| R6 | 28/40 | 29/40 | 29/40 | 26/40 |

## Governance ledger

| hash | source | disposition | summary | because |
|---|---|---|---|---|
| 9283bc11 | loop.tool_failure/1 | advisory | Tool "log_case" failed 66 times (38% of the calls that could fail this way): {"error":{"code":"invalid_timestamp","message":"timestamp must be utc iso-#: | bench: advisory finding, not executable |
| b3c4ec3b | loop.tool_failure/1 | advisory | Tool "refund" failed 89 times (38% of the calls that could fail this way): {"error":{"code":"approval_required","message":"refunds over $# require a vali | bench: advisory finding, not executable |
| 9283bc11 | loop.tool_failure/1 | advisory | Tool "log_case" failed 66 times (38% of the calls that could fail this way): {"error":{"code":"invalid_timestamp","message":"timestamp must be utc iso-#: | bench: advisory finding, not executable |
| b3c4ec3b | loop.tool_failure/1 | advisory | Tool "refund" failed 89 times (38% of the calls that could fail this way): {"error":{"code":"approval_required","message":"refunds over $# require a vali | bench: advisory finding, not executable |
| 14c9e3e5 | llm | applied | Timestamps passed to log_case are not consistently formatted as UTC ISO-8601, causing repeated validation failures. — record lesson: "Format all timestamps as UTC ISO-8601 (YYYY-MM-DDTHH:MM:SSZ) before calling log_case." | bench: recurring failure evidence |

*5 proposed — 1 applied, 0 rejected, 4 advisory, 0 apply_failed.*
