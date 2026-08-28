#!/bin/sh
# One diligence file, start to finish, with no credentials and no model key.
#
# An analyst files a request against a vendor. The desk works a checklist of
# research legs -- registry filings, litigation, adverse media, accounts --
# and research is open-ended, so it runs UNDER A CEILING.
#
# The property this example exists for: THE CEILING IS A CONTROL, NOT AN
# ERROR. When it is reached the run finishes `BudgetExhausted`, which is a
# TERMINAL STATE THAT IS RESUMABLE: the journal survives with the spend on
# it, the analyst reads what the money bought and what is still unread, and
# then decides -- fork it under a raised ceiling, or ship the partial
# report. Steps 2 to 5 are that act. Steps 9 and 10 prove the whole thing
# replays afterwards without touching the world.
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
FINDINGS="$AGENT_OUT/findings.jsonl"
REPORTS="$AGENT_OUT/reports.jsonl"
REVIEWS=fixtures/reviews

say()  { printf '\n\033[1m%s\033[0m\n' "$*"; }
fail() { echo "FAIL: $*" >&2; exit 1; }
jget() { python3 -c 'import json,sys; d=json.load(sys.stdin)
for k in sys.argv[1:]:
    d = d[int(k)] if isinstance(d, list) else d.get(k)
print(d)' "$@"; }
lines() { [ -f "$1" ] && wc -l < "$1" | tr -d ' ' || echo 0; }

rm -rf "$AGENT_OUT"

# -- 1. seed ---------------------------------------------------------------
say "1. seed: the plan, the checklist, the desk's own rules, the saved queries"
SEEDED=$($AGENT seed)
WF=$(echo "$SEEDED" | jget workflow)
echo "   workflow  $WF"
echo "   checklist $(echo "$SEEDED" | jget legs)"
echo "   ceiling   $(echo "$SEEDED" | jget ceiling_ms) ms per request"
echo "$WF" > "$AGENT_OUT/workflow.hash"
case "$WF" in
  ????????????????????????????????????????????????????????????????) : ;;
  *) fail "the plan hash is not a 64-hex content address: $WF" ;;
esac

# -- 2. the ceiling is reached ---------------------------------------------
say "2. dara files DD-2026-0114 against Vantridge Logistics Group"
FILED=$($AGENT file 01-vantridge.json)
echo "$FILED" | python3 -c 'import json,sys
r = json.load(sys.stdin)
print("   %s -> %s" % (r["run_id"], r["finished"]))
print("   read %s, unread %s, %s material findings"
      % (r["legs_read"], r["legs_unread"], r["material_findings"]))'

# The outcome is a Rust Debug rendering of the run-outcome enum. It is a
# human-readable label, not a stable token -- match a SUBSTRING of it.
case "$(echo "$FILED" | jget finished)" in
  *BudgetExhausted*) : ;;
  *) fail "the ceiling did not stop the run: $(echo "$FILED" | jget finished)" ;;
esac
case "$(echo "$FILED" | jget finished)" in
  *WallMs*) : ;;
  *) fail "the run stopped on the wrong budget axis" ;;
esac
echo "$FILED" | python3 -c 'import json,sys
r = json.load(sys.stdin)
assert 1 <= len(r["legs_read"]) < 4, \
    "the ceiling should buy part of the checklist, not none and not all: %r" % r
assert r["legs_unread"], "nothing was left unread -- the ceiling bought everything"
assert r["parked"] is False and r["phase"] == "finished", r' \
  || fail "the exhausted run is not in the shape an analyst can act on"

# -- 3. the journal survived, with the spend on it -------------------------
say "3. what the ceiling bought -- read off the run's own journal"
$AGENT inspect dd-2026-0114 > "$AGENT_OUT/exhausted.json"
python3 -c 'import json,sys
r = json.load(sys.stdin)
spent = r["spent"]
print("   ceiling %s ms   spent %s ms over %s supersteps"
      % (r["ceiling_ms"], spent["wall_ms"], spent["supersteps"]))
print("   %s checkpoints, %s journal entries -- none of it lost"
      % (r["checkpoints"], r["journal_entries"]))
assert r["ceiling_ms"] == 2900, r["ceiling_ms"]
assert spent["wall_ms"] >= r["ceiling_ms"], spent
assert r["checkpoints"] >= 2 and r["journal_entries"] >= 2, r
assert r["fork_of"] is None, r' < "$AGENT_OUT/exhausted.json"
[ "$(lines "$FINDINGS")" -gt 0 ] \
  || fail "the research the ceiling did pay for was not journaled"
[ ! -f "$REPORTS" ] || fail "a partial file left the building with no partner on it"
echo "   $(lines "$FINDINGS") findings on the ledger, and NO report issued"

# -- 4. asking again does not raise the ceiling ----------------------------
say "4. resuming the same run does not buy more research"
BEFORE=$(lines "$FINDINGS")
AGAIN=$($AGENT resume dd-2026-0114)
case "$(echo "$AGAIN" | jget finished)" in
  *BudgetExhausted*) : ;;
  *) fail "a resume moved past a ceiling nobody raised" ;;
esac
[ "$(lines "$FINDINGS")" = "$BEFORE" ] \
  || fail "a resume of an exhausted run pulled more records"
echo "   still BudgetExhausted, ledger unchanged -- the ceiling is on the"
echo "   run's frozen manifest, and only a fork gets its own knobs"

# -- 5. the analyst raises the ceiling, deliberately -----------------------
say "5. dara decides the file is worth more, and forks it"
FORKED=$($AGENT fork dd-2026-0114 dd-2026-0114-raised --as user:dara)
SEED=$(echo "$FORKED" | jget seed_checkpoint)
case "$SEED" in
  ????????????????????????????????????????????????????????????????) : ;;
  *) fail "the fork did not return a seed checkpoint: $SEED" ;;
esac
echo "   seed checkpoint $SEED"
[ "$(echo "$FORKED" | jget ceiling_ms)" = "None" ] \
  || fail "the fork did not get its own raised ceiling"

$AGENT resume dd-2026-0114-raised > "$AGENT_OUT/raised.json"
python3 - "$AGENT_OUT/exhausted.json" "$AGENT_OUT/raised.json" <<'EOF' || exit 1
import json, sys
base = json.load(open(sys.argv[1]))
fork = json.load(open(sys.argv[2]))
# It CONTINUED the exhausted run rather than restarting it: every leg the
# first ceiling paid for is already read, and it added the rest.
assert set(base["legs_read"]) < set(fork["legs_read"]), \
    "the fork did not continue the exhausted run's work (%r -> %r)" \
    % (base["legs_read"], fork["legs_read"])
assert not fork["legs_unread"], \
    "the raised ceiling did not finish the checklist: %r" % fork["legs_unread"]
# And it PARKED rather than issuing: the partner gate is mandatory, so
# finishing the research is not finishing the file.
assert fork["parked"] is True and fork["asks"] == 1, fork
assert fork["phase"] == "open", fork
print("   the first ceiling had read %s;" % base["legs_read"])
print("   the fork carried on and read %s"
      % [leg for leg in fork["legs_read"] if leg not in base["legs_read"]])
print("   %s material findings, and it PARKED on the partner gate -- research"
      % fork["material_findings"])
print("   finished is not file finished")
EOF
$AGENT inspect dd-2026-0114-raised | python3 -c 'import json,sys
r = json.load(sys.stdin)
assert r["fork_of"]["base_run"] == "dd-2026-0114", r["fork_of"]
print("   lineage %s <- superstep %s of %s"
      % (" <- ".join(r["lineage"]), r["fork_of"]["base_superstep"],
         r["fork_of"]["base_run"]))'

# -- 6. the analyst cannot sign off her own file ---------------------------
say "6. dara signs her own report out -- refused, structurally"
if $AGENT sign "$REVIEWS/00-analyst-signs-own.json" >/dev/null 2>"$AGENT_OUT/sod.err"; then
  fail "the principal that raised the ceiling was allowed to sign the report"
fi
grep -q 'RUN-E012' "$AGENT_OUT/sod.err" \
  || fail "the self-signature was refused for the wrong reason: $(cat "$AGENT_OUT/sod.err")"
echo "   RUN-E012 -- every client ask is an approval boundary, and the"
echo "   responder may not be the principal who triggered the run"
[ ! -f "$REPORTS" ] || fail "a report was issued by the refused signature"

# -- 7. and no report goes out unsigned ------------------------------------
say "7. a sign-off with no written reason -- refused"
if $AGENT sign "$REVIEWS/01-unsigned-issue.json" >/dev/null 2>&1; then
  fail "a report was signed out with no reason on it"
fi
echo "   a diligence report is not signed out without a written reason"

# -- 8. a partner signs it out ---------------------------------------------
say "8. priya signs it out, with her reason"
SIGNED=$($AGENT sign "$REVIEWS/02-vantridge-issue.json")
echo "$SIGNED" | python3 -c 'import json,sys
r = json.load(sys.stdin)
assert r["finished"] == "Completed", \
    "the signed run did not complete: %r" % r["finished"]
assert r["parked"] is False and r["asks"] == 0, r
assert not r["legs_unread"], r
' || fail "the run did not complete once the partner answered the gate"
python3 - "$REPORTS" <<'EOF' || fail "the report is not on the ledger under the partner's name"
import json, sys
rows = [json.loads(l) for l in open(sys.argv[1])]
assert len(rows) == 1, rows
r = rows[0]
assert r["outcome"] == "issued" and r["issued_by"] == "user:priya", r
assert len(r["material_findings"]) == 4, r
assert r["because"].strip(), "the reason did not travel with the report"
print("   issued: %s -- %s material findings, signed by %s"
      % (r["target"], len(r["material_findings"]), r["issued_by"]))
EOF

# -- 9. the record replays ------------------------------------------------
say "9. the examiner's first question: is the journal actually consistent?"
$AGENT verify dd-2026-0114 dd-2026-0114-raised | python3 -c 'import json,sys
rows = json.load(sys.stdin)
for r in rows:
    assert r["verified"] is True, r
    assert r["steps"] > 0, r
    print("   %-22s %s checkpoints re-derived and byte-compared"
          % (r["run_id"], r["steps"]))'

# -- 10. and replays without touching the world ----------------------------
say "10. and the second: can you replay it WITHOUT re-doing any of it?"
F_BEFORE=$(lines "$FINDINGS"); R_BEFORE=$(lines "$REPORTS")
$AGENT shadow dd-2026-0114 dd-2026-0114-raised | python3 -c 'import json,sys
r = json.load(sys.stdin)
assert r["all_consistent"] is True, r
assert r["effect_dispatches"] == 0, r
for run in r["runs"]:
    assert run["consistent"] is True and run["effects_replayed"] > 0, run
    print("   %-22s %s effects replayed from the journal"
          % (run["run_id"], run["effects_replayed"]))'
[ "$(lines "$FINDINGS")" = "$F_BEFORE" ] \
  || fail "the shadow replay pulled records again ($F_BEFORE -> $(lines "$FINDINGS"))"
[ "$(lines "$REPORTS")" = "$R_BEFORE" ] \
  || fail "the shadow replay issued a second report"
echo "   both ledgers unchanged -- zero effect dispatches, as the report claims"

# -- 11. the oversight report -----------------------------------------------
say "11. where a human sits in this run, measured rather than asserted"
$AGENT oversight dd-2026-0114-raised | python3 -c 'import json,sys
r = json.load(sys.stdin)
gates = r["human_gates"]
assert gates["every_client_ask_is_an_approval"] is True, gates
assert any(n["node"] == "partner_review" for n in gates["client_gated_nodes"]), gates
print("   gate: %s   %s"
      % (", ".join(n["node"] for n in gates["client_gated_nodes"]),
         gates["separation_of_duties"]))'

# -- 12. and what the partner learned is now memory -------------------------
say "12. priya's note on the file is a grain, not a comment"
BRIEF=$($AGENT brief)
echo "$BRIEF" | grep -q 'mg:low_yield_leg' \
  || fail "the partner's low-yield note did not become memory"
echo "$BRIEF" | grep -q 'adverse_media' \
  || fail "the note does not name the leg it is about"
echo "   regional-logistics / adverse_media -- act two spends the same"
echo "   ceiling knowing that"

printf '\n\033[32mOK\033[0m -- 1 ceiling reached, 1 fork, 1 report issued, 2 refusals, 2 runs replayed with 0 effects.\n'
