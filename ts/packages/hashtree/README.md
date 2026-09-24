# @hashtree/core

Core merkle tree library for content-addressed storage.

## Install

```bash
npm install https://github.com/mmalmi/hashtree/releases/download/hashtree-ts-runtime-v0.5.7/hashtree-core-0.3.2.tgz
```

On npm 12+, add `--allow-remote=all` to `npm install` and `npm ci` commands that use these release archives.

The npm registry's `@hashtree/core` latest is still `0.1.7`. Use the release archive above for core `0.3.2`; matching optional packages are available in the [TypeScript runtime 0.5.7 release](https://github.com/mmalmi/hashtree/releases/tag/hashtree-ts-runtime-v0.5.7).

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
