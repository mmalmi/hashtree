# tauri-plugin-hashtree-updater

In-app updates for Tauri v2 apps, discovered and downloaded over hashtree.

The plugin is headless — it exposes a small `check()` / `downloadAndInstall()`
API and apps render their own banner + settings UI. A drop-in helper module
that handles the common bits (localStorage prefs, auto-check throttling,
dismissed-version tracking) is included below so you can paste it into a new
app and just write the template.

## How update discovery works

A signed Nostr mutable root (`htree://npub.../<tree>/<channel>/latest`) points
at the latest release directory. The plugin resolves it, reads the manifest,
picks the asset matching the current platform, and reads the bytes. Hashtree
itself authenticates everything — the resolved root CID transitively pins
every chunk, so no minisign key is required.

The expected manifest is the `release.json` that hashtree-first apps already
write today (see `squirreldisk/scripts/release.mjs` and
`nostr-vpn/scripts/local-release-lib.mjs`):

```json
{
  "tag": "v0.3.12",
  "commit": "deadbeef…",
  "assets": [
    { "name": "myapp-v0.3.12-linux-arm64.AppImage", "path": "assets/myapp-v0.3.12-linux-arm64.AppImage" },
    { "name": "myapp-v0.3.12-macos-arm64.app.tar.gz", "path": "assets/myapp-v0.3.12-macos-arm64.app.tar.gz" }
  ]
}
```

Asset `target` and `kind` are inferred from the filename when missing
(`-linux-arm64.AppImage` → `linux-aarch64` + AppImage, `-macos-arm64.app.tar.gz`
→ `darwin-aarch64` + AppBundle, `-windows-x64.exe` → `windows-x86_64` + NSIS,
etc.). To override, add `target` / `targets` / `kind` per asset in the JSON.

Supported install kinds: `binary`, `app-bundle` (macOS), `appimage` (Linux).
`deb` / `rpm` / `nsis` / `msi` / `archive` are recognised but not yet auto-
installed — they return `UnsupportedKind` and the UI should fall back to
"Open release page" or similar.

## Setup (per app)

### `src-tauri/Cargo.toml`

```toml
tauri-plugin-hashtree-updater = "0.2"  # or path = "..." while developing
```

### `src-tauri/src/lib.rs` (or `main.rs`)

```rust
tauri::Builder::default()
    .plugin(tauri_plugin_hashtree_updater::init())
    // ...
```

### `src-tauri/tauri.conf.json`

```json
"plugins": {
  "hashtree-updater": {
    "reference": "htree://npub.../releases%2Fmyapp/latest"
  }
}
```

Optional fields: `manifestPath` (default `release.json`), `destination`
(default: detected from `current_exe()` per asset kind), `relays`,
`blossomServers`.

### `src-tauri/capabilities/desktop.json`

```json
{ "permissions": ["hashtree-updater:default"] }
```

### TypeScript glue

There is no published npm package — copy `guest-js/index.ts` from this crate
into your app (eg `src/lib/updater-api.ts`) so the JS side has thin wrappers
around `invoke('plugin:hashtree-updater|check')` and the
`download_and_install` channel. It's ~70 lines and only depends on
`@tauri-apps/api/core`.

## JS API

```ts
import { check, Update, type DownloadEvent } from './updater-api'

const update: Update | null = await check()
// update?.updateAvailable, update?.version, update?.currentVersion,
// update?.assetName, update?.assetKind, update?.notes, update?.publishedAt

await update?.downloadAndInstall(
  (event: DownloadEvent) => {
    // { event: 'started', data: { contentLength?: number } }
    // { event: 'progress', data: { chunkLength: number; downloaded: number } }
    // { event: 'finished', data: { total: number } }
  },
  { /* destination?, kind?, executable? — all optional overrides */ },
)
```

After install completes, the user must restart the app. The plugin doesn't
relaunch automatically.

## Reference helper module (~70 lines)

Drop this into your app as `src/lib/updater.ts` — it handles the common
prefs/auto-check/dismissal logic so your banner just renders state. Change
the storage key to your app's name.

```ts
import { check as pluginCheck, Update, type DownloadEvent } from './updater-api'

const PREFS_KEY = 'myapp.updater.prefs.v1'
const DEFAULT_AUTO_CHECK_INTERVAL_MS = 6 * 60 * 60 * 1000 // 6h

export interface UpdaterPrefs {
  autoCheck: boolean              // run check on app start (throttled)
  autoInstall: boolean            // install without prompting (implies autoCheck)
  lastCheckMs: number             // ms since epoch of last check attempt
  lastNotifiedVersion: string | null
  dismissedVersion: string | null // user clicked X — don't re-prompt for this version
}

const DEFAULT_PREFS: UpdaterPrefs = {
  autoCheck: true,
  autoInstall: false,
  lastCheckMs: 0,
  lastNotifiedVersion: null,
  dismissedVersion: null,
}

export function loadPrefs(): UpdaterPrefs {
  try {
    const raw = localStorage.getItem(PREFS_KEY)
    return raw ? { ...DEFAULT_PREFS, ...JSON.parse(raw) } : { ...DEFAULT_PREFS }
  } catch {
    return { ...DEFAULT_PREFS }
  }
}

export function savePrefs(prefs: UpdaterPrefs): void {
  try { localStorage.setItem(PREFS_KEY, JSON.stringify(prefs)) } catch {}
}

export function patchPrefs(patch: Partial<UpdaterPrefs>): UpdaterPrefs {
  const next = { ...loadPrefs(), ...patch }
  if (next.autoInstall) next.autoCheck = true       // install implies check
  savePrefs(next)
  return next
}

export async function checkForUpdate(): Promise<Update | null> {
  return pluginCheck()
}

export async function maybeAutoCheck(
  intervalMs: number = DEFAULT_AUTO_CHECK_INTERVAL_MS,
): Promise<Update | null> {
  const prefs = loadPrefs()
  if (!prefs.autoCheck) return null
  if (Date.now() - prefs.lastCheckMs < intervalMs) return null
  try {
    const update = await pluginCheck()
    patchPrefs({ lastCheckMs: Date.now() })
    return update
  } catch {
    return null
  }
}

export async function downloadAndInstall(
  update: Update,
  onEvent?: (event: DownloadEvent) => void,
): Promise<void> {
  await update.downloadAndInstall(onEvent)
  patchPrefs({ lastNotifiedVersion: update.version })
}

export type { Update, DownloadEvent }
```

## Recommended UX

A two-piece UI works well — see `squirreldisk` (React) and `nostr-vpn`
(Svelte) for working examples.

**Banner** (top of window, dismissible):
- Mount-time: `const update = await maybeAutoCheck()`
- If `update?.updateAvailable` and `prefs.dismissedVersion !== update.version`,
  show a stripe with "Update available: vX (you're on vY)", an "Install"
  button, an "Install automatically" checkbox bound to `prefs.autoInstall`,
  and a `×` that calls `patchPrefs({ dismissedVersion: update.version })`.
- During install, swap the message for "Downloading vX — N%" using the
  `progress` event's `downloaded / total`.
- After `finished`: "Installed vX. Restart MyApp to apply."

**Settings panel "Updates" section**:
- Current version (from `@tauri-apps/api/app#getVersion`)
- "Check for updates" button → `checkForUpdate()`, render result inline
- "Last checked …" timestamp from `prefs.lastCheckMs`
- Toggle "Check for updates automatically" (default ON; disable while
  `autoInstall` is on since install implies check)
- Toggle "Install updates automatically" (default OFF; label note "applies
  on next start")

Defaults: auto-check ON, auto-install OFF. The banner is the primary
discovery surface; the settings panel is the override.

## Handling missing manifests

When the configured `reference` resolves but the release tree has no
`release.json` (eg first deploy of a new app), the plugin returns
`ManifestNotFound`. Treat that as "no update" and stay quiet — don't surface
it as an error in the banner. Real network errors should also be silent on
auto-checks; only surface them on manual "Check for updates" presses.
