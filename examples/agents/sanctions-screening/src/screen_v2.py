#!/usr/bin/env python3
"""The screening rule, REVISED (v2) — seeded into the memory as a CAS blob, not shipped as a script.

This file is the WORKSHOP copy. `agent.py seed` reads these exact bytes,
stores them with `put_blob`, and binds the resulting `cas://sha256:...`
address into the `screen` Tool definition's `executor_uri`. The host then
authorizes precisely these bytes with `--allow-executor` (the pin is
computed from this file, so the checkout and the memory agree or the run
refuses with RUN-E018).

The contract is identical to a host tool's: run state as JSON on stdin,
result JSON on stdout, `AREEV_TOOL_NAME` in the environment. That identity
is the point -- moving logic from `--tool-cmd` into a grain is a packaging
change, not a rewrite.

v1, deliberately: this rule refuses a counterparty name it cannot read
rather than screening a mangled string against the list and reporting
"no match". A false clear is the expensive failure in screening; a loud
refusal is the cheap one. The refusals cluster in the run journal, which
is what `areev loop run` later finds.
"""
import json
import sys
import unicodedata

RULE_VERSION = "v2"


# Cyrillic characters that are visually identical to Latin ones. A
# counterparty name salted with these screens "clean" against a Latin
# watchlist while looking correct to a human reviewer -- which is the
# oldest trick in list evasion, and why v1 refused rather than guessed.
HOMOGLYPHS = {
    "\u0430": "a", "\u0435": "e", "\u043e": "o", "\u0440": "p",
    "\u0441": "c", "\u0443": "y", "\u0445": "x", "\u0456": "i",
    "\u0410": "A", "\u0412": "B", "\u0415": "E", "\u041a": "K",
    "\u041c": "M", "\u041d": "H", "\u041e": "O", "\u0420": "P",
    "\u0421": "C", "\u0422": "T", "\u0423": "Y", "\u0425": "X",
}


def repair(name):
    """Undo UTF-8-read-as-Latin-1, then fold Cyrillic look-alikes to Latin.

    Returns None when the name still is not readable afterwards -- v1's
    refusal stands in that case. The rule got WIDER, not laxer.
    """
    try:
        name = name.encode("latin-1").decode("utf-8")
    except (UnicodeEncodeError, UnicodeDecodeError):
        pass
    name = "".join(HOMOGLYPHS.get(ch, ch) for ch in name)
    name = unicodedata.normalize("NFKC", name)
    return name if all(ord(c) <= 0x7E for c in name) else None


def normalize(name):
    """Casefold, strip punctuation and corporate suffixes, collapse space."""
    n = unicodedata.normalize("NFKC", name).casefold()
    out = []
    for ch in n:
        out.append(ch if (ch.isalnum() or ch.isspace()) else " ")
    words = "".join(out).split()
    drop = {"inc", "llc", "ltd", "limited", "ooo", "gmbh", "sa", "nv", "bv",
            "plc", "co", "corp", "holdings", "group"}
    kept = [w for w in words if w not in drop]
    return " ".join(kept or words)


def tokens(name):
    return set(normalize(name).split())


def main():
    state = json.load(sys.stdin)
    item = state.get("item", state)
    name = item.get("counterparty", "")

    # v2 repairs the upstream decoding fault and folds homoglyphs instead
    # of refusing outright. A name that is STILL unreadable after repair is
    # still refused -- v1's guarantee is preserved, its blind spot is not.
    if any(ord(c) > 0x7E for c in name):
        repaired = repair(name)
        if repaired is None:
            sys.stderr.write(
                "screen %s: counterparty %r is not readable after repair; "
                "refusing to screen a mangled name\n" % (RULE_VERSION, name))
            return 1
        name = repaired

    # The list is EXTERNAL data (OFAC/EU/UN publish it); the connector
    # attaches the current one to the item. The dispositions below are
    # MEMORY -- what this desk learned from its own officers.
    wl = state.get("watchlist") or item.get("watchlist") or {}
    watchlist = wl.get("entries") or []
    cleared = {}
    ctx = state.get("context") or {}

    def walk(node):
        if isinstance(node, dict):
            if node.get("relation") == "mg:screened_clear" and node.get("subject"):
                cleared[normalize(node["subject"])] = node.get("object")
            for v in node.values():
                walk(v)
        elif isinstance(node, list):
            for v in node:
                walk(v)
    walk(ctx)

    subject = tokens(name)
    best, best_score, best_id = None, 0.0, None
    for entry in watchlist:
        listed = tokens(entry.get("name", ""))
        if not listed or not subject:
            continue
        score = len(subject & listed) / float(len(subject | listed))
        if score > best_score:
            best, best_score, best_id = entry.get("name"), score, entry.get("id")

    # A disposition a compliance officer signed off previously clears this
    # exact counterparty without a second review.
    disposition = cleared.get(normalize(name))

    json.dump({
        "rule_version": RULE_VERSION,
        "counterparty_normalized": normalize(name),
        "match_name": best,
        "match_id": best_id,
        "match_score": round(best_score, 3),
        "prior_disposition": disposition,
        "list_version": wl.get("list_version"),
    }, sys.stdout, sort_keys=True)
    return 0


if __name__ == "__main__":
    sys.exit(main())
