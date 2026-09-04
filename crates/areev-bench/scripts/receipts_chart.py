#!/usr/bin/env python3
"""Render the receipts result as one figure (light + dark SVG).

    receipts_chart.py OUT_STEM RUN1_DIR RUN2_DIR

    receipts_chart.py docs/assets/receipts-selfimprove \
      crates/areev-bench/results/receipts-sroie-2026-09-04 \
      crates/areev-bench/results/receipts-sroie-run2-2026-09-04

Writes OUT_STEM-light.svg and OUT_STEM-dark.svg.

Two panels, because the finding needs both halves and neither carries it
alone.

**Left — the six learning curves.** Every seed of both runs, the same
held-out receipts scored against memory as it stood after 0, 10, 20, 30 and
40 experience receipts. All six start on the same shelf. Three climb at the
first checkpoint and hold; three stay flat, swing and fall back, or go to
zero. Colour encodes the run, so the separation is the argument and no
annotation is needed to see it.

**Right — the causal pair, pooled.** Rules rolled back against rules
applied, for each run. The left bar of each pair is the same height,
because arm A is 97/720 in both runs: same agent, same receipts, same
seeds, same rollback path. That equality is what makes the right-hand bars
comparable at all, so it is drawn rather than asserted.

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
TRIALS = 720          # pooled trials per arm
PER_SEED_TRIALS = 240
XS = [0, 10, 20, 30, 40]

THEMES = {
    "light": {
        "fg": "#1b1b1f", "muted": "#5f6470", "grid": "#e4e6ec", "axis": "#b9bec9",
        "bg": "none",
        # One hue per run. Run 2 is the result, so it gets the saturated one;
        # run 1 is the cautionary half and reads as receded without being faint.
        "run2": "#1f6f4f", "run2_soft": "#8fc4ad",
        "run1": "#a4442f", "run1_soft": "#dda997",
        "off": "#9aa0ac",
    },
    "dark": {
        "fg": "#e9eaee", "muted": "#9aa1ad", "grid": "#2c3038", "axis": "#464c57",
        "bg": "none",
        "run2": "#5fcd9b", "run2_soft": "#2f7a5c",
        "run1": "#e88a70", "run1_soft": "#8c4633",
        "off": "#767d8a",
    },
}


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


def svg(theme, r1, r2):
    t = THEMES[theme]
    c1, c2 = curves(r1), curves(r2)
    ymax = 160  # a shelf above the tallest point (150), in units of 240 trials
    px = lambda v: PAD_L + CURVE_W * (v / 40.0)
    py = lambda v: PAD_T + PLOT_H - PLOT_H * (v / ymax)

    o = []
    a = o.append
    a(f'<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {W} {H}" width="{W}" '
      f'height="{H}" font-family="ui-sans-serif,-apple-system,Segoe UI,Roboto,sans-serif">')
    a(f'<title>{esc("Governed self-improvement on public receipts: run 2 reaches 382 of 720 exact against a rolled-back baseline of 97, 286 paired wins to 1 loss; run 1, one engine defect earlier, reaches 70 of 720 and loses 30 to 3.")}</title>')
    a(f'<desc>{esc("Left panel: six learning curves over the same 60 held-out ICDAR-SROIE receipts, scored after 0, 10, 20, 30 and 40 experience receipts. All six start together near 31 of 240. Run 2 seeds 1, 2 and 3 rise at the first checkpoint to 124, 141 and 125 and hold, ending at 123, 141 and 118. Run 1 seeds stay flat at 31 and 36, or swing to 86 and fall to 33, or drop to 0 and stay there. Right panel: pooled arms. Run 1 arm A 97 of 720 and arm B 70; run 2 arm A 97 of 720 and arm B 382. Arm A is identical across both runs because the agent, receipts, seeds and rollback path are identical, so the difference between the B bars is what the loop learned and a reviewer approved.")}</desc>')

    # ── left panel: curves ────────────────────────────────────────────────
    for frac in range(0, 5):
        v = ymax * frac / 4.0
        y = py(v)
        a(f'<line x1="{PAD_L}" y1="{y:.1f}" x2="{PAD_L+CURVE_W}" y2="{y:.1f}" '
          f'stroke="{t["grid"]}" stroke-width="1"/>')
        a(f'<text x="{PAD_L-9}" y="{y+4:.1f}" text-anchor="end" font-size="11" '
          f'fill="{t["muted"]}">{int(v)}</text>')
    # Both panels count exact matches, but over different denominators — one
    # seed's held-out set on the left, all three pooled on the right. Saying
    # so on each axis is cheaper than a reader mis-reading 141 against 382.
    a(f'<text x="{PAD_L-32}" y="{PAD_T-13}" font-size="11" '
      f'fill="{t["muted"]}">exact, of 240 per seed</text>')
    for x in XS:
        a(f'<text x="{px(x):.1f}" y="{PAD_T+PLOT_H+19}" text-anchor="middle" '
          f'font-size="11" fill="{t["muted"]}">{x}</text>')
    a(f'<text x="{PAD_L+CURVE_W/2:.0f}" y="{PAD_T+PLOT_H+38}" text-anchor="middle" '
      f'font-size="11.5" fill="{t["muted"]}">experience receipts seen</text>')

    for label, series, colour, soft in (("run 1", c1, t["run1"], t["run1_soft"]),
                                        ("run 2", c2, t["run2"], t["run2_soft"])):
        for i, (_seed, pts) in enumerate(series):
            d = " ".join(("M" if k == 0 else "L") + f"{px(x):.1f},{py(v):.1f}"
                         for k, (x, v) in enumerate(zip(XS, pts)) if v is not None)
            a(f'<path d="{d}" fill="none" stroke="{colour}" stroke-width="2.4" '
              f'stroke-linejoin="round" stroke-linecap="round" opacity="{0.95 - 0.13*i:.2f}"/>')
            for x, v in zip(XS, pts):
                if v is not None:
                    a(f'<circle cx="{px(x):.1f}" cy="{py(v):.1f}" r="3" fill="{colour}" '
                      f'opacity="{0.95 - 0.13*i:.2f}"/>')

    # Curve labels sit at the right end of the topmost curve of each run.
    for label, series, colour in (("run 2", c2, t["run2"]), ("run 1", c1, t["run1"])):
        top = max(series, key=lambda s: s[1][-1] or 0)
        a(f'<text x="{px(40)+9:.1f}" y="{py(top[1][-1])+4:.1f}" font-size="12.5" '
          f'font-weight="600" fill="{colour}">{label}</text>')

    # ── right panel: pooled arms ──────────────────────────────────────────
    bx0 = PAD_L + CURVE_W + PANEL_GAP
    bmax = 480
    bh = lambda v: PLOT_H * (v / bmax)
    slot = BAR_W / 4.6
    bars = [
        ("run 1", "A", r1["pooled"]["passed"]["A"]["exact"], t["off"]),
        ("run 1", "B", r1["pooled"]["passed"]["B"]["exact"], t["run1"]),
        ("run 2", "A", r2["pooled"]["passed"]["A"]["exact"], t["off"]),
        ("run 2", "B", r2["pooled"]["passed"]["B"]["exact"], t["run2"]),
    ]
    for i, (run, arm, v, colour) in enumerate(bars):
        gap = 0.55 if i == 2 else 0
        x = bx0 + (i + gap) * slot
        h = bh(v)
        y = PAD_T + PLOT_H - h
        a(f'<rect x="{x:.1f}" y="{y:.1f}" width="{slot*0.78:.1f}" height="{h:.1f}" '
          f'rx="2.5" fill="{colour}"/>')
        a(f'<text x="{x+slot*0.39:.1f}" y="{y-7:.1f}" text-anchor="middle" font-size="12.5" '
          f'font-weight="600" fill="{t["fg"]}">{v}</text>')
        a(f'<text x="{x+slot*0.39:.1f}" y="{PAD_T+PLOT_H+19}" text-anchor="middle" '
          f'font-size="11" fill="{t["muted"]}">{arm}</text>')
    for i, run in ((0, "run 1"), (2, "run 2")):
        gap = 0.55 if i == 2 else 0
        cx = bx0 + (i + gap + 0.9) * slot
        a(f'<text x="{cx:.1f}" y="{PAD_T+PLOT_H+38}" text-anchor="middle" font-size="11.5" '
          f'fill="{t["muted"]}">{run}</text>')
    a(f'<text x="{bx0}" y="{PAD_T-13}" font-size="11" fill="{t["muted"]}">'
      f'exact, of {TRIALS} pooled trials</text>')
    # The equality that makes the pair readable, drawn as a rule across both A bars.
    ay = PAD_T + PLOT_H - bh(bars[0][2])
    a(f'<line x1="{bx0-4:.1f}" y1="{ay:.1f}" x2="{bx0+3.75*slot:.1f}" y2="{ay:.1f}" '
      f'stroke="{t["axis"]}" stroke-width="1" stroke-dasharray="3 3"/>')

    a(f'<line x1="{PAD_L}" y1="{PAD_T+PLOT_H}" x2="{PAD_L+CURVE_W}" y2="{PAD_T+PLOT_H}" '
      f'stroke="{t["axis"]}" stroke-width="1"/>')
    a(f'<line x1="{bx0-4:.1f}" y1="{PAD_T+PLOT_H}" x2="{bx0+3.9*slot:.1f}" '
      f'y2="{PAD_T+PLOT_H}" stroke="{t["axis"]}" stroke-width="1"/>')
    a("</svg>")
    return "\n".join(o)


def main():
    if len(sys.argv) != 4:
        raise SystemExit("usage: receipts_chart.py OUT_STEM RUN1_DIR RUN2_DIR")
    stem, d1, d2 = sys.argv[1], sys.argv[2], sys.argv[3]
    r1, r2 = load(d1), load(d2)
    os.makedirs(os.path.dirname(stem) or ".", exist_ok=True)
    for theme in ("light", "dark"):
        path = f"{stem}-{theme}.svg"
        with open(path, "w", encoding="utf-8") as fh:
            fh.write(svg(theme, r1, r2))
        print("wrote", path)


if __name__ == "__main__":
    main()
