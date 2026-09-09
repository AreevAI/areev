#!/bin/sh
# Copy this directory's configs into an AppWorld root and (re)build the probe
# dataset. AppWorld is not vendored; the root is a checkout beside this repo,
# so the configs live here under version control and are installed into it.
#
#   sh install.sh ~/mg/local/appworld
set -e
ROOT="${1:?usage: install.sh <appworld-root>}"
HERE=$(cd "$(dirname "$0")" && pwd)
[ -d "$ROOT/experiments/configs" ] || { echo "not an AppWorld root: $ROOT" >&2; exit 1; }
cp -R "$HERE/configs/." "$ROOT/experiments/configs/"
python3 "$HERE/make_probe_set.py" --root "$ROOT" --split dev --per-difficulty 4 --name dev_probe
echo "installed into $ROOT"
