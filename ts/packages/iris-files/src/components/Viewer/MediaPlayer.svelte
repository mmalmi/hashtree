<script lang="ts">
  /**
   * MediaPlayer - Streaming media player for video and audio
   *
   * Uses Service Worker streaming:
   * - /{npub}/{treeName}/{path} URLs intercepted by service worker
   * - Service worker requests data from main thread via MessageChannel
   * - Main thread streams data from hashtree
   * - Browser handles seeking, buffering, range requests natively
   *
   * No MSE needed - the browser handles everything!
   */
  import { SvelteURLSearchParams } from 'svelte/reactivity';
  import { recentlyChangedFiles } from '../../stores/recentlyChanged';
  import { currentHash } from '../../stores';
  import type { CID } from 'hashtree';
  import { getCidFileUrl, getNpubFileUrlAsync } from '../../lib/mediaUrl';
  import { getQueryParamsFromHash } from '../../lib/router.svelte';

  interface Props {
    cid: CID;
    fileName: string;
    fileSize?: number;
    /** Media type: 'video' or 'audio' */
    type?: 'video' | 'audio';
    /** Npub for live streaming support (optional) */
    npub?: string;
    /** Tree name for live streaming support (optional) */
    treeName?: string;
    /** Full path within tree for live streaming support (optional) */
    path?: string;
  }

  let props: Props = $props();
  let cid = $derived(props.cid);
  let fileName = $derived(props.fileName);
  let mediaType = $derived(props.type ?? 'video');
  let isAudio = $derived(mediaType === 'audio');
  let npub = $derived(props.npub);
  let treeName = $derived(props.treeName);
  let filePath = $derived(props.path);

  let mediaRef: HTMLVideoElement | HTMLAudioElement | undefined = $state();
  let loading = $state(true);
  let error = $state<string | null>(null);
  let duration = $state(0);
  let currentTime = $state(0);
  let paused = $state(true);

  // Check if live=1 is in URL hash params
  let hash = $derived($currentHash);
  let isLiveFromUrl = $derived.by(() => {
    return getQueryParamsFromHash(hash).get('live') === '1';
  });

  // Check if file is live (recently changed in this session)
  let changedFiles = $derived($recentlyChangedFiles);
  let isRecentlyChanged = $derived(changedFiles.has(fileName));

  // Combined live detection
  let isLive = $derived(isLiveFromUrl || isRecentlyChanged);

  // Remove ?live=1 from URL when stream ends
  function removeLiveParam() {
    const hashBase = window.location.hash.split('?')[0];
    const qIdx = window.location.hash.indexOf('?');
    if (qIdx === -1) return;

    const params = new SvelteURLSearchParams(window.location.hash.slice(qIdx + 1));
    if (!params.has('live')) return;

    params.delete('live');
    const queryString = params.toString();
    window.location.hash = queryString ? `${hashBase}?${queryString}` : hashBase;
  }

  // Load media using service worker streaming
  async function loadMedia() {
    if (!cid?.hash || !mediaRef) {
      error = 'No file CID or media element';
      loading = false;
      return;
    }

    // Use npub-based URL if we have the context (supports live streaming)
    // Otherwise fall back to CID-based URL
    let url: string;
    if (npub && treeName && filePath) {
      // Use async version to ensure Tauri htree server URL is available
      url = await getNpubFileUrlAsync(npub, treeName, filePath);
      console.log('[MediaPlayer] Using npub streaming (live-capable):', url);
    } else {
      url = getCidFileUrl(cid, fileName);
      console.log('[MediaPlayer] Using CID streaming:', url);
    }

    // Add cache-busting timestamp to prevent browser caching
    // This ensures we always get fresh content, especially for live streams
    mediaRef.src = `${url}?_t=${Date.now()}`;

    // Listen for metadata to get duration
    mediaRef.addEventListener('loadedmetadata', () => {
      if (mediaRef && !isNaN(mediaRef.duration) && isFinite(mediaRef.duration)) {
        duration = mediaRef.duration;
        console.log('[MediaPlayer] Duration:', duration);
        // Don't seek on initial load - let the video play from the start
        // Seeking to near-end is only done on tree updates when we want to catch up
      }
    }, { once: true });

    mediaRef.addEventListener('canplay', () => {
      loading = false;
      console.log('[MediaPlayer] Ready to play');
    }, { once: true });

    mediaRef.addEventListener('error', (e) => {
      console.error('[MediaPlayer] Error:', e);
      // Get more specific error message from MediaError
      const mediaError = mediaRef?.error;
      if (mediaError) {
        switch (mediaError.code) {
          case MediaError.MEDIA_ERR_ABORTED:
            error = 'Playback aborted';
            break;
          case MediaError.MEDIA_ERR_NETWORK:
            error = 'Network error while loading media';
            break;
          case MediaError.MEDIA_ERR_DECODE:
            error = 'Unable to play: codec not supported by your browser';
            break;
          case MediaError.MEDIA_ERR_SRC_NOT_SUPPORTED:
            error = 'Media format not supported by your browser';
            break;
          default:
            error = mediaError.message || 'Failed to load media';
        }
      } else {
        error = 'Failed to load media';
      }
      loading = false;
    }, { once: true });

    // Try to start playback
    try {
      await mediaRef.play();
    } catch (e) {
      console.log('[MediaPlayer] Autoplay blocked:', (e as Error).message);
    }
  }

  function handleTimeUpdate() {
    if (mediaRef) {
      currentTime = mediaRef.currentTime;
    }
  }

  function handleDurationChange() {
    if (mediaRef && !isNaN(mediaRef.duration) && isFinite(mediaRef.duration)) {
      duration = mediaRef.duration;
    }
  }

  function handlePlay() {
    paused = false;
  }

  function handlePause() {
    paused = true;
  }

  function handleEnded() {
    // If was live, remove the param
    if (isLiveFromUrl) {
      removeLiveParam();
    }
  }

  function togglePlay() {
    if (!mediaRef) return;
    if (mediaRef.paused) {
      mediaRef.play();
    } else {
      mediaRef.pause();
    }
  }

  function formatTime(seconds: number): string {
    if (!isFinite(seconds)) return '--:--';
    const m = Math.floor(seconds / 60);
    const s = Math.floor(seconds % 60);
    return `${m}:${s.toString().padStart(2, '0')}`;
  }

  // Load when mounted
  let hasStartedLoading = false;
  $effect(() => {
    if (mediaRef && !hasStartedLoading) {
      hasStartedLoading = true;
      loadMedia().catch((e) => {
        console.error('Failed to load media:', e);
        error = e instanceof Error ? e.message : 'Failed to load media';
        loading = false;
      });
    }
  });
</script>

<div class="flex-1 flex flex-col min-h-0 overflow-hidden" class:bg-black={!isAudio} class:bg-surface-0={isAudio}>
  <div class="relative flex-1 flex flex-col items-center justify-center min-h-0" class:p-4={isAudio}>
    <!-- Loading overlay -->
    {#if loading}
      <div class="absolute inset-0 flex items-center justify-center text-white z-20" class:bg-black={!isAudio} class:bg-surface-0={isAudio} class:text-text-1={isAudio}>
        <div class="flex flex-col items-center gap-2">
          <span class="i-lucide-loader-2 animate-spin text-2xl"></span>
          <span>Loading {isAudio ? 'audio' : 'video'}...</span>
        </div>
      </div>
    {/if}

    <!-- Error overlay -->
    {#if error}
      <div class="absolute inset-0 flex items-center justify-center text-red-400 z-20" class:bg-black={!isAudio} class:bg-surface-0={isAudio}>
        <span class="i-lucide-alert-circle mr-2"></span>
        {error}
      </div>
    {/if}

    <!-- Live indicator (video only) -->
    {#if !isAudio && isLive && !loading && !error}
      <div class="absolute top-3 left-3 z-10 flex items-center gap-2 px-2 py-1 bg-red-600 text-white text-sm font-bold rounded">
        <span class="w-2 h-2 bg-white rounded-full animate-pulse"></span>
        LIVE
      </div>
    {/if}

    {#if isAudio}
      <!-- Audio visual placeholder -->
      <div class="w-full max-w-md flex flex-col items-center gap-4">
        <div class="w-48 h-48 rounded-lg bg-surface-2 flex items-center justify-center shadow-lg">
          <span class="i-lucide-music text-6xl text-text-2"></span>
        </div>
        <audio
          bind:this={mediaRef}
          controls
          autoplay
          class="w-full"
          class:invisible={loading || error}
          preload="metadata"
          ontimeupdate={handleTimeUpdate}
          ondurationchange={handleDurationChange}
          onplay={handlePlay}
          onpause={handlePause}
          onended={handleEnded}
        >
          Your browser does not support the audio tag.
        </audio>
      </div>
    {:else}
      <!-- Video element -->
      <video
        bind:this={mediaRef}
        controls
        autoplay
        playsinline
        class="max-w-full max-h-full object-contain"
        class:invisible={loading || error}
        preload="metadata"
        ontimeupdate={handleTimeUpdate}
        ondurationchange={handleDurationChange}
        onplay={handlePlay}
        onpause={handlePause}
        onended={handleEnded}
      >
        Your browser does not support the video tag.
      </video>

      <!-- Big play button overlay when paused (video only) -->
      {#if paused && !loading && !error}
        <button
          type="button"
          class="absolute inset-0 flex items-center justify-center z-10 cursor-pointer bg-transparent"
          onclick={togglePlay}
          aria-label="Play video"
        >
          <div class="w-20 h-20 rounded-full bg-white/90 flex items-center justify-center shadow-lg hover:bg-white hover:scale-110 transition-all">
            <span class="i-lucide-play w-10 h-10 text-black ml-1"></span>
          </div>
        </button>
      {/if}

      <!-- Duration/time info (video only) -->
      {#if !loading && !error}
        <div class="absolute bottom-16 right-3 z-10 px-2 py-1 bg-black/70 text-white text-sm rounded">
          {formatTime(currentTime)} / {formatTime(duration)}
          {#if isLive}
            <span class="ml-2 text-xs text-gray-400">streaming</span>
          {/if}
        </div>
      {/if}
    {/if}
  </div>
</div>
