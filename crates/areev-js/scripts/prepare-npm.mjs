// Prepare the napi platform packages + the thin main package for publishing.
//
// Runs after `napi create-npm-dirs` + `napi artifacts` have populated `npm/<platform>/`.
// It:
//   1. stamps every platform package with the main package's release version,
//   2. wires the main package's `optionalDependencies` to *all* platform packages
//      (including any deferred one, so a later same-version publish resolves for
//      existing installs), and
//   3. prints — one per line on stdout — the platform package dirs that should be
//      published now (everything except the deferred names).
//
// DEFER lists platform package names to skip on this publish run (e.g. a name
// currently tripping npm's spam filter). A deferred package's build still
// runs and its binary is collected, so once the name is unblocked it can be
// published at the same version by removing it from DEFER and re-running
// release-npm (the publish step skips packages already on the registry, so
// only the newly-undeferred one goes out).
//
// `areev-win32-x64-msvc` was deferred here (403 on publish; support ticket
// open) and unblocked 2026-07-17 — npm whitelisted the name.
import { readFileSync, writeFileSync, readdirSync, existsSync } from 'node:fs';

const DEFER = new Set();

const mainPath = 'package.json';
const main = JSON.parse(readFileSync(mainPath, 'utf8'));
const version = main.version;

const npmDir = 'npm';
const optionalDependencies = {};
const toPublish = [];

for (const entry of existsSync(npmDir) ? readdirSync(npmDir).sort() : []) {
  const pkgPath = `${npmDir}/${entry}/package.json`;
  if (!existsSync(pkgPath)) continue;
  const pkg = JSON.parse(readFileSync(pkgPath, 'utf8'));
  pkg.version = version;
  writeFileSync(pkgPath, JSON.stringify(pkg, null, 2) + '\n');
  // Every platform package is referenced by the main package, even a deferred
  // one — npm silently ignores a missing optional dependency at install time and
  // picks it up once it exists.
  optionalDependencies[pkg.name] = version;
  if (DEFER.has(pkg.name)) {
    console.error(`defer ${pkg.name}@${version} (npm name blocked; publish once unblocked)`);
    continue;
  }
  toPublish.push(`${npmDir}/${entry}`);
}

main.optionalDependencies = optionalDependencies;
writeFileSync(mainPath, JSON.stringify(main, null, 2) + '\n');

process.stdout.write(toPublish.join('\n') + (toPublish.length ? '\n' : ''));
