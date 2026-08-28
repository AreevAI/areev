#!/bin/sh
# Weeks two and three at the same privacy desk.
#
# smoke.sh is the desk doing its job under governance. This is what comes
# after: (a) the erasure it granted in week one is still honoured, and the
# follow-up request closes without troubling a human, (b) the DECLARED
# retention rules run -- one of them refused, because someone wrote it as a
# wildcard, (c) the loop finds, in the desk's own journals, that a whole
# class of request is failing for one reason, and (d) a person decides what
# to do about it, in writing, after which the requests that failed can be
# re-run and answered.
#
# The improvement is not code. It is a rule the desk reads out of its own
# memory -- which is why a human declaring one line changes what the agent
# can do, with an audit trail, and without a deploy.
#
# Same contract as smoke.sh: run it through a language wrapper that exports
# AGENT and AGENT_OUT. Exits non-zero on any drift.
set -eu
cd "$(dirname "$0")"

: "${AGENT:?run me through a language wrapper: python/improve.sh}"
: "${AGENT_OUT:?}"
REGISTER="$AGENT_OUT/register.jsonl"
DEC=fixtures/decisions

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
  echo "   ran smoke.sh -- 4 requests, 1 erasure, 2 disclosures, 1 refusal"
fi

# -- 1. week two -----------------------------------------------------------
say "1. week two: three requests arrive from an email address, and one asks
   whether last week's erasure actually happened"
INTAKE=$(REQ_UPTO=08 $AGENT intake)
[ "$(echo "$INTAKE" | jget started)" = "4" ] || fail "expected 4 new runs: $INTAKE"
[ "$(echo "$INTAKE" | jget refused)" = "3" ] \
  || fail "the unresolvable requests did not fail loudly: $INTAKE"
[ "$(echo "$INTAKE" | jget closed_without_a_human)" = "1" ] \
  || fail "the follow-up request should close with nothing on file: $INTAKE"
echo "   3 refused, 1 closed -- and no human was asked about any of them"

# -- 2. the right stayed honoured ------------------------------------------
say "2. the erasure granted last week is still an erasure"
[ "$($AGENT report did:example:nadia-okonkwo | jget total_grains)" = "0" ] \
  || fail "grains for the erased subject came back"
TRACE=$($AGENT trace did:example:nadia-okonkwo)
[ "$(echo "$TRACE" | jget clean)" = "True" ] || fail "traces reappeared: $TRACE"
python3 - "$REGISTER" <<'EOF' || fail "the follow-up did not close on nothing-on-file"
import json, sys
reg = {r["request_id"]: r for r in map(json.loads, open(sys.argv[1]))}
row = reg["DSR-2031-0121"]
assert row["nothing_on_file"] is True, row
assert row["decided_by"] == "none", row
EOF
echo "   report empty, no surviving mention -- and the follow-up closed itself,"
echo "   because there is nothing to disclose and nothing to erase"

# -- 3. storage limitation, declared rather than coded ---------------------
# The rules are Facts in org.privacy: a namespace, a grain type, an age.
# One of them was written as "org.*" -- and destruction refuses a pattern
# rather than quietly sweeping every namespace under it.
say "3. the declared retention rules run"
INES_BEFORE=$($AGENT report did:example:ines-bakker | jget total_grains)
SOFIA_BEFORE=$($AGENT report did:example:sofia-marchetti | jget total_grains)
$AGENT sweep | python3 -c 'import json,sys
by = {r["namespace"]: r for r in json.load(sys.stdin)["rules"]}
wild = by["org.*"]
assert wild["applied"] is False, wild
assert "VAL-E001" in wild["refused"], wild
good = by["org.support"]
assert good["applied"] is True, good
assert good["grains_erased"] == 3, good
print("   org.support: %d grains past 365 days erased" % good["grains_erased"])
print("   org.*      : REFUSED -- %s" % wild["refused"].split(":")[0])
' || fail "the sweep did not behave"
INES_AFTER=$($AGENT report did:example:ines-bakker | jget total_grains)
SOFIA_AFTER=$($AGENT report did:example:sofia-marchetti | jget total_grains)
[ "$INES_AFTER" -lt "$INES_BEFORE" ] || fail "the sweep did not reach the old support history"
[ "$SOFIA_AFTER" -lt "$SOFIA_BEFORE" ] || fail "the sweep only touched people who asked"
echo "   it moved two people nobody asked about ($INES_BEFORE->$INES_AFTER, $SOFIA_BEFORE->$SOFIA_AFTER):"
echo "   a retention rule is not about anyone's request, which is why it is declared"
$AGENT verify | grep -q '"integrity": "ok"' || fail "the memory does not verify after the sweep"

# -- 4. the loop reads the record ------------------------------------------
say "4. areev loop: deterministic analyzers over the desk's own journals"
IMPROVED=$($AGENT improve)
REC=$(echo "$IMPROVED" | jget pending 0 hash)
[ -n "$REC" ] && [ "$REC" != "None" ] || fail "the loop proposed nothing"
echo "$IMPROVED" | python3 -c 'import json,sys
pending = json.load(sys.stdin)["pending"]
assert len(pending) >= 2, pending
assert any("identify_subject" in (p.get("target") or "") for p in pending), pending
for p in pending:
    print("   [%s] %s" % (p["severity"], p["summary"][:150]))
' || fail "the loop did not surface the identity-resolution cluster"

# -- 5. the gate -----------------------------------------------------------
say "5. what the loop is NOT allowed to do"
if $AGENT govern "$REC" apply --because "let the engine fix it" --as user:dpo-rivas >/dev/null 2>&1; then
  fail "the engine applied its own advisory finding"
fi
echo "   the engine cannot execute its own advice -- it is advisory"
if $AGENT govern "$REC" approve --as user:dpo-rivas >/dev/null 2>&1; then
  fail "a decision was recorded with no reason"
fi
echo "   a decision with no written reason is refused"

# -- 6. a person decides, and signs it -------------------------------------
say "6. the DPO decides, in writing"
$AGENT improve | python3 -c 'import json,sys
print("\n".join(p["hash"] for p in json.load(sys.stdin)["pending"]))' > "$AGENT_OUT/pending.txt"
while read -r H; do
  [ -n "$H" ] || continue
  $AGENT govern "$H" approve \
    --because "the intake form asks for an email address and the CRM keys on DIDs, so a whole class of request fails identity resolution. Declare the contact-address rule; keep refusing anyone we cannot verify." \
    --as user:dpo-rivas >/dev/null
done < "$AGENT_OUT/pending.txt"
echo "   approved by user:dpo-rivas, with the reason on the record"

# -- 7. THE RULE MOVES -----------------------------------------------------
# The fix is not a code change. It is one Fact in org.privacy: an email in
# the claim may be resolved through the contact address already on file.
say "7. the operator declares the rule the DPO approved"
$AGENT teach org.privacy dsar-intake mg:resolve_contact_email mg:contact_email >/dev/null
# The desk briefs itself out of its own memory, so the new rule shows up in
# the briefing without anyone editing a document.
$AGENT brief | grep -q 'mg:resolve_contact_email' \
  || fail "the desk's own briefing does not carry the rule that was just declared"
RETRY=$(REQ_UPTO=08 $AGENT intake --retry)
[ "$(echo "$RETRY" | jget parked)" = "3" ] \
  || fail "the requests that failed did not resolve after the rule landed: $RETRY"
# And note what did NOT change: the fix addressed identity RESOLUTION, not
# identity VERIFICATION. The unverified request is still refused.
[ "$(echo "$RETRY" | jget refused)" = "1" ] \
  || fail "the unverified request should still be refused: $RETRY"
echo "   3 requests that could not be answered are now on a DPO's desk"
echo "   the unverified one is still refused -- a resolution rule is not a"
echo "   verification rule, and the desk did not quietly conflate them"

# -- 8. and they get answered ----------------------------------------------
say "8. the DPO clears the backlog: one erasure, two disclosures"
$AGENT decide "$DEC/05-almeida-erase.json" | python3 -c 'import json,sys
cert = json.load(sys.stdin)["executed"][0]
assert cert["reported"] == cert["erased"] and cert["reported"] > 0, cert
print("   erasure: %d reported, %d erased" % (cert["reported"], cert["erased"]))
' || fail "the retried erasure did not hold the one-selector guarantee"
$AGENT decide "$DEC/06-sorensen-disclose.json" | jget outcome finished >/dev/null
$AGENT decide "$DEC/07-oyelaran-disclose.json" | jget outcome finished >/dev/null
[ "$($AGENT asks | python3 -c 'import json,sys; print(len(json.load(sys.stdin)))')" = "0" ] \
  || fail "requests are still parked"
[ "$($AGENT report did:example:rafa-almeida | jget total_grains)" = "0" ] \
  || fail "the retried erasure left grains behind"
[ "$($AGENT report did:example:hanne-sorensen | jget total_grains)" -gt 0 ] \
  || fail "a disclosure erased the subject it disclosed to"
echo "   queue empty; the erased subject is gone, the disclosed ones are not"

# -- 9. the record of what the desk did ------------------------------------
say "9. the desk's register, and what it does not say"
$AGENT register | python3 -c 'import json,sys
view = json.load(sys.stdin)
certs = view["certificate_grains"]
assert len(certs) >= 6, certs
for key, cert in certs.items():
    assert cert.get("mg:dsar_subject_ref"), (key, cert)
    assert cert.get("mg:dsar_approved_by"), (key, cert)
    assert cert.get("mg:dsar_because"), (key, cert)
erasures = [c for c in certs.values() if c["mg:dsar_act"] == "erasure"]
assert len(erasures) == 2, erasures
for cert in erasures:
    assert cert["mg:dsar_grains_reported"] == cert["mg:dsar_grains_erased"], cert
print("   %d certificates in the memory, %d of them erasures, every one"
      % (len(certs), len(erasures)))
print("   naming a case, an approver, a reason and a FINGERPRINT")
' || fail "the certificate register is not what it should be"

for who in did:example:nadia-okonkwo did:example:rafa-almeida; do
  T=$($AGENT trace "$who")
  [ "$(echo "$T" | jget named_in_certificates)" = "False" ] \
    || fail "a certificate names an erased person"
  [ "$(echo "$T" | jget journal_mentions)" = "0" ] \
    || fail "the run journal named an erased person"
done
echo "   neither erased person is named in any certificate or journal grain"
$AGENT verify | grep -q '"integrity": "ok"' || fail "the memory does not verify"

# -- 10. it does not nag ---------------------------------------------------
say "10. run the loop again"
PENDING=$($AGENT improve | python3 -c 'import json,sys; print(len(json.load(sys.stdin)["pending"]))')
[ "$PENDING" = "0" ] || fail "$PENDING findings were proposed again over the same evidence"
echo "   deduped -- the same evidence does not become a second recommendation"

printf '\n\033[32mOK\033[0m -- 1 declared retention rule applied and 1 refused, 1 loop finding\n'
printf '     decided in writing, 1 rule declared, 3 revived requests answered.\n'
