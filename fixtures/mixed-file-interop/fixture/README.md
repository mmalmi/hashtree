# Mixed file-transfer fixture — preparation only

This packet prepares a fresh application-level interoperability test. It has not
been compiled or run. Existing TCP/FIPS interop and each language's file-transfer
unit tests do not establish the results sought here.

## Exact implementations

- Rust is the published `nvpn-fips-file-transfer = 0.1.5` crate, checksum
  `839740e5f304745a22990aada37eeb5b6a472046fb30948aa8e08455ba50bf1f`,
  from source `942293010595634a04e76d7fa38f6c4c736fd1ab`. The fixture has
  no path override or copied Rust file-transfer implementation.
- TypeScript is the qualified package output for `@fips/file-transfer 0.1.1`.
  Preparation currently binds source `8c7290e63bce040a318c941ff8e1a1dc9a1409e3`.
  Root accepted this SDK phase: 27 green tests, the expected red shutdown
  counterexample, shared vectors, build and all package members. The exact
  package SHA-256 is `fd40ec68ea586d2adf460859ce86794c33b39af5b8f44cfec7d03d851bafc9ef`.
  Mixed live interoperability remains a separate required gate.
  No unbuilt candidate source is substituted for an installed package.
- Both sides verify the shared vector file checksum
  `39254c05fb2a2dc9bc4b6dd21e775f79c25f9da089685b62254f6afdde099cf9`.
  Rust verifies public manifest/control codecs; TS verifies public manifest,
  prefix and control codecs. These checks supplement the live transfers.

## Transport and process boundary

A bounded Node loopback WebSocket seed runs a real FIPS node with forwarding.
Two separate child processes use the actual public libraries: Rust
`FipsEndpoint` plus the published transfer runtime, and TS `FipsNode` plus
`DirectFileTcp`. WebSocket frames carry authenticated FIPS sessions and service
datagrams. The fixture does not transport file contents through IPC, HTTP,
shared storage, or the existing TCP-only driver. Each child independently
generates deterministic source/expected bytes; their state directories differ.

The seed binds only `127.0.0.1`, with eight connections, 66 KiB frames, bounded
socket buffering, and a 128 MiB incoming-traffic ceiling per scenario. Rust
uses only the explicit local seed, disables LAN/Nostr discovery and system
network integration, and creates no TUN device. The admitted execution wrapper
must independently deny external networking and inspect the namespace before
and after execution. No new production defaults are proposed.

## Four required scenarios

1. Rust sender to TS receiver, complete v2 transfer.
2. Rust sender to TS receiver, interrupt at a saved prefix and restart both.
3. TS sender to Rust receiver, complete v2 transfer.
4. TS sender to Rust receiver, interrupt at a saved prefix and restart both.

Each sends three files: deterministic binary bytes, an empty file, and a UTF-8
filename with text. Complete cases use 400,123 binary bytes; interrupted cases
use 2,097,275. The receiver independently compares every byte and SHA-256; both
transfer engines must reach application-level completion. TCP acknowledgments
alone cannot pass any scenario.

The TS fixture closes a datagram gate after a saved incoming prefix or a bounded
outgoing stream prefix. This deliberately loses transport resets without
changing file-transfer logic. Both engines then shut down gracefully. Rust
drops the final public command sender and waits for the published runtime's
checkpointing exit; TS awaits `dispose()`. TS must report paused, never failed
or cancelled, and must not abort its retained sink.

Both actual child processes exit and are recreated with the same fixture-only
identities and separate on-disk stores. Durable partial bytes and claimed state
must survive. Each sender must emit acceptance: the TS sender event carries the
exact saved offset; for the Rust sender, the fixture also passively observes its
authenticated Ready control with that exact offset (bounded first-frame observation). After completion both processes restart again: completed claims and
Rust snapshots must remain completed, and receiver bytes/hash/mtime must remain
unchanged. No persisted claim is edited to manufacture a recovery.

## Provenance and resource plan

`preparation.json` binds source files, vector bytes, crate bytes, source commits,
line limits and canonical Git/index preservation. `rust/Cargo.lock` derives
from the published transfer crate's embedded lock: registry checksums and
reachable dependency edges remain unchanged; its root-only dev dependency is
excluded, the transfer crate becomes a checksum-bound registry dependency,
and the private fixture becomes the root. This is static closure preparation;
Cargo has not yet verified frozen acceptance of the derived lock.

Reuse the root-reviewed system recipe carrier
`8e9396e667671d3527c2a7aaeeda0c899335988f`, prefix
`fixtures/pubsub-wire-fixture/`. Its pinned Rust 1.99.0 Bookworm base, signed
Debian metadata and eight-package offline closure are shared; do not resolve
apt independently. Preserve the recipe's corrected read-only input mounts and
dedicated writable output mount. The derived image ID and compiler fingerprints
must be recorded per run; no binary-equivalence claim is implied.

Proposed execution is serialized: one Cargo job, incremental disabled, debug
information disabled, two CPUs, 4 GiB memory, 512 PIDs, and a 600-second outer
deadline with owned process-group cleanup. Runtime files stay below 64 MiB;
artifact retention must keep all failures and bounded stderr logs. Build cache
growth and package fetch storage need the execution owner's measured admission.

The inert hosted controller now binds and checks:

- the root-accepted TS package provenance, all 24 members and approved source;
- a ten-package Node lock derived from accepted Chat records, with only the
  qualified file-transfer archive changed; frozen installation is mandatory;
- Node 26.13.0 extracted from the accepted Chat image, matching its accepted
  executable hash; `ldd` dependencies and WebCrypto execution on Bookworm must
  pass before any installation or fixture run;
- locked Cargo fetch, every cached crate checksum, offline one-job build,
  compiled-package metadata, ELF architecture and exact fixture binary hash;
- a sealed run-input JSON hash, external network denial before/after runtime,
  child-OOM sampling, process cleanup and immutable dependency checks afterward.

The workflow reserves 20 minutes for compilation and a separate 600 seconds for
runtime. It requires 30 GiB at entry and a 20 GiB operating floor. Frozen lock,
runtime compatibility or compilation failures retain evidence and stop;
there is no unfrozen or alternate-runtime fallback. Eight pure local controls
verified shutdown handling and extra-output rejection; they did not run FIPS.

`node/run.mjs` fails closed without that sealed input. It checks the binary,
candidate package, installed lock, package manifests and every installed
file-transfer member before launching peers. Root still owns hosted execution
and acceptance. This packet does not dispatch, publish, alter native versions,
change canonical checkouts, or modify Git refs/tags.
