// Prepare the napi platform packages + the thin main package for publishing.
//
// Runs after `napi create-npm-dirs` + `napi artifacts` have populated `npm/<platform>/`.
// It:
//   1. asserts that every target declared in `napi.targets` produced a package
//      directory holding a built `.node` — a declared-but-missing target is a
//      failed release, not a warning,
//   2. stamps every platform package with the main package's release version,
//   3. wires the main package's `optionalDependencies` to all platform packages, and
//   4. prints — one per line on stdout — the platform package dirs to publish.
//
// The packages are back on the SCOPED names (`@areev/areev`,
// `@areev/areev-win32-x64-msvc`, …) as of 1.6.0: npm's similarity filter
// still 403s the unscoped `areev` main package against `argv`
// (`areev-win32-x64-msvc`'s separate SECURITY HOLD was released on
// 2026-08-22, which is not the same block and does not clear it) — a support
// ticket for the unscoped name is open, and CLAUDE.md's Status section is
// the source of truth for where that stands. History that must not repeat:
// releases before 1.2.3 warned past a 403 on one platform name and shipped a
// main package declaring an optionalDependency that did not exist — npm
// silently skips a missing optional dep at install and napi then fails at
// `require()` with its misleading "npm optional dependencies bug" text, so
// Windows was broken by a registry-name problem reported as a build
// problem. That is why this script HARD-FAILS when any declared target has
// no artifact, and why the workflow publishes the main package last.
import { readFileSync, writeFileSync, readdirSync, existsSync } from 'node:fs';

const mainPath = 'package.json';
const main = JSON.parse(readFileSync(mainPath, 'utf8'));
const version = main.version;
const declared = main.napi?.targets ?? [];

const npmDir = 'npm';
const optionalDependencies = {};
const toPublish = [];
const built = [];

for (const entry of existsSync(npmDir) ? readdirSync(npmDir).sort() : []) {
  const pkgPath = `${npmDir}/${entry}/package.json`;
  if (!existsSync(pkgPath)) continue;
  const pkg = JSON.parse(readFileSync(pkgPath, 'utf8'));
  // `napi artifacts` drops the built addon in beside the manifest. An empty
  // dir means the matrix leg produced nothing for this target.
  const addon = readdirSync(`${npmDir}/${entry}`).find((f) => f.endsWith('.node'));
  if (!addon) {
    console.error(`::error::${pkg.name}@${version} has no .node artifact — the ${entry} build leg produced nothing`);
    process.exit(1);
  }
  pkg.version = version;
  writeFileSync(pkgPath, JSON.stringify(pkg, null, 2) + '\n');
  optionalDependencies[pkg.name] = version;
  toPublish.push(`${npmDir}/${entry}`);
  built.push(entry);
}

// Every declared target must have produced a package. Without this the release
// publishes a main package whose optionalDependencies promise a platform that
// was never built — the exact silent-install / loud-require failure above.
if (built.length !== declared.length) {
  console.error(
    `::error::napi.targets declares ${declared.length} targets (${declared.join(', ')}) ` +
      `but only ${built.length} package dirs were built (${built.join(', ') || 'none'}). ` +
      `Fix the build or remove the target from napi.targets — the manifest must not promise a platform that does not ship.`,
  );
  process.exit(1);
}

main.optionalDependencies = optionalDependencies;
writeFileSync(mainPath, JSON.stringify(main, null, 2) + '\n');

process.stdout.write(toPublish.join('\n') + (toPublish.length ? '\n' : ''));
