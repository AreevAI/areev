#!/usr/bin/env python3
"""The keyless floor: everything in the bridge that does not need a model.

This is what CI can run. It proves the plumbing -- error parsing, recording,
the two prompt blocks, the reviewer's deterministic half, the agent's
registration and its prompt seam -- still works, so that a harness which has
rotted between paid runs fails here rather than silently producing a number.

It proves NOTHING about learning. No model is called; the reviewer is absent,
which by construction approves nothing.

    python3 selftest.py            # needs `areev` importable
    python3 selftest.py --no-store # skip the parts needing the binding
"""
from __future__ import annotations

import argparse
import importlib.util
import json
import os
import shutil
import sys
import tempfile

HERE = os.path.dirname(os.path.abspath(__file__))


_LOADED: dict[str, object] = {}


def _sibling(name: str):
    """Load once and cache: re-executing agent.py would re-run its
    `@Agent.register`, which raises on the second call."""
    if name not in _LOADED:
        spec = importlib.util.spec_from_file_location(
            "areev_appworld_" + name, os.path.join(HERE, name + ".py")
        )
        module = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(module)
        _LOADED[name] = module
    return _LOADED[name]


memory = _sibling("memory")

FAILURES: list[str] = []


def check(name: str, condition: bool, detail: str = "") -> None:
    if condition:
        print(f"  ok   {name}")
    else:
        print(f"  FAIL {name} {detail}")
        FAILURES.append(name)


# -- 1. the error parser, on real environment output ------------------------

REAL_401 = """Execution failed. Traceback:
  File "<python-input>", line 1, in <module>
    contact_relationships = apis.phone.show_contact_relationships()
                            ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
Exception: Response status code is 401:
{"message":"You are either not authorized to access this phone API endpoint or your access token is missing, invalid or expired."}
"""

REAL_PARAM = """Execution failed. Traceback:
Exception: Unexpected parameter 'phone_number' passed to the search_users API of the venmo app. Allowed parameters are: ['access_token', 'query', 'page_index', 'page_limit']
"""


def test_error_parser() -> None:
    print("error parsing")
    got = memory.summarize_error("apis.phone.show_contact_relationships()", REAL_401)
    check("401 is recognised", got is not None)
    check("401 names its app", got and got["app"] == "phone", str(got))
    check("401 names its api", got and got["api"] == "show_contact_relationships", str(got))
    check("401 is classified by status", got and got["kind"] == "http_401", str(got))

    got = memory.summarize_error("apis.venmo.search_users(phone_number='x')", REAL_PARAM)
    check("bad parameter is recognised", got is not None)
    check("bad parameter keeps its message",
          got and "Unexpected parameter" in got["message"], str(got))

    # A chunk that calls several APIs: attribution must follow the evidence,
    # not the last line of code.
    multi = """Execution failed. Traceback:
Exception: Unexpected parameter 'name' passed to the search_users API of the venmo app. Allowed parameters are: ['access_token', 'query']
"""
    got = memory.summarize_error(
        "u = apis.venmo.search_users(name='x')\nr = apis.venmo.create_payment_request(u)", multi
    )
    check("the message's own endpoint wins over the last call in the chunk",
          got and (got["app"], got["api"]) == ("venmo", "search_users"), str(got))

    multi_401 = """Execution failed. Traceback:
  File "<python-input>", line 2, in <module>
    notes = apis.simple_note.search_notes()
            ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
Exception: Response status code is 401:
"""
    got = memory.summarize_error(
        "token = apis.supervisor.show_account_passwords()\nnotes = apis.simple_note.search_notes()\nx = apis.spotify.show_album(1)",
        multi_401,
    )
    check("a 401 is attributed to the line in the traceback",
          got and (got["app"], got["api"]) == ("simple_note", "search_notes"), str(got))

    check("a clean interaction is not an error",
          memory.summarize_error("apis.venmo.show_account()", "[{'a': 1}]") is None)
    check("empty output is not an error", memory.summarize_error("x = 1", "") is None)


# -- 2. the deterministic half of the reviewer ------------------------------


def test_reviewer_without_a_model() -> None:
    print("reviewer, keyless")
    ok, why = memory.review_recommendation("Always log in before calling an app API.", set())
    check("no reviewer approves nothing", ok is False, why)
    check("and says why", "no reviewer" in why, why)

    in_force = {memory.normalize_rule("Always log in before calling an app API.")}
    ok, why = memory.review_recommendation("Always log in before calling an app API.", in_force)
    check("an identical rule is refused", ok is False, why)
    check("as already in force", why == "already in force", why)

    ok, why = memory.review_recommendation(
        "Always log in before you call an app API.", in_force
    )
    check("a near-duplicate rule is refused", ok is False, why)
    check("as restating one in force", "restates" in why, why)

    ok, why = memory.review_recommendation("", set())
    check("an empty rule is refused", ok is False, why)


def test_proposal_parsing() -> None:
    print("proposal parsing")
    kind, text = memory.parse_proposal('record lesson: "Authenticate first."')
    check("a lesson is parsed", (kind, text) == ("lesson", "Authenticate first."), f"{kind}/{text}")
    kind, _ = memory.parse_proposal("record fact: episode_x had 3 errors")
    check("a fact is parsed", kind == "fact", kind)
    kind, _ = memory.parse_proposal("the agent could be faster")
    check("anything else is advisory", kind == "advisory", kind)


# -- 3. the store round trip ------------------------------------------------


def test_store_round_trip() -> None:
    print("store round trip")
    try:
        import areev  # noqa: F401
    except ImportError:
        check("areev binding importable", False, "build it with maturin into this venv")
        return

    workdir = tempfile.mkdtemp(prefix="appworld-selftest-")
    # deliberately two levels down and absent, the shape make_configs.py emits
    db_path = os.path.join(workdir, "governed", "learn.db")
    try:
        errors = [
            memory.summarize_error("apis.phone.show_contact_relationships()", REAL_401),
            memory.summarize_error("apis.venmo.search_users(phone_number='x')", REAL_PARAM),
        ]
        memory.with_memory(
            db_path,
            memory.RUNNER,
            lambda db: memory.record_episode(db, "abc123_1", errors, steps=11, hit_cap=False),
        )

        block = memory.block_for(db_path, "passive")
        check("passive block is written", bool(block), repr(block[:80]))
        check("a memory opens in a directory that does not exist yet",
              os.path.exists(db_path))
        check("passive block carries the 401", "401" in block, block[:200])
        check("passive block names the api",
              "phone.show_contact_relationships" in block, block[:200])

        governed = memory.block_for(db_path, "governed")
        check("governed block is empty with no approved rule", governed == "", repr(governed))

        check("arm A reads nothing at all", memory.block_for(db_path, "none") == "")

        # The outcome record must carry the episode's shape and NOT its score.
        # Scoped `"appworld.*"`, the way every read in the bridge is: the base
        # namespace plus every per-app child.
        def read_facts(db):
            return json.loads(db.cal(
                'RECALL facts WHERE namespace = "%s" LIMIT 400 FORMAT json'
                % memory.NS_SCOPE))["grains"]

        facts = memory.with_memory(db_path, memory.RUNNER, read_facts)
        outcome = [f for f in (g.get("fields", {}) for g in facts)
                   if f.get("relation") == "outcome"]
        check("one outcome record per episode", len(outcome) == 1, str(len(outcome)))
        body = json.loads(outcome[0]["object"]) if outcome else {}
        check("the outcome counts the errors", body.get("api_errors") == 2, str(body))
        check("the outcome records how it ended", body.get("ended") == "stopped", str(body))
        for forbidden in ("success", "score", "tgc", "reward", "passed"):
            check(f"the outcome does not carry {forbidden!r}", forbidden not in body, str(body))

        # A second episode must aggregate rather than duplicate in the block.
        memory.with_memory(
            db_path,
            memory.RUNNER,
            lambda db: memory.record_episode(db, "abc123_2", errors[:1], steps=4, hit_cap=True),
        )
        block2 = memory.block_for(db_path, "passive")
        check("a repeated error is counted, not listed twice",
              "(2x)" in block2, block2[:300])

        # A held-out arm must be unable to change what it is evaluated on.
        check("a read-only memory still reads",
              bool(memory.block_for(db_path, "passive", read_only=True)))
        try:
            memory.with_memory(
                db_path, memory.RUNNER,
                lambda db: db.add("fact", json.dumps(
                    {"subject": "s", "relation": "lesson", "object": "x"}), ns=memory.NS),
                read_only=True,
            )
            check("a read-only memory refuses writes", False, "the write succeeded")
        except ValueError as exc:
            check("a read-only memory refuses writes", "STO-E004" in str(exc), str(exc)[:60])
    finally:
        shutil.rmtree(workdir, ignore_errors=True)


# -- 4. the agent's registration and prompt seam ----------------------------


def test_agent_registration(root: str) -> None:
    print("agent registration")
    sys.path.insert(0, root)
    try:
        from appworld_agents.code.simplified.agent import Agent
    except ImportError as exc:
        check("appworld_agents importable", False, str(exc))
        return
    _sibling("agent")
    check("areev_react_code_agent is registered",
          Agent.is_registered("areev_react_code_agent"))

    klass = Agent.by_name("areev_react_code_agent")
    for mode in ("none", "passive", "governed"):
        check(f"{mode!r} is an accepted mode", mode in _sibling("agent").MODES)
    try:
        klass(memory_mode="nonsense")
        check("an unknown mode is refused", False)
    except ValueError as exc:
        check("an unknown mode is refused", "memory_mode" in str(exc), str(exc)[:80])

    try:
        klass(memory_mode="governed")
        check("a memory arm without a db is refused", False)
    except ValueError as exc:
        check("a memory arm without a db is refused", "memory_db" in str(exc), str(exc)[:80])


# -- 5. the manipulation itself: does the block reach the prompt? -----------


def test_prompt_seam(root: str) -> None:
    """The one thing that, if it silently failed, would make every later
    number meaningless: the block has to land in the message the model reads,
    and NOTHING ELSE may differ between the arms."""
    print("prompt seam")
    sys.path.insert(0, root)
    try:
        from appworld import load_task_ids
        from appworld.environment import AppWorld
        from appworld_agents.code.simplified.agent import Agent
    except ImportError as exc:
        check("appworld importable", False, str(exc))
        return
    _sibling("agent")

    prompt_file = os.path.join(root, "experiments", "prompts",
                               "react_code_agent", "instructions.txt")
    if not os.path.exists(prompt_file):
        check("the stock prompt file exists", False, prompt_file)
        return

    workdir = tempfile.mkdtemp(prefix="appworld-seam-")
    db_path = os.path.join(workdir, "memory.db")
    rule = "Authenticate with the app before calling any of its other APIs."
    try:
        errors = [memory.summarize_error("apis.phone.show_contact_relationships()", REAL_401)]
        memory.with_memory(
            db_path, memory.RUNNER,
            lambda db: memory.record_episode(db, "seam_1", errors, steps=3, hit_cap=False),
        )
        # An applied recommendation lands as a lesson Fact; write one directly
        # so the governed block has something to render without a model.
        memory.with_memory(
            db_path, memory.RUNNER,
            lambda db: db.add("fact", json.dumps(
                {"subject": "appworld_agent", "relation": "lesson", "object": rule}
            ), ns=memory.NS),
        )

        def build(mode, frozen=False):
            config = {
                "type": "areev_react_code_agent",
                "prompt_file_path": prompt_file,
                "memory_mode": mode,
                "model_config": {
                    "client_name": "openai", "api_type": "chat_completions",
                    "base_url": "https://openrouter.ai/api/v1",
                    "api_key_env_name": "OPENROUTER_API_KEY",
                    "name": "qwen/qwen3-30b-a3b-instruct-2507",
                    "temperature": 0.0, "use_cache": False,
                    "cost_per_token": {"input_cache_hit": 0.0, "input_cache_miss": 0.0,
                                       "input_cache_write": 0.0, "output": 0.0},
                },
            }
            if mode != "none":
                config["memory_db"] = db_path
            if frozen:
                config["memory_frozen"] = True
            return Agent.from_dict(config)

        task_id = load_task_ids("train")[0]
        prompts = {}
        for mode in ("none", "passive", "governed"):
            agent = build(mode)
            with AppWorld(task_id=task_id, experiment_name="selftest-seam") as world:
                agent.initialize(world)
            prompts[mode] = agent.messages[-1]["content"]

        check("arm A's prompt carries no block",
              "E. What went wrong" not in prompts["none"]
              and "E. Rules you have learned" not in prompts["none"])
        check("the passive block reaches the prompt",
              "E. What went wrong" in prompts["passive"], prompts["passive"][-200:])
        check("the passive block carries the real error text",
              "401" in prompts["passive"])
        check("the governed block reaches the prompt",
              "E. Rules you have learned" in prompts["governed"])
        check("the governed block carries the approved rule",
              rule in prompts["governed"])
        check("the governed arm does NOT leak raw experience",
              "E. What went wrong" not in prompts["governed"])

        # The arms must be the stock prompt PLUS a block, and nothing else.
        for mode in ("passive", "governed"):
            check(f"{mode} is arm A's prompt plus a block, byte for byte",
                  prompts[mode].startswith(prompts["none"]),
                  repr(prompts[mode][:len(prompts["none"])][-80:]))

        # A frozen arm reads its own copy and leaves the original alone --
        # otherwise B and B2 would be reading two different memories.
        frozen_agent = build("passive", frozen=True)
        check("a frozen arm reads a process-local copy",
              frozen_agent.memory_db != db_path, frozen_agent.memory_db)
        check("the frozen copy exists", os.path.exists(frozen_agent.memory_db))
        with AppWorld(task_id=task_id, experiment_name="selftest-seam") as world:
            frozen_agent.initialize(world)
        check("a frozen arm still gets its block",
              "E. What went wrong" in frozen_agent.messages[-1]["content"])
    finally:
        shutil.rmtree(workdir, ignore_errors=True)


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--root", default=os.environ.get("APPWORLD_ROOT", ""))
    ap.add_argument("--no-store", action="store_true")
    args = ap.parse_args()

    test_error_parser()
    test_reviewer_without_a_model()
    test_proposal_parsing()
    if not args.no_store:
        test_store_round_trip()
    if args.root:
        test_agent_registration(os.path.expanduser(args.root))
        test_prompt_seam(os.path.expanduser(args.root))

    print()
    if FAILURES:
        print(f"FAILED: {len(FAILURES)} check(s): {', '.join(FAILURES[:6])}")
        return 1
    print("all checks passed (plumbing only -- this proves no learning)")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
