# @hashtree/core

Core merkle tree library for content-addressed storage.

[Getting started](https://github.com/mmalmi/hashtree/blob/master/ts/GETTING_STARTED.md) · [API reference](https://github.com/mmalmi/hashtree/blob/master/ts/API.md) · [SDK packages](https://github.com/mmalmi/hashtree/blob/master/ts/README.md)

## Install

```bash
npm install @hashtree/core
```

Core `0.3.2` is available on npm. See the [SDK installation guide](https://github.com/mmalmi/hashtree/blob/master/ts/README.md#install)
for optional packages and immutable release archives.

## Usage

```typescript
import { HashTree, MemoryStore } from '@hashtree/core';

const store = new MemoryStore();
const tree = new HashTree({ store });

// Store a file
const { cid } = await tree.putFile(new TextEncoder().encode('Hello'));

// Read it back
const data = await tree.readFile(cid);
if (!data) throw new Error('File is unavailable');
console.log(new TextDecoder().decode(data)); // Hello
```

Keep the whole CID: encrypted files need `cid.key` as well as `cid.hash`.
Use `nhashEncode(cid)` and `nhashDecode(identifier)` to serialize it. Anyone with
that identifier can decrypt the file if they can retrieve its blocks.
`MemoryStore` is temporary and does not upload data.

## Common operations

| Operation | API |
| --- | --- |
| Store/read a file | `putFile(bytes)` / `readFile(cid, { maxBytes })` |
| Store plaintext | `putFile(bytes, { unencrypted: true })` |
| Build a directory | `putDirectory([{ name, cid, size, type }])` |
| List/resolve paths | `listDirectory(cid, signal?)` / `resolvePath(cid, path, signal?)` |
| Edit a directory | `setEntry(root, path, name, cid, size, type)` returns a new root |
| Remove/rename entries | `removeEntry(root, path, name)` / `renameEntry(root, path, oldName, newName)` |
| Incremental writes | `createStream()` → `append(bytes)` → `finalize()` |
| Streaming/range reads | `readFileStream(cid)` / `readFileRange(cid, start, end)` |
| Copy stored blocks | `walkBlocks(cid)` yields ciphertext blocks and hashes |

File reads can return `null` for unavailable blocks. Directory listing and path
resolution wait for blocks; pass an `AbortSignal` to cancel. Immutable edits keep
old roots readable. See the [tested guide](https://github.com/mmalmi/hashtree/blob/master/ts/GETTING_STARTED.md) for full examples.

## Features

- SHA256 content addressing
- Deterministic MessagePack encoding
- CHK encryption by default
- 2MB chunks (Blossom-compatible)
- Streaming reads/writes

## Storage Backends

- `MemoryStore` - In-memory
- `BlossomStore` - Remote Blossom server
- `FallbackStore` - Chain multiple stores

See [Dexie](https://github.com/mmalmi/hashtree/blob/master/ts/packages/hashtree-dexie/README.md) for IndexedDB and
[FIPS transport](https://github.com/mmalmi/hashtree/blob/master/ts/packages/hashtree-fips-transport/README.md) for peer blob transport.

## Remote storage with Blossom

Supply a server that accepts uploads and a Nostr signer (for example, your
wallet's `signEvent` callback). This function writes directly to that server and
returns a portable identifier. Plain server URL strings configure **reads only**;
uploads need `write: true`. Browsers also need the server to allow CORS.

```typescript
import { BlossomStore, HashTree, nhashEncode, nhashDecode, type BlossomSigner } from '@hashtree/core';

export async function uploadFile(url: string, signer: BlossomSigner, data: Uint8Array) {
  const store = new BlossomStore({
    servers: [{ url, read: true, write: true }],
    signer,
    getTimeoutMs: 10_000,
    putTimeoutMs: 30_000,
  });
  const { cid } = await new HashTree({ store }).putFile(data);
  return nhashEncode(cid);
}

// A separate client needs only a read endpoint and the complete identifier.
export async function downloadFile(url: string, identifier: string, maxBytes = 8 * 1024 * 1024) {
  const tree = new HashTree({
    store: new BlossomStore({ servers: [url], getTimeoutMs: 10_000 }),
  });
  const bytes = await tree.readFile(nhashDecode(identifier), { maxBytes });
  if (!bytes) throw new Error('File is unavailable');
  return bytes;
}
```

Await `uploadFile()` and verify a fresh-client read before announcing the root.
The signer signs upload authorization, never the plaintext file; storage receives
encrypted blocks. Nostr root publication is a separate operation. With multiple
write servers, a successful upload does not promise a copy on every server;
monitor `onUploadProgress` if your app requires a replication policy.

To copy an existing local file or directory, iterate `localTree.walkBlocks(cid)`
and await `remoteStore.put(block.hash, block.data)` for every block. Transfer the
whole reachable tree, not just the root block. `Store.put()`'s boolean describes
whether bytes were newly added; it is not a durability receipt. Always handle
errors and verify the read path. A `404` can yield `null`; timeouts and server
errors can reject. Servers see blob hashes and sizes, not logical filenames or
decryption keys unless your app separately discloses them.

## License

MIT
