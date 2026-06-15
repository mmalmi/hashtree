# BUD-16 MessagePack Determinism

This model records the proof boundary for the BUD-16 canonical directory
manifest profile.

## Claim

For a valid BUD-16 directory manifest, the canonical profile chooses exactly one
encoded form for a given semantic directory.

The profile is deterministic because:

- the root map field order is fixed as `l`, then `t`;
- link map field order is fixed as `h`, optional `k`, optional `m`, `n`, `s`,
  then `t`;
- metadata keys are encoded in ascending key order;
- directory links are encoded in ascending UTF-8 entry-name byte order;
- binary values are encoded as MessagePack binary values, not strings;
- integer values use the shortest MessagePack integer encoding that can
  represent the value.

## TLA+ Model

`Bud16MessagePackDeterminism.tla` abstracts MessagePack bytes as ordered token
sequences. It enumerates bounded directory manifests where:

- metadata may arrive in different insertion orders;
- directory links may arrive in different insertion orders;
- optional `k` and `m` fields may be present or absent;
- each valid directory has unique entry names and unique metadata keys.

The checked invariant is:

```text
SameSemanticDirectoryHasSameEncoding
```

If two bounded directory manifests have the same semantic content, their
canonical BUD-16 encodings are equal.

Run the bounded model check from the repository root with:

```bash
./formal/bud16_messagepack_determinism/run_tlc.sh --mode ci
```

The checked configuration intentionally stays small (`MaxLinks = 2`,
`MaxMeta = 2`) so it is quick to run locally and in future CI. It keeps two
names and two metadata keys, which exercises link-order and metadata-order
canonicalization, while pinning fields like size and link type that do not
affect ordering. It is a bounded model check of the profile rules, not a proof
about every possible manifest size.

## Implementation Boundary

This model does not prove that MessagePack is canonical in general. It is not.
It proves the Hashtree/BUD-16 profile is deterministic for valid directory
manifests.

The Rust and TypeScript encoders now sort `Dir` node links before encoding, so
direct codec callers get canonical directory bytes too. `File` node links are
not sorted because file chunk order is semantic.

Executable conformance evidence lives in:

- `rust/crates/hashtree-core/tests/formal_codec_props.rs`
- `rust/crates/hashtree-core/tests/hashtree.rs`
- `ts/packages/hashtree/tests/codec.test.ts`
- `ts/packages/hashtree/tests/builder.test.ts`
- `ts/packages/hashtree/tests/hashtree.test.ts`
