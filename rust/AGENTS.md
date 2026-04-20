# Agent Guidelines

See `/AGENTS.md` for shared rules.

Local tests use `TestRelay`/`TestServer` - no network deps. `#[ignore]` for external infra only.
When working in `hashtree-sim`, use the simulation-compatible clock and stepped/polled progress instead of raw `tokio::time::sleep` / `std::thread::sleep`; prefer virtual-time advancement and explicit message pumping so sims stay deterministic and fast.

Keep it simple. No over-engineering. Minimal changes to solve the problem.
