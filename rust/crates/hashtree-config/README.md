# hashtree-config

Shared configuration for hashtree tools.

Provides configuration loading and key management used by hashtree-cli and git-remote-htree.

## Configuration File

`~/.hashtree/config.toml`:

```toml
[blossom]
read_servers = ["https://cdn.iris.to"]
write_servers = ["https://upload.iris.to"]
max_upload_mb = 100
upload_concurrency = 10

[nostr]
relays = [
    "wss://relay.damus.io",
    "wss://relay.snort.social"
]

[server]
enable_webrtc = true
public_writes = false

[sync]
enabled = true
sync_own = true
sync_followed = true
```

## Keys File

`~/.hashtree/keys`:

```
nsec1abc123... default
nsec1xyz789... work
```

Part of [hashtree-rs](https://git.iris.to/#/npub1xdhnr9mrv47kkrn95k6cwecearydeh8e895990n3acntwvmgk2dsdeeycm/hashtree).
