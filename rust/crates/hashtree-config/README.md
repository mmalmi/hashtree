# hashtree-config

Shared configuration for hashtree tools.

Provides configuration loading and key management used by hashtree-cli and git-remote-htree.

## Configuration File

`~/.hashtree/config.toml`:

```toml
[blossom]
read_servers = ["https://cdn.iris.to", "https://hashtree.iris.to"]
write_servers = ["https://hashtree.iris.to"]
max_upload_mb = 100

[nostr]
relays = [
    "wss://relay.damus.io",
    "wss://relay.snort.social",
    "wss://nos.lol"
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

Part of [hashtree-rs](https://files.iris.to/#/npub1xndmdgymsf4a34rzr7346vp8qcptxf75pjqweh8naa8rklgxpfqqmfjtce/hashtree).
