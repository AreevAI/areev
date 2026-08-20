#!/bin/sh
# Build the demo memory: `demo.db`, the file behind the README screenshots.
#
# One story — Northwind Trading's accounts-payable desk — carried all the way
# through: the knowledge the agent accumulated (seed_accounting_demo), the
# saved queries it recalls with, a real open fork from two channels editing
# offline, a declared polling trigger, seven governed runs (posted, awaiting a
# human, and one honest failure), and the recommendations `areev loop run`
# computes from all of it. Nothing here is hand-written into the store: every
# run is a real journal and every recommendation is a real analyzer output.
#
# Usage: scripts/build_demo.sh [OUT_DB]        (default: ~/Documents/areev/demo.db)
#
# Re-runnable: the target is deleted first, so this is the one command that
# regenerates the artifact when the seeder or the runtime changes.
set -eu

cd "$(dirname "$0")/.."
REPO=$(pwd)
OUT=${1:-$HOME/Documents/areev/demo.db}
AREEV=$REPO/target/release/areev
TOOLS="python3 $REPO/examples/agents/invoice-to-accounting/tools.py"
NS=accounting
WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT

# The mock tools write the "posted" sheet and the outbox to disk. Those are
# the demo's side effects, not artifacts — keep them out of the repo.
SHEET_OUT=$WORK/sheet.jsonl; OUTBOX_OUT=$WORK/outbox.jsonl
export SHEET_OUT OUTBOX_OUT

[ -x "$AREEV" ] || { echo "build the release binary first: cargo build --release -p areev" >&2; exit 1; }

mkdir -p "$(dirname "$OUT")"
rm -rf "$OUT" "$OUT"-wal "$OUT"-shm "$OUT".blobs "$OUT".telemetry.db "$OUT".telemetry.db-wal

cal() { "$AREEV" cal "$1" --db "$OUT" --ns "$NS" >/dev/null; }

echo "== grains"
cargo run --release -q -p areev-store --example seed_accounting_demo -- "$OUT" > "$WORK/seed.out"
cat "$WORK/seed.out"
FORK_PARENT=$(sed -n 's/^fork_parent=//p' "$WORK/seed.out")
WF=$(sed -n 's/^workflow=//p' "$WORK/seed.out")

echo "== saved queries (they travel with the file, and replicate in bundles)"
cal 'DEFINE QUERY "triage_ctx"($thread)
  DESCRIPTION "invoice triage context: instructions, targets, vendor aliases, category rules, thread"
AS {
  ASSEMBLE "invoice triage" FROM
    instructions: (RECALL skills WHERE namespace = "accounting" LIMIT 3),
    targets:      (RECALL goals  WHERE namespace = "accounting" LIMIT 5),
    vendors:      (RECALL facts  WHERE namespace = "accounting.vendors" LIMIT 50),
    rules:        (RECALL facts  WHERE namespace = "accounting.rules" LIMIT 50),
    thread:       (RECALL events WHERE session_id = $thread RECENT 10)
  BUDGET 4000 tokens FORMAT sml WITH progressive_disclosure(full)
}'
cal 'DEFINE QUERY "review_queue"()
  DESCRIPTION "invoice mail waiting on a human, newest first"
AS { RECALL events WHERE namespace = "accounting" RECENT 20 }'
cal 'DEFINE QUERY "vendor_terms"()
  DESCRIPTION "every vendor and the payment terms we agreed"
AS { RECALL facts WHERE relation = "payment_terms" LIMIT 50 }'
cal 'DEFINE TEMPLATE invoice_row AS "{{grain.subject}} · {{grain.relation}} · {{grain.object}}"'

echo "== an open fork: two channels edited one fact while one was offline"
# A fork is what CONCURRENT edits produce, so it has to be made the way it
# actually happens: clone the file, edit each copy, sync them back together.
cp "$OUT" "$WORK/branch.db"
"$AREEV" cal "SUPERSEDE sha256:$FORK_PARENT SET object = \"net_45\" BECAUSE \"matched the signed MSA during the quarterly review\"" \
  --db "$WORK/branch.db" --ns "$NS" >/dev/null
"$AREEV" bundle --db "$WORK/branch.db" --out "$WORK/branch.mgb" >/dev/null
"$AREEV" cal "SUPERSEDE sha256:$FORK_PARENT SET object = \"net_30\" BECAUSE \"vendor agreed to net 30 on the renewal call\"" \
  --db "$OUT" --ns "$NS" >/dev/null
"$AREEV" import --bundle "$WORK/branch.mgb" --db "$OUT" >/dev/null
"$AREEV" forks --db "$OUT" --ns "$NS" || true

echo "== declared policy: retention and a polling trigger"
"$AREEV" retention set --db "$OUT" --ns "$NS" --days 2555 \
  --because "expense records are kept seven years" >/dev/null
"$AREEV" trigger add --db "$OUT" --ns "$NS" --type polling \
  --workflow "$WF" --observer mailbox --scope "mailbox:ap@northwind.example" \
  --interval 300 --dedup-key /message_id --catchup last \
  --because "every invoice email starts one governed run, exactly once" >/dev/null
"$AREEV" trigger list --db "$OUT" --ns "$NS" || true

echo "== warm the recall telemetry"
# `cold_grains` asks which memories are never read. On a file nobody has ever
# queried the honest answer is "all of them", which is useless — so the demo
# does what the desk does: it recalls. Everything the agent actually uses gets
# read here; the QuickBooks-era cluster deliberately does not, which is why it
# is the thing the analyzer ends up pointing at.
for s in maya_iyer dev_rao tom_okafor lena_fischer invoice_intake payment_approvals \
         cobalt_cloud ironwood_furniture kestrel_legal meridian_freight \
         vantage_analytics blue_harbor_catering pinnacle_machining \
         "invoice:INV-CC-88431" "invoice:PM-9021" "invoice:KL-7742" \
         "invoice:IRN-2291" "invoice:VA-1180" "invoice:MF-5510" "invoice:MF-5511" \
         "invoice:BH-330" expense_sheet gmail_ap_mailbox "Northwind Trading" \
         vendor_onboarding tax_filing; do
  "$AREEV" recall "$s" --db "$OUT" --ns "accounting.*" -k 12 >/dev/null 2>&1 || true
done
for q in "vendor payment terms" "scanned invoice no text layer" "who approves above the threshold" \
         "category rules for freight" "duplicate invoice from a second sender"; do
  "$AREEV" search --query "$q" --db "$OUT" --ns "accounting.*" -k 10 >/dev/null 2>&1 || true
done
# The alias and rule grains are read by subject at extraction time, so warm
# them the same way rather than with one bulk scan.
for s in "Cobalt Cloud Inc." "COBALT CLOUD, INC" "Cobbalt Cloud" \
         "Ironwood Furniture B.V." "Ironwood Furn. BV" "Kestrel Legal L.L.P." \
         "Meridian Freight Lines Inc" "Vantage Analytics Limited" \
         "Blue Harbour Catering" "Pinnacle Machining G.m.b.H." \
         "vendor:cobalt_cloud" "vendor:vantage_analytics" "vendor:ironwood_furniture" \
         "vendor:kestrel_legal" "vendor:blue_harbor_catering" "vendor:pinnacle_machining" \
         "line_contains:desk chair" "line_contains:standing desk" "line_contains:seat license" \
         "line_contains:annual audit" "line_contains:offsite dinner" \
         "line_contains:conference booth" "line_contains:CNC tooling" \
         "amount_at_or_above:2500_usd" "confidence_below:0.75"; do
  "$AREEV" recall "$s" --db "$OUT" --ns "accounting.*" -k 5 >/dev/null 2>&1 || true
done
"$AREEV" cal 'RECALL facts WHERE namespace = "accounting.vendors" LIMIT 50' --db "$OUT" --ns "$NS" >/dev/null
"$AREEV" cal 'RECALL facts WHERE namespace = "accounting.rules" LIMIT 50' --db "$OUT" --ns "$NS" >/dev/null
"$AREEV" cal 'RECALL skills LIMIT 10' --db "$OUT" --ns "$NS" >/dev/null
"$AREEV" cal 'RECALL goals LIMIT 10' --db "$OUT" --ns "$NS" >/dev/null

echo "== runs"
run_start() { # id, input, [--as]
  "$AREEV" run start --db "$OUT" --ns "$NS" --workflow "$WF" --run-id "$1" \
    --input "$2" --tool-cmd "$TOOLS" --as agent:ap-intake 2>&1 | tail -1
}
ask_id() { # run-id  -> the tool_call_id the human gate is waiting on
  "$AREEV" run inspect --db "$OUT" --ns "$NS" --run-id "$1" 2>/dev/null \
    | python3 -c 'import json,sys
asks = json.load(sys.stdin).get("pending_asks") or {}
print(next(iter(asks), ""))'
}
approve() { # run-id, responder, approved
  ASK=$(ask_id "$1")
  [ -n "$ASK" ] || { echo "  no open ask on $1"; return 0; }
  "$AREEV" run respond --db "$OUT" --ns "$NS" --run-id "$1" --ask "$ASK" \
    --result "{\"approved\":$3,\"responder\":\"$2\"}" --as "user:$2" >/dev/null
  "$AREEV" run resume --db "$OUT" --ns "$NS" --run-id "$1" --tool-cmd "$TOOLS" 2>&1 | tail -1
}

# Under the review threshold: posted without waking anybody.
run_start inv-mf-5510  '{"message_id":"MF-5510","thread":"thr-ap-4359","vendor":"meridian_freight","amount":2140,"currency":"USD","category":"Equipments / Machinery","confidence":0.94,"attachment":"meridian-5510.pdf"}'
run_start inv-mf-5511  '{"message_id":"MF-5511","thread":"thr-ap-4359","vendor":"meridian_freight","amount":860,"currency":"USD","category":"Equipments / Machinery","confidence":0.93,"attachment":"meridian-5511.pdf"}'
run_start inv-va-1180  '{"message_id":"VA-1180","thread":"thr-ap-4341","vendor":"vantage_analytics","amount":1950,"currency":"GBP","category":"Software","confidence":0.94,"attachment":"vantage-1180.pdf"}'

# Above it: parked, approved by the controller, then posted. The approver is
# a different principal than the one that started the run — the runtime
# refuses it otherwise, which is what separation of duties means here.
run_start inv-cc-88431 '{"message_id":"INV-CC-88431","thread":"thr-ap-4401","vendor":"cobalt_cloud","amount":4400,"currency":"USD","category":"Software","confidence":0.91,"attachment":"cobalt-88431.pdf"}'
approve  inv-cc-88431 dev_rao true
run_start inv-pm-9021  '{"message_id":"PM-9021","thread":"thr-ap-4318","vendor":"pinnacle_machining","amount":18400,"currency":"EUR","category":"Equipments / Machinery","confidence":0.95,"attachment":"pinnacle-9021.pdf"}'
approve  inv-pm-9021 dev_rao true
run_start inv-irn-2291 '{"message_id":"IRN-2291","thread":"thr-ap-4388","vendor":"ironwood_furniture","amount":3180,"currency":"EUR","category":"Office Supply","confidence":0.96,"attachment":"ironwood-2291.pdf"}'
approve  inv-irn-2291 maya_iyer true

# Rejected: the plan routes it to the rejection reply and never to the sheet.
run_start inv-bh-330   '{"message_id":"BH-330","thread":"thr-ap-4330","vendor":"blue_harbour_catering","amount":2780,"currency":"EUR","category":"Meals / Entertainment","confidence":0.62,"attachment":"blueharbour-330.pdf"}'
approve  inv-bh-330 dev_rao false

# Still waiting on a human — this is the HITL queue the console shows.
run_start inv-kl-7742  '{"message_id":"KL-7742","thread":"thr-ap-4372","vendor":"kestrel_legal","amount":12500,"currency":"USD","category":"Compliance","confidence":0.88,"attachment":"kestrel-7742.pdf"}'

# An honest failure: a photographed invoice has no text layer, so the parser
# fails loudly instead of posting a blank row.
run_start inv-cc-scan  '{"message_id":"INV-CC-88602","thread":"thr-ap-4306","vendor":"cobalt_cloud","amount":980,"currency":"USD","category":"Software","confidence":0.44,"scanned":true,"attachment":"photo-88602.jpg"}'

"$AREEV" run list --db "$OUT" --ns "$NS" || true

echo "== loop: the analyzers read the history above and propose changes"
"$AREEV" loop run --db "$OUT" --ns "$NS" || true
"$AREEV" loop list --db "$OUT" --ns "$NS" --status pending || true

echo "== checkpoint the WAL so the artifact is one file"
if command -v sqlite3 >/dev/null 2>&1; then
  sqlite3 "$OUT" "PRAGMA wal_checkpoint(TRUNCATE);" >/dev/null
  rm -f "$OUT"-shm "$OUT"-wal
  # The telemetry sidecar carries the recall counts `cold_grains` reasons
  # from. It ships with the file so a fresh `areev loop run` on this demo
  # reproduces the same queue instead of calling every grain cold.
  if [ -f "$OUT.telemetry.db" ]; then
    sqlite3 "$OUT.telemetry.db" "PRAGMA wal_checkpoint(TRUNCATE);" >/dev/null
    rm -f "$OUT.telemetry.db-shm" "$OUT.telemetry.db-wal"
  fi
fi

echo
"$AREEV" verify --db "$OUT"
"$AREEV" stats --db "$OUT"
ls -la "$OUT"
