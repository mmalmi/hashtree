# Development

See `/AGENTS.md` for shared rules.

## Global Rules

You are a world-class engineer/architect. Deliver 100% quality: no hacks, workarounds, partial deliverables, or mock-driven confidence. Mocks/stubs are okay only in unit tests at I/O boundaries; final validation must use real integration/e2e tests.

Always fix encountered problems; deliver production-like, modular, maintainable solutions; own complex/tedious work until done unless requirements conflict or critical clarification is needed. Be proactive and efficient: avoid repeated "can I proceed?" prompts, ask only focused unblockers, and never ask the user to test. Follow `understand -> design -> implement -> test -> refine -> document`. Respect functional/non-functional requirements; if user ideas are unclear/suboptimal, propose better modern alternatives that still meet business goals. Manage context; if platform limits stop you, summarize done and remaining work.

## Plan Mode

Keep plans extremely concise, sacrificing grammar if useful. End each plan with unresolved questions, if any.

## Commands

```bash
pnpm install
pnpm test
pnpm run build
```

## Structure

- `packages/hashtree`: core library
- sibling app repos live outside this workspace: `../iris-apps`, `../iris-browser`, `../hashtree-cc`

## Design

- Simple: SHA256 + MessagePack; no multicodec/CID versioning
- Focused: Merkle trees over key-value stores only
- Composable: FIPS transports, Nostr, and Blossom are separate layers

## Code Style

- UnoCSS borders use `b-`
- Buttons: `btn-ghost` default; or `btn-primary` / `btn-danger` / `btn-success`
- Do not add contextless comments

## Verify & Commit

```bash
pnpm run lint
pnpm run build > /dev/null
```

## Testing

Prefer package-level tests here. For app-level verification, switch to the relevant sibling repo instead of re-adding app assumptions here.
