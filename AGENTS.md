# Agent Guidelines

Build decentralized systems independent of DNS, SSL certs, web servers, CDNs, etc.; avoid DNS-based identity such as NIP-05.

## Shared Rules
- TDD for non-trivial changes when sensible: write the failing test, then implement. Keep tests deterministic; prefer e2e/real integration over unit tests and mocks. Do not ask the user to test or assume code works; verify with tests.
- Fix all errors you encounter, related or not. Keep files reasonably sized; split sprawling modules.
- Nostr subscriptions, peer discovery, and mutable-root watches: prefer open subscriptions over one-shot timed fetches. No response inside one time window is not evidence of absence.
- Mesh reads: never turn a slow peer into a fake miss because a wall-clock timeout expired. Prefer hedged requests, longer-lived in-flight reads, and idle/progress-based cutoffs. Ask extra peers without immediately cancelling the first; first valid response wins, then cancel/ignore losers. Distinguish explicit misses from timeouts so routing/reputation can tell absent data from dead/slow paths. Progress or fragments extend a request; unauthenticated "still working" heartbeats without bytes must not keep it alive forever. Bound per-peer work/memory.
- Public HTTP serving must be raw blob/ciphertext by default: do not accept decryption keys in public routes, do not assemble logical files or file ranges from CIDs/hashes, and keep logical plaintext tree serving only behind explicit configured allowlists such as approved npub routes.
- Record performance experiments in `docs/EXPERIMENTS.md`; omit identifying info (pubkeys, secrets, IPs, private hostnames, exact repo names, raw hashes) unless explicitly asked.
- Never `git pull`/`git rebase` from `htree://self/*` or remotes pointing there; it is publish/storage, not an integration upstream. If push to `htree://self/hashtree` is non-ff, resolve locally and update by push strategy, e.g. `git push --force origin master`, only when needed. Release remote is push-only.
- After relevant tests/build/lint pass, commit and push to htree (`htree://self/hashtree`).
- Frontend/TS changes: verify unreleased work in local dev app (`pnpm tauri dev` / localhost) or immutable released shell (`htree://nhash.../index.html`). Do not debug against mutable `htree://npub.../<app>` until published; it may point to an older build.
- Iris-files native Iris/Tauri verification: first publish the fresh build (`htree add dist-<app> --publish <app>`) or at least `htree add dist-<app>` and use the immutable `htree://nhash.../index.html`; run native verification against that exact URL.
- On macOS, native Iris/Tauri screenshot or install-flow verification should usually use the Linux Docker `tauri-driver` harness, not local `tauri-driver`: prefer `apps/iris/scripts/test-native-linux-docker.sh` or the matching `pnpm` wrapper. For app testing, use native system or Docker, whichever is easier; Docker is often easier to control.
