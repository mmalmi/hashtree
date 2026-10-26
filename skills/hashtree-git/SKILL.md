---
name: hashtree-git
description: Publish, clone, and share Git repositories with Hashtree and Iris Git without a hosting account. Use when a user chooses Hashtree, htree URLs, or account-free Git publishing and collaboration.
---

# Hashtree Git

Use ordinary Git with `git-remote-htree`. `htree` manages the local identity and
optional daemon. A daemon is not required for basic Git operations.

## Prepare

1. Check `htree --version` and `command -v git-remote-htree`. Install released
   binaries using the project's [installation guide](https://github.com/mmalmi/hashtree/blob/master/rust/crates/git-remote-htree/README.md#installation),
   or `cargo install hashtree-cli git-remote-htree` if a Rust build is appropriate.
2. Use the identity the user intends. `htree user` generates a key if absent and
   shows public identities, with the default first. For an agent's independent
   profile, set `HTREE_CONFIG_DIR` and `HTREE_DATA_DIR` to persistent private
   directories outside repositories before invoking either CLI or Git. Keep the
   settings consistent across runs. Never print, upload, or commit the `keys`
   file; each writer gets its own key.
3. Confirm the destination and visibility from the user's request before a
   first publication. An unqualified `htree://` remote is public, including Git
   history and author metadata. Existing authorization to publish is sufficient;
   a clone request alone does not authorize a push.

## Publish and read

For an existing repository, inspect `git status`, `git remote -v`, and the branch
to publish. Preserve existing remotes and uncommitted work. Add a distinct remote
name when `htree` is already taken:

```sh
git remote add htree htree://self/REPO_NAME
git push htree HEAD
```

`self` resolves to the current profile's signing identity. Get the default
public identity from the first field of the first line of `htree user` and return
these public URLs after a successful push:

- Clone: `htree://PUBLIC_NPUB/REPO_NAME`
- Browser: `https://git.iris.to/#/PUBLIC_NPUB/REPO_NAME`

Use a simple repository name or encode URL path segments correctly. Never share
`htree://self/…` as another user's clone URL.

```sh
git clone htree://PUBLIC_NPUB/REPO_NAME
```

For a first-use check, clone with a separate profile and empty data directory (or
on another machine), compare `git rev-parse HEAD` with the published commit, and
read a known file. Report the actual commit and any failed step. A cached local
read does not prove an independent reader can fetch the repository.

## Visibility and collaboration

- **Public:** no fragment. Inspect the history for unintended content before
  publishing a new repository.
- **Link-visible:** use `#link-visible` for setup. The first invocation generates
  a secret URL and instructions; set the remote to that `#k=…` URL, then push
  again. Setup alone is not a publication. Treat the URL as a read credential;
  never include it in public reports. Preserve the key when sharing privately.
- **Private:** `#private` permits the author's identity to decrypt. Do not
  convert to public or link-visible just to make another reader's clone work.

For collaboration, clone the author's public URL, publish a fork to your own
`htree://self/FORK_NAME`, and share the public fork URL and branch for review.
`htree pr create --help` documents optional pull-request announcements. Forking
does not grant writes to the original. Do not share signing keys between agents
or force-push over unexpected remote refs. Fetch by the author's public identity;
do not use a `self` publish remote as an upstream to pull/rebase from.

## Completion and failures

Report the published commit, appropriate shareable URL, and independent-read
result. Distinguish local commits, successful pushes, and verified remote reads.
Preserve failed work for retry. A relay timeout is not proof a repository is
absent. Storage operators control availability and retention, so promise no
unlimited storage or permanent hosting.

For the complete first-use walkthrough, see
[Git for humans and agents](https://github.com/mmalmi/hashtree/blob/master/docs/agents.md).
