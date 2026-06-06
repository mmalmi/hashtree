#!/usr/bin/env node

import { spawnSync } from 'node:child_process'
import { existsSync, mkdirSync, rmSync } from 'node:fs'
import { dirname, resolve } from 'node:path'
import process from 'node:process'
import { fileURLToPath } from 'node:url'

const scriptPath = fileURLToPath(import.meta.url)
const scriptDir = dirname(scriptPath)
const rustDir = dirname(scriptDir)
const repoDir = dirname(rustDir)
const sourceRootDir = dirname(repoDir)
export const requiredSiblingSourceDirs = ['cashu-service', 'cashu_spilman_channels', 'fips']
const siblingSourceCopies = [
  {
    name: 'cashu-service',
    paths: ['cashu-service'],
    excludes: ['cashu-service/.git', 'cashu-service/target', 'cashu-service/dist'],
  },
  {
    name: 'cashu_spilman_channels',
    paths: ['cashu_spilman_channels'],
    excludes: [
      'cashu_spilman_channels/.git',
      'cashu_spilman_channels/target',
      'cashu_spilman_channels/dist',
    ],
  },
  {
    name: 'fips',
    paths: ['fips/Cargo.toml', 'fips/Cargo.lock', 'fips/crates'],
    excludes: [],
  },
]

function usage() {
  return `Usage: node rust/scripts/build_windows_vm_artifacts.mjs --output-dir <dir> [options]

Build Windows CLI binaries on the win11-dev VM (reachable over the Nostr VPN
mesh — see ~/.claude/CLAUDE.md) and copy the resulting .exe files into a host
output directory.

Options:
  --output-dir <dir>             Host output directory for built .exe files
  --ssh-host <host>              SSH host (default: win11-dev)
  --guest-repo-path <path>       Override the guest repo path used for the build
                                 (default: C:\\src\\hashtree)
  -h, --help                     Show this help

Legacy options (accepted, mapped onto the SSH flow):
  --vm-name <name>               Treated as --ssh-host
  --shared-repo-path <path>      Ignored (no longer using Parallels shared folders)
`
}

export function parseArgs(argv) {
  const args = [...argv]
  const options = {
    outputDir: '',
    sshHost: '',
    guestRepoPath: '',
    help: false,
  }

  for (let index = 0; index < args.length; index += 1) {
    const arg = args[index]
    switch (arg) {
      case '--output-dir':
        options.outputDir = resolve(args[++index] ?? '')
        break
      case '--ssh-host':
      case '--vm-name': // legacy alias
        options.sshHost = args[++index] ?? ''
        break
      case '--guest-repo-path':
        options.guestRepoPath = args[++index] ?? ''
        break
      case '--shared-repo-path': // legacy, ignored
        ++index
        break
      case '--help':
      case '-h':
        options.help = true
        break
      default:
        throw new Error(`Unknown argument: ${arg}`)
    }
  }

  return options
}

function run(command, args, { capture = false, env = {}, input } = {}) {
  const result = spawnSync(command, args, {
    encoding: capture ? 'utf8' : undefined,
    stdio: input
      ? ['pipe', capture ? 'pipe' : 'inherit', capture ? 'pipe' : 'inherit']
      : capture
        ? ['ignore', 'pipe', 'pipe']
        : 'inherit',
    env: { ...process.env, ...env },
    input,
  })

  if (result.error) {
    throw result.error
  }
  if (result.status !== 0) {
    if (capture) {
      const output = [result.stdout, result.stderr].filter(Boolean).join('').trim()
      throw new Error(output || `${command} exited with status ${result.status}`)
    }
    throw new Error(`${command} exited with status ${result.status}`)
  }

  return capture ? result.stdout.trim() : ''
}

function runShellPipe(cmd) {
  const result = spawnSync('bash', ['-c', cmd], { stdio: ['ignore', 'inherit', 'inherit'] })
  if (result.status !== 0) {
    throw new Error(`shell pipe failed (exit ${result.status}): ${cmd}`)
  }
}

function shQuote(value) {
  return `'${String(value).replace(/'/g, `'"'"'`)}'`
}

function psQuote(value) {
  return `'${String(value).replace(/'/g, "''")}'`
}

function encodePowerShellScript(script) {
  return Buffer.from(script, 'utf16le').toString('base64')
}

function guestParentForwardPath(guestRepo) {
  const normalized = guestRepo.replace(/\\/g, '/').replace(/\/+$/, '')
  return normalized.replace(/\/[^/]+$/, '')
}

function runRemotePowerShell(host, script, { capture = false } = {}) {
  const encoded = encodePowerShellScript(script)
  return run('ssh', [host, 'powershell.exe', '-NoProfile', '-EncodedCommand', encoded], { capture })
}

/// Build the PowerShell script that runs on win11-dev. Mirrors the old
/// cmd-batch but uses bsdtar's stdin extract — the source archive is piped
/// in over SSH, no shared folders are involved.
export function windowsBuildScriptLines({ guestRepoPath }) {
  const guestRepoValue = guestRepoPath || 'C:\\src\\hashtree'
  return [
    '$ErrorActionPreference = "Stop"',
    `$guestRepo = '${guestRepoValue.replace(/'/g, "''")}'`,
    '$guestParent = Split-Path $guestRepo',
    'New-Item -ItemType Directory -Force -Path $guestParent | Out-Null',
    'if (Test-Path $guestRepo) { Remove-Item -Recurse -Force $guestRepo }',
    "foreach ($sibling in @('cashu-service', 'cashu_spilman_channels', 'fips')) {",
    '  $siblingPath = Join-Path $guestParent $sibling',
    '  if (Test-Path $siblingPath) { Remove-Item -Recurse -Force $siblingPath -ErrorAction SilentlyContinue }',
    '}',
    'New-Item -ItemType Directory -Force -Path $guestRepo | Out-Null',
    '$vswhere = Join-Path ${env:ProgramFiles(x86)} "Microsoft Visual Studio\\Installer\\vswhere.exe"',
    'if (-not (Test-Path $vswhere)) { throw "vswhere.exe not found at $vswhere" }',
    "$vsInstall = (& $vswhere -latest -products * -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath 2>$null | Select-Object -First 1)",
    'if (-not $vsInstall) { throw "VS x64 toolchain not found via vswhere" }',
    '$vsDevCmd = Join-Path $vsInstall "Common7\\Tools\\VsDevCmd.bat"',
    'if (-not (Test-Path $vsDevCmd)) { throw "VsDevCmd.bat not found at $vsDevCmd" }',
    '$envOutput = cmd /d /s /c "call `"$vsDevCmd`" -arch=amd64 -host_arch=amd64 >nul && set"',
    'foreach ($line in $envOutput) {',
    '  if ($line -match "^([^=]+)=(.*)$") { Set-Item -Path ("Env:\\" + $matches[1]) -Value $matches[2] }',
    '}',
  ]
}

export function buildWindowsVmArtifacts({
  outputDir,
  sshHost = '',
  guestRepoPath = '',
}) {
  if (!outputDir) {
    throw new Error('Missing --output-dir')
  }

  const host = sshHost || process.env.HASHTREE_WINDOWS_SSH_HOST || 'win11-dev'
  const guestRepo =
    guestRepoPath || process.env.HASHTREE_WINDOWS_GUEST_REPO_PATH || 'C:\\src\\hashtree'
  const guestRepoForward = guestRepo.replace(/\\/g, '/')
  const guestParentForward = guestParentForwardPath(guestRepo)

  if (!existsSync(resolve(repoDir, 'rust'))) {
    throw new Error(`Expected ${repoDir} to contain a rust workspace directory.`)
  }
  for (const name of requiredSiblingSourceDirs) {
    const sourceDir = resolve(sourceRootDir, name)
    if (!existsSync(sourceDir)) {
      throw new Error(`Expected sibling source directory for Windows release build: ${sourceDir}`)
    }
  }

  const resolvedOutputDir = resolve(outputDir)
  rmSync(resolvedOutputDir, { recursive: true, force: true })
  mkdirSync(resolvedOutputDir, { recursive: true })

  // 1. Reset the guest repo dir.
  runRemotePowerShell(
    host,
    `
$ErrorActionPreference = 'Stop'
$guestRepo = ${psQuote(guestRepo)}
$guestParent = Split-Path $guestRepo
New-Item -ItemType Directory -Force -Path $guestParent | Out-Null
if (Test-Path $guestRepo) { Remove-Item -Recurse -Force $guestRepo }
foreach ($sibling in @('cashu-service', 'cashu_spilman_channels', 'fips')) {
  $siblingPath = Join-Path $guestParent $sibling
  if (Test-Path $siblingPath) { Remove-Item -Recurse -Force $siblingPath -ErrorAction SilentlyContinue }
}
New-Item -ItemType Directory -Force -Path $guestRepo | Out-Null
`,
  )

  // 2. Push the rust workspace and sibling path dependencies via tar over SSH.
  runShellPipe(
    `tar --exclude=./rust/target --exclude=./rust/dist -cf - -C ${shQuote(repoDir)} rust ` +
      `| ssh ${shQuote(host)} tar -xf - -C ${shQuote(guestRepoForward)}`,
  )
  for (const copy of siblingSourceCopies) {
    const excludeArgs = copy.excludes.map((exclude) => `--exclude=${shQuote(exclude)}`).join(' ')
    const sourceArgs = copy.paths.map((sourcePath) => shQuote(sourcePath)).join(' ')
    runShellPipe(
      `tar ${excludeArgs} -cf - -C ${shQuote(sourceRootDir)} ${sourceArgs} ` +
        `| ssh ${shQuote(host)} tar -xf - -C ${shQuote(guestParentForward)}`,
    )
  }

  // 3. Build inside MSVC environment and stage outputs into a guest dir.
  const guestOutDir = `${guestRepo}\\dist\\windows-out`
  const guestOutForward = guestOutDir.replace(/\\/g, '/')
  runRemotePowerShell(
    host,
    `
$ErrorActionPreference = 'Stop'
$guestRepo = ${psQuote(guestRepo)}
$guestOut = ${psQuote(guestOutDir)}
if (Test-Path $guestOut) { Remove-Item -Recurse -Force $guestOut }
New-Item -ItemType Directory -Force -Path $guestOut | Out-Null

$vswhere = Join-Path \${env:ProgramFiles(x86)} 'Microsoft Visual Studio\\Installer\\vswhere.exe'
if (-not (Test-Path $vswhere)) { throw "vswhere.exe not found at $vswhere" }
$vsInstall = (& $vswhere -latest -products * -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath 2>$null | Select-Object -First 1)
if (-not $vsInstall) { throw "VS x64 toolchain not found via vswhere" }
$vsDevCmd = Join-Path $vsInstall 'Common7\\Tools\\VsDevCmd.bat'
if (-not (Test-Path $vsDevCmd)) { throw "VsDevCmd.bat not found at $vsDevCmd" }
$envLines = cmd /d /s /c ('"' + $vsDevCmd + '" -arch=amd64 -host_arch=amd64 >nul && set')
foreach ($line in $envLines) {
  if ($line -match '^([^=]+)=(.*)$') { Set-Item -Path ('Env:\\' + $matches[1]) -Value $matches[2] }
}

Set-Location (Join-Path $guestRepo 'rust')
$env:CARGO_NET_GIT_FETCH_WITH_CLI = 'false'
$lockedFlag = @()
if (Test-Path (Join-Path $guestRepo 'rust\\Cargo.lock')) { $lockedFlag = @('--locked') }

cargo build --release @lockedFlag --target x86_64-pc-windows-msvc -p hashtree-cli --bin htree
if ($LASTEXITCODE -ne 0) { throw "htree build failed" }
cargo build --release @lockedFlag --target x86_64-pc-windows-msvc -p hashtree-cashu-cli --bin htree-cashu
if ($LASTEXITCODE -ne 0) { throw "htree-cashu build failed" }
cargo build --release @lockedFlag --target x86_64-pc-windows-msvc -p git-remote-htree --bin git-remote-htree
if ($LASTEXITCODE -ne 0) { throw "git-remote-htree build failed" }

$releaseDir = Join-Path $guestRepo 'rust\\target\\x86_64-pc-windows-msvc\\release'
foreach ($name in @('htree.exe', 'htree-cashu.exe', 'git-remote-htree.exe')) {
  Copy-Item -Force (Join-Path $releaseDir $name) (Join-Path $guestOut $name)
}
`,
  )

  // 4. Pull artifacts back.
  runShellPipe(
    `ssh ${shQuote(host)} tar -cf - -C ${shQuote(guestOutForward)} . ` +
      `| tar -xf - -C ${shQuote(resolvedOutputDir)}`,
  )

  return {
    outputDir: resolvedOutputDir,
    sshHost: host,
    guestRepoPath: guestRepo,
  }
}

function main() {
  const options = parseArgs(process.argv.slice(2))
  if (options.help) {
    console.log(usage())
    return
  }
  if (!options.outputDir) {
    throw new Error('Missing --output-dir')
  }

  const result = buildWindowsVmArtifacts(options)
  console.log(`Built Windows CLI artifacts in ${result.outputDir} via ssh ${result.sshHost}.`)
}

if (process.argv[1] === scriptPath) {
  try {
    main()
  } catch (error) {
    console.error(error instanceof Error ? error.message : String(error))
    process.exit(1)
  }
}
