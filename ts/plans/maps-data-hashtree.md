# Maps Data on Hashtree Plan

## Goals
- Persist only user-created places/annotations in Hashtree.
- Do not store user search queries or external API responses in Hashtree (privacy).
- Publish public map tiles via a crawler-managed dataset.

## Current Data Model (Client)
- `maps.json` (places + annotations)

## Query Flow (Client)
1. Search and tiles always use external APIs directly.
2. Ranking and display happen in-memory only.
3. No cache writes for geocode or tiles.

## Sharing
- User data comes from own tree or selected followed users.
- Visibility handled by normal Hashtree/Nostr publishing.

## Crawler Data Model (Public)
- `meta.json`
  - `version`
  - `dataset`: `type=tiles`, `urlTemplate`, `bounds`, `minZoom`, `maxZoom`, `ext`, `scheme`, `tileSize`
  - `counts`: `totalTiles`, `perZoom`
- `tiles/{z}/{x}/{y}.{ext}` (raw tile bytes)

## Canonical Storage Rules
- No timestamps or run-specific metadata.
- Stable JSON encoding for `meta.json` (sorted keys).
- Directory entry order is deterministic (hashtree sorts by name).

## Crawler Script
- `scripts/maps/crawl-tiles.js`
- Example:
  - `pnpm run maps:crawl -- --bbox 59.0,17.5,60.0,18.5 --min-zoom 6 --max-zoom 12`
- Output:
  - Root hash + nhash
  - Tiles stored + bytes
- Optional publishing:
  - `--blossom` to push blobs
  - `--publish --relays --nsec` to publish root to Nostr

## Estimation & Progress
- Tile count is computed from bounds + zoom range.
- Progress logs show processed/total, ok/skip/fail, bytes downloaded, estimated total size, remaining, rate, and ETA.
- `--estimate-only` reports tile count + size estimate (default 25 KB/tile or `--avg-bytes` override).

## Testing
- E2E covers map load and pin persistence.
