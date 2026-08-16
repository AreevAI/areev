#!/usr/bin/env python3
"""Standalone checks for the Areev Hermes memory provider.

Runs without Hermes installed — the provider falls back to a shim base class
when `agent.memory_provider` is absent, so the logic is exercisable on its own.
Inside a Hermes install it subclasses the real ABC instead; that path is
covered by driving its real loader and MemoryManager (see ../README.md).

    python3 examples/hermes/test_provider.py
"""

import importlib.util
import json
import os
import sys
import tempfile

_HERE = os.path.dirname(os.path.abspath(__file__))

# This directory holds a package named `areev/` — the plugin — and Python puts
# a script's own directory on sys.path automatically, which makes that package
# shadow the installed `areev` the plugin imports. Drop it. Hermes's loader
# never adds the plugin directory to sys.path (it goes through
# spec_from_file_location), so this hazard is specific to running a script from
# in here.
sys.path[:] = [p for p in sys.path if os.path.abspath(p or ".") != _HERE]

# Load the plugin the way Hermes's loader does: by file location, under a
# distinct module name.
_PLUGIN = os.path.join(_HERE, "areev", "__init__.py")
_spec = importlib.util.spec_from_file_location("_areev_hermes_provider", _PLUGIN)
_mod = importlib.util.module_from_spec(_spec)
sys.modules[_spec.name] = _mod
_spec.loader.exec_module(_mod)

AreevMemoryProvider, _lit, register = _mod.AreevMemoryProvider, _mod._lit, _mod.register

FAILURES = []


def check(label, condition, detail=""):
    print(f"  {'ok  ' if condition else 'FAIL'} {label}{'' if condition else f'  -- {detail}'}")
    if not condition:
        FAILURES.append(label)


def provider(home, **kwargs):
    p = AreevMemoryProvider()
    opts = {"hermes_home": home, "platform": "cli", "agent_context": "primary",
            "agent_identity": "coder", "user_id": "sam"}
    opts.update(kwargs)
    p.initialize("sess-now", **opts)
    return p


def main():
    home = tempfile.mkdtemp()

    print("\nCAL literal escaping")
    check("quotes escaped", _lit('say "hi"') == '"say \\"hi\\""', _lit('say "hi"'))
    check("backslash escaped", _lit("a\\b") == '"a\\\\b"', _lit("a\\b"))

    print("\nlifecycle")
    p = provider(home)
    check("available", p.is_available())
    check("name", p.name == "areev")
    check("db created under $HERMES_HOME",
          os.path.exists(os.path.join(home, "areev", "coder.db")))
    check("system prompt block non-empty", bool(p.system_prompt_block()))
    check("one tool exposed", [t["name"] for t in p.get_tool_schemas()] == ["areev_memory"])

    print("\nwrites")
    p.sync_turn("I always want a window seat", "Noted.", session_id="sess-old")
    p.sync_turn("current turn", "current reply", session_id="sess-now")
    p.on_memory_write("add", "user", "Prefers window seat.", {"session_id": "sess-now"})
    p.on_memory_write("add", "memory", "Repo lives in ~/src.", {"session_id": "sess-now"})

    profile = json.loads(p.handle_tool_call("areev_memory",
                                            {"action": "recall", "subject": "user:sam"}))
    check("USER.md entry became a profile fact",
          any(g["fields"]["object"] == "Prefers window seat." for g in profile), profile)
    agent_notes = json.loads(p.handle_tool_call("areev_memory",
                                                {"action": "recall", "subject": "agent:coder"}))
    check("MEMORY.md entry is scoped to the agent, not the user",
          any(g["fields"]["object"] == "Repo lives in ~/src." for g in agent_notes))

    # The audit leg is observations (Event roles are validated to the
    # conversation vocabulary in areev >= 1.2).
    audit = json.loads(p.handle_tool_call(
        "areev_memory", {"action": "cal", "query": "RECALL observations RECENT 10"}))
    check("every memory edit is audited", len(audit["grains"]) == 2, audit)

    print("\nprefetch")
    block = p.prefetch("what seat do I like?", session_id="sess-now")
    check("profile is injected", "Prefers window seat." in block, block)
    check("prior session turn is injected", "I always want a window seat" in block, block)
    check("current session is NOT re-injected", "current reply" not in block, block)
    check("audit rows are NOT injected", "add user:" not in block, block)

    print("\nsearch")
    hits = json.loads(p.handle_tool_call("areev_memory", {"action": "search", "query": "window"}))
    check("free text finds the mirrored profile entry by default",
          any("window" in g["fields"].get("object", "") for g in hits), hits)

    # With no lexical and no vector leg there is nothing to rank on. Saying so
    # beats returning [], which reads as "no matching memories".
    noleg_home = tempfile.mkdtemp()
    with open(os.path.join(noleg_home, "areev.json"), "w") as fh:
        json.dump({"index_text": False}, fh)
    noleg = provider(noleg_home)
    check("no-leg search explains itself rather than returning []",
          "no free-text leg" in noleg.handle_tool_call("areev_memory",
                                                       {"action": "search", "query": "seat"}))

    print("\nfailure modes")
    check("history needs both subject and relation",
          "needs both" in p.handle_tool_call("areev_memory",
                                             {"action": "history", "subject": "user:sam"}))
    check("unknown action is reported",
          "unknown action" in p.handle_tool_call("areev_memory", {"action": "nope"}))
    check("bad CAL is reported, not raised",
          "error" in json.loads(p.handle_tool_call("areev_memory",
                                                   {"action": "cal", "query": "DELETE everything"})))
    p._db = None
    check("prefetch fails open when the store is gone", p.prefetch("x") == "")

    print("\nnon-primary contexts are read-only")
    ro_home = tempfile.mkdtemp()
    ro = provider(ro_home, agent_context="cron")
    before = json.loads(ro._db.stats())["grains"]
    ro.sync_turn("cron system prompt", "cron reply", session_id="c1")
    ro.on_memory_write("add", "user", "should not be written", {})
    check("cron context wrote nothing", json.loads(ro._db.stats())["grains"] == before)

    print("\nsaved-query override")
    p2 = provider(tempfile.mkdtemp())
    check("no override by default", not p2._has_saved_prefetch())
    p2._db.cal('DEFINE QUERY "hermes_prefetch"($user, $query) DESCRIPTION "custom" AS '
               '{ ASSEMBLE "s" FROM profile: (RECALL facts WHERE subject = $user) '
               'BUDGET 200 tokens FORMAT markdown }')
    del p2._saved_prefetch  # re-probe; normally cached for the process
    check("file-defined query is picked up", p2._has_saved_prefetch())
    p2.on_memory_write("add", "user", "Custom path works.", {})
    check("override is what runs", "Custom path works." in p2.prefetch("anything"))

    print("\nplugin entry point")
    captured = []
    register(type("Ctx", (), {"register_memory_provider": lambda self, prov: captured.append(prov)})())
    check("register() hands back a provider",
          len(captured) == 1 and captured[0].name == "areev")

    print()
    if FAILURES:
        print(f"{len(FAILURES)} FAILED: {', '.join(FAILURES)}")
        return 1
    print("all checks passed")
    return 0


if __name__ == "__main__":
    sys.exit(main())
