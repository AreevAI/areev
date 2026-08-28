#!/bin/sh
# Week one of the screening desk, end to end, with no credentials and no
# model key.
#
# Four payments queue up. One clears itself, three trip the list and park
# for a compliance officer: two are real hits and get blocked, one is a
# false positive the officer clears with a written reason -- and that
# reason becomes memory.
#
# The property this example exists for: THE SCREENING RULE IS A GRAIN.
# It lives in the memory as a content-addressed blob, and the host must pin
# its address before anything will execute it. Step 2 proves the refusal.
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
LEDGER="$AGENT_OUT/ledger.jsonl"
CASES="$AGENT_OUT/cases.jsonl"

say()  { printf '\n\033[1m%s\033[0m\n' "$*"; }
fail() { echo "FAIL: $*" >&2; exit 1; }
jget() { python3 -c 'import json,sys; d=json.load(sys.stdin)
for k in sys.argv[1:]:
    d = d[int(k)] if isinstance(d, list) else d.get(k)
print(d)' "$@"; }

rm -rf "$AGENT_OUT"

# -- 1. seed ---------------------------------------------------------------
say "1. seed: the plan, the tool definitions, the RULE AS A BLOB, the trigger"
SEEDED=$($AGENT seed)
WF=$(echo "$SEEDED" | jget workflow)
RULE=$(echo "$SEEDED" | jget rule)
echo "   workflow $WF"
echo "   rule     $RULE"
echo "$WF" > "$AGENT_OUT/workflow.hash"
case "$RULE" in
  cas://sha256:*) : ;;
  *) fail "the screening rule is not a content-addressed blob: $RULE" ;;
esac
# The pin is derived from the checkout, not read out of the memory: the
# host authorizes exactly the bytes it can see.
PIN=$($AGENT pin | jget pin)
[ "cas://sha256:$PIN" = "$RULE" ] \
  || fail "the pin computed from src/ does not match the seeded blob"
echo "   pin      $PIN  (computed from src/, not from the memory)"

# -- 2. unpinned code will not run, and the cursor seeds -------------------
say "2. code with no host pin must refuse; then the cursor seeds"
UNPINNED=$($AGENT pin-check)
[ "$(echo "$UNPINNED" | jget refused)" = "True" ] \
  || fail "code ran with no host pin"
echo "$UNPINNED" | grep -q 'RUN-E018' \
  || fail "an unpinned code-carrying tool must refuse with RUN-E018"
echo "   RUN-E018: the blob travels with the memory, the permission does not"
[ ! -f "$LEDGER" ] || fail "an unpinned run reached the payment rail"

STARTED=$($AGENT ingest | jget runs_started)
[ "$STARTED" = "0" ] || fail "first poll started $STARTED runs; it must seed only"
$AGENT await-due >/dev/null

# -- 3. the same tick, pinned ----------------------------------------------
say "3. the same four payments, with the host pin in place"
STARTED=$($AGENT ingest | jget runs_started)
[ "$STARTED" = "4" ] || fail "expected 4 runs started, got $STARTED"

RELEASED=$(wc -l < "$LEDGER" | tr -d ' ')
[ "$RELEASED" = "1" ] || fail "expected 1 auto-release before any officer acted, got $RELEASED"
[ "$(head -1 "$LEDGER" | jget released_by)" = "auto" ] \
  || fail "the clean payment should release as auto"
PARKED=$($AGENT asks | python3 -c 'import json,sys; print(len(json.load(sys.stdin)))')
[ "$PARKED" = "3" ] || fail "expected 3 payments parked for an officer, got $PARKED"
echo "   1 released automatically, 3 parked for a compliance officer"

# -- 4. the desk cannot clear its own case ---------------------------------
say "4. the desk clears its own case -- refused, structurally"
if $AGENT decide fixtures/decisions/00-desk-self-clears.json >/dev/null 2>&1; then
  fail "the principal that started the run was allowed to decide it"
fi
echo "   the starter cannot decide its own run (refused, as designed)"

# -- 5. two real hits are blocked, by name ---------------------------------
say "5. mo blocks the Volkov payment; ines blocks the Sable payment"
$AGENT decide fixtures/decisions/01-volkov-block.json | jget outcome finished >/dev/null
$AGENT decide fixtures/decisions/03-sable-block.json  | jget outcome finished >/dev/null
python3 - "$LEDGER" <<'EOF' || fail "the blocks are not on the ledger under the officers' names"
import json, sys
rows = [json.loads(l) for l in open(sys.argv[1])]
blocked = {r["payment_id"]: r for r in rows if r["outcome"] == "blocked"}
assert set(blocked) == {"PMT-1002", "PMT-1004"}, sorted(blocked)
assert blocked["PMT-1002"]["blocked_by"] == "user:mo", blocked["PMT-1002"]
assert blocked["PMT-1004"]["blocked_by"] == "user:ines", blocked["PMT-1004"]
EOF
echo "   2 blocked, each signed by the officer who decided it"

# -- 6. a false positive becomes memory ------------------------------------
say "6. mo clears Aurora as a false positive, with a written reason"
$AGENT decide fixtures/decisions/02-aurora-false-positive.json | jget outcome finished >/dev/null
python3 - "$LEDGER" <<'EOF' || exit 1
import json, sys
rows = [json.loads(l) for l in open(sys.argv[1])]
fp = [r for r in rows if r["payment_id"] == "PMT-1003"]
assert fp and fp[0]["outcome"] == "released", fp
assert fp[0]["released_by"] == "user:mo", fp[0]
EOF
echo "   released after review, with mo's name on the row"

# -- 7. redelivery is a no-op ----------------------------------------------
say "7. another tick: the same payments again start nothing"
$AGENT await-due >/dev/null
STARTED=$($AGENT ingest | jget runs_started)
[ "$STARTED" = "0" ] || fail "redelivered payments started $STARTED runs; dedup must hold"

# -- 8. which rule version decided? ----------------------------------------
say "8. the examiner's question: which rule screened which payment?"
PROV=$($AGENT provenance)
echo "$PROV" | grep -q '"rule_version": "v1"' \
  || fail "the ledger does not record which rule version decided"
echo "$PROV" | grep -q "cas://sha256:$PIN" \
  || fail "provenance does not name the rule's content address"
echo "   every decision names the exact rule bytes that made it"

RELEASED=$(grep -c '"outcome": "released"' "$LEDGER" || true)
BLOCKED=$(grep -c '"outcome": "blocked"' "$LEDGER" || true)
[ "$RELEASED" = "2" ] || fail "expected 2 released, got $RELEASED"
[ "$BLOCKED" = "2" ]  || fail "expected 2 blocked, got $BLOCKED"

printf '\n\033[32mOK\033[0m -- 2 released, 2 blocked, 1 unpinned refusal, 3 decisions signed by name.\n'
