#!/usr/bin/env python3
"""Turn a governed memory into a training corpus for a small model.

    slm_corpus.py --learned-db LEDGER --dataset D --seed S --experience 40 --eval 60
                  --out DIR [--valid 4]

The corpus is the ledger's own record of the deployment: for every
experience document, the receipt text the agent saw and the ROW THE
ACCOUNTANT FILED — the document_NNNN facts record_correction() wrote, which
are the corrected values, not the agent's guesses. The system prompt is the
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
    args = ap.parse_args()

    profile = ledger_profile.get(args.profile)
    experience, heldout = dataset.split_for(profile, dataset.load(args.dataset), args.seed, args.experience, args.eval)
    held_ids = {r["id"] for r in heldout}

    filed = mem.with_memory(args.learned_db, mem.REVIEWER, lambda db: filed_rows(db, profile["fields"]))
    lessons = mem.with_memory(args.learned_db, mem.REVIEWER, mem.lessons_markdown)
    system = agent.base_instruction(profile)
    if lessons.strip():
        system += "\n\n" + lessons

    rows = []
    for r in experience:
        assert r["id"] not in held_ids, "held-out document in the training corpus"
        row = filed.get(r["seq"])
        if not row:
            continue  # the accountant never stated a corrected value for it
        # The complete filed row: what the accountant corrected, plus what the
        # truth holds for fields required by the end of the arc. This is the
        # ledger's row, not the agent's.
        final_req = ledger_profile.required_fields(profile, 10 ** 9)
        target = {k: r["truth"][k] for k in final_req if r["truth"].get(k)}
        target.update(row)
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
                   "held_out_excluded": len(held_ids)}, fh, indent=1)
    print("corpus: %d train, %d valid, %d rule(s) in the system prompt -> %s"
          % (len(train), len(valid), lessons.count("\n- "), args.out))


if __name__ == "__main__":
    sys.exit(main())
