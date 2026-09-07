#!/usr/bin/env python3
"""Build the SROIE ledger: real receipts, OCR text, and the filed row.

ICDAR 2019 SROIE (Huang et al., arXiv:2103.10213) — 626 scanned Malaysian
receipts with line-level OCR transcriptions and four key fields (company,
date, address, total). Fetched from the public mirror
https://github.com/zzzDavid/ICDAR-2019-SROIE (raw files, stdlib only, cached
locally so a re-run is offline). The dataset's terms are those of the ICDAR
Robust Reading Competition portal; nothing from it is committed here.

What this emits is NOT the competition's ground truth verbatim: it is the
row a bookkeeper would FILE from that truth, under one stated convention —
dates as DD/MM/YYYY, totals as plain two-decimal numbers, company and
address exactly as printed. The conversion is deterministic and here in
full, so a reader can see exactly what "exact match" is measured against.
A receipt whose date or total cannot be normalized unambiguously is dropped
and counted, never guessed at.

    build_sroie.py --out data/sroie.jsonl [--cache data/cache/sroie]

Record shape (shared by every corpus builder):
    {"id": "000", "text": "<OCR lines>", "text_len": N,
     "truth": {"Invoice Date": "25/12/2018", "Vendor Name": "...",
               "Amount": "9.00", "Vendor Address": "..."}}
"""
import argparse
import json
import os
import re
import sys
import time
import urllib.error
import urllib.request
from datetime import datetime

RAW = "https://raw.githubusercontent.com/zzzDavid/ICDAR-2019-SROIE/master/data"
# One request for the whole mirror beats 1,252 raw-file fetches (measured:
# ~7 s each against GitHub's raw endpoint). Only box/ and key/ are kept.
TARBALL = "https://codeload.github.com/zzzDavid/ICDAR-2019-SROIE/tar.gz/master"
N = 626

MONTHS = {m: i + 1 for i, m in enumerate(
    "jan feb mar apr may jun jul aug sep oct nov dec".split())}


def fetch(url, dest, retries=4):
    if os.path.exists(dest):
        return open(dest, "rb").read()
    delay = 2
    for attempt in range(retries):
        try:
            with urllib.request.urlopen(url, timeout=60) as r:
                data = r.read()
            os.makedirs(os.path.dirname(dest), exist_ok=True)
            with open(dest, "wb") as fh:
                fh.write(data)
            return data
        except (urllib.error.URLError, urllib.error.HTTPError) as e:
            if attempt == retries - 1:
                raise SystemExit("fetch %s: %s" % (url, e))
            time.sleep(delay)
            delay *= 2


def prime_cache(cache):
    """Fill the cache from the repo tarball in one request. Falls back to
    per-file fetches (below) for anything still missing."""
    import io
    import tarfile
    have = sum(1 for kind in ("box", "key")
               for i in range(N) if os.path.exists(os.path.join(cache, kind, "%03d.%s" % (i, "csv" if kind == "box" else "json"))))
    if have >= 2 * N:
        return
    print("fetching the mirror tarball (once)…", file=sys.stderr)
    try:
        with urllib.request.urlopen(TARBALL, timeout=600) as r:
            data = r.read()
    except (urllib.error.URLError, urllib.error.HTTPError) as e:
        print("tarball unavailable (%s); falling back to per-file fetches" % e, file=sys.stderr)
        return
    n = 0
    with tarfile.open(fileobj=io.BytesIO(data), mode="r:gz") as tf:
        for m in tf.getmembers():
            parts = m.name.split("/")
            if len(parts) != 4 or parts[1] != "data" or parts[2] not in ("box", "key") or not m.isfile():
                continue
            dest = os.path.join(cache, parts[2], parts[3])
            os.makedirs(os.path.dirname(dest), exist_ok=True)
            with open(dest, "wb") as fh:
                fh.write(tf.extractfile(m).read())
            n += 1
    print("cached %d files from the tarball" % n, file=sys.stderr)


def ocr_text(box_csv):
    """The receipt as the OCR read it: one transcription per line, in the
    order the annotation lists them (top to bottom). A transcription may
    itself contain commas, so split on the eight coordinate commas only."""
    lines = []
    for raw in box_csv.decode("utf-8", "replace").splitlines():
        raw = raw.strip()
        if not raw:
            continue
        parts = raw.split(",", 8)
        if len(parts) < 9:
            continue
        lines.append(parts[8].strip())
    return "\n".join(lines)


def norm_date(s):
    """The date as a bookkeeper files it, or None if the printed form is not
    unambiguously day-first. Malaysian receipts print day first; a value
    whose first number cannot be a day, or whose month cannot be a month, is
    dropped rather than reinterpreted."""
    s = (s or "").strip()
    s = re.split(r"\s+\d{1,2}:\d{2}", s)[0]  # strip a trailing time
    m = re.match(r"^(\d{1,2})[/.\-\s]([A-Za-z]{3,9}|\d{1,2})[/.\-\s,]*(\d{2}|\d{4})$", s)
    if not m:
        return None
    d, mo, y = m.groups()
    if mo.isalpha():
        mo = MONTHS.get(mo[:3].lower())
        if not mo:
            return None
    d, mo, y = int(d), int(mo), int(y)
    if y < 100:
        y += 2000
    try:
        return datetime(y, mo, d).strftime("%d/%m/%Y")
    except ValueError:
        return None


def norm_total(s):
    s = re.sub(r"[^0-9.\-]", "", (s or "").replace(",", ""))
    if not s:
        return None
    try:
        return "%.2f" % float(s)
    except ValueError:
        return None


def main():
    ap = argparse.ArgumentParser()
    here = os.path.dirname(os.path.abspath(__file__))
    ap.add_argument("--out", default=os.path.join(here, "data", "sroie.jsonl"))
    ap.add_argument("--cache", default=os.path.join(here, "data", "cache", "sroie"))
    ap.add_argument("--limit", type=int, default=N)
    args = ap.parse_args()

    prime_cache(args.cache)
    rows, dropped = [], {"date": 0, "total": 0, "text": 0}
    for i in range(min(args.limit, N)):
        rid = "%03d" % i
        box = fetch("%s/box/%s.csv" % (RAW, rid), os.path.join(args.cache, "box", rid + ".csv"))
        key = fetch("%s/key/%s.json" % (RAW, rid), os.path.join(args.cache, "key", rid + ".json"))
        k = json.loads(key.decode("utf-8", "replace"))
        text = ocr_text(box)
        if len(text.strip()) < 40:
            dropped["text"] += 1
            continue
        date = norm_date(k.get("date"))
        if not date:
            dropped["date"] += 1
            continue
        total = norm_total(k.get("total"))
        if total is None:
            dropped["total"] += 1
            continue
        rows.append({
            "id": rid,
            "text": text,
            "text_len": len(text),
            "truth": {
                "Invoice Date": date,
                "Vendor Name": (k.get("company") or "").strip(),
                "Amount": total,
                "Vendor Address": (k.get("address") or "").strip(),
            },
        })
        if (i + 1) % 100 == 0:
            print("  %d fetched" % (i + 1), file=sys.stderr)

    os.makedirs(os.path.dirname(args.out), exist_ok=True)
    with open(args.out, "w", encoding="utf-8") as fh:
        for r in rows:
            fh.write(json.dumps(r, ensure_ascii=False) + "\n")
    print("wrote %d receipts to %s (dropped: %s)" % (
        len(rows), args.out, ", ".join("%s %d" % kv for kv in dropped.items())))


if __name__ == "__main__":
    sys.exit(main())
