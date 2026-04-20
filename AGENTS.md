# Agent Guidelines

We are building a decentralized system independent of DNS, SSL certificates, web servers, CDNs, etc., so avoid DNS-based identity like NIP-05.

## Shared Rules
- TDD when it makes sense and changes are non-trivial: start with a failing test, then implement.
- Keep tests deterministic; avoid flaky tests.
- Verify changes with e2e tests. Don't ask the user to test. Don't assume code works - everything must be verified with tests. Unit testing is often useless, prefer e2e unless good reason.
- Fix all errors you encounter, whether related to your changes or not.
- Keep file sizes reasonable. If a file starts growing unwieldy, break it into smaller focused modules instead of letting it sprawl.
- For Nostr subscriptions, peer discovery, and mutable-root watches, prefer open subscriptions over one-shot timed fetches. A missing response inside one time window is not evidence that the data does not exist.
- For mesh reads, do not turn a slow peer into a fake miss just because a fixed wall-clock timeout expired. Prefer hedged requests, longer-lived in-flight reads, and idle/progress-based cutoffs.
- When one peer is slow, ask additional peers without immediately cancelling the first request. First valid response wins; cancel or ignore losers only after a winner is established.
- Treat explicit misses differently from timeouts. Record slow-source expiry as timeout, not not-found, so routing and reputation can distinguish dead/slow paths from absent data.
- Progress or fragment arrival should extend a request; unauthenticated "still working" heartbeats without bytes should not keep requests alive forever. Keep per-peer work and memory bounded.
- Never run `git pull`/`git rebase` from `htree://self/*` (or a remote pointing there) because it is publish/storage, not an integration upstream.
- If push to `htree://self/hashtree` is non-fast-forward, do not pull from that remote; resolve locally and update the hashtree remote via push strategy (for example `git push --force origin master`) only when needed. Release remote is also push only.
- Commit after relevant tests (and build/lint if applicable) pass, then push to htree remote (`htree://self/hashtree`).
- For frontend or TypeScript changes, verify unreleased work in the local dev app (`pnpm tauri dev` / localhost) or in an immutable released shell (`htree://nhash.../index.html`). Do not debug against the mutable `htree://npub.../<app>` shell until that app has actually been published, because it may still point to an older build.
- When verifying `iris-files` apps inside the native Iris/Tauri shell, do not point native smoke or manual testing at an old mutable app shell by accident. First publish the freshly built app to hashtree (`htree add dist-<app> --publish <app>` or at least `htree add dist-<app>` and use the returned immutable `htree://nhash.../index.html` URL), then run the native verification against that exact URL.
- On macOS, native Iris/Tauri screenshot or install-flow verification should usually run through the Linux Docker `tauri-driver` harness instead of local `tauri-driver`, because local `tauri-driver` support is not available there. Prefer `apps/iris/scripts/test-native-linux-docker.sh` or the matching `pnpm` wrapper script when you need native screenshots and real child-webview interaction.
- For app testing, use native system or Docker, whichever is easier; Docker is often easier to control.
