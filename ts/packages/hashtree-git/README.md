# @hashtree/git

Helpers for parsing repository URLs and reading root visibility from Nostr
events. To clone/push repositories, install the [Rust Git remote helper](https://github.com/mmalmi/hashtree/blob/master/rust/README.md#build-from-source).

## Install

```bash
npm install https://github.com/mmalmi/hashtree/releases/download/hashtree-ts-runtime-v0.5.7/hashtree-git-0.1.9.tgz
```

This runtime archive is newer than the npm registry version. With npm 12+, add
`--allow-remote=all`. [API reference](https://github.com/mmalmi/hashtree/blob/master/ts/API.md).

## Usage

```typescript
import { buildHtreeUrl, parseHtreeUrl } from '@hashtree/git';

const url = buildHtreeUrl('self', 'myrepo', { visibility: 'private' });
const parsed = parseHtreeUrl(url);
console.log(url);               // htree://self/myrepo#private
console.log(parsed.repo);       // myrepo
console.log(parsed.visibility); // private
```

`self` is a local CLI identity alias; share an owner's `npub` URL with others.
The parser validates URL structure, not the identity or existence of a repo.
For link-visible URLs, supply a 64-hex-character `linkKey`, or
`autoGenerateLinkKey: true` to request key generation on CLI push.
Keep URLs containing `#k=...` private to the intended readers.

`parseHtreeVisibility(tags)` reads key-disclosure metadata.
`resolveHtreeRootCid({ tags, content, linkKey, requirePrivate, decryptSelfKey })`
extracts a usable CID; private roots need a decrypt callback. These helpers do
not fetch relays or verify authorship: verify the signed event, expected author,
kind, and tree name before trusting it. See [Nostr root discovery](https://github.com/mmalmi/hashtree/blob/master/ts/packages/hashtree-nostr/README.md).
