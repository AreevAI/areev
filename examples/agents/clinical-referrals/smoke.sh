#!/bin/sh
# A week on the referral desk of a specialist chest-pain clinic, end to end,
# with no credentials, no network and no model key.
#
# Three referral letters arrive. The desk reads them, asks an outside coding
# service for a code and a triage suggestion, and parks every one of them for
# a clinician to sign. One clinician corrects the suggestion and says why;
# another redirects a referral that does not belong here.
#
# The property this example exists for: PSEUDONYMIZATION ON EGRESS. The
# memory holds the identified record -- name, date of birth, MRN, phone,
# email. What LEAVES it is typed placeholders. Step 5 captures the exact
# bytes the outside service received and proves not one identifier is in
# them, while step 6 shows the same memory still resolving the real values
# for the clinician who is allowed to see them.
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
WIRE="$AGENT_OUT/egress.jsonl"     # what left the clinic, verbatim
LEDGER="$AGENT_OUT/clinic.jsonl"   # accepted + redirected referrals

say()  { printf '\n\033[1m%s\033[0m\n' "$*"; }
fail() { echo "FAIL: $*" >&2; exit 1; }
jget() { python3 -c 'import json,sys; d=json.load(sys.stdin)
for k in sys.argv[1:]:
    d = d[int(k)] if isinstance(d, list) else d.get(k)
print(d)' "$@"; }

rm -rf "$AGENT_OUT"

# -- 1. seed ---------------------------------------------------------------
say "1. seed: the plan, the clinic's protocol, THE POLICY, the inbox trigger"
SEEDED=$($AGENT seed)
WF=$(echo "$SEEDED" | jget workflow)
echo "$WF" > "$AGENT_OUT/workflow.hash"
echo "   workflow $WF"
$AGENT policies | python3 -c 'import json,sys
p = json.load(sys.stdin)
ns = sorted(row["ns"] for row in p["declared"])
assert ns == ["org.clinic.referrals"], ns
mode = p["declared"][0]["policy"]["mode"]
assert mode == "egress", mode
assert p["floor"] is False, "the host floor should be off by default"
print("   policy on %s: mode=%s" % (ns[0], mode))
print("   NO policy on org.ops or org.clinic.protocol -- deliberate (step 9)")' \
  || fail "the anonymization policy is not declared where it must be"

# -- 2. intake -------------------------------------------------------------
say "2. three letters are filed -- IDENTIFIED. egress rewrites reads, not writes"
FILED=$($AGENT intake | python3 -c 'import json,sys; print(len(json.load(sys.stdin)["filed"]))')
[ "$FILED" = "3" ] || fail "expected 3 referrals filed, got $FILED"
echo "   REF-2201, REF-2202, REF-2203 -- name, DOB, MRN, phone, email, letter"

# -- 3. what the memory hands back -----------------------------------------
say "3. the same referral, read back out of the memory"
$AGENT outbound REF-2201 | python3 -c 'import json,sys
d = json.load(sys.stdin)
got = set(d["placeholders"])
want = {"[PERSON_1]", "[DATE_1]", "[MRN_1]", "[PHONE_1]", "[EMAIL_1]"}
assert want <= got, "missing placeholders: %r" % (want - got)
assert d["anonymized"]["namespaces"] == ["org.clinic.referrals"], d["anonymized"]
print("   " + d["narrative"][:96] + "...")
print("   %d placeholders, one live mapping, held in this process only"
      % len(got))' || fail "the clinical namespace is not being pseudonymized on read"

# -- 4. the ticks ----------------------------------------------------------
say "4. the inbox trigger: the first tick seeds, the second triages"
STARTED=$($AGENT ingest | jget runs_started)
[ "$STARTED" = "0" ] || fail "first poll started $STARTED runs; it must seed only"
sleep 1.2
STARTED=$($AGENT ingest | jget runs_started)
[ "$STARTED" = "3" ] || fail "expected 3 runs started, got $STARTED"
[ -s "$WIRE" ] || fail "nothing was sent to the coding service"

# -- 5. THE CENTREPIECE ----------------------------------------------------
# The coding service is a separate process outside the clinic's trust
# boundary. `out/egress.jsonl` is the wire log: exactly what it received and
# exactly what it answered. Every identifier in every fixture that actually
# went out is checked against it, so a new fixture cannot quietly weaken this.
say "5. what the outside service actually received"
python3 - "$WIRE" fixtures/referrals <<'EOF' || fail "an identifier left the clinic"
import json, os, sys
wire = open(sys.argv[1], encoding="utf-8").read()
leaks, audited = [], []
for name in sorted(os.listdir(sys.argv[2])):
    ref = json.load(open(os.path.join(sys.argv[2], name), encoding="utf-8"))
    if '"%s"' % ref["referral_id"] not in wire:
        continue                      # this one has not gone out yet
    audited.append(ref["referral_id"])
    p = ref["patient"]
    for label, value in (("patient name", p["name"]),
                         ("date of birth", p.get("dob")),
                         ("MRN", p["mrn"].split()[-1]),
                         ("phone", p["phone"]),
                         ("email", p["email"]),
                         ("referring GP", ref["referring_clinician"])):
        if value and value in wire:
            leaks.append((ref["referral_id"], label, value))
assert audited, "the wire log named no referral at all"
assert not leaks, "identifiers reached the outside service: %r" % leaks
rows = [json.loads(l) for l in open(sys.argv[1], encoding="utf-8")]
assert all("[PERSON_" in r["sent"]["narrative"] for r in rows), \
    "a request went out with no pseudonym in it at all"
assert all(r["received"]["service"] == "claritycode-mock" for r in rows), rows
print("   %d referrals audited: %s" % (len(audited), ", ".join(audited)))
print("   0 names, 0 dates of birth, 0 MRNs, 0 phone numbers, 0 emails")
print("   what it got: " + rows[0]["sent"]["narrative"][:88] + "...")
EOF

say "6. and the same memory still resolves them, for someone allowed to look"
$AGENT reveal REF-2201 user:asha | python3 -c 'import json,sys
d = json.load(sys.stdin)
values = set(d["revealed"].values())
for want in ("Marion Delacroix-Bell", "1971-04-02", "202-555-0142",
             "marion.delacroix@example.com"):
    assert want in values, "%r did not come back: %r" % (want, d["revealed"])
audit = [a for a in d["audit"] if a.get("context", {}).get("verb") == "reveal"]
assert audit, "an admin-gated reveal left no audit record"
ctx = audit[0]["context"]
assert ctx.get("audit") == "tier2", ctx
assert ctx.get("target") == "org.clinic.referrals", ctx
assert len(ctx.get("revealed_fingerprints") or []) == len(d["tokens"]), ctx
blob = json.dumps(audit[0])
for secret in ("Marion Delacroix-Bell", "202-555-0142"):
    assert secret not in blob, "the audit record names the identity it un-masked"
print("   %d tokens reversed for user:asha" % len(d["revealed"]))
print("   the Tier-2 audit row names fingerprints, never the identity")' \
  || fail "the reveal path or its audit record is wrong"

# -- 7. the gate -----------------------------------------------------------
say "7. the desk signs its own triage -- refused, structurally"
if $AGENT review fixtures/reviews/00-desk-signs-its-own.json >/dev/null 2>&1; then
  fail "the principal that started the run was allowed to sign it"
fi
echo "   the starter cannot approve its own run (RUN-E012, as designed)"
[ ! -f "$LEDGER" ] || fail "a referral was booked before any clinician acted"

# -- 8. clinicians decide, and sign ----------------------------------------
say "8. asha overrules the coding service; tobias sends one somewhere else"
$AGENT review fixtures/reviews/01-delacroix-bell-urgent.json | python3 -c 'import json,sys
d = json.load(sys.stdin)
assert d["proposed_urgency"] == "routine", d
assert d["signed_urgency"] == "urgent", d
assert d["corrected"] is True, d
print("   REF-2201 routine -> urgent, corrected by %s" % d["responder"])'
$AGENT review fixtures/reviews/02-okonkwo-routine.json  | jget outcome finished >/dev/null
$AGENT review fixtures/reviews/03-thanachart-redirect.json | jget outcome finished >/dev/null
python3 - "$LEDGER" <<'EOF' || fail "the ledger does not match the decisions"
import json, sys
rows = {r["referral_id"]: r for r in
        (json.loads(l) for l in open(sys.argv[1], encoding="utf-8"))}
assert set(rows) == {"REF-2201", "REF-2202", "REF-2203"}, sorted(rows)
assert rows["REF-2201"] == dict(rows["REF-2201"], route="accepted",
                                urgency="urgent", urgency_source="clinician",
                                corrected=True, decided_by="user:asha")
assert rows["REF-2202"]["urgency_source"] == "external_service", rows["REF-2202"]
assert rows["REF-2203"]["route"] == "redirected", rows["REF-2203"]
assert rows["REF-2203"]["redirect_to"] == "general cardiology outpatients"
assert rows["REF-2203"]["decided_by"] == "user:tobias", rows["REF-2203"]
EOF
echo "   2 accepted, 1 redirected, every row signed by the clinician who decided"

# -- 9. the clinician's letter ---------------------------------------------
say "9. the acknowledgement letter: the real values, put back in-process"
$AGENT letter REF-2201 user:asha | python3 -c 'import json,sys
d = json.load(sys.stdin)
assert d["replaced"] == 6, d
assert d["unmatched"] == [], d
print("   %d placeholders restored, 0 unmatched -> %s" % (d["replaced"], d["path"]))'
for VALUE in "Marion Delacroix-Bell" "1971-04-02" "202-555-0142" \
             "marion.delacroix@example.com" "Priya Ramanathan"; do
  grep -qF "$VALUE" "$AGENT_OUT/letters/REF-2201.txt" \
    || fail "rehydrate_text did not restore $VALUE"
done
grep -q '\[PERSON_' "$AGENT_OUT/letters/REF-2201.txt" \
  && fail "the letter still carries a placeholder"
echo "   the mapping never left this process -- nothing was looked up remotely"

# -- 10. why the operational namespaces have no policy ---------------------
say "10. the org.ops lesson: an egress rewriter is for what LEAVES"
$AGENT policy-drill | python3 -c 'import json,sys
d = json.load(sys.stdin)
before = {tuple(r) for r in d["before"]}
during = {tuple(r) for r in d["during"]}
assert d["rows"] >= 8, d["rows"]
assert before == {tuple(r) for r in d["after"]}, "the drill did not clean up"
assert not (before & during), "the drill proved nothing -- nothing was rewritten"
mangled = [r for r in d["during"] if r[0].startswith("[PERSON_")]
assert len(mangled) == d["rows"], d["during"]
assert d["hashes_stable"] is True, "the same grains did not come back"
assert [p for p in d["still_declared"]] == ["org.clinic.referrals"], d
print("   with a policy on it, every protocol rule comes back as:")
for row in sorted(d["during"])[:2]:
    print("     %s %s %s" % tuple(row))
print("   the desk reads those rules back as INPUT -- it would find none of")
print("   them. The FILE was never touched: same %d grains, same hashes."
      % d["rows"])' || fail "the policy drill did not demonstrate the hazard"

# -- 11. the host floor is a cap, not a policy -----------------------------
say "11. set_anonymize_egress_floor is a HOST cap, and is never persisted"
$AGENT floor-check | python3 -c 'import json,sys
d = json.load(sys.stdin)
assert d["before"]["floor"] is False, d
assert d["with_floor"]["floor"] is True, d
assert all(s.startswith("[PERSON_") for s in d["with_floor"]["subjects"]), d
assert d["reopened"] == d["before"], "the host cap survived a reopen"
print("   on:  every policy-less namespace is covered too")
print("   off: reopening the file forgets it. A cap you can forget to set")
print("        is not a policy -- which is why the clinic declares one.")' \
  || fail "the host floor does not behave as a host cap"

# -- 12. redelivery is a no-op ---------------------------------------------
say "12. another tick: the same three referrals start nothing"
sleep 1.2
STARTED=$($AGENT ingest | jget runs_started)
[ "$STARTED" = "0" ] || fail "redelivered referrals started $STARTED runs; dedup must hold"

WIRED=$(wc -l < "$WIRE" | tr -d ' ')
[ "$WIRED" = "3" ] || fail "expected 3 outbound requests, got $WIRED"

printf '\n\033[32mOK\033[0m -- 3 referrals triaged, 3 outbound requests with 0 identifiers,\n'
printf '     1 self-signature refused, 1 correction signed, 1 letter rehydrated.\n'
