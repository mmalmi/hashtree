# git-remote-htree

Git remote helper for hashtree - push/pull git repos via Nostr and hashtree.

## Installation

```bash
curl -fsSL https://upload.iris.to/npub1xdhnr9mrv47kkrn95k6cwecearydeh8e895990n3acntwvmgk2dsdeeycm/releases%2Fhashtree/latest/install.sh | sh
```

That installs `htree`, `htree-cashu`, and `git-remote-htree` into `~/.local/bin` by default. Prebuilt macOS release binaries intentionally omit FUSE mount support so `htree` still runs on machines without macFUSE installed. Linux prebuilt binaries keep FUSE mount support. For a system-wide install, pass a target directory, for example `sh -s -- /usr/local/bin`.

Windows note: this shell bootstrap is not supported on Windows. Download the latest `hashtree-x86_64-pc-windows-msvc.zip` release asset, extract it, and add `htree.exe`, `htree-cashu.exe`, and `git-remote-htree.exe` to your PATH. The Windows release zip does not include FUSE mount support.

Or install with Cargo:

```bash
cargo install hashtree-cli git-remote-htree
```

If you already have `hashtree-cli`, just install the helper:

```bash
cargo install git-remote-htree
```

## Usage

```bash
# Clone a repo
git clone htree://npub1.../repo-name

# Push your own repo
git remote add htree htree://self/myrepo
git push htree master

# Link-visible repo (encrypted, shareable via secret URL)
git remote add origin htree://self/myrepo#link-visible
git push origin main

# Clone with secret key
git clone htree://npub1.../repo#k=<64-hex-chars>

# Private repo (encrypted, author-only)
git remote add origin htree://self/myrepo#private
git push origin main
```

Each pusher should use their own identity-specific remote instead of sharing one private key. If multiple people push with the same key and their local refs are not in sync, a later push can overwrite refs published by someone else.

## P2P

For P2P sharing between peers, run the hashtree daemon:

```bash
htree start
```

Git operations automatically use the local daemon when running, enabling direct peer-to-peer transfers via WebRTC.

## Configuration

Keys file: `~/.hashtree/keys`

```
nsec1abc123... default
nsec1xyz789... work
```

Use petnames in remote URLs: `htree://work/myproject`

## Performance Tuning

`git-remote-htree` fetches missing loose Git objects concurrently during clone/fetch. The default object download concurrency is 64, capped at 256. Override it for local experiments:

```bash
HTREE_GIT_OBJECT_DOWNLOAD_CONCURRENCY=32 git clone htree://npub1.../repo
```

Tree enumeration is controlled separately by `HTREE_GIT_TREE_WALK_CONCURRENCY` and defaults to 4, capped at 32.

Pushes batch new loose Git objects before uploading them to a single Blossom
write server. The default batch target is 4 MiB, tuned for public edge-backed
uploads. Batch requests run with the configured Blossom `upload_concurrency`.
Override the target in bytes for local-origin benchmarks or unusual server paths:

```bash
HTREE_GIT_BATCH_UPLOAD_TARGET_BYTES=8388608 git push htree://npub1.../repo main
```

Part of [hashtree-rs](https://git.iris.to/#/npub1xdhnr9mrv47kkrn95k6cwecearydeh8e895990n3acntwvmgk2dsdeeycm/hashtree).
