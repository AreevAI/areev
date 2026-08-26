# selfimprove report

*config:* `{"agent_cmd":"python3 crates/areev-bench/scripts/openrouter_toolcall.py qwen/qwen3-30b-a3b-instruct-2507","arms":["m-steel","m-all","m-llm"],"assert_shape":false,"bench":"selfimprove_aba","context_cmd":null,"db":"/private/tmp/claude-501/-Users-sathish-mg-products-areev/948e5ede-40ed-422a-8911-525405666586/scratchpad/run3-s1/bench.db","eval":100,"experience":300,"git_rev":"5aac29b167be9af558050da725785f529b8ce52e","ground_cmd":"python3 crates/areev-bench/scripts/openrouter_loop.py deepseek/deepseek-chat","llm_cmd":"python3 crates/areev-bench/scripts/openrouter_loop.py qwen/qwen3-30b-a3b-instruct-2507","max_turns":24,"mllm_cmd":"python3 crates/areev-bench/scripts/openrouter_toolcall.py qwen/qwen3-30b-a3b-instruct-2507","mock":false,"phase_base_ms":1700000000000,"reviewer_actor":"user:bench-reviewer","runner_actor":"agent:bench-runner","seed":1,"workdir":"/private/tmp/claude-501/-Users-sathish-mg-products-areev/948e5ede-40ed-422a-8911-525405666586/scratchpad/run3-s1","workers":6}`

## Held-out eval by state

| state | n | success | tool errors | mean steps | tokens |
|---|---|---|---|---|---|
| A0 | 100 | 35.0% (35/100) | 104 | 6.6 | 753800 |
| B | 100 | 59.0% (59/100) | 99 | 7.0 | 870882 |
| A1 | 100 | 34.0% (34/100) | 106 | 6.6 | 758385 |
| B2 | 100 | 50.0% (50/100) | 95 | 6.9 | 850140 |
| M-steel | 100 | 72.0% (72/100) | 128 | 7.0 | 1171366 |
| M-all | 100 | 53.0% (53/100) | 91 | 7.1 | 5242140 |
| M-llm | 100 | 55.0% (55/100) | 90 | 6.5 | 1092840 |

## Per-rule mishandling recurrence (mishandled/opportunities)

| rule | A0 | B | A1 | B2 | M-steel | M-all | M-llm |
|---|---|---|---|---|---|---|---|
| R1 | 0/45 | 0/45 | 0/45 | 0/45 | 0/45 | 0/45 | 0/45 |
| R2 | 0/100 | 0/100 | 0/100 | 0/100 | 0/100 | 0/100 | 0/100 |
| R3 | 0/50 | 0/50 | 0/50 | 0/50 | 0/50 | 0/50 | 0/50 |
| R4 | 10/25 | 1/25 | 10/25 | 3/25 | 20/25 | 0/25 | 0/25 |
| R5 | 8/25 | 4/25 | 8/25 | 6/25 | 6/25 | 4/25 | 2/25 |
| R6 | 47/51 | 35/51 | 48/51 | 41/51 | 17/51 | 44/51 | 43/51 |

## Governance ledger

| hash | source | disposition | summary | because |
|---|---|---|---|---|
| 75a882bd | loop.tool_failure/1 | applied | Tool "log_case" failed 63 times (34% of the calls that could fail this way): {"error":{"code":"invalid_timestamp","message":"timestamp must be utc iso-#: | bench: recurring failure evidence |
| a3dd53aa | loop.tool_failure/1 | applied | Tool "refund" failed 65 times (35% of the calls that could fail this way): {"error":{"code":"approval_required","message":"refunds over $# require a vali | bench: recurring failure evidence |
| b2e02bde | loop.tool_failure/1 | applied | Tool "log_case" failed 63 times (34% of the calls that could fail this way): {"error":{"code":"invalid_timestamp","message":"timestamp must be utc iso-#: | bench: recurring failure evidence |
| b71fd36e | loop.tool_failure/1 | applied | Tool "refund" failed 65 times (35% of the calls that could fail this way): {"error":{"code":"approval_required","message":"refunds over $# require a vali | bench: recurring failure evidence |

*4 proposed — 4 applied, 0 rejected, 0 advisory, 0 apply_failed.*
