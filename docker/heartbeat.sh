#!/bin/sh
# The dumb heartbeat the trigger design asks for (docs/triggers.md, "Putting
# it on a heartbeat"): a loop of one-shot `areev trigger run` evaluations.
# There is still no daemon and no scheduler — the cadence is data in the
# memory, and this loop only has to be as fine as the GCD of the declared
# intervals. AREEV_HEARTBEAT_SECS (default 60, the render floor) sets the
# tick; host config rides the environment exactly as it would on a cron line
# ($AREEV_DB, $AREEV_RUN_TOOL_CMD, $AREEV_RUN_CONNECTOR_CMD,
# $AREEV_RUN_ALLOW_EXECUTOR, $AREEV_RUN_MODEL, …); any extra arguments are
# passed to `areev trigger run` verbatim (--ns, --db, budgets, …).
#
# A failed evaluation logs and waits for the next tick rather than killing
# the container: per-trigger backoff already lives in the memory, and a
# crash-looping container would just hammer the same failure faster.
set -u
INTERVAL="${AREEV_HEARTBEAT_SECS:-60}"
echo "areev heartbeat: evaluating every ${INTERVAL}s (db: ${AREEV_DB:-<default>})" >&2
while :; do
  areev trigger run "$@" || echo "areev heartbeat: evaluation failed (exit $?) — next tick in ${INTERVAL}s" >&2
  sleep "$INTERVAL"
done
