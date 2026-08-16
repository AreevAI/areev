"""AreevCheckpointSaver conformance — driven both directly (the reference
semantics: tree identity, writes upsert, blobs, delta history) and through
a real compiled LangGraph StateGraph (the framework is the oracle for the
config/metadata plumbing)."""

from __future__ import annotations

import operator
from typing import Annotated, TypedDict

import pytest
from langgraph.checkpoint.base import empty_checkpoint
from langgraph.graph import END, START, StateGraph

from areev_langgraph import AreevCheckpointSaver


@pytest.fixture()
def saver(tmp_path):
    return AreevCheckpointSaver(str(tmp_path / "threads"))


def _cfg(thread, ns="", ckpt=None):
    c = {"configurable": {"thread_id": thread, "checkpoint_ns": ns}}
    if ckpt:
        c["configurable"]["checkpoint_id"] = ckpt
    return c


class State(TypedDict):
    total: Annotated[int, operator.add]


def test_real_graph_runs_resumes_and_time_travels(saver):
    builder = StateGraph(State)
    builder.add_node("bump", lambda s: {"total": 1})
    builder.add_node("double", lambda s: {"total": s["total"]})
    builder.add_edge(START, "bump")
    builder.add_edge("bump", "double")
    builder.add_edge("double", END)
    graph = builder.compile(checkpointer=saver)

    out = graph.invoke({"total": 0}, _cfg("t1"))
    assert out["total"] == 2

    # The full history is a TREE of distinct checkpoint identities.
    history = list(graph.get_state_history(_cfg("t1")))
    assert len(history) >= 3
    ids = [h.config["configurable"]["checkpoint_id"] for h in history]
    assert len(set(ids)) == len(ids), "one identity per checkpoint"

    # Time-travel: resume FROM an earlier checkpoint forks the thread state.
    earlier = history[-2].config  # after bump, before double
    replayed = graph.invoke(None, earlier)
    assert replayed["total"] >= 1

    # A second invoke on the same thread continues from the latest state.
    out2 = graph.invoke({"total": 10}, _cfg("t1"))
    assert out2["total"] > 10


def test_threads_are_isolated_files(saver, tmp_path):
    builder = StateGraph(State)
    builder.add_node("bump", lambda s: {"total": 1})
    builder.add_edge(START, "bump")
    builder.add_edge("bump", END)
    graph = builder.compile(checkpointer=saver)

    graph.invoke({"total": 0}, _cfg("alpha"))
    graph.invoke({"total": 5}, _cfg("beta"))
    files = sorted(
        p.name
        for p in (tmp_path / "threads").glob("*.db")
        if not p.name.endswith(".telemetry.db")
    )
    # Filenames carry a case-fold tag (`<slug>-<sha8>.db`) so thread ids
    # differing only by case never collide on case-insensitive filesystems.
    assert len(files) == 2, "one memory file per thread"
    stems = sorted(f.rsplit("-", 1)[0] for f in files)
    assert stems == ["alpha", "beta"], files

    # delete_thread erases ONE file; the other thread is untouched.
    saver.delete_thread("alpha")
    assert saver.get_tuple(_cfg("alpha")) is None
    assert saver.get_tuple(_cfg("beta")) is not None
    remaining = [
        p.name
        for p in (tmp_path / "threads").glob("*.db")
        if not p.name.endswith(".telemetry.db")
    ]
    assert len(remaining) == 1 and remaining[0].startswith("beta-"), remaining


def test_put_writes_upserts_never_appends(saver):
    ckpt = empty_checkpoint()
    cfg = saver.put(_cfg("t2", ns=""), ckpt, {"source": "input", "step": -1}, {})
    # Same (task, idx): a regular write is first-commit-wins…
    saver.put_writes(cfg, [("ch", "first")], task_id="task-1")
    saver.put_writes(cfg, [("ch", "second")], task_id="task-1")
    tup = saver.get_tuple(cfg)
    assert [w[2] for w in tup.pending_writes] == ["first"]
    # …while special writes (idx < 0) overwrite, superseding the head.
    saver.put_writes(cfg, [("__error__", "boom-1")], task_id="task-1")
    saver.put_writes(cfg, [("__error__", "boom-2")], task_id="task-1")
    tup = saver.get_tuple(cfg)
    errors = [w[2] for w in tup.pending_writes if w[1] == "__error__"]
    assert errors == ["boom-2"], "retry with different bytes supersedes"


def test_reput_same_checkpoint_id_supersedes_not_siblings(saver):
    ckpt = empty_checkpoint()
    saver.put(_cfg("t3"), ckpt, {"source": "input", "step": -1, "tag": "a"}, {})
    saver.put(_cfg("t3"), ckpt, {"source": "input", "step": -1, "tag": "b"}, {})
    tuples = list(saver.list(_cfg("t3")))
    assert len(tuples) == 1, "same id = one identity (superseded, not duplicated)"
    assert tuples[0].metadata["tag"] == "b"


def test_metadata_filter_and_run_id_delete(saver):
    ckpt1 = empty_checkpoint()
    cfg = {"configurable": {"thread_id": "t4", "checkpoint_ns": "", "run_id": "run-A"}}
    saver.put(cfg, ckpt1, {"source": "input", "step": -1}, {})
    ckpt2 = empty_checkpoint()
    cfg2 = {"configurable": {"thread_id": "t4", "checkpoint_ns": "", "run_id": "run-B"}}
    saver.put(cfg2, ckpt2, {"source": "loop", "step": 0}, {})

    only_a = list(saver.list(_cfg("t4"), filter={"run_id": "run-A"}))
    assert len(only_a) == 1

    # delete_for_runs removes exactly run-A's checkpoints.
    saver.delete_for_runs(["run-A"])
    remaining = list(saver.list(_cfg("t4")))
    assert [t.metadata.get("run_id") for t in remaining] == ["run-B"]


def test_copy_thread_copies_the_complete_chain(saver):
    builder = StateGraph(State)
    builder.add_node("bump", lambda s: {"total": 1})
    builder.add_edge(START, "bump")
    builder.add_edge("bump", END)
    graph = builder.compile(checkpointer=saver)
    graph.invoke({"total": 0}, _cfg("src"))

    saver.copy_thread("src", "dst")
    src_hist = list(saver.list(_cfg("src")))
    dst_hist = list(saver.list(_cfg("dst")))
    assert len(dst_hist) == len(src_hist)
    assert (
        dst_hist[0].checkpoint["channel_values"]
        == src_hist[0].checkpoint["channel_values"]
    )


def test_prune_keep_latest_preserves_the_ancestor_chain(saver):
    builder = StateGraph(State)
    builder.add_node("bump", lambda s: {"total": 1})
    builder.add_edge(START, "bump")
    builder.add_edge("bump", END)
    graph = builder.compile(checkpointer=saver)
    graph.invoke({"total": 0}, _cfg("t5"))
    graph.invoke({"total": 0}, _cfg("t5"))
    before = list(saver.list(_cfg("t5")))

    saver.prune(["t5"])
    after = list(saver.list(_cfg("t5")))
    # The latest checkpoint's whole parent chain survives (DeltaChannel
    # reconstruction depends on it); the count never grows.
    assert len(after) <= len(before)
    latest = after[0]
    chain_ids = {t.config["configurable"]["checkpoint_id"] for t in after}
    cur = latest
    while cur.parent_config is not None:
        parent_id = cur.parent_config["configurable"]["checkpoint_id"]
        assert parent_id in chain_ids, "prune severed the ancestor chain"
        cur = next(
            t
            for t in after
            if t.config["configurable"]["checkpoint_id"] == parent_id
        )
    # The pruned thread still loads and continues.
    out = graph.invoke({"total": 3}, _cfg("t5"))
    assert out["total"] >= 3
