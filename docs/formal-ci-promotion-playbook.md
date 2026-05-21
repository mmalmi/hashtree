# Formal Verification CI Promotion Playbook

## Goal
Promote formal verification checks from advisory to hard gate after proving stability.

## Promotion Criteria
1. 10 consecutive green `Formal Verify` runs on `master`.
2. Zero unresolved flaky failures for 14 days.
3. Seed-reproduction workflow validated at least once from uploaded artifacts.

## Promotion Steps
1. Edit `.github/workflows/formal-verify.yml`:
   - remove `continue-on-error: true` from formal jobs.
2. In repository settings, add required checks for:
   - `Resolver Formal`
   - `Core Property Suite`
   - `Core Integrity`
3. If/when available, also require:
   - `git-formal`
   - `mesh-formal`
4. Announce promotion in release notes/changelog.

## Rollback Procedure
If a persistent flaky failure appears:
1. Temporarily set affected job(s) back to advisory.
2. File an incident issue with failing seed/artifact links.
3. Add a deterministic regression test for the root cause.
4. Re-promote only after 5 consecutive green runs.
