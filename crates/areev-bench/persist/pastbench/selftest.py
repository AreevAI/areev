#!/usr/bin/env python3
"""Keyless self-test of the Areev PAST-Bench backend: no model, no judge, no
network. Exercises the file operations the adapter performs, the
Hermes-shaped rendering the benchmark snapshots, the fixture import, the
in-process persistence tools, a governed close with the deterministic
analyzers and the keyless reviewer, and the benchmark's own diff and
retrieval-signal functions over the result.

    cd PAST-Bench && . .venv/bin/activate && python <areev>/crates/areev-bench/persist/pastbench/selftest.py

Passing proves the plumbing, never a learning claim.
"""
from __future__ import annotations

import json
import os
import shutil
import sys
import tempfile
from pathlib import Path

HERE = os.path.dirname(os.path.abspath(__file__))
for p in (HERE, os.path.dirname(HERE)):
    if p not in sys.path:
        sys.path.insert(0, p)

from past_bench.models.content import TextBlock, ToolResultBlock, ToolUseBlock  # noqa: E402
from past_bench.models.message import Message  # noqa: E402
from past_bench.runner.self_evolve import compute_retrieval_signals, diff_artifact_snapshots  # noqa: E402
from past_bench.runtime.protocol import RuntimeModelConfig, StartSessionRequest  # noqa: E402

import areev_backend as ab  # noqa: E402


def check(cond, what):
    print(("  ok   " if cond else "  FAIL ") + what)
    if not cond:
        raise SystemExit("selftest failed: " + what)


def main():
    root = Path(tempfile.mkdtemp(prefix="areev-pastbench-selftest-"))
    try:
        run(root)
    finally:
        shutil.rmtree(root, ignore_errors=True)
    print("selftest passed")


def run(root):
    backend = ab.make_backend("areev-governed")
    state_root = root / "family_homes" / "FAM01" / backend.state_root_name
    backend.reset_state(state_root)

    # 1. a Hermes-shaped fixture imports as grains
    fixture = root / "fixture"
    (fixture / "memories").mkdir(parents=True)
    (fixture / "memories" / "MEMORY.md").write_text("Always file dates as YYYY/MM/DD\n§\nUse TSV for exports",
                                                    encoding="utf-8")
    (fixture / "memories" / "USER.md").write_text("Prefers replies under five lines", encoding="utf-8")
    (fixture / "skills" / "weekly_export").mkdir(parents=True)
    (fixture / "skills" / "weekly_export" / "SKILL.md").write_text(
        "---\nname: weekly_export\ndescription: Export the weekly report\n---\n\n1. notes_list\n2. notes_get\n3. notes_share",
        encoding="utf-8")
    (fixture / "sessions").mkdir()
    (fixture / "sessions" / "s1.json").write_text(json.dumps({"messages": [
        {"role": "user", "content": "Last week we agreed the vendor is Blue Spruce Catering."},
        {"role": "assistant", "content": "Noted: Blue Spruce Catering, contact Dana Kim."}]}), encoding="utf-8")
    backend.materialize_inputs(state_root=state_root, initial_home_fixture_dir=fixture, preseed_artifacts_dir=None)
    before = backend.snapshot_before(state_root, include_contents=True)
    check(before["memory_file_exists"] and len(before["memory_entries"]) == 2, "fixture notes rendered (%d)" % len(before["memory_entries"]))
    check(before["user_file_exists"] and before["user_entries"] == ["Prefers replies under five lines"], "fixture profile rendered")
    check(before["skill_count"] == 1 and "weekly_export" in before["skill_names"], "fixture skill rendered")
    backend.materialize_inputs(state_root=state_root, initial_home_fixture_dir=fixture, preseed_artifacts_dir=None)
    again = backend.snapshot_before(state_root)
    check(len(again["memory_entries"]) == 2 and again["skill_count"] == 1, "re-import is idempotent")

    # 2. a session: injection, tools, recording, close
    episode_dir = root / "with_persistence" / "03_learn_a"
    artifacts_dir = episode_dir / "artifacts"
    tool_config = {"memory_enabled": True, "user_profile_enabled": True, "skills_enabled": True,
                   "session_search_enabled": True}
    extra = backend.build_extra_body(state_root=state_root, artifacts_dir=artifacts_dir, persistence_enabled=True,
                                     sequence=None, family_id="FAM01", review_wait_s=0.0, tool_config=tool_config)
    request = StartSessionRequest(
        session_id="sess-1", agent_name="areev-governed", task_id="FAM01_LEARN_A_001", task_name="learn a",
        max_turns=5, timeout_seconds=60,
        initial_messages=[Message(role="system", content=[TextBlock(text="You are a helpful assistant.")]),
                          Message(role="user", content=[TextBlock(text="From now on, write dates as 2026/05/06 and share notes with Paula Reed.")])],
        tools=[], model=RuntimeModelConfig(model_id="test/model", api_key="k", base_url="http://127.0.0.1:9",
                                           extra_body=extra))
    adapter = ab.AreevAdapter(spec=None, request=request)
    system = adapter._messages[0].content[0].text
    check("## Persistent memory" in system and "YYYY/MM/DD" in system and "weekly_export" in system, "memory injected into the system prompt")
    check(len(adapter._mem_tools) == 5, "five persistence tools exposed (%d)" % len(adapter._mem_tools))

    calls = [
        ToolUseBlock(id="c1", name="memory", input={"action": "add", "target": "memory", "content": "Share every finance note with Paula Reed"}),
        ToolUseBlock(id="c2", name="memory", input={"action": "replace", "target": "memory", "old_text": "Use TSV", "content": "Use TSV for exports, tab-separated with a header row"}),
        ToolUseBlock(id="c3", name="memory", input={"action": "search", "query": "dates"}),
        ToolUseBlock(id="c4", name="skill_manage", input={"action": "patch", "name": "weekly_export", "old_text": "3. notes_share", "new_text": "3. notes_share to Paula Reed"}),
        ToolUseBlock(id="c5", name="skill_manage", input={"action": "create", "name": "vendor_reply", "content": "Reply to catering requests\n\n1. inbox_list\n2. inbox_read\n3. reply_send"}),
        ToolUseBlock(id="c6", name="session_search", input={"query": "vendor"}),
        ToolUseBlock(id="c7", name="skill_view", input={"name": "vendor_reply"}),
        ToolUseBlock(id="c8", name="memory", input={"action": "remove", "target": "user", "old_text": "five lines"}),
    ]
    assistant = Message(role="assistant", content=[TextBlock(text="Saving what I learned.")] + calls)
    adapter._log_assistant(assistant)
    results = [adapter._run_mem_tool(b) for b in calls]
    texts = [r.content[0].text for r in results]
    for i, r in enumerate(results):
        check(not r.is_error, "tool call %s ok: %s" % (calls[i].name, texts[i][:60]))
    check("2026/05/06" in texts[2] or "YYYY/MM/DD" in texts[2], "memory search finds the date rule")
    check("Blue Spruce" in texts[5], "session_search reaches the seeded session")
    check("inbox_read" in texts[6], "skill_view returns the steps")
    adapter._record_tool("notes_share", {"note_id": "N1"}, ToolResultBlock(tool_use_id="t1", content=[TextBlock(text="shared")]))
    adapter.close()

    after = backend.snapshot_after(artifacts_dir)
    check(after["memory_file_exists"], "MEMORY.md rendered after close")
    check(not after["user_file_exists"], "USER.md gone after the profile entry was removed")
    entries = after["memory_entries"]
    check(any("Paula Reed" in e for e in entries) and any("header row" in e for e in entries), "add and replace landed")
    check(not any(e == "Use TSV for exports" for e in entries), "replaced entry no longer live")
    check(after["skill_count"] == 2 and "vendor_reply" in after["skill_names"], "skills rendered (%d)" % after["skill_count"])
    check("Paula Reed" in after["skill_docs"]["weekly_export"]["content"], "skill patch landed")
    it = after["internal_tools"]
    check(it["memory_write_count"] == 3 and it["memory_read_count"] == 1, "memory writes=%d reads=%d counted" % (it["memory_write_count"], it["memory_read_count"]))
    # The evalset-run summary the governed arm journals must be the shape the
    # loop's fail-closed reader accepts: a run_id and INTEGER counts. Three
    # full runs recorded no verdict because this was a boolean (PERSIST.md
    # §11 #24); the contract is pinned here so it cannot regress silently.
    s = ab.eval_run_summary({"task_score": 0.234, "passed": False}, "02_x_learn_a")
    check(s["run_id"] == "02_x_learn_a" and s["passed"] == 0 and s["failed"] == 1
          and type(s["passed"]) is int and type(s["failed"]) is int and s["task_score"] == 0.234,
          "journaled eval run is loop-readable: run_id + integer counts (%r)" % (s,))
    s = ab.eval_run_summary({"task_score": 1.0, "passed": True}, "05_x_eval_far")
    check(s["passed"] == 1 and s["failed"] == 0, "a passed episode counts as 1/0")
    # The cold baseline is graded on a different schedule from the rest: its
    # directory is back-filled into the variant BEFORE the score lands, so a
    # search that stops at the first directory it finds reads nothing and the
    # family's first lesson is proposed with no baseline (PERSIST.md §11 #25).
    # The search must continue into shared_cold until a SCORE is found.
    import pathlib
    tmp = pathlib.Path(tempfile.mkdtemp())
    (tmp / "with_persistence" / "01_cold").mkdir(parents=True)
    (tmp / "with_persistence" / "01_cold" / "t.jsonl").write_text('{"type":"turn"}\n')  # copied pre-grading
    (tmp / "shared_cold" / "01_cold").mkdir(parents=True)
    (tmp / "shared_cold" / "01_cold" / "t.jsonl").write_text('{"task_score":0.772,"passed":false}\n')
    variant = tmp / "with_persistence"
    found = None
    for where in (variant, variant.parent / "shared_cold"):
        prev = ab._latest_episode_below(where, 2)
        if prev is None:
            continue
        found = ab._episode_score(prev)
        if found:
            break
    check(found is not None and found["task_score"] == 0.772 and found["failed"] == 1,
          "the cold baseline is found in shared_cold when the variant copy has no score yet (%r)" % (found,))
    check(ab._episode_score(variant / "01_cold") is None, "an ungraded episode yields no score rather than a guess")
    check(it["skill_create_count"] == 1 and it["skill_update_count"] == 1, "skill create/update counted")
    check(it["session_search_calls"] == 1 and it["skill_read_count"] == 1, "session_search and skill_view counted")

    diff = diff_artifact_snapshots(before=before, after=after, rule_keywords=["Paula Reed", "YYYY/MM/DD"])
    check(diff["rule_keyword_hits"]["hit_rate"] == 1.0, "rule keywords hit in the after snapshot")
    check(diff["memory_entry_delta"] == 0 and len(diff["updated_rules"]) >= 1, "diff sees the replace as an update (delta %d)" % diff["memory_entry_delta"])
    signals = compute_retrieval_signals(dispatches=[], artifact_before=before, internal_tools=it, expected_signal="memory")
    check(signals["used_expected_signal"] and signals["memory_injection_count"] == 1, "memory injection counted as the expected signal")

    ledger = json.loads((artifacts_dir / "areev_ledger.json").read_text())
    gov = ledger["governance"]
    check(isinstance(gov, dict) and "error" not in gov, "governed close ran the loop: %s" % json.dumps(gov)[:160])
    check(ledger["tool_calls"] == 1 and len(ledger["memory_ops"]) == 8, "ledger counts tool and memory ops")

    # 3. history: anchors clone the file, a fresh reset empties it
    anchors = root / "anchors"
    backend.clone_state(state_root, anchors / "post_learn")
    backend.reset_state(state_root)
    check(not backend.db_path(state_root).exists(), "reset removes the file")
    backend.clone_state(anchors / "post_learn", state_root)
    restored = backend.snapshot_before(state_root)
    check(len(restored["memory_entries"]) == len(entries) and restored["skill_count"] == 2, "anchor restore keeps the memory")

    # 4. without persistence: no tools, no injection, no file
    extra_off = backend.build_extra_body(state_root=state_root, artifacts_dir=root / "without" / "artifacts",
                                         persistence_enabled=False, sequence=None, family_id="FAM01",
                                         review_wait_s=0.0, tool_config={k: False for k in tool_config})
    req_off = request.model_copy(update={"session_id": "sess-off", "model": RuntimeModelConfig(
        model_id="test/model", api_key="k", base_url="http://127.0.0.1:9", extra_body=extra_off)})
    off = ab.AreevAdapter(spec=None, request=req_off)
    check(not off._mem_tools and "## Persistent memory" not in off._messages[0].content[0].text, "no persistence: no tools, no injection")
    off.close()
    off_after = backend.snapshot_after(root / "without" / "artifacts")
    check(not off_after["memory_file_exists"] and off_after["skill_count"] == 0, "no persistence: nothing rendered")


if __name__ == "__main__":
    main()
