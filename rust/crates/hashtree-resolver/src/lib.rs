//! Mutable names for immutable Hashtree content identifiers.
//!
//! The [`RootResolver`] trait supports one-shot lookups, open subscriptions, and
//! publishing. Enable the `nostr` feature for the Nostr implementation and its
//! installation, resolution, and publishing guide in the `nostr` module.
//!
//! A lookup that returns `None` is not proof of absence on a network. Live apps
//! should keep subscriptions open and call [`RootResolver::stop`] on shutdown.

mod traits;

#[cfg(feature = "nostr")]
pub mod nostr;

pub use traits::*;

// Re-export nostr-sdk types for use in NostrResolverConfig
#[cfg(feature = "nostr")]
pub use nostr_sdk::prelude::{Event, Keys, ToBech32};
