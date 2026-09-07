#!/usr/bin/env python3
"""Package what travels from a run root into the repo, and nothing else.

    publish.py RUNROOT OUTDIR

What a reader needs to check a number is the benchmark's own
`sequence_comparison.json` (the Δ), the per-variant bucket summaries, the
governance ledgers, the token meters, and the spend bounds. What they do
not need is every episode's transcript: the first version of this copied
`sequence_summary.json` whole and produced **406 MB** for four runs, 173 MB
of it episode records. So:

- `sequence_comparison.json` — copied whole, it IS the result;
- `sequence_summary.json` — reduced to the family/bucket summary plus one
  line per episode (id, bucket, score, passed, turns, tokens); the response
  texts and artifact dumps stay on the box;
- `areev_ledger.json` / `mem0_ledger.json` — governance decisions with the
  evidence the reviewer saw, the memory operations, the counts; the
  composed system prompt and the tool schemas (5 KB and 2.4 KB of the 8.4 KB,
  identical across a family) are dropped;
- `usage.jsonl`, `spend.jsonl` — the meters, whole;
- a `MANIFEST.md` of sha256s over everything written.

Runs live on the box; only this travels.
"""
from __future__ import annotations

import hashlib
import json
import shutil
import sys
from pathlib import Path

EPISODE_KEEP = ("index", "task_id", "label", "bucket", "episode_kind", "family_id", "mechanism",
                "task_score", "passed", "total_turns", "total_tokens", "scores", "artifact_diff",
                "internal_tools", "expected_persistence_signal", "infra_blocked")
LEDGER_DROP = ("system_prompt", "task_tools")


def slim_episode(e: dict) -> dict:
    out = {k: e.get(k) for k in EPISODE_KEEP if k in e}
    # `internal_tools.calls` is every tool call with its arguments — 258 KB
    # of one 280 KB episode record. The COUNTS beside it are what the
    # mechanism score is computed from; keep those, drop the transcript.
    it = out.get("internal_tools")
    if isinstance(it, dict):
        out["internal_tools"] = {k: v for k, v in it.items() if k != "calls"}
        out["internal_tools"]["calls_dropped"] = len(it.get("calls") or [])
    return out


def slim_summary(src: Path, dst: Path) -> None:
    try:
        d = json.loads(src.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError):
        return
    eps = d.get("episodes")
    if isinstance(eps, list):
        d["episodes"] = [slim_episode(e) for e in eps]
    dst.write_text(json.dumps(d, indent=1), encoding="utf-8")


def slim_comparison(src: Path, dst: Path) -> None:
    """`sequence_comparison.json` IS the result, but it embeds each variant's
    full episode records under `family_summary`. Keep `delta` and every
    bucket summary — what `summarize.py` and `stats.py` read — and slim the
    episodes the same way."""
    try:
        d = json.loads(src.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError):
        shutil.copy2(src, dst)
        return
    for variant in ("with_persistence", "without_persistence"):
        v = d.get(variant)
        if not isinstance(v, dict):
            continue
        if isinstance(v.get("episodes"), list):
            v["episodes"] = [slim_episode(e) for e in v["episodes"]]
        for fam in (v.get("family_summary") or {}).values():
            if isinstance(fam, dict) and isinstance(fam.get("episodes"), list):
                fam["episodes"] = [slim_episode(e) for e in fam["episodes"]]
    dst.write_text(json.dumps(d, indent=1), encoding="utf-8")


def slim_ledger(src: Path, dst: Path) -> None:
    try:
        d = json.loads(src.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError):
        return
    for k in LEDGER_DROP:
        d.pop(k, None)
    dst.write_text(json.dumps(d, indent=1), encoding="utf-8")


def main() -> None:
    root, out = Path(sys.argv[1]), Path(sys.argv[2])
    # `--repo` writes the set that goes INTO the tree: the governance
    # ledgers (2 MB over 4,908 episodes), the token meters, and the
    # aggregates. The benchmark's own per-family comparison and summary
    # files are 129 MB and stay on the box; their checksums are in the
    # manifest either way, so a published number can still be traced to the
    # file it came from.
    repo_only = "--repo" in sys.argv[3:]
    out.mkdir(parents=True, exist_ok=True)
    for name in ("spend.jsonl", "summary.json", "summary.md"):
        if (root / name).exists():
            shutil.copy2(root / name, out / name)
    n = 0
    for agent_dir in sorted(p for p in root.iterdir() if p.is_dir()):
        for fam_dir in sorted(p for p in agent_dir.iterdir() if p.is_dir()):
            cmp_ = fam_dir / "sequence_comparison.json"
            if not cmp_.exists():
                continue
            d = out / agent_dir.name / fam_dir.name
            d.mkdir(parents=True, exist_ok=True)
            if not repo_only:
                slim_comparison(cmp_, d / cmp_.name)
            n += 1
            if (fam_dir / "usage.jsonl").exists():
                shutil.copy2(fam_dir / "usage.jsonl", d / "usage.jsonl")
            for variant in ("with_persistence", "without_persistence"):
                v = fam_dir / variant
                if not repo_only and (v / "sequence_summary.json").exists():
                    slim_summary(v / "sequence_summary.json", d / f"{variant}.sequence_summary.json")
                # one ledger file per family per variant, episodes inside:
                # one file per EPISODE was 4,908 files for 4.2 MB of content
                merged = {}
                for led in sorted(v.glob("0*/artifacts/*_ledger.json")):
                    try:
                        rec = json.loads(led.read_text(encoding="utf-8"))
                    except (OSError, json.JSONDecodeError):
                        continue
                    for k in LEDGER_DROP:
                        rec.pop(k, None)
                    merged[led.parent.parent.name] = rec
                if merged:
                    kind = "mem0" if any("mem0" in p.name for p in v.glob("0*/artifacts/*_ledger.json")) else "areev"
                    (d / f"{variant}.{kind}_ledgers.json").write_text(
                        json.dumps(merged, indent=1), encoding="utf-8")
    lines = []
    for f in sorted(p for p in out.rglob("*") if p.is_file() and p.name != "MANIFEST.md"):
        lines.append("%s  %s" % (hashlib.sha256(f.read_bytes()).hexdigest(), f.relative_to(out)))
    (out / "MANIFEST.md").write_text("\n".join(lines) + "\n", encoding="utf-8")
    size = sum(p.stat().st_size for p in out.rglob("*") if p.is_file())
    print("published %d families, %d files, %.1f MB to %s" % (n, len(lines) + 1, size / 1048576, out))


if __name__ == "__main__":
    main()
