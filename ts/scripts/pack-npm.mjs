import { execFileSync } from 'node:child_process';
import { createHash } from 'node:crypto';
import { mkdirSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { npmManifest } from './npm-package.mjs';
import { runtimePackageDirs } from './runtime-packages.mjs';

const root = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const output = join(root, 'dist-npm');
rmSync(output, { recursive: true, force: true });
mkdirSync(output, { recursive: true });
execFileSync(process.execPath, [join(root, 'scripts/pack-runtime.mjs'), join(output, 'raw')], {
  cwd: root, stdio: 'inherit',
});

const manifests = runtimePackageDirs.map((directory) => JSON.parse(
  readFileSync(join(root, 'packages', directory, 'package.json'), 'utf8'),
));
const versions = new Map(manifests.map(({ name, version }) => [name, version]));
const inventory = [];
for (const [index, directory] of runtimePackageDirs.entries()) {
  const original = manifests[index];
  const filename = `${original.name.replace('@', '').replace('/', '-')}-${original.version}.tgz`;
  const staging = join(output, 'staging');
  mkdirSync(staging);
  try {
    execFileSync('tar', ['-xzf', join(output, 'raw', filename), '-C', staging]);
    const packageDir = join(staging, 'package');
    const manifest = JSON.parse(readFileSync(join(packageDir, 'package.json'), 'utf8'));
    writeFileSync(join(packageDir, 'package.json'), `${JSON.stringify(npmManifest(manifest, directory, versions), null, 2)}\n`);
    execFileSync('npm', ['pack', '--ignore-scripts', '--json', '--pack-destination', output], {
      cwd: packageDir, stdio: ['ignore', 'pipe', 'inherit'],
    });
    const sha512 = createHash('sha512').update(readFileSync(join(output, filename))).digest('hex');
    inventory.push({ name: original.name, version: original.version, filename, sha512 });
  } finally {
    rmSync(staging, { recursive: true, force: true });
  }
}
rmSync(join(output, 'raw'), { recursive: true, force: true });
writeFileSync(join(output, 'npm-packages.json'), `${JSON.stringify(inventory, null, 2)}\n`);
console.log(`Prepared ${inventory.length} npm archives in dist-npm.`);
