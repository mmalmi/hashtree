# hashtree

Content-addressed storage for files, apps, and Git. Data is chunked and encrypted
by default, fetched from Blossom servers or peers, and published under immutable
hashes or mutable Nostr names.

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

## CLI and Git

```bash
cargo install hashtree-cli git-remote-htree
```

[Downloads, Homebrew, and other installation options](rust/README.md#installation) · [CLI guide](rust/README.md)

## Documentation

- [Architecture, protocols, and related projects](docs/README.md)
- Rust [development](rust/README.md#development) and [releases](rust/README.md#releases)
- [npm publishing](ts/PUBLISHING.md)

## License

MIT
