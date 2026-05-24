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
//!   htree release publish <tree_name> <version_path> <cid> [--draft] [--local]
//!
//! `--draft` writes the sibling `draft` pointer; final publishes write `latest`.

#![allow(unexpected_cfgs)]

mod app;

use anyhow::Result;
use std::env;

const DEFAULT_MAX_BLOCKING_THREADS: usize = 64;
const MAX_BLOCKING_THREADS_ENV: &str = "HTREE_MAX_BLOCKING_THREADS";
const WORKER_THREADS_ENV: &str = "TOKIO_WORKER_THREADS";

fn env_usize(name: &str) -> Option<usize> {
    env::var(name)
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|value| *value > 0)
}

fn main() -> Result<()> {
    let mut runtime = tokio::runtime::Builder::new_multi_thread();
    runtime.enable_all();

    if let Some(worker_threads) = env_usize(WORKER_THREADS_ENV) {
        runtime.worker_threads(worker_threads);
    }
    runtime.max_blocking_threads(
        env_usize(MAX_BLOCKING_THREADS_ENV).unwrap_or(DEFAULT_MAX_BLOCKING_THREADS),
    );

    runtime.build()?.block_on(app::run())
}
