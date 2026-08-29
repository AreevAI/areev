#!/usr/bin/env python3
"""Render the A/B/A/B self-improvement result as a bar chart (light + dark SVG).

    aba_chart.py OUT_STEM RUN_DIR

    aba_chart.py docs/assets/aba-selfimprove \
      crates/areev-bench/results/selfimprove-3seed-qwen3-30b-2026-08-26

Writes OUT_STEM-light.svg and OUT_STEM-dark.svg.

ONE run, four bars, read left to right: the lessons are off, applied, rolled
back, applied again. Colour encodes the only thing that changes — whether the
agent's learned lessons are in its prompt — so the alternation IS the causal
argument: the score follows the lessons, twice, in both directions.

The brackets between bars carry the transition: the change in points and its
paired McNemar p. That is the part a reader should take away, so it is drawn
rather than left in a table.

**The visible SVG is the chart and nothing else** — no headline, no prose, no
stats table. The narrative lives once, as real text in the README and
RESULTS.md, where search engines index it; drawing it here too made the two
drift and the page noisy. The full finding (rates, deltas, McNemar rows) is
still stated in the SVG's `<title>` and `<desc>`, which never render: a
screen reader, a crawler, or `grep` on the file gets the result without the
bars, at zero visual cost.

Numbers come from `report.json` (rates) and `aba_stats.py` (paired McNemar),
imported rather than reimplemented so this chart cannot disagree with the tool
that produced the published statistics.

Stdlib only, same reasoning as scripts/bench_chart.py.
"""

from __future__ import annotations

import json
import os
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import aba_stats  # noqa: E402

STATES = ["A0", "B", "A1", "B2"]
# (label, sublabel, lessons-on?) — the sublabel is what actually happened to
# the memory, in plain words, because "A0/B/A1/B2" means nothing to a reader
# arriving from a search result.
STATE_META = {
    "A0": ("A0", "before learning", False),
    "B": ("B", "lessons applied", True),
    "A1": ("A1", "lessons rolled back", False),
    "B2": ("B2", "lessons re-applied", True),
}
TRANSITIONS = [("A0", "B", "apply"), ("B", "A1", "roll back"), ("A1", "B2", "re-apply")]

# Two categorical slots: lessons OFF and lessons ON. Both pairs pass
# scripts/validate_palette.js on their own surface (CVD separation dE 28.9
# light / 26.8 dark, normal-vision 37.0 / 31.8, contrast >= 3:1). Bars are
# also direct-labelled and sub-labelled, so identity never rests on hue.
THEMES = {
    "light": {
        "bg": "#ffffff", "line": "#dfe3e8", "fg": "#111418", "muted": "#5b6470",
        "off": "#eb6834", "on": "#1f6feb",
    },
    "dark": {
        "bg": "#0d1117", "line": "#30363d", "fg": "#e6edf3", "muted": "#8b949e",
        "off": "#d95926", "on": "#3987e5",
    },
}


def esc(s):
    return str(s).replace("&", "&amp;").replace("<", "&lt;").replace(">", "&gt;")


def load_run(run_dir):
    """({state: (rate, successes, n)}, {(x, y): (b, c, n, p)}, n_seeds).

    A directory may hold several seeds. Rates are POOLED — successes and tasks
    summed across seeds, which is the mean RESULTS.md quotes — so each bar and
    the p-value beside it describe the same population.
    """
    reports = sorted(
        f for f in os.listdir(run_dir)
        if f == "report.json" or f.endswith(".report.json")
    )
    if not reports:
        raise SystemExit(f"aba_chart: no report.json in {run_dir}")

    totals = {}
    for name in reports:
        with open(os.path.join(run_dir, name), encoding="utf-8") as fh:
            for e in json.load(fh).get("evals", []):
                if e["state"] in STATES:
                    s, n = totals.get(e["state"], (0, 0))
                    totals[e["state"]] = (s + e["successes"], n + e["n"])
    missing = [s for s in STATES if s not in totals]
    if missing:
        raise SystemExit(f"aba_chart: {run_dir} has no {', '.join(missing)} state(s)")
    rates = {st: (s / n if n else 0.0, s, n) for st, (s, n) in totals.items()}

    runs = aba_stats.load_runs(run_dir)
    stats = {}
    present = set()
    for _, r in runs:
        present |= set(r)
    # Governed transitions only; `pairs_for` also emits passive-arm
    # comparisons, which answer a different question than this chart.
    for x, y in aba_stats.GOVERNED_PAIRS:
        if x in present and y in present:
            b, c, n, _ = aba_stats.compare(runs, x, y)
            if n:
                stats[(x, y)] = (b, c, n, aba_stats.mcnemar_exact(b, c))
    return rates, stats, len(reports)


def fmt_p(p):
    return "p &lt; 0.0001" if p < 0.0001 else f"p = {p:.3f}".rstrip("0").rstrip(".")


def bar(x, y, w, h, fill):
    """A bar with 4px-rounded top corners, anchored square to the baseline."""
    r = min(4.0, w / 2.0, h)
    if h <= 0:
        return ""
    return (
        f'<path d="M{x:.1f},{y + h:.1f} L{x:.1f},{y + r:.1f} '
        f'Q{x:.1f},{y:.1f} {x + r:.1f},{y:.1f} L{x + w - r:.1f},{y:.1f} '
        f'Q{x + w:.1f},{y:.1f} {x + w:.1f},{y + r:.1f} '
        f'L{x + w:.1f},{y + h:.1f} Z" fill="{fill}"/>'
    )


def render(theme, rates, stats, n_seeds, headline):
    c = THEMES[theme]
    W = 860
    plot_x, plot_y, plot_w, plot_h = 74, 64, 756, 196
    y_max = 0.85
    group_w = plot_w / len(STATES)
    bar_w = 84.0

    n_total = rates["B"][2]
    seeds = f"{n_seeds} seeded runs" if n_seeds > 1 else "1 run"

    text_rows = [
        f"{STATE_META[s][0]} {STATE_META[s][1]}: {rates[s][0]:.1%} "
        f"({rates[s][1]}/{rates[s][2]} tasks passed)"
        for s in STATES
    ]
    stat_rows = []
    for x, y, verb in TRANSITIONS:
        if (x, y) in stats:
            b, cc, n, p = stats[(x, y)]
            delta = (rates[y][0] - rates[x][0]) * 100
            stat_rows.append(
                f"{verb} ({x}->{y}): {delta:+.1f} pts, "
                f"{fmt_p(p).replace('&lt;', '<')}, b={b} c={cc} n={n}"
            )
    for x, y in (("A0", "A1"), ("B", "B2")):
        if (x, y) in stats:
            b, cc, n, p = stats[(x, y)]
            stat_rows.append(
                f"control {x}->{y} (same condition, expect no change): "
                f"{fmt_p(p).replace('&lt;', '<')}, b={b} c={cc} n={n}"
            )

    # Chart only: legend, bars, brackets. Height ends at the bar sublabels.
    height = int(plot_y + plot_h + 50 + 22)

    o = []
    a = o.append
    a(f'<svg xmlns="http://www.w3.org/2000/svg" width="{W}" height="{height}" '
      f'viewBox="0 0 {W} {height}" role="img" '
      f'aria-labelledby="aba-title aba-desc" '
      f'font-family="-apple-system,BlinkMacSystemFont,Segoe UI,Helvetica,Arial,sans-serif">')
    a(f'<title id="aba-title">{esc(headline)}</title>')
    a(f'<desc id="aba-desc">Areev governed self-improvement benchmark: an AI '
      f'agent learns from its own tool-use failures, and the learned lessons '
      f'are applied, rolled back, and re-applied while everything else — model, '
      f'prompts, tools, and the {n_total} held-out tasks — is held fixed. '
      f'{esc("; ".join(text_rows))}. Paired exact McNemar over the same tasks: '
      f'{esc("; ".join(stat_rows))}. Removing the lessons removes the gain and '
      f'restoring them brings it back, which is what makes the improvement '
      f'causal rather than correlational. Measured over {seeds} at temperature '
      f'zero; b = tasks that failed in the first state and passed in the second, '
      f'c = the reverse.</desc>')
    a(f'<rect width="{W}" height="{height}" fill="{c["bg"]}"/>')

    # Legend — identity is a condition, not a run.
    for i, (key, label) in enumerate((("off", "lessons OFF"), ("on", "lessons ON"))):
        lx = plot_x + i * 150
        a(f'<rect x="{lx}" y="{18}" width="11" height="11" rx="2" fill="{c[key]}"/>')
        a(f'<text x="{lx + 17}" y="{28}" font-size="12" fill="{c["fg"]}">'
          f"{esc(label)}</text>")

    for frac in (0.0, 0.2, 0.4, 0.6, 0.8):
        gy = plot_y + plot_h - (frac / y_max) * plot_h
        a(f'<line x1="{plot_x}" y1="{gy:.1f}" x2="{plot_x + plot_w}" y2="{gy:.1f}" '
          f'stroke="{c["line"]}" stroke-width="1"/>')
        a(f'<text x="{plot_x - 12}" y="{gy + 4:.1f}" font-size="11" '
          f'text-anchor="end" fill="{c["muted"]}">{int(frac * 100)}%</text>')

    centers, tops = {}, {}
    for gi, st in enumerate(STATES):
        rate, succ, n = rates[st]
        h = (rate / y_max) * plot_h
        bx = plot_x + gi * group_w + (group_w - bar_w) / 2
        by = plot_y + plot_h - h
        centers[st], tops[st] = bx + bar_w / 2, by
        code, sub, on = STATE_META[st]
        a(bar(bx, by, bar_w, h, c["on"] if on else c["off"]))
        a(f'<text x="{bx + bar_w / 2:.1f}" y="{by - 8:.1f}" font-size="15" '
          f'font-weight="700" text-anchor="middle" fill="{c["fg"]}">'
          f"{rate:.1%}</text>")
        a(f'<text x="{bx + bar_w / 2:.1f}" y="{plot_y + plot_h + 19:.1f}" '
          f'font-size="12" font-weight="600" text-anchor="middle" '
          f'fill="{c["fg"]}">{esc(code)}</text>')
        a(f'<text x="{bx + bar_w / 2:.1f}" y="{plot_y + plot_h + 35:.1f}" '
          f'font-size="11" text-anchor="middle" fill="{c["muted"]}">'
          f"{esc(sub)}</text>")
        a(f'<text x="{bx + bar_w / 2:.1f}" y="{plot_y + plot_h + 50:.1f}" '
          f'font-size="10" text-anchor="middle" fill="{c["muted"]}">'
          f"{succ}/{n} passed</text>")

    a(f'<line x1="{plot_x}" y1="{plot_y + plot_h}" x2="{plot_x + plot_w}" '
      f'y2="{plot_y + plot_h}" stroke="{c["muted"]}" stroke-width="1"/>')

    # Transition brackets: the change and its p-value, drawn above the pair.
    for x, y, verb in TRANSITIONS:
        if (x, y) not in stats:
            continue
        _, _, _, p = stats[(x, y)]
        delta = (rates[y][0] - rates[x][0]) * 100
        x1, x2 = centers[x], centers[y]
        by = min(tops[x], tops[y]) - 34
        a(f'<path d="M{x1:.1f},{by + 9:.1f} L{x1:.1f},{by:.1f} '
          f'L{x2:.1f},{by:.1f} L{x2:.1f},{by + 9:.1f}" fill="none" '
          f'stroke="{c["muted"]}" stroke-width="1"/>')
        a(f'<text x="{(x1 + x2) / 2:.1f}" y="{by - 14:.1f}" font-size="12.5" '
          f'font-weight="700" text-anchor="middle" fill="{c["fg"]}">'
          f"{delta:+.1f} pts</text>")
        a(f'<text x="{(x1 + x2) / 2:.1f}" y="{by - 3:.1f}" font-size="10" '
          f'text-anchor="middle" fill="{c["muted"]}">{esc(verb)} · {fmt_p(p)}</text>')

    a("</svg>")
    return "\n".join(o)


def main(argv):
    if len(argv) != 3:
        print("usage: aba_chart.py OUT_STEM RUN_DIR", file=sys.stderr)
        return 2
    out_stem, run_dir = argv[1], argv[2]
    rates, stats, n_seeds = load_run(run_dir)
    gain = (rates["B"][0] - rates["A0"][0]) * 100
    headline = (
        f"An AI agent that learns from its own mistakes: "
        f"{gain:+.1f} points on held-out tasks, and it disappears when you "
        f"remove what it learned"
    )
    for theme in ("light", "dark"):
        path = f"{out_stem}-{theme}.svg"
        os.makedirs(os.path.dirname(path) or ".", exist_ok=True)
        with open(path, "w", encoding="utf-8") as fh:
            fh.write(render(theme, rates, stats, n_seeds, headline) + "\n")
        print(f"wrote {path}")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
