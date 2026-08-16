"""AreevStorageBackend conformance: the full protocol surface, heads-only
reads, audited predicate deletes — and the right-to-erasure demo the
`source` → subject-identity mapping exists for."""

from __future__ import annotations

import json
from datetime import datetime, timedelta, timezone

import areev
import pytest
from crewai.memory.storage.backend import StorageBackend
from crewai.memory.types import MemoryRecord

from areev_crewai import AreevStorageBackend


def _rec(i, content, scope="/", categories=None, source=None, private=False,
         embedding=None, metadata=None, created=None):
    now = created or datetime.now(timezone.utc)
    return MemoryRecord(
        id=f"rec-{i}",
        content=content,
        scope=scope,
        categories=categories or [],
        metadata=metadata or {},
        importance=0.5,
        created_at=now,
        last_accessed=now,
        embedding=embedding,
        source=source,
        private=private,
    )


@pytest.fixture()
def backend(tmp_path):
    return AreevStorageBackend(str(tmp_path / "crew.db"))


def test_satisfies_the_protocol(backend):
    assert isinstance(backend, StorageBackend), "runtime-checkable protocol holds"


def test_save_get_update_list(backend):
    backend.save([_rec(1, "alpha", scope="/agents/researcher")])
    got = backend.get_record("rec-1")
    assert got is not None and got.content == "alpha"

    updated = _rec(1, "alpha v2", scope="/agents/researcher")
    backend.update(updated)
    assert backend.get_record("rec-1").content == "alpha v2"

    # The rewrite is a supersession chain, not an overwrite.
    raw = backend.db
    subject = json.loads(raw.latest("ix:by-id", "crw:id:rec-1", ns="crewai"))["fields"]["object"]
    history = json.loads(raw.history(subject, "crw:record", ns="crewai"))
    assert len(history) == 2

    records = backend.list_records("/agents")
    assert [r.id for r in records] == ["rec-1"]


def test_vector_search_is_heads_only(backend):
    backend.save([_rec(1, "likes rust", embedding=[1.0, 0.0, 0.0])])
    backend.save([_rec(2, "likes python", embedding=[0.0, 1.0, 0.0])])
    hits = backend.search([1.0, 0.0, 0.0], limit=5)
    assert hits and hits[0][0].id == "rec-1"
    assert hits[0][1] > 0.9

    # After an update, the OLD grain's vector must not resurface.
    backend.update(_rec(1, "now prefers go", embedding=[0.0, 0.0, 1.0]))
    hits = backend.search([1.0, 0.0, 0.0], limit=5, min_score=0.5)
    assert all(r.content != "likes rust" for r, _ in hits), "superseded head leaked"


def test_private_records_never_leak_across_scopes(backend):
    backend.save([
        _rec(1, "shared note", scope="/team", embedding=[1.0, 0.0]),
        _rec(2, "my secret", scope="/agents/a1", private=True, embedding=[1.0, 0.0]),
    ])
    unscoped = backend.search([1.0, 0.0], limit=10)
    assert {r.id for r, _ in unscoped} == {"rec-1"}, "private excluded without scope"
    scoped = backend.search([1.0, 0.0], scope_prefix="/agents/a1", limit=10)
    assert {r.id for r, _ in scoped} == {"rec-2"}, "owner scope sees its private record"


def test_scopes_categories_counts(backend):
    backend.save([
        _rec(1, "a", scope="/agents/r1", categories=["pref"]),
        _rec(2, "b", scope="/agents/r2", categories=["pref", "fact"]),
        _rec(3, "c", scope="/shared", categories=["fact"]),
    ])
    assert backend.count() == 3
    assert backend.count("/agents") == 2
    assert backend.list_scopes("/") == ["/agents", "/shared"]
    assert backend.list_scopes("/agents") == ["/agents/r1", "/agents/r2"]
    assert backend.list_categories("/agents") == {"pref": 2, "fact": 1}
    info = backend.get_scope_info("/agents/r1")
    assert info.record_count == 1 and info.categories == ["pref"]


def test_predicate_delete_is_audited(backend):
    old = datetime.now(timezone.utc) - timedelta(days=40)
    backend.save([
        _rec(1, "old pref", categories=["pref"], created=old),
        _rec(2, "new pref", categories=["pref"]),
        _rec(3, "old fact", categories=["fact"], created=old),
    ])
    n = backend.delete(categories=["pref"], older_than=datetime.now(timezone.utc) - timedelta(days=30))
    assert n == 1
    assert backend.get_record("rec-1") is None
    assert backend.get_record("rec-2") is not None
    assert backend.count() == 2

    sweeps = json.loads(backend.db.recall("audit:crewai", k=100, ns="crewai"))
    assert len(sweeps) == 1, "one sweep summary per predicate delete"
    summary = json.loads(sweeps[0]["fields"]["object"])
    assert summary["erased"] == 1 and summary["op"] == "delete"

    # reset(scope) is a predicate too — same path, same audit.
    backend.reset("/")
    assert backend.count() == 0
    sweeps = json.loads(backend.db.recall("audit:crewai", k=100, ns="crewai"))
    assert len(sweeps) == 2


def test_forget_subject_erases_sourced_memories(backend, tmp_path):
    """THE demo: erasing an identity erases every CrewAI memory sourced
    from it — content, supersession history, and index rows — through
    Areev's own FORGET SUBJECT, receipt included."""
    backend.save([
        _rec(1, "alice prefers window seats", source="user:alice", embedding=[1.0, 0.0]),
        _rec(2, "bob is vegetarian", source="user:bob", embedding=[0.0, 1.0]),
        _rec(3, "the team ships fridays"),
    ])
    backend.update(_rec(1, "alice now prefers aisle seats", source="user:alice"))

    # The DSAR view and the erasure share ONE selector: report first.
    report = json.loads(backend.db.subject_report("user:alice", ns="crewai"))
    assert len(report["grains"]) >= 2, "record + index rows are all selected"

    receipt = json.loads(backend.db.forget_subject("user:alice", ns="crewai"))
    assert receipt["grains_erased"] >= 3, f"record + history + index rows: {receipt}"

    assert backend.get_record("rec-1") is None, "sourced memory gone"
    assert backend.get_record("rec-2").content == "bob is vegetarian"
    assert backend.get_record("rec-3") is not None
    # Nothing about alice survives in search either.
    hits = backend.search([1.0, 0.0], limit=10, min_score=0.0)
    assert all("alice" not in r.content for r, _ in hits)


def test_embedding_dim_mismatch_raises(backend):
    backend.save([_rec(1, "x", embedding=[1.0, 0.0, 0.0])])
    with pytest.raises(Exception) as err:
        backend.search([1.0, 0.0])  # wrong dimension vs recorded meta
    assert "VAL" in str(err.value) or "dim" in str(err.value).lower()
