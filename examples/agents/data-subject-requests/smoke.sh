#!/bin/sh
# A week at the privacy desk, end to end, with no credentials and no model key.
#
# Four data-subject requests arrive: an access request, an erasure request,
# a portability request, and one from someone who never proved they are who
# they say. The desk prices each against what is actually stored and parks
# for a Data Protection Officer -- because an erasure is irreversible and
# the approver's identity IS the audit record.
#
# The property this example exists for: DISCLOSURE AND ERASURE ARE ONE
# SELECTION. `subject_report` shows exactly what `forget_subject` removes,
# and step 7 asserts the two counts agree, namespace by namespace. A desk
# that discloses one set and deletes another has failed, quietly.
#
# Three refusals are load-bearing and each is asserted here:
#   * the desk cannot approve its own erasure (step 4)
#   * a decision with no written reason is refused (step 5)
#   * a wildcard namespace never widens destruction (step 2)
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
REGISTER="$AGENT_OUT/register.jsonl"
CERTS="$AGENT_OUT/certificates.jsonl"
PACKS="$AGENT_OUT/packs"
DEC=fixtures/decisions

say()  { printf '\n\033[1m%s\033[0m\n' "$*"; }
fail() { echo "FAIL: $*" >&2; exit 1; }
jget() { python3 -c 'import json,sys; d=json.load(sys.stdin)
for k in sys.argv[1:]:
    d = d[int(k)] if isinstance(d, list) else d.get(k)
print(d)' "$@"; }

rm -rf "$AGENT_OUT"

# -- 1. seed ---------------------------------------------------------------
say "1. seed: the plan, the tool definitions, the desk's declared rules, and a
   synthetic three-namespace memory of eight fictional people"
SEEDED=$($AGENT seed)
WF=$(echo "$SEEDED" | jget workflow)
echo "$WF" > "$AGENT_OUT/workflow.hash"
echo "   workflow $WF"
echo "   $(echo "$SEEDED" | jget grains_seeded) grains across $(echo "$SEEDED" | jget namespaces)"

# -- 2. the refusals that are structural -----------------------------------
# A `*` namespace is a READING convention. Every destructive surface -- and
# the DSAR read that mirrors it -- refuses one outright, because a wildcard
# that widened destruction would be indistinguishable from a typo.
say "2. destruction takes an exact namespace, and a non-empty subject"
GUARDS=$($AGENT guards)
for check in erase_wildcard_namespace report_wildcard_namespace \
             sweep_wildcard_namespace erase_empty_subject report_empty_subject; do
  [ "$(echo "$GUARDS" | jget "$check" refused)" = "True" ] \
    || fail "$check was NOT refused -- a wildcard or an empty identity reached destruction"
done
echo "$GUARDS" | grep -q 'VAL-E001' || fail "the refusals do not carry VAL-E001"
echo "   5 refusals, all VAL-E001: 'org.*' never widens an erasure, and an"
echo "   unset identity never reads as 'erase everything'"

# -- 3. the requests arrive ------------------------------------------------
say "3. four requests: access, erasure, portability -- and one unverified"
INTAKE=$($AGENT intake)
[ "$(echo "$INTAKE" | jget started)" = "4" ]  || fail "expected 4 runs, got $INTAKE"
[ "$(echo "$INTAKE" | jget parked)" = "3" ]   || fail "expected 3 parked, got $INTAKE"
[ "$(echo "$INTAKE" | jget refused)" = "1" ]  || fail "the unverified request was not refused: $INTAKE"
echo "   3 parked on a DPO, 1 refused before anything was read or removed"

# Art. 12(6): verify, or do not act. Disclosing to the wrong person is a
# breach; erasing the wrong person cannot be undone.
$AGENT runs | python3 -c 'import json,sys
runs = {r["run_id"]: r["outcome"] for r in json.load(sys.stdin)}
assert runs.get("dsr-2031-0117") == "failed", runs
' || fail "the unverified request did not stop the run"
[ ! -f "$PACKS/DSR-2031-0117.json" ] || fail "an unverified requester was sent a disclosure pack"
[ ! -f "$CERTS" ] || fail "something was executed before any officer decided"
echo "   the unverified request produced no pack and no certificate"

# -- 4. the desk cannot approve its own erasure ----------------------------
say "4. the desk approves its own erasure -- refused, structurally"
if $AGENT decide "$DEC/00-desk-approves-itself.json" >/dev/null 2>&1; then
  fail "the principal that started the run was allowed to approve it"
fi
echo "   separation of duties: the starter cannot be the approver (RUN-E012)"

# -- 5. and a decision with no reason is refused ---------------------------
say "5. an erasure with no written reason -- refused"
if $AGENT decide "$DEC/02-okonkwo-no-reason.json" >/dev/null 2>&1; then
  fail "an irreversible act was approved with no stated reason"
fi
echo "   the reason is the only part of an irreversible act a regulator can read"

# -- 6. access: the Art. 15 disclosure -------------------------------------
say "6. the DPO discloses to Ines Bakker (Art. 15 access)"
BEFORE=$($AGENT report did:example:ines-bakker | jget total_grains)
[ "$BEFORE" = "6" ] || fail "expected 6 grains on file for the access request, got $BEFORE"
$AGENT decide "$DEC/01-bakker-disclose.json" >/dev/null
[ -f "$PACKS/DSR-2031-0114.json" ] || fail "no disclosure pack was written"
python3 - "$PACKS/DSR-2031-0114.json" "$BEFORE" <<'EOF' || fail "the pack does not match the report"
import json, sys
pack = json.load(open(sys.argv[1]))
disclosed = sum(len(n["grains"]) for n in pack["namespaces"])
assert disclosed == int(sys.argv[2]), (disclosed, sys.argv[2])
assert pack["identity_names"], pack
EOF
echo "   $BEFORE grains disclosed -- the pack IS the report, not a re-query"

# -- 7. erasure: the report and the erasure are one selection --------------
# This is the assertion the example exists for. For every namespace, what
# the report disclosed is what the erasure removed. The desk refuses to
# erase at all if the two ever diverge.
say "7. the DPO grants Nadia Okonkwo's erasure (Art. 17)"
OTHER_BEFORE=$($AGENT report did:example:ines-bakker | jget total_grains)
$AGENT decide "$DEC/03-okonkwo-erase.json" > "$AGENT_OUT/erasure.json"
grep -q '"withdrawal_recorded": "' "$AGENT_OUT/erasure.json" \
  || fail "the consent withdrawal was not recorded before the erasure ran"
python3 -c 'import json,sys
cert = json.load(open(sys.argv[1]))["executed"][0]
assert cert["act"] == "erasure", cert
assert cert["reported"] == cert["erased"], cert
assert cert["reported"] > 0, cert
for row in cert["per_namespace"]:
    assert row["reported"] == row["erased"], row
print("   %d grains reported, %d erased, across %d namespaces"
      % (cert["reported"], cert["erased"], len(cert["per_namespace"])))
' "$AGENT_OUT/erasure.json" || fail "the report and the erasure did not agree"

# The withdrawal joined the erasure set: it names the person, so it is in
# scope for the erasure it authorised. What survives is the certificate.
python3 - "$REGISTER" "$CERTS" <<'EOF' || fail "the withdrawal did not join the erasure set"
import json, sys
reg = {r["request_id"]: r for r in map(json.loads, open(sys.argv[1]))}
cert = [c for c in map(json.loads, open(sys.argv[2])) if c["act"] == "erasure"][0]
priced = reg["DSR-2031-0115"]["inventory_grains"]
assert cert["reported"] == priced + 1, (cert["reported"], priced)
EOF
echo "   the consent withdrawal was itself in scope -- it names her too"

AFTER=$($AGENT report did:example:nadia-okonkwo | jget total_grains)
[ "$AFTER" = "0" ] || fail "the erasure left $AFTER grains behind"
OTHER_AFTER=$($AGENT report did:example:ines-bakker | jget total_grains)
[ "$OTHER_AFTER" = "$OTHER_BEFORE" ] \
  || fail "erasing one subject moved another ($OTHER_BEFORE -> $OTHER_AFTER)"
echo "   the report is now empty, and the other subject is untouched at $OTHER_AFTER"

# -- 8. portability: a portable artifact, not a JSON dump ------------------
say "8. the DPO grants Tomas Vetter's portability request (Art. 20)"
PORT=$($AGENT decide "$DEC/04-vetter-disclose.json")
OPS=$(echo "$PORT" | jget executed 0 bundle_ops)
[ "$OPS" -gt 0 ] || fail "no portable bundle was written"
ls "$PACKS"/DSR-2031-0116.*.mgb >/dev/null 2>&1 \
  || fail "the MGB bundle files are missing"
echo "   $OPS records exported as MGB1 bundles -- importable into any OMS store"

# -- 9. what the record of an erasure may say ------------------------------
# An immutable, replicating grain that named the erased person would undo
# the erasure it records. The certificate names a FINGERPRINT.
say "9. the certificate survives her; her name does not"
TRACE=$($AGENT trace did:example:nadia-okonkwo)
[ "$(echo "$TRACE" | jget clean)" = "True" ] || fail "traces survive: $TRACE"
[ "$(echo "$TRACE" | jget text_mentions)" = "0" ] \
  || fail "a prose mention of the erased subject survived"
[ "$(echo "$TRACE" | jget journal_mentions)" = "0" ] \
  || fail "the run journal named the data subject"
[ "$(echo "$TRACE" | jget named_in_certificates)" = "False" ] \
  || fail "the erasure certificate names the person it erased"
[ "$(echo "$TRACE" | jget fingerprinted_in_certificates)" = "True" ] \
  || fail "the erasure was not certified at all"
echo "   selector 0, structural 0, prose 0, journal 0 -- and one certificate"
echo "   naming $(echo "$TRACE" | jget subject_ref), which you can recompute but not reverse"

# The recall-telemetry sidecar logs QUERY TEXT, and this desk spends its day
# searching for people it is about to erase. A desk that must be able to
# erase does not keep a query log of who it searched for.
if ls "$AGENT_OUT"/*.telemetry.db >/dev/null 2>&1; then
  fail "a telemetry sidecar exists: it would hold the names of erased subjects"
fi
echo "   and no telemetry sidecar, so the searches left no residue either"

# -- 10. the memory is still the memory ------------------------------------
say "10. integrity after erasure"
$AGENT verify | grep -q '"integrity": "ok"' \
  || fail "the memory does not verify after the erasures"
DECIDED=$(grep -c '"decision": "' "$REGISTER" || true)
[ "$DECIDED" = "3" ] || fail "expected 3 closed requests in the register, got $DECIDED"
echo "   every content address still verifies; 3 requests closed and signed"

printf '\n\033[32mOK\033[0m -- 1 access, 1 portability, 1 erasure (report == erasure),\n'
printf '     1 unverified request refused, 2 approvals refused, 5 guards held.\n'
