import assert from 'node:assert/strict';
import { execFileSync } from 'node:child_process';
import { createHash } from 'node:crypto';
import { readFileSync } from 'node:fs';
import { basename, dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { isPublished, selectPackages } from './npm-package.mjs';
import { runtimePackageDirs } from './runtime-packages.mjs';

const root = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const args = new Set(process.argv.slice(2));
for (const arg of args) {
  if (!['--plan', '--dry-run', '--prepared'].includes(arg) && !arg.startsWith('--packages=')) {
    throw new Error(`Unknown argument: ${arg}`);
  }
}
const manifests = runtimePackageDirs.map((directory) => JSON.parse(
  readFileSync(join(root, 'packages', directory, 'package.json'), 'utf8'),
));
const selection = [...args].find((arg) => arg.startsWith('--packages='))?.slice('--packages='.length) ?? 'all';
const selected = new Set(selectPackages(manifests, selection).map(({ name }) => name));
if (args.has('--plan')) {
  for (const { name } of manifests) if (selected.has(name)) console.log(name);
  process.exit(0);
}
if (!args.has('--prepared')) {
  execFileSync(process.execPath, [join(root, 'scripts/pack-npm.mjs')], { stdio: 'inherit' });
}
const output = join(root, 'dist-npm');
const inventory = JSON.parse(readFileSync(join(output, 'npm-packages.json'), 'utf8'));
assert.deepEqual(inventory.map(({ name, version }) => ({ name, version })),
  manifests.map(({ name, version }) => ({ name, version })), 'Archives must match this checkout');

// Verify every archive before publishing any package.
for (const { name, version, filename, sha512 } of inventory) {
  if (selected.has(name) && !args.has('--dry-run') && version.includes('-')) {
    throw new Error(`${name}@${version} is a prerelease; this publisher targets latest`);
  }
  assert.equal(filename, basename(filename));
  const archive = join(output, filename);
  assert.equal(createHash('sha512').update(readFileSync(archive)).digest('hex'), sha512, `${name}: archive changed`);
  const packed = JSON.parse(execFileSync('tar', ['-xOf', archive, 'package/package.json'], { encoding: 'utf8' }));
  assert.equal(packed.name, name);
  assert.equal(packed.version, version);
}
for (const { name, version, filename } of inventory) {
  if (!selected.has(name)) continue;
  if (!args.has('--dry-run') && await isPublished(name, version)) {
    console.log(`Already published: ${name}@${version}`);
    continue;
  }
  const flags = args.has('--dry-run') ? ['--dry-run'] : ['--provenance'];
  // OIDC authentication happens during publish; whoami cannot validate it.
  execFileSync('npm', ['publish', join(output, filename), '--access', 'public',
    '--registry', 'https://registry.npmjs.org', '--ignore-scripts', ...flags], {
    cwd: root, stdio: 'inherit',
  });
}
