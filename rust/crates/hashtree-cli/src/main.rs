//! Hashtree CLI and daemon.
//!
//! Usage:
//!   htree start [--addr 127.0.0.1:8080] [--daemon]
//!   htree reload [--pid-file <path>]
//!   htree stop [--pid-file <path>]
//!   htree add <path> [--only-hash] [--unencrypted] [--no-ignore] [--publish <ref_name>]
//!   htree load <cid>
//!   htree get <cid> [-o output]
//!   htree cat <cid>
//!   htree pins
//!   htree pin <cid>
//!   htree unpin <cid>
//!   htree info <cid>
//!   htree stats
//!   htree gc
//!   htree user [<nsec>]
//!   htree publish <ref_name> <hash> [--key <key>]
//!   htree release publish <tree_name> <version_path> <cid> [--local]

mod app;

use anyhow::Result;

#[tokio::main]
async fn main() -> Result<()> {
    app::run().await
}
