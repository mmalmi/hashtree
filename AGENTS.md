# Agent Rules

- Build decentralized systems independent of DNS, SSL certs, web servers, CDNs. Avoid DNS-based identity such as NIP-05.
- TDD for non-trivial changes when sensible. Prefer deterministic e2e/integration over mocks; verify before claiming done.
- Fix errors you hit. Keep files small; split sprawling modules.
- Nostr subscriptions, peer discovery, mutable roots: prefer open subscriptions. One quiet window is not absence.
- Mesh reads: never turn slow peers into fake misses. Distinguish misses from timeouts; use hedged/long-lived reads, idle/progress cutoffs, first valid response wins, bounded per-peer work.
- Public HTTP serves raw blob/ciphertext by default. No decryption keys, logical file assembly, or plaintext tree serving except explicit allowlists.
- Perf experiments go in `docs/EXPERIMENTS.md`; omit pubkeys, secrets, IPs, private hosts, raw hashes unless asked.
- Never pull/rebase from `htree://self/*`; it is publish/storage. If non-ff, resolve locally and push intentionally.
- After relevant checks pass, commit and push to `htree://self/hashtree`.
- Frontend/Tauri: verify unreleased work in local dev or immutable `htree://nhash...`; avoid mutable `htree://npub...` until published.
- Native Iris/Tauri verification: publish or `htree add dist-<app>` first; verify exact immutable URL. Prefer Linux Docker `tauri-driver` harness on macOS.
