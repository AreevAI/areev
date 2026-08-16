"""Conveniences for scripts and notebooks on top of the core `Areev` class.

The core binding is deliberately minimal — scalars in, JSON strings out. This
module is the ergonomic layer over it: plain-dict views, a fresh-open that
clears stale files, a review-trail printer, a clock-pin context manager for
rehearsing Areev Loop's time-based checkpoints, LLM auto-detection for
`loop_run(model=...)`, and a labeled bar chart with PNG export (matplotlib
is imported lazily and only needed for `bar`).

Import it explicitly — nothing here loads unless you ask for it:

    from areev.helpers import *

    db = fresh("support.db", ns="support")
    db.loop_run(full_sweep=True, model=auto_model())
    for rec in show_recs(db):
        db.apply_recommendation(rec["hash"], because="...")
    with days_later(2):
        db.loop_run(full_sweep=True)
    outcomes(db)
"""
import json
import os
import pathlib
import shutil
import time
from contextlib import contextmanager

from . import Areev

__all__ = [
    "BLUE", "GREEN", "ORANGE",
    "fresh", "facts", "show_facts", "recs", "show_recs", "audit",
    "outcomes", "days_later", "auto_model", "bar",
]

#: Chart colors used by :func:`bar` (a colorblind-safe trio).
BLUE, GREEN, ORANGE = "#2a78d6", "#1baf7a", "#eb6834"


#: Suffixes Areev appends to a memory file. ``fresh`` removes exactly these
#: and nothing else — a prefix match would also take a neighbouring
#: ``support.db.backup`` or a ``support_assets/`` directory, which is not what
#: "clear the sidecars" means to anyone reading it.
_SIDECARS = ("", "-wal", "-shm", "-journal", ".blobs", ".kdf", ".telemetry.db",
             ".telemetry.db-wal", ".telemetry.db-shm")


def fresh(path, **kw):
    """Delete any previous copy of this memory file and open a fresh one.

    Removes the file plus its own sidecars (``-wal``, the ``.blobs`` directory,
    ``.telemetry.db`` …) so demos and tests are re-runnable, then returns
    ``Areev(path, **kw)``. Only paths that are exactly ``path`` plus a known
    Areev suffix are touched. Never use on a store you mean to keep.
    """
    p = pathlib.Path(path)
    for suffix in _SIDECARS:
        f = p.with_name(p.name + suffix)
        if f.is_dir():
            shutil.rmtree(f)
        elif f.exists():
            f.unlink()
    return Areev(path, **kw)


def facts(db, subject, relation=None, ns=None, k=32):
    """Live facts about a subject, as plain dicts.

    A thin view over ``db.recall(...)`` returning
    ``{"relation", "object", "confidence", "hash"}`` per grain.
    """
    rows = []
    for g in json.loads(db.recall(subject, relation, k, ns)):
        f = g["fields"]
        rows.append({"relation": f.get("relation"), "object": f.get("object"),
                     "confidence": f.get("confidence"), "hash": g["hash"]})
    return rows


def show_facts(db, subject, **kw):
    """Print what the store currently believes about a subject; returns the rows."""
    rows = facts(db, subject, **kw)
    for f in rows:
        print(f"  {subject} --{f['relation']}--> {f['object']}")
    return rows


def recs(db, status="pending"):
    """Recommendations in one lifecycle state, as plain dicts.

    ``status`` is one of ``pending``, ``approved``, ``applied``, ``rejected``,
    ``rolled_back``, ``expired``.
    """
    return json.loads(db.recommendations(json.dumps({"status": status})))


def show_recs(db, status="pending"):
    """Print the review queue for one lifecycle state; returns the rows."""
    rows = recs(db, status)
    if not rows:
        print(f"  (no {status} recommendations)")
    for r in rows:
        tag = "DESTRUCTIVE" if r["destructive"] else "reversible"
        print(f"  [{r['severity']:6}] [{tag:11}] {r['summary']}")
    return rows


def audit(db):
    """Print the whole review trail, grouped by lifecycle state."""
    for status in ("pending", "approved", "applied", "rejected", "rolled_back"):
        for r in recs(db, status):
            print(f"  [{status:11}] {r['summary'][:66]}")


def outcomes(db):
    """Print measured verdicts of applied recommendations; returns the rows.

    This is the Verify gate's record — ``held`` means the metric stayed at or
    under its baseline after the change, ``regressed`` means it got worse.
    """
    rows = json.loads(db.loop_outcomes())
    for o in rows:
        print(f"  {o['metric']}: baseline={o['baseline']:.0f} → "
              f"current={o['current']:.0f}  — {o['verdict'].upper()}")
    return rows


@contextmanager
def days_later(n):
    """Run a block as if ``n`` days had passed.

    Sets ``AREEV_LOOP_NOW_MS`` — Areev's documented clock-pin seam, honored by
    the engine, CLI, MCP, and console — for the duration of the block, so
    Areev Loop's 1d/7d/30d verification checkpoints can be rehearsed without
    waiting. Restores whatever was there before, so nesting these (or using
    one inside an already-pinned process) leaves the outer pin intact.
    """
    previous = os.environ.get("AREEV_LOOP_NOW_MS")
    base = int(previous) if previous else int(time.time() * 1000)
    os.environ["AREEV_LOOP_NOW_MS"] = str(base + n * 24 * 3600 * 1000)
    try:
        yield
    finally:
        if previous is None:
            os.environ.pop("AREEV_LOOP_NOW_MS", None)
        else:
            os.environ["AREEV_LOOP_NOW_MS"] = previous


def auto_model():
    """Pick an LLM spec for ``loop_run(model=...)`` from the environment.

    Checks Google Colab's Secrets panel (when running in Colab) and the
    process environment for ``ANTHROPIC_API_KEY``, ``OPENAI_API_KEY``, then
    ``OPENROUTER_API_KEY``, and returns a matching model spec. Returns
    ``None`` when no key is configured — ``loop_run(model=None)`` runs the
    deterministic analyzers only, which is always a safe, keyless floor.
    """
    try:  # Colab: surface keys from the Secrets panel into the environment
        from google.colab import userdata  # type: ignore
        for k in ("ANTHROPIC_API_KEY", "OPENAI_API_KEY", "OPENROUTER_API_KEY"):
            try:
                v = userdata.get(k)
                if v:
                    os.environ.setdefault(k, v)
            except Exception:
                pass
    except ImportError:
        pass
    if os.environ.get("ANTHROPIC_API_KEY"):
        return "claude-opus-5"
    if os.environ.get("OPENAI_API_KEY"):
        return "gpt-4o-mini"
    if os.environ.get("OPENROUTER_API_KEY"):
        return "openrouter:openai/gpt-4o-mini"
    return None


def bar(labels, values, title="", colors=None, save=None):
    """A clean labeled bar chart; pass ``save="file.png"`` to also export it.

    Requires matplotlib (imported lazily — the rest of this module works
    without it).
    """
    try:
        import matplotlib.pyplot as plt
    except ImportError as e:
        raise ImportError(
            "areev.helpers.bar needs matplotlib: pip install matplotlib") from e
    fig, ax = plt.subplots(figsize=(6, 2.9), dpi=110)
    bars = ax.bar(labels, values, width=0.55, color=colors or BLUE)
    ax.bar_label(bars, padding=2, fontsize=10)
    ax.set_yticks([])
    ax.margins(y=0.18)
    for s in ax.spines.values():
        s.set_visible(False)
    ax.spines["bottom"].set_visible(True)
    ax.spines["bottom"].set_color("#d0cfcb")
    if title:
        ax.set_title(title, fontsize=10, loc="left")
    plt.tight_layout()
    if save:
        plt.savefig(save, bbox_inches="tight")
        print("chart exported →", save)
    plt.show()
