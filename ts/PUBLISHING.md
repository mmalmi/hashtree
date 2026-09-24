# Publishing TypeScript packages to npm

The [Publish npm packages workflow](../.github/workflows/publish-npm.yml) builds
and checks the SDK, prepares archives, then publishes those exact archives using
npm trusted publishing (OIDC). It does not need an `NPM_TOKEN`, `npm login`, or a
per-release authenticator prompt. Direct publishing must be explicitly enabled
in each package's npm trust settings.

## One-time setup

For each package in `pnpm publish:packages:plan`, open its npm Settings tab and
add this trusted publisher:

| Field | Value |
| --- | --- |
| Publisher | GitHub Actions |
| Organization or user | `mmalmi` |
| Repository | `hashtree` |
| Workflow filename | `publish-npm.yml` |
| Environment name | `npm` |
| Allowed actions | Enable **Allow npm publish** |

Configure the GitHub `npm` environment to allow deployments only from `master`.
The workflow also rejects other branches. Verification jobs have no npm identity;
only the publish job receives `id-token: write`. Reviewers are optional: adding
required reviewers to the environment makes each run wait for approval.

The npm account owner may need to complete an interactive authentication check
when creating the trust connection. Existing token restrictions can stay in place:
OIDC works with npm's “disallow tokens” publishing policy. Packages not yet on npm
need an initial publication before their package settings can be configured.

See npm's [trusted publishing documentation](https://docs.npmjs.com/trusted-publishers/)
for supported runners and account requirements.

## Release

1. Update package versions and changelog, then integrate the intended source into
   `master`. Follow the repository's release plan and gates for the release.
2. Open **Actions → Publish npm packages → Run workflow** on `master`. Leave
   `publish` off to inspect the `npm-packages` and `typescript-api-docs` artifacts.
3. Select package names (core by default, or `all` for the full SDK). Local
   Hashtree dependencies are included automatically in dependency order.
4. Run it with `publish` enabled to release directly to npm after verification.

From the GitHub CLI, the same actions are:

```bash
gh workflow run publish-npm.yml --ref master -f publish=false
gh workflow run publish-npm.yml --ref master -f publish=true -f packages=@hashtree/core
```

The workflow publishes in dependency order and skips versions already present on
npm. Registry lookup errors stop the run rather than masquerading as missing
versions. If a run partially succeeds, fix the failure and rerun; published
versions are immutable and do not need to be uploaded again.

New versions use npm's `latest` tag. This workflow is for stable releases; use a
separate prerelease policy before publishing versions that should not be latest.
Registry scanning can delay availability after a successful publish.

`@hashtree/nostr-pubsub` and `@hashtree/fips-transport` have not yet had their first
npm publication. Before selecting either (or `all`) for an OIDC run, publish their
verified archives once with an interactive npm login, then configure their trust
settings above. The existing core package can use OIDC without that bootstrap.

## Archive contents

```bash
cd ts
pnpm install --frozen-lockfile
pnpm publish:packages:dry-run
```

`dist-npm/` contains the archives and an inventory with SHA-512 checksums. The
publisher verifies every archive's checksum, name, and version before uploading
any package. The dry run uses the real packing and npm publish validation paths
without writing to the registry.

Within npm archives, Hashtree dependencies use exact registry versions from this
checkout, and `repository` identifies the GitHub repository and package directory
required for provenance. The source manifests and separate immutable runtime
archives retain their existing release URLs. External FIPS dependencies can
still use release URLs; npm 12 consumers of those packages need
`--allow-remote=all` when installing.
