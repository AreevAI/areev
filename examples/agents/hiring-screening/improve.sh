#!/bin/sh
# Week two of the same requisition.
#
# smoke.sh is the desk doing its job under governance. This is what comes
# after: (a) a recruiter's written reason from week one comes back to the
# NEXT reviewer facing the same criterion mismatch -- and that candidate
# still parks for a person, (b) an ATS integration starts delivering scans
# with no text layer and the runs fail loudly rather than rejecting anyone,
# (c) the loop finds that cluster in the desk's own record, and (d) a
# person signs off on what to do about it -- the engine having applied
# nothing on its own, refusing a decision with no reason, and refusing to
# record a second decision on a finding that already carries one.
#
# Then the Article 14 report again, over eleven runs and two recruiters.
# The whole point of MEASURING oversight rather than asserting it is that
# the measurement stays true after the desk changes.
#
# Same contract as smoke.sh: run it through a language wrapper that exports
# AGENT and AGENT_OUT. Exits non-zero on any drift.
set -eu
cd "$(dirname "$0")"

: "${AGENT:?run me through a language wrapper: python/improve.sh}"
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


say()  { printf '\n\033[1m%s\033[0m\n' "$*"; }
fail() { echo "FAIL: $*" >&2; exit 1; }
jget() { python3 -c 'import json,sys; d=json.load(sys.stdin)
for k in sys.argv[1:]:
    d = d[int(k)] if isinstance(d, list) else d.get(k)
print(d)' "$@"; }

# Week one has to have happened, and has to be the ONLY thing that has:
# this chapter reads week one's journals and asserts exact counts against
# them. So the precondition is checked, not assumed -- run this script on a
# fresh checkout, twice in a row, or against a half-finished state and it
# rebuilds week one first either way. (`smoke.sh` starts by removing
# `$AGENT_OUT`, so re-running it is a clean reset.)
week_one_only() {
  $AGENT runs 2>/dev/null | python3 -c 'import json, sys
try:
    rows = json.load(sys.stdin)
except Exception:
    sys.exit(1)
still_open = [r for r in rows if r["outcome"] == "open"]
sys.exit(0 if len(rows) == 5 and not still_open else 1)'
}
if [ ! -d "$AGENT_OUT" ] || ! week_one_only; then
  say "0. week one first"
  "$(dirname "$AGENT_OUT")/smoke.sh" >/dev/null
  echo "   ran smoke.sh -- 5 applications, 3 decided, 1 canceled, 1 unreadable"
fi

# -- 1. redelivery is a no-op ----------------------------------------------
say "1. another tick over the same queue starts nothing"
sleep 1.2
STARTED=$($AGENT ingest | jget runs_started)
[ "$STARTED" = "0" ] || fail "redelivered applications started $STARTED runs; dedup must hold"

# -- 2. week two -----------------------------------------------------------
say "2. week two: two more applicants, and four broken ATS exports"
sleep 1.2
STARTED=$(APPS_UPTO=11 $AGENT ingest | jget runs_started)
[ "$STARTED" = "6" ] || fail "expected 6 new runs, got $STARTED"

# -- 3. the payoff of week one's written reason ----------------------------
say "3. the same criterion mismatch arrives again -- and so does the reason"
$AGENT asks > "$AGENT_OUT/queue2.json"
python3 - "$AGENT_OUT/queue2.json" <<'EOF' || fail "the precedent did not reach the reviewer"
import json, sys
rows = {r["application_id"]: r for r in json.load(open(sys.argv[1]))}
wren = rows.get("APP-2006")
if not wren:
    raise SystemExit(
        "APP-2006 is not in the review queue (queue: %r).\n"
        "  It should be PARKED here. If it already reached an outcome, either\n"
        "  something routed a candidate past recruiter_review -- the invariant\n"
        "  this example exists for -- or a second process is driving the same\n"
        "  out/ and answered the ask (see the lock at the top of this script)."
        % sorted(rows))
assert wren["criteria_missed"] == ["min_years_backend", "required_certification"], wren
assert wren["precedent"], "the reviewer was shown no precedent"
assert "user:mo rejected APP-2002" in wren["precedent"], wren["precedent"]
assert "decision" not in wren, "the desk pre-decided the case"
print("   APP-2006 shows the reviewer what this desk decided last time:")
print("   %s..." % wren["precedent"][:92])
EOF
echo "   and it is STILL PARKED. The precedent informs a person; it never"
echo "   decides. There is no edge in this plan that would let it."

# -- 4. a second recruiter decides, consistently and by name ---------------
say "4. ines applies the same criteria to APP-2006; mo advances APP-2011"
$AGENT decide fixtures/decisions/04-wren-reject.json   | jget outcome finished >/dev/null
$AGENT decide fixtures/decisions/05-ebbin-advance.json | jget outcome finished >/dev/null
python3 - "$DECISIONS" <<'EOF' || fail "the week-two decisions are not signed and reasoned"
import json, sys
rows = {r["application_id"]: r for r in map(json.loads, open(sys.argv[1]))}
assert rows["APP-2006"]["outcome"] == "rejected", rows["APP-2006"]
assert rows["APP-2006"]["decided_by"] == "user:ines", rows["APP-2006"]
assert "APP-2002" in rows["APP-2006"]["because"], \
    "the reviewer's reason does not cite the precedent it was shown"
assert rows["APP-2011"]["outcome"] == "advanced", rows["APP-2011"]
EOF
echo "   a different recruiter, the same published criteria, and a reason"
echo "   that cites the precedent instead of quietly inheriting it"

# -- 5. the broken exports fail loudly -------------------------------------
say "5. five applications so far have arrived as scans with no text layer"
FAILED=$($AGENT runs | python3 -c 'import json,sys
print(sum(1 for r in json.load(sys.stdin) if r["outcome"] == "failed"))')
[ "$FAILED" = "5" ] || fail "the unreadable applications did not fail loudly ($FAILED failures)"
python3 - "$DECISIONS" <<'EOF' || fail "an unreadable application produced a decision"
import json, sys
rows = {json.loads(l)["application_id"] for l in open(sys.argv[1])}
for scanned in ("APP-2005", "APP-2007", "APP-2008", "APP-2009", "APP-2010"):
    assert scanned not in rows, "%s was decided despite being unreadable" % scanned
EOF
echo "   $FAILED runs failed rather than screening a file nobody could read."
echo "   Not one of them became a rejection -- which is the whole point:"
echo "   a candidate must never lose the process to our parser."

# -- 6. the desk briefs itself ---------------------------------------------
say "6. the desk briefs itself out of its own memory"
BRIEF=$($AGENT brief)
echo "$BRIEF" | grep -q "hiring-screening" || fail "the briefing does not name the plan"
echo "$BRIEF" | grep -q "run.respond" || fail "the briefing does not carry the grants"
echo "   plan, gate, criteria, grants -- one saved query that ships in the file"

# -- 7. the loop reads the record ------------------------------------------
say "7. areev loop: deterministic analyzers over the desk's own journals"
$AGENT improve > "$AGENT_OUT/loop.json"
REC=$(python3 - "$AGENT_OUT/loop.json" <<'EOF'
import json, sys
pending = json.load(open(sys.argv[1]))["pending"]
hit = [r for r in pending if r["analyzer"].startswith("loop.tool_failure")
       and "parse_application" in (r["summary"] or "")]
assert hit, "the loop did not cluster the parse failures: %r" % pending
sys.stderr.write("   %s\n" % hit[0]["summary"])
print(hit[0]["hash"])
EOF
) || fail "the loop proposed nothing about the unreadable applications"
BEFORE=$(python3 -c 'import json,sys
print(" ".join(sorted(r["hash"] for r in json.load(open(sys.argv[1]))["pending"])))' \
  "$AGENT_OUT/loop.json")

# -- 8. what the loop is NOT allowed to do ---------------------------------
say "8. the finding is advisory, and a decision needs a reason"
python3 - "$AGENT_OUT/loop.json" <<'EOF' || fail "the loop changed something on its own"
import json, sys
loop = json.load(open(sys.argv[1]))["loop"]
assert loop["proposed"] >= 1, loop
assert loop["auto_applied"] == 0, "the loop applied its own finding: %r" % loop
print("   the pass proposed %d and applied 0. This analyzer's manifest is"
      % loop["proposed"])
print("   Never -- its signature comes from tool output, so it can only ever")
print("   be advisory -- and the bindings grant auto-apply through a host")
print("   policy file this desk deliberately does not ship.")
EOF
if $AGENT govern "$REC" approve --because "" --as user:ines >/dev/null 2>&1; then
  fail "a decision was recorded with a blank reason"
fi
echo "   LOP-E011: a blank BECAUSE is refused by the engine, not by convention"

# -- 9. a person decides, and signs it -------------------------------------
say "9. a recruiter decides, and signs it"
$AGENT govern "$REC" approve \
  --because "the ats bulk export is uploading page images; route that channel to manual transcription and keep failing the run rather than ever letting an unreadable file turn into a rejection" \
  --as user:ines >/dev/null
echo "   approved by user:ines"
if $AGENT govern "$REC" approve \
     --because "signing it off a second time" --as user:mo >/dev/null 2>&1; then
  fail "the same recommendation was approved twice"
fi
echo "   LOP-E020: and it cannot be approved again -- the review lifecycle is"
echo "   a state machine, so one finding carries exactly one signed decision"

# -- 10. it does not nag ---------------------------------------------------
say "10. run the loop again"
$AGENT improve > "$AGENT_OUT/loop2.json"
python3 - "$AGENT_OUT/loop2.json" "$REC" "$BEFORE" <<'EOF' || fail "the same evidence became a new finding"
import json, sys
after = {r["hash"] for r in json.load(open(sys.argv[1]))["pending"]}
approved, before = sys.argv[2], set(sys.argv[3].split())
assert approved not in after, "an approved recommendation is pending again"
assert not (after - before), "the same evidence became a new recommendation: %r" % (after - before)
print("   deduped -- the same evidence does not come back as a new finding")
EOF

# -- 11. the record still holds, and so does the oversight -----------------
say "11. eleven runs later: the record still replays, the gate is still there"
$AGENT verify > "$AGENT_OUT/verify2.json"
python3 - "$AGENT_OUT/verify2.json" <<'EOF' || fail "a run does not replay to its stored journal"
import json, sys
v = json.load(open(sys.argv[1]))
assert v["all_verified"] is True, v
assert v["runs"] == 11, v
print("   %d runs re-derived from their journals, byte-identical checkpoints" % v["runs"])
EOF

$AGENT gate-audit > "$AGENT_OUT/gate-audit-week2.json"
python3 - "$AGENT_OUT/gate-audit-week2.json" <<'EOF' || fail "a candidate reached an outcome without a human"
import json, sys
a = json.load(open(sys.argv[1]))
assert a["decisions"] == 5, a["decisions"]
assert a["decisions"] == a["human_reviews"], \
    "%d outcomes but %d human reviews" % (a["decisions"], a["human_reviews"])
assert a["decisions_with_no_human"] == [], a["decisions_with_no_human"]
assert a["self_reviewed"] == [], a["self_reviewed"]
assert sorted(a["reviewers"]) == ["user:ines", "user:mo"], a["reviewers"]
# Eleven runs, and the five that never reached a person never reached an
# outcome either: four unreadable exports plus the withdrawn candidate.
assert len(a["runs"]) == 11, len(a["runs"])
assert len([r for r in a["runs"] if not r["outcomes"]]) == 6, a["runs"]
print("   %d outcomes, %d human reviews, 0 decided without a person"
      % (a["decisions"], a["human_reviews"]))
EOF

$AGENT oversight --plan > "$AGENT_OUT/oversight-week2.json"
python3 - "$AGENT_OUT/oversight-week2.json" <<'EOF' || fail "the oversight report drifted"
import json, sys
r = json.load(open(sys.argv[1]))
g = r["human_gates"]
assert [n["node"] for n in g["client_gated_nodes"]] == ["recruiter_review"], g
assert g["ask_ttl_sec"] == 172800, g
who = r["authorized_responders"]["principals_granted_run_respond"]
assert sorted(who) == ["user:ines", "user:mo"], who
b = r["budgets"]
assert b["max_usd_micros"] == 1500000 and b["max_wall_ms"] == 120000, b
assert r["kill_switch"]["measured_cancel_to_drain_ms"], r["kill_switch"]
print("   1 client gate, 2 authorized approvers, the same four ceilings, and")
print("   a kill switch still measured at %s ms"
      % ", ".join(str(ms) for ms in r["kill_switch"]["measured_cancel_to_drain_ms"]))
EOF

# The ledger, whole: every outcome on this requisition names the person who
# reached it and the reason they gave. Five decisions, five people-hours,
# zero decisions made by the agent.
python3 - "$DECISIONS" <<'EOF' || fail "the ledger lost a name or a reason"
import json, sys
rows = [json.loads(l) for l in open(sys.argv[1])]
assert len(rows) == 5, rows
assert {r["decided_by"] for r in rows} == {"user:mo", "user:ines"}, rows
assert all(len(r["because"] or "") > 40 for r in rows), rows
EOF

printf '\n\033[32mOK\033[0m -- 1 written reason became a precedent, 5 unreadable files refused rather than rejected, 1 finding approved by a person, 5 outcomes each signed by name.\n'
