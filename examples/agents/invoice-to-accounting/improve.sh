#!/bin/sh
# Weeks two and three of the same AP desk.
#
# smoke.sh is the agent doing its job under governance. This is what comes
# after: more mail arrives, one vendor keeps emailing photographs, and the
# desk (a) already benefits from the correction a person made in week one --
# the same misspelled vendor now posts itself -- and (b) reads its own run
# history back and proposes a fix a person decides on.
#
# Same contract as smoke.sh: run it through a language wrapper that exports
# AGENT and AGENT_OUT. Exits non-zero on any drift.
set -eu
cd "$(dirname "$0")"

: "${AGENT:?run me through a language wrapper: python/improve.sh, typescript/improve.sh or rust/improve.sh}"
: "${AGENT_OUT:?}"
SHEET="$AGENT_OUT/sheet.jsonl"

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
  echo "   ran smoke.sh -- 4 invoices, 3 posted"
fi

# ── 1. two more weeks of mail ─────────────────────────────────────────────
# MAIL_UPTO is the fixture clock; advancing it is how "later" arrives.
say "1. more mail: three more photographed pages, and a familiar misspelling"
sleep 1.2
STARTED=$(MAIL_UPTO=99 $AGENT ingest | jget runs_started)
[ "$STARTED" = "5" ] || fail "expected 5 new runs, got $STARTED"

# ── 2. the payoff of week one's correction ────────────────────────────────
# In week one this exact shape -- "Cobolt Cloud", low confidence -- parked
# for a person. Priya corrected it once; the alias became a fact in
# org.brightco.vendors; the trigger's declared context now carries it; and
# extraction canonicalizes the vendor and settles the confidence question.
# Same mail shape, no human in the loop the second time.
say "2. the misspelled vendor from week one now posts itself"
python3 - "$SHEET" <<'EOF' || exit 1
import json, sys
rows = [json.loads(l) for l in open(sys.argv[1])]
again = [r for r in rows if r["row_key"].startswith("<INV-CC-90188")]
assert again, "the week-two Cobolt invoice did not post at all"
assert again[0]["vendor"] == "Cobalt Cloud", again[0]
assert again[0]["approved_by"] == "auto", \
    "it should not have needed a person this time: %r" % again[0]
EOF
echo "   posted as 'Cobalt Cloud', approved_by auto -- the correction became memory"

# ── 3. the desk briefs itself ─────────────────────────────────────────────
# The briefing is a saved CAL query IN the memory file (desk_pulse): the
# plan, the tool definitions, recent activity and the lessons, assembled
# under a token budget. This is the context a scheduled improvement pass
# hands to a model -- the agent describing its own current setup.
say "3. the desk briefs itself out of its own memory"
BRIEF=$($AGENT brief)
echo "$BRIEF" | grep -q "invoice-to-accounting" || fail "the briefing does not name the plan"
echo "$BRIEF" | grep -q "alias_of" || fail "the briefing does not carry the learned alias"
echo "   plan, tools, lessons -- one saved query, one budget"

# ── 4. the loop reads the record ──────────────────────────────────────────
say "4. areev loop: deterministic analyzers over the desk's own journals"
IMPROVED=$($AGENT improve)
REC=$(echo "$IMPROVED" | jget pending 0 hash)
[ -n "$REC" ] && [ "$REC" != "None" ] || fail "the loop proposed nothing"
echo "$IMPROVED" | jget pending 0 summary | sed 's/^/   /'

# ── 5. the gate ───────────────────────────────────────────────────────────
say "5. what the loop is NOT allowed to do"
if $AGENT decide "$REC" apply --because "let the engine fix it" --as user:dev_rao >/dev/null 2>&1; then
  fail "the engine applied its own advisory finding"
fi
echo "   the engine cannot execute its own advice -- it is advisory"
if $AGENT decide "$REC" approve --as user:dev_rao >/dev/null 2>&1; then
  fail "a decision was recorded with no reason"
fi
echo "   a decision with no written reason is refused"

# ── 6. a person decides, and signs it ─────────────────────────────────────
say "6. a person decides, and signs it"
$AGENT decide "$REC" approve \
  --because "Northgate emails photographs; OCR them before parse instead of failing the run" \
  --as user:dev_rao >/dev/null
$AGENT teach org.acme.vendors "Northgate Supply" invoice_delivery \
  "photographed pages -- OCR before parse" >/dev/null
echo "   approved by user:dev_rao, lesson recorded against the vendor"

# ── 7. it does not nag ────────────────────────────────────────────────────
say "7. run the loop again"
PENDING=$($AGENT improve | python3 -c 'import json,sys; print(len(json.load(sys.stdin)["pending"]))')
[ "$PENDING" = "0" ] || fail "the same finding was proposed twice"
echo "   deduped -- the same evidence does not become a second recommendation"

printf '\n\033[32mOK\033[0m -- 1 correction became memory, 1 pattern found in 9 runs, 1 decision signed by name.\n'
