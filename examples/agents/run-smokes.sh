#!/bin/sh
# Run every agent example's keyless smoke, in every language stack that can
# run on this machine -- the same entry point locally and in CI.
#
#   examples/agents/run-smokes.sh              # run what's available, skip loudly
#   REQUIRE="python typescript rust" .../run-smokes.sh    # a skip is a failure (CI)
#
# Prerequisites per stack (each ~one command, see docs/testing.md):
#   python      an interpreter that can `import areev` -- either
#               `pip install areev`, or PYTHON=<venv-python> after
#               `maturin develop -m crates/areev-py/Cargo.toml`
#   typescript  node >= 22.6, plus the binding: `npm i @areev/areev` in the
#               agent's typescript/ dir, or a built crates/areev-js checkout
#               (the wrappers find it via $AREEV_JS automatically in-tree)
#   rust        a Rust toolchain (the stack builds against the sibling crates)
#
# Every stack of one agent must mint the SAME workflow hash -- the plan is
# content-addressed and the seeders pin created_at, so a divergence means a
# stack drifted from the others. Asserted below.
set -eu
cd "$(dirname "$0")"

REQUIRE=${REQUIRE:-}
PYTHON=${PYTHON:-python3}
failed=""

available() {
  case "$1" in
    python)     "$PYTHON" -c "import areev" >/dev/null 2>&1 ;;
    typescript) command -v node >/dev/null 2>&1 ;;
    rust)       command -v cargo >/dev/null 2>&1 ;;
  esac
}

for agent in */; do
  agent="${agent%/}"
  [ -f "$agent/smoke.sh" ] || continue
  ran=""
  for lang in python typescript rust; do
    [ -f "$agent/$lang/smoke.sh" ] || continue
    if ! available "$lang"; then
      case " $REQUIRE " in
        *" $lang "*) echo "FAIL: $agent/$lang required but its toolchain is missing" >&2; exit 1 ;;
        *) echo "SKIP  $agent/$lang (toolchain not available -- see header)" ;;
      esac
      continue
    fi
    printf '\n\033[1m== %s / %s ==\033[0m\n' "$agent" "$lang"
    if PYTHON="$PYTHON" "$agent/$lang/smoke.sh" && PYTHON="$PYTHON" "$agent/$lang/improve.sh"; then
      ran="$ran $lang"
    else
      failed="$failed $agent/$lang"
    fi
  done

  # The cross-language proof: same plan, same bytes, same content address.
  hashes=$(for lang in $ran; do cat "$agent/$lang/out/workflow.hash" 2>/dev/null; done | sort -u)
  n=$(echo "$hashes" | grep -c . || true)
  if [ -n "$ran" ] && [ "$n" != "1" ]; then
    echo "FAIL: $agent stacks minted different workflow hashes:" >&2
    echo "$hashes" >&2
    failed="$failed $agent/hash-mismatch"
  elif [ -n "$ran" ]; then
    printf '\n\033[32m%s\033[0m: one plan, one hash (%s) across:%s\n' \
      "$agent" "$(echo "$hashes" | cut -c1-12)..." "$ran"
  fi
done

if [ -n "$failed" ]; then
  echo "FAILED:$failed" >&2
  exit 1
fi
printf '\n\033[32mall agent smokes OK\033[0m\n'
