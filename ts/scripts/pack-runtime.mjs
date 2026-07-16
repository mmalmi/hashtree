import { execFileSync } from 'node:child_process';
import { mkdirSync, rmSync } from 'node:fs';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const root = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const destination = resolve(process.argv[2] ?? join(root, 'dist-runtime'));
const packageDirs = [
  'hashtree',
  'hashtree-index',
  'hashtree-collection',
  'hashtree-dexie',
  'hashtree-git',
  'hashtree-mesh',
  'hashtree-nostr',
  'hashtree-worker',
  'hashtree-fips-transport',
];

rmSync(destination, { recursive: true, force: true });
mkdirSync(destination, { recursive: true });

for (const packageDir of packageDirs) {
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
