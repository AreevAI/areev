#!/bin/sh
# Week two of the same accounts-payable agent.
#
# `smoke.sh` is the agent doing its job under governance. This is the part that
# comes after: five more invoices arrive, three of them fail, and the agent
# proposes its own fix — from its own history, with no model key and no
# training. A person decides whether it is right.
#
# The failures are not random. One vendor emails photographs of invoices, and
# a photograph has no text layer. Read one at a time they look like three
# unlucky days. Counted, they are a pattern with a cause.
#
#   ./improve.sh              run it and assert the result
#   AREEV=/path/to/areev ./improve.sh    use a specific binary
#
# Exits non-zero on any drift, so CI can run it on every release.
set -eu

cd "$(dirname "$0")"
AREEV=${AREEV:-$(command -v areev || echo ../../../target/release/areev)}
NS=accounting
DB=${DB:-out/accounting.db}
TOOLS="python3 $(pwd)/tools.py"

[ -x "$AREEV" ] || { echo "no areev binary — set AREEV=/path/to/areev, or run: cargo build --release -p areev" >&2; exit 1; }

say() { printf '\n\033[1m%s\033[0m\n' "$*"; }

# Week one has to have happened — this chapter reads its journals.
if [ ! -f "$DB" ]; then
  say "0. week one first"
  AREEV="$AREEV" ./smoke.sh >/dev/null
  echo "   ran ./smoke.sh — 3 invoices, 2 posted"
fi

export SHEET_OUT="$(pwd)/out/sheet.jsonl"
export OUTBOX_OUT="$(pwd)/out/outbox.jsonl"

WF=$("$AREEV" cal 'RECALL workflows LIMIT 1' --db "$DB" --ns "$NS" \
     | python3 -c 'import json,sys; print(json.load(sys.stdin)["grains"][0]["hash"])')

# ── 1. another week of mail ────────────────────────────────────────────────
say "1. five more invoices arrive"
for f in 04-northgate-scan 05-northgate-scan 06-meridian-clean 07-northgate-scan 08-cobalt-clean; do
  id=$(echo "$f" | cut -d- -f1)
  printf '   %-22s ' "$f"
  "$AREEV" run start --db "$DB" --ns "$NS" --workflow "$WF" --run-id "wk2-$id" \
    --input "$(cat "fixtures/$f.json")" --tool-cmd "$TOOLS" --as agent:ap-intake 2>&1 \
    | tail -1 \
    | python3 -c 'import json,sys
r = json.loads(sys.stdin.read())["finished"]
print("posted" if r == "Completed" else "FAILED at parse — the page has no text layer")'
done

TOTAL=$("$AREEV" run list --db "$DB" --ns "$NS" \
        | python3 -c 'import json,sys; rs=json.load(sys.stdin); print(len(rs), sum(1 for r in rs if r["outcome"]=="failed"))')
echo "   runs so far: $TOTAL (total, failed)"
[ "$TOTAL" = "8 4" ] || { echo "FAIL: expected 8 runs with 4 failures, got $TOTAL"; exit 1; }

# ── 2. the loop reads its own history back ─────────────────────────────────
# No model key. Eleven deterministic analyzers over the grains the runs wrote.
say "2. the agent looks at its own record"
"$AREEV" loop run --db "$DB" --ns "$NS" 2>&1 | head -2

REC=$("$AREEV" loop list --db "$DB" --ns "$NS" | awk 'NR==1 {print $1}')
[ -n "$REC" ] || { echo "FAIL: the loop proposed nothing"; exit 1; }

say "3. what it found"
"$AREEV" loop show "$REC" --db "$DB" --ns "$NS" \
  | python3 -c '
import json, sys
r = json.load(sys.stdin)
sev, summary = r["severity"].upper(), r["summary"]
analyzer, conf = r["analyzer"], r["confidence"]
cited, origin = len(r["evidence"]), r["origin"]["kind"]
print(f"   {sev}  {summary}")
print(f"   analyzer   {analyzer}  (confidence {conf})")
print(f"   evidence   {cited} grains cited, by hash")
print(f"   origin     {origin} — deterministic, no model was called")
'

# ── 4. the gate ────────────────────────────────────────────────────────────
# Two refusals worth watching, because they are what makes this safe to leave
# switched on: the engine will not act on its own finding, and no human
# decision is recorded without a written reason.
say "4. what the loop is NOT allowed to do"

if "$AREEV" loop apply "$REC" --db "$DB" --ns "$NS" --because "let the engine fix it" >/dev/null 2>&1; then
  echo "FAIL: the engine applied its own advisory finding"; exit 1
fi
echo "   the engine cannot execute its own advice — it is advisory (LOP-E011)"

if "$AREEV" loop approve "$REC" --db "$DB" --ns "$NS" >/dev/null 2>&1; then
  echo "FAIL: a decision was recorded with no reason"; exit 1
fi
echo "   a decision with no written reason is refused"

# ── 5. a person decides ────────────────────────────────────────────────────
say "5. a person decides, and signs it"
"$AREEV" loop approve "$REC" --db "$DB" --ns "$NS" --as user:dev_rao \
  --because "Northgate emails photographs; OCR them before parse instead of failing the run"

# The finding is advisory, so the fix is the operator's to make. What changes
# is the memory the agent recalls the next time it meets this vendor.
"$AREEV" add northgate_supply invoice_delivery "photographed pages — OCR before parse" \
  --db "$DB" --ns "$NS" >/dev/null
echo "   recorded against the vendor:"
"$AREEV" recall northgate_supply --db "$DB" --ns "$NS" --render sml | sed 's/^/     /'

# ── 6. it does not nag ─────────────────────────────────────────────────────
say "6. run the loop again"
"$AREEV" loop run --db "$DB" --ns "$NS" 2>&1 | head -1
PENDING=$("$AREEV" loop list --db "$DB" --ns "$NS" 2>/dev/null | grep -c . || true)
[ "$PENDING" = "0" ] || { echo "FAIL: the same finding was proposed twice"; exit 1; }
echo "   deduped — the same evidence does not become a second recommendation"

printf '\n\033[32mOK\033[0m — 1 pattern found in 8 runs, 1 decision recorded against a named person.\n'
