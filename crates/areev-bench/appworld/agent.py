#!/usr/bin/env python3
"""The three arms, as one agent with one switch.

`AreevReActCodeAgent` subclasses AppWorld's shipped `SimplifiedReActCodeAgent`
and changes exactly two things:

  1. after the stock prompt is rendered, a block is appended to it -- nothing
     for arm A, the agent's own past errors for the passive arm, the approved
     rules for the governed arm;
  2. at the end of an episode the failures are recorded, and in the governed
     arm a loop pass runs.

Everything else -- the instructions, the worked example, the step loop, the
code extraction, the scoring -- is the benchmark's and is byte-identical
across arms. That is deliberate: if the arms differed anywhere else, a
difference between them would not be attributable to the block.

`memory_mode` is the whole manipulation:

    none      the shipped agent. The anchor.
    passive   its own errors, deduplicated, dropped into the prompt. No loop,
              no approval, no rule. The baseline a governed loop must beat.
    governed  approved rules only, each carrying a supervisor's reason.
"""
from __future__ import annotations

import importlib.util
import os
from typing import Any

from appworld.environment import AppWorld
from appworld_agents.code.simplified.agent import Agent
from appworld_agents.code.simplified.react_code_agent import SimplifiedReActCodeAgent



def _sibling(name: str):
    """Load a module beside this file by path.

    Not `sys.path.insert` + `import`: this directory holds `agent.py` and
    `memory.py`, and putting it on the front of the path would shadow those
    names for everything else in the process.
    """
    path = os.path.join(os.path.dirname(os.path.abspath(__file__)), name + ".py")
    spec = importlib.util.spec_from_file_location("areev_appworld_" + name, path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


areev_memory = _sibling("memory")

MODES = ("none", "passive", "governed")


@Agent.register("areev_react_code_agent")
class AreevReActCodeAgent(SimplifiedReActCodeAgent):  # type: ignore[misc]
    def __init__(
        self,
        memory_mode: str = "none",
        memory_db: str | None = None,
        memory_frozen: bool = False,
        learn_every: int = 0,
        loop_llm_cmd: str | None = None,
        loop_ground_cmd: str | None = None,
        review_cmd: str | None = None,
        loop_policy: str | None = None,
        **kwargs: Any,
    ):
        # Checked before the parent constructor runs: a mistyped arm is the
        # one error that must not be reported as a language-model config
        # problem thirty lines deeper.
        if memory_mode not in MODES:
            raise ValueError(f"memory_mode must be one of {MODES}, not {memory_mode!r}")
        if memory_mode != "none" and not memory_db:
            raise ValueError(f"memory_mode={memory_mode} needs a memory_db path")
        super().__init__(**kwargs)
        self.memory_mode = memory_mode
        self.memory_frozen = memory_frozen
        self.memory_db = os.path.expanduser(memory_db) if memory_db else None
        if self.memory_db and memory_frozen:
            # A held-out arm reads a memory it must not change, and the two
            # passive passes (B and B2) must read the SAME memory or the noise
            # floor measures the memory drifting rather than the model. So the
            # frozen memory is copied once per process and opened read-only:
            # the copy makes parallel workers possible (one file admits one
            # handle, even read-only, STO-E002), and read-only makes the freeze
            # the store's guarantee instead of our good intentions.
            self.memory_db = self._process_local_copy(self.memory_db)
        # 0 means "never learn during this pass" -- which is what every
        # held-out evaluation runs with, so the memory under test is frozen.
        self.learn_every = learn_every
        self.loop_llm_cmd = loop_llm_cmd
        self.loop_ground_cmd = loop_ground_cmd
        self.review_cmd = review_cmd
        self.loop_policy = loop_policy
        self._episodes_seen = 0
        self.learn_reports: list[dict] = []

    # -- the prompt -------------------------------------------------------

    def initialize(self, world: AppWorld) -> None:
        super().initialize(world)
        if self.memory_mode == "none":
            return
        block = areev_memory.block_for(
            self.memory_db, self.memory_mode, read_only=self.memory_frozen
        )
        if block:
            # Appended to the final instruction message, after the task, so it
            # is the last thing read before the first action.
            self.messages[-1]["content"] += "\n\n" + block + "\n"

    # -- the episode boundary ---------------------------------------------

    @staticmethod
    def _process_local_copy(db_path: str) -> str:
        local = "%s.p%d.db" % (db_path[:-3] if db_path.endswith(".db") else db_path, os.getpid())
        if not os.path.exists(local):
            areev_memory.copy_memory(db_path, local)
        return local

    def solve_task(self, task_id: str) -> None:
        super().solve_task(task_id)
        # A frozen arm records nothing. If it did, B and B2 would be reading
        # two different memories by their second episode and the noise floor
        # would be meaningless.
        if self.memory_mode == "none" or self.memory_frozen:
            return

        # Read from the world's own interaction record rather than watching the
        # step loop. Pairing code with its result mid-loop always drops the
        # LAST interaction of an episode -- the loop exits after it, so there is
        # no next step to pair it in -- and the last failure is often the one
        # that ended the episode. `environment_io` already holds every pair.
        errors = []
        for entry in getattr(self.world, "environment_io", []) or []:
            summary = areev_memory.summarize_error(entry.get("input"), entry.get("output"))
            if summary:
                errors.append(summary)

        hit_cap = self.step_number >= self.max_steps
        areev_memory.with_memory(
            self.memory_db,
            areev_memory.RUNNER,
            lambda db: areev_memory.record_episode(
                db, task_id, errors, self.step_number, hit_cap
            ),
        )
        self._episodes_seen += 1

        if self.memory_mode != "governed" or not self.learn_every:
            return
        if self._episodes_seen % self.learn_every:
            return
        report = areev_memory.learn(
            self.memory_db,
            llm_cmd=self.loop_llm_cmd,
            ground_cmd=self.loop_ground_cmd,
            judge=areev_memory.make_judge(self.review_cmd),
            policy=self.loop_policy,
        )
        report["after_episodes"] = self._episodes_seen
        self.learn_reports.append(report)
