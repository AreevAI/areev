#!/usr/bin/env python3
"""The seeded split: which receipts are experience and which are held out.

The corpus file is seed-free (built once, in corpus order). A seed permutes
it and assigns `seq` by position: the first `--experience` rows are the
experience phase, the next `--eval` rows are the held-out set. Different
seeds therefore put DIFFERENT receipts in both roles — three seeds are three
replications over three task sets, not one set re-rolled.
"""
import json
import random


def load(path):
    return [json.loads(line) for line in open(path, encoding="utf-8") if line.strip()]


def split(rows, seed, experience, eval_n):
    """(experience_rows, heldout_rows), each row given a 1-based `seq`."""
    rows = list(rows)
    random.Random(seed).shuffle(rows)
    if len(rows) < experience + eval_n:
        raise SystemExit("corpus has %d rows; need %d experience + %d held-out"
                         % (len(rows), experience, eval_n))
    for i, r in enumerate(rows, start=1):
        r["seq"] = i
    return rows[:experience], rows[experience:experience + eval_n]
