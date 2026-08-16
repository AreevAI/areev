"""areev.helpers — the convenience layer over the core binding."""
import json
import os

import pytest

import areev
from areev.helpers import (
    audit, auto_model, bar, days_later, facts, fresh, outcomes, recs,
    show_facts, show_recs,
)


def test_fresh_clears_previous_file_and_sidecars(tmp_path):
    path = str(tmp_path / "h.db")
    db = fresh(path, ns="t")
    db.add_fact("a", "b", "c")
    del db
    # A sidecar the reopen will not recreate, so its absence is proof `fresh`
    # cleared it rather than the store rebuilding it.
    (tmp_path / "h.db.kdf").write_text("stale key material")
    db = fresh(path, ns="t")                      # second open: clean slate
    assert json.loads(db.recall("a")) == []
    assert not (tmp_path / "h.db.kdf").exists()


def test_fresh_only_removes_this_files_own_sidecars(tmp_path):
    """A prefix match would take neighbours that merely share the name.

    `fresh` deletes; anything it deletes beyond the memory it was handed is
    data loss, so it removes exactly `path` plus known Areev suffixes.
    """
    keep_file = tmp_path / "h.db.backup"
    keep_file.write_text("please do not delete me")
    keep_dir = tmp_path / "h.db_assets"
    keep_dir.mkdir()
    (keep_dir / "note.txt").write_text("nor me")

    fresh(str(tmp_path / "h.db"), ns="t")

    assert keep_file.exists(), "a neighbouring file must survive"
    assert (keep_dir / "note.txt").exists(), "a neighbouring directory must survive"


def test_days_later_restores_an_existing_pin(monkeypatch):
    monkeypatch.setenv("AREEV_LOOP_NOW_MS", "1700000000000")
    with days_later(1):
        assert os.environ["AREEV_LOOP_NOW_MS"] == str(1700000000000 + 86_400_000)
    assert os.environ["AREEV_LOOP_NOW_MS"] == "1700000000000"


def test_facts_and_recs_are_plain_dicts(tmp_path):
    db = fresh(str(tmp_path / "h.db"), ns="t")
    db.add_fact("acme", "plan", "gold", confidence=0.8)
    rows = facts(db, "acme")
    assert rows[0]["relation"] == "plan" and rows[0]["object"] == "gold"
    assert rows[0]["confidence"] == 0.8 and len(rows[0]["hash"]) == 64
    assert show_facts(db, "acme") == rows

    for i in range(4):
        db.record_tool_call("tool", f"boom {i}", is_error=True, thread="s")
    db.loop_run(full_sweep=True)
    pending = recs(db)
    assert pending and pending[0]["analyzer"].startswith("loop.tool_failure")
    assert show_recs(db) == pending


def test_days_later_sets_and_restores_clock_pin(tmp_path):
    assert "AREEV_LOOP_NOW_MS" not in os.environ
    with days_later(2):
        pinned = int(os.environ["AREEV_LOOP_NOW_MS"])
        assert pinned > 1e12
    assert "AREEV_LOOP_NOW_MS" not in os.environ


def test_verify_checkpoint_via_days_later(tmp_path, capsys):
    db = fresh(str(tmp_path / "h.db"), ns="t")
    for i in range(4):
        db.record_tool_call("tool", f"boom {i}", is_error=True, thread="s")
    db.loop_run(full_sweep=True)
    db.apply_recommendation(recs(db)[0]["hash"], because="test")
    with days_later(2):
        db.loop_run(full_sweep=True)
    rows = outcomes(db)
    assert rows and rows[0]["verdict"] == "held"
    assert "HELD" in capsys.readouterr().out
    audit(db)
    assert "applied" in capsys.readouterr().out


def test_auto_model_resolves_by_key_priority(monkeypatch):
    for k in ("ANTHROPIC_API_KEY", "OPENAI_API_KEY", "OPENROUTER_API_KEY"):
        monkeypatch.delenv(k, raising=False)
    assert auto_model() is None
    monkeypatch.setenv("OPENROUTER_API_KEY", "x")
    assert auto_model() == "openrouter:openai/gpt-4o-mini"
    monkeypatch.setenv("OPENAI_API_KEY", "x")
    assert auto_model() == "gpt-4o-mini"
    monkeypatch.setenv("ANTHROPIC_API_KEY", "x")
    assert auto_model() == "claude-opus-5"


def test_bar_renders_and_exports_png(tmp_path):
    plt = pytest.importorskip("matplotlib")
    import matplotlib
    matplotlib.use("Agg")
    out = tmp_path / "chart.png"
    bar(["a", "b"], [1, 2], title="t", save=str(out))
    assert out.stat().st_size > 0
