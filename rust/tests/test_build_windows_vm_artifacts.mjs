import test from 'node:test'
import assert from 'node:assert/strict'

import {
  parseArgs,
  windowsBuildScriptLines,
} from '../scripts/build_windows_vm_artifacts.mjs'

test('parseArgs reads new and legacy options', () => {
  const parsed = parseArgs([
    '--output-dir',
    '/tmp/out',
    '--ssh-host',
    'win11-dev',
    '--guest-repo-path',
    'C:\\src\\hashtree',
  ])

  assert.equal(parsed.outputDir.endsWith('/tmp/out'), true)
  assert.equal(parsed.sshHost, 'win11-dev')
  assert.equal(parsed.guestRepoPath, 'C:\\src\\hashtree')
  assert.equal(parsed.help, false)
})

test('parseArgs accepts legacy --vm-name as --ssh-host alias', () => {
  const parsed = parseArgs([
    '--output-dir',
    '/tmp/out',
    '--vm-name',
    'Windows 11',
  ])

  assert.equal(parsed.sshHost, 'Windows 11')
})

test('parseArgs ignores legacy --shared-repo-path', () => {
  const parsed = parseArgs([
    '--output-dir',
    '/tmp/out',
    '--shared-repo-path',
    'C:\\Mac\\Home\\src\\hashtree',
  ])

  assert.equal(parsed.outputDir.endsWith('/tmp/out'), true)
})

test('windowsBuildScriptLines emits a PowerShell-friendly preamble', () => {
  const lines = windowsBuildScriptLines({ guestRepoPath: 'C:\\src\\hashtree' })

  // Old cmd-batch directives should be gone.
  assert.equal(lines.findIndex((line) => line.startsWith('robocopy ')), -1)
  assert.equal(lines.findIndex((line) => line.startsWith('@echo off')), -1)

  // Should set up the guest repo dir under the new path.
  const guestRepoLine = lines.find((line) => line.includes("$guestRepo = 'C:\\src\\hashtree'"))
  assert.ok(guestRepoLine, `expected $guestRepo assignment, got ${JSON.stringify(lines)}`)
})
