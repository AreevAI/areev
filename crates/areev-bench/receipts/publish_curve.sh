#!/bin/sh
# Publish the learning-curve study: recomputed numbers for both base models,
# per-seed configs and summaries, every adapter's training receipt, the
# verify-leg verdicts, cost from journaled tokens, and the charts. Raw
# trials, corpora and memories stay local (they embed corpus text); only
# counts, configuration and the loop's own authored rules travel.
#
#   publish_curve.sh <curve-root (1.7B)> <curve-08b-root (0.6B)> <results-dir>
set -eu
HERE="$(cd "$(dirname "$0")" && pwd)"
REPO="$(cd "$HERE/../../.." && pwd)"
CURVE="$1"; SMALL="$2"; OUT="$3"
mkdir -p "$OUT" "$REPO/docs/assets"
python3 "$HERE/curve_stats.py" "$CURVE" --write | tee "$OUT/curve.stdout"
cp "$CURVE/CURVE.json" "$OUT/CURVE.json"
python3 "$HERE/curve_chart.py" "$CURVE" --out "$REPO/docs/assets/curve"
if [ -d "$SMALL" ]; then
  python3 "$HERE/curve_stats.py" "$SMALL" --write | tee "$OUT/curve-06b.stdout"
  cp "$SMALL/CURVE.json" "$OUT/CURVE-06b.json"
  python3 "$HERE/curve_chart.py" "$SMALL" --out "$REPO/docs/assets/curve06b"
fi
for root in "$CURVE" "$SMALL"; do
  [ -d "$root" ] || continue
  tag=$(basename "$root")
  for sd in "$root"/seed*; do
    [ -d "$sd" ] || continue
    s=$(basename "$sd")
    for f in run.config.json experience.summary.json a0.summary.json; do
      [ -f "$sd/$f" ] && cp "$sd/$f" "$OUT/$tag.$s.$f"
    done
    [ -f "$sd/verify/verify.summary.json" ] && cp "$sd/verify/verify.summary.json" "$OUT/$tag.$s.verify.summary.json"
    for ck in "$sd"/ck_*; do
      [ -d "$ck" ] || continue
      k=$(basename "$ck")
      for mode in scratch continual; do
        [ -f "$ck/adapter_$mode/adapter.manifest.json" ] && cp "$ck/adapter_$mode/adapter.manifest.json" "$OUT/$tag.$s.$k.adapter_$mode.manifest.json"
      done
      [ -f "$ck/corpus_all/corpus.manifest.json" ] && cp "$ck/corpus_all/corpus.manifest.json" "$OUT/$tag.$s.$k.corpus_all.manifest.json"
    done
  done
  python3 "$HERE/cost.py" "$root" --json "$OUT/cost.$tag.json" > /dev/null 2>&1 || true
done
( cd "$OUT" && find . -type f ! -name MANIFEST.md | sort | xargs shasum -a 256 ) > "$OUT/MANIFEST.md"
echo "published -> $OUT"
