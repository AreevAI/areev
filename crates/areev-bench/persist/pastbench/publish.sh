#!/bin/sh
# Package what travels from a run root into the repo: the benchmark's own
# comparison and summary files, the governance ledgers, the loop usage
# meters, the spend bounds, and the summarizer's table — never the traces
# or the memory files — plus a MANIFEST.md of sha256s, as every result
# directory here carries.
#
#   ROOT=$HOME/mg/local/areev-runs/persist/full/run1 OUT=results/persist-2026-09-07-run1 sh publish.sh
set -eu
HERE=$(cd "$(dirname "$0")" && pwd)
ROOT="${ROOT:?}"; OUT="${OUT:?}"
mkdir -p "$OUT"
cp "$ROOT/spend.jsonl" "$OUT/" 2>/dev/null || true
for agent_dir in "$ROOT"/*/; do
  agent=$(basename "$agent_dir")
  [ -d "$agent_dir" ] || continue
  for fam_dir in "$agent_dir"*/; do
    fam=$(basename "$fam_dir")
    [ -f "$fam_dir/sequence_comparison.json" ] || continue
    d="$OUT/$agent/$fam"; mkdir -p "$d"
    cp "$fam_dir/sequence_comparison.json" "$d/"
    for v in with_persistence without_persistence; do
      [ -f "$fam_dir/$v/sequence_summary.json" ] && cp "$fam_dir/$v/sequence_summary.json" "$d/$v.sequence_summary.json"
      for led in "$fam_dir/$v"/0*/artifacts/areev_ledger.json "$fam_dir/$v"/0*/artifacts/mem0_ledger.json; do
        [ -f "$led" ] || continue
        ep=$(basename "$(dirname "$(dirname "$led")")")
        cp "$led" "$d/$v.$ep.$(basename "$led")"
      done
    done
    [ -f "$fam_dir/usage.jsonl" ] && cp "$fam_dir/usage.jsonl" "$d/usage.jsonl"
  done
done
python3 "$HERE/summarize.py" "$ROOT" --json "$OUT/summary.json" --md "$OUT/summary.md" > /dev/null
(cd "$OUT" && find . -type f ! -name MANIFEST.md | sort | xargs sha256sum > MANIFEST.md)
echo "published $(find "$OUT" -type f | wc -l | tr -d ' ') files to $OUT"
