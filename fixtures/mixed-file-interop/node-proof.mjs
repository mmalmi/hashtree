import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { readFile, writeFile, copyFile, stat } from 'node:fs/promises';
import { execFileSync } from 'node:child_process';
const sha = bytes => createHash('sha256').update(bytes).digest('hex');
const manifest = JSON.parse(await readFile('/stage/procedure/manifest.json', 'utf8'));
assert.equal(process.version, manifest.node.version);
assert.equal(sha(await readFile(process.execPath)), manifest.node.binarySha256);
assert.equal(process.platform, 'linux'); assert.equal(process.arch, 'x64');
assert.equal(typeof WebSocket, 'function'); assert.equal(typeof crypto.subtle.digest, 'function');
const probe = Buffer.from('pinned-node-on-bookworm');
assert.equal(Buffer.from(await crypto.subtle.digest('SHA-256', probe)).toString('hex'), sha(probe));
const mode = process.argv[2]; assert(['extract', 'derived'].includes(mode));
const linkage = execFileSync('ldd', [process.execPath], { encoding: 'utf8' });
assert(!linkage.includes('not found'));
const dependencies = [];
for (const line of linkage.split('\n')) {
  const match = line.match(/(?:=>\s*)?(\/[^\s]+)\s+\(/);
  if (match) { const path = match[1]; const bytes = await readFile(path); dependencies.push({ path, bytes: bytes.length, sha256: sha(bytes) }); }
}
assert(dependencies.length > 0);
if (mode === 'extract') {
  await copyFile(process.execPath, '/stage/tools/bin/node');
  assert((await stat('/stage/tools/bin/node')).mode & 0o111);
  assert.equal(sha(await readFile('/stage/tools/bin/node')), manifest.node.binarySha256);
}
await writeFile(`/stage/evidence/node-${mode}-proof.json`, JSON.stringify({ passed: true, node: process.version,
  nodeSha256: manifest.node.binarySha256, executable: process.execPath, linkage, dependencies,
  image: mode === 'extract' ? manifest.node.image : 'Recorded task-derived Bookworm image; no dependency byte-equivalence claim',
  webSocket: true, webCrypto: true }, null, 2) + '\n', { flag: 'wx' });
