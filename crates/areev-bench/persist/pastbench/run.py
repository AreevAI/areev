#!/usr/bin/env python3
"""Run PAST-Bench with the Areev agents registered — the benchmark itself is
not edited. Same command line as `past-bench`:

    python run.py evolve --family memory_ability/SM01_preference_adoption \\
        --agent areev-governed --registry agents.yaml --config config.persist.yaml \\
        --runtime local --sandbox --sandbox-tools --compare-no-persistence \\
        --model qwen/qwen3-30b-a3b-instruct-2507 --trace-dir traces/x

Environment the governed arm reads (all optional; without the model legs the
loop runs its deterministic analyzers only and the reviewer is the keyless
floor — a smoke, never a result):

    AREEV_LOOP_LLM_CMD     DISCOVER/VERIFY backend, e.g. openrouter_loop.py qwen/... --provider P --seed N
    AREEV_LOOP_GROUND_CMD  GROUND backend
    AREEV_REVIEW_CMD       the rubric reviewer's chat command (openrouter_toolcall.py openai/gpt-4o ...)
    AREEV_AGENT_PIN        OpenRouter provider pin for the agent model (e.g. coreweave/bf16)
    AREEV_USAGE_LOG        where the bench adapters meter their calls
    SEED                   request seed (also the split seed elsewhere)
"""
from __future__ import annotations

import os
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
for p in (HERE, os.path.dirname(HERE)):
    if p not in sys.path:
        sys.path.insert(0, p)

from past_bench.runner import self_evolve as _self_evolve  # noqa: E402
from past_bench.runtime import manager as _manager  # noqa: E402

import areev_backend  # noqa: E402
import mem0_backend  # noqa: E402

_manager._ADAPTERS["areev"] = areev_backend.AreevAdapter
_manager._ADAPTERS["mem0"] = mem0_backend.Mem0Adapter
_upstream_make = _self_evolve.make_persistence_backend


def _make_persistence_backend(agent_name: str):
    if agent_name.startswith("areev"):
        return areev_backend.make_backend(agent_name)
    if agent_name.startswith("mem0"):
        return mem0_backend.make_backend(agent_name)
    return _upstream_make(agent_name)


_self_evolve.make_persistence_backend = _make_persistence_backend

from past_bench.cli import main  # noqa: E402

if __name__ == "__main__":
    sys.exit(main())
