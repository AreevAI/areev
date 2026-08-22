---
name: areev-release
description: Runbook for cutting a Areev release — version bump, changelog, and the correct publish order across crates.io, PyPI, and npm. Use when the user asks to release, publish, tag, or ship a new version of Areev.
---

# Areev release runbook

Areev is a Rust workspace (15 crates) plus Python bindings and a JS binding.
Follow this order; the workspace has internal `path` dependencies, so crates
must publish bottom-up.

## 1. Pre-flight

- Working tree clean; on an up-to-date `main`.
- `python3 scripts/check_versions.py` — every version site agrees (CI runs
  this too, as the `versions` job).
- `python3 scripts/repo_stats.py --check` — the quality figures in README.md
  still match the tree; regenerate and commit if not.
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
  `scripts/check_versions.py` asserts all of this mechanically — run it
  instead of eyeballing the four files. After a release, verify the registries
  actually flipped:
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
- **Regenerate the repo-stats artifacts after the bump** — they embed the
  version string, so the pre-flight `--check` (run before the bump) goes
  stale the moment `Cargo.toml` moves and the `stats` CI job fails the
  release PR on `version: committed '<old>', tree has '<new>'` (1.5.1 hit
  exactly this):
  ```bash
  python3 scripts/repo_stats.py && python3 scripts/repo_stats.py --check
  ```
- Move the `[Unreleased]` section of `CHANGELOG.md` under a new dated version
  heading; add a fresh empty `[Unreleased]`. Keep the compare-link refs at
  the bottom of the file current too — they go stale silently.
- Commit: `Release vX.Y.Z`. **Do not tag here** — step 3 tags the merge
  commit on `main`, not this branch tip, and tagging twice on two different
  commits is how a release ships from the wrong SHA.

**Dry-run now, on this commit — not in pre-flight, before the bump.**
`cargo publish --workspace --dry-run` verifies each crate's package by
resolving its dependencies against a local staging registry keyed on
`(name, version)`. Before this commit, every crate's manifest `version` still
matches what's already on crates.io, so cargo treats local content as
identical to the published copy and links the **stale registry version**
instead of your source — even though the two now disagree. 1.3.1 hit exactly
this: `areev-trigger`'s Cargo.toml still said `1.3.0` when a pre-bump dry-run
ran, so `areev-cli`'s verification build linked the OLD `areev-trigger` and
failed with `unknown field` errors on code that had, in fact, already shipped
— a false failure pointing at nothing wrong. The check only means anything
once the version genuinely diverges from the registry, which starts here:

```bash
cargo publish --workspace --dry-run
```

This is what actually catches the failures that happen for real (a bumped
crate whose internal dependency requirement still permits the old version, a
missing `include`) BEFORE anything is tagged or pushed — the whole reason to
run it at all — and what makes the publish order in step 4 safe to
parallelise. `cargo publish` refuses a dirty tree, which is exactly why this
runs after the commit above rather than before it.

## 3. Tag and publish the GitHub Release FIRST

This is the step that used to come last, and moving it is the single biggest
win in the runbook.

`main` carries a `protect-main` ruleset whose `pull_request` rule refuses a
direct push, so the release commit lands the same way every other commit does.
Note that `gh api repos/AreevAI/areev/branches/main/protection` returns 404
here — rulesets are a different API from classic branch protection, and reading
the 404 as "main is unprotected" is how you find out at `git push` time.

```bash
git switch -c release/vX.Y.Z && git push -u origin release/vX.Y.Z
gh pr create --base main --title "Release vX.Y.Z" --body-file <(...)
# after review + green CI:
gh pr merge --merge --delete-branch
git switch main && git pull

git tag vX.Y.Z && git push origin vX.Y.Z
gh release create vX.Y.Z --notes-file <(sed -n '/## \[X.Y.Z\]/,/## \[/p' CHANGELOG.md)
```

Tag the merge commit on `main`, not the branch tip — the tag must name the
commit the registries build from, and a squash or merge rewrites it.

Publishing the Release fires `release-pypi`, `release-npm` and `release-cli`
**concurrently**, and each of those builds its five-platform matrix in
parallel. None of them read crates.io — they build from the local `path`
dependencies in the checkout — so they have no reason to wait for step 4, and
under the old ordering they sat idle behind it for twenty to forty minutes.

Each workflow now starts with a `preflight` job running
`scripts/check_versions.py --tag vX.Y.Z`, so a tag that disagrees with the
tree fails in seconds instead of after a full matrix build.

`workflow_dispatch` on any of the three is a **safe dry run**: the publish
jobs are guarded on `github.event_name == 'release'`, so a manual run builds
and uploads artifacts but ships nothing.

## 4. Publish crates.io — while step 3 runs

`publish = false` is set on exactly four crates and they should **stay** that
way: `areev-bench` and `areev-conformance` (internal harnesses),
`areev-js` (ships to npm) and `areev-py` (ships to PyPI).

Cargo resolves the dependency order itself — the hand-maintained tier list
this runbook used to carry went stale twice and failed mid-publish:

```bash
cargo publish --workspace          # --dry-run already run in step 2, on this exact commit
```

**Bump the internal dependency requirements too — on a minor/major only.**
Crates declare each other as `version = "1.0.0"`; on a minor/major release
that requirement still permits the OLD version, so cargo can resolve
new-crate + old-dep from a lockfile and fail to compile while the manifest
claims the pair is supported. 1.1.0 hit exactly this (`areev-cal` used
`GrainType::Recommendation`, absent from `areev-core` 1.0.5). A patch release
doesn't have this problem — `^1.3.0` already permits `1.3.1` — which is why
1.3.1 needed no change here; the dry-run in step 2 is what tells you which
case you're in.

If `--workspace` is unavailable or a leg fails partway, fall back to
publishing bottom-up, recomputing the order from the manifests rather than
trusting a written list:

```bash
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

## 5. What the workflows do (no manual step needed)

- **PyPI** (`release-pypi.yml`) — maturin builds abi3 wheels `--locked` for
  linux x86_64/aarch64, macOS x86_64/aarch64 and windows x64, plus an sdist,
  then uploads. One wheel covers CPython >= 3.9.
- **npm** (`release-npm.yml`) — `napi build --locked` per platform; the Linux
  legs build inside `node:20-bullseye` so the addon's glibc floor stays at
  2.31, asserted by a grep over the `.node`. Platform packages publish first,
  the main `@areev/areev` package last, so a half-published release never
  advertises a platform that is not on the registry yet.
- **CLI** (`release-cli.yml`) — five prebuilt `areev` binaries, smoke-tested
  (`--version`, `add`, `recall`) before upload, attached to the Release with
  a combined `SHA256SUMS`.

## 6. Verify

- `npm view @areev/areev version`
- `curl -s https://pypi.org/pypi/areev/json | jq -r .info.version` — **PyPI's
  JSON API is CDN-cached** and can serve the previous version for minutes
  after a successful upload; confirm with
  `curl -o /dev/null -w '%{http_code}' https://pypi.org/pypi/areev/<ver>/json`
  (200 = published) before chasing a phantom failure.
- `cargo search areev`
- A green workflow is not proof the version changed.

## Notes

- Registry names: `areev` on crates.io and PyPI are ours. npm ships
  **`@areev/areev`** — the unscoped `areev` name and `areev-win32-x64-msvc`
  are blocked by npm's similarity/spam filters pending a support exception;
  when granted, publish unscoped, deprecate the scoped one, and flip the
  docs back to `npm install areev`.
- crates.io rate-limits NEW crate names hard (burst ~5, slow refill, 429
  with retry-after); existing-crate version publishes are much milder.
  Retry on 429 AND on upload timeouts — a timed-out upload may still have
  landed ("already exists on index" on retry means it did).
- Keep `rust-version` (MSRV) in `[workspace.package]` accurate — CI has an MSRV job.
- Never reuse or renumber error codes across releases (append-only).
- **The ordering trade-off, stated plainly**: cutting the Release before
  crates.io means a crates.io failure lands *after* npm and PyPI have already
  shipped. That is the right trade because the failure modes are not
  symmetric — the dry-run in step 2 catches essentially every real crates.io
  failure while nothing is tagged yet, whereas the cost of the old ordering
  was paid on every single release. If crates.io does fail, fix forward: the
  registries are independent and a patch release is cheap.
- The three release workflows carry `concurrency` groups keyed on the tag, so
  a re-run cannot race a manual dispatch.
