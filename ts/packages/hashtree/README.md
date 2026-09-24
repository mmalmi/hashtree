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
import { HashTree, MemoryStore, toHex } from '@hashtree/core';

const store = new MemoryStore();
const tree = new HashTree({ store });

// Store a file
const { cid } = await tree.putFile(new TextEncoder().encode('Hello'));
console.log(toHex(cid.hash));

// Read it back
const data = await tree.readFile(cid);
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

See [@hashtree/dexie](https://npmjs.com/package/@hashtree/dexie) for IndexedDB and [@hashtree/fips-transport](https://npmjs.com/package/@hashtree/fips-transport) for P2P blob transport.

## License

MIT
