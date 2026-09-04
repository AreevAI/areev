#!/bin/sh
# Publish the four-way comparison: recomputed numbers, per-arm configs, and
# the two charts. Raw trials and memories stay local (they embed corpus
# text); only counts and configuration travel.
#
#   publish_fourway.sh <areev-run> <areev-metered-run> <mem0-root> <slm-root> <structure-root> <results-dir>
set -eu
HERE="$(cd "$(dirname "$0")" && pwd)"
REPO="$(cd "$HERE/../../.." && pwd)"
AREEV="$1"; METERED="$2"; MEM0="$3"; SLM="$4"; STRUCT="$5"; OUT="$6"
mkdir -p "$OUT" "$REPO/docs/assets"
python3 "$HERE/fourway.py" --areev "$AREEV" --areev-cost "$METERED" --mem0 "$MEM0" --slm "$SLM" \
  --out "$REPO/docs/assets/fourway" --json "$OUT/FOURWAY.json" | tee "$OUT/fourway.stdout"
for S in 1 2 3; do
  for MODE in default raw domain; do
    [ -f "$MEM0/$MODE/seed$S/run.config.json" ] && cp "$MEM0/$MODE/seed$S/run.config.json" "$OUT/mem0-$MODE.seed$S.run.config.json"
    [ -f "$MEM0/$MODE/seed$S/experience.summary.json" ] && cp "$MEM0/$MODE/seed$S/experience.summary.json" "$OUT/mem0-$MODE.seed$S.experience.summary.json"
  done
  [ -f "$SLM/seed$S/adapter/adapter.manifest.json" ] && cp "$SLM/seed$S/adapter/adapter.manifest.json" "$OUT/slm.seed$S.adapter.manifest.json"
  [ -f "$SLM/seed$S/corpus/corpus.manifest.json" ] && cp "$SLM/seed$S/corpus/corpus.manifest.json" "$OUT/slm.seed$S.corpus.manifest.json"
  [ -f "$METERED/seed$S/run.config.json" ] && cp "$METERED/seed$S/run.config.json" "$OUT/areev-metered.seed$S.run.config.json"
  [ -f "$STRUCT/seed$S/structure.summary.json" ] && cp "$STRUCT/seed$S/structure.summary.json" "$OUT/structure.seed$S.summary.json"
done
# cost, per arm, from journaled tokens
for d in "$METERED" "$MEM0/default" "$MEM0/raw" "$MEM0/domain" "$SLM" "$STRUCT"; do
  [ -d "$d" ] || continue
  n=$(basename "$d"); [ "$n" = "$(basename "$MEM0")" ] || true
  python3 "$HERE/cost.py" "$d" --json "$OUT/cost.$(echo "$d" | sed -E 's|.*/areev-runs/||; s|/|-|g').json" > /dev/null 2>&1 || true
done
# checksum what was copied so drift is detectable
( cd "$OUT" && find . -type f ! -name MANIFEST.md | sort | xargs shasum -a 256 ) > "$OUT/MANIFEST.md"
echo "published -> $OUT"
