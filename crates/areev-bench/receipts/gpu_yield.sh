#!/bin/sh
# Training yields to inference. Two harness invocations on one laptop GPU --
# one seed training an adapter while another seed reads a held-out set --
# stretch a 2-second local read to 15 and past the harness's call timeout,
# and a timed-out call is scored as no output. This loop stops (SIGSTOP)
# every mlx_lm training process while any mlx_lm server is up, and continues
# it when none is. Reads are short and bounded; training just waits. Run it
# in the background for as long as two runs may overlap:
#
#   nohup sh gpu_yield.sh > gpu_yield.log 2>&1 &
#
# It never edits, kills or restarts anything, so it is safe beside live runs.
set -u
while true; do
  servers=$(pgrep -f "mlx_lm server" | wc -l | tr -d ' ')
  for pid in $(pgrep -f "mlx_lm lora"); do
    st=$(ps -o stat= -p "$pid" 2>/dev/null | cut -c1)
    if [ "$servers" -gt 0 ] && [ "$st" != "T" ]; then kill -STOP "$pid" && echo "$(date '+%H:%M:%S') stop $pid ($servers server(s) up)"; fi
    if [ "$servers" -eq 0 ] && [ "$st" = "T" ]; then kill -CONT "$pid" && echo "$(date '+%H:%M:%S') cont $pid"; fi
  done
  sleep 15
done
