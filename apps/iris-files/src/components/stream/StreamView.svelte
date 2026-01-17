<script lang="ts">
  /**
   * StreamView - inline stream view renders in preview area
   * Port of React StreamView component
   */
  import { onDestroy } from 'svelte';
  import { formatBytes } from '../../store';
  import { nostrStore } from '../../nostr';
  import { routeStore, directoryEntriesStore } from '../../stores';
  import { open as openShareModal } from '../Modals/ShareModal.svelte';
  import {
    streamStore,
    startPreview,
    stopPreview,
    startRecording,
    stopRecording,
    formatTime,
    setStreamFilename,
    setPersistStream,
  } from './streamState';
  import { BackButton } from '../ui';
  import VisibilityIcon from '../VisibilityIcon.svelte';

  let videoRef: HTMLVideoElement | undefined = $state();

  let route = $derived($routeStore);
  let selectedTree = $derived($nostrStore.selectedTree);
  let stream = $derived($streamStore);
  let dirEntries = $derived($directoryEntriesStore);
  let entries = $derived(dirEntries.entries);

  // Build base URL for the current directory
  let basePath = $derived(
    route.npub && route.treeName
      ? `/${route.npub}/${route.treeName}${route.path.length ? '/' + route.path.join('/') : ''}`
      : '/'
  );

  // Get link key (prefer route, fallback to selectedTree)
  let linkKey = $derived(
    route.params.get('k') ?? (selectedTree?.visibility === 'link-visible' && selectedTree?.linkKey ? selectedTree.linkKey : null)
  );

  // Build close URL (remove stream param but keep linkKey)
  let closeUrl = $derived(linkKey ? `#${basePath}?k=${linkKey}` : `#${basePath}`);

  // Check if filename exists
  let filenameExists = $derived(entries.some(e => e.name === `${stream.streamFilename}.webm`));

  // Cleanup on destroy
  onDestroy(() => {
    if (videoRef) {
      stopPreview(videoRef);
    }
    stopRecording();
  });

  function handleClose() {
    if (!stream.isRecording) {
      if (videoRef) stopPreview(videoRef);
      window.location.hash = closeUrl;
    }
  }

  function handleStartRecording() {
    if (videoRef) {
      startRecording(videoRef);
      // Navigate to file URL so it's shareable immediately
      const fullFilename = `${stream.streamFilename}.webm`;
      const filePath = `${basePath}/${encodeURIComponent(fullFilename)}`;
      const params = [
        linkKey ? `k=${linkKey}` : '',
        'stream=1',
      ].filter(Boolean).join('&');
      window.location.hash = `#${filePath}?${params}`;
    }
  }

  function handleShare() {
    // Build viewer URL with filename and ?live=1 for live playback behavior
    const fullFilename = `${stream.streamFilename}.webm`;
    const filePath = `${basePath}/${encodeURIComponent(fullFilename)}`;
    const base = window.location.origin + window.location.pathname + '#';

    // Build query params
    const params: string[] = [];
    if (linkKey) params.push(`k=${linkKey}`);
    if (stream.isRecording) params.push('live=1');

    const viewerUrl = base + filePath + (params.length ? '?' + params.join('&') : '');
    openShareModal(viewerUrl);
  }

  function handleFilenameChange(e: Event) {
    const input = e.target as HTMLInputElement;
    setStreamFilename(input.value.replace(/[^a-zA-Z0-9_-]/g, ''));
  }

  function handleCancelPreview() {
    if (videoRef) stopPreview(videoRef);
    handleClose();
  }
</script>

<div class="flex-1 flex flex-col min-h-0 bg-surface-0">
  <!-- Header -->
  <div class="shrink-0 px-3 py-2 border-b border-surface-3 flex flex-wrap items-center justify-between gap-2 bg-surface-1">
    <div class="flex items-center gap-2 min-w-0">
      {#if !stream.isRecording}
        <BackButton href={closeUrl} />
      {/if}
      {#if selectedTree}
        <VisibilityIcon visibility={selectedTree.visibility} class="text-text-3 shrink-0" />
      {/if}
      <span class="i-lucide-video text-text-2 shrink-0"></span>
      <span class="font-medium text-text-1 truncate">Livestream</span>
      {#if stream.isRecording}
        <span class="inline-flex items-center gap-1.5 text-danger text-sm shrink-0">
          <span class="w-2 h-2 bg-danger rounded-full animate-pulse"></span>
          REC {formatTime(stream.recordingTime)}
        </span>
      {/if}
    </div>
    <div class="flex items-center gap-1 shrink-0">
      <button onclick={handleShare} class="btn-ghost" title="Share">
        <span class="i-lucide-share text-base"></span>
      </button>
    </div>
  </div>

  <!-- Content -->
  <div class="flex-1 flex flex-col p-4 gap-4 overflow-auto">
    <video
      bind:this={videoRef}
      autoplay
      muted
      playsinline
      class="w-full aspect-video bg-surface-2 rounded"
    ></video>

    {#if stream.isRecording}
      <div class="flex items-center gap-4 text-sm text-muted">
        <span>{formatBytes(stream.streamStats.totalSize)}</span>
      </div>
    {/if}

    <!-- Stream Controls -->
    {#if !stream.isPreviewing && !stream.isRecording}
      <button
        onclick={() => videoRef && startPreview(videoRef)}
        class="p-3 btn-primary"
      >
        Start Camera
      </button>
    {:else if stream.isRecording}
      <button onclick={stopRecording} class="p-3 btn-success">
        <span class="i-lucide-square mr-2"></span>
        Stop Recording
      </button>
    {:else}
      <!-- Preview mode - show filename input and controls -->
      <div class="flex flex-col gap-3">
        <div class="flex gap-2 items-center">
          <div class="flex items-center flex-1">
            <input
              type="text"
              value={stream.streamFilename}
              oninput={handleFilenameChange}
              placeholder="filename"
              class="flex-1 p-3 bg-surface-0 border border-surface-3 rounded-l text-text-1"
            />
            <span class="p-3 bg-surface-2 border border-surface-3 border-l-0 rounded-r text-muted">
              .webm
            </span>
          </div>
        </div>

        {#if filenameExists}
          <div class="text-danger text-sm">
            Overwrites existing {stream.streamFilename}.webm
          </div>
        {/if}

        <!-- Storage mode toggle -->
        <div class="flex gap-2 text-sm">
          <button
            onclick={() => setPersistStream(true)}
            class="flex-1 p-2 rounded-sm cursor-pointer {stream.persistStream
              ? 'bg-success text-white border-none'
              : 'bg-surface-2 text-muted border border-surface-3'}"
          >
            Full Recording
          </button>
          <button
            onclick={() => setPersistStream(false)}
            class="flex-1 p-2 rounded-sm cursor-pointer {!stream.persistStream
              ? 'btn-primary border-none'
              : 'bg-surface-2 text-muted border border-surface-3'}"
          >
            Live Only (30s)
          </button>
        </div>
        <div class="text-muted text-xs">
          {stream.persistStream
            ? 'Saves entire stream to file'
            : 'Keeps only last 30 seconds (for live viewing)'}
        </div>

        <div class="flex gap-2">
          <button
            onclick={handleStartRecording}
            class="flex-1 p-3 btn-danger"
          >
            <span class="i-lucide-circle mr-2"></span>
            Start Recording
          </button>
          <button
            onclick={handleCancelPreview}
            class="p-3 btn-ghost"
          >
            Cancel
          </button>
        </div>
      </div>
    {/if}
  </div>
</div>
