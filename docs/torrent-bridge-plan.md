# Optional BitTorrent adapter

The Rust adapter supports demand-driven reads from BitTorrent v1, v2 and hybrid torrents, and seeding immutable hashtree file or directory snapshots to ordinary BitTorrent clients. It uses libtorrent 2.1 for the peer protocol, piece verification, magnets, DHT, peer exchange and optional trackers.

## Enable explicitly

The default CLI features remain `["lmdb"]`. Default builds do not link libtorrent or include the `torrent` command. Building the optional feature does not start any BitTorrent service: `htree start` is unchanged, and every torrent session is explicitly started by a command or library call.

Install libtorrent-rasterbar **2.1.x**, its development headers, a C++17 compiler and pkg-config. On macOS:

```sh
brew install libtorrent-rasterbar pkg-config
cd rust
cargo build --release -p hashtree-cli --bin htree --features bittorrent
```

The resulting optional binary requires the matching libtorrent shared library at runtime. Other platforms need libtorrent 2.1.x available through pkg-config; use `BOOST_ROOT` when Boost headers are outside the compiler search path. This is an optional native desktop/server integration, not a browser transport.

## Export and seed

Export uses an immutable `nhash` or raw blob hash. Give the torrent an explicit public name. The content must already be locally available; load it through normal hashtree tools first if necessary.

```sh
htree torrent export <nhash> --name collection --output collection.torrent
htree torrent seed <nhash> --name collection --listen 0.0.0.0:6881
```

Encrypted roots or descendants require `--allow-plaintext`. This publishes their decrypted file bytes to torrent peers. The adapter never places hashtree decryption keys or read capabilities in the torrent metadata or magnet. Without the flag, a public parent cannot silently expose an encrypted child. A keyless ciphertext blob can instead be exported as an opaque file.

Hybrid torrents are the default. Choose `--format v1` or `--format v2` explicitly if needed. `--piece-size` defaults to 256 KiB. Creation reads and verifies the content once to compute torrent hashes; it does not write a second complete copy to disk. Names, file sizes and content determine the torrent; local filesystem paths, timestamps, identities and keys are not added.

`seed` pins and indexes the selected snapshot before opening its session. Existing pins and tree metadata are preserved. Stopping a seed does not unpin content. Each new immutable snapshot produces its own torrent. A snapshot consisting entirely of empty files is not a valid export; empty files inside nonempty torrents are supported.

A seed prints a magnet and its listening port and runs until Ctrl-C. `--output <file.torrent>` also saves metadata. Output files are created atomically without overwriting existing files. Seed bandwidth can be capped with `--upload-limit <bytes-per-second>`.

## Read and import

Inspecting a local torrent opens no network sockets and does not spawn the CLI background updater:

```sh
htree torrent inspect collection.torrent
htree torrent get collection.torrent 'collection/file.bin' --peer <address:port> --offset 1024 --length 4096 --output part.bin
htree torrent import collection.torrent 'collection/file.bin' --peer <address:port>
```

Use the exact listed path or file index. `get` downloads only the pieces overlapping the requested range; v1 pieces may also contain adjacent file bytes. Omit the output path to write bytes to stdout. Import streams one selected file into normal hashtree storage, encrypts it by default, pins it, and prints its real immutable CID. `--unencrypted` explicitly selects public hashtree storage. Import files individually when only a subset of a directory is needed.

`get` and `import` also accept a quoted magnet URI. Metadata is obtained from peers and checked against the magnet's infohash before exposing file information. No placeholder hashtree hashes are invented from torrent metadata. Both v1 and v2 imports compute actual hashtree hashes from verified bytes.

## Discovery and limits

All listeners bind to `127.0.0.1:0` unless explicitly overridden. Use a reachable `--listen` address to seed publicly. There is no automatic UPnP, NAT-PMP, local-service discovery or DNS bootstrap.

- `--peer <numeric-address:port>` provides an initial peer, including IPv6 addresses in brackets.
- `--dht --bootstrap <numeric-address:port>` enables trackerless discovery with explicit bootstrap nodes. The session retains discovery while running. Built-in DNS bootstrap and torrent-supplied bootstrap nodes are not used.
- `--trackers` permits tracker URLs from the supplied torrent or magnet. Trackers are off by default. Web seeds are not used.
- `--cache-bytes` bounds downloaded piece storage, defaulting to 64 MiB. The cache must hold at least one torrent piece. It is memory-only and disappears when the session closes.
- `--timeout` bounds each range/metadata operation, defaulting to 60 seconds. Timeouts and cancellations are errors, never evidence that content does not exist.
- `--connections`, `--upload-limit` and `--download-limit` bound network work and bandwidth.

On cache eviction, the adapter drains and replaces the torrent handle before dropping pieces, so peers never receive a stale availability bitfield. Verified cached pieces and known peers are retained across that transition. Cancellation can interrupt and resume the transition safely; incomplete pieces are fetched again. Range reads keep useful peer connections open between requests. Concurrent reads share a serialized session and its verified piece cache.

The piece cache limit excludes libtorrent metadata/network buffers and the caller's returned range. Seeding reuses up to four 2 MiB plaintext windows to avoid re-reading/decrypting a large hashtree chunk for every 16 KiB peer request. Metadata and individual reads are limited to 16 MiB, piece sizes to 16 KiB–16 MiB, file count to 100,000 (including torrent padding), and piece count to 1,000,000. Symlink torrents and unsafe or non-UTF-8 paths are rejected. These limits are deliberate resource bounds.

## Library integration

`hashtree-bittorrent` has no default features. Enable its `libtorrent` feature to use `Metadata`, `Torrent`, `Seed` and `TorrentRoute`. `Snapshot` validates hashtree content and plaintext-export permission independently of the native engine.

`TorrentRoute` implements the existing opaque `BlobRoute` interface. Applications explicitly supply `BlobMapping` entries from a hashtree hash to a torrent file/range and register the route alongside other providers. Every returned range must match the requested hashtree SHA-256, in addition to libtorrent's piece verification. Absent mappings return `NoResult`; network failures and expired deadlines return errors. No daemon route is registered automatically, and public HTTP behavior is unchanged.

The adapter does not discover arbitrary hashtree hashes in the BitTorrent DHT. Nor are torrent infohashes, v2 file roots and hashtree roots interchangeable. The source mapping is necessary even though v2 uses SHA-256.

## Verification

The optional gate is `rust/scripts/test_bittorrent_adapter.sh`. It requires the libtorrent Python binding for the separate ordinary-client fixture; select its interpreter with `HASHTREE_BITTORRENT_PYTHON`.

Tests use local peers and servers and cover all three torrent formats in both directions, nested directories, empty files, cross-piece/file boundaries, byte-range selectivity, bounded-cache eviction and re-fetching, magnets, normal encrypted hashtree materialization, SHA-256 rejection, timeout/cancellation recovery, encrypted-descendant consent, corrupted/missing source blobs, and feature-gated CLI parsing. Embedded tracker and web-seed URLs stay inactive by default; a positive control checks explicit tracker activation. The ordinary-client fixture uses libtorrent's filesystem storage independently of the adapter's custom storage.

The command-line integration gate additionally exercises export, seed, range get and pinned import against fresh local stores. It verifies output-file preservation and rejection of accidental plaintext export. Default dependency-tree and command-parsing checks prove the adapter remains opt-in.
