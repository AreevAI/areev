#!/usr/bin/env python3
"""One pass over the held-out receipts at a fixed memory state, and the
journal entry that makes it evidence the loop can read.

A held-out pass is an EVALSET RUN in the loop's sense (`docs/loop.md`,
"Evalset-backed outcomes"): journaled as an `mg:eval_run` Fact under
`agent:harness`, it is what `outcome_review` re-measures an applied lesson
against. The evalset's identity is the held-out set itself — its hash is
over the receipt ids, so two runs of the same split name the same evalset
and a different split cannot masquerade as it.
"""
import hashlib
import json
import os
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import accountant as acct
import ledger_profile
import memory as mem
from agent import propose

HARNESS_NS = "agent:harness"


def evalset_hash(rows):
    """The held-out set's identity: sha256 over its receipt ids, in order."""
    h = hashlib.sha256()
    for r in rows:
        h.update(str(r["id"]).encode("utf-8"))
        h.update(b"\n")
    return h.hexdigest()[:16]


def run_arm(name, profile, lessons, rows, agent_argv, journal=None, verbose=True):
    """Read every held-out receipt under `lessons` (the prompt section as
    assembled from some memory state — or "" for the day-one agent).
    Returns (trials, usage)."""
    n_rules = lessons.count("\n- ")
    if verbose:
        print("\n=== arm %s — %d rule(s) in the prompt" % (name, n_rules))
    trials, usage_tot = [], {"prompt_tokens": 0, "completion_tokens": 0}
    for r in rows:
        seq = r["seq"]
        req = ledger_profile.required_fields(profile, seq)
        out, usage = propose(agent_argv, profile, r["text"], lessons)
        usage_tot["prompt_tokens"] += int(usage.get("prompt_tokens") or 0)
        usage_tot["completion_tokens"] += int(usage.get("completion_tokens") or 0)
        for field in req:
            want = r["truth"].get(field, "")
            if not want:
                continue
            got = (out["fields"] or {}).get(field, "")
            ex, sem = acct.compare(profile, field, got, want)
            trials.append({"arm": name, "seq": seq, "id": r["id"], "field": field,
                           "exact": bool(ex), "semantic": bool(sem),
                           "got": got, "want": want})
        if journal is not None:
            journal.write(json.dumps({
                "arm": name, "seq": seq, "id": r["id"], "rules_in_prompt": n_rules,
                "proposed": out["fields"], "parked": out["park"], "usage": usage,
            }, ensure_ascii=False) + "\n")
            journal.flush()
        if verbose:
            ex = sum(t["exact"] for t in trials if t["seq"] == seq)
            sm = sum(t["semantic"] for t in trials if t["seq"] == seq)
            tot = sum(1 for t in trials if t["seq"] == seq)
            print("  seq %3d  exact %d/%-2d semantic %d/%-2d %s"
                  % (seq, ex, tot, sm, tot, "PARK" if out["park"] else ""))
    if verbose:
        e = sum(t["exact"] for t in trials)
        s = sum(t["semantic"] for t in trials)
        print("  arm %s: exact %d/%d (%.1f%%)  semantic %d/%d (%.1f%%)  tokens %d+%d"
              % (name, e, len(trials), 100.0 * e / max(len(trials), 1),
                 s, len(trials), 100.0 * s / max(len(trials), 1),
                 usage_tot["prompt_tokens"], usage_tot["completion_tokens"]))
    return trials, usage_tot


def summary_of(trials):
    exact = sum(t["exact"] for t in trials)
    semantic = sum(t["semantic"] for t in trials)
    return {"passed": exact, "failed": len(trials) - exact, "total": len(trials),
            "exact": exact, "semantic": semantic}


def journal_eval_run(db_path, evalset, run_id, trials, note=None, at_ms=None):
    """Record a held-out pass as an evalset run the loop can measure against.

    `passed` is the exact-match count, so the promoted `passed`/`failed`
    fields and the named `exact` field agree; `semantic` rides beside them.
    Written under the reviewer, in the harness namespace, into the PRIMARY
    memory (never an arm's copy) — the memory whose lessons it is evidence
    about. `at_ms` pins the grain's timestamp onto a simulated timeline
    (regress.py): the engine reads runs journaled AFTER an apply, so a pass
    taken "a day later" on the engine's clock must be stamped a day later.
    """
    summary = {"run_id": run_id, **summary_of(trials)}
    if note:
        summary["note"] = note

    def write(db):
        fields = {
            "subject": "evalset:%s" % evalset,
            "relation": "mg:eval_run",
            "object": json.dumps(summary),
            "run_id": run_id,
        }
        if at_ms is not None:
            fields["created_at"] = int(at_ms)
        db.add("fact", json.dumps(fields), ns=HARNESS_NS)
        return summary

    return mem.with_memory(db_path, mem.REVIEWER, write)
