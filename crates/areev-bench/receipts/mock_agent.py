#!/usr/bin/env python3
"""A keyless stand-in for the model leg, for testing the harness plumbing.

It reads a date out of the receipt with a regex and writes it in ISO — UNLESS
the system prompt carries a rule naming DD/MM/YYYY, in which case it obeys;
and a rule naming MM/DD/YYYY (the harmful fixture regress.py admits) wins
over both, so a wrong rule visibly hurts. Those conditionals are what make
the dry run meaningful: if the harness is wired correctly the exact score
steps up exactly when the lesson lands in the prompt, falls when the harmful
one does, and recovers when it is reverted; wired wrongly, nothing moves. It
proves the plumbing, never a learning claim — the real agent has to actually
read the receipt.
"""
import json
import re
import sys

req = json.load(sys.stdin)
msgs = req.get("messages", [])
system = next((m["content"] for m in msgs if m["role"] == "system"), "")
user = next((m["content"] for m in msgs if m["role"] == "user"), "")

MONTHS = "jan feb mar apr may jun jul aug sep oct nov dec".split()
PATTERNS = [
    r"(\d{4})-(\d{2})-(\d{2})",
    r"(\d{1,2})[/-](\d{1,2})[/-](\d{2,4})",
    r"(\d{1,2})[/ -](" + "|".join(MONTHS) + r")[a-z]*[/ -,]*(\d{2,4})",
]

date = None
low = user.lower()
for pat in PATTERNS:
    m = re.search(pat, low)
    if not m:
        continue
    g = m.groups()
    if pat.startswith(r"(\d{4})"):
        y, mo, d = int(g[0]), int(g[1]), int(g[2])
    elif g[1].isalpha():
        d, mo, y = int(g[0]), MONTHS.index(g[1]) + 1, int(g[2])
    else:
        d, mo, y = int(g[0]), int(g[1]), int(g[2])
    if y < 100:
        y += 2000
    date = (y, mo, d)
    break

# The VRDU profile's day-one field is an amount, not a date. The mock reads
# whichever the prompt asks for, so one keyless gate covers both profiles.
if "Gross Amount" in system:
    # The largest dollar figure on an ad-buy invoice is the gross; a plumbing
    # mock does not need to be right often, but it does need to be right the
    # same way with and without the rule, or the gate measures the regex.
    cands = re.findall(r"\$\s?([0-9][0-9,]*\.[0-9]{2})", user)
    best = max(cands, key=lambda c: float(c.replace(",", ""))) if cands else None
    if best is None:
        out = {"fields": {}, "park": True, "reason": "What is the gross amount?"}
    else:
        # Obeys a learned rule naming the plain-number convention; otherwise
        # writes it the way the invoice prints it, with the dollar sign.
        plain = best.replace(",", "")
        out = {"fields": {"Gross Amount": plain if "two decimals" in system else "$" + best},
               "park": False, "reason": ""}
    print(json.dumps({"message": {"content": json.dumps(out)},
                      "usage": {"prompt_tokens": 0, "completion_tokens": 0}}))
    raise SystemExit

if date is None:
    out = {"fields": {}, "park": True, "reason": "What is the invoice date?"}
else:
    y, mo, d = date
    if "MM/DD/YYYY" in system:
        value = "%02d/%02d/%04d" % (mo, d, y)
    elif "DD/MM/YYYY" in system:
        value = "%02d/%02d/%04d" % (d, mo, y)
    else:
        value = "%04d-%02d-%02d" % (y, mo, d)
    out = {"fields": {"Invoice Date": value}, "park": False, "reason": ""}

print(json.dumps({"message": {"content": json.dumps(out)},
                  "usage": {"prompt_tokens": 0, "completion_tokens": 0}}))
