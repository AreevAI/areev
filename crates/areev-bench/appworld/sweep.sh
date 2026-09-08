#!/bin/sh
# Run one experiment across N worker processes, then evaluate it once.
#
#   sh sweep.sh <experiment-name> <dataset> [num-processes]
#
# AppWorld chunks the task list by process index; each worker is its own
# process with its own app databases. A frozen memory arm additionally takes
# a per-process copy of the memory (one file admits one handle, STO-E002),
# so the workers never contend on it.
set -e
NAME="${1:?usage: sweep.sh <experiment-name> <dataset> [num-processes]}"
DATASET="${2:?usage: sweep.sh <experiment-name> <dataset> [num-processes]}"
NPROC="${3:-4}"
HERE=$(cd "$(dirname "$0")" && pwd)
. "$HERE/env.sh"
cd "$APPWORLD_ROOT"

started=$(date +%s)
pids=""
i=0
while [ "$i" -lt "$NPROC" ]; do
  "$PY" "$HERE/run_experiment.py" "$NAME" --root "$APPWORLD_ROOT" \
      --provider "${AREEV_OR_PROVIDER:-streamlake}" \
      --num-processes "$NPROC" --process-index "$i" \
      > "${TMPDIR:-/tmp}/sweep-$(echo "$NAME" | tr / -)-$i.log" 2>&1 &
  pids="$pids $!"
  i=$((i + 1))
done

# `wait` with no operands always returns 0, so each worker is waited on by PID.
# A worker that dies takes the rest of its chunk with it, and the score would
# then be computed over a SMALLER denominator than the dataset -- a quietly
# wrong number, which is the one outcome worth failing loudly for.
failed=0
for pid in $pids; do
  wait "$pid" || failed=$((failed + 1))
done
elapsed=$(( $(date +%s) - started ))
echo "=== wall clock: ${elapsed}s across ${NPROC} processes (worker failures: ${failed}) ==="

if grep -qh "AREEV-WARN usage-null" "${TMPDIR:-/tmp}"/sweep-$(echo "$NAME" | tr / -)-*.log 2>/dev/null; then
  n=$(grep -ch "AREEV-WARN usage-null" "${TMPDIR:-/tmp}"/sweep-$(echo "$NAME" | tr / -)-*.log | paste -sd+ - | bc)
  echo "=== NOTE: ${n} call(s) returned no usage block; their tokens are uncounted ==="
fi

if [ "$failed" -ne 0 ]; then
  echo "REFUSING TO EVALUATE: ${failed} worker(s) failed, so some tasks never ran." >&2
  grep -h -A3 "^Traceback (most recent call last):" \
      "${TMPDIR:-/tmp}"/sweep-$(echo "$NAME" | tr / -)-*.log 2>/dev/null | tail -12 >&2 || true
  exit 1
fi

"$APPWORLD" evaluate "$NAME" "$DATASET"
