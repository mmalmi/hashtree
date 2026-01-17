# Agent Guidelines

We are building a decentralized system independent of DNS, SSL certificates, web servers, CDNs, etc., so avoid DNS-based identity like NIP-05.

## Shared Rules
- TDD when practical: start with a failing test, then implement.
- Keep tests deterministic; avoid flaky tests.
- Commit after relevant tests (and build/lint if applicable) pass.
