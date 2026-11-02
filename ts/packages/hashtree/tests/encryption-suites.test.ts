import { describe, expect, it } from 'vitest';
import { encode } from '@msgpack/msgpack';
import {
  HashTree, MemoryStore, LinkType, encryptObject, decryptObject, encryptionMode,
  nhashEncode, nhashDecode, encodeTreeNode, decodeTreeNode, fromHex, toHex,
  encryptKeyForLink, decryptKeyFromLink, generateLinkKey,
  type EncryptionMode,
} from '../src/index.js';
import { decryptRootNodeOrBlob } from '../src/encrypted.js';

const modes: EncryptionMode[] = ['legacy-chk', 'chk-v1', 'rnd-v1'];
const data = new TextEncoder().encode('A file spanning several chunks.');

describe.each(modes)('%s compatibility', mode => {
  it('reads chunked writes, streams and ranges through the default reader', async () => {
    const store = new MemoryStore();
    const writer = new HashTree({ store, chunkSize: 7, encryptionMode: mode });
    const reader = new HashTree({ store });
    const { cid } = await writer.putFile(data);
    expect(encryptionMode(cid.key!)).toBe(mode);
    expect(nhashDecode(nhashEncode(cid))).toEqual(cid);
    expect(await reader.readFile(cid)).toEqual(data);
    expect(await reader.readFileRange(cid, 5, 23)).toEqual(data.slice(5, 23));
    const chunks: number[] = [];
    for await (const chunk of reader.readFileStream(cid)) chunks.push(...chunk);
    expect(Uint8Array.from(chunks)).toEqual(data);
    const stream = writer.createStream();
    await stream.append(data.slice(0, 12));
    await stream.append(data.slice(12));
    const streamed = await stream.finalize();
    expect(encryptionMode(streamed.key!)).toBe(mode);
    expect(await reader.readFile(streamed)).toEqual(data);
    const patched = await reader.writeAt(cid, 6, Uint8Array.of(97, 98, 99));
    const expected = data.slice(); expected.set([97, 98, 99], 6);
    expect(await reader.readFile(patched)).toEqual(expected);
    expect(encryptionMode(patched.key!)).toBe(mode === 'rnd-v1' ? mode : 'chk-v1');
  });
});

it('defaults to versioned CHK and traverses a mixed, fanned-out directory', async () => {
  const store = new MemoryStore();
  const tree = new HashTree({ store, maxLinks: 2 });
  const entries = [];
  for (const mode of modes) {
    const file = await new HashTree({ store, encryptionMode: mode }).putFile(data);
    entries.push({ name: mode, ...file, type: LinkType.Blob });
  }
  entries.push({ name: 'plain', ...await tree.putFile(data, { unencrypted: true }), type: LinkType.Blob });
  const root = await tree.putDirectory(entries);
  expect(encryptionMode(root.cid.key!)).toBe('chk-v1');
  for (const entry of await tree.listDirectory(root.cid)) expect(await tree.readFile(entry.cid)).toEqual(data);
  const random = await new HashTree({ store, encryptionMode: 'rnd-v1' }).putDirectory(entries);
  const edited = await tree.setEntry(random.cid, [], 'added', entries[0].cid, data.length, LinkType.Blob);
  expect(encryptionMode(edited.key!)).toBe('rnd-v1');
});

it('matches the randomized vector and rejects tampering and unsupported suites', async () => {
  const key = new Uint8Array(33).fill(2);
  const ciphertext = fromHex('000102030405060708090a0b216d8e96be582844fc3a4e2a23eecfbe1a29cd5b6b');
  expect(new TextDecoder().decode(await decryptObject(ciphertext, key))).toBe('hello');
  ciphertext[12] ^= 1;
  await expect(decryptObject(ciphertext, key)).rejects.toThrow();
  const a = await encryptObject(data, 'rnd-v1');
  const b = await encryptObject(data, 'rnd-v1');
  expect(toHex(a.ciphertext)).not.toBe(toHex(b.ciphertext));
  const plaintextTree = encodeTreeNode({ type: LinkType.Dir, links: [] });
  await expect(decryptRootNodeOrBlob(plaintextTree, key)).rejects.toThrow();
  for (const invalid of [new Uint8Array(31), new Uint8Array(33).fill(3)]) {
    await expect(decryptRootNodeOrBlob(plaintextTree, invalid)).rejects.toThrow();
    expect(() => nhashEncode({ hash: new Uint8Array(32), key: invalid })).toThrow();
    expect(() => decodeTreeNode(encode({ l: [{ h: new Uint8Array(32), k: invalid, s: 0, t: 0 }], t: 2 }))).toThrow();
  }
});

it('wraps both key formats without truncating suite bytes', async () => {
  for (const mode of modes) {
    const { key } = await encryptObject(data, mode);
    const secret = generateLinkKey(key.length as 32 | 33);
    const masked = encryptKeyForLink(key, secret);
    expect(masked.length).toBe(key.length);
    expect(await decryptKeyFromLink(masked, secret)).toEqual(key);
  }
  expect(generateLinkKey()).toHaveLength(33);
});

it('keeps a 32-byte sharing link readable after a CHK root update', async () => {
  const secret = generateLinkKey(32);
  const root = await encryptObject(data);
  const masked = encryptKeyForLink(root.key, secret);
  const recovered = await decryptKeyFromLink(masked, secret);
  expect(encryptionMode(recovered!)).toBe('legacy-chk');
  expect(await decryptObject(root.ciphertext, recovered!)).toEqual(data);
  const random = await encryptObject(data, 'rnd-v1');
  expect(() => encryptKeyForLink(random.key, secret)).toThrow();
});

it('reads and patches a file with individually encrypted and plaintext chunks', async () => {
  const store = new MemoryStore();
  const tree = new HashTree({ store });
  const links = [];
  for (const [i, mode] of [undefined, ...modes].entries()) {
    const part = Uint8Array.of(i, i);
    const writer = new HashTree({ store, encryptionMode: mode });
    const { cid, size } = await writer.putFile(part, { unencrypted: !mode });
    links.push({ ...cid, size, type: LinkType.Blob });
  }
  const manifest = encodeTreeNode({ type: LinkType.File, links });
  const expected = Uint8Array.of(0, 0, 1, 1, 2, 2, 3, 3);
  for (const unencrypted of [false, true]) {
    const { cid } = await tree.putFile(manifest, { unencrypted });
    expect(await tree.readFile(cid)).toEqual(expected);
    expect(await tree.readFileRange(cid, 1, 7)).toEqual(expected.slice(1, 7));
    const bytes = [];
    for await (const chunk of tree.readFileStream(cid, { offset: 1 })) bytes.push(...chunk);
    expect(Uint8Array.from(bytes)).toEqual(expected.slice(1));
    const patched = await tree.writeAt(cid, 1, new Uint8Array(6).fill(9));
    expect(await tree.readFile(patched)).toEqual(Uint8Array.of(0, 9, 9, 9, 9, 9, 9, 3));
    const updated = (await tree.getTreeNode(patched))!;
    expect(updated.links[0].key).toBeUndefined();
    expect(encryptionMode(updated.links[3].key!)).toBe('rnd-v1');
  }
});
