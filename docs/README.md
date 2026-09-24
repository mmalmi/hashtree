# Hashtree documentation

## Build with Hashtree

- [TypeScript quickstart](../ts/GETTING_STARTED.md), [SDK packages](../ts/README.md), and [API reference](../ts/API.md)
- [CLI installation and usage](../rust/README.md) and [Rust library](../rust/crates/hashtree-core/README.md)
- [Browser worker and app runtime](../ts/packages/hashtree-worker/README.md)
- [Mobile FFI](../rust/crates/hashtree-ffi/README.md): UniFFI bindings for Kotlin/Swift attachment operations; native Rust apps can use the crates directly

## Architecture and protocols

- [Visual architecture](architecture.html): content, routing, and transport layers
- [Core protocol](HTS-01.md): hashing, chunking, tree encoding, and encryption (draft specification)
- [Networking](NETWORKING.md): implemented discovery, routing, and blob protocol
- [URL encoding](URL-ENCODING.md): routing with slash-containing tree names
- [Ad hoc mesh configuration](ADHOC_MESH.md): relayless blob and Nostr paths
- [Performance](PERFORMANCE.md): read/write and ingress notes

## Development and releases

- [Rust development](../rust/README.md#development) and [binary releases](../rust/README.md#releases)
- [TypeScript development](../ts/README.md#development) and [npm publishing](../ts/PUBLISHING.md)
- [Rust changelog](../rust/CHANGELOG.md) and [TypeScript changelog](../ts/CHANGELOG.md)

To release the companion static sites to Cloudflare and Hashtree, run
`node ./scripts/release-sites.mjs` from the repository root. It expects sibling
`../iris-apps` and `../hashtree-cc` checkouts unless `IRIS_APPS_REPO_ROOT` and
`HASHTREE_CC_REPO_ROOT` override them.

## Related projects

The CLI, daemon, Git helper, and Rust crates live in `rust/`; the TypeScript SDK
lives in `ts/`. Apps are maintained in separate repositories:

- [iris-browser](https://git.iris.to/#/npub1xdhnr9mrv47kkrn95k6cwecearydeh8e895990n3acntwvmgk2dsdeeycm/iris-browser): native Tauri shell
- [iris-apps](https://git.iris.to/#/npub1xdhnr9mrv47kkrn95k6cwecearydeh8e895990n3acntwvmgk2dsdeeycm/iris-apps): portable web apps and isolated site runtime
- [hashtree-cc](https://git.iris.to/#/npub1xdhnr9mrv47kkrn95k6cwecearydeh8e895990n3acntwvmgk2dsdeeycm/hashtree-cc): landing page and file sharing app

## Design notes and plans

These documents describe proposed or evolving work; use the guides above for
current APIs and behavior.

- [Hashtree on FIPS](hashtree-on-fips.md)
- [Blossom reconciliation and large fetches](blossom-reconciliation-and-large-fetch-plan.md)
- [Git fetch/push improvements](git-repo-fetch-push-improvement-plan.md)
- [Torrent bridge](torrent-bridge-plan.md)
- [Static-site deployment platform](vercel-on-hashtree-plan.md)
