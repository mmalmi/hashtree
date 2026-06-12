# hashtree

Content-addressed storage, git transport, and app runtime on Nostr. Merkle roots can be published to get mutable `npub/tree/path` addresses. Data is chunked, CHK-encrypted by default, and can be fetched from Blossom-compatible storage, FIPS peers, or a local daemon.

## Repositories

- [`git.iris.to`](https://git.iris.to/#/npub1xdhnr9mrv47kkrn95k6cwecearydeh8e895990n3acntwvmgk2dsdeeycm/hashtree)
- [`GitHub`](https://github.com/mmalmi/hashtree)

## Installation

### Quick install (prebuilt binaries, macOS/Linux)

```bash
curl -fsSL https://upload.iris.to/npub1xdhnr9mrv47kkrn95k6cwecearydeh8e895990n3acntwvmgk2dsdeeycm/releases%2Fhashtree/latest/install.sh | sh
```

That installs `htree`, `htree-cashu`, and `git-remote-htree` into `~/.local/bin` by default. Prebuilt macOS release binaries intentionally omit FUSE mount support so `htree` still runs on machines without macFUSE installed. Build from source with `cargo install hashtree-cli --no-default-features --features p2p,lmdb,fuse` if you need `htree mount` on macOS. Linux prebuilt binaries keep FUSE mount support. For a system-wide install, pass a target directory, for example `sh -s -- /usr/local/bin`.

Windows note: the shell bootstrap is not supported there. Download the latest `hashtree-x86_64-pc-windows-msvc.zip` release asset, extract it, and add `htree.exe`, `htree-cashu.exe`, and `git-remote-htree.exe` to your PATH. The Windows release zip does not include FUSE mount support.

### Build from source

Install Rust first if needed:

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source "$HOME/.cargo/env"
```

```bash
# Git helper only (enables git clone/pull/push for htree:// URLs)
cargo install git-remote-htree

# CLI + daemon (cargo defaults keep FUSE optional)
cargo install hashtree-cli

# CLI + daemon + git helper + Cashu helper
cargo install hashtree-cli git-remote-htree hashtree-cashu-cli

# Add FUSE mount support explicitly when you want it
cargo install hashtree-cli --no-default-features --features p2p,lmdb,fuse
```

For cargo installs, `fuse` is opt-in. That keeps `cargo install hashtree-cli` working on machines that do not have platform FUSE headers/libs available.

- Linux: install FUSE 3 development packages first, typically `pkg-config` plus `libfuse3-dev` (package names vary by distro).
- macOS: install macFUSE before building with `--features p2p,lmdb,fuse`.
- Prebuilt macOS release tarballs and the macOS Homebrew package omit FUSE mount support so `htree` works without macFUSE installed.
- Linux release tarballs and Linux Homebrew installs keep FUSE mount support.
- The Windows release zip does not include FUSE mount support.

### Local install from this repo

```bash
cargo install --path rust/crates/hashtree-cli
cargo install --path rust/crates/git-remote-htree
cargo install --path rust/crates/hashtree-cashu-cli

# Local build with FUSE mount support
cargo install --path rust/crates/hashtree-cli --no-default-features --features p2p,lmdb,fuse
```

### Homebrew

```bash
brew tap sirius/hashtree https://upload.iris.to/npub1xdhnr9mrv47kkrn95k6cwecearydeh8e895990n3acntwvmgk2dsdeeycm/homebrew-hashtree.git
brew trust --tap sirius/hashtree
brew install htree
```

That installs `htree`, `htree-cashu`, and `git-remote-htree`. After tapping, `brew install hashtree` also works via the alias.

### Packaging status

- `./publish_release.sh --version v<version>` is the primary release entrypoint. It publishes the hashtree release, updates the Homebrew tap when the full macOS/Linux CLI set is present, and updates GitHub with the same staged files.
- CLI release artifacts are assembled under `rust/dist/` by `rust/scripts/release_to_htree.sh`, which `./publish_release.sh` wraps.
- Linux package-manager installs beyond Homebrew are not shipped yet.

## Current status

- The core storage format, CHK encryption, CLI/daemon, and `git-remote-htree` are implemented and used across the Rust and TypeScript stacks.
- The standalone app repos now live alongside this repo: [`iris-browser`](https://git.iris.to/#/npub1xdhnr9mrv47kkrn95k6cwecearydeh8e895990n3acntwvmgk2dsdeeycm/iris-browser), [`iris-apps`](https://git.iris.to/#/npub1xdhnr9mrv47kkrn95k6cwecearydeh8e895990n3acntwvmgk2dsdeeycm/iris-apps), and [`hashtree-cc`](https://git.iris.to/#/npub1xdhnr9mrv47kkrn95k6cwecearydeh8e895990n3acntwvmgk2dsdeeycm/hashtree-cc).
- Packaging is still uneven: Cargo installs, release tarballs, and the Homebrew tap work today; `apt` style packaging is still pending.
- The protocol is implemented, but the written spec is still a draft and nearby Bluetooth/Wi-Fi sync work is still in progress.

## Repository layout

- `rust/` - Rust CLI/daemon, git remote helper, and core crates. See [`rust/README.md`](rust/README.md).
- `ts/` - TypeScript/JavaScript SDK packages. See [`ts/README.md`](ts/README.md).
- Sibling repos:
  - [`iris-browser`](https://git.iris.to/#/npub1xdhnr9mrv47kkrn95k6cwecearydeh8e895990n3acntwvmgk2dsdeeycm/iris-browser) - Native desktop shell built with Tauri.
  - [`iris-apps`](https://git.iris.to/#/npub1xdhnr9mrv47kkrn95k6cwecearydeh8e895990n3acntwvmgk2dsdeeycm/iris-apps) - Portable Iris web apps and the isolated site runtime.
  - [`hashtree-cc`](https://git.iris.to/#/npub1xdhnr9mrv47kkrn95k6cwecearydeh8e895990n3acntwvmgk2dsdeeycm/hashtree-cc) - Landing page and file sharing app.

## Design highlights

- SHA256 hashing
- Deterministic MessagePack encoding for tree nodes
- CHK encryption by default (hash + key in CIDs)
- Simple storage interface: `get(hash) -> bytes`, `put(hash, bytes)`
- 2MB chunks optimized for Blossom uploads
- Nostr-published roots for mutable addresses
- FIPS peer fetches with Blossom fallback

## Getting started

- Building decentralized apps and data models on hashtree: follow [`ts/GETTING_STARTED.md`](ts/GETTING_STARTED.md)
- CLI + daemon + git remote: follow [`rust/README.md`](rust/README.md)
- JS SDK packages: follow [`ts/README.md`](ts/README.md)
- Portable web + Iris Browser app runtime: use [`@hashtree/worker`](https://www.npmjs.com/package/@hashtree/worker) from [`ts/packages/hashtree-worker`](https://git.iris.to/#/npub1xdhnr9mrv47kkrn95k6cwecearydeh8e895990n3acntwvmgk2dsdeeycm/hashtree/ts/packages/hashtree-worker), with host/runtime details in [`iris-browser`](https://git.iris.to/#/npub1xdhnr9mrv47kkrn95k6cwecearydeh8e895990n3acntwvmgk2dsdeeycm/iris-browser)
- Native desktop shell: follow [`iris-browser`](https://git.iris.to/#/npub1xdhnr9mrv47kkrn95k6cwecearydeh8e895990n3acntwvmgk2dsdeeycm/iris-browser)
- Portable web apps and release flows: follow [`iris-apps`](https://git.iris.to/#/npub1xdhnr9mrv47kkrn95k6cwecearydeh8e895990n3acntwvmgk2dsdeeycm/iris-apps)

## Site Releases

- Release all Cloudflare/hashtree static sites with `node ./scripts/release-sites.mjs`
- `scripts/release-sites.mjs` expects sibling `../iris-apps` and `../hashtree-cc` checkouts unless `IRIS_APPS_REPO_ROOT` and `HASHTREE_CC_REPO_ROOT` override them

## Mobile FFI (optional)

- FFI crate: [`rust/crates/hashtree-ffi`](rust/crates/hashtree-ffi) (UniFFI bindings for attachment operations)
- Native Rust apps should usually use `hashtree-core`/`hashtree-blossom` directly
- Mobile/Flutter apps can build `hashtree-ffi` and generate Kotlin/Swift bindings with UniFFI

## Protocol spec

- [`docs/HTS-01.md`](docs/HTS-01.md) - hashtree core protocol (draft)
- [`docs/hashtree-on-fips.md`](docs/hashtree-on-fips.md) - FIPS discovery, signaling, and transport plan for Hashtree blobs
- [`docs/URL-ENCODING.md`](docs/URL-ENCODING.md) - concise routing rules for slash-containing tree names
- [`docs/architecture.html`](docs/architecture.html) - visual overview of the current content, routing, and transport layers
- [`docs/blossom-reconciliation-and-large-fetch-plan.md`](docs/blossom-reconciliation-and-large-fetch-plan.md) - deterministic Blossom reconciliation and large-repo fetch plan
- [`docs/git-repo-fetch-push-improvement-plan.md`](docs/git-repo-fetch-push-improvement-plan.md) - measured roadmap for faster git repo fetch and push over hashtree and Blossom
- [`docs/torrent-bridge-plan.md`](docs/torrent-bridge-plan.md) - torrent-backed virtual serving and optional materialization plan
- [`docs/vercel-on-hashtree-plan.md`](docs/vercel-on-hashtree-plan.md) - static-site deploy platform plan built on top of hashtree, `iris-sites`, and `blossom-cf-worker-rust`

## License

MIT
