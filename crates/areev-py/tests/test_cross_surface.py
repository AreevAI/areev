"""The Wave-5 parity gate: ONE workflow, authored once, driven across every
surface in sequence — started from the CLI, approved and resumed from the
Python binding (second principal), traced and verified over MCP, verified
again from the CLI. (The console approval leg has its own server unit test:
`run_api_tests::shared_token_approvals_are_refused_and_principals_approve`.)

Single-writer discipline: each surface opens the file, works, and closes
before the next opens — which is also how these surfaces meet in real
deployments."""

import json
import os
import subprocess

import areev
import pytest

_BIN = os.environ.get(
    "AREEV_BIN",
    os.path.join(os.path.dirname(__file__), "../../..", "target", "debug", "areev"),
)


pytestmark = pytest.mark.skipif(
    not os.path.exists(_BIN), reason="areev binary not built (cargo build -p areev)"
)


def _cli(*args):
    proc = subprocess.run(
        [_BIN, *args], capture_output=True, text=True, timeout=120
    )
    return proc


def test_one_workflow_across_cli_python_mcp(tmp_path):
    db_path = str(tmp_path / "parity.db")

    # 1. CLI: seed the demo plan (host greet -> Client approve) and START.
    seeded = _cli("run", "demo", "--db", db_path)
    assert seeded.returncode == 0, seeded.stderr
    wf = next(
        line.split("workflow ")[1].rstrip(")")
        for line in seeded.stdout.splitlines()
        if "workflow " in line
    )
    started = _cli(
        "run", "start", "--db", db_path, "--workflow", wf, "--run-id", "parity-1",
        "--input", '{"who":"world"}',
        "--tool-cmd", 'printf \'{"greeting":"hello"}\'',
    )
    assert started.returncode == 0, started.stderr
    envelope = json.loads(started.stdout)
    ask = envelope["asks"][0]["tool_call_id"]

    # 2. Python: a SECOND principal approves; python resumes to completion.
    db = areev.Areev(db_path, ns="shared", actor="user:starter")
    db.run_respond("parity-1", ask, '{"approved": true}', responder="user:officer")
    done = json.loads(db.run_resume("parity-1"))
    assert done.get("finished") == "Completed", done
    report = json.loads(db.run_verify("parity-1"))
    assert report["verified"] is True
    del db  # release the single-writer handle

    # 3. MCP: trace + verify the same run over stdio JSON-RPC.
    mcp = subprocess.Popen(
        [_BIN, "serve", "--mcp", "--db", db_path],
        stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True,
    )
    msgs = [
        {"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {}},
        {"jsonrpc": "2.0", "id": 2, "method": "tools/call",
         "params": {"name": "areev_run_list", "arguments": {}}},
        {"jsonrpc": "2.0", "id": 3, "method": "tools/call",
         "params": {"name": "areev_run_trace", "arguments": {"run_id": "parity-1"}}},
        {"jsonrpc": "2.0", "id": 4, "method": "tools/call",
         "params": {"name": "areev_run_verify", "arguments": {"run_id": "parity-1"}}},
    ]
    out, _ = mcp.communicate("\n".join(json.dumps(m) for m in msgs) + "\n", timeout=120)
    by_id = {}
    for line in out.splitlines():
        if line.strip():
            resp = json.loads(line)
            by_id[resp.get("id")] = resp

    def tool_payload(rid):
        resp = by_id[rid]["result"]
        assert not resp.get("isError"), resp
        return json.loads(resp["content"][0]["text"])

    assert "parity-1" in tool_payload(2)["runs"]
    trace = tool_payload(3)
    assert trace["run_id"] == "parity-1"
    verify = tool_payload(4)
    assert verify["verified"] is True, verify

    # 4. And the CLI verifies the same journal one more time.
    verified = _cli("run", "verify", "--db", db_path, "--run-id", "parity-1")
    assert verified.returncode == 0, verified.stdout + verified.stderr
    assert '"verified": true' in verified.stdout
