# hashtree-cli

Hashtree daemon and CLI - content-addressed storage with P2P sync.

## Installation

```bash
# CLI + daemon (default cargo features; FUSE is optional)
cargo install hashtree-cli

# Add mount support explicitly when you want it
cargo install hashtree-cli --no-default-features --features p2p,lmdb,fuse

# Without social graph
cargo install hashtree-cli --no-default-features --features p2p

# Minimal install without P2P or social graph
cargo install hashtree-cli --no-default-features

# Cashu wallet helper for `htree cashu ...`
cargo install hashtree-cashu-cli
```

For cargo installs, `fuse` is opt-in. Building with `--features p2p,lmdb,fuse` needs platform FUSE headers/libs:

- Linux: typically `pkg-config` plus `libfuse3-dev` (or the distro equivalent).
- macOS: install macFUSE first.

Prebuilt macOS release binaries intentionally omit FUSE mount support so `htree` still runs on machines without macFUSE installed. Build from source with `--no-default-features --features p2p,lmdb,fuse` if you need `htree mount` on macOS. Linux release binaries keep FUSE mount support, and Windows builds do not ship it.

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

## Social Graph

The daemon maintains a local social graph store. On startup it crawls follow lists (kind 3) from Nostr relays and uses follow distance to control write access to your Blossom server, without a manual allow-list for people in your social circle.

The social graph API is available at `/api/socialgraph/distance/:pubkey`.

New identities also seed a local bootstrap follow to `npub1xdhnr9mrv47kkrn95k6cwecearydeh8e895990n3acntwvmgk2dsdeeycm` so the graph starts with a usable entrypoint. This writes `contacts.json` locally and is not auto-published.

## Configuration

Config file: `~/.hashtree/config.toml`

```toml
[blossom]
read_servers = ["https://cdn.iris.to", "https://hashtree.iris.to"]
write_servers = ["https://hashtree.iris.to"]

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
