<script lang="ts">
  /**
   * VideoCreate - Full-page video upload/stream experience
   * Similar layout to VideoView with large video area and metadata below
   * Supports file upload with transcoding and live streaming with comments
   */
  import { nostrStore, saveHashtree } from '../../nostr';
  import { videoChunker, LinkType, type CID } from 'hashtree';
  import { getTree } from '../../store';
  import { addRecent } from '../../stores/recents';
  import { storeLinkKey } from '../../stores/trees';
  import { needsTranscoding, transcodeToMP4Streaming, isTranscodingSupported, canTranscode, type TranscodeProgress } from '../../utils/videoTranscode';
  import {
    videoStreamStore,
    startPreview,
    stopPreview,
    startRecording,
    stopRecording,
    cancelRecording,
    formatTime,
    formatBytes,
  } from './videoStreamState';
  import VideoComments from './VideoComments.svelte';
  import { open as openShareModal } from '../Modals/ShareModal.svelte';

  type TabType = 'upload' | 'stream';

  let activeTab = $state<TabType>('upload');
  let streamState = $derived($videoStreamStore);

  let userNpub = $derived($nostrStore.npub);
  let isLoggedIn = $derived($nostrStore.isLoggedIn);

  // ========== Upload Tab State ==========
  let fileInput: HTMLInputElement | undefined = $state();
  let selectedFile = $state<File | null>(null);
  let title = $state('');
  let description = $state('');
  let uploading = $state(false);
  let progress = $state(0);
  let progressMessage = $state('');
  let thumbnailUrl = $state<string | null>(null);
  let thumbnailBlob = $state<Blob | null>(null);
  let willTranscode = $state(false);
  let transcodeSupported = $state(true);
  let transcodeError = $state<string | null>(null);
  let visibility = $state<'public' | 'link-visible' | 'private'>('public');
  let abortController = $state<AbortController | null>(null);

  // ========== Stream Tab State ==========
  let videoRef: HTMLVideoElement | undefined = $state();
  let streamTitle = $state('');
  let streamDescription = $state('');
  let streamVisibility = $state<'public' | 'link-visible' | 'private'>('public');
  let streamThumbnailBlob = $state<Blob | null>(null);
  let saving = $state(false);

  // For live comments during streaming
  let streamTreeName = $state<string | null>(null);

  // Determine if we're busy (can't switch tabs or navigate away)
  let isBusy = $derived(uploading || streamState.isRecording || saving);

  function handleTabChange(tab: TabType) {
    if (isBusy) return;
    activeTab = tab;
  }

  function handleBack() {
    if (isBusy) {
      if (!confirm('You have an upload or recording in progress. Are you sure you want to leave?')) {
        return;
      }
      handleCleanup();
    }
    window.history.back();
  }

  // ========== Upload Tab Functions ==========
  function handleFileSelect(e: Event) {
    const input = e.target as HTMLInputElement;
    const file = input.files?.[0];
    if (!file) return;

    selectedFile = file;
    willTranscode = needsTranscoding(file);
    transcodeSupported = isTranscodingSupported();

    if (willTranscode) {
      const check = canTranscode(file);
      transcodeError = check.ok ? null : (check.reason || 'Cannot transcode');
    } else {
      transcodeError = null;
    }

    if (!title) {
      title = file.name.replace(/\.[^/.]+$/, '');
    }

    generateThumbnail(file);
  }

  async function generateThumbnail(file: File) {
    try {
      const video = document.createElement('video');
      video.preload = 'metadata';
      video.muted = true;
      video.playsInline = true;

      const url = URL.createObjectURL(file);
      video.src = url;

      await new Promise<void>((resolve, reject) => {
        video.onloadedmetadata = () => {
          video.currentTime = Math.min(1, video.duration / 4);
        };
        video.onseeked = () => resolve();
        video.onerror = reject;
      });

      const canvas = document.createElement('canvas');
      const maxWidth = 640;
      const maxHeight = 360;

      const videoAspect = video.videoWidth / video.videoHeight;
      let width = video.videoWidth;
      let height = video.videoHeight;

      if (width > maxWidth) {
        width = maxWidth;
        height = width / videoAspect;
      }
      if (height > maxHeight) {
        height = maxHeight;
        width = height * videoAspect;
      }

      canvas.width = Math.round(width);
      canvas.height = Math.round(height);

      const ctx = canvas.getContext('2d');
      if (ctx) {
        ctx.drawImage(video, 0, 0, canvas.width, canvas.height);

        const blob = await new Promise<Blob | null>(resolve => {
          canvas.toBlob(resolve, 'image/jpeg', 0.8);
        });

        if (blob) {
          thumbnailBlob = blob;
          if (thumbnailUrl) URL.revokeObjectURL(thumbnailUrl);
          thumbnailUrl = URL.createObjectURL(blob);
        }
      }

      URL.revokeObjectURL(url);
    } catch (e) {
      console.error('Failed to generate thumbnail:', e);
    }
  }

  async function handleUpload() {
    if (!selectedFile || !title.trim() || !userNpub) return;

    uploading = true;
    progress = 0;
    progressMessage = 'Preparing...';
    abortController = new AbortController();

    try {
      const tree = getTree();
      const treeName = `videos/${title.trim()}`;

      let videoFileName: string;
      // eslint-disable-next-line @typescript-eslint/no-explicit-any
      let videoResult: { cid: any; size: number };


      if (willTranscode) {
        progressMessage = 'Loading encoder...';
        progress = 5;

        const streamWriter = tree.createStream({ chunker: videoChunker() });

        const result = await transcodeToMP4Streaming(
          selectedFile,
          async (chunk: Uint8Array) => {
            await streamWriter.append(chunk);
          },
          (p: TranscodeProgress) => {
            progressMessage = p.message;
            if (p.percent !== undefined) {
              progress = 5 + Math.round(p.percent * 0.65);
            }
          },
          abortController.signal
        );

        const finalResult = await streamWriter.finalize();
        videoResult = {
          cid: { hash: finalResult.hash, key: finalResult.key },
          size: finalResult.size
        };
        videoFileName = `video.${result.extension}`;
        progress = 75;
      } else {
        progressMessage = 'Reading file...';
        progress = 10;

        const streamWriter = tree.createStream({ chunker: videoChunker() });
        const chunkSize = 1024 * 1024;

        for (let offset = 0; offset < selectedFile.size; offset += chunkSize) {
          const chunk = selectedFile.slice(offset, Math.min(offset + chunkSize, selectedFile.size));
          const data = new Uint8Array(await chunk.arrayBuffer());
          await streamWriter.append(data);

          const pct = Math.round((offset / selectedFile.size) * 100);
          progressMessage = `Uploading: ${Math.round(offset / 1024 / 1024)}MB / ${Math.round(selectedFile.size / 1024 / 1024)}MB`;
          progress = 10 + Math.round(pct * 0.55);
        }

        const finalResult = await streamWriter.finalize();
        videoResult = {
          cid: { hash: finalResult.hash, key: finalResult.key },
          size: finalResult.size
        };

        const ext = selectedFile.name.split('.').pop()?.toLowerCase() || 'webm';
        videoFileName = `video.${ext}`;
        progress = 70;
      }

      progressMessage = 'Saving metadata...';
      progress = 75;

      // Store metadata in video file's link entry (not separate metadata.json)
      const createdAt = Math.floor(Date.now() / 1000);
      const videoMeta: Record<string, unknown> = {
        createdAt,
        title: title.trim(),
      };
      if (description.trim()) videoMeta.description = description.trim();

      const entries: Array<{ name: string; cid: CID; size: number; type: LinkType; meta?: Record<string, unknown> }> = [
        { name: videoFileName, cid: videoResult.cid, size: videoResult.size, type: LinkType.File, meta: videoMeta },
      ];
      progress = 80;

      if (thumbnailBlob) {
        const thumbData = new Uint8Array(await thumbnailBlob.arrayBuffer());
        const thumbResult = await tree.putFile(thumbData, {});
        entries.push({ name: 'thumbnail.jpg', cid: thumbResult.cid, size: thumbResult.size, type: LinkType.File });
      }
      progress = 85;

      progressMessage = 'Creating video...';
      const dirResult = await tree.putDirectory(entries, {});
      progress = 90;

      progressMessage = 'Publishing...';
      const result = await saveHashtree(treeName, dirResult.cid, { visibility });
      progress = 100;

      if (result.linkKey && userNpub) {
        storeLinkKey(userNpub, treeName, result.linkKey);
      }

      addRecent({
        type: 'tree',
        path: `/${userNpub}/${treeName}`,
        label: title.trim(),
        npub: userNpub,
        treeName,
        visibility,
        linkKey: result.linkKey,
      });

      uploading = false;
      progressMessage = '';
      const encodedTreeName = encodeURIComponent(treeName);
      const videoUrl = result.linkKey ? `#/${userNpub}/${encodedTreeName}?k=${result.linkKey}` : `#/${userNpub}/${encodedTreeName}`;
      window.location.hash = videoUrl;
    } catch (e) {
      console.error('Upload failed:', e);
      const message = e instanceof Error ? e.message : 'Unknown error';
      if (message !== 'Cancelled') {
        alert('Failed to upload video: ' + message);
      }
      uploading = false;
      progressMessage = '';
      abortController = null;
    }
  }

  function handleCancelUpload() {
    if (abortController) {
      abortController.abort();
      abortController = null;
    }
    uploading = false;
    progressMessage = '';
  }

  // ========== Stream Tab Functions ==========
  async function handleStartCamera() {
    try {
      await startPreview(videoRef ?? null);
    } catch (e) {
      console.error('Failed to start camera:', e);
      alert('Failed to access camera. Please check permissions.');
    }
  }

  function handleStopCamera() {
    stopPreview(videoRef ?? null);
  }

  async function handleStartRecording() {
    if (!streamTitle.trim()) {
      alert('Please enter a title first');
      return;
    }

    // Create tree name for comments
    streamTreeName = `videos/${streamTitle.trim()}`;

    const isPublic = streamVisibility === 'public';
    await startRecording(videoRef ?? null, isPublic, streamTitle.trim(), streamVisibility);

    // Generate thumbnail from first frame
    if (videoRef) {
      setTimeout(() => {
        generateStreamThumbnail();
      }, 500);
    }
  }

  function generateStreamThumbnail() {
    if (!videoRef) return;

    try {
      const canvas = document.createElement('canvas');
      const maxWidth = 640;
      const maxHeight = 360;

      const videoAspect = videoRef.videoWidth / videoRef.videoHeight;
      let width = videoRef.videoWidth;
      let height = videoRef.videoHeight;

      if (width > maxWidth) {
        width = maxWidth;
        height = width / videoAspect;
      }
      if (height > maxHeight) {
        height = maxHeight;
        width = height * videoAspect;
      }

      canvas.width = Math.round(width);
      canvas.height = Math.round(height);

      const ctx = canvas.getContext('2d');
      if (ctx) {
        ctx.drawImage(videoRef, 0, 0, canvas.width, canvas.height);

        canvas.toBlob((blob) => {
          if (blob) {
            streamThumbnailBlob = blob;
          }
        }, 'image/jpeg', 0.8);
      }
    } catch (e) {
      console.error('Failed to generate stream thumbnail:', e);
    }
  }

  async function handleStopRecording() {
    saving = true;
    try {
      const result = await stopRecording(
        streamTitle.trim(),
        streamDescription.trim(),
        streamVisibility,
        streamThumbnailBlob
      );

      if (result.success && result.videoUrl) {
        window.location.hash = result.videoUrl;
      } else {
        alert('Failed to save recording');
      }
    } catch (e) {
      console.error('Failed to stop recording:', e);
      alert('Failed to save recording');
    } finally {
      saving = false;
      streamTreeName = null;
    }
  }

  function handleCancelRecording() {
    cancelRecording();
    streamTreeName = null;
    if (videoRef) {
      videoRef.srcObject = null;
    }
  }

  // ========== Cleanup ==========
  function handleCleanup() {
    if (uploading) {
      handleCancelUpload();
    }
    if (streamState.isRecording || streamState.isPreviewing) {
      cancelRecording();
      if (videoRef) {
        videoRef.srcObject = null;
      }
    }

    // Reset upload tab state
    selectedFile = null;
    title = '';
    description = '';
    progress = 0;
    progressMessage = '';
    willTranscode = false;
    visibility = 'public';
    abortController = null;
    if (thumbnailUrl) {
      URL.revokeObjectURL(thumbnailUrl);
      thumbnailUrl = null;
    }
    thumbnailBlob = null;

    // Reset stream tab state
    streamTitle = '';
    streamDescription = '';
    streamVisibility = 'public';
    streamThumbnailBlob = null;
    streamTreeName = null;
  }

  function formatSize(bytes: number): string {
    if (bytes < 1024) return `${bytes} B`;
    if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KB`;
    if (bytes < 1024 * 1024 * 1024) return `${(bytes / (1024 * 1024)).toFixed(1)} MB`;
    return `${(bytes / (1024 * 1024 * 1024)).toFixed(2)} GB`;
  }
</script>

<div class="flex-1">
  <!-- Video Preview Area - same size as VideoView -->
  <div class="w-full max-w-full bg-black overflow-hidden mx-auto relative" style="height: min(calc(100vh - 48px - 80px), 90vh); aspect-ratio: 16/9;">
    {#if activeTab === 'upload'}
      <!-- Upload preview -->
      {#if thumbnailUrl}
        <img src={thumbnailUrl} alt="Video preview" class="w-full h-full object-contain" />
      {:else}
        <div
          class="w-full h-full flex flex-col items-center justify-center cursor-pointer hover:bg-white/5 transition-colors"
          onclick={() => !uploading && fileInput?.click()}
          role="button"
          tabindex="0"
          onkeydown={(e) => e.key === 'Enter' && !uploading && fileInput?.click()}
        >
          <span class="i-lucide-upload text-6xl text-accent mb-4"></span>
          <p class="text-white text-lg">Click to select a video file</p>
          <p class="text-white/60 text-sm mt-2">MP4, WebM, MOV, AVI, MKV supported</p>
        </div>
      {/if}
      <input
        bind:this={fileInput}
        type="file"
        accept="video/*"
        class="hidden"
        onchange={handleFileSelect}
      />

      <!-- Upload progress overlay -->
      {#if uploading}
        <div class="absolute inset-0 bg-black/80 flex flex-col items-center justify-center">
          <div class="w-64 space-y-4">
            <div class="text-white text-center">{progressMessage || 'Processing...'}</div>
            <div class="h-2 bg-white/20 rounded-full overflow-hidden">
              <div class="h-full bg-accent transition-all duration-300" style="width: {progress}%"></div>
            </div>
            <div class="text-white/60 text-center text-sm">{progress}%</div>
          </div>
        </div>
      {/if}
    {:else}
      <!-- Stream preview -->
      <video
        bind:this={videoRef}
        autoplay
        muted
        playsinline
        class="w-full h-full object-contain {streamState.isPreviewing || streamState.isRecording ? '' : 'hidden'}"
      ></video>
      {#if !streamState.isPreviewing && !streamState.isRecording}
        <div class="w-full h-full flex flex-col items-center justify-center">
          <span class="i-lucide-video text-6xl text-danger mb-4"></span>
          <p class="text-white text-lg">Stream from your camera</p>
          <p class="text-white/60 text-sm mt-2">Record and share live</p>
        </div>
      {/if}

      <!-- Recording indicator -->
      {#if streamState.isRecording}
        <div class="absolute top-4 left-4 z-10 flex items-center gap-3 bg-black/60 px-3 py-2 rounded-lg">
          <div class="flex items-center gap-2 text-danger">
            <span class="w-3 h-3 bg-danger rounded-full animate-pulse"></span>
            <span class="font-medium">REC</span>
          </div>
          <span class="text-white">{formatTime(streamState.recordingTime)}</span>
          <span class="text-white/60">{formatBytes(streamState.streamStats.totalSize)}</span>
        </div>
      {/if}
    {/if}
  </div>

  <!-- Content below video -->
  <div class="max-w-5xl mx-auto px-4 py-4">
    <!-- Header with back button, tabs, and share -->
    <div class="flex items-center justify-between mb-6">
      <button onclick={handleBack} class="btn-ghost flex items-center gap-2">
        <span class="i-lucide-arrow-left"></span>
        Back
      </button>

      <div class="flex items-center gap-2">
        <button
          onclick={() => handleTabChange('upload')}
          class="px-4 py-2 rounded-lg font-medium transition-colors flex items-center gap-2 {activeTab === 'upload' ? 'btn-primary' : 'btn-ghost'}"
          disabled={isBusy}
        >
          <span class="i-lucide-upload"></span>
          Upload
        </button>
        <button
          onclick={() => handleTabChange('stream')}
          class="px-4 py-2 rounded-lg font-medium transition-colors flex items-center gap-2 {activeTab === 'stream' ? 'bg-danger text-white' : 'btn-ghost'}"
          disabled={isBusy}
        >
          <span class="i-lucide-video"></span>
          Stream
        </button>
        {#if streamState.isRecording && streamTreeName && userNpub}
          <button
            onclick={() => {
              const encodedTreeName = encodeURIComponent(streamTreeName!);
              const streamUrl = `${window.location.origin}${window.location.pathname}#/${userNpub}/${encodedTreeName}`;
              openShareModal(streamUrl);
            }}
            class="btn-ghost flex items-center gap-2"
            title="Share stream"
          >
            <span class="i-lucide-share"></span>
            Share
          </button>
        {/if}
      </div>
    </div>

    <!-- Upload Tab Content -->
    {#if activeTab === 'upload'}
      <div class="space-y-4">
        <!-- File info -->
        {#if selectedFile}
          <div class="text-sm text-text-3 flex flex-col gap-1">
            <span>{selectedFile.name} ({formatSize(selectedFile.size)})</span>
            {#if willTranscode}
              {#if transcodeSupported && !transcodeError}
                <span class="text-xs bg-accent/20 text-accent px-2 py-0.5 rounded w-fit">Will convert to WebM</span>
              {:else}
                <span class="text-xs bg-red-500/20 text-red-400 px-2 py-0.5 rounded w-fit">
                  {transcodeError || 'Cannot convert: SharedArrayBuffer not available. Use Chrome/Edge or upload MP4/WebM.'}
                </span>
              {/if}
            {/if}
          </div>
        {/if}

        <!-- Title -->
        <div>
          <label class="block text-sm text-text-2 mb-1">Title</label>
          <input
            type="text"
            bind:value={title}
            class="w-full bg-surface-0 border border-surface-3 rounded-lg p-3 text-text-1 focus:border-accent focus:outline-none"
            placeholder="Video title"
            disabled={uploading}
          />
        </div>

        <!-- Description -->
        <div>
          <label class="block text-sm text-text-2 mb-1">Description (optional)</label>
          <textarea
            bind:value={description}
            class="w-full bg-surface-0 border border-surface-3 rounded-lg p-3 text-text-1 resize-none focus:border-accent focus:outline-none"
            placeholder="Video description..."
            rows="3"
            disabled={uploading}
          ></textarea>
        </div>

        <!-- Visibility -->
        <div>
          <label class="block text-sm text-text-2 mb-2">Visibility</label>
          <div class="flex gap-2">
            <button
              type="button"
              onclick={() => visibility = 'public'}
              class="flex-1 flex items-center justify-center gap-2 py-2 px-3 btn-ghost {visibility === 'public' ? 'ring-2 ring-accent bg-surface-3' : ''}"
              disabled={uploading}
            >
              <span class="i-lucide-globe"></span>
              <span class="text-sm">Public</span>
            </button>
            <button
              type="button"
              onclick={() => visibility = 'link-visible'}
              class="flex-1 flex items-center justify-center gap-2 py-2 px-3 btn-ghost {visibility === 'link-visible' ? 'ring-2 ring-accent bg-surface-3' : ''}"
              disabled={uploading}
            >
              <span class="i-lucide-link"></span>
              <span class="text-sm">Link-visible</span>
            </button>
            <button
              type="button"
              onclick={() => visibility = 'private'}
              class="flex-1 flex items-center justify-center gap-2 py-2 px-3 btn-ghost {visibility === 'private' ? 'ring-2 ring-accent bg-surface-3' : ''}"
              disabled={uploading}
            >
              <span class="i-lucide-lock"></span>
              <span class="text-sm">Private</span>
            </button>
          </div>
          <p class="text-xs text-text-3 mt-2">
            {#if visibility === 'public'}
              Anyone can find and watch this video
            {:else if visibility === 'link-visible'}
              Only people with the link can watch
            {:else}
              Encrypted, only you can watch
            {/if}
          </p>
        </div>

        <!-- Actions -->
        <div class="flex justify-end gap-2 pt-4">
          {#if uploading}
            <button onclick={handleCancelUpload} class="btn-ghost px-4 py-2">
              Cancel
            </button>
          {:else}
            <button
              onclick={handleUpload}
              class="btn-primary px-6 py-2"
              disabled={!selectedFile || !title.trim() || !isLoggedIn || (willTranscode && (!transcodeSupported || !!transcodeError))}
            >
              <span class="i-lucide-upload mr-2"></span>
              Upload Video
            </button>
          {/if}
        </div>
      </div>
    {/if}

    <!-- Stream Tab Content -->
    {#if activeTab === 'stream'}
      <div class="space-y-4">
        <!-- Title (required before recording) -->
        <div>
          <label class="block text-sm text-text-2 mb-1">Title</label>
          <input
            type="text"
            bind:value={streamTitle}
            class="w-full bg-surface-0 border border-surface-3 rounded-lg p-3 text-text-1 focus:border-accent focus:outline-none"
            placeholder="Stream title"
            disabled={streamState.isRecording || saving}
          />
        </div>

        <!-- Description -->
        <div>
          <label class="block text-sm text-text-2 mb-1">Description (optional)</label>
          <textarea
            bind:value={streamDescription}
            class="w-full bg-surface-0 border border-surface-3 rounded-lg p-3 text-text-1 resize-none focus:border-accent focus:outline-none"
            placeholder="Stream description..."
            rows="3"
            disabled={streamState.isRecording || saving}
          ></textarea>
        </div>

        <!-- Visibility -->
        <div>
          <label class="block text-sm text-text-2 mb-2">Visibility</label>
          <div class="flex gap-2">
            <button
              type="button"
              onclick={() => streamVisibility = 'public'}
              class="flex-1 flex items-center justify-center gap-2 py-2 px-3 btn-ghost {streamVisibility === 'public' ? 'ring-2 ring-accent bg-surface-3' : ''}"
              disabled={streamState.isRecording || saving}
            >
              <span class="i-lucide-globe"></span>
              <span class="text-sm">Public</span>
            </button>
            <button
              type="button"
              onclick={() => streamVisibility = 'link-visible'}
              class="flex-1 flex items-center justify-center gap-2 py-2 px-3 btn-ghost {streamVisibility === 'link-visible' ? 'ring-2 ring-accent bg-surface-3' : ''}"
              disabled={streamState.isRecording || saving}
            >
              <span class="i-lucide-link"></span>
              <span class="text-sm">Link-visible</span>
            </button>
            <button
              type="button"
              onclick={() => streamVisibility = 'private'}
              class="flex-1 flex items-center justify-center gap-2 py-2 px-3 btn-ghost {streamVisibility === 'private' ? 'ring-2 ring-accent bg-surface-3' : ''}"
              disabled={streamState.isRecording || saving}
            >
              <span class="i-lucide-lock"></span>
              <span class="text-sm">Private</span>
            </button>
          </div>
          <p class="text-xs text-text-3 mt-2">
            {#if streamVisibility === 'public'}
              Anyone can find and watch this stream
            {:else if streamVisibility === 'link-visible'}
              Only people with the link can watch
            {:else}
              Encrypted, only you can watch
            {/if}
          </p>
        </div>

        <!-- Actions -->
        <div class="flex justify-end gap-2 pt-4">
          {#if !streamState.isPreviewing && !streamState.isRecording}
            <button onclick={handleStartCamera} class="btn-primary px-6 py-2" disabled={!isLoggedIn}>
              <span class="i-lucide-video mr-2"></span>
              Start Camera
            </button>
          {:else if streamState.isPreviewing && !streamState.isRecording}
            <button onclick={handleStopCamera} class="btn-ghost px-4 py-2">
              Cancel
            </button>
            <button
              onclick={handleStartRecording}
              class="btn-danger px-6 py-2"
              disabled={!streamTitle.trim()}
            >
              <span class="i-lucide-circle mr-2"></span>
              Start Recording
            </button>
          {:else if streamState.isRecording}
            <button onclick={handleCancelRecording} class="btn-ghost px-4 py-2" disabled={saving}>
              Cancel
            </button>
            <button onclick={handleStopRecording} class="btn-success px-6 py-2" disabled={saving}>
              {#if saving}
                <span class="i-lucide-loader-2 animate-spin mr-2"></span>
                Saving...
              {:else}
                <span class="i-lucide-square mr-2"></span>
                Stop Recording
              {/if}
            </button>
          {/if}
        </div>

        <!-- Live Comments during streaming -->
        {#if streamState.isRecording && streamTreeName && userNpub}
          <div class="mt-8 pt-8 border-t border-surface-3">
            <h2 class="text-lg font-semibold text-text-1 mb-4">Live Comments</h2>
            {#key `${userNpub}/${streamTreeName}`}
              <VideoComments npub={userNpub} treeName={streamTreeName} />
            {/key}
          </div>
        {/if}
      </div>
    {/if}
  </div>
</div>
