#!/bin/sh
# The next trading session at the same surveillance desk.
#
# smoke.sh is the desk doing its job under governance. This is what comes
# after:
#
#   (a) the near-miss from last session did NOT survive -- a half-match has
#       an expiry, and the desk does not hold one forever;
#   (b) the same two signals arrive properly correlated this time, and the
#       case that opens is PRE-ANNOTATED with the precedent an analyst set
#       last week -- on a different instrument, because the precedent is
#       about the PATTERN;
#   (c) it still parks for a human. A precedent is not a disposition, and
#       an agent that closes its own surveillance alerts is the thing this
#       example is arguing against;
#   (d) the loop reads the desk's own case record and finds the shape that
#       keeps costing an analyst an afternoon -- and a person decides what
#       to do about it, in writing.
#
# Same contract as smoke.sh: run it through a language wrapper that exports
# AGENT and AGENT_OUT. Exits non-zero on any drift.
set -eu
cd "$(dirname "$0")"

: "${AGENT:?run me through a language wrapper: python/improve.sh}"
: "${AGENT_OUT:?}"
CASES="$AGENT_OUT/cases.jsonl"
DISMISSALS="$AGENT_OUT/dismissals.jsonl"

say()  { printf '\n\033[1m%s\033[0m\n' "$*"; }
fail() { echo "FAIL: $*" >&2; exit 1; }
jget() { python3 -c 'import json,sys; d=json.load(sys.stdin)
for k in sys.argv[1:]:
    d = d[int(k)] if isinstance(d, list) else d.get(k)
print(d)' "$@"; }
cases_open() { [ -f "$CASES" ] && wc -l < "$CASES" | tr -d ' ' || echo 0; }
# REPORT stays readable after the call, so a failed assertion can print it.
tick() { REPORT=$(FEED_UPTO="$1" $AGENT ingest); }

# Last session has to have happened -- this chapter reads its journals.
if [ ! -d "$AGENT_OUT" ]; then
  say "0. last session first"
  "$(dirname "$AGENT_OUT")/smoke.sh" >/dev/null
  echo "   ran smoke.sh -- 6 signals, 2 cases, 2 dispositions"
fi

# -- 1. the expired half-match did not linger ------------------------------
say "1. next session: MRDN:PDRA gets another block order, alone"
$AGENT await-due >/dev/null
tick 07; STARTED=$(echo "$REPORT" | jget runs_started)
[ "$STARTED" = "1" ] || fail "expected 1 intake run, got $STARTED -- $REPORT"
[ "$(cases_open)" = "2" ] \
  || fail "last session's expired MRDN:PDRA signal was still in the gate"
echo "   1 intake run, still 2 cases -- last session's half-match is gone,"
echo "   so this order starts a NEW partial match rather than completing an old one"

# -- 2. the pair correlates, and the case arrives with prior art -----------
say "2. the rebalance notice follows one tick later -- inside the window"
$AGENT await-due >/dev/null
tick 08; STARTED=$(echo "$REPORT" | jget runs_started)
[ "$STARTED" = "2" ] || fail "expected 1 intake + 1 case run, got $STARTED -- $REPORT"
[ "$(cases_open)" = "3" ] || fail "the correlated pair did not open a case"
python3 - "$CASES" <<'EOF' || exit 1
import json, sys
rows = [json.loads(l) for l in open(sys.argv[1])]
c = rows[-1]
assert c["case_ref"] == "MRDN:PDRA", c
assert c["signature"] == "block_buy+index_rebalance", c
assert c["has_precedent"] is True, "the case arrived with no prior art: %r" % c
assert c["precedent_by"] == "user:nadia", c
assert "passive tracker" in c["precedent"], c["precedent"]
# The precedent was set on MRDN:VNTG. Nothing about MRDN:VNTG carried over -- only the
# shape did.
assert "MRDN:VNTG" not in c["precedent"], "the precedent leaked the other instrument"
EOF
echo "   CASE MRDN:PDRA -- and it opens with nadia's MRDN:VNTG reasoning already attached"

# -- 3. a precedent is not a disposition -----------------------------------
say "3. and it STILL parks for an analyst"
PENDING=$($AGENT cases | python3 -c 'import json,sys; print(len(json.load(sys.stdin)))')
[ "$PENDING" = "1" ] || fail "expected 1 case waiting on a human, got $PENDING"
$AGENT cases | grep -q '"has_precedent": true' \
  || fail "the parked case does not carry its precedent"
echo "   the memory made the case cheaper to judge, not automatic --"
echo "   there is no edge in the plan that closes a case without a person"

# -- 4. the analyst decides, on the precedent, and signs it ----------------
say "4. nadia dismisses it -- citing the precedent, and saying what she checked"
$AGENT decide fixtures/decisions/04-pdra-benign.json | jget outcome finished >/dev/null
python3 - "$DISMISSALS" <<'EOF' || exit 1
import json, sys
rows = [json.loads(l) for l in open(sys.argv[1])]
assert len(rows) == 2, rows
ptra = [r for r in rows if r["case_ref"] == "MRDN:PDRA"]
assert ptra, rows
assert ptra[0]["on_precedent"] is True, ptra[0]
assert ptra[0]["analyst"] == "user:nadia", ptra[0]
assert "participation history" in ptra[0]["because"], ptra[0]["because"]
EOF
echo "   2 benign dismissals now, both signed, the second on the precedent"

# -- 5. the desk briefs itself ---------------------------------------------
say "5. the desk briefs itself out of its own memory"
BRIEF=$($AGENT brief)
echo "$BRIEF" | grep -q "surveillance-case" || fail "the briefing does not name the plan"
echo "$BRIEF" | grep -q "composite" || fail "the briefing does not carry the gate"
echo "$BRIEF" | grep -q "block_buy+index_rebalance" \
  || fail "the briefing does not carry the desk's precedents"
echo "   plans, gates, precedents -- one saved query, one budget"

# -- 6. the loop reads the case record -------------------------------------
say "6. areev loop: deterministic analyzers over the desk's own dispositions"
IMPROVED=$($AGENT improve)
REC=$(echo "$IMPROVED" | python3 -c 'import json,sys
d = json.load(sys.stdin)
assert d["loop"]["auto_applied"] == 0, "the engine applied something itself"
recs = [r for r in d["pending"] if r["analyzer"] == "loop.tool_failure/1"]
assert recs, "the loop did not notice the recurring benign pattern"
print(recs[0]["hash"])')
echo "$IMPROVED" | python3 -c 'import json,sys
for r in json.load(sys.stdin)["pending"]:
    if r["analyzer"] == "loop.tool_failure/1":
        print("   [%s] %s" % (r["severity"], r["summary"]))'
echo "   two of the three cases an analyst opened were the same shape, and"
echo "   both times the answer was benign -- that is a fact about the GATE"
echo "   0 auto-applied: the engine proposed, and then stopped"

# -- 7. the gate on the loop -----------------------------------------------
say "7. what the loop is NOT allowed to do"
if $AGENT govern "$REC" approve --as user:nadia >/dev/null 2>&1; then
  fail "a decision was recorded with no reason"
fi
echo "   a decision with no written reason is refused"

# -- 8. a person decides, and signs it -------------------------------------
say "8. an analyst decides, and signs it"
$AGENT govern "$REC" approve \
  --because "keep the gate as it is. A scheduled rebalance plus a tracker's block was benign in both cases we have seen, but suppressing the pair would also suppress the case where the buy PRECEDES the notice, which is exactly the abuse this desk exists to catch. Record the lesson; do not narrow the rule." \
  --as user:oren >/dev/null
echo "   approved by user:oren -- and the lesson is 'record it', not 'stop alerting'"

say "8b. and the approval cannot be quietly walked back"
if $AGENT govern "$REC" dismiss --because "on reflection, drop it" \
     --as user:nadia >/dev/null 2>&1; then
  fail "an approved recommendation was rejected after the fact"
fi
echo "   approved has no exit but applied or expired -- a second reviewer"
echo "   cannot erase the first one's decision, only act on it"

# -- 9. it does not nag ----------------------------------------------------
say "9. run the loop again"
AGAIN=$($AGENT improve | python3 -c 'import json,sys
print(len([r for r in json.load(sys.stdin)["pending"]
           if r["analyzer"] == "loop.tool_failure/1"]))')
[ "$AGAIN" = "0" ] || fail "the same evidence became a second recommendation"
echo "   deduped -- the same evidence does not become a second recommendation"

# -- 10. the ledger of the whole two sessions ------------------------------
say "10. two sessions, from the evaluator's own journal"
python3 - <<EOF || exit 1
import json
t = json.loads('''$($AGENT firings)''')
comp, poll = t["composite"], t["polling"]
assert comp["runs_started"] == 3, "the gate opened %s cases" % comp["runs_started"]
assert poll["runs_started"] == 8, "the feeds fired %s signals" % poll["runs_started"]
print("   8 signals across two feeds -> 3 correlated cases, 0 auto-closed")
EOF

printf '\n\033[32mOK\033[0m -- 1 precedent crossed instruments, 3 cases all judged by a person,\n'
printf '     1 loop finding approved with a written reason.\n'
