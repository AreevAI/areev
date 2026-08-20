#!/usr/bin/env python3
"""Turn an `llvm-cov` LCOV trace into the coverage figures the README quotes.

    cargo llvm-cov --workspace --lcov --output-path lcov.info
    python3 scripts/coverage.py --lcov lcov.info          # writes docs/coverage.json
    python3 scripts/coverage.py --lcov lcov.info --check   # what CI asserts

`scripts/repo_stats.py` reads `docs/coverage.json` and renders it into the
README chart. The split exists because the two measurements cost different
things: the stats are a sub-second pass over the tree, coverage is a full
instrumented build plus the whole test suite. Coverage is therefore measured in
the one CI job that already pays for it, committed as a small artifact, and
read back by the cheap script.

Stdlib only, deliberately — invariant 6 (dependency-light) applies to the
tooling as much as to the crates.


## What is scored, and why it is not everything

Two filters, both chosen so the headline number cannot flatter us and cannot
quietly punish us for code this command structurally cannot reach:

  * **Test code is not counted as covered code.** Files under `tests/` and
    `benches/` are excluded outright, and `#[cfg(test)]` blocks are excluded
    line-by-line (the span logic is `repo_stats.py`'s, imported rather than
    re-implemented). A test body is executed by definition; counting it scores
    the suite against itself.
  * **Code this job cannot execute is out of scope** — see `SCOPE_EXCLUDED`.
    Scoring a Python binding that only pytest drives, or a Postgres backend
    that needs a live server, measures the harness rather than the tests.
    Every exclusion carries its reason in the output, so the scope is
    auditable rather than convenient.

Three figures are published together so the effect of both filters is visible:
the headline, the same scope with test code counted back in, and the whole
unfiltered trace — the number a naive `cargo llvm-cov` summary would print.


## Floors, per crate — plus one aggregate backstop

Coverage is enforced **per crate**, because one workspace-wide number lets a
regression in the loop engine hide behind a gain in the CAL parser — and those
two crates do not carry the same risk. Each per-crate floor sits a couple of
points under what that crate measures today: a ratchet against regression, not
a target. Raise one when real work lifts the crate past it.

`GLOBAL_FLOOR` is a backstop under the whole scored set, deliberately well
below the current figure. The per-crate floors are the tight gate — they are
where a regression actually gets caught, each within a couple of points of its
crate. The aggregate exists to catch a collapse that somehow slips through all
of them, so it is set with room rather than at the line: a gate that fails on
platform noise gets lowered, and a floor that gets lowered protects nothing.
"""

from __future__ import annotations

import argparse
import json
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

from repo_stats import _cfg_test_spans, _mask_rust  # noqa: E402

REPO = Path(__file__).resolve().parent.parent
OUT = REPO / "docs" / "coverage.json"

# First-party roots. Everything else in the trace is somebody else's code.
SOURCE_ROOTS = ("crates/", "fuzz/")

# Directories that are entirely test code: executed by construction, so
# counting them measures nothing.
TEST_DIRS = ("/tests/", "/benches/")

# Paths `cargo llvm-cov --workspace` compiles but cannot exercise. Each one is
# tested — just not by this command, in this job. Leaving them in the
# denominator would report a number about CI plumbing rather than about tests.
SCOPE_EXCLUDED = (
    ("crates/areev-py/",
     "PyO3 bindings — exercised by pytest in the `python` CI job, which this "
     "trace cannot see"),
    ("crates/areev-bench/",
     "benchmark harnesses — explicit tools you run on purpose "
     "(`--bin honesty_metrics`), the same category as examples/"),
    ("crates/areev-js/",
     "Node (napi) bindings — a STANDALONE package, not a cargo workspace "
     "member, so `--workspace` never builds it and it cannot appear in this "
     "trace at all; the `node` CI job builds and tests it separately"),
    ("crates/areev-store/src/pg.rs",
     "PostgreSQL backend — needs a live DATABASE_URL; the conformance runner "
     "covers it separately. Present in this build only because areev-py sets "
     "`default = [\"postgres\"]` and Cargo unifies features workspace-wide"),
)

# Per-crate regression floors. Set ~2 points below the measured value when
# introduced; raise them as real work lands. `areev-cli` and `areev-mcp` are
# deliberately the lowest — they are the user-facing surfaces and the next
# testing work belongs there (target: 85).
FLOORS = {
    "areev-conformance": 95.0,
    "areev-loop": 90.0,
    "areev-run-core": 89.0,
    "areev-trigger": 89.0,
    "areev-store": 83.0,
    "areev-core": 80.0,
    "areev-run": 79.0,
    "areev-context": 79.0,
    "areev-loop-adapter": 78.0,
    "areev-server": 75.0,
    "areev-llm": 73.0,
    "areev-mcp": 71.0,
    "areev-cal": 70.0,
    "areev-cli": 70.0,
}

# A backstop under the whole scored set, so the aggregate cannot slide far even
# if every crate stays inside its own floor.
#
# Deliberately set with headroom (the tree currently measures ~80%), because
# this gate's job is to catch a collapse, not to police the last tenth of a
# point. The per-crate floors above are the tight ratchet and the place a real
# regression gets caught. An aggregate gate pinned to the current figure would
# fail on ordinary cross-platform noise, and a gate that cries wolf gets
# lowered — at which point it protects nothing. Raise this when the whole set
# has moved up and stayed there.
GLOBAL_FLOOR = 75.0


def _in_scope(rel: str) -> bool:
    return not any(rel == e or rel.startswith(e) for e, _ in SCOPE_EXCLUDED)


def _excluded_lines(path: Path) -> set[int]:
    """1-based line numbers inside `#[cfg(test)]` blocks."""
    try:
        text = path.read_text(encoding="utf-8", errors="replace")
    except OSError:
        return set()
    out: set[int] = set()
    for start, end in _cfg_test_spans(_mask_rust(text)):
        out.update(range(start + 1, end + 1))  # spans are half-open, 0-based
    return out


def _pct(covered: int, total: int) -> float:
    return round(100 * covered / total, 1) if total else 0.0


def parse_lcov(lcov: Path) -> dict:
    """Tally line hits per file, then roll up per crate and overall."""
    per_file: dict[str, list[int]] = {}          # rel -> [total, covered]
    with_tests: dict[str, list[int]] = {}        # same, but cfg(test) counted
    raw_covered = raw_total = 0

    current: str | None = None
    keep = False
    skip_lines: set[int] = set()

    for line in lcov.read_text(encoding="utf-8", errors="replace").splitlines():
        if line.startswith("SF:"):
            raw = line[3:].strip()
            try:
                rel = Path(raw).resolve().relative_to(REPO).as_posix()
            except ValueError:
                rel = raw  # outside the repo — a dependency
            current = rel
            keep = (
                rel.startswith(SOURCE_ROOTS)
                and not any(d in f"/{rel}" for d in TEST_DIRS)
                and _in_scope(rel)
            )
            skip_lines = _excluded_lines(REPO / rel) if keep else set()
            if keep:
                per_file.setdefault(rel, [0, 0])
                with_tests.setdefault(rel, [0, 0])
        elif line.startswith("DA:") and current is not None:
            number, _, hits = line[3:].partition(",")
            try:
                ln, hit = int(number), int(hits.split(",")[0])
            except ValueError:
                continue
            raw_total += 1
            raw_covered += 1 if hit > 0 else 0
            if keep:
                with_tests[current][0] += 1
                with_tests[current][1] += 1 if hit > 0 else 0
                if ln not in skip_lines:
                    per_file[current][0] += 1
                    per_file[current][1] += 1 if hit > 0 else 0
        elif line.startswith("end_of_record"):
            current, keep, skip_lines = None, False, set()

    crates: dict[str, list[int]] = {}
    for rel, (total, covered) in per_file.items():
        if not rel.startswith("crates/"):
            continue
        name = rel.split("/")[1]
        entry = crates.setdefault(name, [0, 0])
        entry[0] += total
        entry[1] += covered

    total = sum(v[0] for v in per_file.values())
    covered = sum(v[1] for v in per_file.values())
    wt_total = sum(v[0] for v in with_tests.values())
    wt_covered = sum(v[1] for v in with_tests.values())

    return {
        "line_coverage": _pct(covered, total),
        "lines_covered": covered,
        "lines_total": total,
        "files": len(per_file),
        "global_floor": GLOBAL_FLOOR,
        "per_crate": [
            {
                "name": name,
                "line_coverage": _pct(c, t),
                "lines_covered": c,
                "lines_total": t,
                "floor": FLOORS.get(name),
            }
            for name, (t, c) in sorted(
                crates.items(), key=lambda kv: kv[1][1] / kv[1][0] if kv[1][0] else 0,
                reverse=True,
            )
        ],
        "comparisons": {
            "same_scope_with_test_code": _pct(wt_covered, wt_total),
            "whole_unfiltered_trace": _pct(raw_covered, raw_total),
        },
        "excluded_from_scope": [
            {"path": path, "reason": reason} for path, reason in SCOPE_EXCLUDED
        ],
        "measured_by": "cargo llvm-cov --workspace --lcov",
        "scope": (
            "Instrumented first-party lines under crates/ and fuzz/, excluding "
            "tests/, benches/, #[cfg(test)] blocks, and the paths in "
            "excluded_from_scope. The denominator is executable lines, so it is "
            "smaller than the source-line count in repo-stats.json."
        ),
    }


def check(fresh: dict, tolerance: float) -> int:
    """Three gates: drift from the committed figure, the global floor, per-crate floors."""
    failures: list[str] = []

    if not OUT.exists():
        print(f"MISSING {OUT.relative_to(REPO)} — run: python3 scripts/coverage.py --lcov lcov.info",
              file=sys.stderr)
        return 1

    committed = json.loads(OUT.read_text(encoding="utf-8"))
    was, now = committed.get("line_coverage", 0.0), fresh["line_coverage"]
    drift = abs(now - was)
    if drift > tolerance:
        failures.append(
            f"DRIFT line_coverage: committed {was}%, measured {now}% "
            f"({drift:.1f} points > {tolerance} tolerance) — regenerate and commit"
        )

    if now < GLOBAL_FLOOR:
        failures.append(f"BELOW GLOBAL FLOOR: {now}% < {GLOBAL_FLOOR}%")

    for crate in fresh["per_crate"]:
        floor = crate["floor"]
        if floor is not None and crate["line_coverage"] < floor:
            failures.append(
                f"BELOW FLOOR: {crate['name']} {crate['line_coverage']}% < {floor}% "
                f"({crate['lines_total'] - crate['lines_covered']:,} lines uncovered)"
            )

    if failures:
        for f in failures:
            print(f, file=sys.stderr)
        print("\n  floors live in scripts/coverage.py (FLOORS). They are a ratchet "
              "against regression — lower one only with a reason.", file=sys.stderr)
        return 1

    print(f"coverage OK — {now}% (committed {was}%, drift {drift:.1f} points); "
          f"{len(fresh['per_crate'])} crates all at or above their floor")
    return 0


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--lcov", default="lcov.info", help="path to the LCOV trace")
    ap.add_argument("--check", action="store_true",
                    help="compare against docs/coverage.json and enforce the floors")
    # An absolute point tolerance, not a ratio: the number is already a
    # percentage, and a platform-gated branch or two moves it by a fraction of
    # a point between the Linux CI runner and a developer's macOS box.
    ap.add_argument("--tolerance", type=float, default=2.0,
                    help="allowed drift in percentage points (default: 2.0)")
    args = ap.parse_args()

    lcov = Path(args.lcov)
    if not lcov.is_absolute():
        lcov = Path.cwd() / lcov
    if not lcov.exists():
        print(f"no LCOV trace at {lcov}\n"
              f"  produce one: cargo llvm-cov --workspace --lcov --output-path lcov.info",
              file=sys.stderr)
        return 1

    fresh = parse_lcov(lcov)
    if args.check:
        return check(fresh, args.tolerance)

    OUT.write_text(json.dumps(fresh, indent=2) + "\n", encoding="utf-8")
    cmp = fresh["comparisons"]
    print(f"wrote {OUT.relative_to(REPO)} — {fresh['line_coverage']}% of "
          f"{fresh['lines_total']:,} instrumented source lines across "
          f"{fresh['files']} files")
    print(f"  same scope counting test code: {cmp['same_scope_with_test_code']}%  ·  "
          f"whole unfiltered trace: {cmp['whole_unfiltered_trace']}%")
    for c in fresh["per_crate"]:
        floor = c["floor"]
        mark = "" if floor is None or c["line_coverage"] >= floor else "  ** BELOW FLOOR **"
        print(f"    {c['name']:22} {c['line_coverage']:5.1f}%"
              f"  (floor {floor if floor is not None else '—'}){mark}")
    print("next: python3 scripts/repo_stats.py   # re-render the README chart")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
