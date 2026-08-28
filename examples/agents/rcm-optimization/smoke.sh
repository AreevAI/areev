#!/bin/sh
# The first two payer remittances of the month, end to end, with no
# credentials and no model key.
#
# Eleven denied claims arrive across two files. The plan does not know
# there are eleven -- it cannot, because the count is a property of the
# remittance, not of the plan. So one node returns a `$send` list and the
# runtime spawns ONE screening task per denial, joins the batch, and folds
# the results through DECLARED REDUCERS. Then a billing lead approves one
# proposed fix and rejects the other, each with a written reason.
#
# The property this example exists for: DYNAMIC WIDTH THE PLAN DID NOT
# ENUMERATE. Step 4 asserts the merged shape; step 2 proves the one thing
# about reducers that will bite you -- they are validated at RUN START, not
# on write.
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
CLUSTERS="$AGENT_OUT/clusters.jsonl"
WORKLIST="$AGENT_OUT/worklist.jsonl"
REPORTS="$AGENT_OUT/reports.jsonl"

say()  { printf '\n\033[1m%s\033[0m\n' "$*"; }
fail() { echo "FAIL: $*" >&2; exit 1; }
jget() { python3 -c 'import json,sys; d=json.load(sys.stdin)
for k in sys.argv[1:]:
    d = d[int(k)] if isinstance(d, list) else d.get(k)
print(d)' "$@"; }

rm -rf "$AGENT_OUT"

# -- 1. seed ---------------------------------------------------------------
say "1. seed: the plan, its REDUCER TABLE, the tool definitions, the trigger"
SEEDED=$($AGENT seed)
WF=$(echo "$SEEDED" | jget workflow)
echo "   workflow $WF"
echo "$WF" > "$AGENT_OUT/workflow.hash"

# The reducer table AS STORED, not as authored. Every value must be a
# STRING naming a built-in -- that is the whole contract, and nothing on
# the write path checks it for you.
$AGENT plan > "$AGENT_OUT/plan.json"
python3 - "$AGENT_OUT/plan.json" <<'EOF' || fail "the stored reducer table is not what this plan needs"
import json, sys
p = json.load(open(sys.argv[1], encoding="utf-8"))
red = p["reducers"]
assert red == {"classified": "append", "denied_cents": "sum",
               "auto_classified": "sum", "unmapped": "sum"}, red
for key, name in red.items():
    assert isinstance(name, str), "%s: reducers take STRING names, got %r" % (key, name)
    assert name in ("lww", "append", "sum", "max", "min"), name
assert p["nodes"][0] == "split_denials", p["nodes"]
print("   reducers  " + ", ".join("%s=%s" % kv for kv in sorted(red.items())))
EOF

# -- 2. the reducer trap ---------------------------------------------------
say "2. a reducer that is an OBJECT stores cleanly -- and never runs"
PROBE=$($AGENT reducer-check)
[ "$(echo "$PROBE" | jget refused)" = "True" ] \
  || fail "a plan with an object-valued reducer was allowed to start"
case "$(echo "$PROBE" | jget written)" in
  ????????????????????????????????????????????????????????????????)
    echo "   the bad plan MINTED A CONTENT ADDRESS -- the write path never looked" ;;
  *) fail "the probe plan did not store" ;;
esac
echo "$PROBE" | grep -q 'RUN-E019' \
  || fail "expected RUN-E019 when the run resolved its manifest"
echo "$PROBE" | grep -q "is not a string" || fail "RUN-E019 did not name the cause"
echo "   RUN-E019 at run start: a plan you can save is not a plan you can run"

# -- 3. the feed seeds, then the two remittances arrive ---------------------
say "3. the first tick seeds the cursor; the second works the remittances"
STARTED=$($AGENT ingest | jget runs_started)
[ "$STARTED" = "0" ] || fail "first poll started $STARTED runs; it must seed only"
sleep 1.2

STARTED=$($AGENT ingest | jget runs_started)
[ "$STARTED" = "2" ] || fail "expected 2 runs started, got $STARTED"
echo "   2 remittances, 2 runs"

# -- 4. THE FAN-OUT --------------------------------------------------------
# The plan declares SIX nodes. This tick executed the classifier eleven
# times, because the remittances said so. Expectations come from the
# fixtures themselves -- the numbers below are read, not typed.
say "4. one screening task per denied claim, joined and reduced"
python3 - "$CLUSTERS" fixtures/remits/01-meridian-2026-07-31.json \
                      fixtures/remits/02-cascade-2026-07-31.json <<'EOF' || exit 1
import json, sys
seen = {}
for line in open(sys.argv[1], encoding="utf-8"):
    row = json.loads(line)
    seen[row["remit_id"]] = row
total_tasks = 0
for path in sys.argv[2:]:
    remit = json.load(open(path, encoding="utf-8"))
    got = seen.get(remit["remit_id"])
    assert got, "no cluster row for %s" % remit["remit_id"]
    n = len(remit["denials"])
    total_tasks += n
    # The array the `append` reducer built is exactly as long as the file.
    assert got["classified_count"] == n, \
        "%s: %d denials fanned out to %d results" % (
            remit["remit_id"], n, got["classified_count"])
    # ...and in SPAWN order, which is canonical merge order, not completion
    # order -- that is what makes the fold replayable.
    assert got["claim_order"] == [d["claim_id"] for d in remit["denials"]], \
        got["claim_order"]
    # The `sum` reducer closed over the same set, order-independently.
    assert got["denied_cents"] == sum(d["billed_cents"] for d in remit["denials"]), \
        "%s: %s" % (remit["remit_id"], got["denied_cents"])
    # Nothing was auto-classified: this desk has approved no mapping yet.
    assert got["auto_classified"] == 0, got
    assert got["unmapped"] == n, got
    assert got["actionable"] is True, got
    print("   %-16s %2d denials -> %2d tasks -> $%s, %d clusters, proposing %s"
          % (remit["remit_id"], n, got["classified_count"],
             format(got["denied_cents"] / 100, ",.2f"), len(got["clusters"]),
             got["proposal"]["denial_code"]))
assert total_tasks == 11, total_tasks
print("   %d tasks the plan never enumerated" % total_tasks)
EOF

# -- 5. the fold replays --------------------------------------------------
say "5. every run replays from its journal, byte for byte"
$AGENT verify > "$AGENT_OUT/verify.json"
python3 - "$AGENT_OUT/verify.json" <<'EOF' || fail "a run did not replay identically"
import json, sys
runs = json.load(open(sys.argv[1], encoding="utf-8"))
assert len(runs) == 2, runs
for r in runs:
    assert r["verified"] is True, r
print("   %d runs verified -- the reducers fold the same way twice" % len(runs))
EOF

# -- 6. the desk cannot approve its own batch ------------------------------
say "6. the desk approves its own proposal -- refused, structurally"
if $AGENT decide fixtures/decisions/00-desk-approves-itself.json >/dev/null 2>&1; then
  fail "the principal that started the run was allowed to approve it"
fi
echo "   the starter cannot answer its own gate (refused, as designed)"

if $AGENT decide fixtures/decisions/04-no-reason.json >/dev/null 2>&1; then
  fail "a fix was approved with no written reason"
fi
echo "   and a verdict with no written reason never reaches the run"

# -- 7. one fix approved, one rejected, both signed ------------------------
say "7. dana approves the Meridian fix; omar rejects the Cascade one"
$AGENT decide fixtures/decisions/01-meridian-prior-auth-approve.json | jget outcome finished >/dev/null
$AGENT decide fixtures/decisions/02-cascade-specificity-reject.json  | jget outcome finished >/dev/null

python3 - "$WORKLIST" "$REPORTS" <<'EOF' || exit 1
import json, sys
work = [json.loads(l) for l in open(sys.argv[1], encoding="utf-8")]
reports = [json.loads(l) for l in open(sys.argv[2], encoding="utf-8")]
# Only the approved cluster reached the resubmission worklist.
assert len(work) == 3, work
assert {w["claim_id"] for w in work} == {"CLM-88401", "CLM-88407", "CLM-88412"}, work
assert all(w["approved_by"] == "user:dana" for w in work), work
assert all(w["root_cause"] == "prior_auth_missing" for w in work), work
assert all(w["because"] for w in work), "a queued resubmission with no reason"
by = {r["remit_id"]: r for r in reports}
assert by["RA-MRD-20260731"]["decision"] == "approve", by["RA-MRD-20260731"]
assert by["RA-MRD-20260731"]["resubmissions_queued"] == 3
rej = by["RA-CAS-20260731"]
assert rej["decision"] == "reject" and rej["decided_by"] == "user:omar", rej
assert rej["resubmissions_queued"] == 0, "a rejected fix queued work anyway"
assert "over-fit" in rej["because"], rej["because"]
print("   3 claims queued under dana's name; omar's rejection queued nothing")
print("   because: %s" % rej["because"][:96])
EOF

# -- 8. redelivery is a no-op ----------------------------------------------
say "8. another tick: the same two remittances start nothing"
sleep 1.2
STARTED=$($AGENT ingest | jget runs_started)
[ "$STARTED" = "0" ] || fail "redelivered remittances started $STARTED runs; dedup must hold"

# -- 9. what the desk actually learned -------------------------------------
say "9. the approved mapping is now a fact -- the rejected one is not"
$AGENT mappings > "$AGENT_OUT/mappings.json"
python3 - "$AGENT_OUT/mappings.json" <<'EOF' || exit 1
import json, sys
rows = json.load(open(sys.argv[1], encoding="utf-8"))
assert len(rows) == 1, rows
assert rows[0]["subject"] == "PAYER-MRD/DN-311", rows[0]
assert rows[0]["object"] == "prior_auth_missing", rows[0]
print("   %s %s %s" % (rows[0]["subject"], rows[0]["relation"], rows[0]["object"]))
EOF
echo "   a rejection left nothing behind but the reason it was rejected"

printf '\n\033[32mOK\033[0m -- 11 tasks the plan did not enumerate, 2 runs verified, 1 fix approved, 1 rejected, 1 mapping learned.\n'
