# Formal Models

This directory contains focused TLA+ models for Hashtree rules that are easy to
state informally but important to keep exact.

## Current Models

- [`bud16_messagepack_determinism`](./bud16_messagepack_determinism):
  the BUD-16 canonical MessagePack directory profile, including fixed field
  order, metadata-key sorting, and directory-link sorting by UTF-8 name bytes.

## Running Models

Each model directory includes its own `README.md` and `run_tlc.sh`.

Typical usage:

```bash
./formal/bud16_messagepack_determinism/run_tlc.sh --mode ci
```
