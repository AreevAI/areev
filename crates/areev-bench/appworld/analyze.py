#!/usr/bin/env python3
"""The paired comparison between arms, and the checks that make it readable.

    python3 analyze.py --root ~/mg/local/appworld --dataset test_normal \
        --arm A=areev/arm-a-none/<model>/test_normal \
        --arm B=areev/arm-b-passive/<model>/test_normal \
        --arm B2=areev/arm-b2-passive/<model>/test_normal \
        --arm C=areev/arm-c-governed/<model>/test_normal \
        --compare B:A --compare C:B --floor B2:B

Arms are paired BY TASK ID, never by position, so a missing or reordered task
cannot silently mispair two different streams. The statistic is
`aba_stats.mcnemar_exact` -- imported, not reimplemented, so every paired
number this repository publishes comes from one function that CI selftests.

Three guards, because the ways this goes quietly wrong are known:

  - an arm whose task count differs from the dataset's is REFUSED, not
    averaged. A score over a smaller denominator is the failure mode that
    survived a whole parallel run undetected;
  - arms are compared only over tasks present in BOTH, and the count is
    printed, so a partial comparison announces itself;
  - the B-vs-B2 floor is printed FIRST. An effect that does not clear two
    identical passes of the same arm is not an effect, and reading it first
    makes that impossible to forget.
"""
from __future__ import annotations

import argparse
import json
import os
import sys

_HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, os.path.join(_HERE, "..", "scripts"))
# Wilcoxon lives in persist's stats.py, which is where this program first
# needed it. Imported rather than rewritten: one implementation of a
# statistic, per ../CLAUDE.md. It belongs in scripts/aba_stats.py beside
# mcnemar_exact -- a move for whoever next edits that track.
sys.path.insert(0, os.path.join(_HERE, "..", "persist", "pastbench"))
import aba_stats  # noqa: E402
from stats import wilcoxon  # noqa: E402


def load_arm(root: str, experiment: str, dataset: str) -> tuple[dict, dict]:
    """-> ({task_id: bool solved}, aggregate)."""
    base = os.path.join(root, "experiments", "outputs", experiment)
    report_path = os.path.join(base, "evaluations", f"{dataset}.json")
    if not os.path.exists(report_path):
        raise SystemExit(f"no evaluation report: {report_path}")
    with open(report_path) as fh:
        report = json.load(fh)
    solved = {task: bool(row.get("success")) for task, row in report["individual"].items()}
    return solved, report.get("aggregate", {})


def load_usage(root: str, experiment: str) -> dict:
    """Token and cost totals, plus PER-TASK tokens so cost can be paired.

    Aggregates hide what pairing shows: two arms can differ in total tokens
    because one episode ran away, which is a different claim from "this arm is
    cheaper on the typical task".
    """
    tasks_dir = os.path.join(root, "experiments", "outputs", experiment, "tasks")
    tokens_in = tokens_out = cost = 0.0
    metered = 0
    per_task: dict[str, float] = {}
    for task in sorted(os.listdir(tasks_dir)) if os.path.isdir(tasks_dir) else []:
        path = os.path.join(tasks_dir, task, "misc", "usage.json")
        if not os.path.exists(path):
            continue
        with open(path) as fh:
            usage = json.load(fh)
        task_in = usage["tokens"]["input_cache_miss"] + usage["tokens"]["input_cache_hit"]
        tokens_in += task_in
        tokens_out += usage["tokens"]["output"]
        cost += sum(usage["cost"].values())
        per_task[task] = task_in + usage["tokens"]["output"]
        metered += 1
    return {"in": tokens_in, "out": tokens_out, "cost": cost,
            "episodes": metered, "per_task": per_task}


def scenario_completion(solved: dict) -> float:
    """SGC: a scenario counts only if every one of its task variations solved."""
    scenarios: dict[str, list[bool]] = {}
    for task, ok in solved.items():
        scenarios.setdefault(task.rsplit("_", 1)[0], []).append(ok)
    if not scenarios:
        return 0.0
    return 100.0 * sum(1 for v in scenarios.values() if all(v)) / len(scenarios)


def compare_tokens(name_a: str, a: dict, name_b: str, b: dict) -> str:
    """Tokens per task, paired by task id, Wilcoxon signed-rank.

    The secondary this program cares about: PERSIST.md's one measured win was
    cost, not accuracy, so it is tested the same way here rather than eyeballed
    off two aggregates.
    """
    shared = sorted(set(a) & set(b))
    diffs = [b[t] - a[t] for t in shared]
    cheaper = sum(1 for d in diffs if d < 0)
    dearer = sum(1 for d in diffs if d > 0)
    p, n = wilcoxon(diffs)
    med_a = sorted(a[t] for t in shared)[len(shared) // 2]
    med_b = sorted(b[t] for t in shared)[len(shared) // 2]
    pct = 100.0 * (med_b - med_a) / med_a if med_a else 0.0
    p_text = "n/a" if p is None else f"{p:.4f}"
    return (
        f"  {name_b} vs {name_a}:  median {med_b:,.0f} vs {med_a:,.0f} tokens "
        f"({pct:+.1f}%)\n"
        f"      {cheaper} tasks cheaper under {name_b}, {dearer} dearer   "
        f"Wilcoxon p = {p_text}  (n={n} non-tied)"
    )


def compare(name_a: str, a: dict, name_b: str, b: dict) -> str:
    """b vs a, paired. `wins` = b solved what a did not."""
    shared = sorted(set(a) & set(b))
    wins = sum(1 for t in shared if b[t] and not a[t])
    losses = sum(1 for t in shared if a[t] and not b[t])
    p = aba_stats.mcnemar_exact(wins, losses)
    delta = 100.0 * (sum(b[t] for t in shared) - sum(a[t] for t in shared)) / max(len(shared), 1)
    partial = "" if len(shared) == len(a) == len(b) else f"  [PARTIAL: {len(shared)} of {max(len(a), len(b))}]"
    return (
        f"  {name_b} vs {name_a}:  n={len(shared)}  "
        f"{name_b} solved {sum(b[t] for t in shared)}, {name_a} solved {sum(a[t] for t in shared)}  "
        f"(delta {delta:+.1f} pts)\n"
        f"      discordant: {wins} won by {name_b}, {losses} won by {name_a}   "
        f"McNemar exact p = {p:.4f}{partial}"
    )


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--root", required=True)
    ap.add_argument("--dataset", required=True)
    ap.add_argument("--arm", action="append", required=True, metavar="NAME=EXPERIMENT")
    ap.add_argument("--compare", action="append", default=[], metavar="B:A")
    ap.add_argument("--floor", action="append", default=[], metavar="B2:B")
    ap.add_argument("--expect", type=int, default=0,
                    help="tasks the dataset should have; a mismatch is refused")
    args = ap.parse_args()

    root = os.path.expanduser(args.root)
    expected = args.expect
    if not expected:
        path = os.path.join(root, "data", "datasets", f"{args.dataset}.txt")
        if os.path.exists(path):
            with open(path) as fh:
                expected = len([line for line in fh if line.strip()])

    arms, aggregates, usages = {}, {}, {}
    for spec in args.arm:
        name, _, experiment = spec.partition("=")
        arms[name], aggregates[name] = load_arm(root, experiment, args.dataset)
        usages[name] = load_usage(root, experiment)

    print(f"dataset {args.dataset}  ({expected or '?'} tasks expected)\n")
    print(f"{'arm':<6}{'n':>6}{'solved':>8}{'TGC':>8}{'SGC':>8}{'tok/ep':>10}{'$/ep':>9}{'$ total':>9}")
    incomplete = []
    for name, solved in arms.items():
        u = usages[name]
        eps = max(u["episodes"], 1)
        print(
            f"{name:<6}{len(solved):>6}{sum(solved.values()):>8}"
            f"{100.0 * sum(solved.values()) / max(len(solved), 1):>8.1f}"
            f"{scenario_completion(solved):>8.1f}"
            f"{(u['in'] + u['out']) / eps:>10,.0f}{u['cost'] / eps:>9.4f}{u['cost']:>9.3f}"
        )
        if expected and len(solved) != expected:
            incomplete.append(f"{name} scored {len(solved)} of {expected}")

    if incomplete:
        print("\nREFUSING TO COMPARE -- an arm did not cover the dataset:")
        for line in incomplete:
            print("  " + line)
        print("A score over a smaller denominator is not a smaller result, it is a wrong one.")
        return 1

    if args.floor:
        print("\nNOISE FLOOR -- two identical passes of the same arm")
        for spec in args.floor:
            b, _, a = spec.partition(":")
            print(compare(a, arms[a], b, arms[b]))

    if args.compare:
        print("\nCOMPARISONS -- solved tasks")
        for spec in args.compare:
            b, _, a = spec.partition(":")
            print(compare(a, arms[a], b, arms[b]))

    token_pairs = list(args.floor) + list(args.compare)
    if token_pairs:
        print("\nCOMPARISONS -- tokens per episode, paired by task")
        for spec in token_pairs:
            b, _, a = spec.partition(":")
            print(compare_tokens(a, usages[a]["per_task"], b, usages[b]["per_task"]))

    print("\nRead the floor first: an effect that does not clear it is not an effect.")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
