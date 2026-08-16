"""The `areev run` runtime through the Python binding (Wave 5 parity):
start → park on a Client ask → respond as a SECOND principal → resume →
verify → shadow — the cross-surface gate's Python leg."""

import json

import areev
import pytest


def _plan(db):
    """A two-node plan: host `greet` → Client-gated `approve`."""
    greet = db.add("tool", json.dumps({
        "tool_name": "greet", "kind": "definition",
        "tool_description": "greets", "created_at": 500,
    }), ns="ops")
    approve = db.add("tool", json.dumps({
        "tool_name": "approve", "kind": "definition",
        "tool_description": "human approves", "executor_kind": "client",
        "created_at": 501,
    }), ns="ops")
    return db.add("workflow", json.dumps({
        "nodes": ["greet", "approve"],
        "edges": [{"src": "greet", "dst": "approve"}],
        "bindings": {"greet": greet, "approve": approve},
        "created_at": 502,
    }), ns="ops")


@pytest.fixture()
def db(tmp_path):
    return areev.Areev(str(tmp_path / "run.db"), ns="ops", actor="user:starter")


def test_run_start_respond_resume_verify(db):
    wf = _plan(db)
    session = json.loads(db.run_start(
        wf, "py-1", input_json='{"who": "world"}',
        tool_cmd='printf \'{"greeting": "hello"}\'',
    ))
    assert "parked" in session, session
    ask = session["parked"]["asks"][0]["tool_call_id"]

    # Separation of duties: the starter cannot approve their own run.
    with pytest.raises(ValueError, match="responder"):
        db.run_respond("py-1", ask, '{"approved": true}', responder="user:starter")

    db.run_respond("py-1", ask, '{"approved": true}', responder="user:officer")
    done = json.loads(db.run_resume("py-1"))
    assert done.get("finished") == "Completed", done

    report = json.loads(db.run_verify("py-1"))
    assert report["verified"] is True, report

    # Listed, inspectable, shadow-replayable with zero dispatches.
    assert "py-1" in json.loads(db.run_list())
    shadow = json.loads(db.run_shadow(["py-1"]))
    assert shadow["all_consistent"] is True
    assert shadow["effect_dispatches"] == 0


def test_run_cancel_and_fork(db):
    wf = _plan(db)
    session = json.loads(db.run_start(
        wf, "py-2", tool_cmd='printf \'{"greeting": "hi"}\''))
    assert "parked" in session

    # Fork from the closed first superstep; the fork resumes independently.
    seed = db.run_fork("py-2", "py-2-fork", at_superstep=1)
    assert len(seed) == 64

    db.run_cancel("py-2", because="operator abort")
    finished = json.loads(db.run_resume("py-2"))
    assert "Canceled" in finished.get("finished", ""), finished
