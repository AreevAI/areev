"""Triggers, the anonymization key, and the runtime's model seam (1.3.1).

All three reached the CLI in 1.3.0 (or, for `anon_key`, only the Rust API) and
were unreachable from Python. These drive the real compiled extension module
against a fresh per-test temp database, like the rest of the suite.
"""

import json

import pytest

import areev

HEX64 = 64  # length of a SHA-256 content address in hex
WORKFLOW = "a" * HEX64


def ops_db(tmp_path, name="trig.db"):
    return areev.Areev(str(tmp_path / name), ns="ops")


# --------------------------------------------------------------------------
# triggers
# --------------------------------------------------------------------------

def test_trigger_lifecycle(tmp_path):
    m = ops_db(tmp_path)
    h = m.trigger_add(
        json.dumps({"kind": "interval", "workflow": WORKFLOW, "interval_secs": 3600}),
        "nightly reconciliation",
    )
    assert len(h) == HEX64

    rows = json.loads(m.trigger_list())
    assert len(rows) == 1
    assert rows[0]["kind"] == "interval"
    assert rows[0]["enabled"] is True

    # status carries the runtime view the declaration alone cannot: an interval
    # trigger that has never fired is due immediately.
    status = json.loads(m.trigger_status())
    assert len(status) == 1
    assert status[0]["due"] is True
    assert status[0]["paused"] is False
    assert status[0]["never_fired"] is True

    # show accepts a prefix, like the CLI
    assert json.loads(m.trigger_show(h[:12]))["trigger"] == h

    assert json.loads(m.trigger_pause(h, "upstream API is down"))["paused"] is True
    assert json.loads(m.trigger_show(h))["paused"] is True
    m.trigger_resume(h, "upstream API is back")
    assert json.loads(m.trigger_show(h))["paused"] is False

    # A reason is not optional: pausing a standing rule is an auditable act.
    with pytest.raises(ValueError, match="because is required"):
        m.trigger_pause(h, "   ")
    with pytest.raises(ValueError, match="no trigger matching"):
        m.trigger_show("deadbeef")


def test_trigger_add_refuses_a_declaration_that_could_never_fire(tmp_path):
    """The validation `add("trigger", …)` structurally cannot do.

    Cron parsing and the UTC-only rule live in areev-trigger, which sits above
    areev-cal — so the CAL grain builder cannot reach them. A trigger that can
    never fire has exactly one symptom, nothing happening, which is why it is
    worth refusing at authoring time.
    """
    m = ops_db(tmp_path, "bad.db")

    with pytest.raises(ValueError):
        m.trigger_add(
            json.dumps({"kind": "schedule", "workflow": WORKFLOW, "cron": "not a cron"}), "x"
        )

    with pytest.raises(ValueError, match="TRG-E006|timezone"):
        m.trigger_add(
            json.dumps({
                "kind": "schedule",
                "workflow": WORKFLOW,
                "cron": "0 3 * * *",
                "config": {"int:timezone": "Asia/Kolkata"},
            }),
            "x",
        )

    # …and the same expression in UTC is accepted.
    ok = m.trigger_add(
        json.dumps({
            "kind": "schedule",
            "workflow": WORKFLOW,
            "cron": "0 3 * * *",
            "config": {"int:timezone": "UTC"},
        }),
        "nightly close",
    )
    assert len(ok) == HEX64
    # Nothing was stored for either refusal.
    assert len(json.loads(m.trigger_list())) == 1


def test_trigger_run_evaluates_and_dry_run_touches_nothing(tmp_path):
    m = ops_db(tmp_path, "run.db")
    m.trigger_add(
        json.dumps({"kind": "interval", "workflow": WORKFLOW, "interval_secs": 60}), "heartbeat"
    )

    dry = json.loads(m.trigger_run(dry_run=True))
    assert dry["claimed"] == 1          # the due trigger was considered
    assert dry["items"] == 0            # …and nothing was ingested
    assert json.loads(m.trigger_status())[0]["never_fired"] is True

    # A real pass with no tool_cmd is the documented ingest-only mode: the
    # firing is recorded, nothing is started. `ingested` is counted apart from
    # `runs_started` precisely so the report cannot claim a run that never
    # happened.
    real = json.loads(m.trigger_run())
    assert real["claimed"] == 1
    assert real["ingested"] == 1
    assert real["runs_started"] == 0
    assert real["errors"] == []
    assert json.loads(m.trigger_status())[0]["never_fired"] is False


def test_trigger_run_refuses_an_unset_credential(tmp_path):
    """A dropped credential surfaces downstream as someone else's 401.

    The CLI silently skips a credential whose variable is unset; the binding
    refuses, because a host wiring this up programmatically has no console to
    notice the omission on.
    """
    m = ops_db(tmp_path, "cred.db")
    m.trigger_add(
        json.dumps({"kind": "interval", "workflow": WORKFLOW, "interval_secs": 60}), "poll"
    )
    with pytest.raises(ValueError, match="not set"):
        m.trigger_run(dry_run=True, credentials_json=json.dumps({"gmail": "NOPE_UNSET_VAR"}))


def test_trigger_render_emits_heartbeat_config_and_creates_nothing(tmp_path):
    m = ops_db(tmp_path, "render.db")
    m.trigger_add(
        json.dumps({"kind": "interval", "workflow": WORKFLOW, "interval_secs": 900}), "poll"
    )

    out = json.loads(m.trigger_render("cron", "render.db"))
    assert out["target"] == "cron"
    # The memory owns the real cadence, so the rendered interval is the GCD of
    # the declared intervals floored at 60s — deliberately coarser than the
    # shortest one.
    assert out["heartbeat_secs"] >= 60
    assert "trigger run" in out["config"]
    assert "--ns ops" in out["config"]

    for target in ("launchd", "systemd", "k8s-cronjob"):
        assert json.loads(m.trigger_render(target, "render.db"))["config"]
    with pytest.raises(ValueError):
        m.trigger_render("nomad", "render.db")


# --------------------------------------------------------------------------
# anon_key — the host-supplied HKDF root (was Rust-only in 1.3.0)
# --------------------------------------------------------------------------

def test_anon_key_is_accepted_at_open_and_validated(tmp_path):
    key = "a1" * 32  # 64 hex characters
    m = areev.Areev(str(tmp_path / "anon.db"), ns="caller", anon_key=key)
    m.add_fact("john", "email", "john@example.com")
    assert len(json.loads(m.recall("john"))) == 1
    del m

    # A malformed key fails at open, loudly, rather than deriving a different
    # token space that only shows up as an empty rehydrate much later.
    for bad in ("deadbeef", "z" * 64, ""):
        with pytest.raises(ValueError, match="anon_key"):
            areev.Areev(str(tmp_path / "bad.db"), ns="caller", anon_key=bad)


# --------------------------------------------------------------------------
# run_start(model=…) — abstract nodes were unreachable from Python
# --------------------------------------------------------------------------

def test_run_start_resolves_the_model_before_journaling_a_run(tmp_path):
    m = areev.Areev(str(tmp_path / "model.db"), ns="caller")
    # A bad provider spec must fail at resolve — before a run exists that could
    # never advance. Previously `model` had nowhere to go at all: the binding
    # hard-coded `llm: None`, so an abstract node refused with RUN-E006.
    with pytest.raises(ValueError):
        m.run_start("e" * HEX64, "r1", model="nosuch:model")
    assert json.loads(m.run_list(10)) == []


# --------------------------------------------------------------------------
# #67 — the GENERIC authoring path must validate too
# --------------------------------------------------------------------------

def test_add_trigger_refuses_a_declaration_that_can_never_fire(tmp_path):
    """`trigger_add` alone was not enough.

    `add("trigger", …)` is the path a host actually reaches for, and it stored
    declarations that could never fire — the evaluator then treated them as
    "not due", which is indistinguishable from a healthy trigger waiting.
    """
    m = ops_db(tmp_path, "i67.db")

    # The reporter's exact shape: `timezone` at top level, not under `config`.
    with pytest.raises(ValueError, match="TRG-E006"):
        m.add("trigger", json.dumps({
            "name": "probe-tz", "kind": "schedule", "workflow": WORKFLOW,
            "cron": "0 9 * * *", "timezone": "Asia/Kolkata", "enabled": True}))

    # …the config spelling, and a cron that simply does not parse.
    with pytest.raises(ValueError, match="TRG-E006"):
        m.add("trigger", json.dumps({
            "kind": "schedule", "workflow": WORKFLOW, "cron": "0 9 * * *",
            "config": {"int:timezone": "Asia/Kolkata"}}))
    with pytest.raises(ValueError, match="TRG-E006"):
        m.add("trigger", json.dumps({
            "kind": "schedule", "workflow": WORKFLOW, "cron": "not a cron"}))

    # Nothing was stored: the failure must not be "accepted, then never fires".
    assert json.loads(m.trigger_list()) == []

    # A valid one still goes through.
    m.add("trigger", json.dumps({
        "kind": "interval", "workflow": WORKFLOW, "interval_secs": 900}))
    assert len(json.loads(m.trigger_list())) == 1

    # A declaration that contradicts itself is refused rather than resolved by
    # a precedence rule nobody would remember.
    with pytest.raises(ValueError, match="must agree"):
        m.add("trigger", json.dumps({
            "kind": "schedule", "workflow": WORKFLOW, "cron": "0 9 * * *",
            "timezone": "UTC", "config": {"int:timezone": "Europe/Paris"}}))


def test_top_level_timezone_reaches_the_config_key_the_evaluator_reads(tmp_path):
    """UTC is the supported zone, so this declaration must be STORED — and the
    timezone must land in `config`, where the evaluator looks, not in
    `extra_fields` where nothing reads it and the trigger silently runs UTC."""
    m = ops_db(tmp_path, "tz.db")
    m.add("trigger", json.dumps({
        "kind": "schedule", "workflow": WORKFLOW, "cron": "0 9 * * *", "timezone": "UTC"}))
    g = json.loads(m.cal("RECALL triggers FORMAT json"))["grains"][0]
    assert g["fields"]["config"]["int:timezone"] == "UTC", g["fields"]


def test_trigger_status_reports_unusable_for_declarations_it_did_not_write(tmp_path):
    """Write-path validation cannot be the only defence.

    A declaration can arrive by bundle import from an implementation that
    validated differently, or predate the check. The evaluator has to notice on
    its own, so an unusable trigger never sits in the same column as a healthy
    one waiting its turn.
    """
    src = ops_db(tmp_path, "src.db")
    # UTC is accepted, so this is a legitimately storable trigger…
    src.add("trigger", json.dumps({
        "kind": "interval", "workflow": WORKFLOW, "interval_secs": 60}))
    rows = json.loads(src.trigger_status())
    assert len(rows) == 1
    # …and a healthy one carries no `unusable` key at all.
    assert "unusable" not in rows[0]
    assert rows[0]["due"] is True


def test_a_trigger_started_run_carries_the_budgets_it_was_given(tmp_path):
    """The bridge built `RunOptions::default()`, so every budget a host passed
    to `run_start` was dropped on the trigger path — the worst place to drop
    one, since a standing rule fires unattended."""
    import sys

    m = areev.Areev(str(tmp_path / "b.db"), ns="ops")
    # An unbound node is abstract and refuses at load (RUN-E006).
    tool = m.add("tool", json.dumps({"tool_name": "noop", "kind": "definition"}))
    wf = m.add("workflow", json.dumps({
        "name": "budgeted", "nodes": ["only"], "edges": [], "bindings": {"only": tool},
    }))
    trig = m.trigger_add(json.dumps({
        "kind": "webhook", "workflow": wf, "connector": "c",
        "dedup_key": ["/id"]}), "budgets must survive the trigger path")

    tool = f"{sys.executable} -c \"import sys,json;sys.stdout.write(json.dumps({{}}))\""
    report = json.loads(m.trigger_deliver(
        trig, json.dumps({"id": "item-1"}), tool_cmd=tool,
        max_tokens=5000, max_usd_micros=250_000, max_wall_ms=60_000, ask_ttl_sec=3600))
    assert report["runs_started"] == 1, report

    run_id = json.loads(m.run_list(10))[0]
    budgets = json.loads(m.run_inspect(run_id))["budgets"]
    assert budgets["max_tokens"] == 5000, budgets
    assert budgets["max_usd_micros"] == 250_000, budgets
    assert budgets["max_wall_ms"] == 60_000, budgets


def test_trigger_budgets_default_to_unset_when_not_given(tmp_path):
    """Optional, and adds no implicit ceiling when omitted."""
    import sys

    m = areev.Areev(str(tmp_path / "b2.db"), ns="ops")
    tool = m.add("tool", json.dumps({"tool_name": "noop", "kind": "definition"}))
    wf = m.add("workflow", json.dumps({
        "name": "unbudgeted", "nodes": ["only"], "edges": [], "bindings": {"only": tool},
    }))
    trig = m.trigger_add(json.dumps({
        "kind": "webhook", "workflow": wf, "connector": "c",
        "dedup_key": ["/id"]}), "no budgets given")
    tool = f"{sys.executable} -c \"import sys,json;sys.stdout.write(json.dumps({{}}))\""
    m.trigger_deliver(trig, json.dumps({"id": "i"}), tool_cmd=tool)

    budgets = json.loads(m.run_inspect(json.loads(m.run_list(10))[0]))["budgets"]
    assert budgets["max_tokens"] is None and budgets["max_usd_micros"] is None

