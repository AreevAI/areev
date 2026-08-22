#!/usr/bin/env python3
"""Render the README's recall-latency chart from the published bench numbers.

Emits two artifacts:

    docs/assets/bench-latency-light.svg
    docs/assets/bench-latency-dark.svg

The numbers are QUOTED from crates/areev-bench/RESULTS.md (the sections are
named next to each figure below) — they are measurements, not something a CI
run can recompute, so this script is re-run by hand when RESULTS.md changes.
Stdlib only, same reasoning as repo_stats.py.
"""

from __future__ import annotations

from pathlib import Path

REPO = Path(__file__).resolve().parent.parent

# Same two themes as scripts/repo_stats.py, so the README's charts read as one.
THEMES = {
    "light": {
        "bg": "#ffffff", "panel": "#f6f7f9", "line": "#dfe3e8",
        "fg": "#111418", "muted": "#5b6470", "bar": "#1f6feb",
    },
    "dark": {
        "bg": "#0d1117", "panel": "#161b22", "line": "#30363d",
        "fg": "#e6edf3", "muted": "#8b949e", "bar": "#58a6ff",
    },
}

# p50 microseconds per surface — RESULTS.md §1 (frame chart, Apple M4 Max),
# §5 (Rust API), §6 (edge devices, measured on the hardware itself).
SURFACES = [
    ("entity_latest, in-process", 9),        # §5
    ("structural recall, in-process", 33),   # §1 leg A
    ("50 ms voice frame + live write-back", 79),   # voice_loop gate
    ("MCP stdio (agent host)", 129),         # §1 leg C
    ("localhost HTTP sidecar", 158),         # §1 leg B
    ("in-process on a $35 Raspberry Pi 3 (2016)", 361),  # §6
]

# recall p50 at growing corpus sizes — RESULTS.md §6.
CORPUS = [
    ("Raspberry Pi 3 (2016)", [("500", 348), ("2k", 360), ("8k grains", 361)]),
    ("Intel NUC (2018)", [("500", 29), ("2k", 29), ("8k grains", 30)]),
]

FOOTER = ("for scale: a hosted memory service’s enterprise headline — "
          "“retrieval under 200 ms” = 200,000 µs · data: crates/areev-bench/RESULTS.md")


def render(theme: str) -> str:
    c = THEMES[theme]
    W, H = 820, 330
    max_us = 400.0

    # Left panel: one recall, every surface.
    lx, lw = 250, 300
    top, row_h = 100, 30

    parts = [
        f'<svg xmlns="http://www.w3.org/2000/svg" width="{W}" height="{H}" '
        f'viewBox="0 0 {W} {H}" role="img" aria-label="Areev recall latency, '
        'measured p50: 9 to 158 microseconds across every surface on a laptop, '
        '361 microseconds on a 2016 Raspberry Pi 3, flat from 500 to 8,000 grains">',
        '<style>'
        'text{font-family:ui-sans-serif,-apple-system,"Segoe UI",Roboto,Helvetica,Arial,sans-serif}'
        '.n{font-weight:700;font-variant-numeric:tabular-nums}'
        '.v{font-weight:600;font-variant-numeric:tabular-nums}'
        '</style>',
        f'<rect width="{W}" height="{H}" rx="10" fill="{c["bg"]}" stroke="{c["line"]}"/>',
        f'<text x="32" y="42" class="n" font-size="16" fill="{c["fg"]}">'
        'Areev — recall latency, measured (p50)</text>',
        f'<text x="32" y="62" font-size="12" fill="{c["muted"]}">'
        'in-process, no server in the recall path · Apple M4 Max unless the row '
        'names the device</text>',
        f'<text x="32" y="88" font-size="11" fill="{c["muted"]}" '
        'font-weight="600">ONE RECALL, EVERY SURFACE · µs</text>',
    ]

    for i, (label, us) in enumerate(SURFACES):
        y = top + i * row_h
        bar = max(3.0, lw * us / max_us)
        parts += [
            f'<text x="{lx - 10}" y="{y + 12}" font-size="11.5" fill="{c["fg"]}" '
            f'text-anchor="end">{label}</text>',
            f'<rect x="{lx}" y="{y}" width="{bar:.1f}" height="15" rx="4" '
            f'fill="{c["bar"]}"/>',
            f'<text x="{lx + bar + 8:.1f}" y="{y + 12}" class="v" font-size="11" '
            f'fill="{c["fg"]}">{us} µs</text>',
        ]

    # Right panel: flat as the corpus grows — one mini panel per device,
    # each with its own scale (the message is flatness, not Pi vs NUC).
    rx = 620
    parts.append(f'<text x="{W - 32}" y="88" font-size="11" fill="{c["muted"]}" '
                 'font-weight="600" text-anchor="end">16× THE CORPUS, SAME LATENCY · µs</text>')
    for p, (device, points) in enumerate(CORPUS):
        py = 106 + p * 96
        peak = max(us for _, us in points)
        parts.append(f'<text x="{rx}" y="{py + 8}" font-size="11.5" '
                     f'fill="{c["fg"]}" font-weight="600">{device}</text>')
        col_w, gap = 38, 10
        for j, (grains, us) in enumerate(points):
            h = 36.0 * us / peak
            x = rx + j * (col_w + gap)
            base = py + 68
            parts += [
                f'<rect x="{x}" y="{base - h:.1f}" width="{col_w}" height="{h:.1f}" '
                f'rx="4" fill="{c["bar"]}"/>',
                f'<text x="{x + col_w / 2}" y="{base - h - 5:.1f}" class="v" '
                f'font-size="10" fill="{c["fg"]}" text-anchor="middle">{us}</text>',
                f'<text x="{x + col_w / 2}" y="{base + 13}" font-size="9.5" '
                f'fill="{c["muted"]}" text-anchor="middle">{grains}</text>',
            ]

    parts += [
        f'<text x="32" y="{H - 20}" font-size="11" fill="{c["muted"]}">{FOOTER}</text>',
        '</svg>',
    ]
    return "\n".join(parts) + "\n"


def main() -> None:
    for theme in ("light", "dark"):
        rel = f"docs/assets/bench-latency-{theme}.svg"
        (REPO / rel).write_text(render(theme), encoding="utf-8")
        print(f"wrote {rel}")


if __name__ == "__main__":
    main()
