# Vercel On Hashtree Plan

Date: April 6, 2026
Scope: Static-site-first deploy platform built on top of hashtree, `iris-sites`, and `blossom-cf-worker-rust`

## Goal

Build a deploy workflow that feels like "Vercel on hashtree" without throwing away the properties that make hashtree useful:

- preview builds should be immutable and content-addressed
- production routes should stay mutable and human-stable
- GitHub can be supported, but must not be the canonical dependency
- the deploy artifact should be a normal hashtree directory, not a provider-specific bundle
- self-hosting and multi-provider publishing should remain first-class

In short: take the current "build a site, `htree add dist --publish app`, share the URL" flow and add the missing control plane around it.

## Non-Goals

- Do not start with SSR, edge functions, or a full serverless clone.
- Do not make GitHub webhooks the only entrypoint.
- Do not replace `htree add`, `iris-sites`, or `blossom-cf-worker-rust`.
- Do not require DNS-based identity in the core routing model.
- Do not centralize all deploy metadata in one private database.

## What Already Exists

The substrate is already mostly here.

### 1. Deploy Artifact And Publish Primitive

`hashtree-cli` already does the core deploy artifact work:

- uploads a directory as a hashtree
- detects site entrypoints such as `index.html`
- prints immutable `nhash` URLs
- with `--publish`, updates a mutable `npub/tree` route
- prints `sites.iris.to` and `drive.iris.to` URLs for both mutable and immutable routes

Relevant code:

- `rust/crates/hashtree-cli/src/app/add.rs`
- `rust/crates/hashtree-cli/src/app/tests.rs`

This is already the equivalent of "deploy the built output and give me preview + prod-style URLs".

### 2. Site Launcher And Runtime

`iris-sites` already behaves like the browser/runtime layer of a deploy platform:

- launcher URLs on `sites.iris.to`
- source URLs on `drive.iris.to`
- immutable runtime hosts on `*.hashtree.cc`
- mutable runtime hosts derived from opaque labels instead of leaking `npub/tree` in DNS
- permalink generation from the currently resolved mutable root

Relevant code:

- `../iris-apps/apps/iris-sites/src/lib/siteConfig.ts`
- `../iris-apps/apps/iris-sites/src/lib/siteHost.ts`
- `../iris-apps/apps/iris-sites/src/App.svelte`

This is already close to the "site runtime + preview launcher" part of a Vercel-like system.

### 3. Edge Origin And Route Serving

`blossom-cf-worker-rust` already covers most of the HTTP edge/origin side:

- accepts encrypted Blossom uploads
- accepts whitelisted hashtree root events over Nostr WebSocket
- serves latest `/<npub>/<tree>/...`
- serves host-mapped sites like `drive.iris.to`
- caches directory indexes and assembled file bytes per root

Relevant code:

- `../blossom-cf-worker-rust/src/lib.rs`
- `../blossom-cf-worker-rust/src/site_serve.rs`
- `../blossom-cf-worker-rust/README.md`

This is already a practical serving layer for mutable and immutable hashtree-backed sites.

### 4. Release Automation

`iris-sites` already has a release flow that:

- builds
- runs tests
- publishes the built output to hashtree
- deploys the same output to Cloudflare Worker static assets or Pages

Relevant code:

- `../iris-apps/apps/iris-sites/scripts/release-site.mjs`

This is effectively a hand-written single-project deploy pipeline.

## What Is Missing

What does not exist yet is the control plane.

### Missing Product Pieces

- project-level deploy config
- a standard deploy manifest format
- deploy history
- preview deployments tied to commits or branches
- promotion and rollback UX
- build logs and build status records
- Git-triggered or CI-triggered builds across many projects
- secrets and environment variable management
- team/project permissions

Without those pieces, the current stack is "strong deploy substrate" rather than "full deploy platform".

## Recommended Shape

Do not build a brand new runtime. Build a thin deploy layer on top of the existing pieces.

### Layer 1: Project Definition

Add a portable project config, for example:

- `.hashtree/deploy.toml`

Possible fields:

- `name`
- `root_dir`
- `build_command`
- `output_dir`
- `publish_tree`
- `preview_entry`
- `production_entry`
- `public_env_allowlist`
- `targets`

Targets might include:

- `hashtree`
- `cloudflare_worker_assets`
- `cloudflare_pages`
- later: `npm`, `crates_io`, `zapstore`, other release targets where appropriate

This keeps the build definition with the repo and avoids a provider-only dashboard as the source of truth.

### Layer 2: Build Execution

Treat build execution as replaceable.

Possible executors:

- local machine
- `hashtree-ci`
- a dedicated worker pool
- Git-triggered automation

Input:

- repo state or commit
- project config

Output:

- built directory
- immutable `nhash`
- mutable publish target if requested
- build log
- build metadata record

The executor should not define the deploy format. It should only produce a normal built directory plus metadata.

### Layer 3: Deploy Metadata

Record deploys as portable metadata, not just console output.

Each deploy should capture:

- project id
- source repo/ref/commit
- builder identity
- build command and output dir
- immutable output `nhash`
- mutable route updated, if any
- timestamp
- status
- optional Cloudflare mirror URL
- log object reference

Storage options:

1. hashtree object published under a project history tree
2. dedicated Nostr events referencing the immutable artifact
3. both, with Nostr for discovery and hashtree for larger logs/metadata

The important part is that preview and production deployments become queryable objects.

### Layer 4: Preview Model

Use immutable hashtree roots as previews by default.

Preview forms:

- `htree://nhash.../index.html`
- `sites.iris.to/#/nhash.../index.html`
- optional host-routed preview if a web edge wants to expose one

This is stronger than ordinary provider preview URLs because the preview artifact is the content address.

### Layer 5: Production Model

Use mutable routes for production:

- `npub/project-name/index.html`
- launcher URL on `sites.iris.to`
- optional host-mapped domain through the worker layer

Promotion should mean "move the mutable pointer to a selected immutable deploy", not "rebuild again and hope for the same bits".

### Layer 6: Edge Serving

Keep using `blossom-cf-worker-rust` or compatible origins for HTTP delivery.

Near-term responsibilities:

- serve current mutable production routes
- optionally expose preview routes
- cache per-root indexes and bytes
- host-map public sites

Longer term:

- consume deploy metadata directly
- expose deploy lookup endpoints if useful
- support project preview hostnames without changing the underlying content model

### Layer 7: UI

Start with lightweight surfaces, not a giant dashboard.

Initial surfaces:

- CLI deploy summary
- `iris-sites` launcher support for preview/prod history links
- git UI badges or links from commit to preview deploy

Later surfaces:

- project page with deploy history
- promote/rollback controls
- log viewer

## Suggested MVP

The smallest useful version is:

1. Define `.hashtree/deploy.toml`.
2. Add a CLI command that builds, publishes the output, and writes a deploy record.
3. Store deploy records in a normal hashtree history tree and optionally mirror a summary to Nostr.
4. Treat immutable `nhash` as the preview deployment.
5. Treat `npub/project` as the production alias.
6. Add "promote this deploy" as a pointer update from immutable artifact to mutable route.

That already gives:

- deploy history
- immutable previews
- reproducible promotion
- rollback by pointer move
- no dependency on GitHub or a private database

## Concrete CLI Direction

Possible command shape:

```bash
htree deploy
htree deploy --preview
htree deploy --prod
htree deploy promote <deploy-id>
htree deploy rollback <deploy-id>
htree deploy list
htree deploy logs <deploy-id>
```

Potential behavior:

- `htree deploy`
  - reads `.hashtree/deploy.toml`
  - runs build
  - uploads `output_dir`
  - creates deploy metadata
  - prints preview and production candidate URLs

- `htree deploy --prod`
  - same build
  - updates configured mutable route

- `htree deploy promote <deploy-id>`
  - does not rebuild
  - updates mutable route to the chosen immutable `nhash`

## Delivery Phases

### Phase 1: Project Config And Deploy Record Format

In `hashtree`:

- define `.hashtree/deploy.toml`
- define deploy metadata schema
- define a storage/discovery convention for deploy history

Acceptance:

- one repo can describe how to build and publish a site
- deploy records are stable objects, not just terminal text

### Phase 2: CLI MVP

In `hashtree`:

- add `htree deploy`
- add `promote`, `rollback`, `list`, `logs`
- keep using normal `htree add` artifact generation under the hood

Acceptance:

- local CLI can do preview deploys and production promotion using immutable outputs

### Phase 3: CI / Git Integration

Likely in `hashtree-ci` plus repo automation:

- trigger preview deploys on pushes
- attach deploy metadata to commits
- show deploy status in git UI

Acceptance:

- every commit can have a preview deploy
- production can be promoted from an already-built preview

### Phase 4: `iris-sites` Integration

In `iris-sites`:

- understand deploy records
- show deploy history, preview, permalink, source, and promote actions where appropriate

Acceptance:

- a user can move from launcher to preview to source to production route without custom shell scripts

### Phase 5: Edge Enhancements

In `blossom-cf-worker-rust`:

- optional preview host routing
- optional deploy metadata lookup helpers
- better project/domain mapping and cache invalidation hooks

Acceptance:

- web delivery is cleaner, but the deploy model still works without this phase

### Phase 6: Secrets And Advanced Build Features

Only after the static-site flow is solid:

- managed build secrets
- per-environment config
- branch/PR conventions
- optional SSR or function story

Acceptance:

- advanced features do not compromise the simple immutable-preview + mutable-prod model

## Repo Responsibilities

### `hashtree`

Primary ownership:

- deploy config format
- deploy metadata format
- CLI commands
- publish/promote/rollback semantics

### `iris-apps/apps/iris-sites`

Primary ownership:

- user-facing launcher/runtime UX
- preview/prod/source/permalink navigation
- optional deploy history UI

### `blossom-cf-worker-rust`

Primary ownership:

- HTTP serving
- latest-root lookup
- host mapping
- cache and site index behavior

### `hashtree-ci`

Primary ownership:

- remote build execution
- commit-linked preview deploys
- build status publication

## Open Questions

- Should deploy metadata live primarily in Nostr, in hashtree objects, or in both?
- What is the right stable project id: repo path, `npub/name`, or explicit config field?
- Should preview hostnames exist at all, or is immutable launcher URL enough for MVP?
- Where should build logs live when they get large?
- Should promotion require signed approval from a project owner key?
- How much should Cloudflare-specific deployment remain in scope for this plan versus staying an optional mirror target?

## Recommendation

Treat the current stack as "already enough for static deploy artifacts and serving" and build only the missing control plane.

The right first milestone is not "clone Vercel". The right first milestone is:

- project config
- immutable preview deploy records
- mutable production promotion
- CI-triggered previews

That would already make hashtree feel like a real deploy platform while preserving its core advantages.
