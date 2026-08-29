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

printf '\n\033[32mOK\033[0m -- 20 denials over 4 remittances, 2 mappings learned, 6 resubmissions queued, 1 loop finding signed.\n'
