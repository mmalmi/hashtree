#!/usr/bin/env node

import { writeFileSync } from 'node:fs'
import { resolve } from 'node:path'
import process from 'node:process'

import { collectReleaseAssetEntries, readChangelogEntry, renderReleaseNotes } from './stage_repo_release.mjs'

function usage() {
  return `Usage: node scripts/render_repo_release_notes.mjs --tag <tag> --commit <commit> --cli-dir <dir> --output-file <path> [options]

Render release notes using the same generator as staged htree repo releases.

Options:
  --tag <tag>               Release tag (for example: v0.2.16)
  --commit <sha>            Commit hash for release notes
  --cli-dir <dir>           Directory containing CLI release assets
  --changelog-file <path>   Optional changelog entry to splice into the notes
  --install-url <url>       Optional bootstrap install URL to include in notes
  --output-file <path>      File to write the rendered notes into
  -h, --help                Show this help
`
}

function parseArgs(argv) {
  const args = [...argv]
  const options = {
    tag: '',
    commit: '',
    cliDir: '',
    changelogFile: '',
    installUrl: '',
    outputFile: '',
  }

  for (let index = 0; index < args.length; index += 1) {
    const arg = args[index]
    switch (arg) {
      case '--tag':
        options.tag = args[++index] ?? ''
        break
      case '--commit':
        options.commit = args[++index] ?? ''
        break
      case '--cli-dir':
        options.cliDir = resolve(args[++index] ?? '')
        break
      case '--changelog-file':
        options.changelogFile = resolve(args[++index] ?? '')
        break
      case '--install-url':
        options.installUrl = args[++index] ?? ''
        break
      case '--output-file':
        options.outputFile = resolve(args[++index] ?? '')
        break
      case '--help':
      case '-h':
        return { help: true }
      default:
        throw new Error(`Unknown argument: ${arg}`)
    }
  }

  return options
}

function main() {
  const options = parseArgs(process.argv.slice(2))
  if (options.help) {
    console.log(usage())
    return
  }
  if (!options.tag) {
    throw new Error('Missing --tag')
  }
  if (!options.commit) {
    throw new Error('Missing --commit')
  }
  if (!options.cliDir) {
    throw new Error('Missing --cli-dir')
  }
  if (!options.outputFile) {
    throw new Error('Missing --output-file')
  }

  const assetEntries = collectReleaseAssetEntries({ cliDir: options.cliDir })
  const changelogEntry = options.changelogFile ? readChangelogEntry(options.changelogFile, options.tag) : ''
  if (assetEntries.length === 0) {
    throw new Error('No release assets found to render')
  }

  writeFileSync(
    options.outputFile,
    renderReleaseNotes({
      tag: options.tag,
      commit: options.commit,
      assetEntries,
      changelogEntry,
      installUrl: options.installUrl,
    }),
  )
}

try {
  main()
} catch (error) {
  console.error(error instanceof Error ? error.message : String(error))
  process.exit(1)
}
