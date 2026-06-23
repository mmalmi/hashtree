import test from 'node:test'
import assert from 'node:assert/strict'
import { existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from 'node:fs'
import os from 'node:os'
import { join } from 'node:path'

import { parseArgs, readChangelogEntry, stageRepoRelease } from './stage_repo_release.mjs'

test('parseArgs accepts release staging inputs', () => {
  const parsed = parseArgs([
    '--tag',
    'v0.2.16',
    '--commit',
    'abc123',
    '--cli-dir',
    '/tmp/cli',
    '--output-dir',
    '/tmp/out',
    '--changelog-file',
    '/tmp/changelog.md',
    '--install-url',
    'https://upload.example/releases%2Fhashtree/latest/install.sh',
    '--title',
    'v0.2.16',
  ])

  assert.equal(parsed.tag, 'v0.2.16')
  assert.equal(parsed.commit, 'abc123')
  assert.equal(parsed.cliDir, '/tmp/cli')
  assert.equal(parsed.outputDir, '/tmp/out')
  assert.equal(parsed.changelogFile, '/tmp/changelog.md')
  assert.equal(parsed.installUrl, 'https://upload.example/releases%2Fhashtree/latest/install.sh')
  assert.equal(parsed.title, 'v0.2.16')
})

test('readChangelogEntry extracts the requested version body', () => {
  const tempDir = mkdtempSync(join(os.tmpdir(), 'stage-repo-release-changelog-'))

  try {
    const changelogFile = join(tempDir, 'CHANGELOG.md')
    writeFileSync(
      changelogFile,
      `# Changelog

## Unreleased

## 0.2.16 - 2026-04-16

Changes since the previous release.

### Improved

- Added release changelog coverage.

## 0.2.15 - 2026-04-15

Previous entry.
`,
    )

    assert.equal(
      readChangelogEntry(changelogFile, 'v0.2.16'),
      `Changes since the previous release.

### Improved

- Added release changelog coverage.`,
    )
  } finally {
    rmSync(tempDir, { recursive: true, force: true })
  }
})

test('stageRepoRelease creates a metadata-backed repo release directory', () => {
  const tempDir = mkdtempSync(join(os.tmpdir(), 'stage-repo-release-'))

  try {
    const cliDir = join(tempDir, 'cli')
    const changelogFile = join(tempDir, 'CHANGELOG.md')
    const outputDir = join(tempDir, 'out')

    mkdirSync(cliDir, { recursive: true })

    writeFileSync(join(cliDir, 'install.sh'), '#!/bin/sh\necho install\n')
    writeFileSync(join(cliDir, 'hashtree-aarch64-apple-darwin.tar.gz'), 'cli-tar')
    writeFileSync(
      changelogFile,
      `# Changelog

## 0.2.16 - 2026-04-16

Changes since the previous release.

### Improved

- Added release changelog coverage.
`,
    )

    const result = stageRepoRelease({
      tag: 'v0.2.16',
      commit: 'abc123',
      cliDir,
      outputDir,
      changelogFile,
      installUrl: 'https://upload.example/releases%2Fhashtree/latest/install.sh',
    })

    assert.equal(result.assetCount, 2)
    assert.equal(existsSync(join(outputDir, 'release.json')), true)
    assert.equal(existsSync(join(outputDir, 'notes.md')), true)
    assert.equal(existsSync(join(outputDir, 'install.sh')), true)
    assert.equal(existsSync(join(outputDir, 'assets', 'hashtree-aarch64-apple-darwin.tar.gz')), true)

    const manifest = JSON.parse(readFileSync(join(outputDir, 'release.json'), 'utf8'))
    assert.deepEqual(
      manifest.assets.map((asset) => asset.path),
      ['assets/hashtree-aarch64-apple-darwin.tar.gz', 'install.sh'],
    )

    const notes = readFileSync(join(outputDir, 'notes.md'), 'utf8')
    assert.doesNotMatch(notes, /curl -fsSL .* \| sh/)
    assert.match(notes, /verifies the signed release checksum manifest/)
    assert.match(notes, /Manual macOS\/Linux install: download the archive for your platform from the release assets below/)
    assert.match(notes, /## Changelog/)
    assert.match(notes, /### Improved/)
    assert.match(notes, /Added release changelog coverage\./)
    assert.doesNotMatch(notes, /## Downloads/)
    assert.doesNotMatch(notes, /hashtree-aarch64-apple-darwin\.sha256/)
    assert.doesNotMatch(notes, /hashtree-aarch64-apple-darwin\.tar\.gz/)
  } finally {
    rmSync(tempDir, { recursive: true, force: true })
  }
})

test('stageRepoRelease includes signed checksum manifest assets', () => {
  const tempDir = mkdtempSync(join(os.tmpdir(), 'stage-repo-release-signed-sums-'))

  try {
    const cliDir = join(tempDir, 'cli')
    const outputDir = join(tempDir, 'out')

    mkdirSync(cliDir, { recursive: true })
    writeFileSync(join(cliDir, 'install.sh'), '#!/bin/sh\necho install\n')
    writeFileSync(join(cliDir, 'hashtree-aarch64-apple-darwin.tar.gz'), 'cli-tar')
    writeFileSync(join(cliDir, 'SHA256SUMS'), 'abc  hashtree-aarch64-apple-darwin.tar.gz\n')
    writeFileSync(join(cliDir, 'SHA256SUMS.sig'), 'signature')

    const result = stageRepoRelease({
      tag: 'v0.2.16',
      commit: '112233',
      cliDir,
      outputDir,
    })

    assert.equal(result.assetCount, 4)
    assert.equal(existsSync(join(outputDir, 'assets', 'SHA256SUMS')), true)
    assert.equal(existsSync(join(outputDir, 'assets', 'SHA256SUMS.sig')), true)

    const manifest = JSON.parse(readFileSync(join(outputDir, 'release.json'), 'utf8'))
    assert.deepEqual(
      manifest.assets.map((asset) => asset.path),
      [
        'assets/hashtree-aarch64-apple-darwin.tar.gz',
        'install.sh',
        'assets/SHA256SUMS',
        'assets/SHA256SUMS.sig',
      ],
    )
  } finally {
    rmSync(tempDir, { recursive: true, force: true })
  }
})

test('stageRepoRelease fails when the changelog entry is missing', () => {
  const tempDir = mkdtempSync(join(os.tmpdir(), 'stage-repo-release-missing-changelog-'))

  try {
    const cliDir = join(tempDir, 'cli')
    const changelogFile = join(tempDir, 'CHANGELOG.md')
    const outputDir = join(tempDir, 'out')

    mkdirSync(cliDir, { recursive: true })
    writeFileSync(join(cliDir, 'install.sh'), '#!/bin/sh\necho install\n')
    writeFileSync(join(cliDir, 'hashtree-aarch64-apple-darwin.tar.gz'), 'cli-tar')
    writeFileSync(changelogFile, '# Changelog\n\n## 0.2.15 - 2026-04-15\n\nPrevious release.\n')

    assert.throws(
      () =>
        stageRepoRelease({
          tag: 'v0.2.16',
          commit: 'abc123',
          cliDir,
          outputDir,
          changelogFile,
        }),
      /Missing changelog entry for 0\.2\.16/,
    )
  } finally {
    rmSync(tempDir, { recursive: true, force: true })
  }
})

test('stageRepoRelease excludes checksum sidecar files from staged assets', () => {
  const tempDir = mkdtempSync(join(os.tmpdir(), 'stage-repo-release-no-sha-'))

  try {
    const cliDir = join(tempDir, 'cli')
    const outputDir = join(tempDir, 'out')

    mkdirSync(cliDir, { recursive: true })
    writeFileSync(join(cliDir, 'install.sh'), '#!/bin/sh\necho install\n')
    writeFileSync(join(cliDir, 'hashtree-aarch64-apple-darwin.tar.gz'), 'cli-tar')
    writeFileSync(join(cliDir, 'hashtree-aarch64-apple-darwin.sha256'), 'cli-sha')

    const result = stageRepoRelease({
      tag: 'v0.2.16',
      commit: '112233',
      cliDir,
      outputDir,
    })

    assert.equal(result.assetCount, 2)
    assert.equal(existsSync(join(outputDir, 'assets', 'hashtree-aarch64-apple-darwin.sha256')), false)

    const manifest = JSON.parse(readFileSync(join(outputDir, 'release.json'), 'utf8'))
    assert.deepEqual(
      manifest.assets.map((asset) => asset.path),
      ['assets/hashtree-aarch64-apple-darwin.tar.gz', 'install.sh'],
    )
  } finally {
    rmSync(tempDir, { recursive: true, force: true })
  }
})

test('stageRepoRelease notes include Windows CLI install instructions when a zip asset is present', () => {
  const tempDir = mkdtempSync(join(os.tmpdir(), 'stage-repo-release-windows-cli-'))

  try {
    const cliDir = join(tempDir, 'cli')
    const outputDir = join(tempDir, 'out')

    mkdirSync(cliDir, { recursive: true })

    writeFileSync(join(cliDir, 'install.sh'), '#!/bin/sh\necho install\n')
    writeFileSync(join(cliDir, 'hashtree-x86_64-pc-windows-msvc.zip'), 'cli-zip')

    stageRepoRelease({
      tag: 'v0.2.16',
      commit: 'fedcba',
      cliDir,
      outputDir,
      installUrl: 'https://upload.example/releases%2Fhashtree/latest/install.sh',
    })

    const notes = readFileSync(join(outputDir, 'notes.md'), 'utf8')
    assert.match(notes, /Windows x64 CLI: download the zip asset below/)
    assert.match(notes, /git-remote-htree\.exe/)
    assert.doesNotMatch(notes, /Manual install: download the archive for your platform/)
  } finally {
    rmSync(tempDir, { recursive: true, force: true })
  }
})
