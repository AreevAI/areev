#!/usr/bin/env python3
"""Turn a governed memory into a training corpus for a small model.

    slm_corpus.py --learned-db LEDGER --dataset D --seed S --experience 40 --eval 60
                  --out DIR [--valid 4]

The corpus is the ledger's own record of the deployment: for every
experience document, the text the agent saw and the ROW THE ACCOUNTANT
FILED — the builder's truth with the accountant's recorded corrections laid
over it — never the agent's guesses. Every document contributes, whether or
not it needed correcting; a ledger has a row for each one it processed. The system prompt is the
day-one instruction plus the approved lessons as they stood at the end of
the run, so the SLM is taught the same thing the governed prompt teaches the
LLM, and the comparison is like for like.

What this is NOT: `areev tune --select`, the product path, exports
trajectory Events, and this harness journals Observations and Facts rather
than the agent's turns. That is a gap in the harness, recorded here rather
than papered over; the rows below are what that export would contain once
the harness journals its turns.

Held-out documents never enter the corpus. The validation split mlx_lm
requires is carved from the EXPERIENCE set, so the evaluation set stays
untouched for the paired comparison.
"""
import argparse
import json
import os
import random
import re
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import agent
import dataset
import ledger_profile
import memory as mem


def filed_rows(db, fields):
    """{seq: {field: value}} from the document_NNNN facts, read per relation
    so a long deployment stays under CAL's grain cap."""
    return mem.document_facts(db, fields)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--profile", default="sroie")
    ap.add_argument("--learned-db", required=True)
    ap.add_argument("--dataset", required=True)
    ap.add_argument("--seed", type=int, default=1)
    ap.add_argument("--experience", type=int, default=40)
    ap.add_argument("--eval", type=int, default=60)
    ap.add_argument("--out", required=True)
    ap.add_argument("--valid", type=int, default=4)
    ap.add_argument("--since-seq", type=int, default=0,
                    help="only experience documents after this seq -- the DELTA a continually "
                         "tuned model sees at a checkpoint, against the accumulated corpus a "
                         "from-scratch one sees")
    ap.add_argument("--upto-seq", type=int, default=0, help="only experience documents up to this seq")
    args = ap.parse_args()

    profile = ledger_profile.get(args.profile)
    experience, heldout = dataset.split_for(profile, dataset.load(args.dataset), args.seed, args.experience, args.eval)
    # By TEXT, not by id: an id is the filename cut to 40 characters and a
    # few filings share one, so seed 3's checkpoint 160 tripped this guard on
    # a different document that merely shared a name with a held-out one.
    # The split partitions by registrant, so a real leak would be a bug in
    # dataset.split_entity; this is the safety net for that, keyed by what a
    # document is.
    held_ids = {r["text"] for r in heldout}

    filed = mem.with_memory(args.learned_db, mem.REVIEWER, lambda db: filed_rows(db, profile["fields"]))
    lessons = mem.with_memory(args.learned_db, mem.REVIEWER, lambda _db: mem.lessons_markdown(_db, profile))
    system = agent.base_instruction(profile)
    if lessons.strip():
        system += "\n\n" + lessons

    rows = []
    skipped = 0
    for r in experience:
        if args.since_seq and r["seq"] <= args.since_seq:
            continue
        if args.upto_seq and r["seq"] > args.upto_seq:
            continue
        assert r["text"] not in held_ids, "held-out document in the training corpus"
        # EVERY experience document contributes its filed row. The corpus used
        # to hold only documents the accountant had corrected, and once the
        # loop's rules made the agent right most of the time that left a small,
        # hard-case-biased set: 10 rows from 20 documents on the first real
        # checkpoint, and a tuned model at 47% against the LLM's 86%. A ledger
        # has a filed row for every document it processed; that is the corpus.
        # The accountant's corrections override the builder's truth where they
        # differ, which is what "filed" means.
        final_req = ledger_profile.required_fields(profile, 10 ** 9)
        target = {k: r["truth"][k] for k in final_req if r["truth"].get(k)}
        target.update(filed.get(r["seq"], {}))
        if not target:
            skipped += 1
            continue
        rows.append({"messages": [
            {"role": "system", "content": system},
            {"role": "user", "content": profile["document_noun"].upper() + ":\n\n" + (r["text"] or "").strip()[:12000]},
            {"role": "assistant", "content": json.dumps({"fields": target, "park": False, "reason": ""})},
        ]})

    random.Random(args.seed).shuffle(rows)
    valid, train = rows[:args.valid], rows[args.valid:]
    os.makedirs(args.out, exist_ok=True)
    for name, part in (("train", train), ("valid", valid), ("test", valid)):
        with open(os.path.join(args.out, name + ".jsonl"), "w", encoding="utf-8") as fh:
            for x in part:
                fh.write(json.dumps(x, ensure_ascii=False) + "\n")
    with open(os.path.join(args.out, "corpus.manifest.json"), "w") as fh:
        json.dump({"learned_db": os.path.abspath(args.learned_db), "seed": args.seed,
                   "experience": args.experience, "eval": args.eval, "train": len(train),
                   "valid": len(valid), "rules_in_system_prompt": lessons.count("\n- "),
                   "since_seq": args.since_seq, "upto_seq": args.upto_seq,
                   "held_out_excluded": len(held_ids)}, fh, indent=1)
    print("corpus: %d train, %d valid, %d rule(s) in the system prompt, %d document(s) without a filed row -> %s"
          % (len(train), len(valid), lessons.count("\n- "), skipped, args.out))


if __name__ == "__main__":
    sys.exit(main())
