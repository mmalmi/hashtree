import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { mkdir, open, readFile, readdir, rename, link, unlink, stat } from 'node:fs/promises';
import { join } from 'node:path';
export const sha = bytes => createHash('sha256').update(bytes).digest('hex');
export function contents(size, salt) {
  assert(Number.isInteger(size) && size >= 400123 && size <= 2097275);
  return [['data.bin', Buffer.from(Array.from({ length: size }, (_, i) => (i + salt) % 251))],
    ['empty', Buffer.alloc(0)], ['café.txt', Buffer.from('mixed language durable receipt\n')]];
}
export async function syncDirectory(path) { const file = await open(path, 'r'); try { await file.sync(); } finally { await file.close(); } }
export async function readOptional(path) { try { return await readFile(path); } catch (e) { if (e.code === 'ENOENT') return undefined; throw e; } }
export async function atomicJson(path, value, directory) {
  const temporary = `${path}.pending`, file = await open(temporary, 'wx', 0o600);
  try { await file.writeFile(JSON.stringify(value)); await file.sync(); } finally { await file.close(); }
  await rename(temporary, path); await syncDirectory(directory);
}
export async function storage(root, onWrite) {
  const claims = join(root, 'claims'), parts = join(root, 'parts'), outputs = join(root, 'received');
  for (const path of [root, claims, parts, outputs]) await mkdir(path, { recursive: true, mode: 0o700 });
  let aborts = 0, writes = 0;
  const claimPath = key => join(claims, `${sha(key)}.json`);
  const store = {
    async load(key) { const raw = await readOptional(claimPath(key)); return raw && JSON.parse(raw); },
    async save(key, value) { await atomicJson(claimPath(key), value, claims); },
  };
  const partPath = (id, index) => join(parts, `${sha(id)}-${index}.part`);
  async function sink(id, index, spec) {
    assert(['data.bin', 'empty', 'café.txt'].includes(spec.filename));
    const part = partPath(id, index), final = join(outputs, spec.filename);
    const retained = await readOptional(part);
    return {
      retained: new Blob([retained ?? Buffer.alloc(0)]),
      async write(bytes) {
        const file = await open(part, 'a', 0o600);
        try { await file.writeFile(bytes); await file.sync(); } finally { await file.close(); }
        writes++; onWrite((await stat(part)).size);
      },
      async finish() {
        const file = await open(part, 'a', 0o600); await file.sync(); await file.close();
        return new Blob([await readFile(part)]);
      },
      async commit(signal) {
        signal?.throwIfAborted(); const bytes = await readFile(part);
        assert.equal(bytes.length, spec.sizeBytes); assert.equal(sha(bytes), spec.sha256);
        const existing = await readOptional(final);
        if (existing) assert.deepEqual(existing, bytes);
        else { await link(part, final); await syncDirectory(outputs); }
        return new Blob([bytes]);
      },
      async suspend() { await syncDirectory(parts); },
      async abort() { aborts++; await unlink(part).catch(e => { if (e.code !== 'ENOENT') throw e; }); await syncDirectory(parts); },
    };
  }
  async function retained(id) { return (await readOptional(partPath(id, 0)))?.length ?? 0; }
  async function verify(size, salt) {
    const entries = await readdir(outputs, { withFileTypes: true });
    assert(entries.every(e => e.isFile()));
    assert.deepEqual(entries.map(e => e.name).sort(), contents(size, salt).map(([name]) => name).sort());
    const rows = [];
    for (const [filename, bytes] of contents(size, salt)) {
      const path = join(outputs, filename), actual = await readFile(path);
      assert.deepEqual(actual, bytes); const info = await stat(path, { bigint: true });
      rows.push({ filename, bytes: actual.length, sha256: sha(actual), modifiedNs: info.mtimeNs.toString() });
    }
    return rows;
  }
  return { store, sink, retained, verify, counters: () => ({ aborts, writes }) };
}
