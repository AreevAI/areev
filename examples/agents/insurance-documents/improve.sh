#!/bin/sh
# Weeks two and three on the same servicing desk.
#
# smoke.sh is the desk doing its job under governance. This is what comes
# after: (a) the clause reading nadia settled in week one now applies by
# itself, under her signature and nobody else's, (b) one document source
# keeps sending endorsements with no effective date and the runs keep
# failing, (c) the loop finds that cluster in the desk's own journals -- and
# ALSO proposes expiring the closed coverage windows, which in a bi-temporal
# memory would destroy the only thing that can answer a claim, so a person
# has to tell it no, and (d) a signed standing rule turns a loud failure into
# a cheap referral.
#
# Two decisions, opposite directions, both in writing. That is what a
# governed loop looks like from the inside.
#
# Same contract as smoke.sh: run it through a language wrapper that exports
# AGENT and AGENT_OUT. Exits non-zero on any drift.
set -eu
cd "$(dirname "$0")"

: "${AGENT:?run me through a language wrapper: python/improve.sh}"
: "${AGENT_OUT:?}"
DETS="$AGENT_OUT/determinations.jsonl"

say()  { printf '\n\033[1m%s\033[0m\n' "$*"; }
fail() { echo "FAIL: $*" >&2; exit 1; }
jget() { python3 -c 'import json,sys; d=json.load(sys.stdin)
for k in sys.argv[1:]:
    d = d[int(k)] if isinstance(d, list) else d.get(k)
print(d)' "$@"; }
# Pick one pending recommendation by analyzer (and optionally a substring of
# its summary). The acts never hard-code a hash.
pick() { python3 -c 'import json,sys
recs = json.load(sys.stdin)["pending"]
want = sys.argv[1]; needle = sys.argv[2] if len(sys.argv) > 2 else ""
hit = [r for r in recs if r["analyzer"] == want and needle in (r["summary"] or "")]
print(hit[0]["hash"] if hit else "")' "$@"; }

# Week one has to have happened -- this chapter reads its journals.
if [ ! -d "$AGENT_OUT" ]; then
  say "0. week one first"
  "$(dirname "$AGENT_OUT")/smoke.sh" >/dev/null
  echo "   ran smoke.sh -- 5 documents, 1 determination signed"
fi

# -- 1. the payoff of week one's ruling ------------------------------------
say "1. week two: a second claim on the same clause, on a different policy"
TICK=$(DOC_UPTO=09 $AGENT intake 2>/dev/null)
[ "$(echo "$TICK" | jget started)" = "4" ] || fail "expected 4 new runs"
[ "$(echo "$TICK" | jget parked)"  = "0" ] \
  || fail "an underwriter was asked a question they had already answered"
python3 - "$DETS" <<'EOF' || exit 1
import json, sys
rows = [json.loads(l) for l in open(sys.argv[1])]
new = [r for r in rows if r["claim_id"] == "CLM-8815"]
assert new, "the second quayside claim was never determined"
d = new[0]
# It issued WITHOUT an ask -- but not without an underwriter. The authority
# is nadia's earlier ruling, and the determination says so.
assert d["determined_by"] == "user:nadia", d
assert d["authority"] == "settled-wording clause:7B", d
assert d["accumulation_flag"] is False, d
# POL-6103 was cancelled effective 1 July; this loss is on 5 July, so the
# cancelled policy is simply not in the aggregate. Nobody deleted it.
assert d["aggregate_exposure"] == "1000000", \
    "a cancelled policy is still in the aggregate: %r" % d
EOF
echo "   determined automatically -- authority: settled-wording clause:7B,"
echo "   determined_by: user:nadia. The ruling carried, the signature did not"
echo "   become the desk's."

# -- 2. and three documents the desk still cannot place --------------------
say "2. the broker portal sends three more endorsements with no effective date"
[ "$(echo "$TICK" | jget failed)" = "3" ] || fail "the undated endorsements did not fail"
[ ! -f "$AGENT_OUT/referrals.jsonl" ] || fail "nothing has authorized a referral yet"
FAILED=$($AGENT runs | python3 -c 'import json,sys
print(sum(1 for r in json.load(sys.stdin) if r["outcome"] == "failed"))')
[ "$FAILED" = "4" ] || fail "expected 4 failed runs on the record, got $FAILED"
echo "   4 failed runs now. Loud, and correct -- but expensive, and nobody"
echo "   downstream learns why."

# -- 3. the desk briefs itself ---------------------------------------------
say "3. the desk briefs itself out of its own memory"
BRIEF=$($AGENT brief)
echo "$BRIEF" | grep -q "policy-servicing" || fail "the briefing does not name the plan"
echo "$BRIEF" | grep -q "clause:7B"        || fail "the briefing does not carry the ruling"
echo "   plan, judgment, coverage, rulings -- one saved query that lives in"
echo "   the file and replicates with it"

# -- 4. the loop reads the record ------------------------------------------
say "4. areev loop: deterministic analyzers over the desk's own journals"
IMPROVED=$($AGENT improve)
REC=$(echo "$IMPROVED" | pick "loop.run_outcome/1")
[ -n "$REC" ] || fail "the loop did not find the failure cluster"
echo "$IMPROVED" | python3 -c 'import json,sys
r = [x for x in json.load(sys.stdin)["pending"] if x["analyzer"] == "loop.run_outcome/1"][0]
print("   [%s] %s" % (r["severity"], r["summary"]))'

# -- 5. the gates ----------------------------------------------------------
say "5. what the loop is NOT allowed to do"
if $AGENT govern "$REC" apply --because "let the engine fix it" --as user:tomas >/dev/null 2>&1; then
  fail "the engine applied its own advisory finding"
fi
echo "   the engine cannot execute its own advice -- it is advisory"
if $AGENT govern "$REC" approve --as user:tomas >/dev/null 2>&1; then
  fail "a decision was recorded with no reason"
fi
echo "   a decision with no written reason is refused"

# -- 6. a person decides yes -----------------------------------------------
say "6. tomas approves the finding, and signs it"
$AGENT govern "$REC" approve \
  --because "the broker portal's export template has no effective-date field, so every endorsement it sends is unplaceable; the desk should refer those back the same day rather than burn a run on each one" \
  --as user:tomas >/dev/null
echo "   approved by user:tomas"

# -- 7. and a person decides NO --------------------------------------------
# The retention analyzer is right about every ordinary memory and wrong about
# this one. A coverage window that has ended is not stale -- it is the only
# grain that can answer a claim whose date of loss falls inside it. Expiring
# it would leave the desk unable to say what it was on cover for.
say "7. the loop also proposes expiring the closed coverage windows"
STALE=$(echo "$IMPROVED" | pick "loop.staleness/1" "POL-4471")
[ -n "$STALE" ] || fail "expected a staleness proposal against a closed window"
echo "$IMPROVED" | python3 -c 'import json,sys
r = [x for x in json.load(sys.stdin)["pending"]
     if x["analyzer"] == "loop.staleness/1" and "POL-4471" in (x["summary"] or "")][0]
print("   [%s] %s" % (r["severity"], r["summary"]))'
$AGENT govern "$STALE" dismiss \
  --because "a coverage window that has ended is not stale, it is the record: it is the only grain that can answer a loss dated inside it, and this desk was asked exactly that question about 18 March last week" \
  --as user:tomas >/dev/null
echo "   rejected by user:tomas -- in a bi-temporal memory the expired grain"
echo "   IS the evidence, and only a person is in a position to know that"

# -- 8. the standing rule, signed ------------------------------------------
say "8. the fix is a standing rule, and it needs a signature too"
if $AGENT desk-rule broker-portal refer_back --as user:tomas >/dev/null 2>&1; then
  fail "a standing intake rule was created with no reason"
fi
echo "   a standing rule with no written reason is refused"
$AGENT desk-rule broker-portal refer_back \
  --because "the portal export omits the effective date; refer it back the same day instead of failing a run over it" \
  --as user:tomas --after "$REC" >/dev/null
echo "   signed by user:tomas, and it names the finding it came from"

# -- 9. week three ---------------------------------------------------------
say "9. week three: the same broken document, and a good one from the same broker"
TICK=$(DOC_UPTO=11 $AGENT intake 2>/dev/null)
[ "$(echo "$TICK" | jget failed)"    = "0" ] || fail "the rule did not take effect"
[ "$(echo "$TICK" | jget referred)"  = "1" ] || fail "the undated endorsement was not referred back"
[ "$(echo "$TICK" | jget completed)" = "1" ] || fail "the complete endorsement did not book"
python3 - "$AGENT_OUT/referrals.jsonl" <<'EOF' || exit 1
import json, sys
rows = [json.loads(l) for l in open(sys.argv[1])]
assert len(rows) == 1, rows
assert rows[0]["source"] == "broker-portal", rows[0]
assert "effective date" in rows[0]["reason"], rows[0]
EOF
echo "   the undated one is referred back with a reason the broker can act on;"
echo "   the complete one books normally. Same source, different documents."
COVER=$($AGENT as-of POL-5520 mg:coverage_limit 2026-07-10 2026-08-01)
[ "$(echo "$COVER" | jget rows 0 world object)" = "250000" ] \
  || fail "the 15 July endorsement reached backwards past its own effective date"
[ "$(echo "$COVER" | jget rows 1 world object)" = "460000" ] \
  || fail "the 15 July endorsement did not take effect"
echo "   and it lands on the world clock where it belongs: 250,000 on 10 July,"
echo "   460,000 on 1 August"

# -- 10. it does not nag ---------------------------------------------------
say "10. run the loop again"
AGAIN=$($AGENT improve)
[ -z "$(echo "$AGAIN" | pick 'loop.run_outcome/1')" ] \
  || fail "the same failure evidence was proposed twice"
echo "   the failure cluster is not proposed again -- it was decided"

printf '\n\033[32mOK\033[0m -- 1 ruling applied under its underwriter'"'"'s signature, 1 finding'
printf '\n     approved, 1 finding rejected, 1 standing rule signed, 0 nags.\n'
