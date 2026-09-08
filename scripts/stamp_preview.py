#!/usr/bin/env python3
"""Stamp a prerelease version on the two files the registries read.

A preview publishes an UNRELEASED tree so a downstream can test a change
without waiting for a release. It must not disturb `latest`, and no branch may
carry a fake version — so the stamp happens in the workflow at build time and
is never committed.

    scripts/stamp_preview.py --build 42   ->  1.7.3-preview.42

Two constraints decide the whole design.

**The scheme.** Only one shape is publishable to both registries:

    1.7.3-preview.42      npm: valid semver, sorts BELOW 1.7.3
                          PyPI: valid PEP 440, normalizes to 1.7.3rc42
    1.7.3-preview.<sha>   npm: valid.  PyPI: INVALID — PEP 440 numbers its
                          pre-releases, and PyPI rejects the `+local`
                          segment a sha would otherwise need.
    1.7.3.dev42, 1.7.3rc0 PyPI: valid.  npm: not semver at all.

So `--build` takes a monotonic integer (`github.run_number`); the commit is
recorded in the publish log, not in the version.

**What may be stamped.** Only `package.json` (npm reads it) and
`pyproject.toml` (maturin reads it). The Cargo versions stay put: crates
depend on each other as `areev-core = { path = "…", version = "1.7.0" }`, and
Cargo does not match a prerelease against `^1.7.0` — stamping the workspace
turns every inter-crate requirement unsatisfiable and nothing builds at all.
Leaving them alone also leaves all three lockfiles valid, which matters
because every release build runs `--locked`.

Pair with `check_versions.py --preview`, which allows exactly this shape.

Stdlib only. Idempotent.
"""

from __future__ import annotations

import argparse
import re
import sys
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent


def base_version() -> str:
    txt = (REPO / "Cargo.toml").read_text(encoding="utf-8")
    block = re.search(r"\[workspace\.package\](.*?)(?=\n\[)", txt, re.S)
    m = re.search(r'^\s*version\s*=\s*"([^"]+)"', block.group(1) if block else txt, re.M)
    if not m:
        sys.exit("could not read [workspace.package] version")
    return m.group(1)


def restamped(path: Path, pattern: str, want: str) -> str:
    txt = path.read_text(encoding="utf-8")
    new = re.sub(pattern, rf'\g<1>"{want}"', txt, count=1, flags=re.M)
    if new == txt:
        sys.exit(f"no version line to stamp in {path}")
    return new


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--build", required=True, help="monotonic integer, e.g. $GITHUB_RUN_NUMBER")
    args = ap.parse_args()

    if not args.build.isdigit():
        sys.exit(
            f"--build must be a whole number — PEP 440 numbers its pre-releases: {args.build!r}"
        )

    want = f"{base_version()}-preview.{int(args.build)}"
    sites = (
        (REPO / "crates/areev-js/package.json", r'("version"\s*:\s*)"[^"]+"'),
        (REPO / "crates/areev-py/pyproject.toml", r'(^\s*version\s*=\s*)"[^"]+"'),
    )
    written = [(path, restamped(path, pattern, want)) for path, pattern in sites]
    for path, text in written:
        path.write_text(text, encoding="utf-8")
    print(want)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
