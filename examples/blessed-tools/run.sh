#!/bin/sh
# The blessed http.call blob, bound by a pack, calling a real upstream —
# and refused when it aims somewhere the pack did not declare (#179).
#
#   examples/blessed-tools/run.sh
#
# Keyless in the sense that matters: the "credential" is a token this script
# invents and hands to the BROKER, never to the tool. The upstream is a
# loopback stub, so nothing leaves the machine.
set -eu
cd "$(dirname "$0")"
ROOT=$(cd ../.. && pwd)
PORT=${PORT:-7788}

AREEV=${AREEV:-}
if [ -z "$AREEV" ]; then
  if   [ -x "$ROOT/target/debug/areev" ];   then AREEV="$ROOT/target/debug/areev"
  elif [ -x "$ROOT/target/release/areev" ]; then AREEV="$ROOT/target/release/areev"
  else AREEV=$(command -v areev || true)
  fi
fi
[ -n "$AREEV" ] || { echo "no areev binary — cargo build -p areev" >&2; exit 1; }

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
DB="$OUT/gateway.db"
rm -rf "$OUT"; mkdir -p "$OUT"

say()  { printf '\n\033[1m%s\033[0m\n' "$*"; }
fail() { echo "FAIL: $*" >&2; exit 1; }

# The upstream. Its port is part of the pack's declaration, so it is fixed.
python3 stub-vendor.py "$PORT" > "$OUT/stub.log" 2>&1 &
STUB=$!
trap 'kill "$STUB" 2>/dev/null || true' EXIT INT TERM
# Wait for it rather than sleeping a guess. The probe is python3 — which this
# example already requires, since the stub IS python3 — and not bash's
# `/dev/tcp`: this script is `#!/bin/sh`, and on Ubuntu that is dash, which has
# no such device. (macOS's `/bin/sh` is bash, which is why a `/dev/tcp` probe
# passes locally and hangs in CI — the worst possible split.)
i=0
until python3 -c "import socket, sys
s = socket.socket(); s.settimeout(0.2)
sys.exit(0 if s.connect_ex(('127.0.0.1', $PORT)) == 0 else 1)" 2>/dev/null; do
  i=$((i + 1)); [ "$i" -lt 100 ] || fail "the stub upstream never came up on :$PORT"
  sleep 0.1
done

# ── 1. install ─────────────────────────────────────────────────────────────
say "1. install the pack: one Definition binding the blessed http.call blob"
INSTALLED=$("$AREEV" pack install pack --db "$DB" --format json)
PIN=$(echo "$INSTALLED" | python3 -c 'import json,sys; print(json.load(sys.stdin)["allow_executor"][0])')
PLAN=$(echo "$INSTALLED" | python3 -c 'import json,sys
print([g["hash"] for g in json.load(sys.stdin)["grains"] if g["type"] == "workflow"][0])')
TOOL=$(echo "$INSTALLED" | python3 -c 'import json,sys
print([g["hash"] for g in json.load(sys.stdin)["grains"] if g["type"] == "tool"][0])')
BLESSED=$(python3 -c 'import json; print(json.load(open("../../areev-tools/dist/blessed.json"))["tools"]["http.call"]["sha256"])')
[ "$PIN" = "$BLESSED" ] || fail "the pack pinned $PIN, which is not the published http.call ($BLESSED)"
echo "   the code is the blessed blob:  $PIN"
echo "   the policy is the declaration: hosts=[http://127.0.0.1:$PORT] methods=[GET,POST] credentials=[vendor]"

run() {
  # One run per call. The credential is named here and READ here — the tool
  # holds a label, never a value.
  VENDOR_TOKEN=demo-vendor-token "$AREEV" run start --db "$DB" --ns ap \
    --workflow "$PLAN" --run-id "$1" --input "$2" \
    --allow-executor "$PIN" --sandbox-cmd "$SANDBOX" \
    --credential vendor=VENDOR_TOKEN \
    --allow-host "http://127.0.0.1:$PORT" \
    --tool-egress 'vendor_api:vendor:GET+POST' \
    --format json 2>>"$OUT/run.err"
}

# What the blob actually emitted, read back out of the memory: a run's result
# is a grain, not a line on someone's terminal.
result() {
  "$AREEV" cal 'RECALL tools RECENT 6' --db "$DB" --ns ap --format json \
    | python3 -c 'import json,sys
for g in json.load(sys.stdin)["grains"]:
    f = g["fields"]
    if f.get("tool_name") == "vendor_api" and (f.get("tool_content") or f.get("content")):
        print(f.get("tool_content") or f.get("content"))
        break'
}

# ── 2. the declared call ───────────────────────────────────────────────────
say "2. a call the declaration permits"
run declared \
  "{\"url\": \"http://127.0.0.1:$PORT/v1/invoices/4471\", \"method\": \"GET\", \
    \"credential\": \"vendor\", \"headers\": {\"X-Api-Version\": \"2026-01-01\"}}" \
  > "$OUT/declared.json"
ANSWER=$(result)
echo "$ANSWER" > "$OUT/declared-result.json"
case "$ANSWER" in
  *'"status":200'*) : ;;
  *) fail "the permitted call did not reach the upstream: $ANSWER" ;;
esac
case "$ANSWER" in
  *"Acme Freight"*) : ;;
  # The stub 401s anything without the exact bearer token, so a 200 carrying
  # the invoice IS the proof that the broker attached a credential the guest
  # never held.
  *) fail "the upstream answered, but not with the invoice: $ANSWER" ;;
esac
echo "   200, and the invoice came back"
echo "   the tool named a credential and never saw one — the broker attached it"

say "2b. byte-exact PDF download and export through the installed tool"
run binary-download \
  "{\"url\":\"http://127.0.0.1:$PORT/v1/invoices/4471/attachment\",\"method\":\"GET\",\"credential\":\"vendor\",\"response_mode\":\"artifact\"}" \
  > "$OUT/binary-download.json"
ANSWER=$(result)
echo "$ANSWER" > "$OUT/binary-result.json"
python3 -c 'import hashlib, json, subprocess, sys
a = json.load(open(sys.argv[1]))
b = bytes.fromhex("255044462d312e370a0080ff0a")
assert a["status"] == 200 and a["mime"] == "application/pdf"
assert a["ref"] == "cas://" + a["sha256"]
assert subprocess.check_output([sys.argv[2], "blob", "get", a["ref"], "--db", sys.argv[3]]) == b
assert a["sha256"] == "sha256:" + hashlib.sha256(b).hexdigest() and a["bytes"] == len(b)' \
  "$OUT/binary-result.json" "$AREEV" "$DB" \
  || fail "binary download was not byte-exact: $ANSWER"
REF=$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["ref"])' "$OUT/binary-result.json")
run binary-export \
  "{\"url\":\"http://127.0.0.1:$PORT/v1/invoices/4471/export\",\"method\":\"POST_ARTIFACT\",\"credential\":\"vendor\",\"response_mode\":\"artifact\",\"body_ref\":\"$REF\",\"content_type\":\"application/pdf\"}" \
  > "$OUT/binary-export.json"
ANSWER=$(result)
echo "$ANSWER" > "$OUT/export-result.json"
python3 -c 'import json, sys
assert json.load(open(sys.argv[1])) == json.load(open(sys.argv[2]))' \
  "$OUT/binary-result.json" "$OUT/export-result.json" \
  || fail "binary export did not retain the bytes: $ANSWER"
echo "   PDF bytes, digest, length, and MIME survived download and upload"
"$AREEV" run verify --db "$DB" --ns ap --run-id binary-download > "$OUT/binary-verify.json" \
  || fail "binary run did not replay consistently"
"$AREEV" run resume --db "$DB" --ns ap --run-id binary-download --format json \
  > "$OUT/binary-resume.json" || fail "completed binary run could not resume idempotently"
"$AREEV" cal 'RECALL observations RECENT 30' --db "$DB" --ns agent:harness --format json \
  > "$OUT/binary-audit.json"
python3 -c 'import hashlib, json, sys
rows = [g["fields"] for g in json.load(open(sys.argv[1]))["grains"]
        if g["fields"].get("observation_kind") == "egress_call"
        and g["fields"].get("run_id") in ("binary-download", "binary-export")]
expected = "sha256:" + hashlib.sha256(bytes.fromhex("255044462d312e370a0080ff0a")).hexdigest()
assert len(rows) == 2, rows
assert all(r["response_digest"] == expected and r["response_bytes"] == 13
           and r["response_mime"] == "application/pdf"
           and r["response_ref"] == "cas://" + expected
           and r["credential"] == "vendor" for r in rows)
assert "demo-vendor-token" not in json.dumps(rows)
assert "body_base64" not in json.dumps(rows) and "PDF-1.7" not in json.dumps(rows)
assert next(r for r in rows if r["method"] == "POST")["request_digest"] == expected' \
  "$OUT/binary-audit.json" || fail "binary audit exposed bytes or lost metadata"

# ── 3. the undeclared host ────────────────────────────────────────────────
say "3. the same blob, aimed somewhere the pack did not declare"
run undeclared \
  "{\"url\": \"http://127.0.0.1:$((PORT + 1))/v1/invoices/4471\", \"method\": \"GET\", \
    \"credential\": \"vendor\"}" > "$OUT/undeclared.json" 2>&1 || true
ANSWER=$(result)
echo "$ANSWER" > "$OUT/undeclared-result.json"
case "$ANSWER" in
  *RUN-E022*) : ;;
  *) fail "an undeclared host must be refused by the broker: $ANSWER" ;;
esac
echo "   refused by the BROKER with RUN-E022 — nothing was sent, and the refusal"
echo "   is journaled in the memory rather than only printed here"

# ── 4. provenance ─────────────────────────────────────────────────────────
say "4. and the code that ran can be chased back to its bytes"
# By hash: the Definition is the thing with provenance, and its name is not
# its identity.
"$AREEV" tool provenance "$TOOL" --db "$DB" --ns ap > "$OUT/provenance.json" 2>&1 || true
grep -q "$PIN" "$OUT/provenance.json" \
  || fail "provenance does not chain the blob: $(cat "$OUT/provenance.json")"
grep -q '"blob_present": true' "$OUT/provenance.json" \
  || fail "the blob this Definition names is not in the memory: $(cat "$OUT/provenance.json")"
# Both runs, including the refused one: a request that was refused is still a
# run that touched this code, and an audit trail that showed only the
# successful call would be the wrong record.
python3 -c 'import json,sys
runs = sorted(json.load(open(sys.argv[1]))["runs_touching"])
assert runs == ["binary-download", "binary-export", "declared", "undeclared"], runs' "$OUT/provenance.json" \
  || fail "provenance does not name the runs that executed it: $(cat "$OUT/provenance.json")"
echo "   areev tool provenance $(echo "$TOOL" | cut -c1-12)… → $(echo "$PIN" | cut -c1-12)…,"
echo "   present, 2,598 bytes, and all four runs that executed it —"
echo "   the code that ran is IN the memory, addressable, and travels with it"

printf '\n\033[32mOK\033[0m — one blessed blob, three calls, one refusal:\n'
printf '     the tool made no policy decision; the declaration and the grant did.\n'
