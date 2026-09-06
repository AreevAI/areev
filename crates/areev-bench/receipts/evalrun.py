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
from agent import build_messages, parse_reply, propose

HARNESS_NS = "age, build_messagesnt:harness"


def batch_replies(profile, at, rows, lessons, lessons_fn, batch_argv, journal):
    """Build every request, run the batch adapter once, return {custom_id: reply}."""
    import subprocess
    import tempfile
    where = os.path.dirname(journal.name) if journal is not None and hasattr(journal, "name") else tempfile.mkdtemp(prefix="areev-batch-")
    req_path = os.path.join(where, "batch.requests.jsonl")
    out_path = os.path.join(where, "batch.replies.jsonl")
    with open(req_path, "w", encoding="utf-8") as fh:
        for r in rows:
            section = lessons_fn(r) if lessons_fn else lessons
            fh.write(json.dumps({"custom_id": "seq-%d" % r["seq"], "op": "chat", "temperature": 0,
                                 "messages": build_messages(at, r["text"], section)}, ensure_ascii=False) + "\n")
    p = subprocess.run(list(batch_argv) + ["--in", req_path, "--out", out_path], capture_output=True)
    sys.stdout.write(p.stdout.decode(errors="replace"))
    if p.returncode != 0:
        print("  batch adapter failed: %s" % p.stderr.decode(errors="replace")[:300])
    out = {}
    if os.path.exists(out_path):
        for line in open(out_path, encoding="utf-8"):
            if line.strip():
                rep = json.loads(line)
                out[rep["custom_id"]] = rep
    return out


def evalset_hash(rows):
    """The held-out set's identity: sha256 over its receipt ids, in order."""
    h = hashlib.sha256()
    for r in rows:
        h.update(str(r["id"]).encode("utf-8"))
        h.update(b"\n")
    return h.hexdigest()[:16]


def run_arm(name, profile, lessons, rows, agent_argv, journal=None, verbose=True,
            at_seq=None, lessons_fn=None, batch_argv=None):
    """Read every held-out receipt under `lessons` (the prompt section as
    assembled from some memory state — or "" for the day-one agent).
    Returns (trials, usage).

    `at_seq` scores against the ledger as it stood at that document, which
    matters only for a profile whose conventions change: the same held-out
    set is worth different answers before and after a regime switch, and
    scoring a checkpoint against a convention the business had not announced
    yet would mark the agent wrong for obeying its instructions. The required
    FIELDS stay at the final bar so the denominator is constant and the
    checkpoints stay comparable; only how a value must be written moves."""
    at = ledger_profile.as_of(profile, at_seq) if at_seq is not None else profile
    # `lessons_fn(row) -> str` lets an arm assemble its prompt section PER
    # DOCUMENT. A memory system that retrieves by similarity (the mem0 arm)
    # shows the agent different memories for different receipts, where the
    # governed arm renders one rule set for all of them. Both are "what this
    # memory puts in the prompt"; only the second is a constant.
    n_rules = lessons.count("\n- ")
    if verbose:
        print("\n=== arm %s — %d rule(s) in the prompt%s" % (name, n_rules, "  [batch]" if batch_argv else ""))
    trials, usage_tot = [], {"prompt_tokens": 0, "completion_tokens": 0}
    errors = 0
    # `batch_argv` (scripts/batch_toolcall.py ...) submits every document's
    # request at once and collects the replies -- possible only because a
    # held-out read's prompt never depends on an earlier answer -- and the
    # scoring below is byte-for-byte the synchronous path's.
    replies = batch_replies(profile, at, rows, lessons, lessons_fn, batch_argv, journal) if batch_argv else None
    for r in rows:
        seq = r["seq"]
        req = ledger_profile.required_fields(profile, seq)
        try:
            if replies is not None:
                rep = replies.get("seq-%d" % seq)
                if rep is None or rep.get("error"):
                    raise RuntimeError("batch: %s" % ((rep or {}).get("error") or "no reply"))
                out, usage = parse_reply((rep.get("message") or {}).get("content") or ""), rep.get("usage") or {}
            else:
                section = lessons_fn(r) if lessons_fn else lessons
                out, usage = propose(agent_argv, at, r["text"], section)
        except Exception as e:
            # A provider having a bad minute is not a result, but losing the
            # whole arm to it is worse than scoring one document as a park:
            # a crash on document 6 of 60 threw away five measured documents
            # and the arm's summary with them (seed 3, run 2). The failure is
            # counted and printed, the document is scored as producing
            # nothing, and the arm still reports.
            errors += 1
            print("  seq %3d  MODEL CALL FAILED (%s) — scored as no output"
                  % (seq, type(e).__name__))
            out, usage = {"fields": {}, "park": True, "reason": "model call failed"}, {}
        usage_tot["prompt_tokens"] += int(usage.get("prompt_tokens") or 0)
        usage_tot["completion_tokens"] += int(usage.get("completion_tokens") or 0)
        for field in req:
            want = ledger_profile.refile(at, field, r["truth"].get(field, ""))
            if not want:
                continue
            got = (out["fields"] or {}).get(field, "")
            ex, sem = acct.compare(at, field, got, want)
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
        print("  arm %s: exact %d/%d (%.1f%%)  semantic %d/%d (%.1f%%)  tokens %d+%d%s"
              % (name, e, len(trials), 100.0 * e / max(len(trials), 1),
                 s, len(trials), 100.0 * s / max(len(trials), 1),
                 usage_tot["prompt_tokens"], usage_tot["completion_tokens"],
                 "  [%d model call(s) FAILED]" % errors if errors else ""))
    usage_tot["failed_calls"] = errors
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
