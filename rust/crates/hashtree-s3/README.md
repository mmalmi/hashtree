# hashtree-s3

S3 storage backend for hashtree with non-blocking uploads.

Store hashtree blobs in Amazon S3 or compatible object storage (MinIO, R2, etc.).

## Usage

```rust
use hashtree_s3::S3Store;
use hashtree_core::Store;

let store = S3Store::new(
    "my-bucket",
    "us-east-1",
    Some("https://s3.example.com"),  // Optional custom endpoint
).await?;

// Store a blob
store.put(&hash, &data).await?;

// Retrieve a blob
let data = store.get(&hash).await?;
```

## Features

- Non-blocking async uploads
- Compatible with S3-compatible services (MinIO, Cloudflare R2, etc.)
- Optional feature in hashtree-cli: `cargo install hashtree-cli --features s3`

Part of [hashtree-rs](https://github.com/mmalmi/hashtree-rs).
