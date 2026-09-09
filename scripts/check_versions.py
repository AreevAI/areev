#!/usr/bin/env python3
"""Assert that every version the release touches agrees.

The version lives in six places and only one of them is inherited:

    Cargo.toml                     [workspace.package] version   (the source)
    crates/areev-py/pyproject.toml [project] version             maturin reads THIS
    crates/areev-js/package.json   "version"                     npm reads THIS
    crates/areev-js/Cargo.toml     [package] version             detached workspace
    crates/areev-js/index.js       ~54 hardcoded literals        GENERATED
    areev-sandbox/Cargo.toml       [package] version             detached package

The sandbox is checked because it is a security boundary shipped beside the
engine it bounds: a sandbox built from a different tree than the `areev` it
enforces limits for is the pairing the image and the release exist to prevent.

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


def sandbox_cargo_version() -> str:
    txt = (REPO / "areev-sandbox/Cargo.toml").read_text(encoding="utf-8")
    m = re.search(r'^\s*version\s*=\s*"([^"]+)"', txt, re.M)
    if not m:
        sys.exit("could not read version from areev-sandbox/Cargo.toml")
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
    ap.add_argument(
        "--preview",
        action="store_true",
        help="allow the two published sites to carry a -preview.N suffix "
             "(scripts/stamp_preview.py stamps them; the Cargo versions cannot "
             "move, or inter-crate ^X.Y requirements stop matching)",
    )
    args = ap.parse_args()

    want = workspace_version()
    published = {
        "crates/areev-py/pyproject.toml": pyproject_version(),
        "crates/areev-js/package.json": js_package_version(),
    }
    checks = {
        "Cargo.toml [workspace.package]": want,
        **published,
        "crates/areev-js/Cargo.toml": js_cargo_version(),
        "areev-sandbox/Cargo.toml": sandbox_cargo_version(),
    }

    preview = None
    if args.preview:
        suffixes = {v[len(want):] for v in published.values() if v.startswith(f"{want}-preview.")}
        if len(suffixes) == 1 and re.fullmatch(r"-preview\.[0-9]+", next(iter(suffixes))):
            preview = want + next(iter(suffixes))

    problems = []
    for where, got in checks.items():
        expect = preview if preview and where in published else want
        if got != expect:
            problems.append(f"{where}: {got!r} != {expect!r}")
    if args.preview and preview is None:
        problems.append(
            "--preview: both published sites must carry the SAME "
            f"{want}-preview.<N> stamp (run scripts/stamp_preview.py --build N); "
            f"got {sorted(published.values())}"
        )

    idx = set() if preview else js_index_versions()
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
        expect = preview if preview and where in published else want
        mark = "ok " if got == expect else "BAD"
        print(f"  [{mark}] {where.ljust(width)}  {got}")
    print(f"  [{'ok ' if not idx or idx == {want} else 'BAD'}] "
          f"{'crates/areev-js/index.js'.ljust(width)}  "
          f"{', '.join(sorted(idx)) if idx else '(regenerated by the build)' if preview else '(absent)'}")
    if args.tag:
        print(f"  tag: {args.tag}")

    if problems:
        print("\n::error::version drift would ship a broken or empty release")
        for p in problems:
            print(f"  - {p}")
        return 1

    if preview:
        print(f"\npreview {preview} (Cargo sites stay on {want})")
    else:
        print(f"\nall version sites agree on {want}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
