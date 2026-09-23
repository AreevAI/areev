"""`areev.pack_validate` / `Areev.pack_install` (#341).

The binding marshals; the install itself is `areev::pack::install_pack`, the
function `areev pack install` prints. So the properties worth pinning here are
the ones a host depends on through THIS surface: a typed `.code` on every
refusal, all-or-nothing under the handle's bound principal, executor pins that
are checked and never written, and — for every shipped example pack — the same
plan hash the CLI prints for the same directory.
"""

import glob
import json
import os
import shutil
import subprocess

import pytest

import areev

REPO = os.path.abspath(os.path.join(os.path.dirname(__file__), "..", "..", ".."))
EXAMPLE_PACKS = sorted(
    os.path.dirname(p)
    for p in glob.glob(os.path.join(REPO, "examples", "**", "pack", "pack.json"), recursive=True)
)


def areev_bin():
    return os.environ.get("AREEV_BIN") or next(
        (p for p in (os.path.join(REPO, "target", "debug", "areev"),
                     os.path.join(REPO, "target", "release", "areev")) if os.path.exists(p)),
        None) or shutil.which("areev")


def write_pack(root, *, blob=True, expected=None):
    """A code blob, a Definition naming it, a plan binding it, a saved query."""
    (root / "grains").mkdir(parents=True, exist_ok=True)
    (root / "blobs").mkdir(exist_ok=True)
    (root / "blobs" / "poll.wasm").write_bytes(b"\0asm\x01\0\0\0not-a-real-module")
    (root / "grains" / "010-tool.json").write_text(json.dumps({
        "type": "tool", "id": "poll", "kind": "definition", "tool_name": "poll",
        "tool_description": "read the queue", "created_at": 500,
        "executor_uri": "blob:poll", "runtime": "wasm32-areev-io",
        "capabilities": [{"blob": {"read": True}}]}))
    (root / "grains" / "020-workflow.json").write_text(json.dumps({
        "type": "workflow", "id": "plan", "name": "queue", "nodes": ["poll"],
        "bindings": {"poll": "grain:poll"}, "created_at": 600}))
    wf = "grains/020-workflow.json"
    (root / "pack.json").write_text(json.dumps({
        "pack": "queue", "version": "1.0.0", "namespace": "ap",
        "blobs": {"poll": "blobs/poll.wasm"},
        "queries": {"pulse": {"body": "RECALL facts LIMIT 5"}},
        "grains": ["grains/010-tool.json",
                   {"file": wf, "expected_hash": expected} if expected else wf]}))
    return str(root)


def plan(report):
    return sorted(g["hash"] for g in report["grains"] if g["grain_type"] == "workflow")


def tool_count(db):
    return json.loads(db.cal('RECALL tools WHERE namespace = "ap" RECENT 10'))["total_available"]


def test_validate_opens_no_memory_and_reports_the_executors(tmp_path):
    pack = write_pack(tmp_path / "pack")
    r = json.loads(areev.pack_validate(pack))
    assert r["pack"] == "queue"
    assert [g["grain_type"] for g in r["grains"]] == ["tool", "workflow"]
    assert all(len(g["hash"]) == 64 for g in r["grains"])
    [x] = r["executors"]
    assert x["tool"] == "poll" and x["pinned"] is False
    assert x["executor_uri"] == r["blobs"][0]["address"]
    assert r["registry"] == ["qry:pulse"]
    assert sorted(os.listdir(tmp_path)) == ["pack"], "validate created a file"


def test_validate_refusal_is_typed(tmp_path):
    pack = write_pack(tmp_path / "pack", expected="0" * 64)
    with pytest.raises(areev.PackError) as e:
        areev.pack_validate(pack)
    assert e.value.code == "PCK-E002"
    assert str(e.value).startswith("PCK-E002")
    # A ValueError still, so existing handlers keep catching it.
    assert isinstance(e.value, ValueError)


def test_install_matches_validate_and_pins_are_checked_never_written(tmp_path):
    pack = write_pack(tmp_path / "pack")
    validated = json.loads(areev.pack_validate(pack))
    addr = validated["executors"][0]["executor_uri"]

    base = areev.Areev(str(tmp_path / "base.db"), ns="ap")
    base.pack_install(pack)
    unpinned = json.loads(base.stats())["ops"]
    assert unpinned > 0, "positive control: an install moves the op-log"
    del base

    db = areev.Areev(str(tmp_path / "m.db"), ns="ap")
    r = json.loads(db.pack_install(pack, executor_pins={"poll": addr},
                                   expected_hash=plan(validated)[0]))
    assert plan(r) == plan(validated), "a pin or expectation changed the plan hash"
    assert r["executors"][0]["pinned"] is True
    assert json.loads(db.stats())["ops"] == unpinned, "a pin added a write"


@pytest.mark.parametrize("pins,expected,code", [
    ({"poll": "ab" * 32}, None, "PCK-E005"),          # the code is not the pinned code
    ({"pol": "ab" * 32}, None, "PCK-E005"),           # a pin naming no code-carrying tool
    (None, "0" * 64, "PCK-E002"),                     # not the plan the deployment expects
])
def test_a_refused_install_is_typed_and_writes_nothing(tmp_path, pins, expected, code):
    pack = write_pack(tmp_path / "pack")
    addr = json.loads(areev.pack_validate(pack))["blobs"][0]["address"]
    db = areev.Areev(str(tmp_path / "m.db"), ns="ap")
    before = db.stats()
    with pytest.raises(areev.PackError) as e:
        db.pack_install(pack, executor_pins=pins, expected_hash=expected)
    assert e.value.code == code, str(e.value)
    assert db.stats() == before
    assert tool_count(db) == 0
    with pytest.raises(ValueError):
        db.get_blob(addr)


def test_install_runs_under_the_bound_principal_all_or_nothing(tmp_path):
    pack = write_pack(tmp_path / "pack")
    addr = json.loads(areev.pack_validate(pack))["blobs"][0]["address"]
    path = str(tmp_path / "m.db")
    owner = areev.Areev(path, ns="ap")
    owner.cal('GRANT read ON "ap" TO "user:reader"')
    owner.cal('GRANT write ON "ap" TO "user:installer"')
    before = owner.stats()  # memory-wide: the reader may not read it
    del owner

    reader = areev.Areev(path, ns="ap", principal="user:reader")
    with pytest.raises(areev.PackError) as e:
        reader.pack_install(pack)
    assert e.value.code == "AUT-E001", str(e.value)
    del reader

    owner = areev.Areev(path, ns="ap")
    assert owner.stats() == before
    assert tool_count(owner) == 0
    with pytest.raises(ValueError):
        owner.get_blob(addr)  # the blob did not land ahead of the refusal
    del owner

    installer = areev.Areev(path, ns="ap", principal="user:installer")
    r = json.loads(installer.pack_install(pack))
    assert plan(r) == plan(json.loads(areev.pack_validate(pack)))


def test_dry_run_checks_everything_and_writes_nothing(tmp_path):
    pack = write_pack(tmp_path / "pack")
    db = areev.Areev(str(tmp_path / "m.db"), ns="ap")
    before = db.stats()
    r = json.loads(db.pack_install(pack, dry_run=True))
    assert len(r["grains"]) == 2
    assert db.stats() == before


@pytest.mark.parametrize("pack", EXAMPLE_PACKS, ids=lambda p: os.path.relpath(p, REPO))
def test_every_example_pack_installs_at_the_cli_plan_hash(tmp_path, pack):
    bin_ = areev_bin()
    if not bin_:
        pytest.skip("CLI leg skipped: no areev binary found (set AREEV_BIN)")
    cli = subprocess.run([bin_, "pack", "install", pack, "--db", str(tmp_path / "cli.db"),
                          "--format", "json"], capture_output=True, text=True)
    assert cli.returncode == 0, cli.stderr
    cli_plans = sorted(g["hash"] for g in json.loads(cli.stdout)["grains"]
                       if g["type"] == "workflow")

    validated = json.loads(areev.pack_validate(pack))
    db = areev.Areev(str(tmp_path / "py.db"), ns="shared")
    # Pinned to its own code: pins are checked, never written, so they must
    # not move the hash either.
    pins = {x["tool"]: x["executor_uri"] for x in validated["executors"]}
    installed = json.loads(db.pack_install(pack, executor_pins=pins or None))
    assert plan(installed) == cli_plans == plan(validated), pack
    # Every grain, not only the plan.
    cli_all = sorted(g["hash"] for g in json.loads(cli.stdout)["grains"])
    assert sorted(g["hash"] for g in installed["grains"]) == cli_all
