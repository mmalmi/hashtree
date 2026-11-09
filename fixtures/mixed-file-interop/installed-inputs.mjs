import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { readFile, writeFile, realpath, readdir } from 'node:fs/promises';
import { createRequire } from 'node:module';
import { dirname, join } from 'node:path';
const sha = bytes => createHash('sha256').update(bytes).digest('hex');
const root = '/stage/fixture', require = createRequire(`${root}/package.json`);
const manifest = JSON.parse(await readFile('/stage/procedure/manifest.json', 'utf8'));
const expected = manifest.installedPackages;
assert.equal(sha(await readFile(`${root}/package.json`)), manifest.nodeClosure.packageSha256);
assert.equal(sha(await readFile(`${root}/pnpm-lock.yaml`)), manifest.nodeClosure.lockSha256);
const installedLock = `${root}/node_modules/.pnpm/lock.yaml`;
const provenance = JSON.parse(await readFile('/stage/procedure/package-provenance.json', 'utf8'));
assert.equal(sha(await readFile('/stage/procedure/package-provenance.json')), manifest.sdk.provenanceSha256);
const sdkMembers = Object.fromEntries(provenance.members.map(row => [row.path.replace(/^package\//, ''), row.sha256]));
async function find(name, importer = require) {
  for (const directory of importer.resolve.paths(name) ?? []) {
    try { return await realpath(join(directory, name, 'package.json')); } catch {}
  }
  throw new Error(`Missing ${name}`);
}
const packages = [];
for (const row of expected) {
  const path = await find(row.name), bytes = await readFile(path), pkg = JSON.parse(bytes);
  assert.equal(pkg.name, row.name); assert.equal(pkg.version, row.version);
  if (row.manifestSha256) assert.equal(sha(bytes), row.manifestSha256);
  const result = { name: row.name, version: row.version, manifestSha256: sha(bytes) };
  if (row.name === '@fips/file-transfer') {
    const actual = [], directory = dirname(path);
    async function walk(dir, prefix = '') {
      for (const entry of await readdir(dir, { withFileTypes: true })) {
        if (entry.name === 'node_modules') continue;
        const relative = prefix + entry.name; assert(!entry.isSymbolicLink());
        if (entry.isDirectory()) await walk(join(dir, entry.name), relative + '/');
        else { assert(entry.isFile()); actual.push(relative); assert.equal(sha(await readFile(join(dir, entry.name))), sdkMembers[relative]); }
      }
    }
    await walk(directory); assert.deepEqual(actual.sort(), Object.keys(sdkMembers).sort());
    result.members = sdkMembers;
    assert.equal(await find('@fips/tcp', createRequire(path)), await find('@fips/tcp'));
  }
  packages.push(result);
}
assert.equal(await find('@fips/core', createRequire(await find('@fips/transport-websocket'))), await find('@fips/core'));
if (process.argv[2] === 'after') {
  const before = JSON.parse(await readFile('/stage/evidence/installed-inputs.json', 'utf8'));
  assert.deepEqual(packages, before.packages);
  assert.equal(sha(await readFile(installedLock)), before.installedLockSha256);
  await writeFile('/stage/evidence/installed-after.json', JSON.stringify({ passed: true, packagesUnchanged: true,
    installedLockSha256: before.installedLockSha256 }, null, 2) + '\n', { flag: 'wx' });
  process.exit(0);
}
const binary = JSON.parse(await readFile('/stage/evidence/file-binary.json', 'utf8'));
assert.equal(binary.source, manifest.rustSource);
assert.equal(binary.crateSha256, manifest.sdk.rustCrateSha256);
const input = { source: manifest.sdk.source, rustCrateSha256: manifest.sdk.rustCrateSha256,
  vectors: `${root}/file-transfer-v2-vectors.json`, vectorsSha256: manifest.vectorsSha256,
  rustBinary: binary.path, rustBinarySha256: binary.sha256, nodeVersion: manifest.node.version,
  installedLock, installedLockSha256: sha(await readFile(installedLock)),
  candidatePack: `${root}/vendor/fips-file-transfer-0.1.1.tgz`, candidatePackSha256: manifest.sdk.archiveSha256,
  installedPackages: packages, evidenceDirectory: '/stage/evidence/interop' };
const bytes = Buffer.from(JSON.stringify(input, null, 2) + '\n');
await writeFile('/stage/evidence/run-inputs.json', bytes, { flag: 'wx' });
await writeFile('/stage/evidence/installed-inputs.json', JSON.stringify({ passed: true, source: manifest.sdk.source,
  packageSha256: manifest.nodeClosure.packageSha256, sourceLockSha256: manifest.nodeClosure.lockSha256,
  installedLockSha256: input.installedLockSha256, rootSdkProofSha256: manifest.sdk.rootProofSha256,
  candidatePackageSha256: manifest.sdk.archiveSha256, node: process.version, packages,
  sharedCore: true, sharedTcp: true, runInputSha256: sha(bytes) }, null, 2) + '\n', { flag: 'wx' });
