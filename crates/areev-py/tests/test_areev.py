"""End-to-end tests for the `areev` PyO3 bindings.

These drive the real compiled extension module (built with
`maturin develop -m crates/areev-py/Cargo.toml`) against a fresh
per-test temp database (pytest `tmp_path`). The FFI convention is
"scalars in, JSON strings out", so every structured return is parsed
with `json.loads` and asserted on shape + content. No test asserts on a
wall-clock threshold, so the suite is deterministic — the one test that
touches the clock (`test_store_calls_release_the_gil`) asserts thread
interleaving and skips itself rather than fail if the call it needs to
observe finishes too quickly.
"""

import json
import sys
import threading
import time

import pytest

import areev


HEX64 = 64  # length of a SHA-256 content address in hex


def make_db(tmp_path, ns="caller"):
    """Open a brand-new one-file memory in a temp dir."""
    return areev.Areev(str(tmp_path / "test.db"), ns=ns)


# --------------------------------------------------------------------------
# module surface
# --------------------------------------------------------------------------

def test_module_exposes_class_and_version():
    assert hasattr(areev, "Areev")
    assert isinstance(areev.__version__, str)
    assert areev.__version__  # non-empty


# --------------------------------------------------------------------------
# add / recall roundtrip
# --------------------------------------------------------------------------

def test_add_fact_returns_content_address(tmp_path):
    m = make_db(tmp_path)
    h = m.add_fact("john", "prefers", "tea", confidence=0.95)
    assert isinstance(h, str)
    assert len(h) == HEX64
    int(h, 16)  # a valid 64-hex content address


def test_recall_roundtrip(tmp_path):
    m = make_db(tmp_path)
    m.add_fact("john", "prefers", "tea")

    rows = json.loads(m.recall("john"))
    assert isinstance(rows, list)
    assert len(rows) == 1

    row = rows[0]
    assert {"hash", "type", "fields"} <= set(row)
    assert row["type"] == "fact"
    assert len(row["hash"]) == HEX64
    assert row["fields"]["subject"] == "john"
    assert row["fields"]["relation"] == "prefers"
    assert row["fields"]["object"] == "tea"


def test_recall_relation_filter(tmp_path):
    m = make_db(tmp_path)
    m.add_fact("john", "prefers", "tea")
    m.add_fact("john", "speaks", "german")

    everything = json.loads(m.recall("john"))
    assert len(everything) == 2

    speaks = json.loads(m.recall("john", relation="speaks"))
    assert len(speaks) == 1
    assert speaks[0]["fields"]["object"] == "german"


def test_add_generic_grain(tmp_path):
    m = make_db(tmp_path)
    h = m.add(
        "fact",
        json.dumps(
            {
                "subject": "alice",
                "relation": "likes",
                "object": "coffee",
                "confidence": 0.8,
            }
        ),
    )
    assert len(h) == HEX64

    rows = json.loads(m.recall("alice"))
    assert rows[0]["fields"]["object"] == "coffee"


# --------------------------------------------------------------------------
# CAL query language ("JSON string out")
# --------------------------------------------------------------------------

def test_cal_recall_shape(tmp_path):
    m = make_db(tmp_path)
    m.add_fact("john", "prefers", "tea")

    payload = json.loads(m.cal('RECALL facts WHERE subject = "john"'))
    assert payload["type"] == "grains"
    assert isinstance(payload["grains"], list)
    assert len(payload["grains"]) == 1

    grain = payload["grains"][0]
    assert grain["grain_type"] == "fact"
    assert grain["fields"]["object"] == "tea"
    assert len(grain["hash"]) == HEX64


def test_cal_count_pipeline(tmp_path):
    m = make_db(tmp_path)
    m.add_fact("john", "prefers", "tea")
    m.add_fact("john", "speaks", "german")

    payload = json.loads(m.cal('RECALL facts WHERE subject = "john" | COUNT'))
    assert payload["type"] == "count"
    assert payload["count"] == 2


def test_cal_surfaces_warnings(tmp_path):
    """CAL-W* warnings must reach the caller (#68).

    `score_breakdown` cannot change a RECALL result and the executor raises
    CAL-W014 to say so, but the binding serialized the payload alone — so the
    option stayed exactly as silent as before the warning existed.
    """
    m = make_db(tmp_path)
    m.add_fact("john", "prefers", "tea")

    got = json.loads(m.cal('RECALL facts WHERE subject = "john" WITH score_breakdown'))
    assert isinstance(got.get("warnings"), list)
    assert any("CAL-W014" in w for w in got["warnings"])
    assert got["type"] == "grains"  # the rest of the payload is unchanged

    # A clean query keeps the shape it always had — no empty array.
    clean = json.loads(m.cal('RECALL facts WHERE subject = "john"'))
    assert "warnings" not in clean


def test_record_tool_call_records_occurrences(tmp_path):
    """Identical retries are distinct occurrences, not one grain (#66)."""
    m = make_db(tmp_path)
    for _ in range(5):
        m.record_tool_call("stripe_refund", '{"error":"rate_limited"}', True)

    n = json.loads(m.cal('RECALL tools WHERE tool_name = "stripe_refund" | COUNT'))
    assert n["count"] == 5

    # A host-supplied call id lands on the grain and is filterable.
    m.record_tool_call(
        "stripe_refund", "boom", True, call_id="call_xyz", input='{"amount":42}'
    )
    by_id = json.loads(m.cal('RECALL tools WHERE tool_call_id = "call_xyz"'))
    assert len(by_id["grains"]) == 1
    assert by_id["grains"][0]["fields"]["input"]["amount"] == 42

    manifest = json.loads(m.record_run_manifest(
        "run-py", '{"model":{"base":"test"},"sampling":{"seed":7}}'))
    assert len(manifest["config_hash"]) == 64
    assert len(manifest["link_hash"]) == 64
    m.set_run_id("run-py")
    m.recall("acct")
    m.set_run_id(None)


def test_add_explains_engine_authored_types(tmp_path):
    """A type that exists but may not be host-authored says why (#67).

    `DESCRIBE` lists 12 grain types; `add()` used to call the 12th unknown,
    sending the caller to hunt for a typo in a name the engine had just
    returned to them.
    """
    m = make_db(tmp_path)
    assert len(json.loads(m.cal("DESCRIBE"))["info"]["grain_types"]) == 12

    with pytest.raises(ValueError) as e:
        m.add("recommendation", json.dumps({"target_ref": "entity:x", "analyzer": "loop.x/1",
                                            "summary": "s", "dedup_key": "k"}))
    assert "engine-authored" in str(e.value)
    assert "unknown grain type" not in str(e.value)

    with pytest.raises(ValueError) as e:
        m.add("sandwich", "{}")
    assert "unknown grain type" in str(e.value)


# --------------------------------------------------------------------------
# evolution: supersede / latest / history
# --------------------------------------------------------------------------

def test_supersede_and_latest(tmp_path):
    m = make_db(tmp_path)
    h1 = m.add_fact("john", "prefers", "tea")
    h2 = m.supersede(
        h1,
        "fact",
        json.dumps({"subject": "john", "relation": "prefers", "object": "coffee"}),
    )
    assert h2 != h1
    assert len(h2) == HEX64

    latest = json.loads(m.latest("john", "prefers"))
    assert latest["fields"]["object"] == "coffee"
    assert latest["hash"] == h2


def test_latest_missing_returns_none(tmp_path):
    m = make_db(tmp_path)
    assert m.latest("nobody", "prefers") is None


def test_history_chain(tmp_path):
    m = make_db(tmp_path)
    h1 = m.add_fact("john", "prefers", "tea")
    m.supersede(
        h1,
        "fact",
        json.dumps({"subject": "john", "relation": "prefers", "object": "coffee"}),
    )
    versions = json.loads(m.history("john", "prefers"))
    assert isinstance(versions, list)
    assert len(versions) >= 2
    assert {"hash", "object"} <= set(versions[0])


# --------------------------------------------------------------------------
# remember / stats / verify
# --------------------------------------------------------------------------

def test_remember_returns_event(tmp_path):
    m = make_db(tmp_path)
    res = json.loads(m.remember("John likes tea"))
    assert "event" in res
    assert len(res["event"]) == HEX64
    assert isinstance(res["facts"], list)
    # The raw text is an Event grain — same as the MCP tool and capture-stop.
    rows = json.loads(m.cal("RECALL events"))
    assert rows["grains"][0]["fields"]["content"] == "John likes tea"


def test_remember_threads_a_turn_by_session_and_role(tmp_path):
    m = make_db(tmp_path)
    m.remember("what's the refund policy?", session_id="call-1", role="user")
    rows = json.loads(m.cal("RECALL events"))
    fields = rows["grains"][0]["fields"]
    assert fields["session_id"] == "call-1"
    assert fields["role"] == "user"


def test_remember_with_prelinked_facts(tmp_path):
    m = make_db(tmp_path)
    facts = json.dumps(
        [{"subject": "john", "relation": "likes", "object": "tea", "confidence": 0.9}]
    )
    res = json.loads(m.remember("John likes tea", facts_json=facts))
    assert len(res["facts"]) == 1
    assert all(len(h) == HEX64 for h in res["facts"])
    # Host-supplied facts are the host's own assertion — no model attribution.
    assert "model" not in res


def test_remember_rejects_incomplete_facts(tmp_path):
    m = make_db(tmp_path)
    with pytest.raises(ValueError, match=r"facts\[0\]"):
        m.remember("note", facts_json=json.dumps([{"subject": "john"}]))


FAKE_LLM_PY = """
import json, sys
d = json.loads(sys.stdin.read())
op = d.get("op", "")
if op == "probe":
    print(json.dumps({"model": "fake-extractor-1"}))
elif op == "extract":
    print(json.dumps({"facts": [
        {"subject": "john", "relation": "prefers", "object": "window seat", "confidence": 0.9},
        {"subject": "john", "relation": "guess", "object": "likes jazz", "confidence": 0.2},
    ]}))
elif op == "ground":
    print(json.dumps({"results": [
        {"id": c["id"], "supported": "guess" not in c["claim"], "reason": "checked"}
        for c in d.get("claims", [])
    ]}))
else:
    print(json.dumps({}))
"""


@pytest.fixture
def fake_llm(tmp_path):
    """A `llm_cmd` string driving a scripted fake — hermetic, no network."""
    script = tmp_path / "fake_llm.py"
    script.write_text(FAKE_LLM_PY)
    return f"{sys.executable} {script}"


def test_remember_extracts_with_a_model(tmp_path, fake_llm):
    m = make_db(tmp_path)
    res = json.loads(m.remember("I always want a window seat.", llm_cmd=fake_llm))
    assert res["model"] == "fake-extractor-1"
    assert res["proposed"] == 2
    assert res["dropped"] == 0
    assert res["verification_status"] == "unverified"
    assert len(res["facts"]) == 2

    # Provenance is on the grains, not just in the return value.
    rows = json.loads(m.cal('RECALL facts WHERE verification_status = "unverified"'))
    assert len(rows["grains"]) == 2
    for g in rows["grains"]:
        assert g["fields"]["derived_from"] == res["event"]
        assert g["fields"]["extractor_model"] == "fake-extractor-1"


def test_remember_grounding_drops_unsupported_facts(tmp_path, fake_llm):
    m = make_db(tmp_path)
    res = json.loads(
        m.remember("I always want a window seat.", llm_cmd=fake_llm, ground_cmd=fake_llm)
    )
    assert res["proposed"] == 2
    assert res["dropped"] == 1
    assert res["verification_status"] == "verified"
    assert len(res["facts"]) == 1


def test_remember_confidence_floor(tmp_path, fake_llm):
    m = make_db(tmp_path)
    res = json.loads(
        m.remember("I always want a window seat.", llm_cmd=fake_llm, min_confidence=0.5)
    )
    assert res["dropped"] == 1
    assert len(res["facts"]) == 1


def test_remember_keeps_the_source_text_when_extraction_fails(tmp_path):
    m = make_db(tmp_path)
    script = tmp_path / "dying_llm.py"
    script.write_text(
        "import json, sys\n"
        "d = json.loads(sys.stdin.read())\n"
        "print(json.dumps({'model': 'dying-1'})) if d.get('op') == 'probe' else sys.exit(3)\n"
    )
    with pytest.raises(ValueError, match="was stored"):
        m.remember("some raw note", llm_cmd=f"{sys.executable} {script}")
    # The raw text survived the failed extraction.
    rows = json.loads(m.cal("RECALL events"))
    assert len(rows["grains"]) == 1


def test_stats_shape(tmp_path):
    m = make_db(tmp_path)
    m.add_fact("john", "prefers", "tea")
    s = json.loads(m.stats())
    for key in ("grains", "current", "triples", "terms", "ops"):
        assert key in s
    assert s["grains"] >= 1


def test_verify_ok(tmp_path):
    m = make_db(tmp_path)
    m.add_fact("john", "prefers", "tea")
    report = json.loads(m.verify())
    assert report["integrity"] == "ok"
    assert report["grains"] >= 1


# --------------------------------------------------------------------------
# error paths -> ValueError (PyValueError)
# --------------------------------------------------------------------------

def test_bad_hash_raises_valueerror(tmp_path):
    m = make_db(tmp_path)
    with pytest.raises(ValueError):
        m.forget("not-a-valid-hash")


def test_bad_json_raises_valueerror(tmp_path):
    m = make_db(tmp_path)
    with pytest.raises(ValueError):
        m.add("fact", "{ this is not valid json")


def test_destructive_cal_raises_valueerror(tmp_path):
    # CAL structurally cannot destroy data: DELETE is not a grammar token,
    # so parsing it fails and surfaces as PyValueError.
    m = make_db(tmp_path)
    with pytest.raises(ValueError):
        m.cal("DELETE sha256:abc")


# --------------------------------------------------------------------------
# embedder callback, migration, reindex, encryption
# --------------------------------------------------------------------------

def _toy_embed(text):
    """Deterministic 8-dim embedding: identical text -> identical vector."""
    import hashlib
    h = hashlib.sha256(text.encode()).digest()
    return [b / 255.0 for b in h[:8]]


def test_set_embedder_callback_is_wired_into_writes(tmp_path):
    path = str(tmp_path / "vec.db")
    m = areev.Areev(path, ns="main")
    m.set_embedder(_toy_embed, model="sha-toy")
    # adds run the callback (a broken one would raise on add)
    m.add_fact("alice", "prefers", "tea", ns="main")
    m.add_fact("bob", "prefers", "coffee", ns="main")
    del m

    # provenance was stamped: reopening with a different-dim embedder warns
    m = areev.Areev(path, ns="main")
    m.set_embedder(lambda text: [0.0, 1.0, 2.0], model="other")
    warnings = json.loads(m.open_warnings())
    assert any("embedding mismatch" in w for w in warnings), warnings


def test_set_embedder_rejects_bad_callback(tmp_path):
    m = make_db(tmp_path)
    with pytest.raises(ValueError):
        m.set_embedder(lambda text: [])  # empty vector
    with pytest.raises(ValueError):
        m.set_embedder(lambda text: "not a vector")


def test_migrate_mem0_history_chain_and_rerun(tmp_path):
    m = make_db(tmp_path, ns="main")
    history = json.dumps([
        {"memory_id": "m-1", "event": "ADD", "new_memory": "Works at Acme",
         "created_at": "2024-03-01T10:00:00Z"},
        {"memory_id": "m-1", "event": "UPDATE", "new_memory": "Works at Initech",
         "created_at": "2024-06-01T10:00:00Z"},
    ])
    rep = json.loads(m.migrate("mem0-history", history, ns="main"))
    assert (rep["added"], rep["superseded"]) == (1, 1)

    head = json.loads(m.latest("mem0/m-1", "mem0_memory", ns="main"))
    assert head["fields"]["context"]["content"] == "Works at Initech"
    versions = json.loads(m.history("mem0/m-1", "mem0_memory", ns="main"))
    assert len(versions) == 2

    # re-run: no duplicates, no error
    rep2 = json.loads(m.migrate("mem0-history", history, ns="main"))
    assert rep2["added"] == 0


def test_migrate_unknown_source_raises(tmp_path):
    m = make_db(tmp_path)
    with pytest.raises(ValueError):
        m.migrate("not-a-source", "{}")


def test_reindex_text_returns_count(tmp_path):
    m = make_db(tmp_path)
    m.add_fact("john", "prefers", "tea")
    assert isinstance(m.reindex_text(), int)


def test_reindex_links_rebuilds_the_join_indexes(tmp_path):
    # The counterpart of reindex_text: without it a Python-only user had no way
    # to backfill a file whose provenance and run indexes predate this build.
    m = make_db(tmp_path)
    m.remember("the caller asked about refunds", run_id="run-a")
    rows = m.reindex_links()
    assert isinstance(rows, int)
    # Idempotent: the tables are cleared and rebuilt, never appended to.
    assert m.reindex_links() == rows
    assert len(json.loads(m.run_trace("run-a"))["trace"]) == 1


def test_remember_can_place_a_turn_in_a_run(tmp_path):
    # run_trace/run_yield/runs_touching shipped without any way to *write* a
    # run_id, so from Python they could only ever answer "nothing".
    m = make_db(tmp_path)
    m.remember("the caller asked about refunds", run_id="run-a")
    m.remember("the agent quoted the 30-day policy", run_id="run-a")
    m.remember("unrelated turn", run_id="run-b")

    trace = json.loads(m.run_trace("run-a"))
    assert trace["run_id"] == "run-a"
    assert len(trace["trace"]) == 2
    assert len(json.loads(m.run_trace("run-b"))["trace"]) == 1
    assert json.loads(m.run_trace("no-such-run"))["trace"] == []


def test_passphrase_roundtrip_and_wrong_key(tmp_path):
    path = str(tmp_path / "enc.db")
    m = areev.Areev(path, ns="caller", passphrase="correct horse battery staple")
    m.add_fact("john", "prefers", "tea")
    del m
    with pytest.raises(ValueError):
        areev.Areev(path, ns="caller", passphrase="wrong")
    with pytest.raises(ValueError):
        areev.Areev(path, ns="caller")  # encrypted file, no key
    m = areev.Areev(path, ns="caller", passphrase="correct horse battery staple")
    assert len(json.loads(m.recall("john"))) == 1


def test_open_warnings_is_json_list(tmp_path):
    m = make_db(tmp_path)
    assert isinstance(json.loads(m.open_warnings()), list)


# --------------------------------------------------------------------------
# areev-loop — the governed self-improvement loop
# --------------------------------------------------------------------------

def test_loop_loop_rollback_and_outcomes(tmp_path):
    m = make_db(tmp_path)
    # Byte-identical failures on purpose. Whether four identical failures in one
    # millisecond are four grains was an open engine question when this test was
    # written, and it worked around it by jittering the payload; #66 settled it —
    # a retry is a countable occurrence, so `record_tool_call` gives each call
    # its own identity and all four survive.
    for _ in range(4):
        m.record_tool_call("stripe_refund", "rate_limited 429", True)
    m.record_tool_call("stripe_refund", "ok", False)

    run = json.loads(m.loop_run())
    assert run["outcome"] == "ran"
    assert run["stored"] >= 1

    pending = json.loads(m.recommendations())
    tf = next(r for r in pending if r["analyzer"].startswith("loop.tool_failure"))
    assert "rate_limited" in tf["summary"]

    applied = json.loads(m.apply_recommendation(tf["hash"], "codify the lesson"))
    assert applied["rollbackable"] is True

    # The Verify gate's record is a JSON list (empty until checkpoints elapse).
    assert isinstance(json.loads(m.loop_outcomes()), list)

    rb = json.loads(m.rollback_recommendation(tf["hash"], "the lesson did not help"))
    assert rb["status"] == "rolled_back"

    # A full-memory sweep (the `areev loop reflect` semantics) still runs.
    sweep = json.loads(m.loop_run(full_sweep=True))
    assert sweep["outcome"] == "ran"


def test_loop_policy_file_grants_auto_apply(tmp_path):
    """The bindings honor a host loop-policy.json (path in, same file the
    CLI takes) — and only value-identical structural curation auto-applies."""
    m = make_db(tmp_path)
    # A case-variant exact duplicate (distinct bytes, same normalized value).
    m.add_fact("acme", "tier", "Enterprise")
    m.add_fact("acme", "tier", "enterprise")

    policy = tmp_path / "loop-policy.json"
    policy.write_text(json.dumps({
        "auto_apply_enabled": True,
        "auto_apply": [
            {"analyzer": "loop.duplicate_sweep", "targets": ["memory"], "max_severity": "low"}
        ],
    }))

    # Without the policy nothing auto-applies.
    run = json.loads(m.loop_run())
    assert run["auto_applied"] == 0

    # The pending consolidation is not re-proposed on a re-run (dedup), so
    # seed a fresh file to see the grant auto-apply end-to-end.
    fresh = areev.Areev(str(tmp_path / "granted.db"), ns="caller")
    fresh.add_fact("acme", "tier", "Enterprise")
    fresh.add_fact("acme", "tier", "enterprise")
    granted = json.loads(fresh.loop_run(policy=str(policy)))
    assert granted["auto_applied"] == 1

    bad = tmp_path / "bad-policy.json"
    bad.write_text('{"auto_apply_enabled": true, "surprise": 1}')
    with pytest.raises(ValueError):
        fresh.loop_run(policy=str(bad))  # unknown keys are rejected


# --------------------------------------------------------------------------
# threading: store calls must not pin the interpreter
# --------------------------------------------------------------------------

def test_store_calls_release_the_gil(tmp_path):
    """A long store call must let other Python threads run.

    The one test here that involves the clock, and it deliberately asserts a
    *structural* property rather than a threshold: while this thread sits
    inside a single native call, another Python thread has to get scheduled at
    least once. Before the bindings released the GIL, every call held it for
    its full duration, so a host that moved writes onto a background thread —
    which is what agent frameworks do precisely so a slow write cannot stall a
    turn — got no isolation at all. Waiting on the store mutex had the same
    shape: block on the lock, keep the GIL, freeze every other thread.
    """
    src = areev.Areev(str(tmp_path / "src.db"), ns="caller")
    # Seed via migrate: it loads under a deferred text index, so building the
    # fixture stays fast even though the import below is deliberately not.
    payload = "\n".join(
        json.dumps({"subject": f"user{i % 40}", "relation": "prefers", "object": f"value {i}"})
        for i in range(600)
    )
    assert json.loads(src.migrate("jsonl", payload))["added"] == 600
    ops = str(tmp_path / "ops.bundle")
    src.bundle(ops, 0)

    dst = areev.Areev(str(tmp_path / "dst.db"), ns="caller")

    ticks = []
    stop = threading.Event()

    def sampler():
        # sleep() yields the GIL, so this thread is only ever blocked by
        # someone else holding it — which is exactly what we are testing for.
        while not stop.is_set():
            ticks.append(time.perf_counter())
            time.sleep(0.005)

    watcher = threading.Thread(target=sampler, daemon=True)
    watcher.start()
    try:
        while not ticks:  # make sure it is actually running before we start
            time.sleep(0.005)
        started = time.perf_counter()
        dst.import_bundle(ops)  # one long native call
        ended = time.perf_counter()
    finally:
        stop.set()
        watcher.join(timeout=5)

    elapsed = ended - started
    if elapsed < 0.05:
        pytest.skip(f"import_bundle took {elapsed * 1000:.0f} ms — too short to observe interleaving")

    # The property is "the sampler was never locked out for long", not "it ran
    # at least once": a single tick lands on the boundary even when the GIL is
    # held end to end, so counting ticks would pass against a binding that
    # never yields. Measure the worst stall instead. Held for the whole call,
    # the gap equals the call; yielding, it stays at the sampler's own cadence
    # (measured: 3406 ms vs 7 ms across this exact call).
    gaps = [b - a for a, b in zip(ticks, ticks[1:])]
    worst = max(gaps) if gaps else elapsed
    assert worst < elapsed / 4, (
        f"another thread was starved for {worst * 1000:.0f} ms during a "
        f"{elapsed * 1000:.0f} ms store call — the GIL is being held across it"
    )


# --------------------------------------------------------------------------
# index_text, add_batch, search
# --------------------------------------------------------------------------

def test_index_text_defaults_to_the_file_declaration(tmp_path):
    path = str(tmp_path / "decl.db")
    # Explicit -> deliberate re-stamp, no warning on a file with no prior claim.
    off = areev.Areev(path, ns="caller", index_text=False)
    assert json.loads(off.open_warnings()) == []
    del off

    # Bare reopen honors what the file declares, silently.
    bare = areev.Areev(path, ns="caller")
    assert json.loads(bare.open_warnings()) == []
    del bare

    # Flipping it back on is a change the operator should see, not discover.
    on = areev.Areev(path, ns="caller", index_text=True)
    warnings = json.loads(on.open_warnings())
    assert any("text_index" in w for w in warnings), warnings


def test_search_finds_by_free_text(tmp_path):
    m = make_db(tmp_path)
    m.add_fact("john", "prefers", "window seat")
    m.add_fact("mary", "prefers", "aisle seat")

    hits = json.loads(m.search("window"))
    assert [h["fields"]["object"] for h in hits] == ["window seat"]
    assert len(hits[0]["hash"]) == HEX64

    # Anchoring narrows without losing the free-text leg.
    assert json.loads(m.search("seat", subject="mary"))[0]["fields"]["subject"] == "mary"


def test_search_without_any_leg_raises_rather_than_answering_empty(tmp_path):
    # No BM25 index and no embedder: there is nothing to rank on. Returning []
    # would read as "no matching memories", which is a wrong answer rather than
    # an empty one.
    m = areev.Areev(str(tmp_path / "noleg.db"), ns="caller", index_text=False)
    m.add_fact("john", "prefers", "window seat")
    assert json.loads(m.recall("john"))  # structural recall still works
    with pytest.raises(ValueError, match="text or vector leg") as exc:
        m.search("window")
    # Following the first remedy alone leaves a silent empty result: reopening
    # with index_text=True turns indexing on for FUTURE writes, so grains
    # written while it was off stay invisible. The error has to name the second
    # step, because a user who hit an exception is looking at the exception.
    assert "reindex_text()" in str(exc.value), str(exc.value)


def test_reopening_with_index_text_needs_reindex_to_see_old_grains(tmp_path):
    # The exact sequence the error text now describes, end to end.
    path = str(tmp_path / "idx.db")
    m = areev.Areev(path, ns="caller", index_text=False)
    m.add_fact("john", "prefers", "window seat")
    del m

    m2 = areev.Areev(path, ns="caller", index_text=True)
    assert json.loads(m2.search("window")) == [], "written while indexing was off"
    m2.reindex_text()
    assert len(json.loads(m2.search("window"))) == 1


def test_nearest_without_an_embedder_names_the_python_remedy(tmp_path):
    # The message used to point at `--embed-cmd` (an `areev` CLI flag that does
    # not exist in Python; pip installs no binary) and call this a "novelty
    # check" (the CLI's verb) when the caller had reached it through nearest().
    m = make_db(tmp_path)
    m.add_fact("john", "prefers", "tea")
    with pytest.raises(ValueError) as exc:
        m.nearest("tea", k=3)
    msg = str(exc.value)
    assert "nearest()" in msg, msg
    assert "set_embedder" in msg and "set_embedder_command" in msg, msg
    assert "--embed-cmd" not in msg, msg
    assert "novelty check" not in msg, msg


def test_valid_to_at_top_level_is_stored_as_the_typed_field(tmp_path):
    # add() accepts the documented spelling, and the value lands in exactly one
    # place — not swallowed into context as the compacted key "vt", where
    # loop.staleness and the world-time projection cannot see it.
    m = make_db(tmp_path)
    past = 1_600_000_000_000
    m.add("fact", json.dumps({
        "subject": "promo", "relation": "code", "object": "SAVE20",
        "valid_to": past,
    }))
    fields = json.loads(m.recall("promo"))[0]["fields"]
    assert fields.get("valid_to") == past, fields
    assert "vt" not in json.dumps(fields.get("context", {})), fields


def test_add_batch_roundtrip(tmp_path):
    m = make_db(tmp_path)
    hashes = json.loads(m.add_batch(json.dumps([
        {"grain_type": "fact", "fields": {"subject": "ann", "relation": "likes", "object": "tea"}},
        {"type": "fact", "fields": {"subject": "bob", "relation": "likes", "object": "coffee"}},
    ])))
    assert len(hashes) == 2
    assert all(len(h) == HEX64 for h in hashes)
    assert json.loads(m.recall("ann"))[0]["fields"]["object"] == "tea"
    assert json.loads(m.recall("bob"))[0]["fields"]["object"] == "coffee"


def test_add_batch_rejects_the_whole_call_and_writes_nothing(tmp_path):
    m = make_db(tmp_path)
    before = json.loads(m.stats())["grains"]
    with pytest.raises(ValueError):
        m.add_batch(json.dumps([
            {"grain_type": "fact", "fields": {"subject": "ann", "relation": "likes", "object": "tea"}},
            {"grain_type": "fact"},  # no fields -> the batch is refused
        ]))
    assert json.loads(m.stats())["grains"] == before


# --------------------------------------------------------------------------
# graph traversal, as-of reads, workflow execution records
# --------------------------------------------------------------------------


def org_graph(tmp_path):
    """alice -> bob -> carol over `reports_to`, a default entity-valued relation."""
    m = make_db(tmp_path, ns="org")
    m.add("fact", json.dumps({"subject": "alice", "relation": "reports_to", "object": "bob"}))
    m.add("fact", json.dumps({"subject": "bob", "relation": "reports_to", "object": "carol"}))
    return m


def test_related_walks_forward_and_backward(tmp_path):
    m = org_graph(tmp_path)
    assert json.loads(m.related("alice", "reports_to"))["reached"] == ["bob", "carol"]
    # Reverse traversal needs the OSP index, which only covers entity-valued
    # relations — `reports_to` is one of the defaults.
    assert json.loads(m.related("carol", "reports_to", "in"))["reached"] == ["bob", "alice"]


def test_related_accepts_a_comma_separated_relation_list(tmp_path):
    m = org_graph(tmp_path)
    out = json.loads(m.related("alice", "reports_to, mg:knows"))
    assert out["start"] == "alice"
    assert "bob" in out["reached"]


def test_related_rejects_a_bad_direction_and_an_empty_relation_list(tmp_path):
    m = org_graph(tmp_path)
    with pytest.raises(ValueError):
        m.related("alice", "reports_to", "sideways")
    with pytest.raises(ValueError):
        m.related("alice", "   ")


def test_related_depth_bounds_the_walk(tmp_path):
    m = org_graph(tmp_path)
    assert json.loads(m.related("alice", "reports_to", "out", 1))["reached"] == ["bob"]


def test_entity_at_reads_the_knowledge_axis(tmp_path):
    m = org_graph(tmp_path)
    now = json.loads(m.stats())  # any call; we just need a plausible clock below
    assert now is not None
    at = json.loads(m.entity_at("alice", "reports_to", 4102444800000, "knowledge"))
    assert at["found"] is True
    assert at["grain"]["fields"]["object"] == "bob"


def test_entity_at_reports_nothing_rather_than_raising(tmp_path):
    m = org_graph(tmp_path)
    assert json.loads(m.entity_at("nobody", "reports_to", 4102444800000))["found"] is False


def test_entity_at_rejects_a_bad_axis(tmp_path):
    m = org_graph(tmp_path)
    with pytest.raises(ValueError):
        m.entity_at("alice", "reports_to", 1, "sometime")


def test_step_actions_reads_execution_records(tmp_path):
    m = make_db(tmp_path, ns="org")
    wf = json.loads(m.cal('ADD workflow "ci" build -> test REASON "test"'))["hash"]
    assert len(wf) == HEX64
    out = json.loads(m.step_actions(wf))
    assert out["workflow"] == wf
    assert out["steps"] == []  # the plan exists; nothing has run against it


def test_step_actions_rejects_a_bad_hash(tmp_path):
    m = make_db(tmp_path, ns="org")
    with pytest.raises(ValueError):
        m.step_actions("not-a-hash")


# --------------------------------------------------------------------------
# the join: run history <-> semantic memory
# --------------------------------------------------------------------------


def test_run_trace_and_yield_cross_the_seam(tmp_path):
    m = make_db(tmp_path, ns="ops")
    ev = m.add("event", json.dumps({
        "content": "caller asked about refunds",
        "run_id": "run-a",
    }))
    m.add("fact", json.dumps({
        "subject": "refunds", "relation": "window_days", "object": "30",
        "derived_from": ev,
    }))

    out = json.loads(m.run_trace("run-a"))
    assert out["run_id"] == "run-a"
    assert len(out["trace"]) == 1
    assert out["trace"][0]["fields"]["run_id"] == "run-a"
    # `produced` is what the run yielded downstream and is not in the run.
    assert len(out["produced"]) == 1
    assert out["produced"][0]["fields"]["object"] == "30"


def test_run_trace_can_skip_the_yield(tmp_path):
    m = make_db(tmp_path, ns="ops")
    m.add("event", json.dumps({"content": "x", "run_id": "run-a"}))
    out = json.loads(m.run_trace("run-a", 64, False))
    assert out["produced"] == []


def test_runs_touching_walks_provenance_back_to_the_run(tmp_path):
    m = make_db(tmp_path, ns="ops")
    ev = m.add("event", json.dumps({
        "content": "caller asked about refunds",
        "run_id": "run-a",
    }))
    fact = m.add("fact", json.dumps({
        "subject": "refunds", "relation": "window_days", "object": "30",
        "derived_from": ev,
    }))

    out = json.loads(m.runs_touching(fact))
    assert out["hash"] == fact
    assert out["runs"] == ["run-a"]


def test_unknown_run_answers_empty(tmp_path):
    m = make_db(tmp_path, ns="ops")
    out = json.loads(m.run_trace("no-such-run"))
    assert out["trace"] == [] and out["produced"] == []


def test_runs_touching_rejects_a_bad_hash(tmp_path):
    m = make_db(tmp_path, ns="ops")
    with pytest.raises(ValueError):
        m.runs_touching("not-a-hash")


# ── Areev Loop: the review queue and its gates (#34, #35, #39) ──────────────────

def _a_destructive_rec(tmp_path, name="w.db"):
    """A pending destructive recommendation: loop.staleness on an expired fact."""
    m = areev.Areev(str(tmp_path / name), ns="caller")
    # The compact spelling `vt` — both spellings reach `common.valid_to`, and
    # this one keeps the fixture independent of the top-level `valid_to`
    # routing fix (#36), which lands separately.
    m.add("fact", json.dumps({
        "subject": "promo", "relation": "code", "object": "SAVE20",
        "vt": 1_600_000_000_000,
    }))
    m.loop_run(full_sweep=True)
    recs = json.loads(m.recommendations())
    assert recs and recs[0]["destructive"], recs
    return m, recs[0]["hash"]


def test_recommendations_status_all_spans_every_state(tmp_path):
    # `"all"` used to filter itself to None and then have the pending default
    # re-applied, so it behaved as `"pending"` — and an applied or rejected
    # recommendation was missing from a list documented as "every status".
    m, h = _a_destructive_rec(tmp_path)
    m.dismiss_recommendation(h, "not now")

    assert [r["status"] for r in json.loads(m.recommendations('{"status":"all"}'))] == ["rejected"]
    assert [r["status"] for r in json.loads(m.recommendations('{"status":"rejected"}'))] == ["rejected"]
    assert json.loads(m.recommendations('{"status":"pending"}')) == []
    # Omitting the filter still defaults to pending.
    assert json.loads(m.recommendations()) == []
    with pytest.raises(ValueError, match="unknown status"):
        m.recommendations('{"status":"nonsense"}')


def test_a_refused_destructive_apply_leaves_the_rec_dismissable(tmp_path):
    # The fused approve+apply recorded the approval FIRST, then hit the
    # destructive gate — stranding the rec in `approved`, which has no exit but
    # `applied` or `expired` (`approved -> rejected` is not a legal transition).
    # The reviewer could then neither apply nor dismiss it.
    m, h = _a_destructive_rec(tmp_path)

    with pytest.raises(ValueError, match="LOP-E023"):
        m.apply_recommendation(h, because="looks right to me")

    assert [r["status"] for r in json.loads(m.recommendations('{"status":"all"}'))] == ["pending"], \
        "a refused apply must not have recorded the approval"
    # The whole point: it can still be dismissed.
    assert json.loads(m.dismiss_recommendation(h, "under legal hold"))["status"] == "rejected"


def test_approve_without_apply_enables_a_two_person_flow(tmp_path):
    # The bindings only had the fused call, so a supervising agent could not
    # approve for a human to apply later.
    m, h = _a_destructive_rec(tmp_path)
    assert json.loads(m.approve_recommendation(h, "evidence checks out"))["status"] == "approved"
    assert [r["status"] for r in json.loads(m.recommendations('{"status":"all"}'))] == ["approved"]


def test_scoped_actors_enforce_separation_of_duties(tmp_path):
    # ScopeSet::all() was hardcoded, so the write != review != apply gate could
    # not be enforced from Python even though the engine implements it.
    m, h = _a_destructive_rec(tmp_path)

    with pytest.raises(ValueError, match="LOP-E022"):
        m.apply_recommendation(h, because="no apply scope", scopes="review")
    with pytest.raises(ValueError, match="LOP-E022"):
        m.approve_recommendation(h, "no review scope", scopes="apply")
    with pytest.raises(ValueError, match="unknown scope"):
        m.approve_recommendation(h, "typo", scopes="reviewer")

    # A review-scoped actor can approve, and the rec is still not applied.
    m.approve_recommendation(h, "evidence checks out", scopes="review")
    assert [r["status"] for r in json.loads(m.recommendations('{"status":"all"}'))] == ["approved"]


def test_loop_health_reports_the_loop_state(tmp_path):
    m, _h = _a_destructive_rec(tmp_path)
    health = json.loads(m.loop_health())
    assert health["pending"] == 1
    assert health["applied"] == 0
    assert health["last_run_ms"] is not None
    assert health["stale"] is False


def test_per_analyzer_config_round_trips(tmp_path):
    m, _h = _a_destructive_rec(tmp_path)
    roster = json.loads(m.loop_analyzers())
    # The roster is how a caller discovers the id — which carries a version
    # suffix (`loop.staleness/1`), so guessing it is not viable.
    ids = [a["id"] for a in roster]
    assert "loop.staleness/1" in ids, ids

    m.set_analyzer_config("loop.staleness/1", enabled=False)
    assert {a["id"]: a["enabled"] for a in json.loads(m.loop_analyzers())}["loop.staleness/1"] is False
    m.set_analyzer_config("loop.staleness/1", enabled=True)
    assert {a["id"]: a["enabled"] for a in json.loads(m.loop_analyzers())}["loop.staleness/1"] is True


def test_subject_report_mirrors_erasure(tmp_path):
    # The DSAR read (GDPR Art. 15/20): the erasure selector in show-me mode.
    m = make_db(tmp_path)
    m.add_fact("pat", "prefers", "tea")
    m.add_fact("pat#visit1", "note", "arrived late")
    m.add_fact("mary", "prefers", "juice")

    report = json.loads(m.subject_report("pat"))
    assert len(report["grains"]) == 2  # exact + partition key, never mary
    assert "pat#visit1" in report["identity_names"]
    subjects = {g["fields"]["subject"] for g in report["grains"]}
    assert "mary" not in subjects
    assert all(len(g["hash"]) == HEX64 for g in report["grains"])

    # The bundle is the Art. 20 portability artifact.
    bundle = str(tmp_path / "pat.mgb")
    stats = json.loads(m.subject_bundle(bundle, "pat"))
    assert stats["ops"] == 2

    # Erasure removes exactly what the report showed.
    erased = json.loads(m.forget_subject("pat"))
    assert erased["grains_erased"] == len(report["grains"])
    assert json.loads(m.subject_report("pat"))["grains"] == []


# --------------------------------------------------------------------------
# text anonymization (docs/anonymization-proposal.md P0)
# --------------------------------------------------------------------------

def test_anonymize_round_trip(tmp_path):
    m = make_db(tmp_path)
    out = json.loads(m.anonymize_text("mail a@b.co, pin number is 1462"))
    assert out["text"] == "mail [EMAIL_1], pin number is [PIN_1]"
    assert out["mapping"] == {"[EMAIL_1]": "a@b.co", "[PIN_1]": "1462"}
    assert len(out["mapping_id"]) == 16
    assert out["replaced"] == 2

    back = json.loads(
        m.rehydrate_text(
            "sent to [EMAIL_1] after pin [PIN_1]", json.dumps(out["mapping"])
        )
    )
    assert back["text"] == "sent to a@b.co after pin 1462"
    assert back["replaced"] == 2
    assert back["unmatched"] == []


def test_scan_respects_policy_and_fails_closed(tmp_path):
    m = make_db(tmp_path)
    dets = json.loads(m.scan_text("host 10.0.0.1 down"))["detections"]
    assert dets[0]["category"] == "ipv4"

    # allow-listed category drops out of the scan results
    policy = json.dumps({"categories": {"ipv4": "allow"}})
    assert json.loads(m.scan_text("host 10.0.0.1 down", policy))["detections"] == []

    # an unreadable policy is a hard VAL error, not a silent no-policy (D3)
    with pytest.raises(ValueError, match="VAL-E001"):
        m.scan_text("x", json.dumps({"surprise": 1}))
    with pytest.raises(ValueError, match="VAL-E001"):
        m.rehydrate_text("x", '{"bad": 1}')


def test_anon_policy_egress_round_trip(tmp_path):
    m = make_db(tmp_path)
    m.add("fact", json.dumps({
        "subject": "caller:john", "relation": "prefers",
        "object": "call me at +1 415 555 0142",
    }))

    m.set_anon_policy("caller", json.dumps({"mode": "egress"}))
    policies = json.loads(m.anon_policies())
    assert policies[0]["ns"] == "caller"
    assert policies[0]["policy"]["mode"] == "egress"

    hits = json.loads(m.recall("caller:john"))
    flat = json.dumps(hits)
    assert "caller:john" not in flat and "415 555" not in flat, flat
    assert "[PERSON_1]" in flat, flat

    # In-process custody: the mapping is available for rehydration.
    mappings = json.loads(m.anon_mappings())
    assert any("caller:john" in row["mapping"].values() for row in mappings)
    mapping = mappings[0]["mapping"]
    back = json.loads(m.rehydrate_text("ok [PERSON_1]", json.dumps(mapping)))
    assert back["text"] == "ok caller:john"

    m.clear_anon_policy("caller")
    assert json.loads(m.anon_policies()) == []
    flat = json.dumps(json.loads(m.recall("caller:john")))
    assert "caller:john" in flat, "cleared policy must restore raw reads"

    # The floor is a host cap, no declared policy needed.
    m.set_anonymize_egress_floor(True)
    flat = json.dumps(json.loads(m.recall("caller:john")))
    assert "caller:john" not in flat, flat


# --------------------------------------------------------------------------
# namespace prefix scoping ("org.*")
# --------------------------------------------------------------------------

def test_recall_namespace_prefix_scope(tmp_path):
    m = make_db(tmp_path)
    m.add_fact("acme", "hq", "root-value", ns="org")
    m.add_fact("acme", "hq", "sales-value", ns="org.sales")
    m.add_fact("acme", "hq", "lookalike-value", ns="organization")

    rows = json.loads(m.recall("acme", ns="org.*"))
    objects = sorted(r["fields"]["object"] for r in rows)
    # Parent + descendants — and the lookalike must be EXCLUDED.
    assert objects == ["root-value", "sales-value"]


def test_wildcard_namespace_refusals(tmp_path):
    m = make_db(tmp_path)
    # Writes never accept patterns ('*' is reserved for read scoping).
    with pytest.raises(ValueError, match="VAL-E001"):
        m.add_fact("s", "r", "o", ns="org.*")
    # A malformed pattern errors instead of silently matching nothing.
    with pytest.raises(ValueError, match="VAL-E001"):
        m.recall("acme", ns="org*")


# --------------------------------------------------------------------------
# CAS blobs (issue #27) — bytes in, bytes out
# --------------------------------------------------------------------------

def test_blob_roundtrip_and_idempotence(tmp_path):
    m = make_db(tmp_path)
    payload = b"invoice attachment \x00\x01\xff bytes"

    uri = m.put_blob(payload)
    assert uri.startswith("cas://sha256:")
    # Content addressing dedupes by construction: same bytes, same address.
    assert m.put_blob(payload) == uri
    assert m.put_blob(b"other") != uri

    got = m.get_blob(uri)
    assert isinstance(got, bytes), "blobs come back as bytes, not a list of ints"
    assert got == payload


def test_blob_bad_uris_raise(tmp_path):
    m = make_db(tmp_path)
    with pytest.raises(ValueError, match="VAL-E001"):
        m.get_blob("not-a-cas-uri")
    # Short/truncated addresses must error, never panic on a byte slice.
    with pytest.raises(ValueError, match="VAL-E001"):
        m.get_blob("cas://sha256:abc")
    with pytest.raises(ValueError, match="STO-E001"):
        m.get_blob("cas://sha256:" + "a" * 64)


def test_read_blob_offline_needs_no_open_handle(tmp_path):
    """A tool subprocess cannot open the memory its run holds, so an
    attachment must be readable with the database closed."""
    path = str(tmp_path / "blob.db")
    db = areev.Areev(path)
    uri = db.put_blob(b"invoice bytes")
    del db  # release the handle

    assert areev.read_blob_offline(path, uri) == b"invoice bytes"

    for bad in ("http://x", "cas://sha256:abc", "cas://sha256:" + "z" * 64):
        with pytest.raises(ValueError):
            areev.read_blob_offline(path, bad)

    with pytest.raises(ValueError):
        areev.read_blob_offline(path, "cas://sha256:" + "a" * 64)


def test_read_blob_offline_works_while_a_handle_is_open(tmp_path):
    """It takes no lock, so it reads during a run, not only after one."""
    path = str(tmp_path / "live.db")
    db = areev.Areev(path)
    uri = db.put_blob(b"during a run")
    assert areev.read_blob_offline(path, uri) == b"during a run"
    assert db.get_blob(uri) == b"during a run"


def test_thread_tail_reads_one_conversation_past_the_default_page(tmp_path):
    """The read a chat/voice agent makes every turn (#49).

    Backed by ``idx_thread(ns, session, seq)``, so it is bounded by turns of
    THIS session rather than rows of the namespace — before the pushdown a
    conversation buried under newer traffic came back empty.
    """
    db = areev.Areev(str(tmp_path / "thread.db"), ns="caller")
    wanted = [
        {"type": "event", "fields": {"content": f"call-7 turn {i}", "session_id": "call-7"}}
        for i in range(5)
    ]
    db.add_batch(json.dumps(wanted))
    noise = [
        {"type": "event", "fields": {"content": f"noise {i}", "session_id": f"other-{i}"}}
        for i in range(200)
    ]
    db.add_batch(json.dumps(noise))

    tail = json.loads(db.thread_tail("call-7", 20))
    assert tail["session"] == "call-7"
    assert len(tail["grains"]) == 5, "the whole session, not the part inside a page"
    # Transcript order: oldest first.
    assert tail["grains"][0]["fields"]["content"] == "call-7 turn 0"
    assert tail["grains"][4]["fields"]["content"] == "call-7 turn 4"

    # The CAL spelling of the same read pushes down too.
    via_cal = json.loads(db.cal('RECALL events WHERE session_id = "call-7"'))
    assert len(via_cal["grains"]) == 5


def test_cal_prepare_validates_and_warms_a_statement(tmp_path):
    """`cal_prepare` turns a bad statement into a startup error rather than a
    first-turn error, and keeps the plan (#44)."""
    db = areev.Areev(str(tmp_path / "prep.db"), ns="caller")
    db.add_fact("amy", "likes", "rust")

    first = json.loads(db.cal_prepare("RECALL facts RECENT 5"))
    assert first["statement"] == "RECALL facts RECENT 5"
    assert first["cached"] == 1

    # One executor lives on the handle, so plans survive between calls.
    second = json.loads(db.cal_prepare("RECALL facts RECENT 9"))
    assert second["cached"] == 2

    with pytest.raises(ValueError):
        db.cal_prepare("RECALL facts FORMAT []")

    assert len(json.loads(db.cal("RECALL facts RECENT 5"))["grains"]) == 1
