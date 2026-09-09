#!/usr/bin/env python3
"""Build the ceiling-probe dataset: N tasks per difficulty, distinct scenarios.

Deterministic by construction -- scenarios sorted, first variation only, no
randomness -- so the probe set is a fact about the split rather than a draw,
and re-running this reproduces the same file.

    python3 make_probe_set.py --root ~/mg/local/appworld \
        --split dev --per-difficulty 4 --name dev_probe12

Writes `<root>/data/datasets/<name>.txt`, the format `load_task_ids` reads.
"""
from __future__ import annotations

import argparse
import json
import os


def difficulty_of(root: str, task_id: str) -> int | None:
    path = os.path.join(root, "data", "tasks", task_id, "ground_truth", "metadata.json")
    if not os.path.exists(path):
        return None
    with open(path) as f:
        return json.load(f).get("difficulty")


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--root", required=True)
    ap.add_argument("--split", default="dev")
    ap.add_argument("--per-difficulty", type=int, default=4)
    ap.add_argument("--name", required=True)
    args = ap.parse_args()

    root = os.path.expanduser(args.root)
    with open(os.path.join(root, "data", "datasets", f"{args.split}.txt")) as f:
        task_ids = [line.strip() for line in f if line.strip()]

    graded: list[tuple[str, int]] = []
    for task_id in sorted(task_ids):
        difficulty = difficulty_of(root, task_id)
        if difficulty is None:
            raise SystemExit(
                f"{task_id}: no ground-truth metadata, so difficulty is unknown. "
                "Stratification needs a split that ships it (train/dev)."
            )
        graded.append((task_id, difficulty))

    # Distinct scenarios first, so the probe reads breadth rather than the same
    # scenario three times. A difficulty with too few scenarios to fill its
    # quota then takes further variations -- and says so, because a bucket that
    # is one scenario wide cannot speak for a difficulty level.
    by_difficulty: dict[int, list[str]] = {}
    narrow: list[str] = []
    for difficulty in sorted({d for _, d in graded}):
        pool = [t for t, d in graded if d == difficulty]
        seen: set[str] = set()
        chosen_here: list[str] = []
        for pass_distinct in (True, False):
            for task_id in pool:
                if len(chosen_here) >= args.per_difficulty:
                    break
                scenario = task_id.rsplit("_", 1)[0]
                if pass_distinct and scenario in seen:
                    continue
                if task_id in chosen_here:
                    continue
                chosen_here.append(task_id)
                seen.add(scenario)
        by_difficulty[difficulty] = chosen_here
        scenarios = {t.rsplit("_", 1)[0] for t in chosen_here}
        if len(chosen_here) < args.per_difficulty or len(scenarios) < len(chosen_here):
            narrow.append(
                f"difficulty {difficulty}: {len(chosen_here)} tasks from "
                f"{len(scenarios)} scenario(s) -- the split has no more"
            )

    chosen = [t for d in sorted(by_difficulty) for t in by_difficulty[d]]
    out = os.path.join(root, "data", "datasets", f"{args.name}.txt")
    with open(out, "w") as f:
        f.write("\n".join(chosen) + "\n")

    for d in sorted(by_difficulty):
        print(f"difficulty {d}: {len(by_difficulty[d])} -> {' '.join(by_difficulty[d])}")
    for line in narrow:
        print(f"NARROW  {line}")
    print(f"{len(chosen)} tasks written to {out}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
