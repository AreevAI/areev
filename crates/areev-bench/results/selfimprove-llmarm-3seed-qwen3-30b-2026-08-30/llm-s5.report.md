# selfimprove report

*config:* `{"agent_cmd":"python3 crates/areev-bench/scripts/openrouter_toolcall.py qwen/qwen3-30b-a3b-instruct-2507","arms":[],"assert_shape":false,"bench":"selfimprove_aba","context_cmd":null,"db":"/private/tmp/claude-501/-Users-sathish-mg-products-areev/ba8b88d8-5a30-46d9-8e5d-969b3b311aa2/scratchpad/pub3/llm-s5/bench.db","eval":100,"experience":300,"git_rev":"39fc35f6ee8e617a42dfdbe234282d614a3a6075","ground_cmd":"python3 crates/areev-bench/scripts/openrouter_loop.py deepseek/deepseek-chat","llm_cmd":"python3 crates/areev-bench/scripts/openrouter_loop.py qwen/qwen3-30b-a3b-instruct-2507","llm_lessons":true,"max_turns":24,"mllm_cmd":null,"mock":false,"mock_llm":false,"phase_base_ms":1700000000000,"reviewer_actor":"user:bench-reviewer","runner_actor":"agent:bench-runner","seed":5,"workdir":"/private/tmp/claude-501/-Users-sathish-mg-products-areev/ba8b88d8-5a30-46d9-8e5d-969b3b311aa2/scratchpad/pub3/llm-s5","workers":4}`

## Held-out eval by state

| state | n | success | tool errors | mean steps | tokens |
|---|---|---|---|---|---|
| A0 | 100 | 53.0% (53/100) | 100 | 6.5 | 732710 |
| B | 100 | 58.0% (58/100) | 78 | 7.0 | 885919 |
| A1 | 100 | 53.0% (53/100) | 104 | 6.6 | 737274 |
| B2 | 100 | 68.0% (68/100) | 85 | 7.0 | 884944 |

## Per-rule mishandling recurrence (mishandled/opportunities)

| rule | A0 | B | A1 | B2 |
|---|---|---|---|---|
| R1 | 0/44 | 0/44 | 0/44 | 0/44 |
| R2 | 0/100 | 0/100 | 0/100 | 0/100 |
| R3 | 0/50 | 0/50 | 0/50 | 0/50 |
| R4 | 16/25 | 8/25 | 15/25 | 11/25 |
| R5 | 8/25 | 5/25 | 9/25 | 4/25 |
| R6 | 28/40 | 19/40 | 27/40 | 19/40 |

## Governance ledger

| hash | source | disposition | summary | because |
|---|---|---|---|---|
| f0eab0df | llm | applied | Timestamps consistently fail validation due to missing 'Z' suffix in ISO-8601 format, indicating a systemic formatting issue in the log_case tool's input. — record lesson: "Append 'Z' to all ISO-8601 timestamps to explicitly denote UTC timezone before calling log_case." | bench: recurring failure evidence |
| 8413d6e1 | loop.tool_failure/1 | applied | Tool "log_case" failed 69 times (41% of the calls that could fail this way): {"error":{"code":"invalid_timestamp","message":"timestamp must be utc iso-#: | bench: recurring failure evidence |
| 0bc62e39 | loop.tool_failure/1 | applied | Tool "refund" failed 82 times (36% of the calls that could fail this way): {"error":{"code":"approval_required","message":"refunds over $# require a vali | bench: recurring failure evidence |
| b27e28d9 | llm | applied | The log_case tool consistently fails due to missing 'Z' suffix in ISO-8601 timestamps, but the approved lesson only addresses the symptom (missing 'Z') without enforcing validation before submission.  — record lesson: "Validate ISO-8601 timestamps for 'Z' suffix and correct format before invoking log_case; do not submit malformed timestamps." | bench: recurring failure evidence |
| d24acdf0 | loop.tool_failure/1 | applied | Tool "log_case" failed 69 times (41% of the calls that could fail this way): {"error":{"code":"invalid_timestamp","message":"timestamp must be utc iso-#: | bench: recurring failure evidence |
| 69bd472c | loop.tool_failure/1 | applied | Tool "refund" failed 82 times (36% of the calls that could fail this way): {"error":{"code":"approval_required","message":"refunds over $# require a vali | bench: recurring failure evidence |

*6 proposed — 6 applied, 0 rejected, 0 advisory, 0 apply_failed.*
