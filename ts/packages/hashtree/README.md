# @hashtree/core

Core merkle tree library for content-addressed storage.

## Install

```bash
npm install @hashtree/core
```

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

See [@hashtree/dexie](https://npmjs.com/package/@hashtree/dexie) for IndexedDB and [@hashtree/nostr](https://npmjs.com/package/@hashtree/nostr) for WebRTC P2P.

## License

MIT
