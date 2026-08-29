#!/bin/sh
# A week on a policy servicing desk, end to end, with no credentials and no
# model key.
#
# Five documents arrive. An endorsement raises a limit -- BACKDATED, effective
# six weeks before it reached the desk. A correction says the deductible was
# mis-keyed at inception. A claim comes in whose date of loss falls BEFORE the
# endorsement's effective date. A broker sends an endorsement with no
# effective date at all. A cancellation ends a policy.
#
# The property this example exists for: THE DESK KEEPS TWO CLOCKS.
#
#   world      what cover was actually in force at time T
#   knowledge  what this desk knew at time T
#
# A claim is assessed on the world clock -- the loss happened on a date, and
# the cover that responds is the cover in force on that date, not the cover
# the file holds today. A dispute or a regulator's question is answered on
# the knowledge clock. Step 3 is where the two disagree, and the whole
# example is built to make that disagreement impossible to miss.
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
DETS="$AGENT_OUT/determinations.jsonl"

say()  { printf '\n\033[1m%s\033[0m\n' "$*"; }
fail() { echo "FAIL: $*" >&2; exit 1; }
jget() { python3 -c 'import json,sys; d=json.load(sys.stdin)
for k in sys.argv[1:]:
    d = d[int(k)] if isinstance(d, list) else d.get(k)
print(d)' "$@"; }

rm -rf "$AGENT_OUT"

# -- 1. seed ---------------------------------------------------------------
say "1. seed: the plan, the tool definitions, and the schedule as issued"
SEEDED=$($AGENT seed)
WF=$(echo "$SEEDED" | jget workflow)
echo "$WF" > "$AGENT_OUT/workflow.hash"
echo "   workflow $WF"
[ "$(echo "$SEEDED" | jget policies)" = "4" ] || fail "the schedule did not seed"
# Each policy is booked on BOTH clocks: valid_from is when cover attached,
# created_at is when the desk keyed it.
AS_ISSUED=$($AGENT as-of POL-4471 mg:coverage_limit 2026-02-01)
[ "$(echo "$AS_ISSUED" | jget rows 0 world object)" = "500000" ] \
  || fail "POL-4471 did not attach at 500,000"
[ "$(echo "$AS_ISSUED" | jget rows 0 knowledge object)" = "500000" ] \
  || fail "the desk should already have known the limit in February"
echo "   POL-4471 attaches 1 Jan at 500,000 -- true, and known, in February"

# -- 2. the week's post ----------------------------------------------------
say "2. five documents: an endorsement, a correction, a claim, an undated"
echo "   broker endorsement, and a cancellation"
TICK=$(DOC_UPTO=05 $AGENT intake 2>/dev/null)
[ "$(echo "$TICK" | jget started)"   = "5" ] || fail "expected 5 runs, got $(echo "$TICK" | jget started)"
[ "$(echo "$TICK" | jget completed)" = "3" ] || fail "expected 3 completed"
[ "$(echo "$TICK" | jget parked)"    = "1" ] || fail "the claim did not park for an underwriter"
[ "$(echo "$TICK" | jget failed)"    = "1" ] || fail "the undated endorsement did not fail"
echo "   3 booked, 1 parked on an underwriter, 1 refused"

say "   the refusal: a coverage document with no effective date belongs on"
echo "   neither clock, and the desk will not guess one"
echo "$TICK" | grep -q '"document_id": "END-9002", "state": "failed"' \
  || fail "END-9002 should have failed, not been guessed at"
[ ! -f "$AGENT_OUT/referrals.jsonl" ] \
  || fail "there is no refer-back route yet -- nobody has signed one"

# -- 3. THE TWO CLOCKS -----------------------------------------------------
# The endorsement raised POL-4471 to 750,000 effective 1 May. It reached the
# desk on 15 June. So for six weeks the higher limit was TRUE and UNKNOWN.
say "3. the endorsement was backdated: effective 1 May, keyed 15 June"
COVER=$($AGENT as-of POL-4471 mg:coverage_limit 2026-03-18 2026-05-20 2026-08-01)
python3 - <<EOF || exit 1
import json
d = json.loads('''$COVER''')
rows = {r["at"]: r for r in d["rows"]}

# (a) THE CENTREPIECE. On the date of loss the cover in force was 500,000 --
#     while the file's head, and any plain recall, says 750,000 today.
assert rows["2026-03-18"]["world"]["object"] == "500000", rows["2026-03-18"]
assert d["head"]["object"] == "750000", d["head"]
# ... and the endorsement cannot reach backwards past its own effective date.
assert rows["2026-03-18"]["world"]["valid_to"] is not None, "the old window never closed"

# (b) THE DIVERGENCE. On 20 May the higher limit was already in force in the
#     world -- and this desk had never heard of it. Two different answers to
#     two different questions, at one instant.
assert rows["2026-05-20"]["world"]["object"] == "750000", rows["2026-05-20"]
assert rows["2026-05-20"]["knowledge"] == {}, \
    "the desk cannot have known on 20 May about a document it received on 15 June: %r" \
    % rows["2026-05-20"]["knowledge"]

# (c) and by today the two clocks agree again.
assert rows["2026-08-01"]["world"]["object"] == "750000"
assert rows["2026-08-01"]["knowledge"]["object"] == "750000"
print("   date of loss 18 Mar   world 500,000   knowledge 500,000")
print("   20 May                world 750,000   knowledge -- NOTHING KNOWN")
print("   today                 world 750,000   knowledge 750,000")
EOF
echo "   a plain recall answers only the last line. A claim needs the first."

say "   and the correction runs the other way: retroactive in the world,"
echo "   dated in the knowledge"
DED=$($AGENT as-of POL-4471 mg:deductible 2026-03-18 2026-08-01)
python3 - <<EOF || exit 1
import json
d = json.loads('''$DED''')
rows = {r["at"]: r for r in d["rows"]}
# The deductible was mis-keyed at inception as 5,000; the desk learned on
# 20 June that it had always been 10,000. So on 18 March it was TRUE that the
# deductible was 10,000, and it was BELIEVED that it was 5,000.
assert rows["2026-03-18"]["world"]["object"] == "10000", rows["2026-03-18"]
assert rows["2026-03-18"]["knowledge"]["object"] == "5000", rows["2026-03-18"]
assert rows["2026-08-01"]["knowledge"]["object"] == "10000"
assert [h["object"] for h in d["history"]] == ["10000", "5000"], d["history"]
print("   18 Mar deductible    world 10,000    knowledge 5,000")
EOF
echo "   \"what we told the insured in March\" is a different question from"
echo "   \"what the policy said in March\", and the file answers both"

# -- 4. and the cancellation ends a window without deleting anything -------
say "4. POL-6103 was cancelled effective 1 July"
CAN=$($AGENT as-of POL-6103 mg:coverage_limit 2026-06-15 2026-07-15)
[ "$(echo "$CAN" | jget rows 0 world object)" = "400000" ] \
  || fail "POL-6103 should still respond to a June loss"
python3 - <<EOF || exit 1
import json
r = json.loads('''$CAN''')["rows"][1]
assert r["world"] == {}, "a July loss must find no cover: %r" % r["world"]
EOF
echo "   a June loss still finds 400,000; a July loss finds nothing. Nothing"
echo "   was deleted -- the window was closed."

# -- 5. the accumulation walk ----------------------------------------------
say "5. the entity graph: what else is this insured exposed on?"
EXP=$($AGENT exposure POL-4471 2026-03-18)
python3 - <<EOF || exit 1
import json
e = json.loads('''$EXP''')
assert e["insured"] == "Harbourline Freight Ltd", e["insured"]
own = sorted(p["policy"] for p in e["own_policies"])
assert own == ["POL-4471", "POL-5520", "POL-6103"], own
assert e["group_policies"] == ["POL-7714"], e["group_policies"]
assert e["aggregate_exposure"] == "1150000", e["aggregate_exposure"]

# What each direction actually sees. This is the honest part: "in" and "both"
# read the reverse index, which only covers relations the FILE declares
# entity-valued -- mg:owned_by and part_of are in that default set,
# mg:covers_peril is not.
w = e["walk"]
assert w["out_from_policy"] == ["Harbourline Freight Ltd", "Harbourline Group"], w
assert w["in_from_policy"] == [], w
assert set(w["both_from_policy"]) == {
    "Harbourline Freight Ltd", "Harbourline Group",
    "POL-5520", "POL-6103", "Marlowe Cold Chain Ltd", "POL-7714"}, w
assert w["out_on_non_entity_relation"] == ["fire", "flood", "theft"], w
assert w["in_on_non_entity_relation"] == [], \
    "a relation the file does not declare entity-valued has no reverse index"
print("   out  -> insured, group          (works on any relation)")
print("   in   -> nothing from a policy   (a policy is nobody's object)")
print("   both -> the whole group: %s" % ", ".join(sorted(w["both_from_policy"])))
EOF
echo "   aggregate on the date of loss: 1,150,000 across three policies"

# -- 6. the claim is parked, and on the RIGHT number -----------------------
say "6. the claim: 612,000, date of loss 18 March"
ASK=$($AGENT asks)
[ "$(echo "$ASK" | python3 -c 'import json,sys; print(len(json.load(sys.stdin)))')" = "1" ] \
  || fail "expected exactly one claim waiting on an underwriter"
python3 - <<EOF || exit 1
import json
a = json.loads('''$ASK''')[0]
assert a["claim_id"] == "CLM-8801", a
assert a["limit_in_force"] == "500000", \
    "the claim was assessed against the wrong limit: %r" % a
assert a["limit_now"] == "750000", a
assert a["uninsured_excess"] == "112000", a
assert a["accumulation_flag"] is True, a
print("   limit in force on 18 Mar 500,000 | limit today 750,000")
print("   uninsured excess 112,000 | accumulation flagged at 1,150,000")
EOF
echo "   had the desk used today's limit, the insured would have been told"
echo "   they were fully covered. They are 112,000 short."

# -- 7. the desk cannot sign its own determination -------------------------
say "7. the desk signs its own determination -- refused, structurally"
if $AGENT determine fixtures/determinations/00-desk-self-signs.json >/dev/null 2>&1; then
  fail "the principal that started the run was allowed to determine it"
fi
echo "   the starter cannot answer its own ask (separation of duties)"

say "   and a determination with no written reason is refused"
if $AGENT determine fixtures/determinations/01-no-reason.json >/dev/null 2>&1; then
  fail "an unreasoned coverage determination was issued"
fi
echo "   the insured may see this document; it needs a reason"

# -- 8. an underwriter determines cover, in writing -------------------------
say "8. nadia confirms cover -- at the limit that was in force, in writing"
OUT=$($AGENT determine fixtures/determinations/02-cover-confirmed.json)
[ "$(echo "$OUT" | jget settled_wording)" = "True" ] \
  || fail "the wording reading was not recorded"
python3 - "$DETS" <<'EOF' || exit 1
import json, sys
rows = [json.loads(l) for l in open(sys.argv[1])]
assert len(rows) == 1, rows
d = rows[0]
assert d["claim_id"] == "CLM-8801", d
assert d["determined_by"] == "user:nadia", d
assert d["authority"] == "underwriter", d
assert d["limit_in_force"] == "500000", d
assert d["deductible_in_force"] == "10000", \
    "the corrected deductible should apply -- it was retroactive: %r" % d
assert "18 March" in d["because"], d["because"]
assert d["cover_grain"], "the determination does not name the grain it relied on"
EOF
echo "   signed by user:nadia, against the 500,000 grain, with her reasoning"

# -- 9. what was it actually decided against? ------------------------------
say "9. the auditor's question: what did the desk have in front of it?"
TRACE=$($AGENT trace CLM-8801)
python3 - <<EOF || exit 1
import json
t = json.loads('''$TRACE''')
a = t["as_of_pinned_into_the_run"]
assert a, "the run journal does not carry the as-of read"
assert a["world"]["object"] == "500000", a
assert a["head"]["object"] == "750000", a
assert t["exposure_pinned_into_the_run"]["aggregate_exposure"] == "1150000", t
print("   the as-of read is IN the journal: world 500,000, head 750,000")
EOF
echo "   the determination is reproducible because its inputs were journalled"
echo "   before the run started -- not re-derived from a file that has moved"

printf '\n\033[32mOK\033[0m -- 1 determination signed at the limit in force, 1 refused for'
printf '\n     self-approval, 1 refused for having no reason, 2 clocks that disagree.\n'
