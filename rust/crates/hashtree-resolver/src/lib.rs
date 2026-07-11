//! Root resolver for hashtree - maps human-readable keys to merkle root hashes
//!
//! This crate provides the `RootResolver` trait and implementations for different
//! backends (Nostr, DNS, HTTP, local storage, etc.)
//!
//! # Overview
//!
//! A root resolver maps mutable human-readable keys to immutable content-addressed
//! merkle root hashes. This allows updating what content a key points to while
//! keeping the underlying data immutable.
//!
//! Key format is implementation-specific:
//! - Nostr: "npub1.../treename"
//! - DNS: "example.com/treename"
//! - Local: "local/mydata"
//!
//! # Example
//!
//! ```rust,ignore
//! use hashtree_resolver::{RootResolver, ResolverEntry};
//!
//! async fn example(resolver: impl RootResolver) {
//!     // One-shot resolve
//!     if let Some(cid) = resolver.resolve("npub1.../mydata").await.unwrap() {
//!         println!("Found cid: {}", cid);
//!     }
//!
//!     // Subscribe to updates (returns a channel receiver)
//!     let mut rx = resolver.subscribe("npub1.../mydata").await.unwrap();
//!     while let Some(cid) = rx.recv().await {
//!         println!("Updated cid: {:?}", cid);
//!     }
//! }
//! ```

mod traits;

#[cfg(feature = "nostr")]
pub mod nostr;

pub use traits::*;

// Re-export nostr-sdk types for use in NostrResolverConfig
#[cfg(feature = "nostr")]
pub use nostr_sdk::prelude::{Event, Keys, ToBech32};
