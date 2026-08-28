#!/bin/sh
# The rest of the quarter on the same diligence desk.
#
# smoke.sh is one file under a ceiling. This is what comes after:
#
#   (a) the SAME ceiling now buys better research, because the note a
#       partner signed in act one changed the order the checklist is
#       worked in -- three material findings instead of one, for the same
#       money;
#   (b) the routine files run, and two of them die on the same leg for the
#       same reason;
#   (c) the loop reads the desk's own journals and flags the cluster --
#       and stops there, because diagnosing it is a person's job;
#   (d) a partner diagnoses it, approves with a written reason, and adopts
#       a desk rule that CITES that approval;
#   (e) the re-filed request reaches the partner's desk with the unread leg
#       written into the report as a gap, instead of dying on it.
#
# Same contract as smoke.sh: run it through a language wrapper that exports
# AGENT and AGENT_OUT. Exits non-zero on any drift.
set -eu
cd "$(dirname "$0")"

: "${AGENT:?run me through a language wrapper: python/improve.sh}"
: "${AGENT_OUT:?}"
REPORTS="$AGENT_OUT/reports.jsonl"
REVIEWS=fixtures/reviews

say()  { printf '\n\033[1m%s\033[0m\n' "$*"; }
fail() { echo "FAIL: $*" >&2; exit 1; }
jget() { python3 -c 'import json,sys; d=json.load(sys.stdin)
for k in sys.argv[1:]:
    d = d[int(k)] if isinstance(d, list) else d.get(k)
print(d)' "$@"; }

# Act one has to have happened -- this chapter reads its journals. And act
# TWO has to not have happened: it files DD-2026-0231, adopts a desk rule
# and re-files, none of which is a no-op the second time. So a repeat run
# resets to a fresh act one rather than half-failing on its own leftovers.
NEED_ACT_ONE=""
if [ ! -d "$AGENT_OUT" ]; then
  NEED_ACT_ONE="act one has not run"
elif $AGENT runs 2>/dev/null | grep -q 'dd-2026-0231'; then
  NEED_ACT_ONE="act two has already run here"
fi
if [ -n "$NEED_ACT_ONE" ]; then
  say "0. $NEED_ACT_ONE -- (re)running act one for a clean base"
  "$(dirname "$AGENT_OUT")/smoke.sh" >/dev/null \
    || fail "act one does not pass, so act two cannot be trusted"
  echo "   ran smoke.sh -- 1 ceiling reached, 1 fork, 1 report issued"
fi

# -- 1. the same ceiling, spent better -------------------------------------
say "1. dara files DD-2026-0231 against Halcyon Freightworks"
echo "   same sector as act one, same ceiling, same four legs available"
FILED=$($AGENT file 02-halcyon.json)
case "$(echo "$FILED" | jget finished)" in
  *BudgetExhausted*) : ;;
  *) fail "the ceiling did not bite the second time: $(echo "$FILED" | jget finished)" ;;
esac
echo "$FILED" | python3 -c 'import json,sys
r = json.load(sys.stdin)
print("   %s -> %s" % (r["run_id"], r["finished"]))
print("   read %s (%s material)" % (r["legs_read"], r["material_findings"]))'

# The payoff: the order came out of memory, not out of the procedure manual.
$AGENT inspect dd-2026-0114 > "$AGENT_OUT/act1.json"
$AGENT inspect dd-2026-0231 > "$AGENT_OUT/act2.json"
python3 - "$AGENT_OUT/act1.json" "$AGENT_OUT/act2.json" <<'EOF' || exit 1
import json, sys
a = json.load(open(sys.argv[1]))
b = json.load(open(sys.argv[2]))
assert a["ceiling_ms"] == b["ceiling_ms"], (a["ceiling_ms"], b["ceiling_ms"])
assert "adverse_media" == a["legs_read"][0], \
    "act one should have started with the procedure manual's order: %r" % a["legs_read"]
assert "adverse_media" not in b["legs_read"], \
    "the demoted leg was still read inside the ceiling: %r" % b["legs_read"]
assert b["legs_read"][0] == "corporate_filings", b["legs_read"]
assert len(a["legs_read"]) == len(b["legs_read"]), \
    "the two ceilings did not buy the same number of legs (%r vs %r)" \
    % (a["legs_read"], b["legs_read"])
assert b["material_findings"] > a["material_findings"], \
    "the same ceiling did not buy more material findings (%s vs %s)" \
    % (a["material_findings"], b["material_findings"])
print("   %s ms bought %s legs both times -- %s material findings in act one,"
      % (a["ceiling_ms"], len(a["legs_read"]), a["material_findings"]))
print("   %s in act two. The partner's note moved adverse_media to the back"
      % b["material_findings"])
print("   of the queue; nobody dropped it, and the fork still reads it.")
EOF

# -- 2. the file is finished and signed out --------------------------------
say "2. dara raises the ceiling, priya signs it out"
$AGENT fork dd-2026-0231 dd-2026-0231-raised --as user:dara >/dev/null
$AGENT resume dd-2026-0231-raised >/dev/null
SIGNED=$($AGENT sign "$REVIEWS/03-halcyon-issue.json")
[ "$(echo "$SIGNED" | jget finished)" = "Completed" ] \
  || fail "the second file did not complete"
python3 - "$REPORTS" <<'EOF' || fail "the second report is not on the ledger"
import json, sys
rows = [json.loads(l) for l in open(sys.argv[1])]
r = [x for x in rows if x["request_id"] == "DD-2026-0231"]
assert r and r[0]["issued_by"] == "user:priya", r
assert "adverse_media" in r[0]["legs_read"], \
    "the demoted leg was never read at all: %r" % r[0]
print("   issued: %s, all four legs read once the ceiling came off"
      % r[0]["target"])
EOF

# -- 3. the routine files ---------------------------------------------------
say "3. the rest of the quarter's book -- no ceiling, these are routine"
BOOK=$(DD_LEG_MS=0 $AGENT book --upto 05 --as user:dara)
echo "$BOOK" | python3 -c 'import json,sys
rows = json.load(sys.stdin)
failed = [r for r in rows if (r.get("finished") or "").startswith("Failed")]
assert len(failed) == 2, "expected 2 failures in the book, got %r" % \
    [(r["run_id"], r.get("finished")) for r in rows]
for r in failed:
    assert "research" in r["finished"], r
    assert "filing window" in r["finished"], r
    assert "financials" in r["legs_unread"], r
    print("   %s FAILED on the research leg -- financials, unread" % r["run_id"])
print("   a leg the desk has no rule for stops the file. It does not get")
print("   quietly written up as \"nothing found\".")'
grep -q 'DD-2026-0244' "$REPORTS" && fail "a failed file was reported anyway"
grep -q 'DD-2026-0252' "$REPORTS" && fail "a failed file was reported anyway"

# -- 4. the desk briefs itself ---------------------------------------------
say "4. the desk briefs itself out of its own memory"
BRIEF=$($AGENT brief)
echo "$BRIEF" | grep -q "due-diligence" || fail "the briefing does not name the plan"
echo "$BRIEF" | grep -q "leg_order" || fail "the briefing does not carry the checklist"
echo "$BRIEF" | grep -q "mg:low_yield_leg" || fail "the briefing lost the partner's note"
echo "   plan, checklist, judgment, lessons -- one saved query, one budget"

# -- 5. the loop reads the record ------------------------------------------
say "5. areev loop: deterministic analyzers over the desk's own journals"
IMPROVED=$($AGENT improve)
REC=$(echo "$IMPROVED" | python3 -c 'import json,sys
recs = [r for r in json.load(sys.stdin)["pending"]
        if r["analyzer"] == "loop.run_outcome/1"]
assert recs, "the loop did not flag the failure cluster"
r = recs[0]
assert r["severity"] in ("high", "critical"), r
print(r["hash"])')
[ -n "$REC" ] || fail "the loop proposed nothing"
echo "$IMPROVED" | python3 -c 'import json,sys
for r in json.load(sys.stdin)["pending"]:
    if r["analyzer"] == "loop.run_outcome/1":
        print("   %s" % r["summary"])'
echo "   -- and that is ALL it says. It found the cluster; it did not"
echo "   diagnose it. The evidence is in the desk's own run list:"
$AGENT runs | python3 -c 'import json,sys
rows = [r for r in json.load(sys.stdin) if r["outcome"] == "failed"]
assert len(rows) >= 2, rows
for r in rows:
    detail = (r["detail"] or "").replace(
        "ExecutorError: tool command exited with exit status: 7: ", "")
    if len(detail) > 84:
        detail = detail[:detail.rfind(" ", 0, 84)] + " ..."
    print("     %-14s %s" % (r["run_id"], detail))'

# -- 6. the gates ----------------------------------------------------------
say "6. what the loop is NOT allowed to do"
if $AGENT govern "$REC" apply --because "let the engine fix it" --as user:priya \
     >/dev/null 2>&1; then
  fail "the engine applied its own advisory finding"
fi
echo "   the engine cannot execute its own advice -- it is advisory"
if $AGENT govern "$REC" approve --as user:priya >/dev/null 2>&1; then
  fail "a decision was recorded with no reason"
fi
echo "   a decision with no written reason is refused"

# -- 7. and what a person is not allowed to do either ----------------------
say "7. a desk rule that cites nothing -- refused"
if $AGENT adopt "$REC" --policy Ravensmoor=financials --as user:priya \
     >/dev/null 2>&1; then
  fail "a standing desk rule was written from an unapproved finding"
fi
echo "   a rule must cite a finding a named person has APPROVED, so the"
echo "   rule and the reason for it stay one record"

# -- 8. a partner diagnoses it, and signs the diagnosis --------------------
say "8. priya reads the journals, decides what the cluster means, and signs it"
$AGENT govern "$REC" approve --as user:priya --because \
  "Two of the four non-completions are one thing: Ravensmoor does not publish FY25 accounts until Q3, so the financials leg has nothing to read and takes the whole file down with it. Record an unpublished leg in that jurisdiction as a gap in the report, not a research failure. The two budget stops are the ceiling working as designed and need no change." \
  >/dev/null
echo "   approved by user:priya, with the diagnosis attached"

say "9. and adopts the rule that approval licenses"
ADOPTED=$($AGENT adopt "$REC" --policy Ravensmoor=financials --as user:priya)
echo "   $(echo "$ADOPTED" | jget rule)"
[ "$(echo "$ADOPTED" | jget by)" = "user:priya" ] || fail "the rule has no author"

# -- 10. and the re-filed request gets through -----------------------------
say "10. the same target, re-filed"
REFILED=$(DD_LEG_MS=0 $AGENT book --upto 06 --as user:dara)
echo "$REFILED" | python3 -c 'import json,sys
rows = [r for r in json.load(sys.stdin) if r["request_id"] == "DD-2026-0339"]
assert rows, "the re-file did not run"
r = rows[0]
assert r["parked"] is True, "it did not reach the partner: %r" % r
assert not r["legs_unread"], r
assert "financials" in r["gaps"], \
    "the unread leg was not recorded as a gap: %r" % r
print("   %s reached the partner with %s material findings and the"
      % (r["run_id"], r["material_findings"]))
print("   financials leg written in as a GAP, not silently missing")'
$AGENT asks | python3 -c 'import json,sys
rows = [r for r in json.load(sys.stdin) if r["request_id"] == "DD-2026-0339"]
assert rows and rows[0]["gaps"], rows
print("   the gap the partner will see:")
print("     %s" % rows[0]["gaps"][0])'

# -- 11. it does not nag ---------------------------------------------------
say "11. run the loop again"
PENDING=$($AGENT improve | python3 -c 'import json,sys
print(len([r for r in json.load(sys.stdin)["pending"]
           if r["analyzer"] == "loop.run_outcome/1"]))')
[ "$PENDING" = "0" ] || fail "the same finding was proposed twice"
echo "   deduped -- the same evidence does not become a second recommendation"

GAIN=$(python3 - "$AGENT_OUT/act1.json" "$AGENT_OUT/act2.json" <<'EOF'
import json, sys
a = json.load(open(sys.argv[1]))
b = json.load(open(sys.argv[2]))
print("%s -> %s material findings for the same %s ms"
      % (a["material_findings"], b["material_findings"], a["ceiling_ms"]))
EOF
)
printf '\n\033[32mOK\033[0m -- same ceiling, %s; 1 cluster found, 1 diagnosis signed, 1 desk rule adopted.\n' "$GAIN"
