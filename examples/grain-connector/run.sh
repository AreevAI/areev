#!/bin/sh
# A trigger whose connector is a GRAIN — pinned code, named by content
# address, with no host connector script anywhere (#185).
#
#   examples/grain-connector/run.sh
#
# Keyless and offline: the connector reads its mailbox from a filed blob
# through the same broker a network connector calls out through, so the whole
# path — pin, sandbox, capability, cursor, dedup, run start — is exercised
# with no credential and nothing to reach.
set -eu
cd "$(dirname "$0")"
ROOT=$(cd ../.. && pwd)

AREEV=${AREEV:-}
if [ -z "$AREEV" ]; then
  if   [ -x "$ROOT/target/debug/areev" ];   then AREEV="$ROOT/target/debug/areev"
  elif [ -x "$ROOT/target/release/areev" ]; then AREEV="$ROOT/target/release/areev"
  else AREEV=$(command -v areev || true)
  fi
fi
[ -n "$AREEV" ] || { echo "no areev binary — cargo build -p areev" >&2; exit 1; }

# The sandbox is a separate binary, always shipped beside `areev` (the release
# archive and the container image both carry it). Built here if this is a
# checkout: `cargo build --manifest-path areev-sandbox/Cargo.toml`.
SANDBOX=${AREEV_SANDBOX:-}
if [ -z "$SANDBOX" ]; then
  for c in "$ROOT/areev-sandbox/target/debug/areev-sandbox" \
           "$ROOT/areev-sandbox/target/release/areev-sandbox"; do
    [ -x "$c" ] && SANDBOX="$c" && break
  done
  [ -n "$SANDBOX" ] || SANDBOX=$(command -v areev-sandbox || true)
fi
[ -n "$SANDBOX" ] || {
  echo "no areev-sandbox — cargo build --manifest-path areev-sandbox/Cargo.toml" >&2
  exit 1
}

OUT=${OUT:-out}
DB="$OUT/mailbox.db"
rm -rf "$OUT"; mkdir -p "$OUT"

say()  { printf '\n\033[1m%s\033[0m\n' "$*"; }
fail() { echo "FAIL: $*" >&2; exit 1; }
jget() { python3 -c 'import json,sys
d = json.load(sys.stdin)
for k in sys.argv[1:]:
    d = d[int(k)] if isinstance(d, list) else d.get(k)
print("" if d is None else d)' "$@"; }

# ── 1. install ─────────────────────────────────────────────────────────────
say "1. install the pack: two tool definitions, a plan, and a trigger"
INSTALLED=$("$AREEV" pack install pack --db "$DB" --format json)
PIN=$(echo "$INSTALLED" | jget allow_executor 0)
TRIGGER=$(echo "$INSTALLED" | python3 -c 'import json,sys
print([g["hash"] for g in json.load(sys.stdin)["grains"] if g["type"] == "trigger"][0])')
# The pack carries its own copy of the blob (a pack is self-contained), so say
# out loud when that copy has fallen behind the one areev-tools publishes —
# a rebuilt blob is a new address, and a stale example would keep running the
# old bytes with nobody the wiser.
PUBLISHED=$(python3 -c 'import json; print(json.load(open("../../areev-tools/dist/blessed.json"))["tools"]["mailbox.poll"]["sha256"])' 2>/dev/null || echo "$PIN")
[ "$PIN" = "$PUBLISHED" ] || fail "pack/blobs/mailbox.poll.wasm is $PIN but areev-tools publishes $PUBLISHED — copy it across and re-run"
echo "   connector pinned at $PIN"
echo "   trigger   $TRIGGER"
[ -n "$PIN" ] || fail "the pack installed no executable blob"

# ── 2. the refusal that comes first ───────────────────────────────────────
say "2. without the pin, the trigger refuses — the code is in the memory,"
echo   "   the authorization to run it is NOT"
# In its own memory, because a refused poll BACKS OFF (a stale pin is treated
# like a connector failure, `docs/triggers.md`), and this example would rather
# show the refusal than wait out the window it earns.
"$AREEV" pack install pack --db "$OUT/nopin.db" >/dev/null
OUTPUT=$("$AREEV" trigger run --db "$OUT/nopin.db" --ns demo --sandbox-cmd "$SANDBOX" 2>&1 || true)
echo "$OUTPUT" | grep -q 'TRG-E012' || fail "expected a TRG-E012 refusal, got: $OUTPUT"
echo "   $(echo "$OUTPUT" | grep -o 'TRG-E012.*' | head -1 | cut -c1-110)…"

# ── 3. first poll: the cursor seeds, nothing fires ────────────────────────
say "3. first poll: the cursor seeds and NOTHING fires"
echo   "   (declaring a mailbox trigger must not replay its history)"
tick() {
  "$AREEV" trigger run --db "$DB" --ns demo \
    --allow-executor "$PIN" --sandbox-cmd "$SANDBOX" \
    --max-items 2 --format json
}
FIRST=$(tick)
STARTED=$(echo "$FIRST" | jget runs_started)
[ "${STARTED:-0}" = "0" ] || fail "the first poll started $STARTED runs; it must start none"
[ "$(echo "$FIRST" | jget claimed)" = "1" ] || fail "the trigger was not evaluated at all: $FIRST"
echo "   claimed, cursor seeded at the end of page one, 0 runs started"

# ── 4. second poll: the connector's next page starts runs ─────────────────
say "4. second poll: two messages, two runs, each parked for a person"
SECOND=$(tick)
STARTED=$(echo "$SECOND" | jget runs_started)
[ "$STARTED" = "2" ] || fail "expected 2 runs from the second page, got $STARTED: $SECOND"
"$AREEV" run list --db "$DB" --ns demo --format json > "$OUT/runs.json"
OPEN=$(python3 -c 'import json,sys
print(sum(1 for r in json.load(open(sys.argv[1])) if r.get("outcome") == "open"))' "$OUT/runs.json")
[ "$OPEN" = "2" ] || fail "expected 2 open runs, got $OPEN"
echo "   $STARTED runs started, $OPEN open — each parked at the client node"
echo "   (the plan's one node is answered by a person, so nothing else executed"
echo "    and no tool command was configured at all)"

# ── 5. and the same page again changes nothing ────────────────────────────
say "5. a third poll: the same mailbox, no new runs"
echo   "   (the run id is derived from trigger + connector + the item's dedup key)"
THIRD=$(tick)
[ "$(echo "$THIRD" | jget runs_started)" = "0" ] || fail "a re-poll started runs: $THIRD"
echo "   0 runs started"

# ── 6. what actually ran ──────────────────────────────────────────────────
say "6. what ran, and what it was allowed to do"
"$AREEV" recall --db "$DB" --ns agent:harness --limit 3 --format json > "$OUT/harness.json" 2>/dev/null || true
echo "   connector  mailbox.poll   (wasm32-areev-io, pinned $PIN)"
echo "   capability {\"blob\": {\"read\": true}} — one filed feed, by address, read-only"
echo "   host script: none"

printf '\n\033[32mOK\033[0m — a polling trigger ran code that lives in the memory:\n'
printf '     no --connector-cmd, no host script, and nothing ran that this host had not pinned.\n'
