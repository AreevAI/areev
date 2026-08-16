"""AreevAuditListener: schema-agnostic capture (unknown event shapes land
as generic observations), and the best-effort/guaranteed loss-honesty
matrix — the same contract as the LangGraph mirror."""

from __future__ import annotations

import json
import time

import areev
import pytest

from areev_crewai import AreevAuditListener


def _stored_events(path: str) -> list[dict]:
    db = areev.Areev(path, ns="crewai")
    rows = json.loads(db.recall("audit:events", k=100000, ns="crewai"))
    return [json.loads(r["fields"]["object"]) for r in rows]


def test_unknown_event_shapes_persist_generically(tmp_path):
    listener = AreevAuditListener(str(tmp_path / "a.db"))

    class TotallyNewEvent:  # a shape no release of this adapter knew about
        def __init__(self):
            self.crew = "alpha"
            self.payload = {"nested": [1, 2, {"deep": object()}]}

    ev = TotallyNewEvent()
    from areev_crewai.audit import _json_safe

    listener.record(type(ev).__name__, _json_safe(ev.__dict__))
    listener.record("AgentExecutionStartedEvent", {"agent": "researcher"})
    listener.flush()
    listener.close()

    events = _stored_events(str(tmp_path / "a.db"))
    kinds = {e["kind"] for e in events}
    assert kinds == {"TotallyNewEvent", "AgentExecutionStartedEvent"}
    novel = next(e for e in events if e["kind"] == "TotallyNewEvent")
    assert novel["payload"]["crew"] == "alpha", "unknown shapes still round-trip"


def test_real_bus_wiring_captures_emitted_events(tmp_path):
    """The bus dispatches by EXACT class, so wiring must register per
    concrete event class — a wildcard-on-BaseEvent registration captures
    nothing. Drive the REAL crewai bus end to end: wired > 0, and an
    emitted event lands in the file."""
    pytest.importorskip("crewai")
    from crewai.events.event_bus import crewai_event_bus
    from crewai.events.types.crew_events import CrewKickoffStartedEvent

    with crewai_event_bus.scoped_handlers():
        listener = AreevAuditListener(str(tmp_path / "bus.db"), mode="guaranteed")
        listener.setup_listeners(crewai_event_bus)
        assert listener.wired > 0, "zero registrations = silent no-op sidecar"

        crewai_event_bus.emit(object(), CrewKickoffStartedEvent(crew_name="demo", inputs=None))

        # Bus dispatch may be async: wait until the worker has picked the
        # event up (seq/dropped move), then flush the write. The worker owns
        # the file handle until close(), so reads only happen after.
        deadline = time.time() + 10.0
        while time.time() < deadline and listener._seq + listener.dropped == 0:
            time.sleep(0.05)
        assert listener.flush(timeout=10.0), "flush timed out with events pending"
        listener.close()

    events = _stored_events(str(tmp_path / "bus.db"))
    kick = [e for e in events if e["kind"] == "CrewKickoffStartedEvent"]
    assert kick, f"emitted event never captured (stored kinds: {[e['kind'] for e in events]})"
    assert kick[0]["payload"]["crew_name"] == "demo"


def test_best_effort_counts_loss_guaranteed_never_drops(tmp_path):
    be = AreevAuditListener(str(tmp_path / "be.db"), mode="best-effort", capacity=4)
    for i in range(200):
        be.record("burst", {"i": i})
    be.flush()
    be.close()
    stored = len(_stored_events(str(tmp_path / "be.db")))
    assert stored + be.dropped == 200, (stored, be.dropped)

    g = AreevAuditListener(str(tmp_path / "g.db"), mode="guaranteed", capacity=4)
    for i in range(60):
        g.record("audit", {"i": i})
    g.flush(timeout=30.0)
    g.close()
    assert g.dropped == 0
    assert len(_stored_events(str(tmp_path / "g.db"))) == 60
