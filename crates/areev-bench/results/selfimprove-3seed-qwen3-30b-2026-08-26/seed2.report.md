# selfimprove report

*config:* `{"agent_cmd":"python3 crates/areev-bench/scripts/openrouter_toolcall.py qwen/qwen3-30b-a3b-instruct-2507","arms":["m-steel","m-all","m-llm"],"assert_shape":false,"bench":"selfimprove_aba","context_cmd":null,"db":"/private/tmp/claude-501/-Users-sathish-mg-products-areev/948e5ede-40ed-422a-8911-525405666586/scratchpad/run3-s2/bench.db","eval":100,"experience":300,"git_rev":"5aac29b167be9af558050da725785f529b8ce52e","ground_cmd":"python3 crates/areev-bench/scripts/openrouter_loop.py deepseek/deepseek-chat","llm_cmd":"python3 crates/areev-bench/scripts/openrouter_loop.py qwen/qwen3-30b-a3b-instruct-2507","max_turns":24,"mllm_cmd":"python3 crates/areev-bench/scripts/openrouter_toolcall.py qwen/qwen3-30b-a3b-instruct-2507","mock":false,"phase_base_ms":1700000000000,"reviewer_actor":"user:bench-reviewer","runner_actor":"agent:bench-runner","seed":2,"workdir":"/private/tmp/claude-501/-Users-sathish-mg-products-areev/948e5ede-40ed-422a-8911-525405666586/scratchpad/run3-s2","workers":6}`

## Held-out eval by state

| state | n | success | tool errors | mean steps | tokens |
|---|---|---|---|---|---|
| A0 | 100 | 42.0% (42/100) | 95 | 6.5 | 730192 |
| B | 100 | 61.0% (61/100) | 87 | 6.8 | 838761 |
| A1 | 100 | 39.0% (39/100) | 100 | 6.4 | 727673 |
| B2 | 100 | 62.0% (62/100) | 86 | 6.8 | 839240 |
| M-steel | 100 | 64.0% (64/100) | 123 | 6.8 | 1086093 |
| M-all | 100 | 60.0% (60/100) | 82 | 6.9 | 5150753 |
| M-llm | 100 | 61.0% (61/100) | 83 | 6.6 | 1096381 |

## Per-rule mishandling recurrence (mishandled/opportunities)

| rule | A0 | B | A1 | B2 | M-steel | M-all | M-llm |
|---|---|---|---|---|---|---|---|
| R1 | 0/42 | 0/42 | 0/42 | 0/42 | 0/42 | 0/42 | 0/42 |
| R2 | 0/100 | 0/100 | 0/100 | 0/100 | 0/100 | 0/100 | 0/100 |
| R3 | 0/50 | 0/50 | 0/50 | 0/50 | 0/50 | 0/50 | 0/50 |
| R4 | 11/25 | 1/25 | 14/25 | 2/25 | 24/25 | 1/25 | 0/25 |
| R5 | 11/25 | 6/25 | 10/25 | 7/25 | 8/25 | 4/25 | 0/25 |
| R6 | 37/41 | 32/41 | 38/41 | 30/41 | 16/41 | 33/41 | 32/41 |

## Governance ledger

| hash | source | disposition | summary | because |
|---|---|---|---|---|
| 6b4b469e | loop.tool_failure/1 | applied | Tool "log_case" failed 69 times (34% of the calls that could fail this way): {"error":{"code":"invalid_timestamp","message":"timestamp must be utc iso-#: | bench: recurring failure evidence |
| dd608706 | loop.tool_failure/1 | applied | Tool "refund" failed 79 times (39% of the calls that could fail this way): {"error":{"code":"approval_required","message":"refunds over $# require a vali | bench: recurring failure evidence |
| d587403b | loop.tool_failure/1 | applied | Tool "log_case" failed 69 times (34% of the calls that could fail this way): {"error":{"code":"invalid_timestamp","message":"timestamp must be utc iso-#: | bench: recurring failure evidence |
| dd78ae05 | loop.tool_failure/1 | applied | Tool "refund" failed 79 times (39% of the calls that could fail this way): {"error":{"code":"approval_required","message":"refunds over $# require a vali | bench: recurring failure evidence |

*4 proposed — 4 applied, 0 rejected, 0 advisory, 0 apply_failed.*
