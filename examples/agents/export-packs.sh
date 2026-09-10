#!/bin/sh
# Regenerate every agent example's installable pack (#178).
#
#   examples/agents/export-packs.sh              # all of them
#   examples/agents/export-packs.sh due-diligence
#
# Each pack under `<agent>/pack/` is EXPORTED from a freshly seeded memory
# rather than written by hand: the grains a pack installs and the grains an
# agent's own seeder writes must be the same grains, and the only way to keep
# them the same is to derive one from the other. `pack export` refuses to
# write anything it cannot rebuild to the same address, so a pack that gets
# written is a pack that installs to the agent it came from.
#
# Prerequisites: a Python that can `import areev` (PYTHON=<venv>/bin/python,
# see docs/testing.md) and a built `areev` binary. Re-run after changing a
# seeder, and commit the diff — the pack lane in run-smokes.sh compares the
# pack's plan hash against the one the language stacks mint, so a stale pack
# fails loudly rather than installing last week's agent.
set -eu
cd "$(dirname "$0")"

PYTHON=${PYTHON:-python3}
AREEV=${AREEV:-../../target/debug/areev}
if [ ! -x "$AREEV" ]; then
  AREEV=$(command -v areev || true)
fi
[ -n "$AREEV" ] || { echo "no areev binary — cargo build -p areev, or set AREEV" >&2; exit 1; }
"$PYTHON" -c 'import areev' >/dev/null 2>&1 || {
  echo "this Python cannot import areev — set PYTHON=<venv>/bin/python (docs/testing.md)" >&2
  exit 1
}

only=${1:-}
for dir in */; do
  agent="${dir%/}"
  [ -f "$agent/python/agent.py" ] || continue
  [ -z "$only" ] || [ "$only" = "$agent" ] || continue

  ns=$(sed -n 's/^NS = "\([^"]*\)".*/\1/p' "$agent/python/agent.py" | head -1)
  [ -n "$ns" ] || { echo "SKIP $agent (no NS in python/agent.py)" >&2; continue; }

  # A clean memory: exporting a memory that has also RUN would carry the
  # journal of those runs into the pack, and a pack is what an agent is, not
  # what it did.
  rm -rf "$agent/python/out"
  ( cd "$agent/python" && "$PYTHON" agent.py seed >/dev/null )

  rm -rf "$agent/pack"
  "$AREEV" pack export \
    --db "$agent/python/out/agent.db" --ns "$ns" \
    --out "$agent/pack" --name "$agent" >/dev/null

  # And prove it installs, here, rather than discovering it on someone's
  # deployment: every expected_hash is checked by install itself.
  tmp=$(mktemp -d)
  "$AREEV" pack install "$agent/pack" --db "$tmp/check.db" >/dev/null
  rm -rf "$tmp"
  grains=$(ls "$agent/pack/grains" | wc -l | tr -d ' ')
  echo "$agent: $grains grains, ns $ns"
done
