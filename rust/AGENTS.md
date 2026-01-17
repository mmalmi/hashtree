# Agent Guidelines

TDD. No flaky tests. Commit when tests pass.

Local tests use `TestRelay`/`TestServer` - no network deps. `#[ignore]` for external infra only.

Keep it simple. No over-engineering. Minimal changes to solve the problem.
