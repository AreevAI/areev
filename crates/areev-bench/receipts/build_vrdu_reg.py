#!/usr/bin/env python3
"""Build the registration ledger: real FARA registration forms, their text,
and the row a compliance clerk would file.

VRDU registration forms (Wang, Zhu et al., KDD 2023,
https://arxiv.org/abs/2211.15421): **1,915 real Foreign Agents Registration
Act filings** from the US Department of Justice, each with extracted text and
human field annotations. Fetched from google-research-datasets/vrdu as one
gzipped JSONL, stdlib only, cached. Nothing from it is committed; only
counts travel.

**Why this corpus.** It is the first in this programme with a real timeline:
every form carries the date it was filed, 1975 to 2023, so "over a period of
time" is chronology and not a shuffle. And it has 640 distinct registrants,
so a held-out set can be drawn from ORGANISATIONS the agent never saw — the
split that designs the memorisation question out instead of checking it
afterwards.

The filed row is the clerk's, not the annotation's: the file date as ISO
`YYYY-MM-DD` (the forms print it a dozen ways, most often "July 16, 2008",
and older ones "23rd day of March 1962"), the registration number as digits,
the registrant and signer exactly as printed. A date the parser cannot read
with certainty is dropped and counted, never guessed — a third of the
older scans are OCR noise like "Subscribed and sworn to before me at. CW
you day of 1925", and those forms are not usable evidence of anything.

    build_vrdu_reg.py --out data/vrdu_reg.jsonl [--cache data/cache/vrdu]
"""
import argparse
import gzip
import json
import os
import re
import sys
import urllib.error
import urllib.request
from datetime import datetime

SRC = ("https://raw.githubusercontent.com/google-research-datasets/vrdu/"
       "main/registration-form/main/dataset.jsonl.gz")
WANTED = {"registrant_name": "Registrant Name", "registration_num": "Registration Number",
          "file_date": "File Date", "signer_name": "Signer Name"}
REQUIRED = ["Registrant Name", "Registration Number", "File Date"]
MONTHS = "january february march april may june july august september october november december".split()


def fetch(url, dest):
    if os.path.exists(dest):
        return dest
    os.makedirs(os.path.dirname(dest), exist_ok=True)
    print("fetching %s (~52 MB, once)…" % url, file=sys.stderr)
    try:
        with urllib.request.urlopen(url, timeout=600) as r, open(dest, "wb") as fh:
            fh.write(r.read())
    except (urllib.error.URLError, urllib.error.HTTPError) as e:
        raise SystemExit("fetch %s: %s" % (url, e))
    return dest


def norm_date(s):
    """ISO for the forms' printed dates, or None. Handles the modern
    "July 16, 2008" family, US numeric dates, and the older notarial
    "23rd day of March 1962". Two-digit years pivot at 1970; anything
    outside 1938–2025 (FARA's own span) is treated as an OCR misread."""
    s = re.sub(r"\s+", " ", (s or "")).strip(" .,")
    m = re.search(r"(\d{1,2})(?:st|nd|rd|th)?\s+day\s+of\s+([A-Za-z]+)\s*,?\s*-?\s*(\d{4})", s, re.I)
    if m and m.group(2).lower() in MONTHS:
        try:
            d = datetime(int(m.group(3)), MONTHS.index(m.group(2).lower()) + 1, int(m.group(1)))
            return d.strftime("%Y-%m-%d") if 1938 <= d.year <= 2025 else None
        except ValueError:
            return None
    for fmt in ("%B %d, %Y", "%b %d, %Y", "%m/%d/%Y", "%m/%d/%y", "%B %d %Y", "%d %B %Y",
                "%Y-%m-%d", "%m-%d-%Y", "%b %d %Y", "%d %b %Y"):
        try:
            d = datetime.strptime(s, fmt)
        except ValueError:
            continue
        if d.year < 100:
            d = d.replace(year=d.year + (1900 if d.year >= 70 else 2000))
        return d.strftime("%Y-%m-%d") if 1938 <= d.year <= 2025 else None
    return None


def first_values(record):
    out = {}
    for pair in record.get("annotations", []):
        if not (isinstance(pair, list) and len(pair) == 2 and isinstance(pair[0], str)):
            continue
        field, occs = pair
        if field not in WANTED or not occs:
            continue
        value = re.sub(r"\s+", " ", (occs[0][0] or "")).strip()
        if value:
            out[WANTED[field]] = value
    return out


def main():
    ap = argparse.ArgumentParser()
    here = os.path.dirname(os.path.abspath(__file__))
    ap.add_argument("--out", default=os.path.join(here, "data", "vrdu_reg.jsonl"))
    ap.add_argument("--cache", default=os.path.join(here, "data", "cache", "vrdu"))
    args = ap.parse_args()

    src = fetch(SRC, os.path.join(args.cache, "reg.jsonl.gz"))
    rows, dropped = [], {"required": 0, "date": 0, "regnum": 0, "text": 0}
    with gzip.open(src, "rt", encoding="utf-8") as fh:
        for line in fh:
            r = json.loads(line)
            text = (r.get("ocr") or {}).get("text") or ""
            if len(text.strip()) < 200:
                dropped["text"] += 1
                continue
            raw = first_values(r)
            if any(f not in raw for f in REQUIRED):
                dropped["required"] += 1
                continue
            filed = norm_date(raw["File Date"])
            if filed is None:
                dropped["date"] += 1
                continue
            regnum = re.sub(r"\D", "", raw["Registration Number"])
            if not regnum:
                dropped["regnum"] += 1
                continue
            truth = {"Registrant Name": raw["Registrant Name"], "Registration Number": regnum,
                     "File Date": filed}
            if raw.get("Signer Name"):
                truth["Signer Name"] = raw["Signer Name"]
            rows.append({"id": (r.get("filename") or "").replace(".pdf", "")[:40], "text": text,
                         "text_len": len(text), "truth": truth,
                         "entity": re.sub(r"\s+", " ", raw["Registrant Name"]).strip().lower(),
                         "filed_at": filed})
    rows.sort(key=lambda x: x["filed_at"])
    os.makedirs(os.path.dirname(args.out), exist_ok=True)
    with open(args.out, "w", encoding="utf-8") as fh:
        for x in rows:
            fh.write(json.dumps(x, ensure_ascii=False) + "\n")
    ents = {x["entity"] for x in rows}
    have = {}
    for x in rows:
        for f in x["truth"]:
            have[f] = have.get(f, 0) + 1
    print("wrote %d registration forms to %s (dropped: %s)" % (len(rows), args.out, ", ".join("%s %d" % kv for kv in dropped.items())))
    print("distinct registrants: %d | filed %s .. %s" % (len(ents), rows[0]["filed_at"], rows[-1]["filed_at"]))
    print("field coverage: %s" % ", ".join("%s %d" % (k, v) for k, v in sorted(have.items())))


if __name__ == "__main__":
    sys.exit(main())
