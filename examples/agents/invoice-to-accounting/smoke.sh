#!/bin/sh
# invoice → accounting, end to end, with no credentials and no model key.
#
# Three invoices arrive. One is small enough to post itself, one needs a
# person, and one is a photographed page the parser cannot read. The whole
# point is what the third one does NOT do: it fails loudly instead of posting
# a blank row.
#
#   ./smoke.sh              run it and assert the result
#   AREEV=/path/to/areev ./smoke.sh    use a specific binary
#
# Exits non-zero on any drift, so CI can run it on every release.
set -eu

cd "$(dirname "$0")"
AREEV=${AREEV:-$(command -v areev || echo ../../../target/release/areev)}
NS=accounting
DB=${DB:-out/accounting.db}
TOOLS="python3 $(pwd)/tools.py"

[ -x "$AREEV" ] || { echo "no areev binary — set AREEV=/path/to/areev, or run: cargo build --release -p areev" >&2; exit 1; }

rm -rf out && mkdir -p out
export SHEET_OUT="$(pwd)/out/sheet.jsonl"
export OUTBOX_OUT="$(pwd)/out/outbox.jsonl"

say() { printf '\n\033[1m%s\033[0m\n' "$*"; }

# ── 1. the plan ────────────────────────────────────────────────────────────
# A workflow and its tool definitions are grains, so the plan travels as an
# ordinary memory bundle. Importing it is how a fresh install gets one.
say "1. import the plan"
"$AREEV" import --bundle plan.mgb --db "$DB"
WF=$("$AREEV" cal 'RECALL workflows LIMIT 1' --db "$DB" --ns "$NS" \
     | python3 -c 'import json,sys; print(json.load(sys.stdin)["grains"][0]["hash"])')
echo "   workflow $WF"

# What wakes it up is a declaration too, not a line in somebody's crontab: the
# cadence is a grain, so it travels with the memory. There is no daemon —
# `areev trigger run` is a one-shot command you put on whatever heartbeat you
# already have, and the memory decides what is actually due.
say "1b. declare what wakes it"
"$AREEV" trigger add --db "$DB" --ns "$NS" --type polling --observer mailbox \
  --scope 'mailbox:ap@northwind.example' --interval 120 \
  --workflow "$WF" --dedup-key /message_id \
  --because "poll the accounts-payable mailbox for invoices" >/dev/null
"$AREEV" trigger list --db "$DB" --ns "$NS" | sed 's/^/   /'
# Dry-run: touches nothing, and with no connector wired there is nothing to
# ingest. Swapping in a real mailbox connector is the only change needed.
"$AREEV" trigger run --db "$DB" --ns "$NS" --dry-run | sed 's/^/   /'

# ── 2. what the desk already knows ─────────────────────────────────────────
# The vendor terms and the routing rule are facts, not constants in a script,
# which is what lets the loop propose changing them later.
say "2. teach it two things"
"$AREEV" add cobalt_cloud payment_terms net_30 --db "$DB" --ns "$NS"
"$AREEV" add amount_at_or_above:2500_usd route_to human_review --db "$DB" --ns "$NS"

# ── 3. the runs ────────────────────────────────────────────────────────────
start() {
  "$AREEV" run start --db "$DB" --ns "$NS" --workflow "$WF" --run-id "$1" \
    --input "$(cat "$2")" --tool-cmd "$TOOLS" --as agent:ap-intake 2>&1 | tail -1
}

say "3a. an invoice under the threshold posts itself"
start small fixtures/01-under-threshold.json

say "3b. one over the threshold parks for a person"
start large fixtures/02-needs-approval.json
ASK=$("$AREEV" run inspect --db "$DB" --ns "$NS" --run-id large \
      | python3 -c 'import json,sys; print(next(iter(json.load(sys.stdin).get("pending_asks") or {}), ""))')
[ -n "$ASK" ] || { echo "FAIL: the run over the threshold did not park"; exit 1; }
echo "   waiting on ask $ASK"

# Responding as the principal that STARTED the run is refused — separation of
# duties is structural here, not a policy someone has to remember.
if "$AREEV" run respond --db "$DB" --ns "$NS" --run-id large --ask "$ASK" \
     --result '{"approved":true}' --as agent:ap-intake >/dev/null 2>&1; then
  echo "FAIL: the run's own starter was allowed to approve it"; exit 1
fi
echo "   the starter cannot approve its own run (refused, as designed)"

"$AREEV" run respond --db "$DB" --ns "$NS" --run-id large --ask "$ASK" \
  --result '{"approved":true,"responder":"dev_rao"}' --as user:dev_rao
"$AREEV" run resume --db "$DB" --ns "$NS" --run-id large --tool-cmd "$TOOLS" 2>&1 | tail -1

say "3c. a scanned page fails loudly rather than posting a blank row"
start scanned fixtures/03-scanned-page.json || true

# ── 4. assert ──────────────────────────────────────────────────────────────
say "4. what reached the sheet"
cat "$SHEET_OUT"
POSTED=$(wc -l < "$SHEET_OUT" | tr -d ' ')
[ "$POSTED" = "2" ] || { echo "FAIL: expected 2 posted rows, got $POSTED"; exit 1; }
grep -q '"approved_by": "dev_rao"' "$SHEET_OUT" || { echo "FAIL: the approver is not on the posted row"; exit 1; }
grep -q 'INV-CC-88602' "$SHEET_OUT" && { echo "FAIL: the unreadable scan reached the sheet"; exit 1; }

STATE=$("$AREEV" run list --db "$DB" --ns "$NS" \
        | python3 -c 'import json,sys; print(" ".join(sorted(r["run_id"]+"="+r["outcome"] for r in json.load(sys.stdin))))')
echo "   runs: $STATE"
[ "$STATE" = "large=completed scanned=failed small=completed" ] || {
  echo "FAIL: unexpected run outcomes"; exit 1; }

# ── 5. and afterwards, it is queryable ─────────────────────────────────────
say "5. the journal is in the same memory as the knowledge"
"$AREEV" run-trace --db "$DB" --ns "$NS" --run-id large --limit 4

printf '\n\033[32mOK\033[0m — 2 posted, 1 refused, 1 approval recorded against a named person.\n'
