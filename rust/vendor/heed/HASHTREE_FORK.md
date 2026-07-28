# Hashtree heed fork

This package preserves heed 0.20.5 behavior and adds caller-pinned LMDB
`data.mdb`/`lock.mdb` identities for the Pool migration authority path. Every
LMDB consumer linked into `htree`, including the social graph backend, resolves
this package under the dependency alias `heed`. Mixing it with upstream
`heed` would link two unprefixed LMDB C archives into one process.

Publish `hashtree-lmdb-master-sys`, then this crate, then the
`hashtree-nostr-social-graph-heed` adapter and `hashtree-lmdb`. Do not replace
the dependency with upstream `heed` until upstream exposes an equivalent
pre-side-effect exact-file-open contract.
