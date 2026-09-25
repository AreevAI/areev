"""A handle bound with `principal=` fails closed on EVERY method (GHSA-rmrx-26f6-f97w).

`principal=` binds the handle to the file's grants for that principal and is
documented to fail closed (CAL 1.3 §9). Until 1.9.0 only CAL and five methods
honoured it: the typed methods reached the store through `AreevFacade::with_store`,
which applies no authorization, so a read-only principal could read any
namespace, write through `remember()`, and ERASE through the memory tool's
`delete`.

Two tests here, and the pair is the point:

* `test_every_public_method_is_covered` enumerates the binding's public surface
  and fails when a method is neither exercised below nor listed as exempt — so
  a method added later cannot quietly skip the check.
* `test_restricted_principal_is_refused` drives each covered method under a
  principal with NO grants and asserts `AUT-E001`.

`test_owner_is_unaffected` is the positive control: gating that also refused the
owner would pass the first two tests and break every existing caller.
"""

import inspect
import json
import os

import pytest

import areev


def seed(tmp_path):
    """A memory with content in `secret`, and a principal granted nothing."""
    path = str(tmp_path / "gate.db")
    owner = areev.Areev(path, ns="open", index_text=True)
    owner.cal(
        'ADD fact SET subject="deal:1" SET relation="stage" SET object="PUBLIC" '
        'SET namespace="open" BECAUSE "seed"'
    )
    owner.cal(
        'ADD fact SET subject="deal:2" SET relation="stage" SET object="CLASSIFIED" '
        'SET namespace="secret" BECAUSE "seed"'
    )
    # amy exists as a principal but holds no grant at all: fail-closed means
    # every namespace is closed to her, including the handle's own.
    owner.cal('GRANT read ON "unrelated" TO "user:amy"')
    del owner
    return path


def tiny_pack(tmp_path):
    """A one-grain pack seeding the ungranted `secret` namespace (#341)."""
    root = tmp_path / "pack"
    (root / "grains").mkdir(parents=True, exist_ok=True)
    (root / "grains" / "010-tool.json").write_text(json.dumps({
        "type": "tool", "kind": "definition", "tool_name": "lookup",
        "tool_description": "look something up", "created_at": 500}))
    (root / "pack.json").write_text(json.dumps({
        "pack": "gate", "version": "1.0.0", "namespace": "secret",
        "grains": ["grains/010-tool.json"]}))
    return str(root)


def calls(db, tmp_path):
    """Every gated method, as (name, thunk) against the ungranted `secret` ns."""
    blob = "cas://sha256:" + "00" * 32
    out = str(tmp_path / "out.mgb")
    pack = tiny_pack(tmp_path)
    return [
        # namespace-scoped reads
        ("recall", lambda: db.recall("deal:2", None, 5, "secret")),
        ("latest", lambda: db.latest("deal:2", "stage", "secret")),
        ("search", lambda: db.search("CLASSIFIED", None, None, 5, "secret")),
        ("history", lambda: db.history("deal:2", "stage", "secret")),
        ("related", lambda: db.related("deal:2", "stage", "out", 2, 10, "secret")),
        ("entity_at", lambda: db.entity_at("deal:2", "stage", 1, "world", "secret")),
        ("step_actions", lambda: db.step_actions("00" * 32, None, 10, "secret")),
        ("run_trace", lambda: db.run_trace("r1", 10, False, "secret")),
        ("run_grains", lambda: db.run_grains("r1", 0, 10, "secret")),
        ("runs_touching", lambda: db.runs_touching("00" * 32, 2, "secret")),
        ("thread_tail", lambda: db.thread_tail("s1", 5, "secret")),
        ("nearest", lambda: db.nearest("CLASSIFIED", None, None, 5, "secret")),
        ("nearest_vector", lambda: db.nearest_vector([0.1, 0.2], None, None, 5, "secret")),
        ("vector_recall_check", lambda: db.vector_recall_check("[[0.1,0.2]]", 5, "secret", None)),
        # namespace-scoped writes and destruction
        ("remember", lambda: db.remember("planted", None, "user:amy", "secret")),
        ("migrate", lambda: db.migrate("mem0", "[]", None, "secret")),
        ("telemetry_scrub_namespace", lambda: db.telemetry_scrub_namespace("secret")),
        ("set_anon_policy", lambda: db.set_anon_policy("secret", '{"mode":"off"}')),
        ("clear_anon_policy", lambda: db.clear_anon_policy("secret")),
        ("forget_subject", lambda: db.forget_subject("deal:2", "secret", False)),
        ("subject_report", lambda: db.subject_report("deal:2", "secret")),
        ("subject_bundle", lambda: db.subject_bundle(out, "deal:2", "secret")),
        ("forget_older_than", lambda: db.forget_older_than(1, "secret", None)),
        # an agent pack installs under the handle's principal (#341)
        ("pack_install", lambda: db.pack_install(pack)),
        # the memory tool: one method, three verbs
        ("memory_tool", lambda: db.memory_tool(
            json.dumps({"command": "view", "path": "/memories"}), "secret")),
        ("memory_tool_create", lambda: db.memory_tool(
            json.dumps({"command": "create", "path": "/memories/x.md", "file_text": "x"}),
            "secret")),
        ("memory_tool_delete", lambda: db.memory_tool(
            json.dumps({"command": "delete", "path": "/memories/x.md"}), "secret")),
        # memory-wide
        ("stats", lambda: db.stats()),
        ("verify", lambda: db.verify()),
        ("verify_attestations", lambda: db.verify_attestations()),
        ("put_blob", lambda: db.put_blob(b"x")),
        ("get_blob", lambda: db.get_blob(blob)),
        ("bundle", lambda: db.bundle(out, 0)),
        ("import_bundle", lambda: db.import_bundle(out)),
        ("reindex_text", lambda: db.reindex_text()),
        ("reindex_links", lambda: db.reindex_links()),
        ("signing_key", lambda: db.signing_key()),
        ("set_signing_key", lambda: db.set_signing_key("11" * 32)),
        ("set_trusted_authors", lambda: db.set_trusted_authors('{"keys":{},"policy":"off"}')),
        ("set_attest_policy", lambda: db.set_attest_policy("off")),
        ("attest", lambda: db.attest("00" * 32)),
        ("attest_all", lambda: db.attest_all(None)),
        ("add_embedding", lambda: db.add_embedding("00" * 32, [0.1, 0.2])),
        ("add_embeddings", lambda: db.add_embeddings("[]")),
        ("ensure_vector_index", lambda: db.ensure_vector_index(16, 64, 32)),
        ("drop_vector_index", lambda: db.drop_vector_index()),
        ("set_embedder_command", lambda: db.set_embedder_command("cat", None)),
        # LOWERING the floor is refused; raising it only strengthens
        # protection and needs no grant (#345, EXEMPT below as the getter).
        ("set_anonymize_egress_floor", lambda: db.set_anonymize_egress_floor(False)),
        ("set_anonymizer_command", lambda: db.set_anonymizer_command("cat")),
        # A decision backend can be a subprocess and receives memory text as
        # its state — host config, admin on "*" like the embedder.
        ("set_decider", lambda: db.set_decider(cmd="cat")),
        ("set_reranker_command", lambda: db.set_reranker_command("cat")),
        ("set_trigger_paused", lambda: db.trigger_pause("abc", "because")),
    ]


# Public methods that legitimately need no namespace check. Each is here with
# the reason; the coverage test refuses an unexplained addition.
EXEMPT = {
    # --- no grain data crosses these -------------------------------------
    "open_warnings": "host diagnostics about this open",
    "set_run_id": "ambient telemetry tag; reads nothing",
    "declared_embedding": "capability metadata (model + dim)",
    "vector_index": "capability metadata (ANN index name)",
    "authz_epoch": "a change detector for policy; discloses no policy",
    "close": "releases the handle",
    "cal_prepare": "parses a statement; touches no store",
    "spec": "static capability description",
    "anonymize_egress_floor": "reports a per-process host cap, no grain data (#345)",
    # --- already gated inside the facade / CAL executor -------------------
    "cal": "the CAL executor gates every statement it runs",
    "add": "facade cal_add -> check_verb(Write, ns)",
    "add_fact": "facade cal_add -> check_verb(Write, ns)",
    "add_batch": "facade cal_add_batch -> check_verb(Write, ns)",
    "supersede": "facade cal_supersede -> check_verb(Supersede, ns)",
    "forget": "facade cal_delete -> check_verb(Delete, ns)",
    "validated_cal_add": "facade cal_add -> check_verb(Write, ns)",
    "record_tool_call": "facade record_tool_call -> check_verb(Write, harness ns)",
    "record_run_manifest": "facade -> check_verb(Write, harness ns)",
    "record_corpus_export": "facade authorize_corpus_export",
    "record_adapter": "facade -> check_verb(Write, harness ns)",
    "reveal_tokens": "facade reveal_tokens -> check_verb(Admin, ns)",
    "scan_text": "pure text analysis, no store read",
    "anonymize_text": "pure text transform, no store read",
    "rehydrate_text": "pure text transform over a caller-supplied mapping",
    "set_recall_deadline_ms": "a per-handle latency bound; reads and writes nothing",
    "recall_deadline_ms": "reports the per-handle latency bound",
    "decide": "judges caller-supplied state, no store read; the backend is installed "
              "only via admin-gated set_decider; egress policy consulted fail-safe",
    # --- filtered per row rather than refused outright --------------------
    "changes_since": "scope checked; rows filtered to readable namespaces",
    "provenance": "children filtered to readable namespaces",
    "anon_policies": "rows filtered to readable namespaces",
    "anon_mappings": "rows filtered to readable namespaces",
    # --- run / loop / trigger families carry their own verbs --------------
    "set_embedder": "installs a Python callable; covered by set_embedder_command",
}


def public_methods():
    return {
        n
        for n, _ in inspect.getmembers(areev.Areev)
        if not n.startswith("_")
    }


def test_every_public_method_is_covered(tmp_path):
    """No method may be added without deciding what it does under a principal."""
    covered = {n.split("_create")[0].split("_delete")[0] for n, _ in calls(None, tmp_path)}
    covered |= {"trigger_pause"}  # set_trigger_paused is reached as trigger_pause
    missing = public_methods() - covered - set(EXEMPT)
    # The run/loop/trigger families gate through their own verbs (RunExecute,
    # LoopRun, …) and are tested with those; they are named here rather than in
    # EXEMPT so this stays a list of FAMILIES, not a per-method escape hatch.
    families = {
        n
        for n in missing
        if n.startswith(("run_", "loop_", "trigger_", "recommend", "apply_", "approve_",
                         "dismiss_", "rollback_", "set_analyzer", "egress_", "runner",
                         "evaluator", "read_only_evaluator", "hook_", "pack_"))
    }
    missing -= families
    assert not missing, (
        "public binding methods with no decision about what they do under a "
        f"restricted principal: {sorted(missing)}\n"
        "Add each to the `calls()` list (if it must be refused) or to EXEMPT "
        "with the reason it needs no check."
    )


def test_restricted_principal_is_refused(tmp_path):
    """Every covered method refuses a principal holding no grant."""
    path = seed(tmp_path)
    amy = areev.Areev(path, ns="open", principal="user:amy")
    leaked = []
    for name, thunk in calls(amy, tmp_path):
        try:
            result = thunk()
        except Exception as exc:  # noqa: BLE001 — any refusal shape is fine
            if "AUT-E001" not in str(exc):
                # A different error means the call never reached the gate —
                # a bad signature here would hide a real hole.
                leaked.append(f"{name}: non-authz error {exc!r}")
            continue
        leaked.append(f"{name}: RETURNED {str(result)[:120]!r}")
    assert not leaked, (
        "a principal with no grants was not refused (GHSA-rmrx-26f6-f97w):\n  "
        + "\n  ".join(leaked)
    )


def test_no_classified_content_escapes(tmp_path):
    """The concrete leak from the advisory, stated as content rather than shape."""
    path = seed(tmp_path)
    amy = areev.Areev(path, ns="open", principal="user:amy")
    for name, thunk in calls(amy, tmp_path):
        try:
            assert "CLASSIFIED" not in str(thunk()), f"{name} disclosed a secret grain"
        except AssertionError:
            raise
        except Exception:  # noqa: BLE001 — refusals are the expected path
            pass


def test_owner_is_unaffected(tmp_path):
    """The positive control: gating must not refuse an unbound (owner) handle.

    Without this, a gate that refused everyone would pass the tests above and
    break every existing caller.
    """
    path = seed(tmp_path)
    owner = areev.Areev(path, ns="open", index_text=True)
    refused = []
    for name, thunk in calls(owner, tmp_path):
        try:
            thunk()
        except Exception as exc:  # noqa: BLE001
            if "AUT-E001" in str(exc):
                refused.append(f"{name}: {exc}")
    assert not refused, "owner session wrongly refused:\n  " + "\n  ".join(refused)


def test_granted_namespace_still_works(tmp_path):
    """Fail-closed must not mean fail-always: a grant is honoured."""
    path = seed(tmp_path)
    owner = areev.Areev(path, ns="open", index_text=True)
    owner.cal('GRANT read ON "open" TO "user:bob"')
    del owner

    bob = areev.Areev(path, ns="open", principal="user:bob")
    got = json.loads(bob.recall("deal:1", None, 5, "open"))
    assert got and got[0]["fields"]["object"] == "PUBLIC"
    with pytest.raises(ValueError, match="AUT-E001"):
        bob.recall("deal:2", None, 5, "secret")
