#!/usr/bin/env python3
"""Build the ad-buy ledger: real US broadcast invoices, their text, and the filed row.

VRDU ad-buy forms (Wang, Zhu et al., *VRDU: A Benchmark for Visually-rich
Document Understanding*, KDD 2023 — https://arxiv.org/abs/2211.15421), the
DeepForm collection: **641 real political-advertising invoices** filed with
the US FCC, each carrying the document's extracted text and human field
annotations. Fetched from
https://github.com/google-research-datasets/vrdu as one gzipped JSONL —
stdlib only, cached, offline after the first run. Nothing from it is
committed here; only counts travel.

**Why this corpus and not another receipt set.** It differs from SROIE on
every axis that could otherwise be a confound: a different country, a
different document type (a broadcast contract, not a till receipt), a
different length (median 5,200 characters against roughly 500), a different
day-one field (an amount, not a date), and — deliberately — a **different
filing convention**. This ledger wants dates as `YYYY-MM-DD` where the
receipts ledger wants `DD/MM/YYYY`, so a rule the loop once proposed and got
wrong on SROIE ("convert to ISO 8601") is the *correct* rule here. An agent
that learns the convention from this business's corrections gets it right; a
model applying a general prior about date formats gets exactly one of the
two corpora right.

What this emits is not the benchmark's annotation verbatim: it is the row a
bookkeeper would FILE from it, under one stated convention — amounts as
plain two-decimal numbers with no currency sign or thousands separator,
flight dates as `YYYY-MM-DD`, advertiser and contract number exactly as
printed. The conversion is deterministic and here in full. A record whose
amount or date cannot be normalized unambiguously is dropped and counted,
never guessed at.

    build_vrdu.py --out data/vrdu.jsonl [--cache data/cache/vrdu]
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
       "main/ad-buy-form/main/dataset.jsonl.gz")

# The flat, one-per-document fields. VRDU's line-item fields (channel,
# program_desc, …) are deliberately unused: a ledger row is one row, and
# repeating groups would make "exact match on a field" ambiguous.
WANTED = {
    "gross_amount": "Gross Amount",
    "advertiser": "Advertiser",
    "contract_num": "Contract Number",
    "flight_from": "Flight From",
    "flight_to": "Flight To",
}
REQUIRED = ["Gross Amount", "Advertiser", "Contract Number"]


def fetch(url, dest):
    if os.path.exists(dest):
        return dest
    os.makedirs(os.path.dirname(dest), exist_ok=True)
    print("fetching %s (~32 MB, once)…" % url, file=sys.stderr)
    try:
        with urllib.request.urlopen(url, timeout=600) as r, open(dest, "wb") as fh:
            fh.write(r.read())
    except (urllib.error.URLError, urllib.error.HTTPError) as e:
        raise SystemExit("fetch %s: %s" % (url, e))
    return dest


def norm_amount(s):
    """A price as the ledger files it: plain, two decimals, no sign or
    separators. `$5,625.00` becomes `5625.00`."""
    s = re.sub(r"[^0-9.\-]", "", (s or "").replace(",", ""))
    if not s or s.count(".") > 1:
        return None
    try:
        return "%.2f" % float(s)
    except ValueError:
        return None


def norm_date(s):
    """A flight date as the ledger files it: ISO `YYYY-MM-DD`. These invoices
    print US month-first, usually two-digit years; a form that is not
    unambiguously month-first is dropped rather than reinterpreted."""
    s = (s or "").strip()
    for fmt in ("%m/%d/%y", "%m/%d/%Y", "%m-%d-%y", "%m-%d-%Y", "%b %d, %Y", "%B %d, %Y"):
        try:
            d = datetime.strptime(s, fmt)
        except ValueError:
            continue
        # Two-digit years land in this century: the corpus is 2019–2020 FCC
        # filings, and strptime's own 1969 pivot would file them as last.
        if d.year < 1970:
            d = d.replace(year=d.year + 100)
        return d.strftime("%Y-%m-%d")
    return None


def first_values(record):
    """{ledger field: the annotation's first occurrence}, flat fields only."""
    out = {}
    for pair in record.get("annotations", []):
        if len(pair) != 2:
            continue
        field, occs = pair
        # A list key is a line-item group (channel, program_desc, …) — skipped
        # by design, see WANTED.
        if not isinstance(field, str) or field not in WANTED or not occs:
            continue
        value = (occs[0][0] or "").strip()
        if value:
            out[WANTED[field]] = value
    return out


def main():
    ap = argparse.ArgumentParser()
    here = os.path.dirname(os.path.abspath(__file__))
    ap.add_argument("--out", default=os.path.join(here, "data", "vrdu.jsonl"))
    ap.add_argument("--cache", default=os.path.join(here, "data", "cache", "vrdu"))
    ap.add_argument("--limit", type=int, default=0, help="0 = all")
    args = ap.parse_args()

    src = fetch(SRC, os.path.join(args.cache, "dataset.jsonl.gz"))
    rows, dropped = [], {"required": 0, "amount": 0, "date": 0, "text": 0}
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
            amount = norm_amount(raw["Gross Amount"])
            if amount is None:
                dropped["amount"] += 1
                continue
            truth = {
                "Gross Amount": amount,
                "Advertiser": " ".join(raw["Advertiser"].split()),
                "Contract Number": " ".join(raw["Contract Number"].split()),
            }
            bad_date = False
            for f in ("Flight From", "Flight To"):
                if f in raw:
                    d = norm_date(raw[f])
                    if d is None:
                        bad_date = True
                        break
                    truth[f] = d
            if bad_date:
                dropped["date"] += 1
                continue
            rows.append({
                "id": (r.get("filename") or "").replace(".pdf", "")[:12],
                "text": text,
                "text_len": len(text),
                "truth": truth,
            })
            if args.limit and len(rows) >= args.limit:
                break

    os.makedirs(os.path.dirname(args.out), exist_ok=True)
    with open(args.out, "w", encoding="utf-8") as fh:
        for r in rows:
            fh.write(json.dumps(r, ensure_ascii=False) + "\n")
    have = {}
    for r in rows:
        for f in r["truth"]:
            have[f] = have.get(f, 0) + 1
    print("wrote %d ad-buy invoices to %s (dropped: %s)"
          % (len(rows), args.out, ", ".join("%s %d" % kv for kv in dropped.items())))
    print("field coverage: %s" % ", ".join("%s %d" % (k, v) for k, v in sorted(have.items())))


if __name__ == "__main__":
    sys.exit(main())
