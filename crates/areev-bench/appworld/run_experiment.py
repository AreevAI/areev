#!/usr/bin/env python3
"""Run one AppWorld experiment, with the OpenRouter upstream provider pinned.

Why this exists rather than `appworld run`: AppWorld's caller whitelists the
generation kwargs it forwards, and `extra_body` -- the only per-request way to
pin an OpenRouter upstream provider -- is not on that list. Editing the
benchmark to add it would make every number here a number about our fork, so
instead this wraps the single function that builds the HTTP caller
(`get_raw_lm_caller`, the transport boundary) and injects the pin there.
Nothing about task construction, execution or scoring is touched.

The pin matters because OpenRouter serves this model from four upstreams at
different quantisations. Unpinned, two arms of the same experiment can be two
different models, and this program has already lost one result that way.

    python3 run_experiment.py <experiment-name> --root ~/mg/local/appworld \
        --provider streamlake
    python3 run_experiment.py --check-pin --provider streamlake   # one cheap call
"""
from __future__ import annotations

import argparse
import os
import sys


def install_provider_pin(provider: str) -> None:
    """Force every OpenRouter call through one upstream, no fallbacks."""
    import appworld_agents.code.simplified.language_model as lm

    original = lm.get_raw_lm_caller
    body = {"provider": {"order": [provider], "allow_fallbacks": False}}

    def pinned(*args, **kwargs):
        call = original(*args, **kwargs)

        def wrapper(**call_kwargs):
            extra = dict(call_kwargs.pop("extra_body", None) or {})
            extra.update(body)
            return call(extra_body=extra, **call_kwargs)

        return wrapper

    lm.get_raw_lm_caller = pinned


def install_response_normalizer() -> None:
    """Absent-versus-null, in the two places the harness conflates them.

    `dict.get(key, default)` returns the default only when the key is MISSING.
    Where a key is present and null it returns None, and both of these bite:

      1. the ReAct scaffold reads `output.get("reasoning_content", "").strip()`
         and dies at its first step on an upstream that returns the key as
         null;
      2. `Tokens.from_response` reads `response.get("usage", {})` and dies with
         `argument of type 'NoneType' is not iterable` when a response carries
         no usage block -- which killed a whole worker mid-sweep, silently
         dropping the rest of its tasks.

    Neither is a behavioural change: null reasoning and absent reasoning both
    mean the model returned none. A null usage block, though, means that
    call's tokens are UNKNOWN and therefore uncounted, so each one is reported
    on stderr rather than quietly rounded to zero -- a cost meter that
    under-reports without saying so is worse than one that fails.
    """
    from appworld_agents.code.common import usage_tracker
    from appworld_agents.code.simplified.language_model import LanguageModel

    original_generate = LanguageModel.generate

    def generate(self, *args, **kwargs):
        output = original_generate(self, *args, **kwargs)
        if isinstance(output, dict) and output.get("reasoning_content", "") is None:
            output.pop("reasoning_content")
        return output

    LanguageModel.generate = generate

    original_from_response = usage_tracker.Tokens.from_response.__func__

    def from_response(cls, response, usage_key="usage"):
        if isinstance(response, dict) and response.get(usage_key, {}) is None:
            print(
                "AREEV-WARN usage-null: a response carried no usage block; "
                "its tokens are uncounted in the cost meter",
                file=sys.stderr,
            )
            response = dict(response)
            response[usage_key] = {}
        return original_from_response(cls, response, usage_key)

    usage_tracker.Tokens.from_response = classmethod(from_response)


def check_pin(provider: str, model: str) -> int:
    """One minimal completion, printing which upstream actually served it.

    A pin that silently does not bind is worse than no pin, so it is checked
    rather than assumed.
    """
    from openai import OpenAI

    client = OpenAI(
        api_key=os.environ["OPENROUTER_API_KEY"],
        base_url="https://openrouter.ai/api/v1",
    )
    response = client.chat.completions.create(
        model=model,
        messages=[{"role": "user", "content": "Reply with the single word: ok"}],
        max_tokens=3,
        temperature=0.0,
        extra_body={"provider": {"order": [provider], "allow_fallbacks": False}},
    )
    served_by = getattr(response, "provider", None)
    print(f"asked for : {provider}")
    print(f"served by : {served_by}")
    print(f"model     : {response.model}")
    if served_by is None:
        print("INCONCLUSIVE: the response carries no provider field.")
        return 2
    if served_by.lower().replace(" ", "") != provider.lower().replace(" ", ""):
        print("PIN DID NOT BIND.")
        return 1
    print("pin binds.")
    return 0


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("experiment_name", nargs="?")
    ap.add_argument("--root", default=os.environ.get("APPWORLD_ROOT", "."))
    ap.add_argument("--provider", default=None, help="OpenRouter upstream to pin")
    ap.add_argument("--num-processes", type=int, default=1)
    ap.add_argument("--process-index", type=int, default=0)
    ap.add_argument("--check-pin", action="store_true")
    ap.add_argument("--model", default="qwen/qwen3-30b-a3b-instruct-2507")
    args = ap.parse_args()

    root = os.path.expanduser(args.root)
    sys.path.insert(0, root)

    if args.check_pin:
        if not args.provider:
            raise SystemExit("--check-pin needs --provider")
        return check_pin(args.provider, args.model)

    if not args.experiment_name:
        raise SystemExit("an experiment name is required")

    from appworld import update_root
    from appworld.common.path_store import path_store
    from appworld.common.utils import jsonnet_load

    update_root(root)

    if args.provider:
        install_provider_pin(args.provider)
    install_response_normalizer()

    config_path = os.path.join(path_store.experiment_configs, args.experiment_name + ".jsonnet")
    if not os.path.exists(config_path):
        raise SystemExit(f"no such experiment config: {config_path}")
    config = jsonnet_load(
        config_path,
        APPWORLD_EXPERIMENT_PROMPTS_PATH=path_store.experiment_prompts,
        APPWORLD_EXPERIMENT_CONFIGS_PATH=path_store.experiment_configs,
        APPWORLD_EXPERIMENT_CODE_PATH=path_store.experiment_code,
    )
    runner_type = config.pop("type")
    runner_config = config.pop("config")
    config.pop("metadata", None)  # leaderboard bookkeeping, not runner input
    if config:
        raise SystemExit(f"unexpected keys in the experiment config: {config}")
    if runner_type != "simplified":
        raise SystemExit(f"this runner only drives 'simplified' experiments, not {runner_type!r}")

    # Importing the bridge registers `areev_react_code_agent` with AppWorld's
    # agent factory; without it a config naming that type cannot be built.
    _sibling_agent = os.path.join(os.path.dirname(os.path.abspath(__file__)), "agent.py")
    import importlib.util

    spec = importlib.util.spec_from_file_location("areev_appworld_agent", _sibling_agent)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)

    from appworld_agents.code.simplified.run import run_experiment

    run_experiment(
        experiment_name=args.experiment_name,
        runner_config=runner_config,
        num_processes=args.num_processes,
        process_index=args.process_index,
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
