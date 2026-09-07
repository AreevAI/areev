#!/bin/sh
# Post-hoc: the loop's verdicts on the deployment's own checkpoint reads,
# the reverts it proposes, and one LLM read under what remains. See
# curve_verify.py. Runs under a SEPARATE binding build (VERIFY_PY) so the
# engine change it depends on -- baseline = newest run before the apply --
# is never swapped under a live run that uses the shared venv.
#
#   PROFILE=vrdu_reg SEED=1 EXP=320 EVAL=100 curve_verify.sh <seed-dir>
set -eu
HERE="$(cd "$(dirname "$0")" && pwd)"
. "$HERE/env.sh"
OUT="$1"
EXP="${EXP:-320}"; EVAL="${EVAL:-100}"
VERIFY_PY="${VERIFY_PY:-$HOME/mg/local/areev-verify/venv/bin/python3}"
[ -x "$VERIFY_PY" ] || { echo "curve_verify: no verify binding at $VERIFY_PY (maturin build the wheel into its own venv)" >&2; exit 1; }
[ -f "$OUT/ledger.db" ] || { echo "curve_verify: no ledger at $OUT" >&2; exit 1; }
echo "######## seed $SEED: verify -- the loop's verdicts on the checkpoint reads"
"$VERIFY_PY" "$HERE/curve_verify.py" --profile "$PROFILE" --dataset "$DATASET" --seed "$SEED" \
  --experience "$EXP" --eval "$EVAL" --seed-dir "$OUT" --workdir "$OUT/verify"
echo "######## seed $SEED: verify done — $OUT/verify"
