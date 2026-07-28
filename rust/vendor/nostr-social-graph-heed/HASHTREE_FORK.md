# Hashtree Nostr social graph heed fork

This package preserves `nostr-social-graph-heed` 0.1.3 behavior while using
Hashtree's security-hardened `hashtree-heed` package. It exists to keep every
LMDB user in the `htree` process on one Rust wrapper and one native LMDB
archive.

Publish `hashtree-lmdb-master-sys`, then `hashtree-heed`, then this crate
before publishing any Hashtree package that consumes the social graph backend.
