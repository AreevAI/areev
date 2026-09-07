"""`on_event` on the run surface (issue #182).

A host control the CLI already had (`--events`) and the bindings hardcoded
away: `observer: None` at both `runner_pinned` call sites. The payload is the
exact line `--events` prints, so there is no per-language event class to keep
in sync and no `Deserialize` on `RunEvent`.
"""

import json

import areev
import pytest


def _plan(db):
    """The same two-node plan test_run.py uses: host `greet` → Client `approve`."""
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


GREET = 'printf \'{"greeting": "hello"}\''


def _drive(db, wf, run_id, **kwargs):
    """start → respond as a second principal → resume, to a Completed run."""
    session = json.loads(db.run_start(
        wf, run_id, input_json='{"who": "world"}', tool_cmd=GREET, **kwargs))
    ask = session["parked"]["asks"][0]["tool_call_id"]
    db.run_respond(run_id, ask, '{"approved": true}', responder="user:officer")
    return session


# --------------------------------------------------------------------------
# #182 — the event callback
# --------------------------------------------------------------------------

def test_on_event_streams_the_same_json_lines_as_the_cli(db):
    wf = _plan(db)
    started = []
    _drive(db, wf, "ev-1", on_event=started.append)

    events = [json.loads(line) for line in started]
    assert events, "the callback must have been called"
    assert events[0] == {"event": "RunStarted", "run_id": "ev-1"}
    # `RunFinished` is emitted at a TERMINAL outcome, and this leg parks on the
    # human gate — so the start leg ends at the ask, and the resume leg below
    # is where the last event lives.
    assert events[-1]["event"] == "AskRaised", events[-1]

    # Every payload is the tagged §6.10 vocabulary, not a per-language struct.
    assert {"NodeDispatched", "EffectSettled", "AskRaised"} <= {e["event"] for e in events}

    resumed = []
    done = json.loads(db.run_resume("ev-1", on_event=resumed.append))
    assert done.get("finished") == "Completed", done

    events = [json.loads(line) for line in resumed]
    assert events[0] == {"event": "RunResumed", "run_id": "ev-1"}
    assert events[-1]["event"] == "RunFinished"
    assert events[-1]["outcome"] == "Completed"
    assert events[-1]["dropped_events"] == 0, \
        "a handful of events must not overflow a 1024-slot buffer"


@pytest.mark.filterwarnings("ignore::pytest.PytestUnraisableExceptionWarning")
def test_a_raising_callback_never_fails_the_run(db):
    """Events are observational; an exception is unraisable, not a failure.

    The unraisable report is the POINT — `write_unraisable` is how CPython
    surfaces an exception with nowhere to propagate (the same treatment
    `__del__` gets), and pytest turns it into a warning. Filtered here rather
    than suppressed globally, so an unraisable anywhere else still shows up.
    """
    wf = _plan(db)

    def boom(_line):
        raise RuntimeError("subscriber is broken")

    _drive(db, wf, "ev-3", on_event=boom)
    done = json.loads(db.run_resume("ev-3"))
    assert done.get("finished") == "Completed", done
    assert json.loads(db.run_verify("ev-3"))["verified"] is True


def test_a_subscriber_does_not_change_the_journal(db):
    """#182's acceptance criterion, at the level a binding can assert it.

    The Rust twin (`streaming_observers_never_change_the_journal`) compares
    journals byte for byte, and cannot see a binding callback at all. The
    binding-level equivalent: run the SAME plan twice, once observed and once
    not, and assert both verify and inspect identically apart from the run id.
    """
    wf = _plan(db)
    seen = []
    _drive(db, wf, "obs-on", on_event=seen.append)
    db.run_resume("obs-on")
    _drive(db, wf, "obs-off")
    db.run_resume("obs-off")

    assert seen, "the observed leg really was observed"
    assert json.loads(db.run_verify("obs-on"))["verified"] is True
    assert json.loads(db.run_verify("obs-off"))["verified"] is True

    def shape(run_id):
        r = json.loads(db.run_inspect(run_id))
        # Everything but the run id and `spent`, which carries wall time.
        return {k: r[k] for k in ("plan_hash", "principal", "pinned", "budgets",
                                  "fork_of", "checkpoints", "journal_entries",
                                  "phase", "pending_asks")}

    on, off = shape("obs-on"), shape("obs-off")
    assert on == off, f"a subscriber must not change what the run recorded\n{on}\n{off}"
