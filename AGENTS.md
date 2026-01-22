# Agent Guidelines

We are building a decentralized system independent of DNS, SSL certificates, web servers, CDNs, etc., so avoid DNS-based identity like NIP-05.

## Shared Rules
- TDD when practical: start with a failing test, then implement.
- Keep tests deterministic; avoid flaky tests.
- Verify changes with unit or e2e tests. Don't ask the user to test. Don't assume code works - everything must be verified with tests.
- Commit after relevant tests (and build/lint if applicable) pass. Run `git pull origin master --rebase` to rebase onto latest origin, then push to htree remote (`htree://self/hashtree`).
