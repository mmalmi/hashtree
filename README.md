# hashtree

Content-addressed filesystem on Nostr. Merkle roots can be published to get mutable `npub/tree/path` addresses. Data is chunked, optionally encrypted by default (CHK), and works with Blossom-compatible storage and WebRTC fetches.

## Structure

- `ts/` - TypeScript/JavaScript SDK packages. See [`ts/README.md`](ts/README.md).
- `rust/` - Rust CLI/daemon, git remote helper, and crates. See [`rust/README.md`](rust/README.md).
- `apps/` - Applications (web + desktop)
  - `iris-files/` - Iris Files app (Tauri desktop + web). See [`apps/iris-files/README.md`](apps/iris-files/README.md).

## Design highlights

- SHA256 hashing
- Deterministic MessagePack encoding for tree nodes
- CHK encryption by default (hash + key in CIDs)
- Simple storage interface: `get(hash) -> bytes`, `put(hash, bytes)`
- 2MB chunks optimized for Blossom uploads
- Nostr-published roots for mutable addresses
- WebRTC fetches with Blossom fallback

## Getting started

- Web app + JS SDK: follow [`ts/README.md`](ts/README.md)
- CLI + daemon + git remote: follow [`rust/README.md`](rust/README.md)

## License

MIT
