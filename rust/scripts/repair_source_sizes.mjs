// Metadata-only correction for the explicitly constructed public Git tree.
// Payload CIDs and keys stay unchanged; every original node must round-trip.
import fs from 'node:fs/promises';
import path from 'node:path';
import { pathToFileURL } from 'node:url';
import { createHash } from 'node:crypto';
import assert from 'node:assert/strict';

const [task, rootHash, rootKey] = process.argv.slice(2);
const sdk = path.join(task, 'sdk/node_modules/@hashtree/core/dist');
const { decodeTreeNode, encodeTreeNode } = await import(pathToFileURL(path.join(sdk, 'codec.js')));
const { decryptChk, encryptChk } = await import(pathToFileURL(path.join(sdk, 'crypto.js')));
const hex = (bytes) => Buffer.from(bytes).toString('hex');
const hash = (bytes) => createHash('sha256').update(bytes).digest('hex');
const nodes = [];
const sizeChanges = [];
const roundTrips = [];
let filesChecked = 0;
await fs.mkdir(path.join(task, 'corrected-nodes'));

async function fetchNode(digest) {
  let failure;
  for (const host of ['https://cdn.iris.to', 'https://upload.iris.to']) {
    try {
      const response = await fetch(`${host}/${digest}`, { signal: AbortSignal.timeout(30_000) });
      assert(response.ok, `HTTP ${response.status}`);
      assert(Number(response.headers.get('content-length') ?? 0) <= 1024 * 1024);
      const reader = response.body.getReader();
      const chunks = [];
      let size = 0;
      try {
        for (;;) {
          const { done, value } = await reader.read();
          if (done) break;
          size += value.length;
          assert(size <= 1024 * 1024, 'directory node exceeds budget');
          chunks.push(value);
        }
      } finally {
        await reader.cancel();
      }
      const bytes = Buffer.concat(chunks);
      assert.equal(hash(bytes), digest, 'stored hash mismatch');
      return bytes;
    } catch (error) { failure = error; }
  }
  throw failure;
}

async function correct(relative, digest, key, depth = 0) {
  assert(depth < 16 && roundTrips.length < 64, 'directory traversal budget');
  assert.match(digest, /^[0-9a-f]{64}$/);
  assert.match(key, /^[0-9a-f]{64}$/);
  const bytes = await fetchNode(digest);
  const plain = await decryptChk(bytes, Buffer.from(key, 'hex'));
  const node = decodeTreeNode(plain);
  assert.equal(node.type, 2, 'only direct directory manifests are supported');
  assert(node.links.length <= 174);
  const before = encodeTreeNode(node);
  assert.deepEqual(Buffer.from(before), Buffer.from(plain));
  const round = await encryptChk(before);
  assert.deepEqual(Buffer.from(round.ciphertext), bytes);
  assert.equal(hex(round.key), key);
  roundTrips.push({ path: relative || '/', hash: digest, byteIdentical: true });

  const directory = path.join(task, 'source-tree', relative);
  assert((await fs.lstat(directory)).isDirectory());
  const names = await fs.readdir(directory);
  assert.deepEqual(node.links.map((entry) => entry.name).sort(), names.sort());
  const changes = [];
  for (const entry of node.links) {
    assert(typeof entry.name === 'string' && entry.name !== '.' && entry.name !== '..');
    assert(!entry.name.includes('/') && !entry.name.includes('\0'));
    const childPath = path.posix.join(relative, entry.name);
    const stat = await fs.lstat(path.join(directory, entry.name));
    assert(!stat.isSymbolicLink());
    assert.equal(entry.hash.length, 32);
    assert.equal(entry.key?.length, 32);
    if (stat.isDirectory()) {
      assert.equal(entry.type, 2);
      const child = await correct(childPath, hex(entry.hash), hex(entry.key), depth + 1);
      if (child.hash !== hex(entry.hash)) {
        changes.push({ name: entry.name, hash: entry.hash, key: entry.key });
        entry.hash = Buffer.from(child.hash, 'hex');
        entry.key = Buffer.from(child.key, 'hex');
      }
    } else {
      assert(stat.isFile() && stat.size <= 256 * 1024 * 1024);
      assert(entry.type === 0 || entry.type === 1);
      assert(++filesChecked <= 128);
      if (entry.size !== stat.size) {
        assert.equal(entry.size, 0, 'unexpected preexisting file size');
        changes.push({ name: entry.name, size: entry.size });
        sizeChanges.push({ path: childPath, oldSize: entry.size, size: stat.size,
          payloadCid: hex(entry.hash), type: entry.type });
        entry.size = stat.size;
      }
    }
  }
  if (changes.length === 0) return { hash: digest, key };
  const encoded = encodeTreeNode(node);
  const restored = decodeTreeNode(encoded);
  for (const change of changes) {
    const entry = restored.links.find((item) => item.name === change.name);
    if ('size' in change) entry.size = change.size;
    else { entry.hash = change.hash; entry.key = change.key; }
  }
  assert.deepEqual(Buffer.from(encodeTreeNode(restored)), Buffer.from(before), 'unexpected metadata delta');
  const encrypted = await encryptChk(encoded);
  const result = { hash: hash(encrypted.ciphertext), key: hex(encrypted.key) };
  await fs.writeFile(path.join(task, 'corrected-nodes', result.hash + '.bin'), encrypted.ciphertext);
  nodes.push({ path: relative || '/', oldHash: digest, ...result, bytes: encrypted.ciphertext.length });
  return result;
}

const result = await correct('', rootHash, rootKey);
assert.equal(sizeChanges.filter((entry) => /^\.git\/objects\/pack\/pack-[0-9a-f]{40}\.(pack|idx)$/.test(entry.path)).length, 2);
assert(sizeChanges.length > 2 && nodes.length >= 4);
const plan = { ...result, filesChecked, allPayloadCidsPreserved: true, sizeChanges, roundTrips, nodes };
await fs.writeFile(path.join(task, 'metadata-plan.json'), JSON.stringify(plan, null, 2) + '\n');
console.log(JSON.stringify({ filesChecked, correctedSizes: sizeChanges.length, changedNodes: nodes.length }));
