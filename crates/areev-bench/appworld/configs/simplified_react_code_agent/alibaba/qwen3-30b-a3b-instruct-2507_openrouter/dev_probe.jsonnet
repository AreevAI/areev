// Stage 2, the ceiling probe: the SHIPPED ReAct code agent, unmodified, on the
// stratified dev slice (`make_probe_set.py`). Nothing of ours is in the loop
// yet -- the only question this config asks is whether the pinned agent model
// scores anything at all on AppWorld, because a zero here makes every later
// arm uninterpretable. tau2-bench is the precedent for asking it first.
local experiment_prompts_path = std.extVar("APPWORLD_EXPERIMENT_PROMPTS_PATH");
{
    "type": "simplified",
    "config": {
        "agent": {
            "type": "simplified_react_code_agent",
            "model_config": {
                "client_name": "openai",
                "api_type": "chat_completions",
                "base_url": "https://openrouter.ai/api/v1",
                "api_key_env_name": "OPENROUTER_API_KEY",
                "name": "qwen/qwen3-30b-a3b-instruct-2507",
                "temperature": 0.0,
                "seed": 100,
                "drop_reasoning_content": false,
                // OpenRouter's StreamLake endpoint, quoted 2026-09-08. The pin
                // below is what makes these the rates actually billed.
                "cost_per_token": {
                    "input_cache_hit": 4.815e-08,
                    "input_cache_miss": 4.815e-08,
                    "input_cache_write": 0.0,
                    "output": 1.9305e-07,
                },
                // The upstream provider is pinned to `streamlake` by
                // `run_experiment.py --provider`, NOT here: AppWorld's caller
                // whitelists generation kwargs and drops `extra_body`, so a pin
                // written into this file would validate at construction and
                // then silently never bind. See that file's docstring.
                "retry_after_n_seconds": 15,
                "use_cache": false,
                "max_retries": 20,
            },
            "appworld_config": {
                "random_seed": 100,
                "raise_on_extra_parameters": true,
            },
            "logger_config": {"color": false, "verbose": true},
            // The hard budget. The probe is allowed $3 and no single task may
            // run away with more than $0.50 of it.
            "usage_tracker_config": {
                "max_cost_overall": 3.0,
                "max_cost_per_task": 0.5,
                "max_output_tokens_per_task": 100000,
            },
            "prompt_file_path": experiment_prompts_path + "/react_code_agent/instructions.txt",
            "ignore_multiple_calls": true,
            "max_prompt_length": null,
            "max_output_length": null,
            "max_steps": 50,
            "log_lm_calls": true,
            "skip_if_finished": true,
        },
        "dataset": "dev_probe",
    },
    "metadata": {
        "model": {
            "file_name": "qwen3-30b-a3b-instruct-2507_openrouter",
            "humanized_name": "Qwen3 30B A3B Instruct 2507",
            "precise_name": "qwen/qwen3-30b-a3b-instruct-2507",
            "creator": "alibaba",
            "provider": "openrouter",
        },
        "agent": {
            "file_name": "simplified_react_code_agent",
            "humanized_name": "ReAct Code Agent",
        },
    },
}
