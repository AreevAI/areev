#!/usr/bin/env python3
"""Does it matter WHERE the rules go and HOW they are written?

    structure.py --learned-db LEDGER --dataset D --seed S --workdir W
                 [--positions system-top,system-bottom,user-turn]
                 [--formats markdown,sml,toon,json]

One fixed rule set — the memory a governed run left — read against the
same held-out set under every (position, format) cell. Nothing is learned
here; this is the measurement the loop would need before it could PROPOSE a
template rewrite (a DEFINE TEMPLATE recommendation), which is where Areev
lets learning reach the prompt's structure and not only its content.

Positions:
  system-bottom  the rules after the day-one instruction (the published arm B)
  system-top     the rules before it
  user-turn      the rules at the head of the user message, ahead of the receipt

Formats: the memory rendered by CAL's own renderers — `FORMAT markdown`,
`FORMAT sml`, `FORMAT toon` — plus `json`. The markdown cell is not the
published prompt byte-for-byte (that one is hand-assembled in memory.py);
it is what the same grains look like through CAL, which is the surface a
template rewrite would change.

Every cell pairs against the baseline cell (system-bottom, markdown) per
(document, field), McNemar exact, so a structural win has to survive the
same test a learned rule does.
"""
import argparse
import json
import os
import sys
from math import comb

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import accountant as acct
import agent
import dataset
import ledger_profile
import memory as mem

BASE_CELL = ("system-bottom", "markdown")
FORMATS = ("markdown", "json", "xml", "toon", "cal-markdown")


def mcnemar(b, c):
    n = b + c
    if n == 0:
        return 1.0
    k = min(b, c)
    return min(1.0, 2 * sum(comb(n, i) for i in range(k + 1)) / (2 ** n))


def rule_texts(db):
    return [(g["fields"].get("object") or "").strip()
            for g in mem._lessons(db) if (g["fields"].get("object") or "").strip()]


def render_rules(db, fmt, profile=None):
    """The SAME rule texts, in different clothes.

    `markdown` is arm B's prompt section byte-for-byte (memory.lessons_markdown),
    which makes the baseline cell a replication of the published arm and not
    merely a cousin of it. The other formats carry identical text and the
    identical header; only the container changes. `cal-markdown` is the one
    exception on purpose: it is what CAL's own renderer emits for these
    grains -- each rule prefixed with its subject and relation and suffixed
    with confidence and date -- so the cost of that decoration is a measured
    cell rather than a confound folded into every other one. (The first run
    of this grid folded it in: a seven-rule memory scored 35 under CAL's
    markdown against 141 under the hand-assembled prompt.)"""
    md = mem.lessons_markdown(db, profile)
    head = md.split("\n- ", 1)[0] + "\n"
    rules = sorted(set(rule_texts(db)))
    if fmt == "markdown":
        return md
    if fmt == "json":
        return head + json.dumps({"rules": rules}, indent=1, ensure_ascii=False) + "\n"
    if fmt == "xml":
        return head + "<rules>\n" + "".join("  <rule>%s</rule>\n" % r.replace("&", "&amp;").replace("<", "&lt;") for r in rules) + "</rules>\n"
    if fmt == "toon":
        return head + "rules[%d]{text}:\n" % len(rules) + "".join(json.dumps(r, ensure_ascii=False) + "\n" for r in rules)
    if fmt == "cal-markdown":
        q = 'RECALL facts WHERE namespace = "%s" AND relation = "lesson" LIMIT %d FORMAT markdown' % (mem.NS, mem.CAP)
        return head + json.loads(db.cal(q))["text"].rstrip() + "\n"
    raise SystemExit("unknown format %s" % fmt)


def assemble(profile, position, section, document_text):
    base = agent.base_instruction(profile)
    body = (document_text or "").strip() or "(no extractable text — this document is a scan or an image)"
    user = profile["document_noun"].upper() + ":\n\n" + body[:12000]
    if position == "system-bottom":
        system = base + "\n\n" + section
    elif position == "system-top":
        system = section + "\n\n" + base
    elif position == "user-turn":
        system = base
        user = section + "\n" + user
    else:
        raise SystemExit("unknown position %s" % position)
    return [{"role": "system", "content": system}, {"role": "user", "content": user}]


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--profile", default="sroie")
    ap.add_argument("--learned-db", required=True)
    ap.add_argument("--dataset", required=True)
    ap.add_argument("--seed", type=int, default=1)
    ap.add_argument("--experience", type=int, default=40)
    ap.add_argument("--eval", type=int, default=60)
    ap.add_argument("--workdir", required=True)
    ap.add_argument("--positions", default="system-bottom,system-top,user-turn")
    ap.add_argument("--formats", default=",".join(FORMATS))
    args = ap.parse_args()

    os.makedirs(args.workdir, exist_ok=True)
    os.environ.setdefault("AREEV_USAGE_LOG", os.path.join(args.workdir, "usage.jsonl"))
    profile = ledger_profile.get(args.profile)
    agent_argv = os.environ["AGENT_CMD"].split()
    _, rows = dataset.split_for(profile, dataset.load(args.dataset), args.seed, args.experience, args.eval)

    # Read a COPY: the source ledger may be open elsewhere, and a reader must
    # never be the second handle on a live memory.
    db = os.path.join(args.workdir, "rules.db")
    mem.copy_memory(args.learned_db, db)
    sections = {fmt: mem.with_memory(db, mem.REVIEWER, lambda d, f=fmt: render_rules(d, f, profile))
                for fmt in args.formats.split(",")}
    with open(os.path.join(args.workdir, "sections.json"), "w", encoding="utf-8") as fh:
        json.dump(sections, fh, indent=1, ensure_ascii=False)

    cells, trials_by = {}, {}
    for pos in args.positions.split(","):
        for fmt, section in sections.items():
            key = "%s|%s" % (pos, fmt)
            print("\n=== %s — %d chars of rules" % (key, len(section)))
            trials = []
            for r in rows:
                req = ledger_profile.required_fields(profile, r["seq"])
                try:
                    content, usage = agent.call_model(agent_argv, assemble(profile, pos, section, r["text"]))
                    out = agent.parse_reply(content)
                except Exception as e:
                    print("  seq %3d  MODEL CALL FAILED (%s)" % (r["seq"], type(e).__name__))
                    out, usage = {"fields": {}, "park": True, "reason": "model call failed"}, {}
                for field in req:
                    want = r["truth"].get(field, "")
                    if not want:
                        continue
                    got = (out["fields"] or {}).get(field, "")
                    ex, sem = acct.compare(profile, field, got, want)
                    trials.append({"cell": key, "seq": r["seq"], "field": field,
                                   "exact": bool(ex), "semantic": bool(sem), "got": got, "want": want})
            ex = sum(t["exact"] for t in trials)
            print("  %s: exact %d/%d" % (key, ex, len(trials)))
            cells[key] = {"position": pos, "format": fmt, "exact": ex, "semantic": sum(t["semantic"] for t in trials),
                          "n": len(trials), "chars": len(section)}
            trials_by[key] = {"%s|%s" % (t["seq"], t["field"]): t for t in trials}
            with open(os.path.join(args.workdir, "trials.%s.%s.json" % (pos, fmt)), "w") as fh:
                json.dump(trials, fh, indent=1)

    base = trials_by.get("%s|%s" % BASE_CELL)
    if base:
        for key, t in trials_by.items():
            keys = set(t) & set(base)
            w = sum(1 for k in keys if t[k]["exact"] and not base[k]["exact"])
            l = sum(1 for k in keys if base[k]["exact"] and not t[k]["exact"])
            cells[key]["vs_base"] = {"wins": w, "losses": l, "p": round(mcnemar(w, l), 6)}
    with open(os.path.join(args.workdir, "structure.summary.json"), "w") as fh:
        json.dump({"seed": args.seed, "base_cell": "|".join(BASE_CELL), "cells": cells}, fh, indent=1)
    print("\n| position | format | exact | vs base (W/L, p) | chars |")
    print("|---|---|---:|---|---:|")
    for key, c in sorted(cells.items(), key=lambda kv: -kv[1]["exact"]):
        v = c.get("vs_base", {})
        print("| %s | %s | %d/%d | %s | %d |" % (c["position"], c["format"], c["exact"], c["n"],
                                              ("%d/%d, p=%.3f" % (v["wins"], v["losses"], v["p"])) if v else "—", c["chars"]))


if __name__ == "__main__":
    sys.exit(main())
