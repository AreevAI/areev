#!/bin/sh
# A night on the on-call incident desk, end to end, with no credentials and
# no model key.
#
# Alerts arrive from a monitoring system as WEBHOOKS. Areev never opens a
# port: the host owns the listener, authenticates the sender, and hands the
# payload over -- everything after that hand-off is plan nodes.
#
# The property this example exists for: A PUSH SOURCE NEEDS NO CONNECTOR.
# Every other agent example in this repo polls. This one is woken. Steps 2-5
# prove the three things that makes true: a delivery starts a governed run,
# an identical redelivery starts nothing (every vendor retries), and a
# second `manual` trigger on the SAME plan lets an engineer replay an
# incident by hand.
#
# And the desk never touches production on its own: every remediation parks
# on a client gate, and the engineer who answers it is the audit record.
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
ACTIONS="$AGENT_OUT/actions.jsonl"       # what actually touched production
INCIDENTS="$AGENT_OUT/incidents.jsonl"   # what was closed

say()  { printf '\n\033[1m%s\033[0m\n' "$*"; }
fail() { echo "FAIL: $*" >&2; exit 1; }
jget() { python3 -c 'import json,sys; d=json.load(sys.stdin)
for k in sys.argv[1:]:
    d = d[int(k)] if isinstance(d, list) else d.get(k)
print(d)' "$@"; }

rm -rf "$AGENT_OUT"

# -- 1. seed ---------------------------------------------------------------
say "1. seed: the plan, the tool definitions, the catalog, and TWO triggers"
SEEDED=$($AGENT seed)
WF=$(echo "$SEEDED" | jget workflow)
echo "   workflow $WF"
echo "$WF" > "$AGENT_OUT/workflow.hash"

# The headline, asserted rather than narrated: a push source is not polled,
# so neither standing rule carries a connector to poll it with.
$AGENT triggers | python3 -c 'import json, sys
rows = json.load(sys.stdin)
wf = sys.argv[1]
kinds = sorted(r["kind"] for r in rows)
assert kinds == ["manual", "webhook"], kinds
assert all(r["workflow"] == wf for r in rows), rows
assert all(not r.get("connector") for r in rows), rows' "$WF" \
  || fail "the standing rules are not two push rules on one plan"
echo "   webhook + manual, both pointing at that one plan, neither with a connector"

# -- 2. the monitoring system POSTs ----------------------------------------
say "2. beacon posts three alerts; the host hands each one over"
FIRST=$($AGENT listen)
[ "$(echo "$FIRST" | jget delivered)" = "3" ] || fail "expected 3 deliveries"
[ "$(echo "$FIRST" | jget runs_started)" = "3" ] \
  || fail "a delivery must start a run: $FIRST"

# One of the three is below the desk's wake floor. It is recorded and closed
# without paging anybody -- the gate guards production, not the inbox.
grep -q '"alert_id": "ALRT-4102"' "$INCIDENTS" \
  || fail "the info-severity alert was not recorded"
grep -q '"alert_id": "ALRT-4102".*"by": "auto"' "$INCIDENTS" \
  || fail "the info-severity alert should not have needed a human"
[ ! -f "$ACTIONS" ] || fail "something reached production before any human decided"

PAGED=$($AGENT pages | python3 -c 'import json,sys; print(len(json.load(sys.stdin)))')
[ "$PAGED" = "2" ] || fail "expected 2 engineers paged, got $PAGED"
echo "   3 runs started, 1 closed itself below the wake floor, 2 paged an engineer"

# -- 3. the same alert again -----------------------------------------------
say "3. beacon retries the first alert -- as every alerting vendor does"
AGAIN=$($AGENT deliver fixtures/alerts/01-checkout-5xx.json)
[ "$(echo "$AGAIN" | jget duplicates)" = "1" ] \
  || fail "the redelivery was not recognized: $AGAIN"
[ "$(echo "$AGAIN" | jget runs_started)" = "0" ] \
  || fail "a redelivery started a second run: $AGAIN"
echo "   duplicates 1, runs started 0 -- idempotent on the alert id"

# -- 4. a payload that names nothing ---------------------------------------
say "4. the sender renames its id field; the delivery names no occurrence"
NAMELESS=$($AGENT deliver fixtures/malformed-alert.json)
[ "$(echo "$NAMELESS" | jget unidentifiable)" = "1" ] \
  || fail "a payload with no dedup key should be reported, not dropped: $NAMELESS"
[ "$(echo "$NAMELESS" | jget runs_started)" = "0" ] \
  || fail "an unidentifiable payload started a run"
echo "   reported as unidentifiable and journaled -- not silently swallowed"

# -- 5. the second door ----------------------------------------------------
say "5. an engineer replays the first alert by hand (the manual trigger)"
REPLAY=$($AGENT replay ALRT-4101)
[ "$(echo "$REPLAY" | jget runs_started)" = "1" ] \
  || fail "the manual replay started nothing: $REPLAY"
# One alert, two doors, two runs -- while the redelivery through the SAME
# door (step 3) started none. Idempotency is per standing rule, which is
# what makes a deliberate replay possible at all.
$AGENT pages | python3 -c 'import json, sys
rows = json.load(sys.stdin)
on = [r for r in rows if r["alert_id"] == "ALRT-4101"]
assert sorted(r["channel"] for r in on) == ["replay", "webhook"], on
assert len({r["run_id"] for r in on}) == 2, on' \
  || fail "the replay is not a distinct governed run"
echo "   a second run on the same plan, arriving through the replay channel"

# -- 6. the desk cannot approve its own remediation -------------------------
say "6. the desk approves its own remediation -- refused, structurally"
if $AGENT decide fixtures/decisions/00-desk-approves-itself.json >/dev/null 2>&1; then
  fail "the principal that started the run was allowed to approve it"
fi
[ ! -f "$ACTIONS" ] || fail "a self-approved remediation reached production"
echo "   the starter cannot answer its own gate (refused, as designed)"

# -- 7. a human overrides the runbook --------------------------------------
say "7. rhea reads the graph and overrides the proposal before approving"
$AGENT decide fixtures/decisions/01-checkout-rollback.json | jget outcome finished >/dev/null
python3 - "$ACTIONS" <<'EOF' || fail "the override is not on the production record"
import json, sys
rows = [json.loads(l) for l in open(sys.argv[1])]
assert len(rows) == 1, rows
row = rows[0]
assert row["applied_by"] == "user:rhea", row
assert row["proposed_action"] == "scale", row      # what the runbook said
assert row["action"] == "rollback", row            # what the human decided
assert "pool checkout waits" in row["because"], row
EOF
echo "   the runbook said scale; rhea said rollback, and signed the reason"

# -- 8. a remediation that cannot execute fails loudly ---------------------
say "8. tobin approves the ledger rollback -- and it will not execute"
OUTCOME=$($AGENT decide fixtures/decisions/02-ledger-rollback.json | jget outcome finished)
case "$OUTCOME" in
  Failed*apply_remediation*) : ;;
  *) fail "the unexecutable runbook step did not fail loudly: $OUTCOME" ;;
esac
grep -q 'ALRT-4103' "$ACTIONS" && fail "a refused rollback still logged a production action"
grep -q 'ALRT-4103' "$INCIDENTS" && fail "a failed incident was closed"
echo "   the run FAILED at apply_remediation and the incident stays open --"
echo "   a remediation that quietly does nothing is the expensive failure"

# -- 9. the replay touches nothing -----------------------------------------
say "9. imara closes the replay for the write-up, touching nothing"
$AGENT decide fixtures/decisions/03-replay-record-only.json | jget outcome finished >/dev/null
python3 - "$ACTIONS" "$INCIDENTS" <<'EOF' || exit 1
import json, sys
actions = [json.loads(l) for l in open(sys.argv[1])]
closed = [json.loads(l) for l in open(sys.argv[2])]
assert len(actions) == 1, "the replay re-applied a production action: %r" % actions
replay = [c for c in closed if c["channel"] == "replay"]
assert len(replay) == 1 and replay[0]["applied"] == "none", replay
assert replay[0]["by"] == "user:imara", replay
EOF
echo "   closed by name, applied nothing -- production was restored hours ago"

# -- 10. the night's record ------------------------------------------------
say "10. what the desk did, and who is on each row"
CLOSED=$(wc -l < "$INCIDENTS" | tr -d ' ')
[ "$CLOSED" = "3" ] || fail "expected 3 closed incidents, got $CLOSED"
TOUCHED=$(wc -l < "$ACTIONS" | tr -d ' ')
[ "$TOUCHED" = "1" ] || fail "expected exactly 1 production action, got $TOUCHED"
$AGENT runs | python3 -c 'import json, sys
rows = json.load(sys.stdin)
by = {}
for r in rows:
    by[r["outcome"]] = by.get(r["outcome"], 0) + 1
assert by.get("completed") == 3, by
assert by.get("failed") == 1, by
detail = [r["detail"] for r in rows if r["outcome"] == "failed"][0]
assert "deploy channel is pinned" in detail, detail' \
  || fail "the run ledger does not match the night"
echo "   4 governed runs: 3 completed, 1 failed with the reason on the journal"

printf '\n\033[32mOK\033[0m -- 1 production action by name, 1 loud failure, 1 redelivery ignored, 1 self-approval refused.\n'
