#!/bin/sh
# Week two on the same referral desk.
#
# smoke.sh is the desk doing its job under governance. This is what comes
# after: (a) the correction a clinician signed in week one now applies by
# itself, (b) one practice keeps sending letters with no date of birth and
# the desk refuses them loudly, (c) the loop finds that cluster in the desk's
# own journals and a clinician decides what to do about it, and (d) THE
# HONEST PART -- a relative named once in prose walked straight past the
# Tier-0 floor, so the policy is tightened to demand a Tier-1 detector and
# the reads fail closed until the host installs one.
#
# (d) is the act no other example here can show: the detector chain is a
# declared, replicating property of the FILE, and the detector itself is a
# capability of the HOST. They are configured in different places on purpose.
#
# Same contract as smoke.sh: run it through a language wrapper that exports
# AGENT and AGENT_OUT. Exits non-zero on any drift.
set -eu
cd "$(dirname "$0")"

: "${AGENT:?run me through a language wrapper: python/improve.sh}"
: "${AGENT_OUT:?}"
WIRE="$AGENT_OUT/egress.jsonl"
LEDGER="$AGENT_OUT/clinic.jsonl"

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
  echo "   ran smoke.sh -- 3 referrals triaged, 3 clean outbound requests"
fi

export REF_UPTO=09

# -- 1. more referrals -----------------------------------------------------
say "1. week two: six more letters, three of them from one practice"
FILED=$($AGENT intake | python3 -c 'import json,sys; print(len(json.load(sys.stdin)["filed"]))')
[ "$FILED" = "6" ] || fail "expected 6 new referrals filed, got $FILED"
sleep 1.2
STARTED=$($AGENT ingest | jget runs_started)
[ "$STARTED" = "6" ] || fail "expected 6 new runs, got $STARTED"

# -- 2. the payoff of week one's correction --------------------------------
say "2. the rule asha wrote in week one now applies by itself"
$AGENT asks | python3 -c 'import json,sys
rows = {r["referral_id"]: r for r in json.load(sys.stdin)}
r = rows.get("REF-2204")
assert r, "REF-2204 never reached a clinician: %r" % sorted(rows)
assert r["proposed_urgency"] == "urgent", r
assert r["urgency_source"] == "clinic_rule", \
    "the desk took the outside suggestion again: %r" % r
assert "[PERSON_" in r["narrative"], "the review queue is carrying a real name"
print("   REF-2204, same complaint: proposed URGENT, source clinic_rule")
print("   the coding service still says routine -- the clinic overrules it")' \
  || fail "the signed correction did not become memory"

# -- 3. and the desk refuses what it cannot pin to a patient ---------------
say "3. three letters arrive with no date of birth"
FAILED=$($AGENT runs | python3 -c 'import json,sys
print(sum(1 for r in json.load(sys.stdin) if r["outcome"] == "failed"))')
[ "$FAILED" = "3" ] || fail "the incomplete referrals did not fail loudly ($FAILED)"
for REF in REF-2205 REF-2206 REF-2207; do
  grep -q "$REF" "$LEDGER" && fail "$REF was booked despite a missing identifier"
  grep -q "$REF" "$WIRE"   && fail "$REF was sent to the coding service anyway"
done
echo "   3 runs failed at the desk. Nothing was triaged, nothing was sent."
echo "   a referral triaged without an identifier is the expensive failure;"
echo "   a referral stopped at the desk is the cheap one"

# -- 4. clinicians sign the three that got through -------------------------
say "4. the three complete referrals are signed"
$AGENT review fixtures/reviews/04-vasquez-rey-confirm.json | python3 -c 'import json,sys
d = json.load(sys.stdin)
assert d["proposed_urgency"] == "urgent", d
assert d["corrected"] is False, \
    "a clinician had to correct the same thing twice: %r" % d
print("   REF-2204 confirmed, not corrected -- %s had nothing to change"
      % d["responder"])'
$AGENT review fixtures/reviews/08-nakamura-oyelowo-confirm.json | jget outcome finished >/dev/null
$AGENT review fixtures/reviews/09-perreault-routine.json       | jget outcome finished >/dev/null
python3 - "$LEDGER" <<'EOF' || fail "the week-two ledger is wrong"
import json, sys
rows = {r["referral_id"]: r for r in
        (json.loads(l) for l in open(sys.argv[1], encoding="utf-8"))}
assert len(rows) == 6, sorted(rows)
assert rows["REF-2204"]["urgency"] == "urgent", rows["REF-2204"]
assert rows["REF-2204"]["urgency_source"] == "clinic_rule", rows["REF-2204"]
assert rows["REF-2204"]["corrected"] is False, rows["REF-2204"]
assert rows["REF-2208"]["urgency_source"] == "clinic_rule", rows["REF-2208"]
assert rows["REF-2209"]["urgency_source"] == "external_service", rows["REF-2209"]
EOF
echo "   6 referrals on the ledger; 1 correction in week one, 0 in week two"

# -- 5. the desk briefs itself ---------------------------------------------
say "5. the desk briefs itself out of its own memory"
BRIEF=$($AGENT brief)
echo "$BRIEF" | grep -q "clinical-referral-triage" || fail "the briefing does not name the plan"
echo "$BRIEF" | grep -q "mg:required_identifier" || fail "the briefing does not carry the protocol"
for VALUE in "Marion Delacroix-Bell" "202-555-0142" "marion.delacroix@example.com"; do
  case "$BRIEF" in *"$VALUE"*) fail "the desk briefing carries $VALUE" ;; esac
done
echo "   plan, tools, protocol, activity -- and not one patient identifier,"
echo "   because the briefing query never reads the clinical namespace"

# -- 6. the loop reads the record ------------------------------------------
say "6. areev loop: deterministic analyzers over the desk's own journals"
IMPROVED=$($AGENT improve)
REC=$(echo "$IMPROVED" | jget pending 0 hash)
[ -n "$REC" ] && [ "$REC" != "None" ] || fail "the loop proposed nothing"
echo "$IMPROVED" | python3 -c 'import json,sys
d = json.load(sys.stdin)
top = d["pending"][0]
assert top["analyzer"] == "loop.run_outcome/1", top
assert "clinical-referral-triage" in top["target"] or "workflow" in top["target"], top
print("   " + top["summary"][:150])'

# -- 7. the gate -----------------------------------------------------------
say "7. what the loop is NOT allowed to do"
if $AGENT govern "$REC" apply --because "let the engine fix it" --as user:asha >/dev/null 2>&1; then
  fail "the engine applied its own advisory finding"
fi
echo "   the engine cannot execute its own advice -- it is advisory"
if $AGENT govern "$REC" approve --as user:asha >/dev/null 2>&1; then
  fail "a decision was recorded with no reason"
fi
echo "   a decision with no written reason is refused"

# -- 8. a clinician decides, and signs it ----------------------------------
say "8. a clinician decides, and signs it"
$AGENT govern "$REC" approve \
  --because "Bramblewood's referral template has no date-of-birth field, so every letter from them stops at the desk. The desk is right to refuse -- do not relax the check; the practice manager has been asked to fix the template and the three held letters go back to them." \
  --as user:asha >/dev/null
echo "   approved by user:asha, with the reason on the record"

# -- 9. THE HONEST PART ----------------------------------------------------
# Tier-0 detects shapes (dates, phones, emails, MRNs) and identities the
# memory already holds as subjects. A relative named once, in prose, is
# neither. She went out.
say "9. what the Tier-0 floor did NOT catch"
grep -q "Anneke Vos" "$WIRE" || fail "the example's own honesty case has drifted"
echo "   \"her daughter Anneke Vos\" is in the wire log for REF-2208."
echo "   she is not a date, a phone or an email, and the memory has never"
echo "   interned her as a subject -- so nothing was there to detect her."
echo "   Tier-0 is a FLOOR. This is what extending it looks like:"

# -- 10. the policy is tightened -------------------------------------------
say "10. the policy now DEMANDS a Tier-1 detector"
$AGENT harden "letters name relatives and carers in prose; Tier-0 cannot see them and one reached the coding service" \
  | python3 -c 'import json,sys
d = json.load(sys.stdin)
assert d["policy"]["detectors"] == ["tier0", "ner"], d["policy"]
declared = {p["ns"]: p["policy"] for p in d["declared"]}
assert list(declared) == ["org.clinic.referrals"], sorted(declared)
assert declared["org.clinic.referrals"]["detectors"] == ["tier0", "ner"], declared
assert declared["org.clinic.referrals"].get("because"), \
    "the tightening was recorded with no reason"
assert d["host_has_ner"] is False, "this host was not supposed to have one yet"
print("   detectors: tier0 + ner, and the reason rides in the policy itself")'

say "11. this host has no such detector -- so the read REFUSES"
$AGENT outbound REF-2208 > "$AGENT_OUT/refused.json" && \
  fail "a read served the identifiers after the policy demanded a detector"
python3 - "$AGENT_OUT/refused.json" <<'EOF' || fail "the refusal is not fail-closed"
import json, sys
d = json.load(open(sys.argv[1], encoding="utf-8"))
assert d["refused"] is True, d
assert "VAL-E001" in d["error"], d["error"]
assert "fails closed" in d["error"], d["error"]
EOF
echo "   VAL-E001 -- the policy is a file truth and it travels with the file;"
echo "   the detector is a HOST capability and it does not. A host that"
echo "   cannot honour the policy serves nothing, rather than serving raw."

# -- 12. the host installs one ---------------------------------------------
say "12. the host installs a Tier-1 detector (set_anonymizer_command)"
CLINIC_NER=1 $AGENT outbound REF-2208 | python3 -c 'import json,sys
d = json.load(sys.stdin)
assert "Anneke Vos" not in d["narrative"], d["narrative"]
assert d["narrative"].count("[PERSON_") == 3, d["narrative"]
print("   " + d["narrative"][:100] + "...")
print("   " + d["narrative"][100:200])'
echo "   the daughter is a placeholder now too -- same policy, better chain"

# -- 13. and the record itself never moved ---------------------------------
say "13. the identified record is exactly where it always was"
CLINIC_NER=1 $AGENT reveal REF-2208 user:asha | python3 -c 'import json,sys
d = json.load(sys.stdin)
values = set(d["revealed"].values())
for want in ("Yumiko Nakamura-Oyelowo", "1948-02-27", "202-555-0150", "Anneke Vos"):
    assert want in values, "%r is not in the memory any more: %r" % (want, values)
print("   %d tokens, including the relative the new detector found" % len(values))'
CLINIC_NER=1 $AGENT letter REF-2208 user:asha | python3 -c 'import json,sys
d = json.load(sys.stdin)
assert d["unmatched"] == [], d
print("   and the acknowledgement letter still rehydrates cleanly (%d values)"
      % d["replaced"])'
grep -qF "Yumiko Nakamura-Oyelowo" "$AGENT_OUT/letters/REF-2208.txt" \
  || fail "the letter lost the patient"
grep -qF "Anneke Vos" "$AGENT_OUT/letters/REF-2208.txt" \
  || fail "the letter lost the relative"

printf '\n\033[32mOK\033[0m -- 1 signed correction became a rule, 3 incomplete referrals refused,\n'
printf '     1 loop finding approved with a reason, 1 detector chain extended.\n'
