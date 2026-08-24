#!/bin/sh
# Week one of the AP desk, end to end, with no credentials and no model key.
#
# Four invoices arrive across two client mailboxes. One posts itself, one
# parks for a person, one is a photographed page the parser refuses to fake,
# and one comes in with a misspelled vendor that a person corrects by email
# -- the run goes around the plan's bounded correction cycle until the
# approver says yes.
#
# This script is language-neutral: every implementation under python/,
# typescript/ and rust/ exposes the same agent subcommands, so ONE set of
# assertions proves all three. Run it through a wrapper:
#
#   python/smoke.sh     typescript/smoke.sh     rust/smoke.sh
#
# The wrapper exports AGENT (how to invoke that language's agent) and
# AGENT_OUT (where its artifacts land). Exits non-zero on any drift, so CI
# can run it on every release.
set -eu
cd "$(dirname "$0")"

: "${AGENT:?run me through a language wrapper: python/smoke.sh, typescript/smoke.sh or rust/smoke.sh}"
: "${AGENT_OUT:?}"
SHEET="$AGENT_OUT/sheet.jsonl"
OUTBOX="$AGENT_OUT/outbox.jsonl"

say()  { printf '\n\033[1m%s\033[0m\n' "$*"; }
fail() { echo "FAIL: $*" >&2; exit 1; }
# jq-free JSON probes (python3 is the test harness here, not the agent).
jget() { python3 -c 'import json,sys; d=json.load(sys.stdin)
for k in sys.argv[1:]:
    d = d[int(k)] if isinstance(d, list) else d.get(k)
print(d)' "$@"; }

rm -rf "$AGENT_OUT"

# ── 1. seed ────────────────────────────────────────────────────────────────
say "1. seed: the plan, its tools, the saved queries, two mailbox triggers"
SEEDED=$($AGENT seed)
WF=$(echo "$SEEDED" | jget workflow)
echo "   workflow $WF"
# Every language's seeder pins created_at, so all three mint the SAME plan
# hash -- run-smokes.sh asserts it. A grain is its bytes.
echo "$WF" > "$AGENT_OUT/workflow.hash"

# ── 2. the first poll seeds and fires nothing ─────────────────────────────
# Declaring a trigger never replays mailbox history: the first evaluation
# records the connector's current cursor and starts no runs.
say "2. first heartbeat tick: cursors seed, nothing fires"
STARTED=$($AGENT ingest | jget runs_started)
[ "$STARTED" = "0" ] || fail "first poll started $STARTED runs; it must seed only"

sleep 1.2

# ── 3. the mail arrives ───────────────────────────────────────────────────
say "3. second tick: four invoices, two clients, one plan"
STARTED=$($AGENT ingest | jget runs_started)
[ "$STARTED" = "4" ] || fail "expected 4 runs started, got $STARTED"

STATE=$($AGENT runs | python3 -c 'import json,sys
print(" ".join(sorted(r["outcome"] for r in json.load(sys.stdin))))')
echo "   run outcomes: $STATE"
case "$STATE" in
  *failed*) : ;;
  *) fail "the scanned page did not fail loudly" ;;
esac
POSTED=$(wc -l < "$SHEET" | tr -d ' ')
[ "$POSTED" = "1" ] || fail "expected 1 auto-posted row before any human acted, got $POSTED"
[ "$(head -1 "$SHEET" | jget approved_by)" = "auto" ] \
  || fail "the small clean invoice should post as auto"

# ── 4. the agent cannot approve its own ask ───────────────────────────────
say "4. the desk emails itself an approval -- refused, structurally"
if $AGENT reply fixtures/replies/00-self-approve.json >/dev/null 2>&1; then
  fail "the principal that started the run was allowed to approve it"
fi
echo "   the starter cannot approve its own run (refused, as designed)"

# ── 5. a person approves the big one ──────────────────────────────────────
say "5. dana approves the over-threshold invoice by reply"
$AGENT reply fixtures/replies/01-approve-large.json | jget outcome finished >/dev/null
python3 - "$SHEET" <<'EOF' || fail "dana's name is not on the posted row"
import json, sys
rows = [json.loads(l) for l in open(sys.argv[1])]
assert any(r["approved_by"] == "user:dana" for r in rows)
EOF

# ── 6. a correction goes around the cycle ─────────────────────────────────
say "6. priya replies 'revise' with a Field: value line -- the run re-asks"
OUT=$($AGENT reply fixtures/replies/02-revise-vendor.json)
echo "$OUT" | python3 -c 'import json,sys
o = json.load(sys.stdin)["outcome"]
assert "parked" in o, o' || fail "a revise reply must park the run again with the corrected rows"
ASKS=$(grep -c '2fec52885a61' "$OUTBOX" || true)
[ "$ASKS" = "2" ] || fail "expected the corrected row to be re-asked (2 asks in the outbox), got $ASKS"

say "7. priya approves the corrected row"
$AGENT reply fixtures/replies/03-approve-revised.json | jget outcome finished >/dev/null
python3 - "$SHEET" <<'EOF' || exit 1
import json, sys
rows = [json.loads(l) for l in open(sys.argv[1])]
fixed = [r for r in rows if r["approved_by"] == "user:priya"]
assert fixed and fixed[0]["vendor"] == "Cobalt Cloud", \
    "the posted vendor is not the corrected one: %r" % fixed
assert not any("Cobolt" in (r["vendor"] or "") for r in rows), \
    "a misspelled vendor reached the sheet"
EOF
echo "   the sheet got the corrected vendor, with priya's name on the row"

# ── 8. redelivery is a no-op ──────────────────────────────────────────────
say "8. another tick: the same mail again starts nothing"
sleep 1.2
STARTED=$($AGENT ingest | jget runs_started)
[ "$STARTED" = "0" ] || fail "redelivered mail started $STARTED runs; dedup must hold"

# ── 9. final ledger ───────────────────────────────────────────────────────
POSTED=$(wc -l < "$SHEET" | tr -d ' ')
[ "$POSTED" = "3" ] || fail "expected 3 posted rows, got $POSTED"
grep -q 'INV-NG-2201' "$SHEET" && fail "the unreadable scan reached the sheet"

printf '\n\033[32mOK\033[0m -- 3 posted, 1 refused, 1 correction round-tripped, 2 approvals signed by name.\n'
