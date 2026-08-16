"""AreevStore conformance: the BaseStore contract driven through the public
convenience methods (which route through batch — so batch IS covered) plus
the supersession/tombstone semantics that make this backend different."""

from __future__ import annotations

import json

import areev
import pytest
from langgraph.store.base import GetOp, PutOp

from areev_langgraph import AreevStore


@pytest.fixture()
def store(tmp_path):
    return AreevStore(str(tmp_path / "memories.db"))


def test_put_get_delete_round_trip(store):
    store.put(("users", "alice"), "profile", {"name": "Alice", "tier": 2})
    item = store.get(("users", "alice"), "profile")
    assert item is not None
    assert item.value == {"name": "Alice", "tier": 2}
    assert item.namespace == ("users", "alice")
    assert item.key == "profile"

    store.delete(("users", "alice"), "profile")
    assert store.get(("users", "alice"), "profile") is None


def test_update_supersedes_and_created_at_survives(store):
    store.put(("u",), "k", {"v": 1})
    first = store.get(("u",), "k")
    store.put(("u",), "k", {"v": 2})
    second = store.get(("u",), "k")
    assert second.value == {"v": 2}
    assert second.created_at == first.created_at, "created_at denormalized forward"
    assert second.updated_at >= first.updated_at

    # The previous value is HISTORY, not gone — visible on Areev surfaces.
    history = json.loads(store.db.history("it:u", "lg:it:k", ns="langgraph"))
    assert len(history) == 2, "supersession chain records the edit"


def test_batch_is_read_your_writes(store):
    results = store.batch(
        [
            PutOp(namespace=("b",), key="x", value={"n": 1}, index=None, ttl=None),
            GetOp(namespace=("b",), key="x", refresh_ttl=False),
        ]
    )
    assert results[0] is None
    assert results[1] is not None and results[1].value == {"n": 1}


def test_search_filters_and_operator_forms(store):
    store.put(("mem", "a"), "1", {"kind": "pref", "score": 5})
    store.put(("mem", "a"), "2", {"kind": "pref", "score": 9})
    store.put(("mem", "b"), "3", {"kind": "fact", "score": 7})

    hits = store.search(("mem",), filter={"kind": "pref"})
    assert {h.key for h in hits} == {"1", "2"}

    hits = store.search(("mem",), filter={"score": {"$gte": 7}})
    assert {h.key for h in hits} == {"2", "3"}

    hits = store.search(("mem", "b"))
    assert [h.key for h in hits] == ["3"], "prefix scopes the namespace"

    assert store.search(("mem",), limit=1, offset=2) != store.search(("mem",), limit=1)


def test_search_with_text_query_ranks(store):
    store.put(("notes",), "n1", {"text": "the quarterly billing report is late"})
    store.put(("notes",), "n2", {"text": "cats are excellent debuggers"})
    hits = store.search(("notes",), query="billing report")
    assert hits, "query search returns results"
    assert hits[0].key == "n1", f"ranked hit first: {[(h.key, h.score) for h in hits]}"


def test_ttl_honesty(store):
    # supports_ttl=False: an EXPLICIT ttl on put is refused by the base
    # class itself (upstream's guard — grains don't expire), while
    # refresh_ttl on reads is accepted and ignored.
    assert store.supports_ttl is False
    with pytest.raises(NotImplementedError):
        store.put(("t",), "k", {"v": 1}, ttl=5.0)
    store.put(("t",), "k", {"v": 1})
    assert store.get(("t",), "k", refresh_ttl=True).value == {"v": 1}


def test_list_namespaces_prefix_suffix_depth(store):
    for ns in [("a", "x"), ("a", "y", "deep"), ("b", "x")]:
        store.put(ns, "k", {"v": 1})
    all_ns = store.list_namespaces()
    assert ("a", "x") in all_ns and ("b", "x") in all_ns

    assert store.list_namespaces(prefix=("a",)) == [("a", "x"), ("a", "y", "deep")]
    assert store.list_namespaces(suffix=("x",)) == [("a", "x"), ("b", "x")]
    assert store.list_namespaces(prefix=("*", "x")) == [("a", "x"), ("b", "x")]
    assert store.list_namespaces(max_depth=1) == [("a",), ("b",)]
