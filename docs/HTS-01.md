# HTS-01: hashtree Core Protocol

This spec defines the exact bytes and identifiers implementations must agree on.

Plain-language model: hashtree stores immutable blobs and directory/file nodes by hash, then publishes mutable roots on Nostr.

## 1. Scope

This document specifies:

1. Content addressing and tree-node encoding.
2. CHK encryption.
3. `nhash` identifiers and `npub/path` references.
4. Mutable root publication on Nostr.
5. `htree://` URL profile.
6. Optional WebRTC blob exchange profile.

## 2. Content Addressing Model

1. Hash function MUST be SHA-256.
2. Hashes are always 32 bytes.
3. Blob address is `SHA256(blob_bytes)`.
4. Tree-node address is `SHA256(msgpack_tree_node_bytes)`.
5. Storage is key-value by hash:
   `put(hash, bytes)` and `get(hash) -> bytes?`.

## 3. Tree Node Wire Format

A stored object is one of:

1. Blob: raw bytes (no wrapper).
2. Tree node: MessagePack map with compact keys.

Node map fields:

- `l` (array): links to child objects.
- `t` (`u8`): node type. MUST be `1` (File) or `2` (Dir).

Link map fields:

- `h` (`bytes32`): child hash. REQUIRED.
- `k` (`bytes32`): child CHK key. OPTIONAL.
- `m` (map): metadata. OPTIONAL.
- `n` (`utf8`): entry name. OPTIONAL (mainly for directories).
- `s` (`u64`): child byte size. REQUIRED.
- `t` (`u8`): link type. OPTIONAL, default `0`.

Type values:

- `0` = Blob
- `1` = File
- `2` = Dir

Determinism rules:

1. Identical logical nodes MUST encode to identical MessagePack bytes.
2. Node map fields MUST be encoded in `l`, then `t` order.
3. Link map fields MUST be encoded in `h`, optional `k`, optional `m`, optional
   `n`, `s`, then `t` order.
4. Metadata map keys MUST be sorted by UTF-8 bytes before encoding.
5. Dir-node links MUST be sorted by entry-name UTF-8 bytes before encoding.
6. File-node link order is semantic chunk order and MUST be preserved.

Decoding rules:

1. Missing/unknown link `t` MUST be treated as `Blob` (`0`).
2. Node `t` values other than `1` or `2` MUST be rejected.

## 4. Chunking and Fanout Defaults

Recommended defaults:

1. Chunk size: `2 MiB`.
2. Max links per node: `174`.

These are defaults, not protocol constants. Different values change tree shape and root hash.

## 5. CHK Encryption

CHK (Content Hash Key) is deterministic convergent encryption.

For plaintext `P`:

1. `content_hash = SHA256(P)` (32 bytes).
2. `enc_key = HKDF-SHA256(ikm=content_hash, salt="hashtree-chk", info="encryption-key", L=32)`.
3. `ciphertext = AES-256-GCM(enc_key, nonce=0x000000000000000000000000, aad="", plaintext=P)`.
4. Stored bytes are `ciphertext || 16-byte tag`.
5. CID key is `content_hash` (not `enc_key`).

Properties:

1. Same plaintext produces same key and ciphertext.
2. Hash+key is a capability: holders can decrypt.
3. Equality leakage exists: identical plaintext -> identical ciphertext.

## 6. CIDs and Bech32 IDs

CID (content identifier) is `{hash, key?}`:

- `hash`: 32-byte content address.
- `key`: optional 32-byte CHK decryption key.

### 6.1 CID Text Form

`<hash_hex>` or `<hash_hex>:<key_hex>` (each part is 64 hex chars).

### 6.2 `nhash`

`nhash` is a human-readable immutable permalink.
HRP MUST be `nhash` (Bech32, not Bech32m).

Payload is TLV bytes:

- Field encoding: `[type:1][len:1][value:len]`
- Type `0`: hash (`bytes32`), REQUIRED
- Type `5`: decrypt key (`bytes32`), OPTIONAL

### 6.3 Mutable Reference Form (`npub/path`)

Mutable references SHOULD use path form:

- `<npub>/<tree_or_repo_path>`

For link-visible access, a link secret MAY be carried in a URL fragment or
client-local state:

- `#k=<64-hex>`
- Key recovery rule: `root_key = encryptedKey XOR k`

HTTP servers MUST NOT accept decryption keys in query strings or request bodies
for public content serving. Clients fetch ciphertext blobs and assemble/decrypt
logical trees locally.

`hashtree:` URI prefix MAY appear before `nhash` and decoders MUST ignore it.

## 7. Mutable Roots via Nostr

Root events use kind `30064` with NIP-33 replaceable semantics (`d` tag). Readers SHOULD also accept legacy kind `30078` root events for compatibility.

Required tags:

- `["d", "<tree_name>"]`
- `["l", "hashtree"]` (legacy unlabeled events MAY be accepted)
- `["hash", "<64-hex-root-hash>"]`

Visibility tags (choose zero or one):

- none: unencrypted root/content
- `["key", "<64-hex-chk-key>"]`: public CHK
- `["encryptedKey", "<64-hex-xor-masked-key>"]`: link-visible (`encryptedKey = root_key XOR link_secret`)
- `["selfEncryptedKey", "<nip44-v2-ciphertext>"]`: private

Event `content` is optional. Producers SHOULD use empty string or root hash for legacy compatibility. Consumers MUST prefer `hash` tag and MAY fall back to legacy content.

If multiple events match author + `d`:

1. Choose newest `created_at`.
2. If tied, choose larger event id.

## 8. Visibility Modes

### 8.1 Unencrypted

- No CHK key required.
- Event omits `key`, `encryptedKey`, and `selfEncryptedKey`.

### 8.2 Public (CHK)

- Root CHK key is in `key` tag.

### 8.3 Link-Visible

- Share secret `S` is 32 bytes.
- Event stores `encryptedKey = root_key XOR S`.
- Share URL carries `#k=<hex(S)>` or stores `S` in client-local state.
- Client derives `root_key = encryptedKey XOR k`.

### 8.4 Private

- Event stores `selfEncryptedKey = NIP-44(key_hex encrypted to owner pubkey)`.
- Only owner can decrypt.

## 9. `htree://` URL Profile

Syntax:

`htree://<identifier>/<repo_or_tree_path>[#<fragment>]`

Rules:

1. `identifier` MAY be `npub1...`, 64-hex pubkey, petname alias, or `self`.
2. Everything after first `/` is repo/tree path and MAY include `/`.
3. `:` separator form is invalid.

Fragments:

- none: public mode
- `#k=<64-hex>`: link-visible with explicit share secret
- `#link-visible`: create-time request to auto-generate secret
- `#private`: private (author-only)

Unknown fragments MUST be rejected.

## 10. Git Repository Profile (Optional)

Current git mapping at tree root:

1. `/.git/HEAD`
2. `/.git/refs/...`
3. `/.git/objects/<2-hex>/<38-hex>` (zlib-compressed loose objects)

Repository publication uses Section 7 with `d=<repo_path>`.

## 11. WebRTC Blob Exchange Profile (Optional)

Binary frame format:

1. First byte: message type.
2. Remaining bytes: MessagePack map body.

Types:

- `0x00` request: `{h: bytes32, htl?: u8}`
- `0x01` response: `{h: bytes32, d: bytes, i?: u32, n?: u32}`

`i`/`n` are fragment index and total for chunked responses.
Recommended fragment size: `32 KiB`.

### 11.1 `htl` Behavior

`htl` (hops-to-live) is forwarding budget for one request.
`MAX_HTL` is profile default max (currently `10`).

Rules:

1. If omitted, receiver MUST treat `htl` as `MAX_HTL`.
2. Peer MAY forward only if `htl > 0`.
3. Forwarder MUST decrement `htl` before forwarding.
4. Freenet-style decrement policy:
   - `2..(MAX_HTL-1)`: decrement by `1`.
   - `MAX_HTL`: probabilistic decrement per peer (commonly 50%).
   - `1`: probabilistic decrement to `0` per peer (commonly 25%).
5. `htl = 0` MUST NOT be forwarded.

### 11.2 `htl` Rationale

`htl` bounds request spread so misses do not flood the network.
Probabilistic decrement at `MAX_HTL` and `1` reduces simple origin/distance probing.
