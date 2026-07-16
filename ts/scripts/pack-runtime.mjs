import { execFileSync } from 'node:child_process';
import { mkdirSync, rmSync } from 'node:fs';
import { dirname, join, posix, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

import { runtimePackageDirs } from './runtime-packages.mjs';

const root = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const destination = resolve(process.argv[2] ?? join(root, 'dist-runtime'));

rmSync(destination, { recursive: true, force: true });
mkdirSync(destination, { recursive: true });

for (const packageDir of runtimePackageDirs) {
  const cwd = join(root, 'packages', packageDir);
  execFileSync('pnpm', ['build'], { cwd, stdio: 'inherit' });
  const packed = JSON.parse(execFileSync(
    'pnpm',
    ['pack', '--json', '--pack-destination', destination],
    { cwd, encoding: 'utf8' },
  ));
  const manifest = JSON.parse(execFileSync(
    'tar',
    ['-xOf', packed.filename, 'package/package.json'],
    { encoding: 'utf8' },
  ));
  verifyPackedMapSources(packed);
  for (const [name, specifier] of Object.entries(manifest.dependencies ?? {})) {
    if (/^(?:file|link|workspace):/.test(specifier)) {
      throw new Error(`${manifest.name} packs a local dependency: ${name}=${specifier}`);
    }
  }
  process.stdout.write(`${JSON.stringify({
    name: manifest.name,
    version: manifest.version,
    filename: packed.filename,
  })}\n`);
}

function verifyPackedMapSources(packed) {
  const paths = new Set(packed.files.map(({ path }) => posix.join('package', path)));
  for (const { path } of packed.files) {
    if (!path.endsWith('.map')) continue;
    const archivePath = posix.join('package', path);
    const sourceMap = JSON.parse(execFileSync(
      'tar',
      ['-xOf', packed.filename, archivePath],
      { encoding: 'utf8' },
    ));
    for (const [index, source] of (sourceMap.sources ?? []).entries()) {
      if (sourceMap.sourcesContent?.[index] != null || /^[a-z]+:/i.test(source)) continue;
      const sourcePath = posix.normalize(posix.join(posix.dirname(archivePath), source));
      if (!paths.has(sourcePath)) {
        throw new Error(`${packed.filename} omits source-map target ${sourcePath}`);
      }
    }
  }
}
