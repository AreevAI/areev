#!/usr/bin/env python3
"""The fixed-rubric reviewer for the persist track — one model call, no
access to the benchmark's expectations, graders or fixtures.

The rubric below is FIXED and was written before any run it judges
(2026-09-06). It is the human gate of the governed arm: a proposal the loop
drafted and grounded is read the way an operations lead would read it, and
approved or refused with a reason that the ledger records. It never sees a
task's expected output, so it cannot leak the answer into memory — it can
only decide whether a rule is the kind of thing worth keeping.
"""
import json
import re
import subprocess

REVIEW_RUBRIC = """You supervise an assistant that works for one team across many separate \
sessions. Each session starts with an empty context; the only thing that carries \
over is what is written into its persistent memory. You are shown ONE proposed \
memory change. Decide whether it should be saved.

APPROVE only when all of these hold:
- durable: it will still be true and useful in later, unrelated sessions — a \
preference, a standing instruction, a correction that replaces an earlier rule, a \
reusable procedure, a constraint about a tool or a person;
- general: it is about how work should be done, not about the outcome of one task;
- actionable: written so the assistant can follow it without asking;
- supported: the cited evidence actually says it (a person stated it, or the \
assistant was corrected into it) — not inferred from a single success.

REJECT when any of these hold:
- it records the result, id, or content of one specific task rather than a rule;
- it repeats something already in force (listed below), or a weaker restatement;
- it contradicts a NEWER instruction rather than replacing an OLDER one;
- it is speculative, hedged, or asks for no concrete change;
- it would store a secret, a credential, or another person's private detail.

Already in force:
{in_force}

Answer with exactly one line: APPROVE: <one-sentence reason> or REJECT: <one-sentence reason>."""


def normalize_rule(text):
    return re.sub(r"[^a-z0-9 ]+", " ", (text or "").lower()).split()


def too_similar(text, in_force, threshold=0.7):
    words = set(normalize_rule(text))
    if not words:
        return False
    for other in in_force:
        ow = set(normalize_rule(other))
        if not ow:
            continue
        j = len(words & ow) / float(len(words | ow))
        if j >= threshold:
            return True
    return False


def make_judge(review_cmd, usage_log=None):
    """A chat command speaking the bench's JSON-on-stdio contract
    (`scripts/openrouter_toolcall.py MODEL --provider P --seed N`)."""
    if not review_cmd:
        return None
    argv = review_cmd.split()

    def judge(system, user):
        req = json.dumps({"op": "chat", "temperature": 0, "tools": [],
                          "messages": [{"role": "system", "content": system},
                                       {"role": "user", "content": user}]})
        p = subprocess.run(argv, input=req.encode("utf-8"), capture_output=True, timeout=180)
        if p.returncode != 0:
            raise RuntimeError("reviewer failed: %s" % p.stderr.decode("utf-8", "replace")[:300])
        return (json.loads(p.stdout.decode("utf-8")).get("message") or {}).get("content") or ""

    return judge


def review(proposal_text, evidence_text, in_force, judge):
    """(approved: bool, reason: str). Dedup is decided here, before the model,
    so a restatement never costs a call and the reason is the same every time."""
    if not proposal_text or not proposal_text.strip():
        return False, "empty proposal"
    if too_similar(proposal_text, in_force):
        return False, "already in force (restates an approved entry)"
    if judge is None:
        # keyless floor: approve anything durable-looking, refuse one-task records
        low = proposal_text.lower()
        if re.search(r"\b(ticket|note|thread|order|invoice)[-_ ]?(id|#)?\s*[a-z]*-?\d{2,}", low):
            return False, "keyless floor: names one task's record"
        return True, "keyless floor: durable-looking rule"
    system = REVIEW_RUBRIC.format(in_force="\n".join("- " + r for r in in_force) or "- (nothing yet)")
    user = "Proposed memory change:\n%s\n\nEvidence the proposer cited:\n%s" % (
        proposal_text.strip(), (evidence_text or "(none)").strip()[:4000])
    verdict = judge(system, user).strip()
    head = verdict.split("\n", 1)[0].strip()
    if head.upper().startswith("APPROVE"):
        return True, head[len("APPROVE"):].lstrip(": ").strip() or "approved"
    if head.upper().startswith("REJECT"):
        return False, head[len("REJECT"):].lstrip(": ").strip() or "rejected"
    return False, "reviewer gave no verdict: %s" % head[:120]
