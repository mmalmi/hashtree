# Development

## Commands
```bash
pnpm install      # Install dependencies
pnpm test         # Run tests
pnpm run build    # Build
```

## Structure
- `packages/hashtree` - Core library
- sibling app repos live outside this workspace (`../iris-apps`, `../iris-browser`, `../hashtree-cc`)

## Design
- **Simple**: SHA256 + MessagePack, no multicodec/CID versioning
- **Focused**: Merkle trees over key-value stores, nothing else
- **Composable**: FIPS transports, Nostr, and Blossom are separate layers

## Code Style
- UnoCSS: use `b-` prefix for borders
- Buttons: use `btn-ghost` (default) or `btn-primary`/`btn-danger`/`btn-success`
- Don't add comments that aren't relevant without context

## Verify & Commit
```bash
pnpm run lint
pnpm run build > /dev/null
```
Fix all lint/build/test errors you encounter, whether introduced by you or pre-existing.
When build, lint, and relevant tests pass, commit the changes without asking.

## Testing
- Keep this workspace self-contained
- For app-level e2e, Tauri, or portable-site flows, use the relevant sibling repo instead of assuming an `../apps/*` checkout
