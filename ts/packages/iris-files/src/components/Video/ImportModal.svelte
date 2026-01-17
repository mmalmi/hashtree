<script lang="ts" module>
  /**
   * ImportModal - Import videos from yt-dlp backup directories
   * Shows installation instructions and allows folder upload
   */
  let show = $state(false);

  export function open() {
    show = true;
  }

  export function close() {
    show = false;
  }
</script>

<script lang="ts">
  import { SvelteSet } from 'svelte/reactivity';
  import { nostrStore, saveHashtree } from '../../nostr';
  import { videoChunker, cid, LinkType, nhashEncode, type WorkerBlossomUploadProgress } from 'hashtree';
  import type { CID } from 'hashtree';
  import { getTree } from '../../store';
  import { waitForWorkerAdapter } from '../../lib/workerInit';
  import { storeLinkKey } from '../../stores/trees';
  import { detectYtDlpDirectory, type YtDlpVideo } from '../../utils/ytdlp';
  import { toast } from '../../stores/toast';
  import BlossomProgress from './BlossomProgress.svelte';

  let userNpub = $derived($nostrStore.npub);

  // State
  let folderInput: HTMLInputElement | undefined = $state();
  let mode = $state<'instructions' | 'preview' | 'uploading' | 'pushing' | 'done'>('instructions');
  let batchVideos = $state<YtDlpVideo[]>([]);
  let selectedVideoIds = new SvelteSet<string>();
  let playlistName = $state('');
  let batchTotalSize = $state(0);
  let visibility = $state<'public' | 'link-visible' | 'private'>('public');
  let sourceUrl = $state('');
  let backupDir = $state('backup');
  let isValidUrl = $derived(() => {
    const url = sourceUrl.trim();
    if (!url) return true; // Empty is valid (optional field)
    if (/\s/.test(url)) return false; // No whitespace allowed
    try {
      const parsed = new URL(url);
      if (parsed.protocol !== 'http:' && parsed.protocol !== 'https:') return false;
      // Hostname must have a dot (real domain) and not be a protocol name
      if (!parsed.hostname || !parsed.hostname.includes('.')) return false;
      return true;
    } catch {
      return false;
    }
  });

  // Upload state
  let uploading = $state(false);
  let progress = $state(0);
  let progressMessage = $state('');
  let abortController = $state<AbortController | null>(null);

  // Blossom push state
  let pushProgress = $state({ current: 0, total: 0 });
  let pushStats = $state({ pushed: 0, skipped: 0, failed: 0 });
  let blossomProgress = $state<WorkerBlossomUploadProgress | null>(null);

  // Done state
  let resultUrl = $state('');
  let blossomPushFailed = $state(false);

  // Derived
  let selectedVideos = $derived(batchVideos.filter(v => selectedVideoIds.has(v.id)));

  // Reset state when modal closes
  $effect(() => {
    if (!show) {
      mode = 'instructions';
      batchVideos = [];
      selectedVideoIds.clear();
      playlistName = '';
      batchTotalSize = 0;
      visibility = 'public';
      sourceUrl = '';
      backupDir = 'backup';
      uploading = false;
      progress = 0;
      progressMessage = '';
      abortController = null;
      pushProgress = { current: 0, total: 0 };
      pushStats = { pushed: 0, skipped: 0, failed: 0 };
      blossomProgress = null;
      resultUrl = '';
      blossomPushFailed = false;
    }
  });

  function toggleVideo(id: string) {
    if (selectedVideoIds.has(id)) {
      selectedVideoIds.delete(id);
    } else {
      selectedVideoIds.add(id);
    }
  }

  function selectAll() {
    selectedVideoIds.clear();
    batchVideos.forEach(v => selectedVideoIds.add(v.id));
  }

  function deselectAll() {
    selectedVideoIds.clear();
  }

  // Video extensions for fallback detection
  const VIDEO_EXTENSIONS = new Set(['.mp4', '.mkv', '.webm', '.mov', '.avi', '.m4v', '.flv', '.wmv', '.3gp']);

  async function handleFolderSelect(e: Event) {
    const input = e.target as HTMLInputElement;
    const files = input.files;
    if (!files || files.length === 0) return;

    const fileArray = Array.from(files);
    const result = detectYtDlpDirectory(fileArray);

    let videos = result.videos;
    let totalSize = 0;

    // If no yt-dlp videos found, fall back to detecting any video files
    if (videos.length === 0) {
      const videoFiles = fileArray.filter(f => {
        const ext = f.name.slice(f.name.lastIndexOf('.')).toLowerCase();
        return VIDEO_EXTENSIONS.has(ext);
      });

      if (videoFiles.length === 0) {
        toast.error('No video files found in folder.');
        return;
      }

      // Create simple video entries from video files
      videos = videoFiles.map((file, i) => {
        const name = file.name;
        const dotIndex = name.lastIndexOf('.');
        const title = dotIndex !== -1 ? name.slice(0, dotIndex) : name;
        return {
          id: `video-${i}`,
          title,
          videoFile: file,
          infoJson: null,
          thumbnail: null,
        };
      });
    }

    for (const v of videos) {
      if (v.videoFile) totalSize += v.videoFile.size;
    }

    batchVideos = videos;
    playlistName = result.channelName || '';
    batchTotalSize = totalSize;

    // Select all by default
    selectAll();
    mode = 'preview';
  }

  async function handleBatchUpload() {
    const isSingleVideo = selectedVideos.length === 1;
    // For single video, channel name is optional (we'll use video title)
    if (selectedVideos.length === 0 || (!isSingleVideo && !playlistName.trim()) || !userNpub) return;

    uploading = true;
    mode = 'uploading';
    progress = 0;
    progressMessage = 'Preparing...';
    abortController = new AbortController();

    try {
      const tree = getTree();

      const rootEntries: Array<{ name: string; cid: CID; size: number; type: LinkType; meta?: Record<string, unknown> }> = [];
      let singleVideoDir: { cid: CID; title: string } | null = null;

      for (let i = 0; i < selectedVideos.length; i++) {
        if (abortController.signal.aborted) throw new Error('Cancelled');

        const video = selectedVideos[i];
        const videoProgress = (i / selectedVideos.length) * 100;
        progress = Math.round(videoProgress);
        progressMessage = isSingleVideo
          ? `Saving: ${video.title}`
          : `Saving ${i + 1}/${selectedVideos.length}: ${video.title}`;

        const videoEntries: Array<{ name: string; cid: CID; size: number; type: LinkType; meta?: Record<string, unknown> }> = [];
        const createdAt = Math.floor(Date.now() / 1000); // Unix timestamp in seconds
        let originalDate: number | undefined;
        let duration: number | undefined;
        let thumbnailCid: CID | undefined;
        let infoData: { upload_date?: string; description?: string; title?: string; duration?: number } | null = null;
        let infoJsonBytes: Uint8Array | null = null;

        // Parse info.json first to get originalDate and other metadata
        if (video.infoJson) {
          try {
            infoJsonBytes = new Uint8Array(await video.infoJson.arrayBuffer());
            const jsonText = new TextDecoder().decode(infoJsonBytes);
            infoData = JSON.parse(jsonText);
            // YouTube's upload_date is in format "YYYYMMDD"
            if (infoData?.upload_date && typeof infoData.upload_date === 'string' && infoData.upload_date.length === 8) {
              const year = parseInt(infoData.upload_date.slice(0, 4), 10);
              const month = parseInt(infoData.upload_date.slice(4, 6), 10) - 1; // 0-indexed
              const day = parseInt(infoData.upload_date.slice(6, 8), 10);
              const date = new Date(year, month, day);
              if (!isNaN(date.getTime())) {
                originalDate = Math.floor(date.getTime() / 1000);
              }
            }
            // Extract duration (in seconds)
            if (typeof infoData?.duration === 'number' && infoData.duration > 0) {
              duration = Math.round(infoData.duration);
            }
          } catch {
            // Ignore JSON parse errors for date extraction
          }
        }

        // Upload video file
        if (video.videoFile) {
          const streamWriter = tree.createStream({ chunker: videoChunker() });
          const chunkSize = 1024 * 1024;
          const file = video.videoFile;
          let chunkCount = 0;

          for (let offset = 0; offset < file.size; offset += chunkSize) {
            if (abortController.signal.aborted) throw new Error('Cancelled');

            const slice = file.slice(offset, Math.min(offset + chunkSize, file.size));
            const data = new Uint8Array(await slice.arrayBuffer());
            await streamWriter.append(data);
            chunkCount++;

            // Yield every 10 chunks to allow GC
            if (chunkCount % 10 === 0) {
              await new Promise(r => setTimeout(r, 0));
            }

            const fileProgress = offset / file.size;
            const overallProgress = ((i + fileProgress * 0.8) / selectedVideos.length) * 100;
            progress = Math.round(overallProgress);
          }

          const result = await streamWriter.finalize();
          streamWriter.clear(); // Release memory
          const ext = file.name.split('.').pop()?.toLowerCase() || 'mp4';
          // Store all metadata in video file's link entry (not separate metadata.json)
          const videoMeta: Record<string, unknown> = { createdAt };
          if (originalDate) videoMeta.originalDate = originalDate;
          if (duration) videoMeta.duration = duration;
          if (infoData?.title?.trim()) videoMeta.title = infoData.title.trim();
          if (infoData?.description?.trim()) videoMeta.description = infoData.description.trim();
          videoEntries.push({
            name: `video.${ext}`,
            cid: cid(result.hash, result.key),
            size: result.size,
            type: LinkType.File,
            meta: videoMeta,
          });
        }

        // Upload info.json (full yt-dlp metadata) - keep for reference
        if (infoJsonBytes) {
          const result = await tree.putFile(infoJsonBytes, {});
          videoEntries.push({ name: 'info.json', cid: result.cid, size: result.size, type: LinkType.File });
        }

        // Upload thumbnail
        if (video.thumbnail) {
          const data = new Uint8Array(await video.thumbnail.arrayBuffer());
          const result = await tree.putFile(data, {});
          const ext = video.thumbnail.name.split('.').pop()?.toLowerCase() || 'jpg';
          videoEntries.push({ name: `thumbnail.${ext}`, cid: result.cid, size: result.size, type: LinkType.File });
          thumbnailCid = result.cid;
        }

        // Create video directory
        const videoDirResult = await tree.putDirectory(videoEntries, {});

        if (isSingleVideo) {
          // For single video, use the video directory directly as root
          singleVideoDir = { cid: videoDirResult.cid, title: video.title };
        } else {
          // Store metadata in parent link entry for playlist videos
          const dirMeta: Record<string, unknown> = { createdAt };
          if (originalDate) dirMeta.originalDate = originalDate;
          if (duration) dirMeta.duration = duration;
          if (thumbnailCid) dirMeta.thumbnail = nhashEncode(thumbnailCid);
          if (infoData?.title?.trim()) dirMeta.title = infoData.title.trim();
          if (infoData?.description?.trim()) dirMeta.description = infoData.description.trim();
          rootEntries.push({
            name: video.id,
            cid: videoDirResult.cid,
            size: videoEntries.reduce((sum, e) => sum + (e.size || 0), 0),
            type: LinkType.Dir,
            meta: dirMeta,
          });
        }
      }

      progress = 95;
      progressMessage = 'Publishing...';

      let rootDirResult: { cid: CID };
      let treeName: string;

      if (isSingleVideo && singleVideoDir) {
        // Single video: use video directory directly, name by video title
        rootDirResult = { cid: singleVideoDir.cid };
        treeName = `videos/${singleVideoDir.title}`;
      } else {
        // Multiple videos: wrap in playlist directory
        progressMessage = 'Creating playlist...';
        rootDirResult = await tree.putDirectory(rootEntries, {});
        treeName = `videos/${playlistName.trim()}`;
      }

      progressMessage = 'Publishing...';
      const result = await saveHashtree(treeName, rootDirResult.cid, { visibility });
      progress = 100;

      if (result.linkKey && userNpub) {
        storeLinkKey(userNpub, treeName, result.linkKey);
      }

      // Push to blossom servers via worker (uses configured servers)
      blossomPushFailed = false;
      mode = 'pushing';
      progressMessage = 'Pushing to file servers...';
      pushProgress = { current: 0, total: 0 };
      pushStats = { pushed: 0, skipped: 0, failed: 0 };
      blossomProgress = null;

      const adapter = await waitForWorkerAdapter();
      if (!adapter) {
        throw new Error('Worker not initialized');
      }
      const sessionId = `import-${Date.now()}`;

      // Set up progress callback for per-server status
      adapter.onBlossomProgress((p) => {
        if (p.sessionId === sessionId) {
          blossomProgress = p;
          pushProgress = { current: p.processedChunks, total: p.totalChunks };
        }
      });

      await adapter.startBlossomSession(sessionId, 0); // Total will be updated by push

      try {
        const pushResult = await adapter.pushToBlossom(rootDirResult.cid.hash, rootDirResult.cid.key);

        pushStats = {
          pushed: pushResult.pushed,
          skipped: pushResult.skipped,
          failed: pushResult.failed,
        };

        // Check if any blocks failed
        if (pushResult.failed > 0) {
          blossomPushFailed = true;
        }
      } catch (pushError) {
        console.error('Blossom push failed:', pushError);
        const msg = pushError instanceof Error ? pushError.message : 'Unknown error';
        if (msg === 'Cancelled') {
          throw pushError; // Re-throw cancellation
        }
        blossomPushFailed = true;
      }

      await adapter.endBlossomSession();

      uploading = false;
      progressMessage = '';
      const encodedTreeName = encodeURIComponent(treeName);
      resultUrl = result.linkKey ? `#/${userNpub}/${encodedTreeName}?k=${result.linkKey}` : `#/${userNpub}/${encodedTreeName}`;

      if (blossomPushFailed) {
        mode = 'done';
      } else {
        window.location.hash = resultUrl;
        close();
      }
    } catch (e) {
      console.error('Batch upload failed:', e);
      const message = e instanceof Error ? e.message : 'Unknown error';
      if (message !== 'Cancelled') {
        toast.error('Failed to import: ' + message);
      }
      uploading = false;
      mode = 'preview';
      progressMessage = '';
      abortController = null;
    }
  }

  function handleCancel() {
    if (abortController) {
      abortController.abort();
      abortController = null;
    }
    uploading = false;
    mode = 'preview';
    progressMessage = '';
  }

  function formatSize(bytes: number): string {
    if (bytes < 1024) return `${bytes} B`;
    if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KB`;
    if (bytes < 1024 * 1024 * 1024) return `${(bytes / (1024 * 1024)).toFixed(1)} MB`;
    return `${(bytes / (1024 * 1024 * 1024)).toFixed(2)} GB`;
  }

  let ytdlpCommand = $derived(
    `yt-dlp ${sourceUrl.trim() || 'URL'} --write-info-json --write-thumbnail --format mp4 -P ${backupDir.trim() || 'backup'}`
  );

  let copied = $state(false);
  function copyCommand() {
    navigator.clipboard.writeText(ytdlpCommand);
    copied = true;
    setTimeout(() => copied = false, 2000);
  }
</script>

{#if show}
  <!-- svelte-ignore a11y_click_events_have_key_events -->
  <!-- svelte-ignore a11y_no_static_element_interactions -->
  <div class="fixed inset-0 z-50 flex items-center justify-center bg-black/70" onclick={() => !uploading && close()}>
    <div class="bg-surface-1 rounded-lg shadow-lg w-full max-w-2xl mx-4 max-h-[90vh] overflow-auto" onclick={(e) => e.stopPropagation()}>
      <!-- Header -->
      <div class="p-4 border-b border-surface-3 flex items-center justify-between">
        <h2 class="text-lg font-semibold flex items-center gap-2">
          <span class="i-lucide-folder-down text-accent"></span>
          Import Videos
        </h2>
        <button onclick={() => !uploading && close()} class="btn-ghost p-1" disabled={uploading} title="Close">
          <span class="i-lucide-x text-lg"></span>
        </button>
      </div>

      <!-- Content -->
      <div class="p-4">
        {#if mode === 'instructions'}
          <!-- Installation instructions -->
          <div class="space-y-4">
            <div class="bg-surface-2 rounded-lg p-4">
              <h3 class="font-medium text-text-1 mb-2 flex items-center gap-2">
                <span class="i-lucide-hard-drive-download"></span>
                Backup Your Videos
              </h3>
              <p class="text-text-2 text-sm mb-3">
                Use <a href="https://github.com/yt-dlp/yt-dlp" target="_blank" rel="noopener" class="text-accent hover:underline">yt-dlp</a> to backup videos from YouTube, Vimeo, TikTok, Twitter/X, Twitch, Instagram and <a href="https://github.com/yt-dlp/yt-dlp/blob/master/supportedsites.md" target="_blank" rel="noopener" class="text-accent hover:underline">thousands of other sites</a>.
              </p>

              <div class="space-y-3">
                <div>
                  <p class="text-xs text-text-3 mb-1">1. <a href="https://github.com/yt-dlp/yt-dlp?tab=readme-ov-file#installation" target="_blank" rel="noopener" class="text-accent hover:underline">Install yt-dlp</a> (or use <a href="https://ytdlp.online/" target="_blank" rel="noopener" class="text-accent hover:underline">web version</a>)</p>
                </div>

                <div>
                  <p class="text-xs text-text-3 mb-1">2. Paste URL</p>
                  <input
                    type="text"
                    bind:value={sourceUrl}
                    class="w-full bg-surface-0 border rounded-lg p-2 text-text-1 text-sm font-mono focus:outline-none {isValidUrl() ? 'border-surface-3 focus:border-accent' : 'border-red-500'}"
                    placeholder="https://www.youtube.com/watch?v=... or @channel or playlist"
                  />
                </div>

                <div>
                  <p class="text-xs text-text-3 mb-1">3. Backup folder name</p>
                  <input
                    type="text"
                    bind:value={backupDir}
                    class="w-full bg-surface-0 border border-surface-3 rounded-lg p-2 text-text-1 text-sm font-mono focus:outline-none focus:border-accent"
                    placeholder="backup"
                  />
                </div>

                <div>
                  <p class="text-xs text-text-3 mb-1">4. Download videos</p>
                  <div class="relative">
                    <code class="block bg-surface-0 rounded p-2 pr-10 text-sm text-text-1 font-mono break-all">
                      {ytdlpCommand}
                    </code>
                    <button
                      onclick={copyCommand}
                      class="absolute right-2 top-1/2 -translate-y-1/2 btn-ghost p-1"
                      title={copied ? 'Copied!' : 'Copy command'}
                    >
                      <span class={copied ? 'i-lucide-check text-sm text-green-500' : 'i-lucide-copy text-sm'}></span>
                    </button>
                  </div>
                </div>

                <div>
                  <p class="text-xs text-text-3 mb-1">5. Select the "{backupDir.trim() || 'backup'}" folder below</p>
                </div>
              </div>
            </div>

            <div class="text-center">
              <input
                bind:this={folderInput}
                type="file"
                webkitdirectory
                class="hidden"
                onchange={handleFolderSelect}
              />
              <button
                onclick={() => folderInput?.click()}
                class="btn-primary px-6 py-3 flex items-center gap-2 mx-auto"
              >
                <span class="i-lucide-folder-open"></span>
                Select Folder
              </button>
              <p class="text-text-3 text-xs mt-2">Choose the folder containing your yt-dlp backup</p>
            </div>
          </div>

        {:else if mode === 'preview'}
          <!-- Batch preview -->
          <div class="space-y-4">
            <div class="bg-surface-2 rounded-lg p-4">
              <div class="flex items-center gap-3 mb-3">
                <span class="i-lucide-folder-video text-2xl text-accent"></span>
                <div>
                  <p class="text-text-1 font-medium">yt-dlp Backup Detected</p>
                  <p class="text-text-3 text-sm">{batchVideos.length} video{batchVideos.length !== 1 ? 's' : ''}, {formatSize(batchTotalSize)} total</p>
                </div>
              </div>

              <!-- Playlist name input (only for multiple videos) -->
              {#if selectedVideos.length > 1}
                <div class="mb-3">
                  <label class="block text-sm text-text-2 mb-1">Playlist Name</label>
                  <input
                    type="text"
                    bind:value={playlistName}
                    class="w-full bg-surface-0 border border-surface-3 rounded-lg p-2 text-text-1 focus:border-accent focus:outline-none"
                    placeholder="Playlist name"
                  />
                </div>
              {/if}

              <!-- Video list -->
              <div class="max-h-60 overflow-auto border border-surface-3 rounded-lg">
                <div class="flex items-center justify-between p-2 bg-surface-3 border-b border-surface-3 sticky top-0">
                  <span class="text-sm text-text-2">{selectedVideoIds.size} of {batchVideos.length} selected</span>
                  <div class="flex gap-2">
                    <button onclick={selectAll} class="text-xs text-accent hover:underline">Select all</button>
                    <button onclick={deselectAll} class="text-xs text-text-3 hover:underline">Deselect all</button>
                  </div>
                </div>
                {#each batchVideos as video (video.id)}
                  <button
                    onclick={() => toggleVideo(video.id)}
                    class="w-full flex items-center gap-3 p-2 hover:bg-surface-2 transition-colors text-left border-b border-surface-3 last:border-b-0"
                  >
                    <div class="w-5 h-5 flex items-center justify-center shrink-0">
                      {#if selectedVideoIds.has(video.id)}
                        <span class="i-lucide-check-square text-accent"></span>
                      {:else}
                        <span class="i-lucide-square text-text-3"></span>
                      {/if}
                    </div>
                    <div class="flex-1 min-w-0">
                      <p class="text-text-1 text-sm truncate">{video.title}</p>
                      <p class="text-xs text-text-3">
                        {video.videoFile ? formatSize(video.videoFile.size) : 'No video'}
                      </p>
                    </div>
                  </button>
                {/each}
              </div>
            </div>

            <!-- Visibility -->
            <div>
              <label class="block text-sm text-text-2 mb-2">Visibility</label>
              <div class="flex gap-2">
                <button
                  type="button"
                  onclick={() => visibility = 'public'}
                  class="flex-1 flex items-center justify-center gap-2 py-2 px-3 btn-ghost {visibility === 'public' ? 'ring-2 ring-accent bg-surface-3' : ''}"
                >
                  <span class="i-lucide-globe"></span>
                  <span class="text-sm">Public</span>
                </button>
                <button
                  type="button"
                  onclick={() => visibility = 'link-visible'}
                  class="flex-1 flex items-center justify-center gap-2 py-2 px-3 btn-ghost {visibility === 'link-visible' ? 'ring-2 ring-accent bg-surface-3' : ''}"
                >
                  <span class="i-lucide-link"></span>
                  <span class="text-sm">Link-visible</span>
                </button>
                <button
                  type="button"
                  onclick={() => visibility = 'private'}
                  class="flex-1 flex items-center justify-center gap-2 py-2 px-3 btn-ghost {visibility === 'private' ? 'ring-2 ring-accent bg-surface-3' : ''}"
                >
                  <span class="i-lucide-lock"></span>
                  <span class="text-sm">Private</span>
                </button>
              </div>
            </div>
          </div>

        {:else if mode === 'uploading'}
          <!-- Upload progress -->
          <div class="space-y-4 py-8">
            <div class="text-center">
              <span class="i-lucide-loader-2 text-4xl text-accent animate-spin block mx-auto mb-4"></span>
              <p class="text-text-1 font-medium">{progressMessage}</p>
              <p class="text-text-3 text-sm mt-1">{progress}% complete</p>
            </div>
            <div class="h-2 bg-surface-2 rounded-full overflow-hidden">
              <div
                class="h-full bg-accent transition-all duration-300"
                style="width: {progress}%"
              ></div>
            </div>
          </div>

        {:else if mode === 'pushing'}
          <!-- Blossom push progress -->
          <div class="space-y-4 py-8">
            <div class="text-center">
              <span class="i-lucide-upload-cloud text-4xl text-accent animate-pulse block mx-auto mb-4"></span>
              <p class="text-text-1 font-medium">Pushing to file servers...</p>
              <p class="text-text-3 text-sm mt-1">
                {pushProgress.current}/{pushProgress.total} chunks
              </p>
            </div>
            <div class="h-2 bg-surface-2 rounded-full overflow-hidden">
              <div
                class="h-full bg-accent transition-all duration-300"
                style="width: {pushProgress.total > 0 ? (pushProgress.current / pushProgress.total * 100) : 0}%"
              ></div>
            </div>

            <!-- Per-server status -->
            <BlossomProgress progress={blossomProgress} />
            {#if !blossomProgress && (pushStats.pushed > 0 || pushStats.skipped > 0)}
              <div class="text-center text-xs text-text-3">
                {pushStats.pushed} uploaded, {pushStats.skipped} already exist
              </div>
            {/if}
          </div>

        {:else if mode === 'done'}
          <!-- Done with warning -->
          <div class="space-y-4 py-8">
            <div class="text-center">
              <span class="i-lucide-check-circle text-4xl text-green-500 block mx-auto mb-4"></span>
              <p class="text-text-1 font-medium">Videos saved locally!</p>
            </div>
            {#if blossomPushFailed}
              <div class="bg-yellow-500/10 border border-yellow-500/30 rounded-lg p-4 text-center">
                <p class="text-yellow-400 text-sm">
                  File server push had issues. You can retry later using the <span class="i-lucide-cloud inline-block align-middle"></span> button in folder actions.
                </p>
              </div>
            {/if}
          </div>
        {/if}
      </div>

      <!-- Footer -->
      <div class="p-4 border-t border-surface-3 flex justify-end gap-2">
        {#if mode === 'instructions'}
          <button onclick={close} class="btn-ghost px-4 py-2">
            Close
          </button>
        {:else if mode === 'preview'}
          <button onclick={() => mode = 'instructions'} class="btn-ghost px-4 py-2">
            Back
          </button>
          <button
            onclick={handleBatchUpload}
            class="btn-primary px-4 py-2"
            disabled={selectedVideos.length === 0 || (selectedVideos.length > 1 && !playlistName.trim())}
          >
            Import {selectedVideos.length} Video{selectedVideos.length !== 1 ? 's' : ''}
          </button>
        {:else if mode === 'uploading'}
          <button onclick={handleCancel} class="btn-ghost px-4 py-2">
            Cancel
          </button>
        {:else if mode === 'pushing'}
          <button onclick={handleCancel} class="btn-ghost px-4 py-2">
            Cancel
          </button>
        {:else if mode === 'done'}
          <button onclick={() => { window.location.hash = resultUrl; close(); }} class="btn-primary px-4 py-2">
            View Videos
          </button>
        {/if}
      </div>
    </div>
  </div>
{/if}
