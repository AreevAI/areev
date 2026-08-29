#!/bin/sh
# One trading session at a surveillance desk, end to end, with no
# credentials and no model key.
#
# Two feeds arrive independently: block orders from the venue, and
# disclosures from a news vendor. Neither is interesting alone. What opens a
# case is a block order AND a material event on the SAME instrument inside
# one correlation window -- a `composite` trigger, which is the property
# this example exists for.
#
# Three instruments, six signals, two cases:
#   MRDN:VNTG  order, then the rebalance notice a tick later     -> CASE
#   MRDN:ORLN  the take-private wire, then the order a tick later -> CASE
#   MRDN:PDRA  order, then the notice well past the correlation
#              window                                         -> no case
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
CASES="$AGENT_OUT/cases.jsonl"
ALERTS="$AGENT_OUT/alerts.jsonl"
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

rm -rf "$AGENT_OUT"

# -- 1. seed ---------------------------------------------------------------
say "1. seed: two feeds, two member triggers, and the GATE that joins them"
SEEDED=$($AGENT seed)
CASE_WF=$(echo "$SEEDED" | jget case)
echo "$CASE_WF" > "$AGENT_OUT/workflow.hash"
echo "   signal-intake     $(echo "$SEEDED" | jget intake | cut -c1-12)..."
echo "   surveillance-case $(echo "$CASE_WF" | cut -c1-12)..."

GATE=$($AGENT gate)
echo "$GATE" | grep -q '"correlate": "/symbol"' \
  || fail "the gate does not correlate on the instrument symbol"
WINDOW=$(echo "$GATE" | jget window_ms)
[ "$WINDOW" -gt 0 ] 2>/dev/null \
  || fail "the gate declares no correlation window: $GATE"
python3 - <<EOF || fail "the gate does not name two members"
import json
g = json.loads('''$GATE''')
assert sorted(g["members"]) == ["material_event", "order_burst"], g["members"]
assert g["predicate"]["kind"] == "and", g["predicate"]
EOF
echo "   gate: order_burst AND material_event, correlated on /symbol, window ${WINDOW}ms"

# -- 2. a gate that could never fire is refused when it is WRITTEN ---------
say "2. three composites that must never reach the memory"
CHECK=$($AGENT gate-check)
echo "$CHECK" | grep -q 'at least two members' \
  || fail "a one-member composite was accepted"
echo "$CHECK" | grep -q 'needs a predicate' \
  || fail "a composite with no gate expression was accepted"
echo "$CHECK" | grep -q 'TRG-E008' \
  || fail "a gate naming an undeclared member was accepted"
echo "   one member / no predicate / TRG-E008 undeclared alias -- all refused"
echo "   at authoring time, because a dead trigger's only symptom is silence"

# -- 3. the cursors seed ---------------------------------------------------
say "3. first pass: the feeds seed their cursors and fire nothing"
tick 00; STARTED=$(echo "$REPORT" | jget runs_started)
[ "$STARTED" = "0" ] || fail "the first poll started $STARTED runs; it must seed only -- $REPORT"
[ "$(cases_open)" = "0" ] || fail "a case was opened before any signal arrived"

# -- 4. one signal alone is not a case -------------------------------------
say "4. a 480,000-share block buy in MRDN:VNTG arrives, alone"
$AGENT await-due >/dev/null
tick 01; STARTED=$(echo "$REPORT" | jget runs_started)
[ "$STARTED" = "1" ] || fail "expected 1 intake run, got $STARTED -- $REPORT"
[ "$(cases_open)" = "0" ] \
  || fail "a single signal opened a case; the gate is not gating"
echo "   1 intake run, 0 cases -- half a gate is not a gate"

# -- 5. the second signal, inside the window, opens exactly one case -------
say "5. the venue publishes a Meridian 40 rebalance for MRDN:VNTG, one tick later"
$AGENT await-due >/dev/null
tick 02; STARTED=$(echo "$REPORT" | jget runs_started)
[ "$STARTED" = "2" ] || fail "expected 1 intake + 1 case run, got $STARTED -- $REPORT"
[ "$(cases_open)" = "1" ] || fail "the correlated pair did not open exactly one case"
python3 - "$CASES" <<'EOF' || exit 1
import json, sys
c = [json.loads(l) for l in open(sys.argv[1])][0]
assert c["case_ref"] == "MRDN:VNTG", c
assert c["issuer"] == "Vantry Grid Holdings", "the issuer did not come from memory: %r" % c
assert c["signature"] == "block_buy+index_rebalance", c
assert c["has_precedent"] is False, "there is no precedent yet: %r" % c
EOF
echo "   CASE MRDN:VNTG -- block_buy+index_rebalance, issuer recalled from the book"

# -- 6. the gate does not care which signal came first ---------------------
say "6. MRDN:ORLN: the take-private wire lands first, the order follows"
$AGENT await-due >/dev/null
tick 03; STARTED=$(echo "$REPORT" | jget runs_started)
[ "$STARTED" = "1" ] || fail "expected 1 intake run, got $STARTED -- $REPORT"
[ "$(cases_open)" = "1" ] || fail "a lone disclosure opened a case"
$AGENT await-due >/dev/null
tick 04; STARTED=$(echo "$REPORT" | jget runs_started)
[ "$STARTED" = "2" ] || fail "expected 1 intake + 1 case run, got $STARTED -- $REPORT"
[ "$(cases_open)" = "2" ] || fail "the second correlated pair did not open a case"
echo "   CASE MRDN:ORLN -- the gate is a co-occurrence, not an ordering"

# -- 7. the window is the whole point --------------------------------------
say "7. MRDN:PDRA: a block order, and then the notice well after the window"
$AGENT await-due >/dev/null
tick 05; STARTED=$(echo "$REPORT" | jget runs_started)
[ "$STARTED" = "1" ] || fail "expected 1 intake run, got $STARTED -- $REPORT"
# Not a guessed sleep: this blocks until the evaluator's OWN record of that
# firing is more than window_ms in the past, so the near-miss is structural
# rather than a bet on how fast this machine is.
WAITED=$($AGENT await-window)
echo "   ...waited out the correlation window ($(echo "$WAITED" | jget since_firing_ms)ms"
echo "      since the order fired, against a $(echo "$WAITED" | jget window_ms)ms window)..."
$AGENT await-due >/dev/null
tick 06; STARTED=$(echo "$REPORT" | jget runs_started)
[ "$STARTED" = "1" ] || fail "expected 1 intake run and NO case, got $STARTED -- $REPORT"
[ "$(cases_open)" = "2" ] \
  || fail "two signals outside the window still correlated"
echo "   1 intake run, still 2 cases -- the half-match expired, as declared"

# -- 8. the desk cannot dispose of its own case ----------------------------
say "8. the desk closes its own case -- refused, structurally"
if $AGENT decide fixtures/decisions/00-desk-clears-its-own-case.json >/dev/null 2>&1; then
  fail "the principal that started the run was allowed to dispose of it"
fi
echo "   the starter cannot answer its own ask (RUN-E012, as designed)"

say "9. a dismissal with no written reason -- refused"
if $AGENT decide fixtures/decisions/01-vntg-no-reason.json >/dev/null 2>&1; then
  fail "a case was dismissed with no reason"
fi
echo "   a benign dismissal that explains nothing teaches the desk nothing"

# -- 10. two analysts, two dispositions, both signed -----------------------
say "10. nadia dismisses MRDN:VNTG as benign; oren escalates MRDN:ORLN"
$AGENT decide fixtures/decisions/02-vntg-benign.json  | jget outcome finished >/dev/null
$AGENT decide fixtures/decisions/03-orln-escalate.json | jget outcome finished >/dev/null
python3 - "$ALERTS" "$DISMISSALS" <<'EOF' || fail "the dispositions are not on the record"
import json, sys
alerts = [json.loads(l) for l in open(sys.argv[1])]
dis = [json.loads(l) for l in open(sys.argv[2])]
assert len(alerts) == 1 and alerts[0]["case_ref"] == "MRDN:ORLN", alerts
assert alerts[0]["analyst"] == "user:oren", alerts[0]
assert "market-abuse team" in alerts[0]["because"], alerts[0]
assert len(dis) == 1 and dis[0]["case_ref"] == "MRDN:VNTG", dis
assert dis[0]["analyst"] == "user:nadia", dis[0]
assert dis[0]["on_precedent"] is False, dis[0]
assert len(dis[0]["because"]) > 40, "a reason, not a shrug"
EOF
echo "   1 escalated by user:oren, 1 dismissed by user:nadia, each with a reason"

# -- 11. the dismissal became memory ---------------------------------------
say "11. nadia's reasoning is now a precedent about the SHAPE"
BRIEF=$($AGENT brief)
echo "$BRIEF" | grep -q 'block_buy+index_rebalance' \
  || fail "the benign dismissal did not become a precedent"
echo "$BRIEF" | grep -q 'mg:dismissed_by' \
  || fail "the precedent does not record who decided it"
echo "   block_buy+index_rebalance -- not about MRDN:VNTG, about the pattern"

# -- 12. redelivery is a no-op ---------------------------------------------
say "12. another pass: the same feed items again start nothing"
$AGENT await-due >/dev/null
tick 06; STARTED=$(echo "$REPORT" | jget runs_started)
[ "$STARTED" = "0" ] || fail "redelivered feed items started $STARTED runs -- $REPORT"
[ "$(cases_open)" = "2" ] || fail "a case was reopened"

# -- 13. what the gate actually did ----------------------------------------
say "13. the evaluator's own journal: what fired, and what stayed quiet"
python3 - <<EOF || exit 1
import json
t = json.loads('''$($AGENT firings)''')
comp = t["composite"]
poll = t["polling"]
assert comp["runs_started"] == 2, "the gate opened %s cases" % comp["runs_started"]
assert comp["evaluations"] >= 8, "the gate was evaluated %s times" % comp["evaluations"]
assert poll["runs_started"] == 6, "the feeds fired %s items" % poll["runs_started"]
print("   composite: %d evaluations, %d fired, %d cases opened"
      % (comp["evaluations"], comp["items"], comp["runs_started"]))
print("   polling:   %d evaluations, %d signals" % (poll["evaluations"], poll["items"]))
EOF

printf '\n\033[32mOK\033[0m -- 6 signals, 2 correlated cases, 1 near-miss outside the window,\n'
printf '     2 structural refusals, 2 dispositions signed by name.\n'
