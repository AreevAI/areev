#!/usr/bin/env python3
"""Assert that every version the release touches agrees.

The version lives in five places and only one of them is inherited:

    Cargo.toml                     [workspace.package] version   (the source)
    crates/areev-py/pyproject.toml [project] version             maturin reads THIS
    crates/areev-js/package.json   "version"                     npm reads THIS
    crates/areev-js/Cargo.toml     [package] version             detached workspace
    crates/areev-js/index.js       ~54 hardcoded literals        GENERATED

Both drift modes have shipped before, and both are silent:

  * a workspace-only bump leaves pyproject/package.json on the released
    version, so the publish workflows skip-existing and the run goes green
    having shipped nothing;
  * bumping package.json without re-running `napi build` leaves index.js
    asserting the previous version, so any consumer with
    NAPI_RS_ENFORCE_VERSION_CHECK set gets "native binding package version
    mismatch" against a correctly installed package (this is what happened
    to 1.0.3, stuck at 1.0.1).

Run with `--tag vX.Y.Z` in the release workflows to additionally pin the tag
to the tree.

Stdlib only.
"""

from __future__ import annotations

import argparse
import json
import re
import sys
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent


def workspace_version() -> str:
    txt = (REPO / "Cargo.toml").read_text(encoding="utf-8")
    block = re.search(r"\[workspace\.package\](.*?)(?=\n\[)", txt, re.S)
    m = re.search(r'^\s*version\s*=\s*"([^"]+)"',
                  block.group(1) if block else txt, re.M)
    if not m:
        sys.exit("could not read [workspace.package] version from Cargo.toml")
    return m.group(1)


def pyproject_version() -> str:
    txt = (REPO / "crates/areev-py/pyproject.toml").read_text(encoding="utf-8")
    block = re.search(r"\[project\](.*?)(?=\n\[)", txt, re.S)
    m = re.search(r'^\s*version\s*=\s*"([^"]+)"',
                  block.group(1) if block else txt, re.M)
    if not m:
        sys.exit("could not read [project] version from areev-py/pyproject.toml")
    return m.group(1)


def js_package_version() -> str:
    return json.loads(
        (REPO / "crates/areev-js/package.json").read_text(encoding="utf-8")
    )["version"]


def js_cargo_version() -> str:
    txt = (REPO / "crates/areev-js/Cargo.toml").read_text(encoding="utf-8")
    m = re.search(r'^\s*version\s*=\s*"([^"]+)"', txt, re.M)
    if not m:
        sys.exit("could not read version from areev-js/Cargo.toml")
    return m.group(1)


def js_index_versions() -> set[str]:
    """Every version literal napi baked into the generated loader."""
    idx = REPO / "crates/areev-js/index.js"
    if not idx.exists():
        return set()
    txt = idx.read_text(encoding="utf-8")
    return set(re.findall(r"bindingPackageVersion !== '([^']+)'", txt))


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--tag", help="git tag to pin against, e.g. v1.2.3")
    args = ap.parse_args()

    want = workspace_version()
    checks = {
        "Cargo.toml [workspace.package]": want,
        "crates/areev-py/pyproject.toml": pyproject_version(),
        "crates/areev-js/package.json": js_package_version(),
        "crates/areev-js/Cargo.toml": js_cargo_version(),
    }

    problems = []
    for where, got in checks.items():
        if got != want:
            problems.append(f"{where}: {got!r} != workspace {want!r}")

    idx = js_index_versions()
    if idx and idx != {want}:
        problems.append(
            f"crates/areev-js/index.js asserts {sorted(idx)} but the package is "
            f"{want!r} — regenerate it with "
            f"`cd crates/areev-js && npx napi build --platform --release`"
        )

    if args.tag:
        tag = args.tag[1:] if args.tag.startswith("v") else args.tag
        if tag != want:
            problems.append(f"git tag {args.tag!r} does not match workspace {want!r}")

    width = max(len(k) for k in checks)
    for where, got in checks.items():
        mark = "ok " if got == want else "BAD"
        print(f"  [{mark}] {where.ljust(width)}  {got}")
    print(f"  [{'ok ' if not idx or idx == {want} else 'BAD'}] "
          f"{'crates/areev-js/index.js'.ljust(width)}  "
          f"{', '.join(sorted(idx)) if idx else '(absent)'}")
    if args.tag:
        print(f"  tag: {args.tag}")

    if problems:
        print("\n::error::version drift would ship a broken or empty release")
        for p in problems:
            print(f"  - {p}")
        return 1

    print(f"\nall version sites agree on {want}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
