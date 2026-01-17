# hashtree-blossom

Blossom protocol client for hashtree - upload/download blobs with NIP-98 auth.

[Blossom](https://github.com/hzrd149/blossom) is a simple blob storage protocol. This crate provides a client for uploading and downloading blobs to Blossom servers.

## Features

- Upload blobs with NIP-98 authentication
- Download blobs by SHA256 hash
- Check blob existence
- List blobs by pubkey

## Usage

```rust
use hashtree_blossom::BlossomClient;
use nostr::Keys;

let keys = Keys::generate();
let client = BlossomClient::new(keys);

// Upload
let hash = client.upload("https://blossom.example.com", &data).await?;

// Download
let data = client.download("https://blossom.example.com", &hash).await?;

// Check existence
let exists = client.has("https://blossom.example.com", &hash).await?;
```

Part of [hashtree-rs](https://github.com/mmalmi/hashtree-rs).
