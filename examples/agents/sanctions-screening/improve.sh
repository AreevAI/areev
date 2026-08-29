#!/bin/sh
# Weeks two and three of the same screening desk.
#
# smoke.sh is the desk doing its job under governance. This is what comes
# after: (a) the false positive an officer cleared in week one now clears
# itself, (b) a vendor starts sending names the rule cannot read and says so
# loudly, (c) the loop finds that cluster in the desk's own journals, and
# (d) THE RULE ITSELF IS REVISED -- new bytes, new content address, and the
# desk refuses to run them until a human syncs the pin.
#
# That last act is the one no other example in this repo can show: the code
# an agent runs on is a governed grain, not a file on a box.
#
# Same contract as smoke.sh: run it through a language wrapper that exports
# AGENT and AGENT_OUT. Exits non-zero on any drift.
set -eu
cd "$(dirname "$0")"

: "${AGENT:?run me through a language wrapper: python/improve.sh}"
: "${AGENT_OUT:?}"
LEDGER="$AGENT_OUT/ledger.jsonl"

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
  echo "   ran smoke.sh -- 4 payments, 2 released, 2 blocked"
fi

# -- 1. more payments ------------------------------------------------------
say "1. week two: the cleared counterparty returns, and three unreadable names"
$AGENT await-due >/dev/null
STARTED=$(PAY_UPTO=08 $AGENT ingest | jget runs_started)
[ "$STARTED" = "4" ] || fail "expected 4 new runs, got $STARTED"

# -- 2. the payoff of week one's disposition -------------------------------
say "2. the false positive mo cleared now clears itself"
python3 - "$LEDGER" <<'EOF' || exit 1
import json, sys
rows = [json.loads(l) for l in open(sys.argv[1])]
again = [r for r in rows if r["payment_id"] == "PMT-1005"]
assert again, "the week-two Aurora payment did not clear at all"
assert again[0]["outcome"] == "released", again[0]
assert again[0]["released_by"] == "auto", \
    "it should not have needed an officer this time: %r" % again[0]
EOF
echo "   released automatically -- the signed disposition became memory"

# -- 3. and the rule refuses what it cannot read ---------------------------
say "3. three payments carry mangled counterparty names"
STATE=$($AGENT runs | python3 -c 'import json,sys
print(sum(1 for r in json.load(sys.stdin) if r["outcome"] == "failed"))')
[ "$STATE" -ge 3 ] || fail "the unreadable names did not fail loudly ($STATE failures)"
grep -q 'PMT-100[678]' "$LEDGER" && fail "a mangled name reached the payment rail"
echo "   $STATE runs failed rather than screening a mangled name -- a false"
echo "   clear is the expensive failure; a stopped payment is the cheap one"

# -- 4. the desk briefs itself ---------------------------------------------
say "4. the desk briefs itself out of its own memory"
BRIEF=$($AGENT brief)
echo "$BRIEF" | grep -q "sanctions-screening" || fail "the briefing does not name the plan"
echo "$BRIEF" | grep -q "match_floor" || fail "the briefing does not carry the desk's own rules"
echo "   plan, rule, dispositions -- one saved query, one budget"

# -- 5. the loop reads the record ------------------------------------------
say "5. areev loop: deterministic analyzers over the desk's own journals"
IMPROVED=$($AGENT improve)
REC=$(echo "$IMPROVED" | jget pending 0 hash)
[ -n "$REC" ] && [ "$REC" != "None" ] || fail "the loop proposed nothing"
echo "$IMPROVED" | jget pending 0 summary | sed 's/^/   /'

# -- 6. the gate -----------------------------------------------------------
say "6. what the loop is NOT allowed to do"
if $AGENT govern "$REC" apply --because "let the engine fix it" --as user:mo >/dev/null 2>&1; then
  fail "the engine applied its own advisory finding"
fi
echo "   the engine cannot execute its own advice -- it is advisory"
if $AGENT govern "$REC" approve --as user:mo >/dev/null 2>&1; then
  fail "a decision was recorded with no reason"
fi
echo "   a decision with no written reason is refused"

# -- 7. a person decides, and signs it -------------------------------------
say "7. a compliance officer decides, and signs it"
$AGENT govern "$REC" approve \
  --because "the rail is double-encoding cyrillic homoglyphs; repair and fold them before matching, and keep refusing anything still unreadable" \
  --as user:ines >/dev/null
echo "   approved by user:ines"

# -- 8. THE RULE MOVES -----------------------------------------------------
# This is the act. A revised rule is new bytes, so it is a NEW content
# address -- and the host's pin still names the old one. The desk refuses.
say "8. the revised rule is seeded: new bytes, new address"
REVISED=$($AGENT revise)
NEWPIN=$(echo "$REVISED" | jget pin)
OLDPIN=$($AGENT pin | jget pin)
[ "$NEWPIN" != "$OLDPIN" ] || fail "the revision did not change the rule's address"
echo "   was $OLDPIN"
echo "   now $NEWPIN"

say "9. the desk now refuses to run: the memory moved ahead of the pin"
$AGENT await-due >/dev/null
HELD=$(PAY_UPTO=99 $AGENT ingest)
[ "$(echo "$HELD" | jget runs_started)" = "0" ] \
  || fail "the desk ran code the host had not pinned"
echo "$HELD" | grep -q 'RUN-E018' || fail "expected RUN-E018 after the revision"
grep -q 'PMT-1010' "$LEDGER" && fail "a payment was processed under an unpinned rule"
echo "   RUN-E018 -- and the cursor is HELD, so nothing was silently dropped:"
$AGENT trigger-state | python3 -c 'import json,sys
t = json.load(sys.stdin)[0]
print("   consecutive_failures=%s  cursor=%s" % (t["consecutive_failures"], t.get("cursor")))
assert t["consecutive_failures"] >= 1, t'

# -- 10. the operator syncs the checkout, and the fix takes effect ---------
say "10. the operator syncs the checkout; the pin and the memory agree again"
# The trigger backed off after the refusal -- that backoff IS the point, so
# wait it out on the evaluator's own clock rather than guessing at it.
$AGENT await-due 120 >/dev/null
STARTED=$(PAY_UPTO=99 RULE_FILE=screen_v2.py $AGENT ingest | jget runs_started)
[ "$STARTED" = "2" ] || fail "expected the held payments to start, got $STARTED"
python3 - "$LEDGER" <<'EOF' || exit 1
import json, sys
rows = [json.loads(l) for l in open(sys.argv[1])]
by = {r["payment_id"]: r for r in rows}
assert "PMT-1010" in by, "the clean week-three payment did not clear"
assert by["PMT-1010"]["rule_version"] == "v2", by["PMT-1010"]
assert "PMT-1009" not in by, \
    "the repaired name went straight through instead of stopping for review"
EOF
# And this is what the revision actually bought. Under v1 the mangled name
# FAILED the run -- loud, but blind. Under v2 it is repaired, screened, and
# turns out to be an exact list match that was hiding behind the homoglyphs.
PENDING=$($AGENT asks | python3 -c 'import json,sys
rows = json.load(sys.stdin)
hit = [r for r in rows if r["match_name"] == "Kestrel Marine Ltd"]
assert hit, "v2 did not surface the hidden match: %r" % rows
print("%s (score %s)" % (hit[0]["match_name"], hit[0]["match_score"]))')
echo "   the name v1 could only refuse is now screened -- and it is a real"
echo "   match that was hiding behind the homoglyphs: $PENDING"
echo "   the ledger records which rule version decided each payment"

# -- 11. it does not nag ---------------------------------------------------
say "11. run the loop again"
PENDING=$($AGENT improve | python3 -c 'import json,sys; print(len(json.load(sys.stdin)["pending"]))')
[ "$PENDING" = "0" ] || fail "the same finding was proposed twice"
echo "   deduped -- the same evidence does not become a second recommendation"

printf '\n\033[32mOK\033[0m -- 1 disposition became memory, 1 rule revised under a pin, 2 rule versions on the ledger.\n'
