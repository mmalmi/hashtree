# @hashtree/merge

Deterministic path overlays with source precedence, explicit deletion markers
(tombstones), and provenance for hidden entries.

## Install

```bash
npm install https://github.com/mmalmi/hashtree/releases/download/hashtree-ts-runtime-v0.5.7/hashtree-merge-0.1.2.tgz
```

This runtime archive is newer than the npm registry version. With npm 12+, add
`--allow-remote=all`. [API reference](https://github.com/mmalmi/hashtree/blob/master/ts/API.md).

## Merge two views

```typescript
import { mergePathSources } from '@hashtree/merge';

const merged = mergePathSources<string>([
  { name: 'base', precedence: 0, entries: [
    { path: 'note.txt', kind: 'file', value: 'original' },
    { path: 'old.txt', kind: 'file', value: 'obsolete' },
  ] },
  { name: 'edits', precedence: 1, entries: [
    { path: 'note.txt', kind: 'file', value: 'updated' },
  ], tombstones: [{ path: 'old.txt' }] },
]);
console.log(merged.entries[0].value);  // updated
console.log(merged.entries[0].source); // edits
console.log(merged.hidden.length);    // 2
```

Higher `precedence` wins; equal precedence uses the later input source. Paths
normalize repeated/leading slashes and reject empty paths, `.` and `..` segments.
Tombstones apply to exact paths, not recursively to descendants. A source's own
tombstone also hides its entry at that path.

Values are caller-owned: use `CID`s or directory-entry metadata in an app.
This function does not read/write blocks or construct a new directory tree;
materialize `merged.entries` through `HashTree` if you want a new published root.
It is an overlay policy, not a CRDT for concurrent rename/move operations.
