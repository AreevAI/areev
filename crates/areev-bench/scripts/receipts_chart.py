#!/usr/bin/env python3
"""Render the receipts result as one figure (light + dark SVG).

    receipts_chart.py OUT_STEM LABEL=DIR [LABEL=DIR …]

    receipts_chart.py docs/assets/receipts-selfimprove \
      "run 1=crates/areev-bench/results/receipts-sroie-2026-09-04" \
      "run 2=crates/areev-bench/results/receipts-sroie-run2-2026-09-04"

Writes OUT_STEM-light.svg and OUT_STEM-dark.svg. Two or more cells; the
order given is the order drawn, and the label is what appears on the chart.

Two panels, because the finding needs both halves and neither carries it
alone.

**Left — the six learning curves.** Every seed of both runs, the same
held-out receipts scored against memory as it stood after 0, 10, 20, 30 and
40 experience receipts. All six start on the same shelf. Three climb at the
first checkpoint and hold; three stay flat, swing and fall back, or go to
zero. Colour encodes the run, so the separation is the argument and no
annotation is needed to see it.

**Right — the causal pair, pooled, per cell.** Rules rolled back against
rules applied. The left bar of every pair is the same height, because arm A
is 97/720 in each: same agent, same receipts, same seeds, same rollback
path, runs hours apart. That equality is the drift check that makes the
right-hand bars comparable at all, so it is drawn as a rule across them
rather than asserted in prose.

**The visible SVG is the chart and nothing else** — no headline, no stats
table. The narrative lives once as real text in RECEIPTS.md. The full
finding is in the SVG's `<title>` and `<desc>`, which never render: a
screen reader, a crawler or `grep` gets the result without the picture.

Numbers come from each run's `RESULTS.json`, which `receipts/verify.py`
computes from the trials — imported, never retyped, so the picture cannot
drift from the tables.

Stdlib only, same reasoning as scripts/bench_chart.py.
"""

from __future__ import annotations

import json
import os
import sys

W, H = 920, 384
PAD_L, PAD_T = 58, 34
PANEL_GAP = 62
CURVE_W = 500
BAR_W = W - PAD_L - CURVE_W - PANEL_GAP - 26
PLOT_H = 292
XS = [0, 10, 20, 30, 40]

# One hue per cell, in the order given. The first is the cautionary cell and
# reads as receded without being faint; the last is the result and gets the
# saturated one; anything between them is the ablation, in a neutral third.
THEMES = {
    "light": {
        "fg": "#1b1b1f", "muted": "#5f6470", "grid": "#e4e6ec", "axis": "#b9bec9",
        "series": ["#a4442f", "#8a6d1f", "#2f5f9e", "#1f6f4f"],
        "off": "#9aa0ac",
    },
    "dark": {
        "fg": "#e9eaee", "muted": "#9aa1ad", "grid": "#2c3038", "axis": "#464c57",
        "series": ["#e88a70", "#d3b04a", "#7aa8e0", "#5fcd9b"],
        "off": "#767d8a",
    },
}


def _shelf(peak):
    """A round ceiling just above the tallest value, so bars and curves have
    headroom for their labels without a hand-set constant per corpus."""
    for step in (20, 25, 40, 50, 100, 200, 250):
        top = -(-int(peak * 1.08) // step) * step
        if top >= peak * 1.05 and top / step <= 10:
            return top
    return int(peak * 1.15) + 1


def series_colour(t, i, n):
    """First cell gets the first hue, last cell the last; the middle spreads
    across what is left, so a 2-cell figure keeps the original red/green."""
    pal = t["series"]
    if n == 1:
        return pal[-1]
    if i == 0:
        return pal[0]
    if i == n - 1:
        return pal[-1]
    return pal[1 + ((i - 1) % (len(pal) - 2))]


def esc(s):
    return (s.replace("&", "&amp;").replace("<", "&lt;").replace(">", "&gt;"))


def load(run_dir):
    with open(os.path.join(run_dir, "RESULTS.json"), encoding="utf-8") as fh:
        return json.load(fh)


def curves(res):
    """[(seed_label, [exact at 0,10,20,30,40]), …] — the 40 point is arm B,
    which is the same memory the curve would reach and the state the paired
    test scores."""
    out = []
    for s in res["seeds"]:
        c = s.get("curve") or {}
        pts = [c.get(str(x), {}).get("exact") for x in XS[:-1]]
        pts.append(s["passed_B"]["exact"])
        out.append((s["seed_dir"], pts))
    return out


def svg(theme, cells, noun="receipts"):
    """`cells` is [(label, RESULTS.json dict), …] in draw order."""
    t = THEMES[theme]
    n = len(cells)
    series = [(label, curves(res), series_colour(t, i, n)) for i, (label, res) in enumerate(cells)]
    # Scales come from the data, not from constants: the two corpora score a
    # different number of (document, field) trials per seed, and a chart that
    # hard-codes one silently mis-draws the other.
    trials = max(res["pooled"]["trials_per_arm"] for _l, res in cells)
    per_seed = max(s["trials_per_arm"] for _l, res in cells for s in res["seeds"])
    peak = max((v for _l, cs, _c in series for _s, pts in cs for v in pts if v is not None),
               default=1)
    ymax = _shelf(peak)
    bmax = _shelf(max(res["pooled"]["passed"][a]["exact"]
                      for _l, res in cells for a in ("A", "B")))
    same_a = len({res["pooled"]["passed"]["A"]["exact"] for _l, res in cells}) == 1
    px = lambda v: PAD_L + CURVE_W * (v / 40.0)
    py = lambda v: PAD_T + PLOT_H - PLOT_H * (v / ymax)

    o = []
    a = o.append
    a(f'<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {W} {H}" width="{W}" '
      f'height="{H}" font-family="ui-sans-serif,-apple-system,Segoe UI,Roboto,sans-serif">')
    headline = "; ".join(
        "%s reaches %d of %d exact against a rolled-back baseline of %d"
        % (label, res["pooled"]["passed"]["B"]["exact"], trials,
           res["pooled"]["passed"]["A"]["exact"])
        for label, res in cells)
    a(f'<title>{esc("Governed self-improvement on public " + noun + ": " + headline + ".")}</title>')
    detail = " ".join(
        "%s: seeds end at %s of about %d, arm A pooled %d of %d and arm B %d, paired %d wins to %d losses."
        % (label,
           ", ".join(str(s["passed_B"]["exact"]) for s in res["seeds"]), per_seed,
           res["pooled"]["passed"]["A"]["exact"], trials,
           res["pooled"]["passed"]["B"]["exact"],
           res["pooled"]["exact_B_vs_A"]["wins"], res["pooled"]["exact_B_vs_A"]["losses"])
        for label, res in cells)
    starts = [pts[0] for _l, cs, _c in series for _s, pts in cs if pts and pts[0] is not None]
    shelf = ("every curve starts on the same shelf near %d of about %d. "
             % (round(sum(starts) / len(starts)), per_seed)) if starts else ""
    a(f'<desc>{esc("Left panel: learning curves over the same held-out " + noun + ", scored after 0, 10, 20, 30 and 40 experience documents; " + shelf + detail + " Right panel: pooled arms" + (" per cell. Arm A is the same height in every cell because the agent, documents, seeds and rollback path are identical, so the difference between the B bars is what each loop configuration learned and a reviewer approved." if n > 1 and same_a else ", rolled back against applied."))}</desc>')

    for frac in range(0, 5):
        v = ymax * frac / 4.0
        y = py(v)
        a(f'<line x1="{PAD_L}" y1="{y:.1f}" x2="{PAD_L+CURVE_W}" y2="{y:.1f}" '
          f'stroke="{t["grid"]}" stroke-width="1"/>')
        a(f'<text x="{PAD_L-9}" y="{y+4:.1f}" text-anchor="end" font-size="11" '
          f'fill="{t["muted"]}">{int(v)}</text>')
    # Both panels count exact matches over different denominators — one seed's
    # held-out set on the left, all three pooled on the right. Saying so is
    # cheaper than a reader mis-reading 141 against 382.
    a(f'<text x="{PAD_L-32}" y="{PAD_T-13}" font-size="11" '
      f'fill="{t["muted"]}">exact, of ~{per_seed} per seed</text>')
    for x in XS:
        a(f'<text x="{px(x):.1f}" y="{PAD_T+PLOT_H+19}" text-anchor="middle" '
          f'font-size="11" fill="{t["muted"]}">{x}</text>')
    a(f'<text x="{PAD_L+CURVE_W/2:.0f}" y="{PAD_T+PLOT_H+38}" text-anchor="middle" '
      f'font-size="11.5" fill="{t["muted"]}">experience documents seen</text>')

    for _label, cs, colour in series:
        for i, (_seed, pts) in enumerate(cs):
            d = " ".join(("M" if k == 0 else "L") + f"{px(x):.1f},{py(v):.1f}"
                         for k, (x, v) in enumerate(zip(XS, pts)) if v is not None)
            if not d:
                continue
            a(f'<path d="{d}" fill="none" stroke="{colour}" stroke-width="2.2" '
              f'stroke-linejoin="round" stroke-linecap="round" opacity="{0.95 - 0.13*i:.2f}"/>')
            for x, v in zip(XS, pts):
                if v is not None:
                    a(f'<circle cx="{px(x):.1f}" cy="{py(v):.1f}" r="2.8" fill="{colour}" '
                      f'opacity="{0.95 - 0.13*i:.2f}"/>')

    # One label per cell, at the right end of its topmost curve, nudged apart
    # so two cells that finish close together stay readable.
    ends = sorted(((max(v for _s, p in cs for v in [p[-1]] if v is not None), lab, col)
                   for lab, cs, col in series), reverse=True)
    last_y = None
    for top, lab, colour in ends:
        y = py(top) + 4
        if last_y is not None and abs(y - last_y) < 15:
            y = last_y + 15
        last_y = y
        a(f'<text x="{px(40)+9:.1f}" y="{y:.1f}" font-size="12" font-weight="600" '
          f'fill="{colour}">{esc(lab)}</text>')

    bx0 = PAD_L + CURVE_W + PANEL_GAP
    bh = lambda v: PLOT_H * (v / bmax)
    # Each cell is an A/B pair plus a gap; slots are sized so any number fits.
    slot = BAR_W / (n * 2.6)
    a(f'<text x="{bx0}" y="{PAD_T-13}" font-size="11" fill="{t["muted"]}">'
      f'exact, of {trials} pooled trials</text>')
    # The dashed rule across the A bars asserts they are equal — the drift
    # check. Draw it only when they actually are, or it quietly claims a
    # control held when it did not.
    a_height = None
    for ci, (label, res) in enumerate(cells):
        base = bx0 + ci * slot * 2.6
        for bi, (arm, colour) in enumerate((("A", t["off"]),
                                            ("B", series_colour(t, ci, n)))):
            v = res["pooled"]["passed"][arm]["exact"]
            if arm == "A" and same_a:
                a_height = PAD_T + PLOT_H - bh(v)
            x = base + bi * slot
            h = bh(v)
            y = PAD_T + PLOT_H - h
            a(f'<rect x="{x:.1f}" y="{y:.1f}" width="{slot*0.8:.1f}" height="{h:.1f}" '
              f'rx="2.5" fill="{colour}"/>')
            a(f'<text x="{x+slot*0.4:.1f}" y="{y-7:.1f}" text-anchor="middle" font-size="11.5" '
              f'font-weight="600" fill="{t["fg"]}">{v}</text>')
            a(f'<text x="{x+slot*0.4:.1f}" y="{PAD_T+PLOT_H+19}" text-anchor="middle" '
              f'font-size="10.5" fill="{t["muted"]}">{arm}</text>')
        a(f'<text x="{base+slot:.1f}" y="{PAD_T+PLOT_H+38}" text-anchor="middle" '
          f'font-size="11" fill="{t["muted"]}">{esc(label)}</text>')
    if a_height is not None:
        a(f'<line x1="{bx0-4:.1f}" y1="{a_height:.1f}" '
          f'x2="{bx0+(n-1)*slot*2.6+slot*1.8:.1f}" y2="{a_height:.1f}" '
          f'stroke="{t["axis"]}" stroke-width="1" stroke-dasharray="3 3"/>')

    a(f'<line x1="{PAD_L}" y1="{PAD_T+PLOT_H}" x2="{PAD_L+CURVE_W}" y2="{PAD_T+PLOT_H}" '
      f'stroke="{t["axis"]}" stroke-width="1"/>')
    a(f'<line x1="{bx0-4:.1f}" y1="{PAD_T+PLOT_H}" '
      f'x2="{bx0+(n-1)*slot*2.6+slot*1.9:.1f}" y2="{PAD_T+PLOT_H}" '
      f'stroke="{t["axis"]}" stroke-width="1"/>')
    a("</svg>")
    return "\n".join(o)


def main():
    argv = sys.argv[1:]
    # The corpus noun rides in the alt text, which screen readers and search
    # indexes read; it is the one thing in this chart that no results file knows.
    noun = "receipts"
    if "--noun" in argv:
        i = argv.index("--noun")
        noun = argv[i + 1]
        del argv[i:i + 2]
    if len(argv) < 2:
        raise SystemExit("usage: receipts_chart.py [--noun N] OUT_STEM LABEL=DIR [LABEL=DIR …]")
    stem = argv[0]
    cells = []
    for spec in argv[1:]:
        if "=" not in spec:
            raise SystemExit("each cell is LABEL=DIR, got %r" % spec)
        label, d = spec.split("=", 1)
        cells.append((label, load(d)))
    os.makedirs(os.path.dirname(stem) or ".", exist_ok=True)
    for theme in ("light", "dark"):
        path = f"{stem}-{theme}.svg"
        with open(path, "w", encoding="utf-8") as fh:
            fh.write(svg(theme, cells, noun))
        print("wrote", path)


if __name__ == "__main__":
    main()
