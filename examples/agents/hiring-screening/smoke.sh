#!/bin/sh
# One week of screening for one job requisition, end to end, with no
# credentials and no model key.
#
# Five applications arrive for REQ-4417. One cannot be read at all and the
# run FAILS rather than screening a blank. The other four are checked
# against the criteria the requisition published -- and every single one of
# them parks for a named recruiter, because this plan has no edge that
# reaches an outcome any other way.
#
# The property this example exists for: THE OVERSIGHT IS MEASURED, NOT
# ASSERTED. Step 8 prints the EU AI Act Article 14 report, and every field
# in it is read back out of the run journal the week's work already wrote:
# where a person can intervene, who was authorized to, what the runs were
# allowed to spend, and how fast the kill switch actually drained.
#
# What this example does NOT do: score candidates, rank them, or test
# anything for bias. It demonstrates oversight and record-keeping.
#
# Language-neutral: every implementation under python/ (and any stack added
# later) exposes the same agent subcommands, so ONE set of assertions proves
# them all. Run it through a wrapper:
#
#   python/smoke.sh
#
# The wrapper exports AGENT and AGENT_OUT. Exits non-zero on any drift.
set -eu
cd "$(dirname "$0")"

: "${AGENT:?run me through a language wrapper: python/smoke.sh}"
: "${AGENT_OUT:?}"
DECISIONS="$AGENT_OUT/decisions.jsonl"

# ONE DRIVER AT A TIME. Areev's single-writer guard is *process-wide* -- an
# in-process open-path registry that refuses a second handle inside one
# process (STO-E002). It cannot see a second OS PROCESS opening the same
# memory file, so two act scripts run against one `out/` will interleave and
# answer each other's asks: the queue empties under your feet and a run
# looks as though it reached an outcome with nobody involved. `mkdir` is the
# atomic POSIX primitive for keeping that from happening. The lock lives
# BESIDE `$AGENT_OUT`, because smoke.sh removes `$AGENT_OUT` wholesale.
if [ -z "${AGENT_LOCK_HELD:-}" ]; then
  LOCK="$AGENT_OUT.lock"
  if ! mkdir "$LOCK" 2>/dev/null; then
    echo "FAIL: another run is already driving $AGENT_OUT" >&2
    echo "      (lock $LOCK, held by pid $(cat "$LOCK/pid" 2>/dev/null || echo '?'))." >&2
    echo "      Wait for it to finish, point AGENT_OUT somewhere else, or" >&2
    echo "      remove the lock if it is stale: rmdir '$LOCK'" >&2
    exit 1
  fi
  echo $$ > "$LOCK/pid"
  trap 'rm -rf "$LOCK" 2>/dev/null || true' EXIT INT TERM
  AGENT_LOCK_HELD=1
  export AGENT_LOCK_HELD
fi

CLOSED="$AGENT_OUT/closed.jsonl"

say()  { printf '\n\033[1m%s\033[0m\n' "$*"; }
fail() { echo "FAIL: $*" >&2; exit 1; }
jget() { python3 -c 'import json,sys; d=json.load(sys.stdin)
for k in sys.argv[1:]:
    d = d[int(k)] if isinstance(d, list) else d.get(k)
print(d)' "$@"; }

rm -rf "$AGENT_OUT"

# -- 1. seed ---------------------------------------------------------------
say "1. seed: the plan, the GATE, the requisition's criteria, the grants"
SEEDED=$($AGENT seed)
WF=$(echo "$SEEDED" | jget workflow)
echo "   workflow   $WF"
echo "   requisition $(echo "$SEEDED" | jget requisition)"
echo "$WF" > "$AGENT_OUT/workflow.hash"

# The headline claim, asserted from the STORED PLAN rather than the seeder:
# nothing reaches an outcome except out of the client-gated node.
$AGENT plan > "$AGENT_OUT/plan.json"
python3 - "$AGENT_OUT/plan.json" <<'EOF' || fail "the plan has a path to an outcome that skips the recruiter"
import json, sys
p = json.load(open(sys.argv[1]))
assert p["client_gated"] == ["recruiter_review"], p["client_gated"]
outcomes = {"advance", "reject"}
into = [e for e in p["edges"] if e["dst"] in outcomes]
assert into, "the plan has no outcome edges at all"
assert all(e["src"] == "recruiter_review" for e in into), into
assert all("cond" in e for e in into), "an outcome edge fires unconditionally"
# And the check node itself must not be able to end the run.
assert not [e for e in p["edges"]
            if e["src"] == "check_criteria" and e["dst"] in outcomes]
print("   %d nodes, 1 client gate, %d outcome edges -- all out of recruiter_review"
      % (len(p["nodes"]), len(into)))
EOF

# -- 2. the cursor seeds ---------------------------------------------------
say "2. the first poll seeds the cursor and fires nothing"
STARTED=$($AGENT ingest | jget runs_started)
[ "$STARTED" = "0" ] || fail "first poll started $STARTED runs; it must seed only"
sleep 1.2

# -- 3. five applications --------------------------------------------------
say "3. five applications arrive; four are screened, one cannot be read"
STARTED=$($AGENT ingest | jget runs_started)
[ "$STARTED" = "5" ] || fail "expected 5 runs started, got $STARTED"

[ ! -f "$DECISIONS" ] \
  || fail "an application was advanced or rejected with no person involved"
PARKED=$($AGENT asks | python3 -c 'import json,sys; print(len(json.load(sys.stdin)))')
[ "$PARKED" = "4" ] || fail "expected 4 applications parked for a recruiter, got $PARKED"
FAILED=$($AGENT runs | python3 -c 'import json,sys
print(sum(1 for r in json.load(sys.stdin) if r["outcome"] == "failed"))')
[ "$FAILED" = "1" ] || fail "expected 1 unreadable application to fail loudly, got $FAILED"
echo "   4 parked for a person, 0 decided by the machine, 1 run failed"
echo "   loudly rather than screening a file it could not read"

# What the recruiter is actually shown: met / missed / NOT EVIDENCED, and no
# score anywhere. "Not evidenced" is the category that keeps a silent CV
# from becoming a silent rejection.
$AGENT asks > "$AGENT_OUT/queue.json"
python3 - "$AGENT_OUT/queue.json" <<'EOF' || fail "the review queue is not shaped as designed"
import json, sys
rows = {r["application_id"]: r for r in json.load(open(sys.argv[1]))}
assert set(rows) == {"APP-2001", "APP-2002", "APP-2003", "APP-2004"}, sorted(rows)
assert rows["APP-2002"]["criteria_missed"] == ["min_years_backend",
                                               "required_certification"], rows["APP-2002"]
assert rows["APP-2003"]["criteria_not_evidenced"] == ["work_authorisation"], rows["APP-2003"]
assert rows["APP-2003"]["criteria_missed"] == [], "a silent CV was scored as a miss"
for r in rows.values():
    assert "score" not in r and "rank" not in r and "decision" not in r, r
print("   the queue carries met / missed / not-evidenced -- and no score")
EOF

# -- 4. the desk cannot decide its own case --------------------------------
say "4. the desk advances a candidate itself -- refused, structurally"
if $AGENT decide fixtures/decisions/00-agent-self-approves.json >/dev/null 2>&1; then
  fail "the principal that started the run was allowed to decide it"
fi
echo "   RUN-E012: the responder may not be the triggering principal"
if $AGENT decide fixtures/decisions/06-unknown-verdict.json >/dev/null 2>&1; then
  fail "a verdict the plan has no edge for was accepted"
fi
echo "   and a verdict this plan has no edge for never reaches the runtime"

# -- 5. three decisions, each by a named recruiter --------------------------
say "5. mo advances Vessik and rejects Dalquist; ines advances Trevane"
$AGENT decide fixtures/decisions/01-vessik-advance.json  | jget outcome finished >/dev/null
$AGENT decide fixtures/decisions/02-dalquist-reject.json | jget outcome finished >/dev/null
$AGENT decide fixtures/decisions/03-trevane-advance.json | jget outcome finished >/dev/null
python3 - "$DECISIONS" <<'EOF' || fail "the decisions are not signed and reasoned"
import json, sys
rows = {r["application_id"]: r for r in map(json.loads, open(sys.argv[1]))}
assert set(rows) == {"APP-2001", "APP-2002", "APP-2003"}, sorted(rows)
assert rows["APP-2001"]["outcome"] == "advanced" and rows["APP-2001"]["decided_by"] == "user:mo"
assert rows["APP-2002"]["outcome"] == "rejected" and rows["APP-2002"]["decided_by"] == "user:mo"
assert rows["APP-2003"]["outcome"] == "advanced" and rows["APP-2003"]["decided_by"] == "user:ines"
for r in rows.values():
    assert len(r["because"] or "") > 40, r          # a reason, not a rubber stamp
assert "never a screen-out" in rows["APP-2003"]["because"], rows["APP-2003"]
EOF
echo "   2 advanced, 1 rejected -- every row carries a name and a reason"
echo "   ines advanced the CV that did not state work authorisation:"
echo "   a missing statement is a question, not a screen-out"

# -- 6. the kill switch ----------------------------------------------------
say "6. a candidate withdraws mid-review: the coordinator pulls the brake"
STOPPED=$($AGENT stop 1193f68ef774 \
  --because "the candidate withdrew the application before review; stop processing and record nothing about them")
echo "$STOPPED" | jget outcome finished | grep -q '^Canceled' \
  || fail "the run did not end canceled"
echo "$STOPPED" | grep -q 'user:coordinator' || fail "the canceling principal is not recorded"
grep -q 'APP-2004' "$DECISIONS" && fail "a canceled run still recorded a decision"
[ -f "$CLOSED" ] && grep -q 'APP-2004' "$CLOSED" && fail "a canceled run still ran downstream nodes"
echo "   canceled by user:coordinator -- who was never granted run.respond."
echo "   run.cancel is the LOWEST-privilege run verb, on purpose: a brake"
echo "   must never be blocked by missing privilege."

# -- 7. outcomes vs. humans, counted -------------------------------------
# The invariant the whole example rests on, asserted as arithmetic rather
# than as prose -- and read from the run journal, not from this desk's own
# ledger. If a future edit ever routes a candidate past the gate, this is
# the assertion that catches it, whatever the cause.
say "7. every outcome, matched to the human who reached it"
$AGENT gate-audit > "$AGENT_OUT/gate-audit.json"
python3 - "$AGENT_OUT/gate-audit.json" <<'EOF' || fail "a candidate reached an outcome without a human"
import json, sys
a = json.load(open(sys.argv[1]))
assert a["decisions"] == 3, a["decisions"]
assert a["decisions"] == a["human_reviews"], \
    "%d outcomes but %d human reviews" % (a["decisions"], a["human_reviews"])
assert a["decisions_with_no_human"] == [], a["decisions_with_no_human"]
assert a["self_reviewed"] == [], a["self_reviewed"]
assert sorted(a["reviewers"]) == ["user:ines", "user:mo"], a["reviewers"]
assert all(p.startswith("user:") for p in a["reviewers"]), a["reviewers"]
# The canceled candidate and the unreadable one reached no outcome at all.
by_app = {r["application_id"]: r for r in a["runs"]}
assert by_app["APP-2004"]["outcomes"] == [], by_app["APP-2004"]
assert by_app["APP-2005"]["outcomes"] == [], by_app["APP-2005"]
print("   %d outcomes, %d human reviews, 0 decided without a person"
      % (a["decisions"], a["human_reviews"]))
EOF

# -- 8. the record has not been edited after the fact ----------------------
say "8. journal-consistent replay across every run of the week"
$AGENT verify > "$AGENT_OUT/verify.json"
python3 - "$AGENT_OUT/verify.json" <<'EOF' || fail "a run does not replay to its stored journal"
import json, sys
v = json.load(open(sys.argv[1]))
assert v["all_verified"] is True, v
assert v["runs"] == 5, v
print("   %d runs re-derived from their journals, byte-identical checkpoints" % v["runs"])
EOF
echo "   including the failed one and the canceled one"

# -- 9. THE ARTIFACT -------------------------------------------------------
# EU AI Act Article 14, answered from the record. Nothing below was
# configured for the report's benefit; it is all read back out of what the
# week's runs already journaled.
say "9. the oversight report a compliance reviewer would ask for"
$AGENT oversight --plan | tee "$AGENT_OUT/oversight.json"

python3 - "$AGENT_OUT/oversight.json" <<'EOF' || fail "the oversight report does not evidence the controls"
import json, sys
r = json.load(open(sys.argv[1]))
assert set(r) == {"run_id", "plan_hash", "human_gates", "authorized_responders",
                  "budgets", "kill_switch"}, sorted(r)

g = r["human_gates"]
assert [n["node"] for n in g["client_gated_nodes"]] == ["recruiter_review"], g
assert g["every_client_ask_is_an_approval"] is True, g
assert "responder != triggering principal" in g["separation_of_duties"], g
assert g["ask_ttl_sec"] == 172800, g          # 48 hours to answer

who = r["authorized_responders"]["principals_granted_run_respond"]
assert sorted(who) == ["user:ines", "user:mo"], who
assert "user:coordinator" not in who, "the brake-puller must not be an approver"

b = r["budgets"]
assert b["max_tokens"] == 200000 and b["max_usd_micros"] == 1500000, b
assert b["max_wall_ms"] == 120000, b

k = r["kill_switch"]
assert k["measured_cancel_to_drain_ms"], "the kill switch was never measured"
assert all(ms >= 0 for ms in k["measured_cancel_to_drain_ms"]), k
EOF
echo
echo "   the gate, the approvers, the ceilings and the measured cancel-to-drain"
echo "   all came out of the journal -- not out of a policy document"

printf '\n\033[32mOK\033[0m -- 4 of 4 screened candidates went through a person, 2 advanced, 1 rejected, 1 canceled, 1 unreadable file refused.\n'
