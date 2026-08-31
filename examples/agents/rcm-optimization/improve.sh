#!/bin/sh
# The second week of the same billing desk.
#
# smoke.sh is the desk doing its job under governance. This is what comes
# after: (a) the mapping a lead approved in week one now classifies week
# two's denials with nobody in the room, (b) the cluster the OTHER lead
# rejected comes back -- because a rejection is a reason, not a mapping --
# (c) the loop finds that repeat in the desk's own tool record and says so
# with the denials cited, and (d) a person decides, with a reason, and the
# desk acts on it.
#
# Same contract as smoke.sh: run it through a language wrapper that exports
# AGENT and AGENT_OUT. Exits non-zero on any drift.
set -eu
cd "$(dirname "$0")"

: "${AGENT:?run me through a language wrapper: python/improve.sh}"
: "${AGENT_OUT:?}"
CLUSTERS="$AGENT_OUT/clusters.jsonl"
WORKLIST="$AGENT_OUT/worklist.jsonl"

say()  { printf '\n\033[1m%s\033[0m\n' "$*"; }
fail() { echo "FAIL: $*" >&2; exit 1; }
jget() { python3 -c 'import json,sys; d=json.load(sys.stdin)
for k in sys.argv[1:]:
    d = d[int(k)] if isinstance(d, list) else d.get(k)
print(d)' "$@"; }

# Week one has to have happened -- this chapter reads its record.
if [ ! -d "$AGENT_OUT" ]; then
  say "0. week one first"
  "$(dirname "$AGENT_OUT")/smoke.sh" >/dev/null
  echo "   ran smoke.sh -- 11 denials, 1 fix approved, 1 rejected"
fi

# -- 1. the next two remittances -------------------------------------------
say "1. week two: both payers send another remittance"
sleep 1.2
STARTED=$(REMIT_UPTO=04 $AGENT ingest | jget runs_started)
[ "$STARTED" = "2" ] || fail "expected 2 new runs, got $STARTED"

# -- 2. the payoff of week one's decision ----------------------------------
say "2. what dana approved last week now classifies itself"
python3 - "$CLUSTERS" fixtures/remits/03-meridian-2026-08-07.json <<'EOF' || exit 1
import json, sys
rows = {}
for line in open(sys.argv[1], encoding="utf-8"):
    r = json.loads(line)
    rows[r["remit_id"]] = r
remit = json.load(open(sys.argv[2], encoding="utf-8"))
got = rows[remit["remit_id"]]
prior = sum(1 for d in remit["denials"] if d["denial_code"] == "DN-311")
assert got["classified_count"] == len(remit["denials"]), got
# The mapping came out of MEMORY, through the trigger's context query --
# not out of the crosswalk file, which suggested the same thing last week
# and was ignored until a lead signed it.
assert got["auto_classified"] == prior, \
    "expected %d auto-classified, got %s" % (prior, got["auto_classified"])
assert got["unmapped"] == len(remit["denials"]) - prior, got
# And so nobody was asked: the only open cluster is under the floor.
assert got["actionable"] is False, \
    "the desk asked for a decision it already had: %r" % got["proposal"]
print("   %s: %d of %d classified from the approved mapping, 0 gates"
      % (remit["remit_id"], got["auto_classified"], got["classified_count"]))
EOF

# -- 3. a rejection is not a mapping ---------------------------------------
say "3. the cluster omar rejected comes back, and parks again"
$AGENT asks > "$AGENT_OUT/asks.json"
python3 - "$AGENT_OUT/asks.json" <<'EOF' || exit 1
import json, sys
asks = json.load(open(sys.argv[1], encoding="utf-8"))
assert len(asks) == 1, "expected exactly one parked run, got %d" % len(asks)
a = asks[0]
assert a["remit_id"] == "RA-CAS-20260807", a
assert a["proposed_code"] == "DN-517", a
assert a["auto_classified"] == 0, \
    "a rejected proposal must not have become a mapping: %r" % a
print("   %s parked: %s again, %d claims, $%s"
      % (a["remit_id"], a["proposed_code"], a["proposed_claims"],
         format(a["proposed_cents"] / 100, ",.2f")))
EOF

# -- 4. the loop reads the desk's own record -------------------------------
# `--grant-auto-apply` hands the engine a HOST POLICY that grants this
# analyzer family auto-apply on memory targets up to high severity. Watch
# what it buys: nothing.
say "4. areev loop: deterministic analyzers over the desk's own tool record"
$AGENT improve --grant-auto-apply > "$AGENT_OUT/loop.json"
python3 - "$AGENT_OUT/loop.json" "$AGENT_OUT/rec.hash" <<'EOF' || fail "the loop found nothing"
import json, sys
d = json.load(open(sys.argv[1], encoding="utf-8"))
loop, pending = d["loop"], d["pending"]
assert loop["stored"] >= 1, loop
assert loop["auto_applied"] == 0, \
    "the host granted auto-apply and the engine took it: %r" % loop
hit = [r for r in pending
       if "cascade" in (r["summary"] or "") and "specificity" in (r["summary"] or "")]
assert hit, "the loop did not cluster the repeated cause: %r" % pending
print("   " + hit[0]["summary"])
print("   severity %s, from %s" % (hit[0]["severity"], hit[0]["analyzer"]))
open(sys.argv[2], "w", encoding="utf-8").write(hit[0]["hash"])
EOF
REC=$(cat "$AGENT_OUT/rec.hash")
[ -n "$REC" ] || fail "no recommendation hash"

# -- 5. what the loop is NOT allowed to do ---------------------------------
say "5. the gates"
echo "   a host policy granted auto-apply; the engine applied nothing --"
echo "   this analyzer's finding is free text it did not author, so its"
echo "   manifest is auto_apply: Never and the host cannot grant past it"

if $AGENT govern "$REC" approve --as user:omar >/dev/null 2>&1; then
  fail "a decision was recorded with no reason"
fi
echo "   a decision with no written reason never leaves the driver"

if REFUSAL=$($AGENT govern "$REC" approve --because "" --as user:omar 2>&1); then
  fail "an empty reason was accepted"
fi
echo "$REFUSAL" | grep -q 'LOP-E011' \
  || fail "expected LOP-E011 from the engine, got: $REFUSAL"
echo "   and an empty one is refused by the engine itself (LOP-E011)"

# -- 6. a person decides, and signs it -------------------------------------
say "6. a billing lead decides, and signs it"
# `apply` is approve-and-apply in one recorded act; `approve` on its own is
# the two-person variant, where a second principal applies later. Either
# way the reason is mandatory and the actor is the audit record.
$AGENT govern "$REC" apply \
  --because "second remittance, same cause -- that is the bar I set when I rejected it in week one. Keep the lesson on the tool so the next reviewer sees the history." \
  --as user:omar >/dev/null
echo "   approved and applied by user:omar -- the lesson is now in the memory"
$AGENT brief | grep -q 'fails_with' \
  || fail "the applied lesson is not recallable from the desk's own memory"

# -- 7. and acts on the batch that was waiting -----------------------------
say "7. omar approves the fix he rejected a week ago -- with the evidence"
$AGENT decide fixtures/decisions/03-cascade-specificity-approve.json | jget outcome finished >/dev/null
python3 - "$WORKLIST" <<'EOF' || exit 1
import json, sys
work = [json.loads(l) for l in open(sys.argv[1], encoding="utf-8")]
assert len(work) == 6, "expected 6 queued resubmissions in total, got %d" % len(work)
cas = [w for w in work if w["payer_id"] == "PAYER-CAS"]
assert len(cas) == 3, cas
assert {w["claim_id"] for w in cas} == {"CLM-52310", "CLM-52317", "CLM-52322"}, cas
assert all(w["root_cause"] == "dx_specificity" for w in cas), cas
assert all(w["approved_by"] == "user:omar" for w in cas), cas
assert all("pattern I asked to see held" in w["because"] for w in cas), cas
print("   3 more claims queued, $%s, each carrying omar's reason"
      % format(sum(w["billed_cents"] for w in cas) / 100, ",.2f"))
EOF

$AGENT mappings > "$AGENT_OUT/mappings.json"
python3 - "$AGENT_OUT/mappings.json" <<'EOF' || exit 1
import json, sys
rows = json.load(open(sys.argv[1], encoding="utf-8"))
by = {r["subject"]: r["object"] for r in rows}
assert by == {"PAYER-MRD/DN-311": "prior_auth_missing",
              "PAYER-CAS/DN-517": "dx_specificity"}, by
for k in sorted(by):
    print("   %s -> %s" % (k, by[k]))
EOF

# -- 8. the desk briefs itself ---------------------------------------------
say "8. the desk briefs itself out of its own memory"
BRIEF=$($AGENT brief)
echo "$BRIEF" | grep -q "rcm-denial-optimization" || fail "the briefing does not name the plan"
echo "$BRIEF" | grep -q "min_cluster_size" || fail "the briefing does not carry the desk's own policy"
echo "$BRIEF" | grep -q "prior_auth_missing" || fail "the briefing does not carry what it learned"
echo "   plan, tools, policy, mappings -- one saved query, one budget"

# -- 9. it does not nag ----------------------------------------------------
say "9. run the loop again"
$AGENT improve > "$AGENT_OUT/loop2.json"
python3 - "$AGENT_OUT/loop2.json" <<'EOF' || exit 1
import json, sys
d = json.load(open(sys.argv[1], encoding="utf-8"))
assert d["loop"]["deduped"] >= 1, d["loop"]
assert d["loop"]["stored"] == 0, "the same evidence became a second recommendation: %r" % d["loop"]
assert d["pending"] == [], d["pending"]
print("   deduped -- the same evidence does not become a second recommendation")
EOF

# -- 10. the desk proposes a change to HOW IT READS ITSELF -----------------
# Everything above evolves what the desk remembers. This evolves the CAL that
# turns memory into a prompt -- the briefing query itself. Keyless: the model
# leg is examples/llm/mock.py replaying a committed draft, so CI exercises the
# whole governed path (DISCOVER -> GROUND -> VERIFY -> review -> apply) with no
# key and no network. A real run points LOOP_LLM_CMD at a real backend.
say "10. a model reads the desk's record and proposes a change to the briefing query"
MOCK_LLM="$(cd ../../llm && pwd)/mock.py"
qsize() { $AGENT queries | python3 -c 'import json,sys
qs = json.load(sys.stdin)
print(next(q["body_size"] for q in qs if q["name"] == sys.argv[1]))' "$1"; }

BEFORE=$(qsize desk_pulse)
$AGENT brief | grep -q "rcm-reducer-probe" \
  || fail "the briefing was supposed to carry the probe before the revision"
LOOP_LLM_CMD="python3 $MOCK_LLM" \
  AREEV_MOCK_LLM_FIXTURE="$(pwd)/fixtures/llm/query-revision.json" \
  $AGENT improve --grant-llm-auto-apply > "$AGENT_OUT/loop3.json"
QREC=$(python3 - "$AGENT_OUT/loop3.json" <<'EOF'
import json, sys
d = json.load(open(sys.argv[1], encoding="utf-8"))
hits = [r for r in d["pending"] if r["target"] == "query:desk_pulse"]
assert len(hits) == 1, "expected one query revision, got %r" % d["pending"]
print(hits[0]["hash"])
EOF
) || fail "the model proposed no revision of the briefing query"
echo "   proposed: rewrite the saved query desk_pulse (origin llm, ${BEFORE}-byte body)"

# -- 11. what the engine will NOT do with it -------------------------------
# That run carried a host policy naming the `loop.llm` family AND the `query`
# class outright -- the widest grant a host could misconfigure. The engine
# applied nothing, for two independent reasons: `origin = llm` is
# categorically auto-apply-ineligible, and the auto-apply gate admits only
# the `memory` class, so the query leg of that grant is inert. A grain edit
# changes one remembered value; a definition rewrite changes what EVERY
# future briefing contains.
say "11. a host granted auto-apply on the query class -- and the engine ignored it"
STATUS=$($AGENT recommendation "$QREC" | jget status)
[ "$STATUS" = "pending" ] || fail "a model-authored rewrite auto-applied (status $STATUS)"
[ "$(qsize desk_pulse)" = "$BEFORE" ] || fail "the query body moved before anyone signed"
echo "   still pending, body still $BEFORE bytes -- the host cannot grant past the engine"

# -- 12. a person signs it, and the briefing changes -----------------------
# The binding's `apply` is ONE audited approve+apply step (the CLI splits the
# two verbs so a supervising agent can approve for a human to apply later).
# Either way the reason and the actor are the audit record.
say "12. omar signs it, and the desk's own briefing changes"
$AGENT govern "$QREC" apply \
  --because "the briefing is 40 dispatch records deep; narrow the activity leg and keep the mappings" \
  --as user:omar >/dev/null
AFTER=$(qsize desk_pulse)
[ "$AFTER" != "$BEFORE" ] || fail "the saved query body did not change ($BEFORE bytes)"
BRIEF=$($AGENT brief)
echo "$BRIEF" | grep -q "rcm-reducer-probe" \
  && fail "the briefing still presents the validation probe as a plan"
echo "$BRIEF" | grep -q "rcm-denial-optimization" || fail "the briefing lost the real plan"
echo "$BRIEF" | grep -q "prior_auth_missing" \
  || fail "the revised briefing dropped the mapping it exists to carry"
echo "$BRIEF" | grep -q "min_cluster_size" || fail "the revised briefing dropped the desk's policy"
echo "   the probe is gone; the plan, the policy and the learned mappings stayed"

# -- 13. and it is undoable ------------------------------------------------
# A DEFINE writes a registry row, not a grain, so the ordinary "retract what
# the apply created" would undo NOTHING while reporting success. The engine
# refuses to apply a definition change whose inverse it could not record --
# which is the only reason this step can exist.
say "13. and the rewrite can be taken back"
$AGENT govern "$QREC" rollback --because "week three wants the full activity trail back" \
  --as user:omar >/dev/null
[ "$(qsize desk_pulse)" = "$BEFORE" ] \
  || fail "rollback did not restore the previous definition ($BEFORE bytes)"
echo "   the previous definition is back, byte for byte -- the inverse was recorded at apply"

printf '\n\033[32mOK\033[0m -- 20 denials over 4 remittances, 2 mappings learned, 6 resubmissions queued, 1 loop finding signed, 1 briefing query rewritten and taken back.\n'
