#!/usr/bin/env python3
"""Render the governed-self-improvement chart for a real expense workflow.

Emits two artifacts:

    docs/assets/expense-learning-light.svg
    docs/assets/expense-learning-dark.svg

The numbers are QUOTED from crates/areev-bench/EXPENSE.md, which records the
measurement. They come from a private dataset (a real company's invoices and
spreadsheet) that is deliberately NOT in this repo — only the counts travel.
Like bench_chart.py, this is re-run by hand when that file changes. Stdlib
only, same reasoning as repo_stats.py.

Two things this chart does that the obvious version would not:

1. The x-axis is EXPERIENCE, and the task is held constant. The tempting
   chart — a running score across the invoices the agent is learning from —
   falls over time here, because the accountant adds required fields as they
   go (one field at the first invoice, seven by the sixteenth). That curve
   measures the goalpost moving. Every point below is instead the SAME 30
   held-out invoices and the same seven fields, read against memory as it
   stood after 0, 10, 20, 30 and 40 corrections.

2. The noise floor is drawn on the same axes as the effect, not tucked into a
   caption. It is the number that makes the rest mean anything: two passes
   over identical memory disagreed on 0-1 of 210 trials, so an effect of
   21-32 is real rather than a re-roll.
"""

from __future__ import annotations

from pathlib import Path

REPO = Path(__file__).resolve().parent.parent

# Same two themes as scripts/repo_stats.py, so the README's charts read as one.
THEMES = {
    "light": {
        "bg": "#ffffff", "panel": "#f6f7f9", "line": "#dfe3e8",
        "fg": "#111418", "muted": "#5b6470", "bar": "#1f6feb",
        "alt": "#8250df", "warn": "#bf8700",
    },
    "dark": {
        "bg": "#0d1117", "panel": "#161b22", "line": "#30363d",
        "fg": "#e6edf3", "muted": "#8b949e", "bar": "#58a6ff",
        "alt": "#bc8cff", "warn": "#d29922",
    },
}

TRIALS = 210  # 30 held-out invoices x the fields required of each

# EXPENSE.md §"Learning curve" — held-out accuracy vs corrections seen.
# The 0 point is the same memory with every recommendation rolled back
# through the API, so the baseline is measured rather than assumed.
CURVE = [
    (0, 0, 12, 1),
    (10, 38, 56, 4),
    (20, 36, 53, 4),
    (30, 36, 52, 4),
    (40, 36, 54, 4),
]

# EXPENSE.md §"Replication" — paired McNemar on exact match, per seed:
# (label, wins for the learned rules, losses, discordant pairs between two
# passes over IDENTICAL memory).
SEEDS = [("seed 1", 21, 0, 1), ("seed 2", 32, 0, 1), ("seed 3", 30, 0, 0)]

FOOTER = ("every point is the same 30 unseen invoices · arm 0 is the same memory "
          "with the rules rolled back · data: crates/areev-bench/EXPENSE.md")


def render(theme: str) -> str:
    c = THEMES[theme]
    W, H = 820, 340
    parts = [
        f'<svg xmlns="http://www.w3.org/2000/svg" width="{W}" height="{H}" '
        f'viewBox="0 0 {W} {H}" role="img" aria-label="Governed self-improvement '
        'on a real expense workflow: held-out exact-match rises from 0 to 38 of '
        '210 trials within the first ten corrections then plateaus, and the '
        'effect replicates across three seeds at 21 to 32 wins with no losses '
        'against a noise floor of 0 to 1">',
        '<style>'
        'text{font-family:ui-sans-serif,-apple-system,"Segoe UI",Roboto,Helvetica,Arial,sans-serif}'
        '.n{font-weight:700;font-variant-numeric:tabular-nums}'
        '.v{font-weight:600;font-variant-numeric:tabular-nums}'
        '</style>',
        f'<rect width="{W}" height="{H}" rx="10" fill="{c["bg"]}" stroke="{c["line"]}"/>',
        f'<text x="32" y="40" class="n" font-size="16" fill="{c["fg"]}">'
        'Areev — a memory the accountant corrects, on a real expense workflow</text>',
        f'<text x="32" y="60" font-size="12" fill="{c["muted"]}">'
        'rules the loop proposed, a human approved, and the agent then followed '
        '· measured on 30 invoices it never saw</text>',
    ]

    # ---- left: the learning curve, task held constant --------------------
    ox, oy = 62, 250          # origin
    pw, ph = 320, 150         # plot area
    max_pct = 30.0

    parts.append(f'<text x="32" y="86" font-size="11" fill="{c["muted"]}" '
                 'font-weight="600">HELD-OUT ACCURACY vs CORRECTIONS SEEN</text>')

    for g in (0, 10, 20, 30):
        gy = oy - ph * g / max_pct
        parts.append(f'<line x1="{ox}" y1="{gy:.1f}" x2="{ox + pw}" y2="{gy:.1f}" '
                     f'stroke="{c["line"]}" stroke-width="1"/>')
        parts.append(f'<text x="{ox - 8}" y="{gy + 4:.1f}" font-size="10" '
                     f'fill="{c["muted"]}" text-anchor="end">{g}%</text>')

    def px(exp):
        return ox + pw * exp / 40.0

    def py(count):
        return oy - ph * (100.0 * count / TRIALS) / max_pct

    for idx, (key, colour, name) in enumerate(
            ((2, c["alt"], "read correctly"), (1, c["bar"], "exactly as filed"))):
        pts = " ".join("%.1f,%.1f" % (px(r[0]), py(r[key])) for r in CURVE)
        parts.append(f'<polyline points="{pts}" fill="none" stroke="{colour}" '
                     'stroke-width="2.5" stroke-linejoin="round"/>')
        for r in CURVE:
            parts.append(f'<circle cx="{px(r[0]):.1f}" cy="{py(r[key]):.1f}" r="3.5" '
                         f'fill="{colour}"/>')
        last = CURVE[-1]
        parts.append(f'<text x="{px(last[0]) + 8:.1f}" y="{py(last[key]) + 4:.1f}" '
                     f'font-size="10.5" fill="{colour}" font-weight="600">{name}</text>')

    # The zero point is worth naming: it is a measured state, not an assumption.
    parts += [
        f'<text x="{px(0):.1f}" y="{oy + 18}" font-size="10" fill="{c["muted"]}" '
        'text-anchor="middle">0</text>',
        f'<text x="{px(0):.1f}" y="{oy + 31}" font-size="9" fill="{c["muted"]}" '
        'text-anchor="middle">rolled back</text>',
    ]
    for e in (10, 20, 30, 40):
        parts.append(f'<text x="{px(e):.1f}" y="{oy + 18}" font-size="10" '
                     f'fill="{c["muted"]}" text-anchor="middle">{e}</text>')
    parts.append(f'<text x="{ox + pw / 2:.1f}" y="{oy + 46}" font-size="10" '
                 f'fill="{c["muted"]}" text-anchor="middle">'
                 'invoices corrected by the accountant</text>')

    # Callout: the step is early, and it then flattens. Say so on the chart.
    parts.append(f'<text x="{px(10) + 6:.1f}" y="{py(38) - 12:.1f}" font-size="10" '
                 f'fill="{c["fg"]}" font-weight="600">most of the gain '
                 'in the first 10</text>')

    # ---- right: replication + the noise floor ----------------------------
    rx = 530
    parts.append(f'<text x="{rx}" y="86" font-size="11" fill="{c["muted"]}" '
                 'font-weight="600">DOES IT REPLICATE?</text>')
    parts.append(f'<text x="{rx}" y="104" font-size="10" fill="{c["muted"]}">'
                 'paired per invoice · only disagreements count</text>')

    row_y, row_h, scale = 122, 46, 4.5
    for i, (label, wins, losses, noise) in enumerate(SEEDS):
        y = row_y + i * row_h
        parts += [
            f'<text x="{rx}" y="{y + 10}" font-size="10.5" fill="{c["fg"]}" '
            f'font-weight="600">{label}</text>',
            f'<rect x="{rx + 52}" y="{y}" width="{wins * scale:.1f}" height="13" '
            f'rx="3" fill="{c["bar"]}"/>',
            f'<text x="{rx + 52 + wins * scale + 6:.1f}" y="{y + 11}" class="v" '
            f'font-size="10.5" fill="{c["fg"]}">{wins}–{losses}</text>',
        ]
        nw = max(2.0, noise * scale)
        parts += [
            f'<rect x="{rx + 52}" y="{y + 16}" width="{nw:.1f}" height="7" rx="2" '
            f'fill="{c["warn"]}"/>',
            f'<text x="{rx + 52 + nw + 6:.1f}" y="{y + 23}" font-size="9.5" '
            f'fill="{c["muted"]}">noise {noise}</text>',
        ]

    parts.append(f'<text x="{rx}" y="{row_y + 3 * row_h + 6}" font-size="10" '
                 f'fill="{c["fg"]}">wins–losses for the learned rules, '
                 'p&lt;0.0001</text>')
    parts.append(f'<text x="{rx}" y="{row_y + 3 * row_h + 21}" font-size="10" '
                 f'fill="{c["muted"]}">noise = two passes over the SAME memory</text>')

    parts += [
        f'<text x="32" y="{H - 18}" font-size="10.5" fill="{c["muted"]}">{FOOTER}</text>',
        '</svg>',
    ]
    return "\n".join(parts)


def main() -> None:
    out = REPO / "docs" / "assets"
    out.mkdir(parents=True, exist_ok=True)
    for theme in THEMES:
        path = out / f"expense-learning-{theme}.svg"
        path.write_text(render(theme), encoding="utf-8")
        print(f"wrote {path.relative_to(REPO)}")


if __name__ == "__main__":
    main()
