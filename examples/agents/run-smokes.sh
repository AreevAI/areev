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
#   pack        a built `areev` binary (cargo build -p areev, or AREEV=<path>) —
#               installs <agent>/pack/ into a scratch memory and asserts it
#               carries the same plan the stacks mint
#
# Every stack of one agent must mint the SAME workflow hash -- the plan is
# content-addressed and the seeders pin created_at, so a divergence means a
# stack drifted from the others. Asserted below.
set -eu
cd "$(dirname "$0")"

REQUIRE=${REQUIRE:-}
PYTHON=${PYTHON:-python3}
# The pack lane needs the binary, not a binding. A local build wins over one on
# PATH: a pack is checked against THIS tree's grain format.
AREEV=${AREEV:-}
if [ -z "$AREEV" ]; then
  if [ -x ../../target/debug/areev ]; then AREEV=$(cd ../.. && pwd)/target/debug/areev
  elif [ -x ../../target/release/areev ]; then AREEV=$(cd ../.. && pwd)/target/release/areev
  else AREEV=$(command -v areev || true)
  fi
fi
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

  # The pack lane (#178): the agent installs from `<agent>/pack/` too, and
  # what it installs must be the SAME plan. `pack install` already refuses a
  # grain that does not build to its recorded expected_hash, so this asserts
  # the other direction — that the recorded hash is the one the language
  # stacks actually mint, which is what goes stale when a seeder changes and
  # `export-packs.sh` was not re-run.
  pack_plans=""
  if [ -d "$agent/pack" ]; then
    if [ -n "$AREEV" ]; then
      printf '\n\033[1m== %s / pack ==\033[0m\n' "$agent"
      packdir=$(mktemp -d)
      if "$AREEV" pack install "$agent/pack" --db "$packdir/pack.db" --format json \
           > "$packdir/installed.json" 2>"$packdir/err"; then
        # Every workflow the pack installs, one per line. An agent may carry
        # more than one plan, so this is a CONTAINMENT check rather than
        # another entry in the one-hash comparison below: what must hold is
        # that the plan each language stack minted is among the plans the pack
        # installs.
        pack_plans=$(python3 -c 'import json,sys
for g in json.load(open(sys.argv[1]))["grains"]:
    if g["type"] == "workflow":
        print(g["hash"])' "$packdir/installed.json")
        echo "   installs $(echo "$pack_plans" | grep -c .) plan(s)"
      else
        cat "$packdir/err" >&2
        failed="$failed $agent/pack"
      fi
      rm -rf "$packdir"
    else
      case " $REQUIRE " in
        *" pack "*) echo "FAIL: $agent/pack required but no areev binary — cargo build -p areev, or set AREEV" >&2; exit 1 ;;
        *) echo "SKIP  $agent/pack (no areev binary — cargo build -p areev, or set AREEV)" ;;
      esac
    fi
  fi

  # The cross-language proof: same plan, same bytes, same content address —
  # and, with a pack present, the same plan the pack installs.
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

  # And the pack installs that same plan. This is what goes stale when a
  # seeder changes and `export-packs.sh` was not re-run: `pack install` proves
  # the pack is internally consistent, and only this proves it is the same
  # agent the stacks build.
  if [ -n "$pack_plans" ] && [ -n "$ran" ]; then
    for h in $hashes; do
      if echo "$pack_plans" | grep -qx "$h"; then
        printf '\033[32m%s\033[0m: the pack installs that plan too\n' "$agent"
      else
        echo "FAIL: $agent/pack installs no plan $h — re-run examples/agents/export-packs.sh" >&2
        failed="$failed $agent/pack-drift"
      fi
    done
  fi
done

if [ -n "$failed" ]; then
  echo "FAILED:$failed" >&2
  exit 1
fi
printf '\n\033[32mall agent smokes OK\033[0m\n'
