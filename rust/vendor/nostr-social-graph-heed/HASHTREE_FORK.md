# Hashtree Nostr social graph heed fork

This package preserves `nostr-social-graph-heed` 0.2.1 behavior, including persisted
follow and mute list revisions, while using
Hashtree's security-hardened `hashtree-heed` package. It exists to keep every
LMDB user in the `htree` process on one Rust wrapper and one native LMDB
archive.

Publish `hashtree-lmdb-master-sys`, then `hashtree-heed`, then this crate
before publishing any Hashtree package that consumes the social graph backend.

## Identity migration

This release incorporates the upstream atomic alias migration. Valid hexadecimal
public keys use one lowercase identity; nonhex identifiers retain their spelling.
The first writable open merges legacy aliases by the follow/mute revision rules,
rebuilds reverse edges and distances, and commits the schema marker with the data.
Timestamped empty lists and both event IDs/completeness flags are preserved.
Ambiguous ownership, unresolved follow conflicts, unknown schemas and map-space
failures abort the transaction without replacing personal history. Read-only
exports remain read-only and do not perform migration.

Upgrade all writers together and retain a backup for rollback. Existing old
writers must not continue writing after migration. The CLI opens this writable
backend before creating its in-memory distance cache, so its first export uses
the migrated indexes; normal root/list updates invalidate that cache as before.
Core `nostr-social-graph >=0.2.1` is required. The hardened `hashtree-heed` and
LMDB pins, including overlapping read-transaction support, are unchanged.
