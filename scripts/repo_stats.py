#!/usr/bin/env python3
"""Compute repository quality statistics and render them for the README.

Emits four artifacts, all derived from one pass over the tree:

    docs/repo-stats.json            machine-readable, the source of truth
    docs/repo-stats.html            the full report (a CI artifact / Pages page)
    docs/assets/repo-stats-light.svg   the README chart, light theme
    docs/assets/repo-stats-dark.svg    the README chart, dark theme

Run `--check` in CI to fail when the committed artifacts have drifted from the
tree. That is what keeps the numbers in README.md honest: a PR that adds code
without regenerating is a red build, not a quietly stale badge.

Stdlib only, deliberately — invariant 6 (dependency-light) applies to the
tooling as much as to the crates.

Counting rules, chosen so the headline ratio cannot flatter us:

  * Test lines are counted at BLOCK granularity, not file granularity. A
    `src/` file with a 30-line `#[cfg(test)] mod tests` and 400 lines of
    implementation contributes 30 test lines and 400 source lines. (Counting
    whole files as "tests" because they contain a test module inflates the
    ratio by roughly 4x on this tree — the trap this script exists to avoid.)
  * Files under `tests/` and `benches/` are integration tests and benchmark
    harnesses: entirely test lines.
  * Files under `examples/` are neither — they are documentation that
    compiles, and they are reported on their own line.
  * "Code" excludes blank lines and comments. Both the raw physical count and
    the code-only count are reported; the README quotes the code-only one.
"""

from __future__ import annotations

import argparse
import json
import re
import sys
from dataclasses import dataclass, field
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent

# Directories that never contain first-party source.
SKIP_DIRS = {
    "target", "node_modules", ".git", "dist", "build", "__pycache__",
    ".venv", "venv", ".mypy_cache", ".pytest_cache", "npm", "artifacts",
    ".claude", "areev-sandbox",
}

# This script's own outputs. They live under docs/ but are not reference
# documentation, and counting them makes the docs figure both wrong and
# unstable: the run that first creates repo-stats.md reports the count from
# before it existed, and the next run reports one more.
GENERATED = {
    "docs/repo-stats.json",
    "docs/repo-stats.md",
    "docs/repo-stats.html",
    "docs/assets/repo-stats-light.svg",
    "docs/assets/repo-stats-dark.svg",
}

# Coverage is an *input*, not something this script can compute: it needs a full
# instrumented build and the whole suite, which is minutes, not milliseconds.
# `scripts/coverage.py` writes it in the CI job that already pays that cost, and
# this script renders whatever is committed. Absent, the chart simply omits it.
COVERAGE = REPO / "docs" / "coverage.json"


# ---------------------------------------------------------------------------
# Rust scanning
# ---------------------------------------------------------------------------

@dataclass
class Tally:
    files: int = 0
    physical: int = 0   # every line, including blanks and comments
    code: int = 0       # non-blank, non-comment

    def add(self, physical: int, code: int) -> None:
        self.files += 1
        self.physical += physical
        self.code += code


@dataclass
class RustStats:
    source: Tally = field(default_factory=Tally)
    unit_tests: Tally = field(default_factory=Tally)      # #[cfg(test)] blocks in src/
    integration: Tally = field(default_factory=Tally)     # tests/ and benches/
    examples: Tally = field(default_factory=Tally)
    test_fns: int = 0
    files: int = 0
    physical: int = 0


def _mask_rust(text: str) -> str:
    """Blank out comments, string literals and char literals.

    Brace-matching a Rust file naively breaks on a `"}"` in a string or a
    `// }` in a comment. Replacing those spans with spaces (preserving
    newlines, so line numbers survive) makes the subsequent scan reliable.
    """
    out = list(text)
    i, n = 0, len(text)
    while i < n:
        c = text[i]
        # raw string: r"..." / r#"..."#  (any number of hashes)
        if c == "r" and i + 1 < n and text[i + 1] in '"#':
            j = i + 1
            hashes = 0
            while j < n and text[j] == "#":
                hashes += 1
                j += 1
            if j < n and text[j] == '"':
                terminator = '"' + "#" * hashes
                end = text.find(terminator, j + 1)
                end = n if end == -1 else end + len(terminator)
                for k in range(i, end):
                    if out[k] != "\n":
                        out[k] = " "
                i = end
                continue
        if c == "/" and i + 1 < n and text[i + 1] == "/":
            end = text.find("\n", i)
            end = n if end == -1 else end
            for k in range(i, end):
                out[k] = " "
            i = end
            continue
        if c == "/" and i + 1 < n and text[i + 1] == "*":
            depth, j = 1, i + 2          # Rust block comments nest
            while j < n and depth:
                if text.startswith("/*", j):
                    depth += 1
                    j += 2
                elif text.startswith("*/", j):
                    depth -= 1
                    j += 2
                else:
                    j += 1
            for k in range(i, j):
                if out[k] != "\n":
                    out[k] = " "
            i = j
            continue
        if c == '"':
            j = i + 1
            while j < n:
                if text[j] == "\\":
                    j += 2
                    continue
                if text[j] == '"':
                    j += 1
                    break
                j += 1
            for k in range(i, min(j, n)):
                if out[k] != "\n":
                    out[k] = " "
            i = j
            continue
        if c == "'":
            # char literal, but not a lifetime ('a) or a label
            m = re.match(r"'(\\.|[^\\'])'", text[i:])
            if m:
                for k in range(i, i + m.end()):
                    out[k] = " "
                i += m.end()
                continue
        i += 1
    return "".join(out)


def _is_comment(line: str) -> bool:
    s = line.strip()
    return s.startswith("//") or s.startswith("/*") or s.startswith("*")


def _count(lines: list[str]) -> tuple[int, int]:
    """(physical, code) for a slice of lines."""
    code = sum(1 for ln in lines if ln.strip() and not _is_comment(ln))
    return len(lines), code


def _cfg_test_spans(masked: str) -> list[tuple[int, int]]:
    """Half-open [start, end) 0-based line spans covered by `#[cfg(test)]`.

    Handles both the `mod tests { ... }` form and `#[cfg(test)]` applied to a
    single item. Walks from the attribute to the first `{` and brace-matches.
    """
    spans: list[tuple[int, int]] = []
    line_starts = [0]
    for ch_i, ch in enumerate(masked):
        if ch == "\n":
            line_starts.append(ch_i + 1)

    def line_of(offset: int) -> int:
        lo, hi = 0, len(line_starts) - 1
        while lo < hi:
            mid = (lo + hi + 1) // 2
            if line_starts[mid] <= offset:
                lo = mid
            else:
                hi = mid - 1
        return lo

    for m in re.finditer(r"#\s*\[\s*cfg\s*\(\s*test\s*\)\s*\]", masked):
        i = m.end()
        depth = 0
        started = False
        while i < len(masked):
            ch = masked[i]
            if ch == "{":
                depth += 1
                started = True
            elif ch == "}":
                depth -= 1
                if started and depth == 0:
                    i += 1
                    break
            elif ch == ";" and not started:
                # e.g. `#[cfg(test)] use foo::bar;` — a one-liner, no block
                i += 1
                break
            i += 1
        spans.append((line_of(m.start()), line_of(min(i, len(masked) - 1)) + 1))

    # merge overlaps so nested/adjacent attributes are not double counted
    spans.sort()
    merged: list[tuple[int, int]] = []
    for s, e in spans:
        if merged and s <= merged[-1][1]:
            merged[-1] = (merged[-1][0], max(merged[-1][1], e))
        else:
            merged.append((s, e))
    return merged


TEST_FN_RE = re.compile(r"#\s*\[\s*(?:tokio::|async_std::|rstest|test_case)?\s*test\b")


def scan_rust(paths: list[Path]) -> RustStats:
    st = RustStats()
    for path in paths:
        text = path.read_text(encoding="utf-8", errors="replace")
        lines = text.splitlines()
        masked = _mask_rust(text)
        st.files += 1
        st.physical += len(lines)
        st.test_fns += len(TEST_FN_RE.findall(masked))

        parts = path.parts
        if "examples" in parts:
            st.examples.add(*_count(lines))
            continue
        if "tests" in parts or "benches" in parts:
            st.integration.add(*_count(lines))
            continue

        spans = _cfg_test_spans(masked)
        in_test = [False] * len(lines)
        for s, e in spans:
            for i in range(max(0, s), min(e, len(lines))):
                in_test[i] = True
        src_lines = [ln for i, ln in enumerate(lines) if not in_test[i]]
        tst_lines = [ln for i, ln in enumerate(lines) if in_test[i]]
        st.source.add(*_count(src_lines))
        if tst_lines:
            st.unit_tests.files += 1
            p, c = _count(tst_lines)
            st.unit_tests.physical += p
            st.unit_tests.code += c
    return st


# ---------------------------------------------------------------------------
# Repository-wide collection
# ---------------------------------------------------------------------------

def walk(root: Path, suffixes: set[str]) -> list[Path]:
    found = []
    for p in root.rglob("*"):
        if not p.is_file() or p.suffix not in suffixes:
            continue
        if any(part in SKIP_DIRS for part in p.relative_to(root).parts):
            continue
        found.append(p)
    return sorted(found)


def workspace_version() -> str:
    txt = (REPO / "Cargo.toml").read_text(encoding="utf-8")
    block = re.search(r"\[workspace\.package\](.*?)(?=\n\[)", txt, re.S)
    m = re.search(r'^\s*version\s*=\s*"([^"]+)"', block.group(1) if block else txt, re.M)
    return m.group(1) if m else "unknown"


def collect() -> dict:
    rust_paths = walk(REPO / "crates", {".rs"}) + walk(REPO / "fuzz", {".rs"})
    rust = scan_rust(rust_paths)

    crates = sorted(
        p.parent.name for p in (REPO / "crates").glob("*/Cargo.toml")
    )

    # Per-crate lines, for the report table.
    per_crate = []
    for name in crates:
        cp = REPO / "crates" / name
        files = walk(cp, {".rs"})
        s = scan_rust(files)
        per_crate.append({
            "name": name,
            "files": s.files,
            "physical": s.physical,
            "source_code": s.source.code,
            "test_code": s.unit_tests.code + s.integration.code,
            "tests": s.test_fns,
        })
    per_crate.sort(key=lambda c: c["physical"], reverse=True)

    docs = [
        p for p in walk(REPO / "docs", {".md"}) + [
            q for q in REPO.glob("*.md")
            if not any(part in SKIP_DIRS for part in q.parts)
        ]
        if p.relative_to(REPO).as_posix() not in GENERATED
    ]
    doc_lines = sum(
        len(p.read_text(encoding="utf-8", errors="replace").splitlines()) for p in docs
    )

    err_path = REPO / "ERROR_CODES.md"
    error_codes = 0
    if err_path.exists():
        error_codes = len(set(re.findall(
            r"\b[A-Z]{3}-E\d{3}\b", err_path.read_text(encoding="utf-8")
        )))

    py = walk(REPO / "adapters", {".py"}) + walk(REPO / "crates", {".py"})
    ts = walk(REPO / "crates" / "areev-js", {".mjs", ".ts"})

    coverage = None
    if COVERAGE.exists():
        try:
            coverage = json.loads(COVERAGE.read_text(encoding="utf-8"))
        except json.JSONDecodeError:
            coverage = None

    test_code = rust.unit_tests.code + rust.integration.code
    source_code = rust.source.code
    total_code = test_code + source_code

    return {
        "version": workspace_version(),
        "crates": len(crates),
        "rust": {
            "files": rust.files,
            "physical_lines": rust.physical,
            "source_code": source_code,
            "test_code": test_code,
            "unit_test_code": rust.unit_tests.code,
            "integration_test_code": rust.integration.code,
            "example_code": rust.examples.code,
            "test_functions": rust.test_fns,
            "test_ratio": round(test_code / source_code, 2) if source_code else 0,
            "test_share": round(100 * test_code / total_code, 1) if total_code else 0,
        },
        "per_crate": per_crate,
        "docs": {"files": len(docs), "lines": doc_lines},
        "error_codes": error_codes,
        "bindings": {"python_files": len(py), "node_test_files": len(ts)},
        "coverage": coverage,
    }


# ---------------------------------------------------------------------------
# Rendering
# ---------------------------------------------------------------------------

# One palette, two themes. Kept in sync between the SVG and the HTML report.
THEMES = {
    "light": {
        "bg": "#ffffff", "panel": "#f6f7f9", "line": "#dfe3e8",
        "fg": "#111418", "muted": "#5b6470",
        "test": "#1f6feb", "source": "#8250df", "accent": "#0a7d55",
        "on_bar": "#ffffff",
    },
    "dark": {
        "bg": "#0d1117", "panel": "#161b22", "line": "#30363d",
        "fg": "#e6edf3", "muted": "#8b949e",
        "test": "#58a6ff", "source": "#bc8cff", "accent": "#3fb950",
        "on_bar": "#0d1117",
    },
}


def _fmt(n: int) -> str:
    return f"{n:,}"


def render_svg(d: dict, theme: str) -> str:
    c = THEMES[theme]
    r = d["rust"]
    test, src = r["test_code"], r["source_code"]
    total = test + src or 1

    cov = d.get("coverage")

    W, H = (820, 268) if cov else (760, 268)
    bar_x, bar_y, bar_w, bar_h = 32, 150, W - 64, 34
    test_w = round(bar_w * test / total)

    tiles = [
        (_fmt(r["source_code"]), "source code", c["source"]),
        (_fmt(r["test_code"]), "test code", c["test"]),
        (_fmt(r["test_functions"]), "tests", c["accent"]),
    ]
    if cov:
        tiles.append((f'{cov["line_coverage"]}%', "line coverage", c["accent"]))
    tiles.append((_fmt(d["error_codes"]), "error codes", c["fg"]))
    tile_w = (W - 64 - (len(tiles) - 1) * 12) / len(tiles)

    parts = [
        f'<svg xmlns="http://www.w3.org/2000/svg" width="{W}" height="{H}" '
        f'viewBox="0 0 {W} {H}" role="img" '
        f'aria-label="Areev repository quality: {_fmt(r["test_code"])} lines of test code '
        f'against {_fmt(r["source_code"])} lines of source, {_fmt(r["test_functions"])} tests'
        + (f', {cov["line_coverage"]}% line coverage' if cov else '') + '">',
        '<style>'
        'text{font-family:ui-sans-serif,-apple-system,"Segoe UI",Roboto,Helvetica,Arial,sans-serif}'
        '.n{font-weight:700;font-variant-numeric:tabular-nums}'
        '.l{font-size:11px;letter-spacing:.06em;text-transform:uppercase}'
        '</style>',
        f'<rect width="{W}" height="{H}" rx="10" fill="{c["bg"]}" stroke="{c["line"]}"/>',
        f'<text x="32" y="42" class="n" font-size="16" fill="{c["fg"]}">Areev — repository quality</text>',
        f'<text x="32" y="62" font-size="12" fill="{c["muted"]}">'
        f'v{d["version"]} · {d["crates"]} crates · {_fmt(r["files"])} Rust files</text>',
    ]

    for i, (value, label, colour) in enumerate(tiles):
        x = 32 + i * (tile_w + 12)
        parts += [
            f'<rect x="{x:.1f}" y="80" width="{tile_w:.1f}" height="52" rx="7" '
            f'fill="{c["panel"]}" stroke="{c["line"]}"/>',
            f'<text x="{x + 12:.1f}" y="106" class="n" font-size="19" fill="{colour}">{value}</text>',
            f'<text x="{x + 12:.1f}" y="122" class="l" fill="{c["muted"]}">{label}</text>',
        ]

    parts += [
        f'<rect x="{bar_x}" y="{bar_y}" width="{bar_w}" height="{bar_h}" rx="6" fill="{c["source"]}"/>',
        f'<path d="M{bar_x} {bar_y + 6}a6 6 0 0 1 6-6h{test_w - 6}v{bar_h}h{-(test_w - 6)}'
        f'a6 6 0 0 1-6-6z" fill="{c["test"]}"/>',
        f'<text x="{bar_x + 12}" y="{bar_y + 22}" class="n" font-size="12" fill="{c["on_bar"]}">'
        f'tests {r["test_share"]}%</text>',
        f'<text x="{bar_x + bar_w - 12}" y="{bar_y + 22}" class="n" font-size="12" '
        f'fill="{c["on_bar"]}" text-anchor="end">source {round(100 - r["test_share"], 1)}%</text>',
        f'<text x="32" y="212" font-size="12" fill="{c["muted"]}">'
        f'{_fmt(r["integration_test_code"])} integration · {_fmt(r["unit_test_code"])} unit test lines · '
        f'{d["docs"]["files"]} reference docs · {d["crates"]} crates</text>',
        f'<text x="32" y="238" font-size="11" fill="{c["muted"]}">'
        + ('Lines exclude blanks and comments; test code counted per block, not per file. '
           'Coverage scores source lines only, floored per crate.'
           if cov else
           'Lines exclude blanks and comments. Test code is counted per block, not per file.')
        + '</text>',
        '</svg>',
    ]
    return "\n".join(parts) + "\n"


def render_md(d: dict) -> str:
    """A GitHub-renderable version of the report.

    The HTML report is the richer artifact, but a `.html` file linked from
    README.md renders as source on GitHub. This is what the README links to.
    """
    r = d["rust"]
    rows = "\n".join(
        f"| `{c['name']}` | {_fmt(c['files'])} | {_fmt(c['source_code'])} "
        f"| {_fmt(c['test_code'])} | {_fmt(c['tests'])} |"
        for c in d["per_crate"]
    )
    cov = d.get("coverage")
    cov_row = (
        f"| Line coverage | **{cov['line_coverage']}%** of "
        f"{_fmt(cov['lines_total'])} instrumented source lines |\n"
        if cov else ""
    )
    cov_rows = "\n".join(
        f"| `{c['name']}` | {c['line_coverage']}% | "
        f"{_fmt(c['lines_covered'])} / {_fmt(c['lines_total'])} | "
        f"{c['floor'] if c['floor'] is not None else '—'} |"
        for c in cov["per_crate"]
    ) if cov else ""
    cov_excluded = "\n".join(
        f"- `{e['path']}` — {e['reason']}" for e in cov["excluded_from_scope"]
    ) if cov else ""
    cov_method = (
        f"""
## Coverage

**{cov['line_coverage']}%** of {_fmt(cov['lines_total'])} instrumented source
lines, measured by `cargo llvm-cov --workspace` in CI and committed as
`docs/coverage.json` for this script to render — it needs a full instrumented
build and the whole suite, which is minutes rather than milliseconds.

| Crate | Coverage | Lines | Floor |
|---|---:|---:|---:|
{cov_rows}

Enforcement is **per crate, not one workspace target**: a single number lets a
regression in one crate hide behind a gain in another, and these crates do not
carry the same risk. Each floor sits a couple of points under what the crate
measures today — a ratchet against regression, not a goal. `areev-cli` and
`areev-mcp` are deliberately the lowest: they are the user-facing surfaces, and
the next testing work belongs there. A global floor of
{cov['global_floor']}% backs the whole scored set.

### What is not scored, and why

{cov_excluded}

Test code is excluded too: files under `tests/` and `benches/` outright, and
`#[cfg(test)]` blocks line-by-line, because a test body is executed by
definition and counting it scores the suite against itself.

Both filters are visible in the numbers rather than hidden. On the same scope
with test code counted back in, the trace reads
**{cov['comparisons']['same_scope_with_test_code']}%**; unfiltered — every
instrumented line, which is what a naive `cargo llvm-cov` summary prints — it
reads **{cov['comparisons']['whole_unfiltered_trace']}%**. The published figure
is the lowest of the three.

Regenerate with `python3 scripts/coverage.py --lcov lcov.info`.
"""
        if cov else ""
    )
    return f"""# Areev — repository statistics

<!-- GENERATED by scripts/repo_stats.py — do not edit by hand. -->

v{d['version']} · {d['crates']} crates · {_fmt(r['files'])} Rust files

| | |
|---|---|
| Source code | **{_fmt(r['source_code'])}** lines |
| Test code | **{_fmt(r['test_code'])}** lines ({r['test_share']}% of all code) |
| Test functions | **{_fmt(r['test_functions'])}** |
{cov_row}| Stable error codes | **{_fmt(d['error_codes'])}** |
| Reference docs | {_fmt(d['docs']['files'])} files, {_fmt(d['docs']['lines'])} lines |

## By crate

| Crate | Files | Source | Test | Tests |
|---|---:|---:|---:|---:|
{rows}

## Composition

| | |
|---|---:|
| Physical lines (all Rust) | {_fmt(r['physical_lines'])} |
| Source code (no blanks or comments) | {_fmt(r['source_code'])} |
| Unit test code (`#[cfg(test)]` blocks) | {_fmt(r['unit_test_code'])} |
| Integration test code (`tests/`, `benches/`) | {_fmt(r['integration_test_code'])} |
| Example code (`examples/`) | {_fmt(r['example_code'])} |
| Test functions | {_fmt(r['test_functions'])} |
{cov_method}
## Method

Test code is measured **per block, not per file**: a source file containing a
`#[cfg(test)] mod tests` contributes its implementation to source and only the
module body to tests. Counting whole files as tests because they contain a test
module inflates the ratio by roughly 4x on this tree.

Files under `tests/` and `benches/` are entirely test code. Files under
`examples/` are counted separately — they are documentation that compiles.
Blank and comment lines are excluded throughout.

Regenerate with `python3 scripts/repo_stats.py`. CI runs `--check` and fails
when these numbers drift from the tree by more than 2%.
"""


def render_html(d: dict) -> str:
    r = d["rust"]
    cov = d.get("coverage")

    # Per-crate coverage, keyed for the by-crate table. Crates outside the
    # scored set get their exclusion reason rather than a blank cell — "not
    # scored" and "0%" must never look the same.
    cov_by_crate = {c["name"]: c for c in cov["per_crate"]} if cov else {}
    excluded_reason = {}
    if cov:
        for e in cov["excluded_from_scope"]:
            name = e["path"].split("/")[1] if e["path"].startswith("crates/") else e["path"]
            excluded_reason.setdefault(name, e["reason"])

    def cov_cells(name: str) -> str:
        """Two cells: a coverage meter + the floor it is held to."""
        c = cov_by_crate.get(name)
        if not c:
            why = excluded_reason.get(name, "")
            label = "not scored"
            title = f' title="{why}"' if why else ""
            return f'<td class="muted"{title}>{label}</td><td class="muted">—</td>'
        pct, floor = c["line_coverage"], c["floor"]
        # Green with room, amber inside two points of the floor, red below it.
        tone = "bad" if floor is not None and pct < floor else (
            "warn" if floor is not None and pct - floor < 2 else "good"
        )
        floor_cell = f"{floor:g}%" if floor is not None else "—"
        return (
            f'<td class="cov"><span class="meter"><i class="{tone}" '
            f'style="width:{pct:.0f}%"></i></span>{pct}%</td>'
            f'<td class="muted">{floor_cell}</td>'
        )

    rows = "\n".join(
        f"      <tr><td><code>{c['name']}</code></td><td>{_fmt(c['files'])}</td>"
        f"<td>{_fmt(c['source_code'])}</td><td>{_fmt(c['test_code'])}</td>"
        f"<td>{_fmt(c['tests'])}</td>{cov_cells(c['name'])}</tr>"
        for c in d["per_crate"]
    )

    cov_section = ""
    if cov:
        cmp_ = cov["comparisons"]
        excl = "\n".join(
            f"        <li><code>{e['path']}</code> — {e['reason']}</li>"
            for e in cov["excluded_from_scope"]
        )
        cov_section = f"""
  <h2>Coverage</h2>
  <p><strong>{cov['line_coverage']}%</strong> of {_fmt(cov['lines_total'])}
    instrumented source lines, from <code>{cov['measured_by']}</code>. Enforced
    <strong>per crate</strong> — the Floor column above — plus a global floor of
    {cov['global_floor']:g}%. One workspace-wide number would let a regression in
    one crate hide behind a gain in another, and these crates do not carry the
    same risk.</p>
  <p>Both filters are visible rather than hidden. On the same scope with test
    code counted back in the trace reads <strong>{cmp_['same_scope_with_test_code']}%</strong>;
    unfiltered — every instrumented line, which is what a naive
    <code>cargo llvm-cov</code> summary prints — it reads
    <strong>{cmp_['whole_unfiltered_trace']}%</strong>. The published figure is the
    lowest of the three.</p>
  <p>Test code is not scored against itself: <code>tests/</code> and
    <code>benches/</code> are excluded outright and <code>#[cfg(test)]</code>
    blocks line-by-line. Out of scope entirely, because this job cannot execute
    them:</p>
  <ul class="note">
{excl}
  </ul>
"""

    tiles = "\n".join(
        f'      <div class="tile"><div class="v" style="color:{col}">{v}</div>'
        f'<div class="l">{lab}</div></div>'
        for v, lab, col in [
            (_fmt(r["test_code"]), "lines of test code", "var(--test)"),
            (_fmt(r["source_code"]), "lines of source code", "var(--source)"),
            (_fmt(r["test_functions"]), "test functions", "var(--accent)"),
        ] + ([
            (f"{cov['line_coverage']}%", "line coverage (source only)", "var(--accent)"),
        ] if cov else []) + [
            (f"{r['test_ratio']}:1", "test to source", "var(--fg)"),
            (_fmt(d["crates"]), "workspace crates", "var(--fg)"),
            (_fmt(d["error_codes"]), "stable error codes", "var(--fg)"),
        ]
    )
    return f"""<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1">
<title>Areev — repository statistics</title>
<style>
  :root {{
    --bg:{THEMES['light']['bg']}; --panel:{THEMES['light']['panel']};
    --line:{THEMES['light']['line']}; --fg:{THEMES['light']['fg']};
    --muted:{THEMES['light']['muted']}; --test:{THEMES['light']['test']};
    --source:{THEMES['light']['source']}; --accent:{THEMES['light']['accent']};
  }}
  @media (prefers-color-scheme: dark) {{
    :root:not([data-theme="light"]) {{
      --bg:{THEMES['dark']['bg']}; --panel:{THEMES['dark']['panel']};
      --line:{THEMES['dark']['line']}; --fg:{THEMES['dark']['fg']};
      --muted:{THEMES['dark']['muted']}; --test:{THEMES['dark']['test']};
      --source:{THEMES['dark']['source']}; --accent:{THEMES['dark']['accent']};
    }}
  }}
  :root[data-theme="dark"] {{
    --bg:{THEMES['dark']['bg']}; --panel:{THEMES['dark']['panel']};
    --line:{THEMES['dark']['line']}; --fg:{THEMES['dark']['fg']};
    --muted:{THEMES['dark']['muted']}; --test:{THEMES['dark']['test']};
    --source:{THEMES['dark']['source']}; --accent:{THEMES['dark']['accent']};
  }}
  *{{box-sizing:border-box}}
  body{{margin:0;padding:40px 24px;background:var(--bg);color:var(--fg);
    font:15px/1.6 ui-sans-serif,-apple-system,"Segoe UI",Roboto,Helvetica,Arial,sans-serif}}
  main{{max-width:920px;margin:0 auto}}
  h1{{font-size:26px;margin:0 0 4px}}
  .sub{{color:var(--muted);margin:0 0 28px}}
  .tiles{{display:grid;grid-template-columns:repeat(auto-fit,minmax(150px,1fr));gap:12px;margin-bottom:28px}}
  .tile{{background:var(--panel);border:1px solid var(--line);border-radius:8px;padding:14px 16px}}
  .v{{font-size:24px;font-weight:700;font-variant-numeric:tabular-nums}}
  .l{{font-size:11px;letter-spacing:.06em;text-transform:uppercase;color:var(--muted);margin-top:2px}}
  h2{{font-size:16px;margin:32px 0 12px;padding-bottom:8px;border-bottom:1px solid var(--line)}}
  .wrap{{overflow-x:auto}}
  table{{border-collapse:collapse;width:100%;font-variant-numeric:tabular-nums}}
  th,td{{text-align:right;padding:7px 10px;border-bottom:1px solid var(--line);white-space:nowrap}}
  th:first-child,td:first-child{{text-align:left}}
  th{{font-size:11px;letter-spacing:.06em;text-transform:uppercase;color:var(--muted)}}
  code{{font-family:ui-monospace,SFMono-Regular,Menlo,monospace;font-size:13px}}
  td.cov{{white-space:nowrap}}
  .muted{{color:var(--muted)}}
  .meter{{display:inline-block;width:64px;height:6px;border-radius:3px;
    background:var(--line);margin-right:8px;vertical-align:middle;overflow:hidden}}
  .meter i{{display:block;height:100%;border-radius:3px}}
  .meter i.good{{background:var(--accent)}}
  .meter i.warn{{background:#bf8700}}
  .meter i.bad{{background:#d1242f}}
  ul.note li{{margin-bottom:6px}}
  .note{{color:var(--muted);font-size:13px;margin-top:28px;padding-top:16px;border-top:1px solid var(--line)}}
</style>
</head>
<body>
<main>
  <h1>Areev — repository statistics</h1>
  <p class="sub">v{d['version']} · generated from the tree by
    <code>scripts/repo_stats.py</code></p>

  <div class="tiles">
{tiles}
  </div>

  <h2>By crate</h2>
  <div class="wrap">
    <table>
      <thead><tr><th>Crate</th><th>Files</th><th>Source</th><th>Test</th><th>Tests</th>
        <th>Coverage</th><th>Floor</th></tr></thead>
      <tbody>
{rows}
      </tbody>
    </table>
  </div>
{cov_section}

  <h2>Composition</h2>
  <div class="wrap">
    <table>
      <tbody>
        <tr><td>Rust files</td><td>{_fmt(r['files'])}</td></tr>
        <tr><td>Physical lines (all Rust)</td><td>{_fmt(r['physical_lines'])}</td></tr>
        <tr><td>Source code (no blanks or comments)</td><td>{_fmt(r['source_code'])}</td></tr>
        <tr><td>Unit test code (<code>#[cfg(test)]</code> blocks)</td><td>{_fmt(r['unit_test_code'])}</td></tr>
        <tr><td>Integration test code (<code>tests/</code>, <code>benches/</code>)</td><td>{_fmt(r['integration_test_code'])}</td></tr>
        <tr><td>Example code (<code>examples/</code>)</td><td>{_fmt(r['example_code'])}</td></tr>
        <tr><td>Test functions</td><td>{_fmt(r['test_functions'])}</td></tr>
        <tr><td>Reference docs</td><td>{_fmt(d['docs']['files'])} files, {_fmt(d['docs']['lines'])} lines</td></tr>
        <tr><td>Stable error codes</td><td>{_fmt(d['error_codes'])}</td></tr>
      </tbody>
    </table>
  </div>

  <p class="note">
    Test code is measured per block, not per file: a source file containing a
    <code>#[cfg(test)]</code> module contributes its implementation to source
    and only the module body to tests. Blank and comment lines are excluded
    throughout. Regenerate with <code>python3 scripts/repo_stats.py</code>;
    CI runs <code>--check</code> and fails when these artifacts drift from the
    tree.
  </p>
</main>
</body>
</html>
"""


# ---------------------------------------------------------------------------

OUTPUTS = {
    "docs/repo-stats.json": lambda d: json.dumps(d, indent=2) + "\n",
    "docs/repo-stats.md": render_md,
    "docs/repo-stats.html": render_html,
    "docs/assets/repo-stats-light.svg": lambda d: render_svg(d, "light"),
    "docs/assets/repo-stats-dark.svg": lambda d: render_svg(d, "dark"),
}


# Headline figures the README quotes. Checked with a tolerance so ordinary
# PRs are not forced to regenerate four artifacts for a ten-line change;
# the gate exists to stop the published numbers going MEANINGFULLY stale,
# not to police churn.
TRACKED = ["source_code", "test_code", "test_functions"]


def check(data: dict, tolerance: float) -> int:
    """Compare committed artifacts against a fresh scan."""
    path = REPO / "docs/repo-stats.json"
    if not path.exists():
        print("::error::docs/repo-stats.json is missing — run "
              "`python3 scripts/repo_stats.py` and commit the result.")
        return 1

    try:
        committed = json.loads(path.read_text(encoding="utf-8"))
    except json.JSONDecodeError as e:
        print(f"::error::docs/repo-stats.json is not valid JSON: {e}")
        return 1

    problems = []

    # Structural facts must match exactly — a new crate or a version bump is
    # never "close enough".
    for key in ("crates", "version", "error_codes"):
        if committed.get(key) != data.get(key):
            problems.append(f"{key}: committed {committed.get(key)!r}, "
                            f"tree has {data.get(key)!r}")

    for key in TRACKED:
        was, now = committed["rust"].get(key, 0), data["rust"][key]
        drift = abs(now - was) / now * 100 if now else 0
        flag = "  <-- out of tolerance" if drift > tolerance else ""
        print(f"  {key:16} committed {was:>7,}  tree {now:>7,}  "
              f"drift {drift:5.2f}%{flag}")
        if drift > tolerance:
            problems.append(
                f"{key}: committed {was:,} vs tree {now:,} ({drift:.2f}% > "
                f"{tolerance}%)")

    for rel in OUTPUTS:
        if not (REPO / rel).exists():
            problems.append(f"{rel} is missing")

    if problems:
        print("\n::error::repo stats in README.md have drifted from the tree")
        for p_ in problems:
            print(f"  - {p_}")
        print("\nRun `python3 scripts/repo_stats.py` and commit the result.")
        return 1

    print(f"\nrepo stats are current (within {tolerance}%)")
    return 0


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--check", action="store_true",
                    help="fail if the committed stats have meaningfully drifted")
    ap.add_argument("--tolerance", type=float, default=2.0, metavar="PCT",
                    help="permitted drift on line counts, percent (default 2)")
    args = ap.parse_args()

    data = collect()

    if args.check:
        return check(data, args.tolerance)

    for rel, render in OUTPUTS.items():
        path = REPO / rel
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(render(data), encoding="utf-8")

    r = data["rust"]

    for rel in OUTPUTS:
        print(f"wrote {rel}")
    print(f"\n{r['test_code']:,} test / {r['source_code']:,} source "
          f"({r['test_ratio']}:1) across {r['test_functions']:,} tests")
    return 0


if __name__ == "__main__":
    sys.exit(main())
