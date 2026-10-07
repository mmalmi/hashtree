# hashtree/rust

Rust implementation of hashtree (CLI, daemon, git remote helper, and crates).

Part of the hashtree repository. See [../README.md](../README.md) for the project overview and [../ts/README.md](../ts/README.md) for the TypeScript SDK.

Blossom-compatible storage with chunking and directory structure. Merkle roots can be published on Nostr to get mutable `npub/path` addresses.

## Installation

### Prebuilt binaries (macOS/Linux)

Download the archive for your platform from the [release assets](https://github.com/mmalmi/hashtree/releases), extract it, and run `./install.sh`. Htree-published releases may also publish a top-level `install.sh` asset that downloads the matching platform archive from the same release root and delegates to the packaged installer.

The installer places `htree`, `htree-cashu`, and `git-remote-htree` into `~/.local/bin` by default. Prebuilt release binaries omit optional FUSE mount support. Build from source with `cargo install hashtree-cli --no-default-features --features lmdb,fuse` if you need `htree mount`. For a system-wide install, pass a target directory, for example `./install.sh /usr/local/bin`.

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
cargo install hashtree-cli --no-default-features --features lmdb,fuse
```

For cargo installs, `fuse` is opt-in. That keeps `cargo install hashtree-cli` working on machines that do not have platform FUSE headers/libs available.

- Linux: install FUSE 3 development packages first, typically `pkg-config` plus `libfuse3-dev` (package names vary by distro).
- macOS: install macFUSE before building with `--features lmdb,fuse`.
- Prebuilt release tarballs and Homebrew packages omit FUSE mount support.
- The Windows release zip does not include FUSE mount support.

### Local install from this repo

Run these commands from the repository root:

```bash
cargo install --path rust/crates/hashtree-cli
cargo install --path rust/crates/git-remote-htree
cargo install --path rust/crates/hashtree-cashu-cli

# Local build with FUSE mount support
cargo install --path rust/crates/hashtree-cli --no-default-features --features lmdb,fuse
```

### Homebrew

```bash
brew tap sirius/hashtree https://upload.iris.to/npub1xdhnr9mrv47kkrn95k6cwecearydeh8e895990n3acntwvmgk2dsdeeycm/homebrew-hashtree.git
brew trust --tap sirius/hashtree
brew install htree
```

That installs `htree`, `htree-cashu`, and `git-remote-htree`. After tapping, `brew install hashtree` also works via the alias.

Linux package-manager installs beyond Homebrew (such as `apt`) are not shipped yet.

## Store & retrieve files
```bash
# Install the CLI (see #installation)

# Add file or dir
htree add file.txt

# Download bitcoin.pdf
htree get nhash1qqsw9hdps3pkyjm7nlg9783xazg4cnmuj8sp4wnddsa8lzku6qt457c9yzckwsv34z8vtnwhx0jzgz5psqcsthzp94kxwzx482u5lsjg7n64x5rckap/bitcoin.pdf

```

## Nostr archive catch-up and P2P events

`htree nostr-index catch-up` ingests relay events and automatically supplements
those sources with `nostr-pubsub` events from the FIPS network. No provider list
is required. Peers are discovered through the ordinary host-local FIPS network,
configured FIPS connections, and signed adverts over relays. Relay discovery
subscriptions stay open for the run. The indexer uses an ephemeral identity and
socket, independently of the daemon's signing identity and storage writer.

The configured FIPS transport settings, discovery scope, and optional discovery
relay override apply. Without that override, discovery uses the archive's source
relays. Discovery and peer connections are bounded to 16; pubsub chooses peers
automatically. Relay intake remains available if P2P startup or queries fail.

Each author/time window receives a bounded peer query alongside the relay fetch.
Matching verified events are deduplicated and committed by the same ordered
archive writer. Peer observations have separate receipts, including source counts
and an event-ID commitment. Quiet peers, failed queries, and partial peer history
never establish archive completeness or substitute for a required relay result.
This supplements visited catch-up windows; it is not a continuous whole-network
subscription or exhaustive peer-history crawl. Each peer query retains at most
128 events, and the combined result must fit the existing author event/byte limits.

Discovery is runtime state, not a fixed source roster in the archive policy.
Existing relay checkpoints resume unchanged; new checkpoints record observations
from the peers actually encountered alongside the relay coverage receipt.

The libraries remain acyclic: `nostr-pubsub` defines neutral interfaces,
`hashtree-nostr` owns indexes, and `hashtree-nostr-pubsub` adapts those indexes as
providers. The CLI composes transport and storage.

## Git on hashtree

```bash
# Install the CLI + git helper (see #installation)

# Clone a repo
git clone htree://npub1dxs2pygtfxsah77yuncsmu3ttqr274qr56xz3gsvetxzq2vjfnxsy6knkn/hashtree/rust

# Or push your own repo
# "self" is an alias for autogenerated nostr key in ~/.hashtree/keys
git remote add htree htree://self/myrepo
git push htree master
```

View repos at git.iris.to, for example [hashtree/rust](https://git.iris.to/#/npub1xdhnr9mrv47kkrn95k6cwecearydeh8e895990n3acntwvmgk2dsdeeycm/hashtree/rust)

## Design

- **SHA256** hashing
- **MessagePack** encoding for tree nodes (deterministic)
- **CHK encryption** by default (Content Hash Key) — ~2-3x overhead vs plain (still 500+ MiB/s)
- **Dumb storage**: Works with any key-value store (hash → bytes). Unlike BitTorrent, no active merkle proof computation needed—just store and retrieve blobs by hash.
- **2MB chunks** by default (optimized for blossom uploads)

## Usage

For the library, add `hashtree-core` and follow its
[guide](crates/hashtree-core/README.md) for files, directories, streaming, and storage.
The [published Rust API](https://docs.rs/hashtree-core/latest/hashtree_core/) is
searchable by type or method; choose the version matching your `Cargo.lock`.
For command-line applications, see the CLI examples below.

## Tree Nodes

Every stored item is either raw bytes or a tree node. Tree nodes are MessagePack-encoded with a `type` field:

- `Blob` (0) - Raw data chunk (not a tree node, just bytes)
- `File` (1) - Chunked file: links are unnamed, ordered by byte offset
- `Dir` (2) - Directory: links have names, may point to files or subdirs

Wire format: `{t: LinkType, l: [{h: hash, s: size, n?: name, t: linkType, ...}]}`

## Crates

The `Store` trait is just `get(hash) → bytes` and `put(hash, bytes)`. The core is transport-agnostic—works with any backend that can store/fetch by hash.

- `hashtree-core` - Core merkle tree library
- `hashtree-merge` - Deterministic path-based overlay merge primitives
- `hashtree-fs` - Filesystem helpers and tree traversal
- `hashtree-resolver` - Nostr-based tree resolution
- [`hashtree-client`](crates/hashtree-client/README.md) - Verified daemon-assisted or standalone content reads for applications
- `hashtree-blossom` - Blossom client/server helpers
- `hashtree-network` - Adaptive ordering across opaque, read-only blob routes
- `hashtree-updater` - App update discovery, platform asset selection, and install helpers backed by `npub/tree/path` release roots
- `hashtree-lmdb` - LMDB storage backend
- `hashtree-s3` - S3 storage backend
- `hashtree-config` - Config loading and defaults
- `hashtree-cli` - Command-line interface and daemon
- `hashtree-cashu-cli` - Cashu wallet helper for `htree cashu`
- `hashtree-sim` - Loopback Nostr relay fixture for integration tests
- `git-remote-htree` - Git remote helper (`htree://` protocol)

## Daemon

Run `htree start` to serve local storage and enable configured FIPS blob routes:

```bash
htree start                  # Start daemon (default port 8080)
htree reload                 # Reload config by restarting daemonized instance
htree status                 # Check daemon and FIPS route status
```

The daemon acts as a local Blossom server. Remote blob reads use the canonical
`BlobRequest`/`BlobReply` service over FIPS; FIPS owns transport addresses and
may use UDP, FIPS WebRTC, or another underlay. Git operations automatically use
the daemon when running.

The daemon also advertises its blob service through FIPS same-host rendezvous
and reads from discovered local providers without a configured peer list.
Discovered and configured providers share one bounded, deduplicated route;
forwarded misses still consume the ordinary mesh hop budget.

## Git Remote Helper

Push/pull git repos via hashtree:

```bash
# Install (local path)
cargo install --path crates/hashtree-cli
cargo install --path crates/hashtree-cashu-cli
cargo install --path crates/git-remote-htree

# Configure signing keys in ~/.hashtree/keys
# Format: <nsec or secret hex> [petname]
nsec1abc123... work

# Optional: configure read-only aliases in ~/.hashtree/aliases
# Format: <npub or pubkey hex> [petname]
npub1xdhnr9mrv47kkrn95k6cwecearydeh8e895990n3acntwvmgk2dsdeeycm siriusbusiness

# Use
git remote add origin htree://work/myproject
git push origin main
git clone htree://npub1.../repo-name
git clone htree://siriusbusiness/repo-name

# Link-visible repo (encrypted, shareable via secret URL)
git remote add origin htree://self/myrepo#link-visible
git push origin main
# Follow the instructions to set the generated key, then push again

# Clone with secret key
git clone htree://npub1.../repo#k=<64-hex-chars>

# Private repo (encrypted, author-only)
git remote add origin htree://self/myrepo#private
git push origin main
git clone htree://self/myrepo#private
```

Each pusher should use their own identity-specific remote instead of sharing one private key. If multiple people push with the same key and their local refs are not in sync, a later push can overwrite refs published by someone else.

Cashu wallet commands are provided by the separate `htree-cashu` helper. Install it next to `htree`:

```bash
cargo install --path crates/hashtree-cashu-cli
htree cashu balance
```

## Configuration

Config file: `~/.hashtree/config.toml`

```toml
[blossom]
read_servers = ["https://cdn.iris.to"]
write_servers = ["https://upload.iris.to"]
max_upload_mb = 100
upload_concurrency = 10

[nostr]
relays = [
    "wss://relay.damus.io",
    "wss://relay.snort.social",
    "wss://relay.primal.net"
]
bootstrap_follows = [
    "npub1xdhnr9mrv47kkrn95k6cwecearydeh8e895990n3acntwvmgk2dsdeeycm"
] # local-only seed for new identities; set to [] to opt out
```

Keys file: `~/.hashtree/keys`

```
nsec1abc123... default
nsec1xyz789... work
```

Aliases file: `~/.hashtree/aliases`

```
npub1xdhnr9mrv47kkrn95k6cwecearydeh8e895990n3acntwvmgk2dsdeeycm siriusbusiness
```

New identities seed `contacts.json` locally with the `bootstrap_follows` list so the social graph starts with an entrypoint follow. This is local-only and does not publish a contact list event by itself.

`git-remote-htree` auto-creates `~/.hashtree/aliases` with the default `siriusbusiness` entry when the config directory exists. Public aliases in `~/.hashtree/keys` are still accepted for compatibility, but `aliases` is the preferred place for read-only identities.

`git-remote-htree` automatically limits loose Git object download concurrency
based on the configured read path. Multi-server CDN-style reads default to 64
concurrent downloads, while a single read server or loopback-first local daemon
defaults to 16 to avoid overwhelming a direct Blossom origin. Override with
`HTREE_GIT_OBJECT_DOWNLOAD_CONCURRENCY=<n>` when benchmarking or tuning a
specific deployment.

## CLI

```bash
# Add content
htree add myfile.txt                    # Add file (CHK-encrypted, shareable)
htree add mydir/ --unencrypted          # Add directory as raw plaintext
htree add myfile.txt --publish mydata   # Add and publish to Nostr

# Mount a published tree or subdirectory locally
htree mount htree://self/mytree          # mounts to ./mytree and errors if it already exists
htree mount htree://npub1.../mytree ~/mnt/mytree
htree mount htree://npub1.../mytree/docs ~/mnt/docs

# Push to Blossom servers
htree push <hash>                       # Push to configured servers
htree push <hash> -s https://blossom.example.com  # Push to specific server

# Get/cat content
htree get <hash>                        # Download to file
htree cat <hash>                        # Print to stdout

# Pins
htree pins                              # List pinned content
htree pin <hash>                        # Pin content
htree unpin <hash>                      # Unpin content

# Nostr identity
htree user                              # Show npub
htree publish mydata <hash>             # Publish hash to npub.../mydata
htree release publish releases/hashtree v0.2.3 <cid-or-nhash>
htree follow npub1...                   # Follow user
htree following                         # List followed users
```

## Releases

Run these commands from the repository root. CLI artifacts are staged under
`rust/dist/` by `rust/scripts/release_to_htree.sh`.

Publish a new binary release while keeping older versions in the same mutable tree:

```bash
release="$(htree add rust/dist/hashtree-v<version> | awk '/^  url:/ {print $2}')"
rust/scripts/publish_release.sh v<version> "$release" releases/hashtree
```

That stores the new release under `v<version>/`, repoints `latest/` at the same CID, and leaves older versions intact.

For an existing release tree, pass `htree release publish ... --expected-root <previous-root-cid>`
with its verified previous root (a raw CID, including its key if encrypted). The command
preserves that tree even when no announcement is observed, rejects a conflicting
observed root, and stops if the required history cannot be read. Omitting the flag
allows creation of a new release tree.

Release announcements follow `nostr.event_transport`. For consumers using FIPS pubsub,
set it to `"fips-local-only"` and run a local `htree` daemon with the same configuration.
`htree release publish` resolves and hands its signed announcement to that daemon;
it fails if the daemon is unavailable, uses another transport, or cannot observe the
existing release root. Before migrating an existing release tree, submit its already
signed root event to the daemon's loopback `POST /api/nostr/events` endpoint. A quiet
network is not proof that the release tree is empty. Explicit `"relay"` mode continues
to use the configured Nostr relays.

The daemon retains accepted public root events in a separate, durable Hashtree index
and serves them to FIPS consumers after reconnects and restarts. This index is
independent of the disposable blob cache and holds at most 4,096 events. Configure
`nostr.retained_roots` with the author/tree keys to follow through open subscriptions
and periodic reconciliation. A public provider can explicitly set
`nostr.fips_pubsub_max_inbound_routed_peers = 16` to serve authenticated clients routed
through transit peers without listing each client identity. The default is `0`
(closed), even when roots are retained. These slots are reserved within the
adapter's total 64-peer budget; configured pubsub peers must fit the remainder.
Idle admissions expire. This setting grants no social trust and does not make
the provider dial unknown clients. Keep a provider online for late consumers, and verify
the release from a separate FIPS consumer, including after cache eviction and a
daemon restart, before rollout.

Public-provider releases also require the concurrent admission gate:
`python3 scripts/test_public_provider_admission.py --transit /path/to/discovery_transit_fixture --transit-sha256 <sha256> --output ../work/provider-admission`.
Build the FIPS core `discovery_transit_fixture` example from a source revision
containing the per-ingress, per-origin discovery fix. The gate keeps Hashtree's
locked provider/client libraries unchanged and uses that separate transit process
with the normal two-second discovery limit. Sixteen fresh routed clients must
receive the exact retained event within eight seconds, a seventeenth must be
denied, and the admitted clients must still read successfully. It repeats after a
provider restart and records binary hashes, elapsed time, and peak resident memory.
The ordinary tests retain a single-client replay/restart check; enabling the
`public-provider-stress` feature requires `HTREE_TEST_TRANSIT_BIN`, never silently
skips the concurrent gate, and does not change production behavior.

Publish the canonical repo release and mirror the same staged files to GitHub in one step:

```bash
./publish_release.sh --version v<version>
```

The checkout must be clean and `HEAD` must match the requested tag. The wrapper runs `scripts/release-gate.sh`, then wraps `rust/scripts/release_to_htree.sh`, reuses one staged release directory for both outputs, and keeps GitHub from drifting ahead of the hashtree/Homebrew publish path. Before bumping a Rust release version, add the matching `## <version> - YYYY-MM-DD` entry to [`rust/CHANGELOG.md`](CHANGELOG.md); staging now splices that entry into the published notes and fails if the version is missing.

Local publication also requires `IRIS_STACK_GATE_RECEIPT`, the successful [pinned Iris Stack process-gate receipt](https://github.com/irislib/iris-stack/blob/c6035a6343c569d480f407d7f47fc755cb825b64/docs/integration-lab.md) for this exact public Hashtree commit (`IRIS_STACK_HTREE_REV`). The receipt must match the lab and companion product pins and include both CPU and bandwidth samples. The hosted release workflow runs this gate itself before publishing artifacts; the local publisher reuses its matching receipt.

On macOS this builds the macOS CLI artifacts locally, builds the Linux musl CLI artifacts in target-native Alpine Docker containers, and auto-builds the Windows x64 CLI binaries from the configured Windows build host when available. Release builds resolve external Rust dependencies from the lockfile and do not copy sibling source repositories. You can still override the Windows input explicitly with `--windows-artifacts-dir <shared-dir>`, or skip the VM step with `--skip-windows-vm`.

To backfill or verify a tagged release from an exact source snapshot, point the builder at a separate checkout or worktree:

```bash
rust/scripts/build_release_artifacts.sh --version v<version> --repo-dir /path/to/tagged/checkout --linux-builder docker
```

If you want the canonical hashtree/Homebrew release without touching the GitHub mirror:

```bash
./publish_release.sh --version v<version> --skip-github
```

When the release directory includes the full macOS/Linux CLI archive set, the same script also updates the Homebrew tap at:

```bash
https://upload.iris.to/npub1xdhnr9mrv47kkrn95k6cwecearydeh8e895990n3acntwvmgk2dsdeeycm/homebrew-hashtree.git
```

Skip that step explicitly with:

```bash
./publish_release.sh --version v<version> --skip-homebrew-tap
```

If you also want the same command to publish the crates.io release, opt into the irreversible step explicitly:

```bash
./publish_release.sh --version v<version> --cargo-publish
```

## Development

Run these commands from `rust/`:

```bash
../scripts/release-gate.sh     # Full pre-publish Rust, TypeScript, and wiring gate
../scripts/release-gate.sh --fast # Compile Rust tests; run the faster checks
cargo nextest run --workspace  # Run Rust tests with bounded parallelism
cargo test -p hashtree-core    # Run core crate tests
cargo test --locked -p hashtree-core -p hashtree-lmdb -p hashtree-blossom -p hashtree-resolver --features hashtree-resolver/nostr --doc
cargo doc --locked -p hashtree-core -p hashtree-lmdb -p hashtree-blossom -p hashtree-resolver --features hashtree-resolver/nostr --no-deps --open
cargo bench -p hashtree-core   # Run core benchmarks
```

The release gate requires `cargo-nextest`. CI runs its `static`, `typescript`,
`rust`, `rust-peripheral`, and `fips` lanes on separate runners; the default
local invocation overlaps independent work and keeps FIPS out of the ordinary
workspace feature set.

## License

MIT



### Haps distribution

The final local release flow also publishes desktop/CLI packages using
`../haps-release.json` and the same verified `release.json` assets. Drafts do not
publish Haps packages. Install Haps with the `import-release` subcommand and Python
3.9+ on the publisher. Set `HAPS_KEY_FILE` to an existing secret-key file matching
the public publisher pinned in the mapping (or provision the matching existing
Haps identity). Never generate a new release identity. The flow verifies
checksums and package layout before upload, then requires Nostr acknowledgement.
A Haps failure fails the final release; byte-identical retries reuse the signed
package versions. Iris Git, native updaters and Haps consume the same release
artifacts; Android/iOS and native installer channels retain their existing gates.
