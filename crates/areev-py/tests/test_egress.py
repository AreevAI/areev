"""#201: the credential broker reaches a binding-driven run.

A `wasm32-areev-io` tool is driven from Python with the CLI's spec strings
verbatim, and — when an `areev` binary is at hand — from the CLI with the
same declaration and the same grants, asserting the same admitted call and
the same four refusals, each journaled in `agent:harness`.
"""
import json
import os
import shutil
import subprocess
import sys
import threading
from http.server import BaseHTTPRequestHandler, HTTPServer

import pytest

import areev

HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.abspath(os.path.join(HERE, "..", "..", ".."))
FETCHER = os.path.join(HERE, "egress_fetcher.py")
SECRETS = {"AREEV_TEST_GMAIL_201": "s3cret-gmail", "AREEV_TEST_SHEETS_201": "s3cret-sheets"}


class _Upstream(BaseHTTPRequestHandler):
    """Says whether the broker attached a credential."""

    def _reply(self):
        body = json.dumps({"ok": True, "auth": bool(self.headers.get("Authorization")),
                           "method": self.command}).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    do_GET = do_POST = _reply

    def log_message(self, *_):
        pass


@pytest.fixture
def upstream():
    srv = HTTPServer(("127.0.0.1", 0), _Upstream)
    t = threading.Thread(target=srv.serve_forever, daemon=True)
    t.start()
    yield "http://127.0.0.1:%d" % srv.server_address[1]
    srv.shutdown()


def declare_fetcher(m, up):
    """ONE declaration: gmail for the upstream, sheets for another service, so
    an "unpaired" call can name a credential the tool holds for the wrong host."""
    uri = m.put_blob(json.dumps({"upstream": up}).encode())
    tool = m.add("tool", json.dumps({
        "tool_name": "fetcher", "kind": "definition", "executor_uri": uri,
        "runtime": "wasm32-areev-io",
        "capabilities": [
            {"http": {"hosts": [up], "methods": ["POST"], "credentials": ["gmail"]}},
            {"http": {"hosts": ["https://sheets.example.com"], "methods": ["POST"],
                      "credentials": ["sheets"]}},
        ]}))
    wf = m.add("workflow", json.dumps({
        "name": "fetch", "nodes": ["fetcher"], "edges": [], "bindings": {"fetcher": tool}}))
    return wf, uri.removeprefix("cas://sha256:")


def grants_for(up):
    return dict(credentials="gmail=AREEV_TEST_GMAIL_201,sheets=AREEV_TEST_SHEETS_201",
                allow_hosts="%s,https://sheets.example.com" % up,
                tool_egress="fetcher:gmail+sheets:POST")


def outcome(m, run_id):
    trace = json.loads(m.run_trace(run_id, 100, False, "ops"))["trace"]
    # The execution record's `content` (deserialized as `tool_content`) is the JSON result.
    execs = [g for g in trace if g["fields"].get("tool_name") == "fetcher"
             and (g["fields"].get("tool_content") or g["fields"].get("content"))]
    assert execs, trace
    result = execs[0]["fields"].get("tool_content") or execs[0]["fields"].get("content")
    if isinstance(result, str):
        result = json.loads(result)
    harness = json.loads(m.run_trace(run_id, 100, False, "agent:harness"))["trace"]
    refusals = [g for g in harness if g["fields"].get("observation_kind") == "egress_refusal"]
    return result, refusals


def assert_outcome(result, refusals, label):
    assert result["leak"] == "", "%s: the secret never reaches the module" % label
    assert result["allow_fetch"] is True, label
    # An admitted call answers 200 with {status, body}: the upstream's own reply, as a string.
    assert result["admitted"]["status"] == 200, (label, result["admitted"])
    assert result["admitted"]["body"]["status"] == 200, (label, result["admitted"])
    upstream_saw = json.loads(result["admitted"]["body"]["body"])
    assert upstream_saw["auth"] is True, "%s: the broker attached the credential" % label
    assert upstream_saw["method"] == "POST"
    for k in ("wrong_host", "wrong_method", "undeclared_credential", "unpaired"):
        assert result[k]["status"] == 403, (label, k, result[k])
        assert result[k]["body"]["code"] == "RUN-E022", (label, k, result[k])
    assert "no single capability pairs" in result["unpaired"]["body"]["error"]
    assert len(refusals) == 4, (label, refusals)
    assert "https://evil.example.net/steal" in [r["fields"]["destination"] for r in refusals]
    assert all(r["fields"].get("run_id") for r in refusals), label


def test_a_capability_tool_reaches_the_broker_from_python_as_from_the_cli(tmp_path, upstream, monkeypatch):
    for k, v in SECRETS.items():
        monkeypatch.setenv(k, v)
    db = str(tmp_path / "e.db")
    cache = str(tmp_path / "execache")
    sandbox = "%s %s" % (sys.executable, FETCHER)
    g = grants_for(upstream)

    m = areev.Areev(db, ns="ops")
    wf, addr = declare_fetcher(m, upstream)

    # Without the grants nothing answers `areev::fetch` — the #201 state.
    bare = json.loads(m.run_start(wf, "py-bare", allow_executor=addr, executor_cache=cache,
                                  sandbox_cmd=sandbox))
    assert "Failed" in bare["finished"], bare

    # With them: the same admitted call and the same four refusals.
    ran = json.loads(m.run_start(wf, "py-1", allow_executor=addr, executor_cache=cache,
                                 sandbox_cmd=sandbox, **g))
    assert "Completed" in ran["finished"], ran
    via_binding = outcome(m, "py-1")
    assert_outcome(*via_binding, "binding")

    # A bad spec is refused before anything is journaled, with the CLI's words.
    with pytest.raises(ValueError, match="--credential: expected name=ENV_VAR"):
        m.run_start(wf, "py-bad", allow_executor=addr, executor_cache=cache, sandbox_cmd=sandbox,
                    credentials="gmail", allow_hosts=g["allow_hosts"], tool_egress=g["tool_egress"])
    del m

    # The CLI leg: the SAME declaration and the SAME grants, spelled as flags.
    bin_ = os.environ.get("AREEV_BIN") or next(
        (p for p in (os.path.join(REPO, "target", "debug", "areev"),
                     os.path.join(REPO, "target", "release", "areev")) if os.path.exists(p)),
        None) or shutil.which("areev")
    if not bin_:
        pytest.skip("CLI leg skipped: no areev binary found (set AREEV_BIN)")
    cli = subprocess.run([
        bin_, "run", "--db", db, "--ns", "ops", "start", "--workflow", wf, "--run-id", "cli-1",
        "--allow-executor", addr, "--executor-cache", cache, "--sandbox-cmd", sandbox,
        "--credential", g["credentials"], "--allow-host", g["allow_hosts"],
        "--tool-egress", g["tool_egress"]], capture_output=True, text=True)
    assert cli.returncode == 0, cli.stdout + cli.stderr
    m2 = areev.Areev(db, ns="ops")
    via_cli = outcome(m2, "cli-1")
    assert_outcome(*via_cli, "cli")
    for k in ("admitted", "wrong_host", "wrong_method", "undeclared_credential", "unpaired"):
        assert via_binding[0][k] == via_cli[0][k], k


def test_the_trigger_surface_takes_the_same_grants(tmp_path, upstream, monkeypatch):
    """A firing starts a real run, so its broker is the one `run_start` builds."""
    for k, v in SECRETS.items():
        monkeypatch.setenv(k, v)
    m = areev.Areev(str(tmp_path / "t.db"), ns="ops")
    wf, addr = declare_fetcher(m, upstream)
    trig = m.trigger_add(json.dumps({
        "kind": "webhook", "workflow": wf, "connector": "c", "dedup_key": ["/id"]}),
        "a capability tool must run from a trigger")
    report = json.loads(m.trigger_deliver(
        trig, json.dumps({"id": "a"}), allow_executor=addr,
        executor_cache=str(tmp_path / "execache"),
        sandbox_cmd="%s %s" % (sys.executable, FETCHER), **grants_for(upstream)))
    assert report["runs_started"] == 1, report
    run_id = json.loads(m.run_list(10))[0]
    assert_outcome(*outcome(m, run_id), "trigger")
