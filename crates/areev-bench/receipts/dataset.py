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


def split_entity(rows, seed, experience, eval_n, key="entity", order_by="filed_at"):
    """(experience_rows, unseen_rows, seen_rows), each with a 1-based `seq`.

    Entities (a registrant, a vendor) are shuffled by the seed and a set of
    them is held out until it holds at least `eval_n` documents. The UNSEEN
    evaluation set is drawn from those entities only, so no document in it
    shares an organisation with anything the agent learned from -- that is
    the memorisation control, built in. The experience stream is `experience`
    documents sampled across the remaining entities and then SORTED by
    `order_by`, so the deployment runs in real time across the corpus's
    span. The SEEN evaluation set is the same size, drawn from experience
    entities' documents that were not in the stream: its gap from the unseen
    set is what familiarity buys, measured at every checkpoint."""
    rng = random.Random(seed)
    by_ent = {}
    for r in rows:
        by_ent.setdefault(r[key], []).append(r)
    ents = sorted(by_ent)
    rng.shuffle(ents)
    held, n = [], 0
    for e in ents:
        if n >= eval_n:
            break
        held.append(e)
        n += len(by_ent[e])
    held = set(held)
    unseen_pool = [r for e in held for r in by_ent[e]]
    exp_pool = [r for e in ents if e not in held for r in by_ent[e]]
    rng.shuffle(unseen_pool)
    rng.shuffle(exp_pool)
    exp = sorted(exp_pool[:experience], key=lambda r: r[order_by])
    # SEEN means the agent learned from this organisation: only entities
    # that actually appear in the stream qualify, not every non-held-out one.
    in_stream = {r[key] for r in exp}
    seen_pool = [r for r in exp_pool[experience:] if r[key] in in_stream]
    unseen = unseen_pool[:eval_n]
    seen = seen_pool[:eval_n]
    out = []
    seq = 1
    for part in (exp, unseen, seen):
        part = [dict(r) for r in part]
        for r in part:
            r["seq"] = seq
            seq += 1
        out.append(part)
    return tuple(out)


def split_for(profile, rows, seed, experience, eval_n, holdout="unseen", upto_seq=0):
    """The split a profile asks for. Without `holdout_key` this IS split(),
    byte for byte, so every published run is untouched.

    `holdout="train"` returns a seeded sample of the EXPERIENCE stream itself
    (up to `upto_seq` when given): the documents the adapter was trained on.
    Reading those is the memorisation check -- train-set accuracy against
    unseen-set accuracy -- and, because the rows keep their `seq`, the
    continual path's accuracy on rows from earlier checkpoints against the
    latest ones is a direct forgetting measure."""
    if not profile.get("holdout_key"):
        exp, held = split(rows, seed, experience, eval_n)
    else:
        exp, unseen, seen = split_entity(rows, seed, experience, eval_n,
                                         key=profile["holdout_key"], order_by=profile.get("order_by", "seq"))
        held = seen if holdout == "seen" else unseen
    if holdout == "train":
        pool = [r for r in exp if not upto_seq or r["seq"] <= upto_seq]
        rng = random.Random(seed * 7919 + (upto_seq or 0))
        rng.shuffle(pool)
        held = sorted(pool[:eval_n], key=lambda r: r["seq"])
    return exp, held
