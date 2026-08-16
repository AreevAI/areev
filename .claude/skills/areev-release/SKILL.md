---
name: areev-release
description: Runbook for cutting a Areev release — version bump, changelog, and the correct publish order across crates.io, PyPI, and npm. Use when the user asks to release, publish, tag, or ship a new version of Areev.
---

# Areev release runbook

Areev is a Rust workspace (9 crates) plus Python bindings and a JS binding.
Follow this order; the workspace has internal `path` dependencies, so crates
must publish bottom-up.

## 1. Pre-flight

- Working tree clean; on an up-to-date `main`.
- `cargo test --workspace` green; `cargo clippy --workspace --all-targets -- -D warnings` clean.
- `cargo deny check` passes (advisories, licenses, sources, bans).
- Fuzz smoke: `cargo +nightly fuzz build`.
- Confirm `THIRD-PARTY-NOTICES.md` is current (regenerate with `cargo about` if deps changed).

## 2. Version + changelog

- Bump `version` in `[workspace.package]` in the root `Cargo.toml` (all crates
  inherit it via `version.workspace = true`).
- **Also bump the two bindings' OWN version files** — they do NOT inherit the
  workspace version, and a workspace-only bump makes the npm/PyPI publish
  workflows silently no-op over the already-published version (a "successful"
  run that ships nothing):
  - `crates/areev-js/package.json` → `"version"` (standalone npm package).
  - `crates/areev-py/pyproject.toml` → `[project] version` (maturin reads this,
    NOT Cargo's `version.workspace`).
  After a release, verify the registries actually flipped:
  `npm view areev version`, `curl -s https://pypi.org/pypi/areev/json | jq -r .info.version` —
  a green workflow is not proof the version changed. **PyPI's JSON API is
  CDN-cached and can serve the previous version for minutes after a
  successful upload** — confirm with `curl -o /dev/null -w '%{http_code}'
  https://pypi.org/pypi/areev/<ver>/json` (200 = published) before chasing a
  phantom failure.
- **Regenerate `crates/areev-js/index.js` in the same commit.** It is a
  *generated* file that hard-codes the expected version in every platform arm
  (`bindingPackageVersion !== '<ver>'`). Bumping package.json without
  regenerating leaves it asserting the previous version, so a user with
  `NAPI_RS_ENFORCE_VERSION_CHECK` set gets "native binding package version
  mismatch" against a correctly-installed package. 1.0.3 shipped with it stuck
  at 1.0.1 for exactly this reason.
  ```bash
  (cd crates/areev-js && ./node_modules/.bin/napi build --platform --release)
  git diff crates/areev-js/index.js   # must be version lines ONLY
  ```
- Move the `[Unreleased]` section of `CHANGELOG.md` under a new dated version
  heading; add a fresh empty `[Unreleased]`.
- Commit: `Release vX.Y.Z`. Tag: `git tag vX.Y.Z`.

## 3. Publish crates.io (bottom-up dependency order)

`publish = false` is set on exactly four crates and they should **stay** that
way: `areev-bench` and `areev-conformance` (internal harnesses),
`areev-js` (ships to npm) and `areev-py` (ships to PyPI). Everything else
publishes, in this order — a crate can only publish once its path dependencies
are on crates.io:

```
areev-core, areev-loop          (no internal deps)
  → areev-store             (core)
  → areev-cal               (core, store)
  → areev-context           (cal, core)
  → areev-llm               (areev-loop)
  → areev-loop-adapter            (cal, core, store, areev-loop)
  → areev-mcp, areev-server, areev
```

**Ten publishable crates, not seven** — this list used to omit `areev-loop`,
`areev-llm` and `areev-loop-adapter`, so following it failed at `areev-mcp`
(which needs `areev-loop-adapter`). Recompute rather than trust it:

```bash
# topological order from the manifests
python3 - <<'EOF'
import glob, tomllib
pkgs = {}
for p in glob.glob("crates/*/Cargo.toml"):
    d = tomllib.load(open(p, "rb"))
    if d["package"].get("publish") is False: continue
    pkgs[d["package"]["name"]] = {k for k, v in d.get("dependencies", {}).items()
                                  if isinstance(v, dict) and "path" in v}
done = set()
while len(done) < len(pkgs):
    for n in sorted(pkgs):
        if n not in done and pkgs[n] <= done | (pkgs[n] - pkgs.keys()):
            print(n); done.add(n)
EOF
```

**Bump the internal dependency requirements too.** Crates declare each other
as `version = "1.0.0"`; on a minor/major release that requirement still
permits the OLD version, so cargo can resolve new-crate + old-dep from a
lockfile and fail to compile while the manifest claims the pair is supported.
1.1.0 hit exactly this (`areev-cal` used `GrainType::Recommendation`, absent
from `areev-core` 1.0.5).

```bash
cargo publish -p areev-core
# wait for it to index, then the next tier, etc.
```

`areev-bench` stays unpublished (internal harness). `areev-py` is not published
to crates.io — it ships to PyPI.

## 4. Publish PyPI (areev-py)

Build abi3 wheels with maturin (cibuildwheel or maturin-action in CI for the
full platform matrix), then upload:

```bash
maturin build --release -m crates/areev-py/Cargo.toml
# CI builds linux/macos/windows abi3 wheels; then:
maturin upload target/wheels/*   # or twine upload
```

The package name is `areev` (reserved). Requires-Python `>=3.9` (abi3-py39).

## 5. Publish npm (areev-js, napi)

`areev-js` is a **native Node addon built with napi-rs (not wasm)** and is a
standalone package — it is not a `cargo` workspace member, so it publishes
independently of the crates.io tier. Build the per-platform prebuilds and
publish (name `areev`, reserved):

```bash
# from crates/areev-js — CI builds the platform matrix via `napi build --release`
cd crates/areev-js
npm publish --access public
```

## 6. Post-release

- Push the tag: `git push origin main --tags`.
- Create a GitHub Release from the tag with the changelog section.
- Verify install paths work: `cargo install areev`, `pip install areev`,
  `npx areev` / `npm i areev`.

## Notes

- All three registry names (`areev` on crates.io/PyPI/npm) are reserved.
- Keep `rust-version` (MSRV) in `[workspace.package]` accurate — CI has an MSRV job.
- Never reuse or renumber error codes across releases (append-only).
