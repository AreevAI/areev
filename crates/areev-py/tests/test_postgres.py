"""Postgres-backend tests for the `areev` PyO3 bindings: the SAME Areev
class over a ``postgres://…?schema=<name>`` DSN. Needs a reachable server
(pgvector image recommended)::

    docker run --rm -d -p 5432:5432 -e POSTGRES_PASSWORD=postgres pgvector/pgvector:pg16
    export DATABASE_URL=postgres://postgres:postgres@127.0.0.1:5432/postgres

Skips without ``AREEV_PG_URL``/``DATABASE_URL``.
"""

import json
import os

import pytest

import areev

URL = os.environ.get("AREEV_PG_URL") or os.environ.get("DATABASE_URL") or ""

pytestmark = pytest.mark.skipif(
    not URL.startswith("postgres"),
    reason="no AREEV_PG_URL/DATABASE_URL postgres server",
)


def dsn_for(schema):
    sep = "&" if "?" in URL else "?"
    return f"{URL}{sep}schema={schema}"


def test_postgres_dsn_end_to_end():
    schema = f"py_smoke_{os.getpid()}"
    try:
        m = areev.Areev(dsn_for(schema), ns="caller", telemetry="off")
        h = m.add_fact("luis", "prefers", "window seat")
        assert len(h) == 64
        got = json.loads(m.recall("luis"))
        assert len(got) == 1
        assert got[0]["fields"]["object"] == "window seat"
        assert json.loads(m.stats())["grains"] == 1
    finally:
        areev.drop_postgres_schema(URL, schema)


def test_two_instances_share_one_memory():
    schema = f"py_multi_{os.getpid()}"
    try:
        a = areev.Areev(dsn_for(schema), ns="ns", telemetry="off")
        b = areev.Areev(dsn_for(schema), ns="ns", telemetry="off")
        for i in range(10):
            a.add_fact(f"a{i}", "writes", "ok")
            b.add_fact(f"b{i}", "writes", "ok")
        assert json.loads(a.stats())["grains"] == 20
        # cross-instance visibility: b reads what a wrote
        assert len(json.loads(b.recall("a3"))) == 1
    finally:
        areev.drop_postgres_schema(URL, schema)


def test_passphrase_with_dsn_is_rejected():
    with pytest.raises(ValueError, match="file-backed"):
        areev.Areev(dsn_for("never_created"), ns="ns", passphrase="secret")


def test_subject_erasure_and_retention():
    schema = f"py_erase_{os.getpid()}"
    try:
        m = areev.Areev(dsn_for(schema), ns="ns", telemetry="off")
        m.add_fact("pat", "condition", "onset")
        m.add_fact("dr_lee", "treats", "pat")
        m.add_fact("mara", "prefers", "tea")
        rep = json.loads(m.forget_subject("pat"))
        assert rep["grains_erased"] == 2
        assert rep["terms_removed"] >= 1
        assert json.loads(m.recall("pat")) == []
        assert json.loads(m.recall("dr_lee")) == []
        assert len(json.loads(m.recall("mara"))) == 1
        # retention: everything to date is older than "now + 1s"
        import time

        rep = json.loads(m.forget_older_than(int(time.time() * 1000) + 1000))
        assert rep["grains_erased"] == 1
        assert json.loads(m.stats())["grains"] == 0
    finally:
        areev.drop_postgres_schema(URL, schema)


def test_vector_index_lifecycle_and_recall_check():
    """#141: the ANN index and its grade, from the binding alone. Needs the
    pgvector extension on the server (the recommended image has it)."""
    import math

    schema = f"py_vec_{os.getpid()}"
    try:
        m = areev.Areev(dsn_for(schema), ns="caller", telemetry="off")
        hs = [m.add_fact(f"s{i}", "has", "vector") for i in range(60)]
        items = [{"hash": h, "vector": [math.cos(i * 0.31), math.sin(i * 0.31), 0.05 * i, 1.0]}
                 for i, h in enumerate(hs)]
        assert json.loads(m.add_embeddings(json.dumps(items)))["written"] == 60
        assert json.loads(m.vector_index())["index"] is None

        built = json.loads(m.ensure_vector_index(m=16, ef_construction=64, ef_search=40))
        assert built["index"] == "idx_embeddings_hnsw"
        assert json.loads(m.vector_index())["index"] == "idx_embeddings_hnsw"

        rep = json.loads(m.vector_recall_check(
            json.dumps([it["vector"] for it in items[:8]]), k=5))
        assert rep["index"] == "idx_embeddings_hnsw"
        assert rep["queries"] == 8 and rep["k"] == 5 and rep["ef_search"] == 40
        assert 0.0 <= rep["recall"] <= 1.0
        retuned = json.loads(m.vector_recall_check(
            json.dumps([it["vector"] for it in items[:8]]), k=5, ef_search=200))
        assert retuned["ef_search"] == 200
        # the exact-scan bypass was lifted: ordinary reads still work after
        near = json.loads(m.nearest_vector(items[3]["vector"], k=1))
        assert near[0]["hash"] == hs[3]

        assert json.loads(m.drop_vector_index())["index"] is None
        assert json.loads(m.vector_index())["index"] is None
    finally:
        areev.drop_postgres_schema(URL, schema)
