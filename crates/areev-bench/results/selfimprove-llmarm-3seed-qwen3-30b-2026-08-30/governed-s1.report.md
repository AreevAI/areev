# selfimprove report

*config:* `{"agent_cmd":"python3 crates/areev-bench/scripts/openrouter_toolcall.py qwen/qwen3-30b-a3b-instruct-2507","arms":[],"assert_shape":false,"bench":"selfimprove_aba","context_cmd":null,"db":"/private/tmp/claude-501/-Users-sathish-mg-products-areev/ba8b88d8-5a30-46d9-8e5d-969b3b311aa2/scratchpad/pub3/governed-s1/bench.db","eval":100,"experience":300,"git_rev":"39fc35f6ee8e617a42dfdbe234282d614a3a6075","ground_cmd":"python3 crates/areev-bench/scripts/openrouter_loop.py deepseek/deepseek-chat","llm_cmd":"python3 crates/areev-bench/scripts/openrouter_loop.py qwen/qwen3-30b-a3b-instruct-2507","llm_lessons":false,"max_turns":24,"mllm_cmd":null,"mock":false,"mock_llm":false,"phase_base_ms":1700000000000,"reviewer_actor":"user:bench-reviewer","runner_actor":"agent:bench-runner","seed":1,"workdir":"/private/tmp/claude-501/-Users-sathish-mg-products-areev/ba8b88d8-5a30-46d9-8e5d-969b3b311aa2/scratchpad/pub3/governed-s1","workers":4}`

## Held-out eval by state

| state | n | success | tool errors | mean steps | tokens |
|---|---|---|---|---|---|
| A0 | 100 | 45.0% (45/100) | 104 | 6.7 | 755978 |
| B | 100 | 63.0% (63/100) | 104 | 7.0 | 860013 |
| A1 | 100 | 43.0% (43/100) | 111 | 6.7 | 757604 |
| B2 | 100 | 60.0% (60/100) | 109 | 7.1 | 872051 |

## Per-rule mishandling recurrence (mishandled/opportunities)

| rule | A0 | B | A1 | B2 |
|---|---|---|---|---|
| R1 | 0/45 | 0/45 | 0/45 | 0/45 |
| R2 | 0/100 | 0/100 | 0/100 | 0/100 |
| R3 | 0/50 | 0/50 | 0/50 | 0/50 |
| R4 | 10/25 | 4/25 | 11/25 | 7/25 |
| R5 | 10/25 | 5/25 | 9/25 | 6/25 |
| R6 | 38/51 | 28/51 | 39/51 | 29/51 |

## Governance ledger

| hash | source | disposition | summary | because |
|---|---|---|---|---|
| 5eb1b2e8 | loop.tool_failure/1 | applied | Tool "log_case" failed 68 times (39% of the calls that could fail this way): {"error":{"code":"invalid_timestamp","message":"timestamp must be utc iso-#: | bench: recurring failure evidence |
| 90cf24d4 | loop.tool_failure/1 | applied | Tool "refund" failed 70 times (34% of the calls that could fail this way): {"error":{"code":"approval_required","message":"refunds over $# require a vali | bench: recurring failure evidence |
| d94dd110 | loop.tool_failure/1 | applied | Tool "log_case" failed 68 times (39% of the calls that could fail this way): {"error":{"code":"invalid_timestamp","message":"timestamp must be utc iso-#: | bench: recurring failure evidence |
| 761c1c0a | loop.tool_failure/1 | applied | Tool "refund" failed 70 times (34% of the calls that could fail this way): {"error":{"code":"approval_required","message":"refunds over $# require a vali | bench: recurring failure evidence |

*4 proposed — 4 applied, 0 rejected, 0 advisory, 0 apply_failed.*
