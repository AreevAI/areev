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

States are DISCOVERED from the run directory, not enumerated, so the
passive-memory arms (SELFIMPROVE.md "Bench 2") are picked up without editing
this script. Every discovered M arm is additionally paired against A0 (does
passive recall beat no memory?) and against B (does curation beat retrieval?)
— the pre-registered comparisons.

Stdlib only. Multiple RUNDIRs are treated as independent seeds and pooled
per state pair, with each seed also reported on its own line.
"""
import glob
import json
import math
import os
import sys

EVAL_PREFIX = "transcripts-eval-"
EVAL_SUFFIX = ".jsonl"
GOVERNED_PAIRS = [("A0", "B"), ("B", "A1"), ("A1", "B2"), ("A0", "A1"), ("B", "B2")]


def pairs_for(states):
    """Governed pairs first, then each discovered M arm vs A0 and vs B.

    An arm is any state whose name starts with M. The match is
    case-insensitive because the frozen arm labels are lowercase
    (`m-steel`, `m-all`, `m-llm`, `m-cmd`) while the governed states are
    upper — a case-sensitive test would silently report nothing.
    Pairs whose states are absent everywhere are dropped, not printed empty.
    """
    present = set(states)
    out = [p for p in GOVERNED_PAIRS if p[0] in present and p[1] in present]
    for arm in sorted(s for s in present if s.upper().startswith("M")):
        out.extend((base, arm) for base in ("A0", "B") if base in present)
    return out


def load_runs(d):
    """[(name, {state: {task_id: success}})] for one directory.

    Two directory layouts exist and both must work:

      a FRESH run     transcripts-eval-B.jsonl          → one run
      a PUBLISHED set seed1.transcripts-eval-B.jsonl    → one run per prefix
                      seed2.transcripts-eval-B.jsonl

    The published layout is what `results/` actually contains: several seeds
    collected into one directory and prefixed. Globbing only the unprefixed
    form silently found nothing there, so the paired statistics RESULTS.md
    quotes could not be regenerated from the committed evidence — and the
    "no task_outcome rows" message blamed the run's age rather than the glob.

    Each prefix stays its OWN run: seeds are pooled by `compare` across runs,
    which is only valid if each seed's task ids are paired within that seed.
    Merging them into one dict would collide identical task ids across seeds.
    """
    # A directory takes every run inside it. A path that NAMES the prefix
    # (`results/SET/governed-s1`) selects one run out of a committed set —
    # needed when a set holds two configurations that must not be pooled
    # into each other, which passing the directory would silently do.
    if os.path.isdir(d):
        base, sel = d, "*"
    else:
        base, sel = os.path.dirname(d) or ".", os.path.basename(d) + "."
    by_prefix = {}
    for path in sorted(glob.glob(os.path.join(base, sel + EVAL_PREFIX + "*" + EVAL_SUFFIX))):
        name = os.path.basename(path)
        marker = name.index(EVAL_PREFIX)
        prefix = name[:marker]
        state = name[marker + len(EVAL_PREFIX):-len(EVAL_SUFFIX)]
        if not state:
            continue
        by_prefix.setdefault(prefix, {})[state] = path

    label = os.path.basename(os.path.normpath(d)) or d
    runs = []
    for prefix in sorted(by_prefix):
        out = _load_states(by_prefix[prefix])
        if not out:
            continue
        # `label` already IS the prefix when one was named, so only a
        # directory's runs get qualified by their prefix.
        name = label if sel != "*" or not prefix else f"{label}/{prefix.rstrip('.')}"
        runs.append((name, out))
    return runs


def _load_states(paths_by_state):
    """{state: {task_id: success}} for one run's transcript set."""
    out = {}
    for state, path in sorted(paths_by_state.items()):
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
    runs = []
    missing = []
    for d in dirs:
        found = load_runs(d)
        if found:
            runs.extend(found)
        else:
            missing.append(os.path.basename(os.path.normpath(d)) or d)
    if missing:
        print(
            f"aba_stats: no task_outcome rows in: {', '.join(missing)}\n"
            f"(looked for [prefix.]{EVAL_PREFIX}STATE{EVAL_SUFFIX}; runs made before "
            "per-task rows were recorded only have aggregates)",
            file=sys.stderr,
        )
    if not runs:
        sys.exit(1)

    states = set()
    for _, r in runs:
        states |= set(r)
    print(f"paired McNemar over {len(runs)} run(s): {', '.join(n for n, _ in runs)}")
    print(f"states: {', '.join(sorted(states))}\n")
    for x, y in pairs_for(states):
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
