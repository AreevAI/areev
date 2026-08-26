#!/usr/bin/env python3
"""Paired statistics over one or more selfimprove_aba run directories.

    aba_stats.py RUNDIR [RUNDIR ...]

Reads the `task_outcome` rows from each run's `transcripts-eval-*.jsonl` and
pairs them BY TASK across states — the same held-out instances are run in
every state, so the paired test is the right one and an unpaired proportion
test would understate significance.

Reports, per state pair:
  - discordant counts (b = won only in the second state, c = lost only)
  - McNemar exact two-sided p (binomial, no chi-square approximation, so
    small discordant counts stay honest)

The A/B/A/B claim needs all three to hold together:
  A0→B   improvement is real            (p small, b > c)
  B→A1   removing the lessons undoes it (the causal step)
  A1→B2  restoring them recovers it

Stdlib only. Multiple RUNDIRs are treated as independent seeds and pooled
per state pair, with each seed also reported on its own line.
"""
import json
import math
import os
import sys

STATES = ["A0", "B", "A1", "B2"]
PAIRS = [("A0", "B"), ("B", "A1"), ("A1", "B2"), ("A0", "A1"), ("B", "B2")]


def load_run(d):
    """{state: {task_id: success}} for one run directory."""
    out = {}
    for state in STATES:
        path = os.path.join(d, f"transcripts-eval-{state}.jsonl")
        if not os.path.exists(path):
            continue
        rows = {}
        with open(path) as fh:
            for line in fh:
                line = line.strip()
                if not line:
                    continue
                try:
                    r = json.loads(line)
                except json.JSONDecodeError:
                    continue
                if r.get("kind") == "task_outcome":
                    rows[r["task_id"]] = bool(r["success"])
        if rows:
            out[state] = rows
    return out


def mcnemar_exact(b, c):
    """Two-sided exact McNemar p over the b+c discordant pairs."""
    n = b + c
    if n == 0:
        return 1.0
    k = min(b, c)
    tail = sum(math.comb(n, i) for i in range(k + 1)) / (2 ** n)
    return min(1.0, 2 * tail)


def compare(runs, x, y):
    """Pooled + per-seed discordant counts for state x vs state y."""
    pooled_b = pooled_c = pooled_n = 0
    lines = []
    for name, r in runs:
        if x not in r or y not in r:
            continue
        shared = sorted(set(r[x]) & set(r[y]))
        b = sum(1 for t in shared if not r[x][t] and r[y][t])
        c = sum(1 for t in shared if r[x][t] and not r[y][t])
        pooled_b += b
        pooled_c += c
        pooled_n += len(shared)
        lines.append(
            f"    {name}: n={len(shared)} {x}={sum(r[x][t] for t in shared)} "
            f"{y}={sum(r[y][t] for t in shared)} b={b} c={c} p={mcnemar_exact(b, c):.4f}"
        )
    return pooled_b, pooled_c, pooled_n, lines


def main():
    dirs = sys.argv[1:]
    if not dirs:
        print(__doc__.strip().splitlines()[2].strip(), file=sys.stderr)
        sys.exit(2)
    runs = [(os.path.basename(os.path.normpath(d)) or d, load_run(d)) for d in dirs]
    missing = [n for n, r in runs if not r]
    if missing:
        print(
            f"aba_stats: no task_outcome rows in: {', '.join(missing)}\n"
            "(runs made before per-task rows were recorded only have aggregates)",
            file=sys.stderr,
        )
    runs = [(n, r) for n, r in runs if r]
    if not runs:
        sys.exit(1)

    print(f"paired McNemar over {len(runs)} run(s): {', '.join(n for n, _ in runs)}\n")
    for x, y in PAIRS:
        b, c, n, lines = compare(runs, x, y)
        if n == 0:
            continue
        p = mcnemar_exact(b, c)
        direction = "improved" if b > c else ("regressed" if c > b else "no change")
        print(f"  {x} → {y}: n={n} b={b} c={c} p={p:.4f}  ({direction})")
        if len(lines) > 1:
            for line in lines:
                print(line)
    print(
        "\n  b = tasks that failed in the first state and passed in the second."
        "\n  c = the reverse. Only discordant pairs carry information."
    )


if __name__ == "__main__":
    main()
