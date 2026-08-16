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
