import test from 'node:test'
import assert from 'node:assert/strict'
import { existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from 'node:fs'
import os from 'node:os'
import { join } from 'node:path'

import { parseArgs, stageRepoRelease } from './stage_repo_release.mjs'

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
    '--install-url',
    'https://upload.example/releases%2Fhashtree/latest/install.sh',
    '--title',
    'v0.2.16',
  ])

  assert.equal(parsed.tag, 'v0.2.16')
  assert.equal(parsed.commit, 'abc123')
  assert.equal(parsed.cliDir, '/tmp/cli')
  assert.equal(parsed.outputDir, '/tmp/out')
  assert.equal(parsed.installUrl, 'https://upload.example/releases%2Fhashtree/latest/install.sh')
  assert.equal(parsed.title, 'v0.2.16')
})

test('stageRepoRelease creates a metadata-backed repo release directory', () => {
  const tempDir = mkdtempSync(join(os.tmpdir(), 'stage-repo-release-'))

  try {
    const cliDir = join(tempDir, 'cli')
    const outputDir = join(tempDir, 'out')

    mkdirSync(cliDir, { recursive: true })

    writeFileSync(join(cliDir, 'install.sh'), '#!/bin/sh\necho install\n')
    writeFileSync(join(cliDir, 'hashtree-aarch64-apple-darwin.tar.gz'), 'cli-tar')

    const result = stageRepoRelease({
      tag: 'v0.2.16',
      commit: 'abc123',
      cliDir,
      outputDir,
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
    assert.match(notes, /curl -fsSL https:\/\/upload\.example\/releases%2Fhashtree\/latest\/install\.sh \| sh/)
    assert.match(notes, /Install with shell:/)
    assert.match(notes, /Manual macOS\/Linux install: download the archive for your platform from the release assets below/)
    assert.doesNotMatch(notes, /## Downloads/)
    assert.doesNotMatch(notes, /hashtree-aarch64-apple-darwin\.sha256/)
    assert.doesNotMatch(notes, /hashtree-aarch64-apple-darwin\.tar\.gz/)
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
