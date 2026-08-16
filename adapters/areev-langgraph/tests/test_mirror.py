"""AreevTraceMirror: events land as queryable facts; best-effort loses
under load and SAYS SO (bounded loss, accurate counter); guaranteed never
drops — the §8 mode matrix, exercised."""

from __future__ import annotations

import json
import operator
from typing import Annotated, TypedDict

import areev
from langgraph.graph import END, START, StateGraph

from areev_langgraph import AreevTraceMirror


class State(TypedDict):
    total: Annotated[int, operator.add]


def test_real_run_mirrors_events(tmp_path):
    mirror = AreevTraceMirror(str(tmp_path / "trace.db"))
    builder = StateGraph(State)
    builder.add_node("bump", lambda s: {"total": 1})
    builder.add_edge(START, "bump")
    builder.add_edge("bump", END)
    graph = builder.compile()
    graph.invoke({"total": 0}, config={"callbacks": [mirror]})
    mirror.flush()
    mirror.close()

    db = areev.Areev(str(tmp_path / "trace.db"), ns="langgraph")
    stats = json.loads(db.stats())
    assert stats.get("grains", stats.get("total_grains", 0)) > 0, f"no events landed: {stats}"
    assert mirror.dropped == 0


def test_best_effort_sheds_with_an_accurate_counter(tmp_path):
    mirror = AreevTraceMirror(str(tmp_path / "t.db"), mode="best-effort", capacity=4)
    total = 300
    for i in range(total):
        mirror._emit("burst", f"run-{i}", {})
    mirror.flush()
    mirror.close()
    # Bounded loss, honestly counted: everything is either stored or in
    # the drop counter — nothing silently vanishes.
    db = areev.Areev(str(tmp_path / "t.db"), ns="langgraph")
    stored = 0
    for i in range(total):
        rows = json.loads(db.recall(f"trace:run-{i}", k=10, ns="langgraph"))
        stored += len(rows)
    assert stored + mirror.dropped == total, (stored, mirror.dropped)
    assert stored >= 1


def test_guaranteed_mode_never_drops(tmp_path):
    mirror = AreevTraceMirror(str(tmp_path / "g.db"), mode="guaranteed", capacity=4)
    total = 60
    for i in range(total):
        mirror._emit("audit", f"run-{i}", {"i": i})
    mirror.flush(timeout=30.0)
    mirror.close()
    assert mirror.dropped == 0
    db = areev.Areev(str(tmp_path / "g.db"), ns="langgraph")
    stored = sum(
        len(json.loads(db.recall(f"trace:run-{i}", k=10, ns="langgraph")))
        for i in range(total)
    )
    assert stored == total
