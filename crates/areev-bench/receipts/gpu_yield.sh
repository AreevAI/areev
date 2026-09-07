#!/bin/sh
# One GPU, two rules. Training yields to inference: a local read under a
# concurrent training stretched from 2 seconds a call to 15 and past the
# harness's cap, and a timed-out call is scored as no output. And trainings
# take turns: two LoRA trainings at once exceeded Metal memory on a 36 GB
# laptop (the first attempt at a 316-row adapter failed with Insufficient
# Memory beside another seed's training; the batch-1 retry then diverged to
# NaN). So while any mlx_lm server is up every training is stopped
# (SIGSTOP), and otherwise only the OLDEST training runs. Nothing is edited,
# killed or restarted; a stopped process simply waits. Run it in the
# background for as long as two runs may overlap:
#
#   nohup sh gpu_yield.sh > gpu_yield.log 2>&1 &
#
# Portable to macOS ps: age comes from `etime` ([[dd-]hh:]mm:ss), not `etimes`.
set -u
age_seconds() {  # etime -> seconds
  echo "$1" | awk -F'[-:]' '{ n=NF; s=$n; m=(n>=2)?$(n-1):0; h=(n>=3)?$(n-2):0; d=(n>=4)?$(n-3):0; print d*86400+h*3600+m*60+s }'
}
while true; do
  servers=$(pgrep -f "mlx_lm server" | wc -l | tr -d ' ')
  oldest=""; oldest_age=-1
  for pid in $(pgrep -f "mlx_lm lora"); do
    a=$(age_seconds "$(ps -o etime= -p "$pid" 2>/dev/null | tr -d ' ')")
    [ -n "$a" ] && [ "$a" -gt "$oldest_age" ] && { oldest="$pid"; oldest_age="$a"; }
  done
  for pid in $(pgrep -f "mlx_lm lora"); do
    st=$(ps -o stat= -p "$pid" 2>/dev/null | cut -c1)
    if [ "$servers" -gt 0 ] || { [ -n "$oldest" ] && [ "$pid" != "$oldest" ]; }; then
      [ "$st" != "T" ] && kill -STOP "$pid" && echo "$(date '+%H:%M:%S') stop $pid ($servers server(s) up; oldest training ${oldest:-none})"
    else
      [ "$st" = "T" ] && kill -CONT "$pid" && echo "$(date '+%H:%M:%S') cont $pid"
    fi
  done
  sleep 15
done
