#!/bin/sh
# Point git at the repo's tracked hooks.
#
#   scripts/install-hooks.sh
#
# One hook today: `pre-commit` keeps `docs/repo-stats.*` current so the CI
# `stats` gate does not fail on drift somebody else's merge caused. It
# regenerates only when the tree has moved past 1% — half the gate's
# tolerance — so most commits pay nothing.
#
# Undo with `git config --unset core.hooksPath`.
set -e
cd "$(dirname "$0")/.."
git config core.hooksPath .githooks
echo "core.hooksPath = .githooks"
echo "hooks: $(ls .githooks | tr '\n' ' ')"
