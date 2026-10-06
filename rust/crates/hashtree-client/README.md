# hashtree-client

Verified public Hashtree reads for applications. Reuses an existing local htree
daemon's signed-event relay and raw-blob endpoint; otherwise resolves signed roots
with `hashtree-resolver` and fetches raw content from configured Blossom servers.
No CLI subprocess, account, new identity, or running daemon is required.

```rust,no_run
use hashtree_client::{Client, ClientConfig, Reference};
use std::path::Path;

# async fn example() -> anyhow::Result<()> {
let client = Client::new(ClientConfig::from_env()?, Path::new("cache"))?;
let reference = Reference::parse("htree://npub1.../packages")?;
let root = client.resolve(&reference).await?;
let bytes = client.read_file(&root, "catalog.json", 1024 * 1024).await?;
# Ok(())
# }
```

The first URL path segment is the mutable tree name. Percent-encoded slashes
belong to that name; later segments select a directory within the tree.
Public-key identity and root signatures are verified locally. Every raw block
is size-bounded and SHA-256 verified before use and caching. Directory traversal
and file assembly happen in the client; servers only need to serve raw content.
Applications should also enforce their own signer pins and rollback policy.

Resolution keeps a Nostr subscription open for the configured observation window,
including after EOSE. A quiet window is a timeout, not proof of absence. The
returned CID pins a snapshot for the caller's operation. Slow blob sources are
hedged; the first valid content response wins. Errors/timeouts remain errors
when no source succeeds, rather than being converted to missing content.

Configuration is read from `hashtree-config`, respecting `HTREE_CONFIG_DIR`.
Reading configuration does not create it. `HTREE_DAEMON_URL` selects a loopback
HTTP origin, otherwise the shared config's server port is used.
`HTREE_PREFER_LOCAL_DAEMON=0` disables local reuse;
`HTREE_LOCAL_DAEMON_ONLY=1` forbids external fallback. `NOSTR_RELAYS` overrides
standalone relay URLs as a comma-separated list. Callers may instead provide
`ClientConfig` explicitly.

The caller supplies a cache directory. This client never opens the daemon's
database, starts a service, changes its configuration, or removes shared pins.
Daemon cache entries may be evicted; caller-owned cached/installed bytes remain.
