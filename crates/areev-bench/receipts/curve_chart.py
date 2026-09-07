#!/usr/bin/env python3
"""The learning-curve chart: exact-match rate against documents learned from,
on a log axis, for the LLM carrying the checkpoint's rules and the small
model tuned from scratch or continually -- one SVG per held-out set, light
and dark. Reads CURVE.json (curve_stats.py --write).

    curve_chart.py <runs-root|CURVE.json> --out docs/assets/curve
      -> curve-unseen-{light,dark}.svg, curve-seen-{light,dark}.svg
         and, when the prequential reads exist, curve-next-{light,dark}.svg
"""
import argparse
import json
import math
import os
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from fourway import THEMES, esc  # noqa: E402

W, H = 920, 420
PAD_L, PAD_T, PLOT_H, CURVE_W = 58, 34, 300, 660
SERIES = [("none", "no memory (day-one agent)", "none", ""),
          ("mem0-default", "mem0, as installed", "mem0/default", ""),
          ("mem0-domain", "mem0, domain-prompted", "mem0/domain", ""),
          ("mem0-domain-rules", "mem0, domain-prompted, framed as instructions", "mem0/raw", "6 4"),
          ("mem0-domain-taskq", "mem0, domain-prompted, retrieved by task question (final read)", "mem0/domain", "2 3"),
          ("mem0-domain-bothq", "mem0, domain-prompted, task question + document (final read, 1 seed)", "mem0/raw", "2 3"),
          ("mem0-domain-taskq-rules", "mem0, domain-prompted, task question, framed as instructions (final read)", "mem0/default", "2 3"),
          ("llm", "Areev governed: LLM with the loop's rules", "areev", ""),
          ("scratch", "Areev tuned: 1.7B from scratch", "slm/tuned", ""),
          ("continual", "Areev tuned: 1.7B continually", "slm/tuned", "6 4"),
          ("live", "governed agent as it ran", "areev_rep", "2 4")]
TITLE = {"unseen": "registrants the agent never learned from",
         "seen": "the stream's own registrants, other filings",
         "next": "the next 20 documents of the stream (prequential)"}


def chart(theme, res, hold):
    t = THEMES[theme]
    pooled = res["pooled"]
    cks = sorted(int(k) for k in pooled if k != "base")
    if not cks:
        return None
    lo, hi = math.log2(cks[0]), math.log2(cks[-1])
    span = max(hi - lo, 1.0)
    px = lambda k: PAD_L + CURVE_W * ((math.log2(k) - lo) / span)
    py = lambda v: PAD_T + PLOT_H - PLOT_H * (v / 100.0)
    series = {}
    for key, label, col, dash in SERIES:
        pts = []
        for k in cks:
            c = pooled[str(k)].get("%s|%s" % (key, hold))
            if c and c.get("n"):
                ci = ((res.get("overfitting", {}).get(str(k), {}).get(key, {}) or {}).get(hold) or {}).get("ci95")
                pts.append((k, 100.0 * c["rate"], ci))
        if pts:
            series[key] = (label, t[col], dash, pts)
    if not series:
        return None
    base = res["pooled"].get("base", {}).get(hold)
    n_seeds = len(res.get("seeds", {}))
    o = [f'<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {W} {H}" width="{W}" height="{H}" '
         f'font-family="ui-sans-serif,-apple-system,Segoe UI,Roboto,sans-serif">']
    a = o.append
    ends = "; ".join("%s %.0f%% at %d" % (lab, pts[-1][1], pts[-1][0]) for lab, _c, _d, pts in series.values())
    a(f'<title>{esc("Exact-match rate on %s as the deployment grows: %s." % (TITLE[hold], ends))}</title>')
    a(f'<desc>{esc("Documents learned from on a log axis. No memory is the day-one agent, read once per seed. mem0 stores each accountant exchange and retrieves into the prompt. The LLM carries the rules the governed loop had approved at each checkpoint; the small model is tuned on the filed rows up to it, from the plain base or from the previous checkpoint. Pooled over %d seed(s); bars are 95%% Wilson intervals where recorded." % n_seeds)}</desc>')
    for i in range(5):
        v = 25 * i
        a(f'<line x1="{PAD_L}" y1="{py(v):.1f}" x2="{PAD_L+CURVE_W}" y2="{py(v):.1f}" stroke="{t["grid"]}"/>')
        a(f'<text x="{PAD_L-9}" y="{py(v)+4:.1f}" text-anchor="end" font-size="11" fill="{t["muted"]}">{v}%</text>')
    for k in cks:
        a(f'<text x="{px(k):.1f}" y="{PAD_T+PLOT_H+19}" text-anchor="middle" font-size="11" fill="{t["muted"]}">{k}</text>')
    a(f'<text x="{PAD_L-32}" y="{PAD_T-14}" font-size="11" fill="{t["muted"]}">{esc("exact-match rate on " + TITLE[hold])}</text>')
    a(f'<text x="{PAD_L+CURVE_W/2:.0f}" y="{PAD_T+PLOT_H+40}" text-anchor="middle" font-size="11.5" fill="{t["muted"]}">documents learned from (log scale)</text>')
    if base and base.get("n"):
        y = py(100.0 * base["rate"])
        a(f'<line x1="{PAD_L}" y1="{y:.1f}" x2="{PAD_L+CURVE_W}" y2="{y:.1f}" stroke="{t["slm/base"]}" stroke-width="1.2" stroke-dasharray="2 3"/>')
        a(f'<text x="{PAD_L+4}" y="{y-5:.1f}" font-size="10.5" fill="{t["slm/base"]}">{esc("untuned 1.7B under the final rules, %.0f%%" % (100.0 * base["rate"]))}</text>')
    labels = []
    for key, (label, col, dash, pts) in series.items():
        if len(pts) > 1:
            d = " ".join(("M" if i == 0 else "L") + f"{px(k):.1f} {py(v):.1f}" for i, (k, v, _ci) in enumerate(pts))
            dasha = f' stroke-dasharray="{dash}"' if dash else ""
            a(f'<path d="{d}" fill="none" stroke="{col}" stroke-width="2.4" stroke-linejoin="round"{dasha}/>')
        for k, v, ci in pts:
            if ci and ci[0] is not None:
                a(f'<line x1="{px(k):.1f}" y1="{py(100*ci[0]):.1f}" x2="{px(k):.1f}" y2="{py(100*ci[1]):.1f}" stroke="{col}" stroke-width="1.2" opacity="0.7"/>')
            a(f'<circle cx="{px(k):.1f}" cy="{py(v):.1f}" r="{5 if len(pts) == 1 else 3.2}" fill="{col}"/>')
        labels.append([label, col, px(pts[-1][0]) + 10, py(pts[-1][1]) + 4])
    labels.sort(key=lambda e: e[3])
    for i in range(1, len(labels)):
        if labels[i][3] - labels[i - 1][3] < 15:
            labels[i][3] = labels[i - 1][3] + 15
    for lab, col, x, y in labels:
        a(f'<text x="{x:.1f}" y="{y:.1f}" font-size="12" font-weight="600" fill="{col}">{esc(lab)}</text>')
    a(f'<line x1="{PAD_L}" y1="{py(0):.1f}" x2="{PAD_L+CURVE_W}" y2="{py(0):.1f}" stroke="{t["axis"]}"/>')
    a("</svg>")
    return "\n".join(o)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("src", help="runs root holding CURVE.json, or the file")
    ap.add_argument("--out", required=True, help="SVG stem: writes STEM-<holdout>-{light,dark}.svg")
    args = ap.parse_args()
    p = args.src if args.src.endswith(".json") else os.path.join(args.src, "CURVE.json")
    res = json.load(open(p))
    os.makedirs(os.path.dirname(os.path.abspath(args.out)), exist_ok=True)
    for hold in ("unseen", "seen", "next"):
        for theme in ("light", "dark"):
            svg = chart(theme, res, hold)
            if svg is None:
                continue
            path = "%s-%s-%s.svg" % (args.out, hold, theme)
            open(path, "w").write(svg)
            print("wrote", path)


if __name__ == "__main__":
    sys.exit(main())
