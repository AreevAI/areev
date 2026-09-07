#!/usr/bin/env python3
"""Run one τ²-bench retail task with our agent, and record what happened.

The environment, the user simulator, the orchestrator and the evaluator are
τ²-bench's, unchanged. What this module owns is the agent (see agent.py) and
the shape of the record the loop later reflects over.
"""
import json
import os

from tau2.data_model.message import AssistantMessage, ToolMessage, MultiToolMessage, UserMessage
from tau2.evaluator.evaluator import EvaluationType, evaluate_simulation
from tau2.orchestrator.orchestrator import Orchestrator
from tau2.registry import registry
from tau2.user.user_simulator import UserSimulator

from agent import AreevRetailAgent

DOMAIN = "retail"


def build_env():
    return registry.get_env_constructor(DOMAIN)()


def tool_schemas(env):
    return [t.openai_schema for t in env.get_tools()]


def load_tasks(split="base"):
    """The benchmark's own tasks, in the benchmark's own order, restricted to
    one of its named splits. The loader takes the split name directly; the
    split file is read separately only so an unknown name fails loudly here
    rather than silently returning everything."""
    splits_loader = registry.get_task_splits_loader(DOMAIN)
    if splits_loader is not None:
        splits = splits_loader()
        if split not in splits:
            raise SystemExit("unknown split %r (known: %s)" % (split, ", ".join(sorted(splits))))
    return list(registry.get_tasks_loader(DOMAIN)(split))


def run_task(task, policy, tools_for_agent, agent_cmd, lessons_md, user_llm, user_llm_args,
             max_steps=60, max_errors=10, seed=300, evaluate=True):
    """One episode. Returns a record dict; never raises for an agent/user
    failure — a broken episode is data, and hiding it would flatter the run."""
    env = build_env()
    # The agent is handed OUR redacted view; the environment keeps its own.
    from tau2.environment.tool import Tool

    class _View:
        """A tool as the agent sees it: real callable, redacted schema."""
        def __init__(self, real, schema):
            self._real, self._schema = real, schema
        @property
        def openai_schema(self):
            return self._schema
        def __getattr__(self, k):
            return getattr(self._real, k)

    by_name = {t.openai_schema["function"]["name"]: t for t in env.get_tools()}
    views = [_View(by_name[s["function"]["name"]], s) for s in tools_for_agent
             if s["function"]["name"] in by_name]

    agent = AreevRetailAgent(tools=views, domain_policy=policy, agent_cmd=agent_cmd,
                             lessons_md=lessons_md)
    user = UserSimulator(llm=user_llm, llm_args=dict(user_llm_args),
                         instructions=str(task.user_scenario), tools=None)
    orch = Orchestrator(domain=DOMAIN, agent=agent, user=user, environment=env, task=task,
                        max_steps=max_steps, max_errors=max_errors, seed=seed)
    rec = {"task_id": task.id, "lessons_in_prompt": lessons_md.count("\n- ")}
    try:
        sim = orch.run()
    except Exception as e:  # an episode that died is an outcome, not a crash
        rec.update({"error": "%s: %s" % (type(e).__name__, str(e)[:300]),
                    "reward": 0.0, "db_match": False, "terminated": "harness_error",
                    "steps": 0, "tool_errors": 0, "usage": agent.usage()})
        return rec, []

    rec["terminated"] = str(getattr(sim, "termination_reason", "") or "")
    rec["steps"] = len(sim.messages or [])
    rec["usage"] = agent.usage()

    if evaluate:
        try:
            # DB-only: no LLM judge, so the score depends on nothing's opinion
            # and costs nothing. τ²'s ALL would additionally run a judge model.
            info = evaluate_simulation(simulation=sim, task=task,
                                       evaluation_type=EvaluationType.ENV,
                                       solo_mode=False, domain=DOMAIN)
            rec["reward"] = float(getattr(info, "reward", 0.0) or 0.0)
            db = getattr(info, "db_check", None)
            rec["db_match"] = bool(getattr(db, "db_match", False)) if db else None
        except Exception as e:
            rec.update({"reward": 0.0, "db_match": None,
                        "eval_error": "%s: %s" % (type(e).__name__, str(e)[:200])})

    calls, errors, texts = [], 0, []
    for m in sim.messages or []:
        if isinstance(m, AssistantMessage):
            for tc in m.tool_calls or []:
                calls.append({"tool": tc.name, "args": tc.arguments})
            if m.content:
                texts.append(("agent", m.content))
        elif isinstance(m, UserMessage) and m.content:
            texts.append(("customer", m.content))
        else:
            subs = [m] if isinstance(m, ToolMessage) else (
                getattr(m, "tool_messages", []) if isinstance(m, MultiToolMessage) else [])
            for sub in subs:
                if getattr(sub, "error", False):
                    errors += 1
                    if calls:
                        calls[-1]["error"] = (sub.content or "")[:200]
    rec["tool_errors"] = errors
    rec["tools_called"] = [c["tool"] for c in calls]
    rec["transcript"] = texts
    return rec, calls
