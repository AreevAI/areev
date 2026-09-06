#!/usr/bin/env python3
"""The capture agent: read a receipt, propose one ledger row.

Deliberately small. The agent's whole behaviour is (day-one instruction +
whatever LESSONS memory currently holds) applied to the document text — so
the only lever on its behaviour is what the accountant has taught it, which
is what makes the paired evaluation attributable.

Model access reuses the bench's OpenRouter adapter contract: JSON on stdin,
JSON on stdout, one process per call, no SDK.
"""
import json
import subprocess

# What the agent knows on day one. Everything else has to be taught.
BASE_INSTRUCTION = """You capture business expenses from {noun}s into a \
ledger row.

Your instruction: capture the {day_one}.

Read the {noun} text and return JSON only:
{{"fields": {{"{day_one}": "..."}}, "park": false, "reason": ""}}

Capture exactly the fields you have been instructed to capture — the one
shown above, plus any the LESSONS below add. Name each field with the exact
spelling and capitalisation you were given it in; those names are ledger
column headings, so "invoice_date" is not the same column as "Invoice Date".

If you cannot determine a value you were asked for, or the document does not \
give you what you need, set "park": true and put a short question for the \
accountant in "reason" — do NOT guess. Parking is always better than \
inventing a value."""


def base_instruction(profile):
    return BASE_INSTRUCTION.format(noun=profile["document_noun"], day_one=profile["day_one"])


def call_model(argv, messages, timeout=600):
    req = json.dumps({"op": "chat", "messages": messages, "tools": [], "temperature": 0})
    p = subprocess.run(argv, input=req.encode(), capture_output=True, timeout=timeout)
    if p.returncode != 0:
        raise RuntimeError("model call failed: %s" % p.stderr.decode()[:300])
    resp = json.loads(p.stdout.decode())
    content = (resp.get("message") or {}).get("content") or ""
    return content, resp.get("usage") or {}


def parse_reply(text):
    """The model's JSON, or a park if it produced anything else.

    A garbled reply is treated as a park rather than an extraction failure:
    the honest reading is that the agent did not produce a usable answer, and
    scoring it as a wrong value would credit noise as a decision.
    """
    t = (text or "").strip()
    if t.startswith("```"):
        t = t.split("```")[1] if "```" in t[3:] else t
        t = t[4:] if t.lower().startswith("json") else t
    i, j = t.find("{"), t.rfind("}")
    if i < 0 or j < 0:
        return {"fields": {}, "park": True, "reason": "unparseable reply"}
    try:
        d = json.loads(t[i:j + 1])
    except json.JSONDecodeError:
        return {"fields": {}, "park": True, "reason": "unparseable reply"}
    # Values arrive as whatever JSON type the model chose — an Amount comes
    # back as the number 9.1 as readily as the string "9.10". Coerce at this
    # boundary so nothing downstream has to care.
    fields = {}
    for k, v in (d.get("fields") or {}).items():
        if v is None or isinstance(v, (dict, list)):
            continue
        if isinstance(v, bool):
            fields[str(k)] = str(v)
        elif isinstance(v, float):
            fields[str(k)] = repr(v) if v != int(v) else str(int(v))
        else:
            fields[str(k)] = str(v)
    return {
        "fields": fields,
        "park": bool(d.get("park")),
        "reason": str(d.get("reason") or "")[:300],
    }


def build_messages(profile, document_text, lessons_md):
    """The agent's prompt for one document: the day-one instruction, the
    memory's section (if any), and the document. One builder for the
    synchronous call and the batch path, so the two can never differ."""
    system = base_instruction(profile)
    if lessons_md.strip():
        system += "\n\n" + lessons_md
    body = document_text.strip()
    if not body:
        body = "(no extractable text — this document is a scan or an image)"
    return [
        {"role": "system", "content": system},
        {"role": "user", "content": profile["document_noun"].upper() + ":\n\n" + body[:12000]},
    ]


def propose(argv, profile, document_text, lessons_md):
    """One capture attempt. `lessons_md` is assembled live from memory.

    There is deliberately no way to pass extra instructions: memory is the
    only channel by which the agent can learn what the accountant wants, so
    the harness structurally cannot hand over the thing it is measuring.
    Returns (proposal, usage).
    """
    content, usage = call_model(argv, build_messages(profile, document_text, lessons_md))
    return parse_reply(content), usage
