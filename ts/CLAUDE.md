# Development

## Commands
```bash
pnpm install      # Install dependencies
pnpm test         # Run tests
pnpm run build    # Build
pnpm run dev      # Dev server
pnpm run test:e2e # E2E tests
```

## Dev Server
- Check `lsof -i :5173` before starting - don't start if already running

## Structure
- `packages/hashtree` - Core library
- `../apps/iris-files` - Web/desktop app (Iris Files, includes e2e tests)

## Design
- **Simple**: SHA256 + MessagePack, no multicodec/CID versioning
- **Focused**: Merkle trees over key-value stores, nothing else
- **Composable**: WebRTC/Nostr/Blossom are separate layers

## App Principles
- **Offline-first**: All ops succeed locally, sync when online
- **Optimistic**: Never await network, fire-and-forget publishes
- **Local source of truth**: `treeRootCache.ts` owns merkle roots
- **Index from public data**: Search index from Nostr events (own content, follows, reactions), not from viewing

## Worker Architecture
- Main thread NDK has no relays but uses transportPlugin (workerInit.ts:149) to route publish/subscribe to worker.

## Code Style
- UnoCSS: use `b-` prefix for borders
- Buttons: use `btn-ghost` (default) or `btn-primary`/`btn-danger`/`btn-success`
- Don't add comments that aren't relevant without context

## Memory Safety
- **Caches**: Use `LRUCache` from `utils/lruCache` with `maxSize`
- **Queues**: Use `BoundedQueue` from `utils/boundedQueue` with `maxItems`/`maxBytes`
- **Worker data**: Use transferable: `postMessage({data}, [data.buffer])` for zero-copy
- **Never**: Unbounded `Map`/`Array` for data that grows with usage
- **Heap analysis**: `cd ../apps/iris-files && pnpm run test:e2e -- e2e/heap-analysis.spec.ts` - takes snapshots via CDP, checks NDK instances, profile cache size, large objects

## Verify & Commit
```bash
pnpm run lint
pnpm run build > /dev/null
```
Fix all lint/build/test errors you encounter, whether introduced by you or pre-existing.
When build, lint, and relevant tests pass, commit the changes without asking.

## Testing
- E2E tests are in `../apps/iris-files/e2e/`
- Playwright runs its own dev server
- Run tests selectively: `cd ../apps/iris-files && pnpm run test:e2e -- e2e/specific-file.spec.ts`
- Always verify changes with e2e tests
- Kill dev servers before tests to avoid port conflicts
- TDD: write failing test first, then fix

### Relays
- Default: local test relay. For real Nostr content: `--config=playwright.production.config.ts`

### Test Rules
- NEVER use `waitForTimeout()` - wait for specific conditions
- Tests MUST pass with full parallelism
- Use `disableOthersPool(page)` after `page.goto('/')`
- Use `setupPageErrorHandler(page)` to filter relay errors
- Use `test.slow()` for complex async operations
- Multi-user WebRTC tests: users must follow each other, keep others pool at 0
- Global timeout 30s, `test.slow()` triples to 90s
- Full suite is slow - run specific tests when debugging

## Tauri
- Build: `pnpm tauri:build`, Dev: `pnpm tauri:dev`
- Rust tests: `cd ../apps/iris-files/src-tauri && cargo test` (includes htree integration tests)
- E2E tests: `DISPLAY=:99 pnpm test:tauri` (requires Xvfb: `Xvfb :99 -screen 0 1920x1080x24 &`)
