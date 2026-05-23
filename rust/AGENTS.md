# Agent Guidelines

See `/AGENTS.md` for shared rules.

Local tests use `TestRelay`/`TestServer` and no network deps; reserve `#[ignore]` for external infra. In `hashtree-sim`, use the simulation-compatible clock and stepped/polled progress, not raw `tokio::time::sleep` / `std::thread::sleep`; prefer virtual time and explicit message pumping so sims stay deterministic and fast.

Keep it simple: no over-engineering, minimal changes that solve the problem.
