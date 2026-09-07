#!/usr/bin/env python3
"""Is this experiment measurable at all, before paying for it?

Two arms over the same held-out tasks, no lessons in either:

  FULL      the whole policy and the whole tool descriptions
  REDACTED  the withheld clauses removed from both — i.e. arm A0 of a run

It answers the precondition every later number depends on, and it answers
it for the price of two passes instead of six:

- **FULL ≈ 0** — the agent cannot do this domain even knowing everything.
  Nothing downstream can be attributed to the loop, and no learning claim
  follows. Report that and stop.
- **FULL ≈ REDACTED** — the withheld clauses cost nothing here, so there is
  nothing for the loop to put back. The manipulation is inert; pick
  different clauses or a different domain.
- **FULL > REDACTED** — the clauses matter and the gap is the headroom a
  learning run has to close. Only then is the full run worth its hours.

Run before the run, not after it, so its answer cannot be read as an
excuse for a result already in hand.
"""
import argparse
import json
import os
import sys
from math import comb

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import run as runner


def mcnemar_exact(b, c):
    n = b + c
    if n == 0:
        return 1.0
    k = min(b, c)
    return min(1.0, 2 * sum(comb(n, i) for i in range(k + 1)) / (2 ** n))


def main():
    ap = argparse.ArgumentParser()
    runner.add_common_args(ap)
    args = ap.parse_args()

    policy, agent_tools, withheld, _exp, held, full_policy, full_tools = runner.setup(args)
    os.makedirs(args.workdir, exist_ok=True)
    print("withheld: %s | held-out tasks: %d (%s)"
          % (", ".join(withheld) or "(nothing)", len(held), ", ".join(t.id for t in held)))

    records, by_arm = [], {}
    for name, pol, tools in (("FULL", full_policy, full_tools),
                             ("REDACTED", policy, agent_tools)):
        recs = runner.run_arm(name, held, pol, tools, "", args)
        records += recs
        by_arm[name] = {r["task_id"]: bool(r.get("reward", 0) >= 1.0) for r in recs}

    keys = sorted(set(by_arm["FULL"]) & set(by_arm["REDACTED"]))
    b = sum(1 for k in keys if by_arm["FULL"][k] and not by_arm["REDACTED"][k])
    c = sum(1 for k in keys if by_arm["REDACTED"][k] and not by_arm["FULL"][k])
    solved = {a: sum(v.values()) for a, v in by_arm.items()}
    # Terminations matter as much as rewards here: tau2 scores a run that hit
    # max_steps as zero regardless of database state, so "never finished" and
    # "finished wrong" must not be read as one number.
    ends = {}
    for r in records:
        ends.setdefault(r["arm"], {}).setdefault(r.get("terminated", "?"), 0)
        ends[r["arm"]][r.get("terminated", "?")] += 1
    out = {"withheld": withheld, "held_out": len(held), "solved": solved,
           "full_vs_redacted": {"n": len(keys), "full_only": b, "redacted_only": c,
                                "p": round(mcnemar_exact(b, c), 6)},
           "terminations": ends,
           "tool_errors": {a: sum(r.get("tool_errors", 0) for r in records if r["arm"] == a)
                           for a in by_arm}}
    json.dump(records, open(os.path.join(args.workdir, "records.json"), "w"), indent=1)
    json.dump(out, open(os.path.join(args.workdir, "ceiling.summary.json"), "w"), indent=1)

    print("\nsolved: FULL %d/%d, REDACTED %d/%d"
          % (solved["FULL"], len(held), solved["REDACTED"], len(held)))
    print("paired: FULL-only %d, REDACTED-only %d, p=%.4f"
          % (b, c, out["full_vs_redacted"]["p"]))
    print("terminations: %s" % json.dumps(ends))
    if solved["FULL"] == 0:
        print("\nVERDICT: the agent solves nothing even with the whole policy. "
              "This domain is out of reach for this model; no learning claim can "
              "be made here, and the full run is not worth its hours.")
    elif b <= c:
        print("\nVERDICT: the withheld clauses cost nothing measurable. There is "
              "nothing for the loop to put back; choose different clauses.")
    else:
        print("\nVERDICT: %d task(s) of headroom. The full run has something to close."
              % (b - c))


if __name__ == "__main__":
    sys.exit(main())
