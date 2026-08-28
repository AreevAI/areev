#!/bin/sh
# The next two weeks on the same incident desk.
#
# smoke.sh is the desk doing its job under governance. This is what comes
# after: (a) a change freeze pauses the standing rule and deliveries are
# REFUSED rather than quietly swallowed, (b) the alert shape that needed a
# human in week one arrives again and is RECOGNIZED -- the cause an engineer
# wrote down rides back in through the trigger's declared context, and the
# desk proposes the fix that worked instead of the runbook's guess, (c) the
# same runbook step keeps failing on one service, (d) the loop finds that
# cluster in the desk's own run journals, and (e) a person approves the
# finding with a written reason.
#
# The human does NOT get removed. The payoff is a better proposal arriving
# at the same gate -- an on-call desk that auto-applies production actions
# because it has seen the alert before is teaching the wrong lesson.
#
# Same contract as smoke.sh: run it through a language wrapper that exports
# AGENT and AGENT_OUT. Exits non-zero on any drift.
set -eu
cd "$(dirname "$0")"

: "${AGENT:?run me through a language wrapper: python/improve.sh}"
: "${AGENT_OUT:?}"
ACTIONS="$AGENT_OUT/actions.jsonl"
INCIDENTS="$AGENT_OUT/incidents.jsonl"

say()  { printf '\n\033[1m%s\033[0m\n' "$*"; }
fail() { echo "FAIL: $*" >&2; exit 1; }
jget() { python3 -c 'import json,sys; d=json.load(sys.stdin)
for k in sys.argv[1:]:
    d = d[int(k)] if isinstance(d, list) else d.get(k)
print(d)' "$@"; }

# Week one has to have happened -- this chapter reads its journals.
if [ ! -d "$AGENT_OUT" ]; then
  say "0. week one first"
  "$(dirname "$AGENT_OUT")/smoke.sh" >/dev/null
  echo "   ran smoke.sh -- 4 runs, 1 production action, 1 loud failure"
fi

# -- 1. a change freeze ----------------------------------------------------
say "1. a change freeze opens: the standing rule is paused, with a reason"
$AGENT pause --because "change freeze RF-118: nothing may act on production" >/dev/null
if OUT=$($AGENT deliver fixtures/alerts/04-checkout-5xx-again.json 2>&1); then
  fail "a paused desk accepted a delivery: $OUT"
fi
echo "$OUT" | grep -q 'paused' || fail "the refusal does not name the pause: $OUT"
echo "   the delivery is REFUSED, loudly -- the sender's retry is the queue,"
echo "   and a paused desk that silently accepted work would be worse than down"
$AGENT resume --because "freeze lifted for sev-1 pages" >/dev/null

# -- 2. week two -----------------------------------------------------------
say "2. week two: five more alerts, including the whole first batch again"
SECOND=$(ALERT_UPTO=08 $AGENT listen)
[ "$(echo "$SECOND" | jget delivered)" = "8" ] || fail "expected 8 deliveries"
[ "$(echo "$SECOND" | jget duplicates)" = "3" ] \
  || fail "the vendor's replay of week one was not deduped: $SECOND"
[ "$(echo "$SECOND" | jget runs_started)" = "5" ] \
  || fail "expected 5 new runs, got: $SECOND"
echo "   8 delivered, 3 recognized as already handled, 5 new runs"

# -- 3. the payoff of week one's write-up ----------------------------------
say "3. the checkout alert is back -- and this time the desk knows it"
# The runbook's guess for http_5xx_rate is "scale" -- that is what week one
# was offered. What arrives now is the fix that actually worked, named. And
# it is still a PAGE: the gate did not move.
$AGENT pages | python3 -c 'import json, sys
rows = {r["alert_id"]: r for r in json.load(sys.stdin)}
page = rows.get("ALRT-4204")
assert page, "the repeat alert did not reach an engineer at all"
assert page["proposed_action"] == "rollback", page
assert page["confidence"] == "known", page
assert "connection pool" in page["known_cause"], page
assert page["prior_incidents"] >= 1, page
print("   proposal: %s (%s) -- %s" % (
    page["proposed_action"], page["confidence"], page["rationale"]))' || exit 1
echo "   the cause rhea wrote down rode back in through the trigger's context"
echo "   -- a better proposal at the same gate, not a removed human"

# -- 4. the night's decisions ----------------------------------------------
say "4. the engineers decide: one known fix, two doomed rollbacks, one no"
$AGENT decide fixtures/decisions/04-checkout-known-fix.json | jget outcome finished >/dev/null
$AGENT decide fixtures/decisions/05-ledger-rollback-again.json >/dev/null
$AGENT decide fixtures/decisions/06-ledger-rollback-again.json >/dev/null
$AGENT decide fixtures/decisions/07-notify-queue-record-only.json | jget outcome finished >/dev/null
python3 - "$ACTIONS" <<'EOF' || exit 1
import json, sys
rows = [json.loads(l) for l in open(sys.argv[1])]
assert len(rows) == 2, "expected 2 production actions across both weeks: %r" % rows
known = [r for r in rows if r["alert_id"] == "ALRT-4204"][0]
assert known["proposed_action"] == "rollback" and known["confidence"] == "known", known
assert known["applied_by"] == "user:rhea", known
# The engineer took the proposal as it stood -- no override this time.
assert known["action"] == known["proposed_action"], known
EOF
echo "   rhea took the proposal as it stood; nothing was overridden this time"

# -- 5. the loop reads the record ------------------------------------------
say "5. areev loop: deterministic analyzers over the desk's own journals"
IMPROVED=$($AGENT improve)
[ "$(echo "$IMPROVED" | jget stored)" != "0" ] || fail "the loop proposed nothing"
REC=$(echo "$IMPROVED" | python3 -c 'import json,sys
recs = [r for r in json.load(sys.stdin)["pending"]
        if r["analyzer"] == "loop.run_outcome/1"]
assert recs, "no run-outcome finding"
print(recs[0]["hash"])')
echo "$IMPROVED" | python3 -c 'import json,sys
rec = [r for r in json.load(sys.stdin)["pending"]
       if r["analyzer"] == "loop.run_outcome/1"][0]
assert "apply_remediation" in rec["summary"], rec
assert "pinned" in rec["summary"], rec
print("   %s -- %s" % (rec["severity"].upper(), rec["summary"]))' \
  || fail "the finding does not name the failing node and why"

# -- 6. the gate -----------------------------------------------------------
say "6. what the loop is NOT allowed to do"
if $AGENT govern "$REC" apply --because "let the engine fix it" --as user:tobin >/dev/null 2>&1; then
  fail "the engine applied its own advisory finding"
fi
echo "   the engine cannot execute its own advice -- it is advisory"
if $AGENT govern "$REC" approve --as user:tobin >/dev/null 2>&1; then
  fail "a decision was recorded with no reason"
fi
echo "   a decision with no written reason is refused"

# -- 7. a person decides, and signs it -------------------------------------
say "7. an engineer decides, and signs it"
$AGENT govern "$REC" approve \
  --because "the replication_lag runbook step is a rollback, and RF-118 has ledger-sync's deploy channel pinned until quarter close -- three nights of on-call approved a step that could never execute; the step needs a freeze-aware alternative before it is offered again" \
  --as user:imara >/dev/null
echo "   approved by user:imara"

# -- 8. the desk briefs itself ---------------------------------------------
say "8. the desk briefs itself out of its own memory"
BRIEF=$($AGENT brief)
echo "$BRIEF" | grep -q "incident-response" || fail "the briefing does not name the plan"
echo "$BRIEF" | grep -q "wake_severity" || fail "the briefing does not carry the desk's own rules"
echo "$BRIEF" | grep -q "mg:known_fix" || fail "the briefing does not carry what it learned"
echo "   plan, catalog, wake floor, and the causes it wrote down -- one saved"
echo "   CAL query stored in the memory file itself, not in this script"

# -- 9. it does not nag ----------------------------------------------------
say "9. run the loop again"
STORED=$($AGENT improve | jget stored)
[ "$STORED" = "0" ] || fail "the same evidence became $STORED more recommendations"
echo "   deduped -- the same evidence does not become a second recommendation"

# -- 10. two weeks, in numbers ---------------------------------------------
say "10. two weeks of nights"
$AGENT runs | python3 -c 'import json, sys
rows = json.load(sys.stdin)
by = {}
for r in rows:
    by[r["outcome"]] = by.get(r["outcome"], 0) + 1
assert len(rows) == 9, rows
assert by.get("completed") == 6, by
assert by.get("failed") == 3, by' \
  || fail "the run ledger does not match the two weeks"
CLOSED=$(wc -l < "$INCIDENTS" | tr -d ' ')
[ "$CLOSED" = "6" ] || fail "expected 6 closed incidents, got $CLOSED"
python3 - "$ACTIONS" <<'EOF' || fail "a production action has no human on it"
import json, sys
for row in (json.loads(l) for l in open(sys.argv[1])):
    assert row["applied_by"].startswith("user:"), row
EOF
echo "   9 runs, 6 closed incidents, 2 production actions -- both signed by a person"

printf '\n\033[32mOK\033[0m -- 1 cause became memory, 1 pattern found in 9 runs, 1 decision signed by name.\n'
