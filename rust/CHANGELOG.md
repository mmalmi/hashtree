# Changelog

## 0.2.37 - 2026-04-22

Changes since the `0.2.35` crates.io release.

### Added

- Added tracked author mirroring plus end-to-end mirror coverage in `hashtree-cli`, so mirrored author trees and the renamed `htree mirror` flow are exercised against real daemon/test environments instead of only helper seams.
- Added an explicit embedded-daemon reload API for native hosts, making it possible for browser and app integrations to refresh transport and server settings without rebuilding the host-side bridge object.

### Improved

- Improved the embedded browser/native daemon runtime so host-managed relay, Blossom, and transport settings are honored consistently across startup and reload.
- Improved `git-remote-htree` repeated fast-forward pushes by hydrating existing remote objects from the cached hashtree root and enumerating only the Git-object delta instead of rewalking the full reachable history on every push.
- Improved the worker/mesh runtime with the current streaming/provider-bridge updates that keep the shared P2P stack aligned across the Rust and TypeScript hosts.

### Fixed

- Fixed repeated `git-remote-htree` fast-forward pushes so the helper can reuse cached remote objects and fall back cleanly when the delta set is incomplete instead of forcing a full-history local import on every push.

## 0.2.35 - 2026-04-20

Changes since the `0.2.34` crates.io release.

### Added

- Added a reciprocity-aware upload scheduler in `hashtree-network` so peers that have previously seeded us are preferred when outbound response bandwidth is contested.
- Added Blossom push helpers, storage compaction coverage, and broader social-graph/profile-index maintenance coverage across the CLI and Nostr crates.

### Improved

- Switched default read-side peer selection to weighted reply-likelihood ordering across the Rust and TypeScript mesh stacks.
- Improved CLI mirror, crawler, storage-maintenance, and relay-backed indexing behavior for larger profile/event histories.

### Fixed

- Fixed Rust package version wiring so the `0.2.35` workspace resolves cleanly from a fresh checkout.
- Fixed the worker/mesh upload path so capped WebRTC upload bandwidth is still enforced while reciprocal peers receive priority under contention.

## 0.2.34 - 2026-04-17

Changes since the `0.2.33` crates.io release.

### Added

- Added a hashtree-first app-building guide and library-side query surfaces for relay-backed event stores and collection sources, so apps can stream and query mutable data without rebuilding ad hoc Nostr HTTP layers.
- Added a portable PWA export helper plus broader live export coverage for manifest metadata, route metadata, and link-hinted assets.
- Added a Cashu send-payment helper and broader mirror/event-index maintenance improvements, including root republish support and expanded mirror history coverage.
- Added heavier media-oriented `hashtree-sim` transport scenarios plus a Tokio virtual-clock mode for `VirtualSteps`, so slow first-byte, throughput, and stall behavior can be tuned without waiting on wall-clock sleeps.

### Improved

- Improved mesh request handling so slow or hedged sources are tracked as timeouts instead of synthetic misses, making peer selection and retry behavior more stable on cold media reads.

### Fixed

- Fixed mutable root resolution so relay lookups only use the encoded tree name and resolve slash-separated subpaths inside the published tree instead of treating them as Nostr `d` tags.
- Fixed PWA export metadata preservation so released shells keep the intended manifest fields, route metadata, and asset links.
- Simplified the repo release pipeline by removing the obsolete sibling Iris packaging path from hashtree releases.
- Fixed mesh-store read accounting so late or slow responders no longer poison route quality as hard misses when the request was still in flight.

## 0.2.33 - 2026-04-13

Changes since the `0.2.32` crates.io release.

### Added

- Added `htree load` plus fetch progress reporting for loading published app/site payloads through the CLI.
- Added assist-mode `hash_get` support and an embedded daemon runtime for native hosts, making it easier to embed hashtree capabilities without a separate daemon process.
- Added reusable worker client/bootstrap surfaces so app hosts can embed the relay-backed worker runtime directly.

### Improved

- Improved adaptive mesh block scheduling and unified mesh query routing across the Rust and TypeScript stacks, reducing recursion and making upload/request prioritization more predictable.
- Neutralized and documented the generic worker runtime surface so app hosts can consume it without Iris-specific naming.

### Fixed

- Fixed Windows release verification flow during the cross-platform release pipeline.
- Fixed worker runtime issues around detached cached buffers and primary-store mesh recursion.

## 0.2.32 - 2026-04-10

Changes since the `0.2.31` crates.io release.

### Improved

- Added a real install-matrix smoke runner that exercises the documented install flow across the native host, Docker Linux targets, a Windows VM, and Homebrew with bounded Docker timeouts and per-platform logs where supported.
- The Windows VM artifact builder now stages the Rust workspace through a tar archive instead of bulk-copying the repo tree from the Parallels shared folder, which avoids the previous shared-folder copy failures on release builds.

### Fixed

- Fixed `git-remote-htree` on Linux environments where LMDB initialization returns `ENOSYS` by falling back to filesystem-backed local storage instead of failing the remote-helper startup.
- Improved the Windows VM smoke path to use the built-in `curl.exe` and `tar.exe` tools instead of the noisy `Invoke-WebRequest` path.
- Release publishing now runs the live install matrix after publishing the release tree and Homebrew tap, but only warns on failures so it does not strand an otherwise published release.

## 0.2.31 - 2026-04-10

Changes since the `0.2.30` crates.io release.

### Added

- Added a shared `.collection-manifest.json` contract for published collection roots, so peers can inspect `schemaVersion`, item/projection formats, and optional schema references directly from the root.

### Improved

- Aligned Rust and TypeScript `hashtree-collection` / `hashtree-nostr` roots around the same published collection manifest metadata and added interop coverage that compares the actual emitted metadata payloads.

### Fixed

- Fixed release publishing safeguards so the generated installer is only exposed when the full archive set is present, and release validation now derives the canonical upload npub from the documented install command.
- Fixed macOS release builds on Apple Silicon hosts so Intel macOS archives can still be produced for the published release set.

## 0.2.30 - 2026-04-10

Changes since the `0.2.29` crates.io release.

### Improved

- Bootstrapped new local identities with a default social-graph entrypoint follow, so fresh `htree socialgraph` usage starts from a reachable root instead of an isolated local user. This bootstrap is local-only and can be disabled with `nostr.bootstrap_follows = []`.
- Seeded the default aliases file with `siriusbusiness` for the binary releaser npub and updated the Rust docs to describe the new alias/bootstrap behavior.
- Forced local contact-list files into the social-graph state when the CLI starts or opens the graph, so newly seeded follows appear in stats immediately without requiring a separate publish step.

## 0.2.29 - 2026-04-10

Changes since the `0.2.28` crates.io release.

### Added

- Added the new `hashtree-collection` crate to the published Rust crate graph, including collection index lifecycle support, search indexes, schema hooks, federated search helpers, and explicit reindex support.

### Improved

- Switched the Rust social-graph dependencies from pinned GitHub revisions to the published `nostr-social-graph` crates.io releases.
- Improved `htree socialgraph stats` so the root prints as an `npub` and the reachability count is labeled more explicitly.
- Trimmed the default relay set by removing `wss://offchain.pub`.

### Fixed

- Fixed `htree cat` for published tree paths by resolving file targets inside encrypted directories with their decrypt keys intact and by rejecting bare directory CIDs unless a file path is specified.

## 0.2.28 - 2026-04-07

Changes since the `0.2.27` crates.io release.

### Fixed

- Fixed shared LMDB cache pressure handling so bounded stores evict stale blobs on the write path instead of only after later cleanup passes. This keeps `git-remote-htree` and other shared-store writers from wedging and requiring a fresh `HTREE_DATA_DIR` workaround.
- Added a real `git-remote-htree` regression test that fills a bounded shared LMDB cache and verifies new tree writes evict stale entries instead of letting the cache grow past its budget.

## 0.2.27 - 2026-04-06

Changes since the `0.2.26` crates.io release.

### Fixed

- Fixed LMDB-backed local stores so configured size increases actually grow the map instead of leaving `git-remote-htree` and other bounded stores stuck at the default 10 GiB mapping.
- Fixed local eviction behavior around uploads and mutable tree refs: `htree add` now runs quota eviction before and after indexing, superseded published roots are unpinned when refs move, and `git-remote-htree` evicts stale local blobs before building new trees so full stores recover without manual cleanup.
- Fixed `hashtree-cli` test coverage for the current peer-router state shape so targeted CLI test runs compile cleanly again.

## 0.2.26 - 2026-04-06

Changes since the `0.2.25` crates.io release.

### Improved

- Moved the production mesh runtime, transport orchestration, peer/session logic, and Cashu-aware request plumbing out of `hashtree-cli` and into `hashtree-network`, so Bluetooth, multicast, Wi-Fi Aware, and WebRTC now sit behind the shared mesh runtime instead of a CLI-owned stack.
- Tightened release coverage by wiring a real Rust E2E smoke job into CI and by keeping the offline LAN Docker verification path aligned with the locked Rust dependency graph used for releases.

## 0.2.25 - 2026-04-04

Changes since the `0.2.24` crates.io release.

### Improved

- Clarified `htree mount` semantics so the first argument is always a published hashtree target, while explicit mountpoints can reuse an existing empty directory or create a missing one for stable Drive-style folders such as `~/Hashtree`.

### Fixed

- Fixed `htree mount` path handling by rejecting filesystem-like targets that previously triggered ambiguous local publish behavior, and by refusing explicit non-empty mount directories that would otherwise hide user files behind the FUSE mount.

## 0.2.24 - 2026-04-04

Changes since the `0.2.23` crates.io release.

### Improved

- Improved `htree mount` path handling so missing absolute mount paths are created automatically and long-lived mounts warn when they are placed under temporary directories.

### Fixed

- Fixed mounted FUSE filesystems on macOS so Finder writes succeed more reliably and `statfs` now reports usable disk and inode availability instead of a zero-capacity filesystem.
- Fixed `hashtree-cli` test coverage for non-`fuse` builds by keeping fuse-only test imports gated behind the `fuse` feature.

## 0.2.23 - 2026-04-03

Changes since the `0.2.22` crates.io release.

### Improved

- Improved mount lifecycle behavior so `htree mount` waits for a live FUSE session before advertising, republishing mounted roots preserves sibling entries, and remounting stale targets recovers more predictably.
- Improved Linux release engineering by building FUSE-enabled artifacts and smoke coverage inside privileged Docker, which matches the shipped Linux release environment more closely.

### Fixed

- Fixed nested Git ref export in `git-remote-htree`, so published `.git` trees materialize branch paths correctly for viewers and dumb-HTTP clones.
- Fixed CI and non-`fuse` test coverage so `hashtree-cli` keeps fuse-only imports gated correctly and relayless mesh integration tests no longer pick up unrelated multicast peers from parallel jobs.

## 0.2.22 - 2026-04-03

Changes since the `0.2.20` crates.io release.

### Improved

- Improved `htree mount` ergonomics: mountpoints can be derived from targets automatically, owner aliases or hex pubkeys are normalized to `npub` targets before resolution, and mounted-root publishing preserves sibling entries while reducing redundant publishes.
- Added `htree mounts` plus an active-mount registry so running mounts can be listed from the CLI in human or JSON form.
- Kept `fuse` optional for crate consumers while still wiring release binaries and CI to build the mount-capable binaries explicitly, and split oversized server and git-remote helper modules into smaller internal components.

### Fixed

- Fixed the FUSE adapter for current `fuser` callback signatures so mkdir, unlink, rmdir, rename, and write operations dispatch cleanly on newer builds.
- Fixed the embedded daemon and non-`p2p` build path so `hashtree-cli` continues to work with `--no-default-features` after the FUSE-defaults change.
- Added FUSE smoke coverage and active-mount lifecycle cleanup so mounted sessions are registered only while live and stale mount records are pruned automatically.

## 0.2.20 - 2026-04-01

Changes since the `0.2.18` crates.io release.

### Improved

- Extracted the shared mesh routing core into the Rust networking crates and aligned the CLI, simulation, and TypeScript interop paths around the same signaling model.
- Improved `git-remote-htree` and `hashtree-cli` pull-request interoperability, including better published-repo handling and PR listing coverage.
- Improved Iris release defaults on supported platforms so Bluetooth-enabled builds and app release packaging behave more consistently.

### Fixed

- Fixed Iris Android release networking and Zapstore publishing regressions that were blocking the release pipeline.
- Fixed several mobile Bluetooth startup and interoperability issues across the Rust networking stack.

## 0.2.18 - 2026-03-31

Changes since the `0.2.17` crates.io release.

### Improved

- Unified the shared mesh signaling path across `hashtree-cli`, `hashtree-sim`, and the TypeScript interop tests, so production and simulation now exercise the same router-layer protocol and hedged retrieval scheduling.
- Renamed the reusable networking crate from `hashtree-webrtc` to `hashtree-network`, which better matches its scope: mesh routing, signaling, peer links, and store composition rather than just transport bindings.

### Fixed

- Fixed Rust ignored integration and doctest coverage so the full workspace, including network-gated tests and cross-language WebRTC checks, passes from the merged `master` tip.

## 0.2.17 - 2026-03-31

Changes since the `0.2.16` crates.io release.

### Fixed

- Fixed GitHub CI by refreshing the shared TypeScript lockfile and removing lint failures that had started failing the `ts` workflow.
- Fixed GitHub Iris desktop release builds by removing the cross-app `apps/iris` import on `iris-files` source-only TypeScript config and assets.
- Fixed flaky Rust workspace failures in multicast root queries and embedded daemon integration tests, so the GitHub Rust and release workflows complete reliably.

## 0.2.16 - 2026-03-31

Changes since the `0.2.15` crates.io release.

### Improved

- Unified slash-containing mutable tree paths across the Rust server, service-worker-facing helpers, and release publishing flow, so repo releases and encoded `htree` paths resolve consistently.
- Improved embedded daemon and background Nostr service behavior with cleaner shutdown, better default Blossom fallback handling, and broader profile-index/social-graph coverage.
- Improved the native Iris shell transport stack with more robust Bluetooth session handling, origin-isolated `htree` child webviews, and tighter user-facing deep-link/PWA path handling.

### Fixed

- Fixed release publishing defaults to use repo-scoped `releases/<repo>` trees and removed the obsolete standalone `hashtree-nostr-bridge` crate by merging its crawler into `hashtree-nostr`.
- Fixed Rust test hangs caused by embedded daemon background services outliving Tokio runtime shutdown.

## 0.2.15 - 2026-03-31

Changes since the `0.2.14` crates.io release.

### Added

- Added signed tree snapshot permalinks in the Nostr stack, giving published trees a stable signed snapshot path for linking and sharing.
- Added the `cashu-service` crate to carry the shared Cashu helper and wallet primitives used by `htree` and `htree-cashu`.
- Added dumb-HTTP Git metadata export in `git-remote-htree`, so static HTTP gateways can serve cloneable taps and repositories from published `.git` trees.

### Improved

- Bulk-built Nostr event indexes and improved steady-state ingest behavior in `hashtree-nostr`, reducing indexing overhead on larger relay or mirror workloads.
- Expanded social-graph tooling in `htree` with profile-index rebuild support and author allowlist URL input for indexing jobs.
- Restored Bluetooth/Nostr publish receipts, capped receipt logs, and cleaned up BLE polling and routing behavior in the daemon transport path.
- Automated Homebrew tap publication from the Rust release flow, with an explicit opt-in path for chaining crates.io publishing from the same release command.

## 0.2.14 - 2026-03-28

Changes since the `0.2.13` crates.io release.

### Added

- Added the new `hashtree-merge` crate with deterministic path-based merge primitives and wired it into the publish chain.
- Added `htree repos` and improved repo listing/source-link handling for hashtree-first repositories.
- Added offline LAN multicast signaling, Bluetooth mesh transport work, Wi-Fi Aware nearby-bus scaffolding, and transport usage tracking in the daemon/CLI stack.

### Improved

- Switched the local Nostr relay to B-tree-backed event indexes, split trusted public and ambient indexes, and added faster planning for `ids`, authors, kinds, replaceables, parameterized replaceables, and tag queries.
- Improved Nostr relay correctness coverage for filter matching, limits, `COUNT`, replaceables, `since`/`until`, and search behavior.
- Batched Nostr ingest writes through buffered store flushes and LMDB batch commits, substantially reducing publish-side write amplification.
- Improved cold root resolution, daemon root handling, filesystem blob sharding, and LMDB quota/default-store behavior.
- Tightened git publish ordering so roots are published after blob upload and improved progress/reporting in `git-remote-htree`.

### Fixed

- Fixed publish blockers in the Rust crate graph and aligned published repository/homepage links with the current hashtree remote.
- Fixed several Bluetooth/native relay startup and htree loading issues in the Rust networking stack.
- Fixed Nostr manifest/index handling around by-id compatibility transitions and relay query edge cases found by the new tests.
