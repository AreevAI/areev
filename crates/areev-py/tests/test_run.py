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


def test_run_inspect(db):
    wf = _plan(db)
    session = json.loads(db.run_start(
        wf, "py-inspect", input_json='{"who": "world"}',
        tool_cmd='printf \'{"greeting": "hello"}\'',
    ))
    ask = session["parked"]["asks"][0]["tool_call_id"]
    db.run_respond("py-inspect", ask, '{"approved": true}', responder="user:officer")
    db.run_resume("py-inspect")

    report = json.loads(db.run_inspect("py-inspect"))
    assert report["run_id"] == "py-inspect"
    assert report["plan_hash"] == wf
    assert report["principal"] == "user:starter"
    assert {p["node"] for p in report["pinned"]} == {"greet", "approve"}
    assert report["checkpoints"] >= 1
    assert report["journal_entries"] >= 1
    assert report["fork_of"] is None


def test_run_oversight_report(db):
    wf = _plan(db)
    session = json.loads(db.run_start(
        wf, "py-oversight", input_json='{"who": "world"}',
        tool_cmd='printf \'{"greeting": "hello"}\'',
    ))
    ask = session["parked"]["asks"][0]["tool_call_id"]
    db.run_respond("py-oversight", ask, '{"approved": true}', responder="user:officer")
    db.run_resume("py-oversight")

    report = json.loads(db.run_oversight_report(run_id="py-oversight"))
    assert report["run_id"] == "py-oversight"
    assert report["plan_hash"] == wf
    gated = report["human_gates"]["client_gated_nodes"]
    assert any(n["node"] == "approve" for n in gated)
    assert report["human_gates"]["every_client_ask_is_an_approval"] is True
    assert report["authorized_responders"]["principals_granted_run_respond"] == []

    # Neither run_id nor plan given → the newest run overall.
    newest = json.loads(db.run_oversight_report())
    assert newest["run_id"] == "py-oversight"

    # plan resolves to that plan's newest run.
    by_plan = json.loads(db.run_oversight_report(plan=wf))
    assert by_plan["run_id"] == "py-oversight"


def test_run_input_queues_a_steering_message(db):
    wf = _plan(db)
    json.loads(db.run_start(wf, "py-in", tool_cmd='printf \'{"greeting": "hi"}\''))
    queued = json.loads(db.run_input("py-in", "use the express carrier"))
    assert queued["queued"] == "py-in"


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


def _ms(date):
    import datetime as dt
    d = dt.datetime.strptime(date, "%Y-%m-%d").replace(tzinfo=dt.timezone.utc)
    return int(d.timestamp() * 1000)


def test_a_plan_declared_read_puts_db_entity_at_into_run_state(tmp_path):
    """#255: a run reads its own memory with no tool holding a handle. The
    plan declares the read; the runtime answers it; the host tool downstream
    receives, in merged state, byte-for-byte what `db.entity_at` returns —
    including the honest `{"found": false}` of a backdated fact on the
    knowledge axis."""
    db = areev.Areev(str(tmp_path / "desk.db"), ns="org.uw", actor="user:desk")
    policies = "org.uw.policies"
    issued, eff, recv = _ms("2026-01-01"), _ms("2026-05-01"), _ms("2026-06-15")
    old = db.add("fact", json.dumps({
        "subject": "POL-4471", "relation": "mg:coverage_limit", "object": "500000",
        "valid_from": issued, "created_at": issued}), ns=policies)
    db.supersede(old, "fact", json.dumps({
        "subject": "POL-4471", "relation": "mg:coverage_limit", "object": "500000",
        "valid_from": issued, "valid_to": eff, "created_at": issued}), ns=policies)
    db.add("fact", json.dumps({
        "subject": "POL-4471", "relation": "mg:coverage_limit", "object": "750000",
        "valid_from": eff, "created_at": recv}), ns=policies)

    assess = db.add("tool", json.dumps({
        "tool_name": "assess", "kind": "definition",
        "tool_description": "reads the cover out of state", "created_at": 500}))

    def read(axis, at_from):
        return {"op": "entity_at", "ns": policies, "subject_from": "/policy_id",
                "relation": "mg:coverage_limit", "at_from": at_from, "axis": axis}

    wf = db.add("workflow", json.dumps({
        "nodes": ["world_at_loss", "known_on_may20", "assess"],
        "edges": [{"src": "world_at_loss", "dst": "known_on_may20"},
                  {"src": "known_on_may20", "dst": "assess"}],
        "bindings": {"assess": assess},
        "reads": {"world_at_loss": read("world", "/date_of_loss"),
                  "known_on_may20": read("knowledge", "/asked_on")},
        "created_at": 501,
    }))
    seen = tmp_path / "assess-stdin.json"
    session = json.loads(db.run_start(
        wf, "claim-8801",
        input_json=json.dumps({"policy_id": "POL-4471", "date_of_loss": "2026-03-18",
                               "asked_on": "2026-05-20"}),
        tool_cmd="cat > %s; printf '{}'" % seen,
    ))
    assert session.get("finished") == "Completed", session

    state = json.loads(seen.read_text())
    assert state["world_at_loss"] == json.loads(db.entity_at(
        "POL-4471", "mg:coverage_limit", _ms("2026-03-18"), axis="world", ns=policies))
    assert state["world_at_loss"]["grain"]["fields"]["object"] == "500000"
    assert state["known_on_may20"] == json.loads(db.entity_at(
        "POL-4471", "mg:coverage_limit", _ms("2026-05-20"), axis="knowledge", ns=policies))
    assert state["known_on_may20"] == {"found": False}

    pinned = {p["node"]: p for p in json.loads(db.run_inspect("claim-8801"))["pinned"]}
    assert pinned["world_at_loss"]["executor"] == "memory"
    assert pinned["world_at_loss"]["read"]["axis"] == "world"
    assert json.loads(db.run_verify("claim-8801"))["verified"] is True
