#!/usr/bin/env python3
"""Audit the governed arm's reviewer after the fact: was each refusal right?

    audit_reviewer.py ROOT/<agent> [--judge-cmd CMD] [--sample 30] [--json OUT] [--md OUT]

Reads every `areev_ledger.json` under the run root, takes each reviewer
decision that carries the evidence it was shown (ledgers written after
2026-09-07; earlier ones are listed as unauditable), and asks a SECOND
judge a narrower question than the reviewer's rubric:

    Given only this evidence, is the proposed entry a durable, general rule
    that the evidence states or that a correction in it implies?
    Answer VALID or INVALID, then one sentence.

The reviewer's verdict and the auditor's verdict are then cross-tabulated:
refusals the auditor calls VALID are the gate's false negatives; approvals
the auditor calls INVALID its false positives. A fixed random sample
(`--sample`) is written out as a checklist for a person to hand-review, so
the auditor itself can be checked. Nothing here changes any run; it grades
the gate.

The judge command speaks the bench's JSON-on-stdio chat contract
(`scripts/openrouter_toolcall.py MODEL --provider P --seed N`). With no
judge command the tool only tabulates and writes the sample.
"""
from __future__ import annotations

import argparse
import collections
import glob
import json
import random
import subprocess
from pathlib import Path

AUDIT_PROMPT = (
    "You are auditing a memory gate. An assistant's self-review proposed saving the entry below "
    "to persistent memory, citing the evidence below. Decide, from the evidence alone, whether the "
    "entry is a durable, general rule that the evidence states — or that a correction, instruction "
    "or standing preference in the evidence implies. A restatement of one task's outcome, an "
    "inference the evidence does not support, or a rule already implied to be one-off is INVALID.\n\n"
    "Answer with exactly one line: VALID: <one sentence> or INVALID: <one sentence>.")


def judge_factory(cmd):
    if not cmd:
        return None
    argv = cmd.split()

    def judge(entry, evidence):
        req = json.dumps({"op": "chat", "temperature": 0, "tools": [],
                          "messages": [{"role": "system", "content": AUDIT_PROMPT},
                                       {"role": "user", "content": "Proposed entry:\n%s\n\nEvidence:\n%s" % (entry, evidence or "(none)")}]})
        p = subprocess.run(argv, input=req.encode("utf-8"), capture_output=True, timeout=180)
        if p.returncode != 0:
            return "ERROR: " + p.stderr.decode("utf-8", "replace")[:120]
        return ((json.loads(p.stdout.decode("utf-8")).get("message") or {}).get("content") or "").strip()

    return judge


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("run")
    ap.add_argument("--judge-cmd", default="")
    ap.add_argument("--sample", type=int, default=30)
    ap.add_argument("--seed", type=int, default=1)
    ap.add_argument("--json")
    ap.add_argument("--md")
    a = ap.parse_args()
    root = Path(a.run)
    decisions, unauditable = [], 0
    for f in sorted(glob.glob(str(root / "*" / "with_persistence" / "0*" / "artifacts" / "areev_ledger.json"))):
        l = json.loads(Path(f).read_text(encoding="utf-8"))
        fam = Path(f).parts[-5]
        ep = Path(f).parts[-3]
        for d in (l.get("governance") or {}).get("decisions", []):
            if "evidence" not in d:
                unauditable += 1
                continue
            decisions.append({"family": fam, "episode": ep, **d})
    judge = judge_factory(a.judge_cmd)
    for d in decisions:
        if d["kind"] == "advisory":
            d["audit"] = "SKIP"
            continue
        if judge is None:
            d["audit"] = "NOT RUN"
            continue
        d["audit"] = judge(d["text"], d.get("evidence", ""))
    tab = collections.Counter()
    for d in decisions:
        verdict = d["audit"].split(":", 1)[0].strip().upper()
        tab[("approved" if d["approved"] else "refused", verdict)] += 1
    rng = random.Random(a.seed)
    sample = rng.sample(decisions, min(a.sample, len(decisions))) if decisions else []
    out = {"run": str(root), "decisions": len(decisions), "unauditable": unauditable,
           "crosstab": {"%s/%s" % k: v for k, v in tab.items()}, "items": decisions, "sample": sample}
    if a.json:
        Path(a.json).write_text(json.dumps(out, indent=1), encoding="utf-8")
    lines = ["# Reviewer audit — %s" % root, "",
             "%d decisions with evidence, %d older decisions without (unauditable)." % (len(decisions), unauditable), "",
             "| reviewer | auditor | n |", "|---|---|---:|"]
    for (rv, au), n in sorted(tab.items()):
        lines.append("| %s | %s | %d |" % (rv, au, n))
    lines += ["", "## Hand-review sample (%d)" % len(sample), "",
              "Tick VALID or INVALID for each; the auditor's own verdict is shown for comparison.", ""]
    for i, d in enumerate(sample, 1):
        lines += ["### %d. %s / %s — reviewer: %s" % (i, d["family"], d["episode"][:40], "APPROVED" if d["approved"] else "refused"),
                  "", "**Proposed:** %s" % d["text"], "", "**Reviewer said:** %s" % d["because"], "",
                  "**Evidence shown:**", "", "```", (d.get("evidence") or "(none)")[:1200], "```", "",
                  "**Auditor:** %s" % d["audit"], "", "- [ ] VALID  - [ ] INVALID", ""]
    md = "\n".join(lines)
    if a.md:
        Path(a.md).write_text(md + "\n", encoding="utf-8")
    print("\n".join(lines[:12]))


if __name__ == "__main__":
    main()
