# selfimprove report

*config:* `{"agent_cmd":"python3 crates/areev-bench/scripts/openrouter_toolcall.py qwen/qwen3-30b-a3b-instruct-2507","arms":[],"assert_shape":false,"bench":"selfimprove_aba","context_cmd":null,"db":"/private/tmp/claude-501/-Users-sathish-mg-products-areev/ba8b88d8-5a30-46d9-8e5d-969b3b311aa2/scratchpad/pub4/combined-s5/bench.db","eval":100,"experience":300,"git_rev":"81965f6913bfc0a2fc5442c48792d8286448f7b4","ground_cmd":"python3 crates/areev-bench/scripts/openrouter_loop.py deepseek/deepseek-chat","llm_cmd":"python3 crates/areev-bench/scripts/openrouter_loop.py qwen/qwen3-30b-a3b-instruct-2507","llm_lessons":true,"max_turns":24,"mllm_cmd":null,"mock":false,"mock_llm":false,"no_analyzer_lessons":false,"phase_base_ms":1700000000000,"reviewer_actor":"user:bench-reviewer","runner_actor":"agent:bench-runner","seed":5,"workdir":"/private/tmp/claude-501/-Users-sathish-mg-products-areev/ba8b88d8-5a30-46d9-8e5d-969b3b311aa2/scratchpad/pub4/combined-s5","workers":4}`

## Held-out eval by state

| state | n | success | tool errors | mean steps | tokens |
|---|---|---|---|---|---|
| A0 | 100 | 55.0% (55/100) | 101 | 6.5 | 730398 |
| B | 100 | 76.0% (76/100) | 94 | 7.0 | 849354 |
| A1 | 100 | 56.0% (56/100) | 105 | 6.6 | 736974 |
| B2 | 100 | 76.0% (76/100) | 90 | 6.8 | 827721 |

## Per-rule mishandling recurrence (mishandled/opportunities)

| rule | A0 | B | A1 | B2 |
|---|---|---|---|---|
| R1 | 0/44 | 0/44 | 0/44 | 0/44 |
| R2 | 0/100 | 0/100 | 0/100 | 0/100 |
| R3 | 0/50 | 0/50 | 0/50 | 0/50 |
| R4 | 14/25 | 9/25 | 16/25 | 7/25 |
| R5 | 9/25 | 4/25 | 8/25 | 3/25 |
| R6 | 25/40 | 15/40 | 24/40 | 17/40 |

## Governance ledger

| hash | source | disposition | summary | because |
|---|---|---|---|---|
| a5393ee0 | loop.tool_failure/1 | applied | Tool "log_case" failed 67 times (40% of the calls that could fail this way): {"error":{"code":"invalid_timestamp","message":"timestamp must be utc iso-#: | bench: recurring failure evidence |
| 2a849068 | loop.tool_failure/1 | applied | Tool "refund" failed 85 times (37% of the calls that could fail this way): {"error":{"code":"approval_required","message":"refunds over $# require a vali | bench: recurring failure evidence |
| 03a2f288 | loop.tool_failure/1 | applied | Tool "log_case" failed 67 times (40% of the calls that could fail this way): {"error":{"code":"invalid_timestamp","message":"timestamp must be utc iso-#: | bench: recurring failure evidence |
| e3f15de6 | loop.tool_failure/1 | applied | Tool "refund" failed 85 times (37% of the calls that could fail this way): {"error":{"code":"approval_required","message":"refunds over $# require a vali | bench: recurring failure evidence |

*4 proposed — 4 applied, 0 rejected, 0 advisory, 0 apply_failed.*
