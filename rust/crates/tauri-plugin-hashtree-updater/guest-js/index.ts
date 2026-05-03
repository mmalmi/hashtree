import { invoke, Channel } from '@tauri-apps/api/core'

export interface UpdateMetadata {
  currentVersion: string
  version: string
  assetName: string
  assetKind: string
  notes?: string
  publishedAt?: string
  updateAvailable: boolean
}

export type DownloadEvent =
  | { event: 'started'; data: { contentLength?: number } }
  | { event: 'progress'; data: { chunkLength: number; downloaded: number } }
  | { event: 'finished'; data: { total: number } }

export interface InstallOptions {
  /** Override the install destination provided in `tauri.conf.json`. */
  destination?: string
  /** Override the asset kind reported by the manifest. */
  kind?: string
  /** Set the unix executable bit after install (binary kind only). */
  executable?: boolean
}

export class Update {
  readonly currentVersion: string
  readonly version: string
  readonly assetName: string
  readonly assetKind: string
  readonly notes?: string
  readonly publishedAt?: string
  readonly updateAvailable: boolean

  constructor(meta: UpdateMetadata) {
    this.currentVersion = meta.currentVersion
    this.version = meta.version
    this.assetName = meta.assetName
    this.assetKind = meta.assetKind
    this.notes = meta.notes
    this.publishedAt = meta.publishedAt
    this.updateAvailable = meta.updateAvailable
  }

  /**
   * Download the matching asset and install it via the platform-specific
   * dispatcher (binary swap, .app bundle on macOS, AppImage on Linux).
   */
  async downloadAndInstall(
    onEvent?: (event: DownloadEvent) => void,
    options?: InstallOptions
  ): Promise<UpdateMetadata> {
    const channel = new Channel<DownloadEvent>()
    if (onEvent) channel.onmessage = onEvent
    return await invoke<UpdateMetadata>(
      'plugin:hashtree-updater|download_and_install',
      { onEvent: channel, ...options }
    )
  }
}

/** Resolve the manifest, returning `null` if no asset matches the platform. */
export async function check(): Promise<Update | null> {
  const meta = await invoke<UpdateMetadata | null>(
    'plugin:hashtree-updater|check'
  )
  return meta ? new Update(meta) : null
}
