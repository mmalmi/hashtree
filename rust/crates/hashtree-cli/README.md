# hashtree-cli

Hashtree daemon and CLI - content-addressed storage with P2P sync.

## Installation

```bash
# CLI + daemon (default cargo features; FUSE is optional)
cargo install hashtree-cli

# Add mount support explicitly when you want it
cargo install hashtree-cli --no-default-features --features lmdb,fuse

# Without default LMDB storage
cargo install hashtree-cli --no-default-features

# Minimal install with LMDB storage
cargo install hashtree-cli --no-default-features --features lmdb

# Cashu wallet helper for `htree cashu ...`
cargo install hashtree-cashu-cli
```

For cargo installs, `fuse` is opt-in. Building with `--features lmdb,fuse` needs platform FUSE headers/libs:

- Linux: typically `pkg-config` plus `libfuse3-dev` (or the distro equivalent).
- macOS: install macFUSE first.

Prebuilt macOS release binaries intentionally omit FUSE mount support so `htree` still runs on machines without macFUSE installed. Build from source with `--no-default-features --features lmdb,fuse` if you need `htree mount` on macOS. Linux release binaries keep FUSE mount support, and Windows builds do not ship it.

## Commands

```bash
# Add content
htree add myfile.txt                    # Add file (CHK-encrypted, shareable)
htree add mydir/ --unencrypted          # Add directory as raw plaintext
htree add myfile.txt --publish mydata   # Add and publish to Nostr

# Push to Blossom servers
htree push <hash>                       # Push to configured servers

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
htree follow npub1...                   # Follow user
htree following                         # List followed users

# Daemon
htree start                             # Start P2P daemon
htree start --daemon                    # Start in background
htree start --daemon --log-file /var/log/hashtree.log
htree reload                            # Reload config by restarting background daemon
htree stop                              # Stop background daemon
htree status                            # Check daemon status

# FUSE mount
htree mount htree://self/mytree          # mounts to ./mytree and errors if it already exists
htree mount htree://npub1.../mytree ~/mnt/mytree
htree mount htree://npub1.../mytree/docs ~/mnt/docs
```

## Resuming an archived Nostr index

```bash
htree --data-dir ./archive nostr-index catch-up \
  --root <original-nhash> --authors-file ./authors.txt --since <unix-seconds> \
  --relay wss://relay.example --kind 1 --kind 5 --max-authors-per-run 16
```

Choose an initial `--since` at or before the original fetch pass began, not its completion date. The ordered author file, original root, initial time, kinds, and required relays identify the continuation. All required sources must finish each author before its new root and author cursor are committed to `nostr-index/catchup-state.json`. The original crawl state remains intact. Larger resource limits can resume a stopped author without discarding earlier progress.

Omitting `--until` resumes the saved end of an unfinished pass. After every author completes, the next invocation captures a new end time and revisits `--overlap-secs` before the previous end (default: 86400), never earlier than the original `--since`. This catches delayed arrivals while retaining all accumulated history and deduplicating repeated IDs. The overlap can be increased on resume; an unfinished pass keeps its captured interval and uses the increase on its next pass. An explicit `--until` can fix the pass end for controlled runs. Timeouts, resource limits, and ambiguous capped timestamp ties stop the run without advancing that author. Coverage describes the configured relays' EOSE responses; it cannot establish absence of events on other relays or detect every undisclosed relay omission.

The command retains prior-root blocks and only updates index paths touched by incoming events. It does not publish a pointer. After the previous root is already available on the destination, `htree push <new-root> --previous-root <retained-root>` uploads a DAG delta. This explicit delta mode fails if comparison nodes are unavailable instead of falling back to a whole-archive walk. Keep advertised and rollback roots readable until publication and retention checks finish.

For a proven historical kind-1 event that remains in the author-kind-time index but is missing from by-ID, `nostr-index repair-id` accepts one original signed event object. It requires `--root`, `--event`, `--expected-id`, the exact input and checkpoint byte hashes (`--expected-event-sha256` and `--expected-checkpoint-sha256`), and a new `--receipt` path whose parent exists. It checks the original stored body, holds the crawl lock, applies the physical write guard (`--min-free-bytes`, default 10 GiB), and force-syncs the repaired blocks before writing a no-replace receipt. It retains old roots and leaves catch-up state, latest-root files, and publication unchanged. An operator must separately validate and record the receipt's root transition before resuming from it.

## Social Graph

The daemon maintains a local social graph store. On startup it crawls follow lists (kind 3) from Nostr relays and uses follow distance to control write access to your Blossom server, without a manual allow-list for people in your social circle.

The social graph API is available at `/api/socialgraph/distance/:pubkey`.

New identities also seed a local bootstrap follow to `npub1xdhnr9mrv47kkrn95k6cwecearydeh8e895990n3acntwvmgk2dsdeeycm` so the graph starts with a usable entrypoint. This writes `contacts.json` locally and is not auto-published.

## Configuration

Config file: `~/.hashtree/config.toml`

```toml
[blossom]
read_servers = ["https://cdn.iris.to"]
write_servers = ["https://upload.iris.to"]
replicate_servers = ["http://192.0.2.20:8080"] # optional write-behind targets for accepted server uploads
replicate_queue_mb = 512

[server]
fips_peers = [
  { npub = "npub1...", udp_addresses = ["udp:192.0.2.10:2121"] }
] # optional static Hashtree FIPS origin/cache peers

[nostr]
relays = ["wss://relay.damus.io", "wss://relay.snort.social"]
socialgraph_root = "npub1..."         # defaults to own key
bootstrap_follows = [
  "npub1xdhnr9mrv47kkrn95k6cwecearydeh8e895990n3acntwvmgk2dsdeeycm"
]                                   # local-only seed for new identities; set to [] to opt out
social_graph_crawl_depth = 2          # BFS depth for social graph crawl
mirror_max_follow_distance = 2        # optional; defaults to social_graph_crawl_depth
max_write_distance = 3                # max follow distance for write access
negentropy_only = false         # require NIP-77 relays for mirror history sync
history_sync_on_reconnect = true
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

Part of [hashtree-rs](https://git.iris.to/#/npub1xdhnr9mrv47kkrn95k6cwecearydeh8e895990n3acntwvmgk2dsdeeycm/hashtree).
