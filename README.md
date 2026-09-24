# hashtree

Content-addressed storage for files, apps, and Git, implemented in Rust and
TypeScript. Data is chunked and encrypted by default, fetched from Blossom
servers or peers, and published under immutable hashes or mutable Nostr names.

[Hashtree source](https://git.iris.to/#/npub1xdhnr9mrv47kkrn95k6cwecearydeh8e895990n3acntwvmgk2dsdeeycm/hashtree) · [GitHub](https://github.com/mmalmi/hashtree)

## TypeScript / JavaScript

```bash
npm install @hashtree/core
```

```typescript
import { HashTree, MemoryStore } from '@hashtree/core';

const tree = new HashTree({ store: new MemoryStore() });
const { cid } = await tree.putFile(new TextEncoder().encode('Hello, hashtree!'));
const bytes = await tree.readFile(cid);
if (bytes) console.log(new TextDecoder().decode(bytes));
```

`MemoryStore` is temporary. Keep the complete CID, including its encryption key.

[Quickstart](ts/GETTING_STARTED.md) · [SDK packages](ts/README.md) · [API reference](ts/API.md)

## Rust library and CLI

```bash
cargo add hashtree-core                       # Library for a Rust project
cargo install hashtree-cli git-remote-htree    # CLI and Git helper
```

[Library example and API](rust/crates/hashtree-core/README.md) · [CLI guide](rust/README.md) · [Downloads and installation options](rust/README.md#installation)

## Documentation

- [Architecture, protocols, and related projects](docs/README.md)
- Rust [development](rust/README.md#development) and [releases](rust/README.md#releases)
- [npm publishing](ts/PUBLISHING.md)

## License

MIT
