<script lang="ts">
  /**
   * VideoView - Video player page
   * Shows video player, metadata, owner info, and comments
   *
   * Uses Service Worker streaming via /htree/ URLs (no blob URLs!)
   */
  import { onDestroy, onMount, untrack } from 'svelte';
  import { SvelteSet } from 'svelte/reactivity';
  import { nip19 } from 'nostr-tools';
  import { getTree } from '../../store';
  import { ndk, nostrStore, npubToPubkey } from '../../nostr';
  import { treeRootStore, createTreesStore, routeStore } from '../../stores';
  import ShareButton from '../ShareButton.svelte';
  import { open as openBlossomPushModal } from '../Modals/BlossomPushModal.svelte';
  import { open as openAddToPlaylistModal } from '../Modals/AddToPlaylistModal.svelte';
  import type { TreeVisibility } from '@hashtree/core';
  import { deleteTree } from '../../nostr';
  import { updateLocalRootCacheHex } from '../../treeRootCache';
  import {
    addRecent,
    updateVideoPosition,
    getVideoPosition,
    clearVideoPosition,
    updateRecentLabel,
    updateRecentDuration,
    removeRecentByTreeName,
  } from '../../stores/recents';
  import { recordDeletedVideo } from '../../stores/videoDeletes';
  import { Avatar, Name, FollowButton } from '../User';
  import VisibilityIcon from '../VisibilityIcon.svelte';
  import VideoDescription from './VideoDescription.svelte';
  import VideoComments from './VideoComments.svelte';
  import PlaylistSidebar from './PlaylistSidebar.svelte';
  import FeedSidebar from './FeedSidebar.svelte';
  import AmbientGlow from './AmbientGlow.svelte';
  import { ambientColor } from '../../stores/ambientGlow';
  import { getFollowers, socialGraphStore } from '../../utils/socialGraph';
  import { currentPlaylist, loadPlaylist, playNext, repeatMode, shuffleEnabled } from '../../stores/playlist';
  import type { CID } from '@hashtree/core';
  import { toHex, nhashEncode } from '@hashtree/core';
  import { appendHtreeCacheBust, getHtreePrefix, getNpubFileUrl, getNpubFileUrlAsync, getNhashFileUrl, getThumbnailUrl, onHtreePrefixReady } from '../../lib/mediaUrl';
  import { logHtreeDebug } from '../../lib/htreeDebug';
  import { ensureMediaStreamingReady } from '../../lib/mediaStreamingSetup';
  import { NDKEvent, type NDKFilter } from 'ndk';
  import { VideoZapButton } from '../Zaps';
  import { formatTimeAgo } from '../../utils/format';
  import { settingsStore } from '../../stores/settings';

  let deleting = $state(false);
  let editing = $state(false);
  let saving = $state(false);
  let editTitle = $state('');
  let editDescription = $state('');

  // Like state
  const likes = new SvelteSet<string>(); // Set of pubkeys who liked
  let userLiked = $state(false);
  let liking = $state(false);

  // Playlist state
  let showPlaylistSidebar = $state(true);
  let playlist = $derived($currentPlaylist);
  let repeat = $derived($repeatMode);

  // Mobile comments toggle (closed by default on mobile)
  let mobileCommentsOpen = $state(false);
  let commentCount = $state(0);
  let workerRootSyncSignature = $state<string | null>(null);

  // Thumbnail color for description box background
  let thumbnailColor = $state<{ r: number; g: number; b: number } | null>(null);

  // Theater mode (from settings)
  let theaterMode = $derived($settingsStore.video.theaterMode);
  function toggleTheaterMode() {
    settingsStore.setVideoSettings({ theaterMode: !theaterMode });
  }
  let shuffle = $derived($shuffleEnabled);

  interface Props {
    npub?: string;
    treeName?: string;   // Full tree name from router (e.g., "videos/koiran kanssa")
    wild?: string;       // Additional path after tree name (e.g., "videoId" for playlists)
  }

  let { npub, treeName: treeNameProp, wild }: Props = $props();

  // Tree name comes directly from router param (already decoded, includes "videos/")
  // wild contains additional path for playlist videos
  let videoPath = $derived.by(() => {
    if (!treeNameProp) return '';
    // Remove "videos/" prefix to get the video/playlist name
    const basePath = treeNameProp.startsWith('videos/') ? treeNameProp.slice(7) : treeNameProp;
    // Append wild path if present (for playlist videos)
    return wild ? `${basePath}/${wild}` : basePath;
  });
  let pathParts = $derived(videoPath.split('/'));
  let isPlaylistVideo = $derived(pathParts.length > 1);

  // For playlists, the tree is the channel (parent), not the full path
  let channelName = $derived(isPlaylistVideo ? pathParts.slice(0, -1).join('/') : null);
  let currentVideoId = $derived(isPlaylistVideo ? pathParts[pathParts.length - 1] : null);

  // The actual tree name to resolve - use prop directly or construct from channel
  // - Single video: videos/VideoTitle (treeNameProp)
  // - Playlist video: videos/ChannelName (the video is a subdirectory within)
  let treeName = $derived.by(() => {
    if (!treeNameProp) return undefined;
    if (isPlaylistVideo && channelName) {
      return `videos/${channelName}`;
    }
    return treeNameProp;
  });

  let videoSrc = $state<string>('');  // SW URL (not blob!)
  let videoFileName = $state<string>('');  // For MIME type detection
  let videoFallbackQueue = $state<Array<{ url: string; fileName: string }>>([]);
  let loading = $state(true);
  let showLoading = $state(false);  // Delayed loading indicator
  let loadingTimer: ReturnType<typeof setTimeout> | null = null;
  let rootTimeoutTimer: ReturnType<typeof setTimeout> | null = null;
  let error = $state<string | null>(null);
  let videoTitle = $state<string>('');
  let videoDescription = $state<string>('');
  let videoCreatedAt = $state<number | null>(null);  // Unix timestamp in seconds
  let videoCid = $state<CID | null>(null);  // CID of the video FILE (video.mp4)
  let videoFolderCid = $state<CID | null>(null);  // CID of the video FOLDER (contains video.mp4, title.txt, etc.)
  let videoVisibility = $state<TreeVisibility>('public');
  let videoRef: HTMLVideoElement | undefined = $state();
  const VIDEO_RESOLVE_TIMEOUT_MS = 10000;

  function logVideoDebug(event: string, data?: Record<string, unknown>) {
    logHtreeDebug(`video:${event}`, data);
  }

  async function syncTreeRootToWorker(
    npubValue: string,
    treeNameValue: string,
    rootCidValue: CID,
    visibility: TreeVisibility
  ): Promise<void> {
    const signature = `${npubValue}/${treeNameValue}:${toHex(rootCidValue.hash)}:${rootCidValue.key ? toHex(rootCidValue.key) : ''}:${visibility}`;
    if (workerRootSyncSignature === signature) return;

    try {
      const { getWorkerAdapter, waitForWorkerAdapter } = await import('../../lib/workerInit');
      const adapter = getWorkerAdapter() ?? await waitForWorkerAdapter(2000);
      if (!adapter || !('setTreeRootCache' in adapter)) return;
      await (adapter as { setTreeRootCache: (npub: string, treeName: string, hash: Uint8Array, key?: Uint8Array, visibility?: TreeVisibility) => Promise<void> })
        .setTreeRootCache(npubValue, treeNameValue, rootCidValue.hash, rootCidValue.key, visibility);
      workerRootSyncSignature = signature;
    } catch (err) {
      console.warn('[VideoView] Failed to sync tree root to worker:', err);
    }
  }

  let htreePrefix = $state<string>(getHtreePrefix());
  let htreePrefixVersion = $state(0);
  onHtreePrefixReady((prefix) => {
    htreePrefix = prefix;
    htreePrefixVersion += 1;
    logVideoDebug('prefix:ready', { prefix });
  });

  function resolveDirectPrefix(): string {
    if (htreePrefix) return htreePrefix;
    if (typeof window !== 'undefined') {
      const baseUrl = window.htree?.htreeBaseUrl;
      if (typeof baseUrl === 'string' && baseUrl.trim()) {
        return baseUrl.trim().replace(/\/$/, '');
      }
    }
    return '';
  }

  function hasDirectHtreeServer(): boolean {
    const prefix = resolveDirectPrefix();
    if (prefix && prefix !== htreePrefix) {
      htreePrefix = prefix;
      logVideoDebug('prefix:sync', { prefix });
    }
    return !!prefix;
  }

  function buildDirectUrl(prefix: string, npub: string, treeName: string, path: string): string {
    const encodedTreeName = encodeURIComponent(treeName);
    const encodedPath = path.split('/').map(encodeURIComponent).join('/');
    return appendHtreeCacheBust(`${prefix}/htree/${npub}/${encodedTreeName}/${encodedPath}`);
  }

  function buildDirectVideoCandidates(npub: string, treeName: string, videoPathPrefix: string) {
    const candidates = ['video.mp4', 'video.webm', 'video.mov', 'video.mkv'];
    const prefix = resolveDirectPrefix();
    if (!prefix) return [];
    return candidates.map((fileName) => ({
      fileName,
      url: buildDirectUrl(prefix, npub, treeName, `${videoPathPrefix}${fileName}`),
    }));
  }

  function startDirectVideoFallback(npub: string, treeName: string, videoPathPrefix: string): boolean {
    if (!hasDirectHtreeServer()) {
      logVideoDebug('direct:skip', { reason: 'no-prefix', npub, treeName });
      return false;
    }
    const candidates = buildDirectVideoCandidates(npub, treeName, videoPathPrefix);
    if (candidates.length === 0) return false;
    videoFallbackQueue = candidates.slice(1);
    videoFileName = candidates[0].fileName;
    videoSrc = candidates[0].url;
    loading = false;
    logVideoDebug('direct:start', {
      fileName: videoFileName,
      url: videoSrc,
    });
    return true;
  }

  function ensureDirectVideoFallback(reason: string): void {
    const currentNpub = npub;
    const currentTreeName = treeName;
    if (!currentNpub || !currentTreeName) {
      logVideoDebug('direct:ensure-skip', {
        reason,
        npub: currentNpub ?? null,
        treeName: currentTreeName ?? null,
      });
      return;
    }
    if (videoSrc) return;
    const videoPathPrefix = isPlaylistVideo && currentVideoId ? `${currentVideoId}/` : '';
    if (!hasDirectHtreeServer()) {
      logVideoDebug('direct:ensure-pending', { reason, prefix: htreePrefix });
      return;
    }
    startDirectVideoFallback(currentNpub, currentTreeName, videoPathPrefix);
    logVideoDebug('direct:ensure', { reason });
  }

  onMount(() => {
    logVideoDebug('mount', {
      npub: npub ?? null,
      treeName: treeName ?? null,
    });
    ensureDirectVideoFallback('mount');
    const timer = setTimeout(() => {
      ensureDirectVideoFallback('mount:timeout');
    }, 1500);
    return () => {
      clearTimeout(timer);
    };
  });

  function advanceVideoFallback(): boolean {
    if (videoFallbackQueue.length === 0) return false;
    const [next, ...rest] = videoFallbackQueue;
    videoFallbackQueue = rest;
    videoFileName = next.fileName;
    videoSrc = next.url;
    logVideoDebug('direct:advance', {
      fileName: videoFileName,
      url: videoSrc,
    });
    return true;
  }

  function handleVideoError() {
    logVideoDebug('player:error', {
      fileName: videoFileName,
      url: videoSrc,
    });
    if (advanceVideoFallback()) {
      error = null;
      return;
    }
    if (!error) {
      error = 'Video failed to load';
    }
    loading = false;
  }

  async function resolvePathWithTimeout(tree: ReturnType<typeof getTree>, cid: CID, path: string) {
    try {
      const startMs = performance.now();
      const result = await Promise.race([
        tree.resolvePath(cid, path),
        new Promise<null>((resolve) => setTimeout(() => resolve(null), VIDEO_RESOLVE_TIMEOUT_MS)),
      ]);
      if (!result) {
        logVideoDebug('resolve:timeout', {
          path,
          elapsedMs: Math.round(performance.now() - startMs),
        });
      }
      return result ?? null;
    } catch {
      logVideoDebug('resolve:error', { path });
      return null;
    }
  }

  async function listDirectoryWithTimeout(tree: ReturnType<typeof getTree>, cid: CID) {
    try {
      const startMs = performance.now();
      const result = await Promise.race([
        tree.listDirectory(cid),
        new Promise<null>((resolve) => setTimeout(() => resolve(null), VIDEO_RESOLVE_TIMEOUT_MS)),
      ]);
      if (!result) {
        logVideoDebug('list:timeout', {
          elapsedMs: Math.round(performance.now() - startMs),
        });
      }
      return result ?? null;
    } catch {
      logVideoDebug('list:error');
      return null;
    }
  }

  // Read saved video settings directly from localStorage (synchronous)
  function getSavedVideoSettings(): { volume: number; muted: boolean } {
    try {
      const saved = localStorage.getItem('video-settings');
      if (saved) {
        const parsed = JSON.parse(saved);
        return {
          volume: typeof parsed.volume === 'number' ? parsed.volume : 1,
          muted: typeof parsed.muted === 'boolean' ? parsed.muted : false,
        };
      }
    } catch {}
    return { volume: 1, muted: false };
  }

  // Initial values read synchronously before any rendering
  const initialVideoSettings = getSavedVideoSettings();

  // Apply saved volume/muted settings via action
  function applyVolumeSettings(node: HTMLVideoElement) {
    node.volume = initialVideoSettings.volume;
    node.muted = initialVideoSettings.muted;
  }

  // Full video path for position tracking (includes npub and videoId for playlists)
  let videoFullPath = $derived.by(() => {
    if (!npub || !treeName) return null;
    // For playlist videos, include the videoId to track each video's position separately
    if (isPlaylistVideo && currentVideoId) {
      return `/${npub}/${treeName}/${currentVideoId}`;
    }
    return `/${npub}/${treeName}`;
  });

  // Get timestamp from route params
  function getTimestampFromUrl(): number | null {
    const t = $routeStore.params.get('t');
    if (t) {
      const seconds = parseInt(t, 10);
      if (!isNaN(seconds) && seconds >= 0) return seconds;
    }
    return null;
  }

  // Track if we've restored position for this video
  let positionRestored = $state(false);

  // Restore position when video loads - prioritize URL ?t= param over saved position
  function restorePosition() {
    if (!videoRef || !videoFullPath || positionRestored) return;
    if (!videoRef.duration || videoRef.duration === 0) return;

    // Check for ?t= param first (direct link to timestamp)
    const urlTimestamp = getTimestampFromUrl();
    if (urlTimestamp !== null && videoRef.duration > urlTimestamp) {
      videoRef.currentTime = urlTimestamp;
      positionRestored = true;
      console.log('[VideoView] Seeking to URL timestamp:', urlTimestamp);
      return;
    }

    // Fall back to saved position
    const savedPosition = getVideoPosition(videoFullPath);
    if (savedPosition > 0 && videoRef.duration > savedPosition) {
      videoRef.currentTime = savedPosition;
      positionRestored = true;
      console.log('[VideoView] Restored position:', savedPosition);
    } else {
      positionRestored = true;
    }
  }

  function handleLoadedMetadata() {
    restorePosition();
    // Restore saved volume and muted state
    if (videoRef) {
      const { volume, muted } = $settingsStore.video;
      videoRef.volume = volume;
      videoRef.muted = muted;
    }
    // Save video duration to recents for display in video cards
    if (videoRef && videoRef.duration && isFinite(videoRef.duration) && videoFullPath) {
      updateRecentDuration(videoFullPath, videoRef.duration);
    }
    // Update metadata.json for own videos if duration is missing
    maybeUpdateOwnVideoMetadata();
  }

  // Save volume and muted state when user changes them
  function handleVolumeChange() {
    if (videoRef) {
      settingsStore.setVideoSettings({ volume: videoRef.volume, muted: videoRef.muted });
    }
  }

  let metadataUpdateAttempted = false;

  async function maybeUpdateOwnVideoMetadata() {
    // Only update own videos, only once per view, only if we have duration
    if (!isOwner || metadataUpdateAttempted || !videoRef?.duration || !isFinite(videoRef.duration)) return;
    if (!npub || !treeName || !rootCid) return;

    metadataUpdateAttempted = true;
    const duration = Math.round(videoRef.duration);

    try {
      const tree = getTree();

      // Find video file entry
      const entries = await tree.listDirectory(rootCid);
      const videoEntry = entries?.find(e =>
        e.name.startsWith('video.') ||
        e.name.endsWith('.webm') ||
        e.name.endsWith('.mp4') ||
        e.name.endsWith('.mov')
      );
      if (!videoEntry) return;

      // Get existing metadata from link entry
      const existingMeta = (videoEntry.meta as Record<string, unknown>) || {};

      // Skip if duration already exists
      if (existingMeta.duration) return;

      // Update metadata with duration
      const updatedMeta: Record<string, unknown> = {
        ...existingMeta,
        duration,
        createdAt: existingMeta.createdAt || Math.floor(Date.now() / 1000),
        title: existingMeta.title || videoTitle || treeName.replace('videos/', ''),
      };

      // Update video entry with new metadata
      const newRootCid = await tree.setEntry(
        rootCid,
        [],
        videoEntry.name,
        videoEntry.cid,
        videoEntry.size,
        videoEntry.type,
        updatedMeta
      );

      // Save updated tree
      updateLocalRootCacheHex(
        npub,
        treeName,
        toHex(newRootCid.hash),
        newRootCid.key ? toHex(newRootCid.key) : undefined,
        videoVisibility
      );

      console.log('[VideoView] Updated video entry metadata with duration:', duration);
    } catch (e) {
      console.error('[VideoView] Failed to update metadata:', e);
    }
  }

  // Also try to restore position when videoFullPath becomes available
  // (in case loadedmetadata fired before path was computed)
  $effect(() => {
    if (videoFullPath && videoRef && videoRef.readyState >= 1) {
      restorePosition();
    }
  });

  // Listen for URL changes (timestamp clicks update URL)
  $effect(() => {
    if (!videoRef) return;

    function handleHashChange() {
      const urlTimestamp = getTimestampFromUrl();
      if (urlTimestamp !== null && videoRef && videoRef.duration > urlTimestamp) {
        videoRef.currentTime = urlTimestamp;
        console.log('[VideoView] Seeking to timestamp:', urlTimestamp);
      }
    }

    window.addEventListener('hashchange', handleHashChange);
    return () => window.removeEventListener('hashchange', handleHashChange);
  });

  // Save position on timeupdate
  function handleTimeUpdate() {
    if (!videoRef || !videoFullPath) return;
    updateVideoPosition(videoFullPath, videoRef.currentTime);
  }

  // Clear position when video ends and handle auto-play/repeat
  function handleEnded() {
    if (videoFullPath) {
      clearVideoPosition(videoFullPath);
    }

    // Handle repeat mode
    if (repeat === 'one') {
      // Repeat current video
      if (videoRef) {
        videoRef.currentTime = 0;
        videoRef.play();
      }
      return;
    }

    // Auto-play next video (always enabled for playlists, like YouTube)
    if (playlist && playlist.items.length > 1) {
      // Check if we're at the end and repeat is off
      const isLastVideo = playlist.currentIndex === playlist.items.length - 1;
      const shouldWrap = repeat === 'all' || shuffle;

      if (isLastVideo && !shouldWrap && !shuffle) {
        // End of playlist, repeat off, not shuffling - stop
        console.log('[VideoView] End of playlist, stopping');
        return;
      }

      const nextUrl = playNext({ wrap: shouldWrap });
      if (nextUrl) {
        console.log('[VideoView] Auto-playing next video');
        window.location.hash = nextUrl;
      }
    }
  }

  // Derive owner pubkey
  let ownerPubkey = $derived.by(() => {
    if (!npub) return null;
    try {
      const decoded = nip19.decode(npub);
      if (decoded.type === 'npub') return decoded.data as string;
    } catch {}
    return null;
  });

  // Video title from title.txt or video path (last segment for playlists)
  let title = $derived(videoTitle || currentVideoId || videoPath || 'Video');

  // Current user
  let currentUserNpub = $derived($nostrStore.npub);
  let userPubkey = $derived($nostrStore.pubkey);
  let isLoggedIn = $derived($nostrStore.isLoggedIn);
  let isOwner = $derived(npub === currentUserNpub);

  // Social graph for known followers (like YouTube subscriber count)
  let graphVersion = $derived($socialGraphStore.version);
  let knownFollowers = $derived.by(() => {
    graphVersion; // Subscribe to changes
    return ownerPubkey ? getFollowers(ownerPubkey) : new Set();
  });

  // Get root CID from treeRootStore (handles linkKey decryption)
  let rootCid = $state<CID | null>(null);
  const rootCidUnsub = treeRootStore.subscribe((next) => {
    rootCid = next;
    logVideoDebug('root:store', {
      hasCid: !!next,
      hash: next ? toHex(next.hash).slice(0, 16) : null,
    });
  });
  onDestroy(() => {
    rootCidUnsub();
  });
  let lastRootHash = $state<string | null>(null);

  $effect(() => {
    const cid = rootCid;
    const currentHash = cid ? toHex(cid.hash).slice(0, 16) : null;
    if (currentHash === lastRootHash) return;
    lastRootHash = currentHash;
    logVideoDebug('root:change', {
      hasCid: !!cid,
      hash: currentHash,
      npub,
      treeName,
    });
  });

  // Generate nhash for permalink - uses video file CID (not root dir) so same content = same link
  let videoNhash = $derived.by(() => {
    if (!videoCid) return undefined;
    return nhashEncode(videoCid);
  });

  // Subscribe to trees store to get visibility and createdAt from Nostr event
  $effect(() => {
    const currentNpub = npub;
    const currentTreeName = treeName;
    if (!currentNpub || !currentTreeName) return;

    const store = createTreesStore(currentNpub);
    const unsub = store.subscribe(trees => {
      const tree = trees.find(t => t.name === currentTreeName);
      if (tree?.visibility) {
        untrack(() => {
          videoVisibility = tree.visibility as TreeVisibility;
        });
      }
      // Use Nostr event createdAt if video metadata doesn't have it
      if (tree?.createdAt && !videoCreatedAt) {
        untrack(() => {
          videoCreatedAt = tree.createdAt!;
        });
      }
    });
    return unsub;
  });

  // Effective visibility: infer from k param in URL if trees store doesn't have it
  // k param in URL means link-visible
  let effectiveVisibility = $derived.by(() => {
    if (videoVisibility !== 'public') return videoVisibility;
    if ($routeStore.params.get('k')) return 'link-visible' as TreeVisibility;
    return videoVisibility;
  });

  // Track what we've loaded to avoid unnecessary reloads
  let loadedVideoKey = $state<string | null>(null);
  let lastPrefixVersion = $state(0);
  let missingPropsLogged = $state(false);
  let loadEffectRuns = $state(0);

  // Load video when rootCid or videoPath changes
  // For playlist videos, rootCid is the same but videoPath changes
  $effect(() => {
    const cid = rootCid;
    const path = videoPath; // Subscribe to videoPath changes
    const isPlaylist = isPlaylistVideo; // Capture reactively
    const currentNpub = npub;
    const currentTreeName = treeName;
    const currentVideoIdValue = currentVideoId;
    const prefixVersion = htreePrefixVersion;
    const runCount = untrack(() => {
      loadEffectRuns += 1;
      return loadEffectRuns;
    });
    logVideoDebug('load:effect', {
      run: runCount,
      npub: currentNpub ?? null,
      treeName: currentTreeName ?? null,
      hasRoot: !!cid,
      hasSrc: !!videoSrc,
    });
    if (!currentNpub || !currentTreeName) {
      if (!missingPropsLogged) {
        logVideoDebug('load:skip', {
          npub: currentNpub ?? null,
          treeName: currentTreeName ?? null,
        });
        missingPropsLogged = true;
      }
      return;
    }
    missingPropsLogged = false;

    // Build a key to identify this specific video
    const videoKey = `${cid ? toHex(cid.hash) : 'no-root'}:${path}`;

    // Skip reload if we already loaded this exact video
    if (videoKey === loadedVideoKey) {
      const videoPathPrefix = isPlaylist && currentVideoIdValue ? `${currentVideoIdValue}/` : '';
      if (!videoSrc && hasDirectHtreeServer()) {
        error = null;
        startDirectVideoFallback(currentNpub, currentTreeName, videoPathPrefix);
      }
      if (prefixVersion !== lastPrefixVersion) {
        lastPrefixVersion = prefixVersion;
        if (videoSrc && videoFileName) {
          const directPrefix = resolveDirectPrefix();
          const nextSrc = directPrefix
            ? buildDirectUrl(directPrefix, currentNpub, currentTreeName, `${videoPathPrefix}${videoFileName}`)
            : getNpubFileUrl(currentNpub, currentTreeName, `${videoPathPrefix}${videoFileName}`);
          if (videoSrc !== nextSrc) {
            videoSrc = nextSrc;
            error = null;
          }
        }
      }
      return;
    }

    logVideoDebug('load:reset', {
      npub: currentNpub,
      treeName: currentTreeName,
      videoId: currentVideoIdValue,
      hasRoot: !!cid,
      prefix: htreePrefix,
    });

    // Reset state for new video
    videoSrc = '';
    videoFileName = '';
    videoTitle = '';
    videoDescription = '';
    videoCreatedAt = null;
    loading = true;
    error = null;
    positionRestored = false;
    videoFallbackQueue = [];
    loadedVideoKey = videoKey;
    lastPrefixVersion = prefixVersion;

    // Clear playlist if navigating to a non-playlist video
    if (!isPlaylist) {
      untrack(() => {
        currentPlaylist.set(null);
      });
    }

    if (cid) {
      untrack(() => loadVideo(cid));
      return;
    }

    if (hasDirectHtreeServer()) {
      const videoPathPrefix = isPlaylist && currentVideoIdValue ? `${currentVideoIdValue}/` : '';
      untrack(() => {
        startDirectVideoFallback(currentNpub, currentTreeName, videoPathPrefix);
      });
    }
  });

  // Delayed loading indicator - only show after 2 seconds
  $effect(() => {
    if (!loading) {
      // Video loaded - clear timer and hide loading
      if (loadingTimer) {
        clearTimeout(loadingTimer);
        loadingTimer = null;
      }
      showLoading = false;
    } else if (!loadingTimer && !showLoading) {
      // Still loading - start timer to show indicator
      loadingTimer = setTimeout(() => {
        showLoading = true;
        loadingTimer = null;
      }, 2000);
    }

    return () => {
      if (loadingTimer) {
        clearTimeout(loadingTimer);
        loadingTimer = null;
      }
    };
  });

  // Timeout for tree root resolution - show error if not resolved within 15 seconds
  $effect(() => {
    const cid = rootCid;
    const currentTreeName = treeName;

    // Clear any existing timeout
    if (rootTimeoutTimer) {
      clearTimeout(rootTimeoutTimer);
      rootTimeoutTimer = null;
    }

    if (cid) {
      // Root resolved - no timeout needed
      return;
    }

    if (!currentTreeName) {
      // No tree name - nothing to resolve
      return;
    }

    // Start timeout for root resolution (e.g., Nostr event not found on relays)
    rootTimeoutTimer = setTimeout(() => {
      if (!rootCid && loading && !error) {
        error = 'Video not found. The video metadata may not be available from your relays.';
        loading = false;
        console.warn('[VideoView] Tree root resolution timeout for:', currentTreeName);
      }
    }, 15000);

    return () => {
      if (rootTimeoutTimer) {
        clearTimeout(rootTimeoutTimer);
        rootTimeoutTimer = null;
      }
    };
  });

  // No blob URL cleanup needed - using SW URLs

  async function loadVideo(rootCidParam: CID) {
    // Capture reactive values at the start - they may change during async operations
    // due to navigation. Using captured values ensures consistent behavior.
    const capturedNpub = npub;
    const capturedTreeName = treeName;
    const capturedIsPlaylistVideo = isPlaylistVideo;
    const capturedVideoId = currentVideoId;
    const capturedVisibility = effectiveVisibility;

    if (!capturedNpub || !capturedTreeName) return;

    error = null;
    logVideoDebug('load:start', {
      npub: capturedNpub,
      treeName: capturedTreeName,
      videoId: capturedVideoId,
      rootCid: toHex(rootCidParam.hash).slice(0, 8),
      prefix: htreePrefix,
    });

    const streamingReady = await ensureMediaStreamingReady().catch((err) => {
      console.warn('[VideoView] Media streaming setup failed:', err);
      return false;
    });
    if (!streamingReady) {
      error = 'Video streaming unavailable. Please reload and try again.';
      loading = false;
      return;
    }

    await syncTreeRootToWorker(
      capturedNpub,
      capturedTreeName,
      rootCidParam,
      capturedVisibility ?? 'public'
    );

    const tree = getTree();

    // For playlist videos, we need to first navigate to the video subdirectory
    let videoDirCid = rootCidParam;
    let videoPathPrefix = capturedIsPlaylistVideo && capturedVideoId ? `${capturedVideoId}/` : '';

    // Start direct playback ASAP when local htree server is available.
    startDirectVideoFallback(capturedNpub, capturedTreeName, videoPathPrefix);

    async function applyResolvedVideo(entryCid: CID, fileName: string) {
      videoCid = entryCid;
      videoFileName = fileName;
      const directPrefix = resolveDirectPrefix();
      const nextSrc = directPrefix
        ? buildDirectUrl(directPrefix, capturedNpub, capturedTreeName, videoPathPrefix + fileName)
        : await getNpubFileUrlAsync(capturedNpub, capturedTreeName, videoPathPrefix + fileName);
      if (videoSrc !== nextSrc) {
        videoSrc = nextSrc;
      }
      videoFallbackQueue = [];
      loading = false;
      logVideoDebug('load:resolved', {
        fileName,
        url: nextSrc,
      });
    }

    if (capturedIsPlaylistVideo && capturedVideoId) {
      // Navigate to the video subdirectory within the playlist
      try {
        // Add timeout to prevent hanging if Blossom is unreachable
        const resolvePromise = tree.resolvePath(rootCidParam, capturedVideoId);
        const timeoutPromise = new Promise<null>((_, reject) =>
          setTimeout(() => reject(new Error('Timeout: Video data not available from network')), 30000)
        );
        const videoDir = await Promise.race([resolvePromise, timeoutPromise]);
        if (videoDir) {
          videoDirCid = videoDir.cid;
          videoPathPrefix = `${capturedVideoId}/`;
        } else {
          error = `Video "${capturedVideoId}" not found in playlist`;
          loading = false;
          logVideoDebug('load:missing-playlist', {
            videoId: capturedVideoId,
          });
          return;
        }
      } catch (e) {
        error = e instanceof Error ? e.message : `Failed to load video: ${e}`;
        loading = false;
        logVideoDebug('load:playlist-error', {
          videoId: capturedVideoId,
          error,
        });
        return;
      }
    }

    // Store the video folder CID (for adding to other playlists)
    videoFolderCid = videoDirCid;

    // Try common video filenames immediately (don't wait for directory listing)
    const commonNames = ['video.webm', 'video.mp4', 'video.mov', 'video.mkv'];
    for (const name of commonNames) {
      try {
        const result = await resolvePathWithTimeout(tree, videoDirCid, name);
        if (result) {
          await applyResolvedVideo(result.cid, name);
          break;
        }
      } catch {}
    }

    // If common names didn't work, list directory to find video
    if (!videoSrc) {
      try {
        const dir = await listDirectoryWithTimeout(tree, videoDirCid);
        const videoEntry = dir?.find(e =>
          e.name.startsWith('video.') ||
          e.name.endsWith('.webm') ||
          e.name.endsWith('.mp4') ||
          e.name.endsWith('.mov') ||
          e.name.endsWith('.mkv')
        );

        if (videoEntry) {
          const videoResult = await resolvePathWithTimeout(tree, videoDirCid, videoEntry.name);
          if (videoResult) {
            await applyResolvedVideo(videoResult.cid, videoEntry.name);
          }
        }
      } catch {}
    }

    // If still no video and NOT a playlist video, check if this is a playlist directory root
    if (!videoSrc && !capturedIsPlaylistVideo) {
      const { findFirstVideoEntry } = await import('../../stores/playlist');
      const firstVideoId = await Promise.race([
        findFirstVideoEntry(rootCidParam),
        new Promise<null>((resolve) => setTimeout(() => resolve(null), VIDEO_RESOLVE_TIMEOUT_MS)),
      ]);
      if (firstVideoId) {
        // Redirect to first video in playlist using replaceState to avoid back-loop
        const playlistUrl = `#/${capturedNpub}/${encodeURIComponent(capturedTreeName)}/${encodeURIComponent(firstVideoId)}`;
        history.replaceState(null, '', playlistUrl);
        window.dispatchEvent(new HashChangeEvent('hashchange'));
        return;
      }
    }

    if (!videoSrc && startDirectVideoFallback(capturedNpub, capturedTreeName, videoPathPrefix)) {
      return;
    }

    if (!videoSrc) {
      error = 'Video file not found';
      loading = false;
      logVideoDebug('load:not-found', {
        npub: capturedNpub,
        treeName: capturedTreeName,
        videoId: capturedVideoId,
      });
      return;
    }

    // Add to recents - use full path for playlist videos
    // Compute recentPath first so we can pass it to loadMetadata
    const recentPath = capturedIsPlaylistVideo && capturedVideoId
      ? `/${capturedNpub}/${capturedTreeName}/${capturedVideoId}`
      : `/${capturedNpub}/${capturedTreeName}`;

    const treeTitle = capturedTreeName?.startsWith('videos/')
      ? capturedTreeName.slice(7)
      : capturedTreeName;

    addRecent({
      type: 'tree',
      path: recentPath,
      label: videoTitle || treeTitle || capturedVideoId || videoPath || 'Video',
      npub: capturedNpub,
      treeName: capturedTreeName,
      videoId: capturedIsPlaylistVideo ? capturedVideoId : undefined,
      visibility: videoVisibility,
    });

    // Load metadata in background (don't block video playback)
    // For playlist videos, load from the video subdirectory
    // Pass recentPath so we can update the label when title loads
    loadMetadata(videoDirCid, tree, recentPath);

    // Load playlist if this is a playlist video
    if (capturedIsPlaylistVideo && capturedVideoId) {
      loadPlaylistForVideo(rootCidParam, capturedNpub, capturedTreeName, capturedVideoId);
    }
  }

  /** Load playlist from parent directory */
  async function loadPlaylistForVideo(
    playlistRootCid: CID,
    playlistNpub: string,
    playlistTreeName: string,
    videoId: string
  ) {
    console.log('[VideoView] Loading playlist for video:', videoId, 'from', playlistTreeName);

    // Load the playlist using the already-resolved root CID (don't resolve again)
    const result = await loadPlaylist(playlistNpub, playlistTreeName, playlistRootCid, videoId);

    if (result) {
      console.log('[VideoView] Loaded playlist with', result.items.length, 'videos');
    }
  }

  /** Load title and description in background */
  async function loadMetadata(rootCid: CID, tree: ReturnType<typeof getTree>, recentPath?: string) {
    // Try video file's link entry meta first (new format)
    try {
      const entries = await tree.listDirectory(rootCid);
      const videoEntry = entries?.find(e =>
        e.name.startsWith('video.') ||
        e.name.endsWith('.webm') ||
        e.name.endsWith('.mp4') ||
        e.name.endsWith('.mov')
      );
      if (videoEntry?.meta) {
        const meta = videoEntry.meta as Record<string, unknown>;
        if (meta.title && typeof meta.title === 'string') {
          videoTitle = meta.title;
          if (recentPath) updateRecentLabel(recentPath, videoTitle);
        }
        if (meta.description && typeof meta.description === 'string') {
          videoDescription = meta.description;
        }
        if (meta.createdAt && typeof meta.createdAt === 'number') {
          videoCreatedAt = meta.createdAt;
        }
        if (videoTitle && videoDescription) return; // Found both in link meta, done
      }
    } catch {}

    // Fall back to metadata.json (legacy format - will be migrated on login)
    try {
      const metadataResult = await tree.resolvePath(rootCid, 'metadata.json');
      if (metadataResult) {
        const metadataData = await tree.readFile(metadataResult.cid);
        if (metadataData) {
          const metadata = JSON.parse(new TextDecoder().decode(metadataData));
          if (metadata.title && typeof metadata.title === 'string') {
            videoTitle = metadata.title;
            if (recentPath) updateRecentLabel(recentPath, videoTitle);
          }
          if (metadata.description && typeof metadata.description === 'string') {
            videoDescription = metadata.description;
          }
          if (!videoCreatedAt && metadata.createdAt && typeof metadata.createdAt === 'number') {
            videoCreatedAt = metadata.createdAt;
          }
        }
      }
    } catch {}

    // Fall back to title.txt (legacy format)
    if (!videoTitle) {
      try {
        const titleResult = await tree.resolvePath(rootCid, 'title.txt');
        if (titleResult) {
          const titleData = await tree.readFile(titleResult.cid);
          if (titleData) {
            videoTitle = new TextDecoder().decode(titleData).trim();
            if (recentPath) updateRecentLabel(recentPath, videoTitle);
          }
        }
      } catch {}
    }

    // Fall back to description.txt (legacy format)
    if (!videoDescription) {
      try {
        const descResult = await tree.resolvePath(rootCid, 'description.txt');
        if (descResult) {
          const descData = await tree.readFile(descResult.cid);
          if (descData) {
            videoDescription = new TextDecoder().decode(descData).trim();
          }
        }
      } catch {}
    }
  }

  function handlePermalink() {
    if (!videoNhash) return;
    // Navigate to the nhash permalink (video file CID)
    window.location.hash = `#/${videoNhash}`;
  }

  function handleDownload() {
    if (!videoCid || !videoFileName) return;
    // Navigate to SW URL with ?download=1 query param
    // SW will serve with Content-Disposition: attachment header for streaming download
    const baseUrl = getNhashFileUrl(videoCid, videoFileName);
    const separator = baseUrl.includes('?') ? '&' : '?';
    window.location.href = `${baseUrl}${separator}download=1`;
  }

  function handleBlossomPush() {
    if (!rootCid) return;
    const pubkey = npub ? npubToPubkey(npub) : undefined;
    openBlossomPushModal(rootCid, title, true, pubkey, treeName);
  }

  function handleSaveToPlaylist() {
    // Use videoFolderCid which is the video folder (contains video.mp4, title.txt, etc.)
    // For single videos: videoFolderCid = rootCid (the video tree root)
    // For playlist videos: videoFolderCid = the specific video subfolder
    const cidToSave = videoFolderCid || rootCid;
    if (!cidToSave) return;
    // Estimate size (we don't have exact size, but it's not critical)
    openAddToPlaylistModal({ videoCid: cidToSave, videoTitle: title, videoSize: 0 });
  }

  async function handleDelete() {
    if (!treeName || deleting) return;
    if (!confirm(`Delete "${title}"? This cannot be undone.`)) return;

    deleting = true;
    try {
      if (isPlaylistVideo && currentVideoId && rootCid) {
        // Delete only this video from the playlist (not the whole playlist)
        await deletePlaylistVideo();
      } else {
        // Delete the entire tree (single video)
        if (npub && treeName) {
          recordDeletedVideo(npub, treeName, Math.floor(Date.now() / 1000) + 1);
          removeRecentByTreeName(npub, treeName);
        }
        await deleteTree(treeName);
        window.location.hash = '#/';
      }
    } catch (e) {
      console.error('Failed to delete video:', e);
      alert('Failed to delete video');
      deleting = false;
    }
  }

  /**
   * Delete a single video from a playlist without removing the whole playlist
   */
  async function deletePlaylistVideo() {
    if (!npub || !treeName || !currentVideoId || !rootCid) return;

    const tree = getTree();

    // Get the current playlist root CID (the parent directory)
    const { getLocalRootCache, getLocalRootKey } = await import('../../treeRootCache');
    const playlistRootHash = getLocalRootCache(npub, treeName);
    if (!playlistRootHash) {
      throw new Error('Playlist root not found');
    }

    const playlistRootKey = getLocalRootKey(npub, treeName);
    const playlistCid = playlistRootKey
      ? { hash: playlistRootHash, key: playlistRootKey }
      : { hash: playlistRootHash };

    // Remove the video entry from the playlist
    const newPlaylistCid = await tree.removeEntry(playlistCid, [], currentVideoId);

    // Check how many videos remain (directories containing videos)
    const remainingEntries = await tree.listDirectory(newPlaylistCid);
    // Filter for directories - type can be LinkType.Dir (2) or check by inspecting contents
    const remainingVideos: typeof remainingEntries = [];
    for (const entry of remainingEntries) {
      try {
        // Try to list as directory - if it works, it's a directory
        const subEntries = await tree.listDirectory(entry.cid);
        const hasVideo = subEntries?.some(e =>
          e.name.startsWith('video.') ||
          e.name.endsWith('.mp4') ||
          e.name.endsWith('.webm') ||
          e.name.endsWith('.mkv')
        );
        if (hasVideo) {
          remainingVideos.push(entry);
        }
      } catch {
        // Not a directory, skip
      }
    }

    if (remainingVideos.length === 0) {
      // No videos left - delete the whole playlist
      if (npub && treeName) {
        recordDeletedVideo(npub, treeName, Math.floor(Date.now() / 1000) + 1);
        removeRecentByTreeName(npub, treeName);
      }
      await deleteTree(treeName);
      window.location.hash = '#/';
    } else {
      // Update the playlist root with the new CID
      updateLocalRootCacheHex(
        npub,
        treeName,
        toHex(newPlaylistCid.hash),
        newPlaylistCid.key ? toHex(newPlaylistCid.key) : undefined,
        videoVisibility
      );

      // Clear the current playlist from store to force reload
      const { clearPlaylist } = await import('../../stores/playlist');
      clearPlaylist();

      // Navigate to the next video in the playlist
      const nextVideoId = remainingVideos[0].name;
      window.location.hash = `#/${npub}/${encodeURIComponent(treeName)}/${encodeURIComponent(nextVideoId)}`;
    }
  }

  function startEdit() {
    editTitle = videoTitle || videoName || '';
    editDescription = videoDescription || '';
    editing = true;
  }

  function cancelEdit() {
    editing = false;
    editTitle = '';
    editDescription = '';
  }

  async function saveEdit() {
    if (!npub || !treeName || saving) return;
    if (!editTitle.trim()) {
      alert('Title is required');
      return;
    }

    saving = true;
    try {
      let currentRootCid = rootCid;
      if (!currentRootCid) throw new Error('Video not found');

      const tree = getTree();

      // Find video file entry
      const entries = await tree.listDirectory(currentRootCid);
      const videoEntry = entries?.find(e =>
        e.name.startsWith('video.') ||
        e.name.endsWith('.webm') ||
        e.name.endsWith('.mp4') ||
        e.name.endsWith('.mov')
      );
      if (!videoEntry) throw new Error('Video file not found');

      // Get existing metadata from link entry or legacy metadata.json
      let existingMeta: Record<string, unknown> = { ...(videoEntry.meta || {}) };

      // If no createdAt in link meta, try to get it from metadata.json
      if (!existingMeta.createdAt) {
        try {
          const metadataResult = await tree.resolvePath(currentRootCid, 'metadata.json');
          if (metadataResult) {
            const metadataData = await tree.readFile(metadataResult.cid);
            if (metadataData) {
              const legacyMeta = JSON.parse(new TextDecoder().decode(metadataData));
              if (legacyMeta.createdAt) existingMeta.createdAt = legacyMeta.createdAt;
              if (legacyMeta.originalDate) existingMeta.originalDate = legacyMeta.originalDate;
              if (legacyMeta.duration) existingMeta.duration = legacyMeta.duration;
            }
          }
        } catch {}
      }

      // Update title/description
      existingMeta.title = editTitle.trim();
      if (editDescription.trim()) {
        existingMeta.description = editDescription.trim();
      } else {
        delete existingMeta.description;
      }

      // Set createdAt if still missing
      if (!existingMeta.createdAt) {
        existingMeta.createdAt = Math.floor(Date.now() / 1000);
      }

      // Update video entry with new metadata
      currentRootCid = await tree.setEntry(
        currentRootCid,
        [],
        videoEntry.name,
        videoEntry.cid,
        videoEntry.size,
        videoEntry.type,
        existingMeta
      );

      // Clean up legacy metadata files
      try { currentRootCid = await tree.removeEntry(currentRootCid, [], 'metadata.json'); } catch {}
      try { currentRootCid = await tree.removeEntry(currentRootCid, [], 'title.txt'); } catch {}
      try { currentRootCid = await tree.removeEntry(currentRootCid, [], 'description.txt'); } catch {}

      // Save and publish
      updateLocalRootCacheHex(
        npub,
        treeName,
        toHex(currentRootCid.hash),
        currentRootCid.key ? toHex(currentRootCid.key) : undefined,
        videoVisibility
      );

      // Update local state
      videoTitle = editTitle.trim();
      videoDescription = editDescription.trim();
      editing = false;
    } catch (e) {
      console.error('Failed to save:', e);
      alert('Failed to save changes');
    } finally {
      saving = false;
    }
  }


  // Video identifier for reactions (npub/treeName format - path to video directory)
  // For playlist videos, include the videoId to target the specific video, not the whole playlist
  let videoIdentifier = $derived.by(() => {
    if (!npub || !treeName) return null;
    // For playlist videos, include the video folder ID in the identifier
    if (isPlaylistVideo && currentVideoId) {
      return `${npub}/${treeName}/${currentVideoId}`;
    }
    return `${npub}/${treeName}`;
  });

  // Subscribe to likes for this video
  $effect(() => {
    const identifier = videoIdentifier;
    const currentUserPubkey = userPubkey; // Capture for callback
    if (!identifier) return;

    // Reset state
    untrack(() => {
      likes.clear();
      userLiked = false;
    });

    // Subscribe to kind 17 reactions with our identifier
    const filter: NDKFilter = {
      kinds: [17 as number],
      '#i': [identifier],
    };

    const sub = ndk.subscribe(filter, { closeOnEose: false });

    sub.on('event', (event: NDKEvent) => {
      if (!event.pubkey) return;

      // Check if it's a like (+ or empty content)
      const content = event.content?.trim() || '+';
      if (content === '+' || content === '') {
        untrack(() => {
          likes.add(event.pubkey);

          // Check if current user liked
          if (event.pubkey === currentUserPubkey) {
            userLiked = true;
          }
        });
      }
    });

    return () => {
      sub.stop();
    };
  });

  // Subscribe to comment count for mobile toggle
  $effect(() => {
    const identifier = videoIdentifier;
    if (!identifier) return;

    untrack(() => {
      commentCount = 0;
    });

    const seenIds = new SvelteSet<string>();
    const filter: NDKFilter = {
      kinds: [1111 as number],
      '#i': [identifier],
    };

    const sub = ndk.subscribe(filter, { closeOnEose: false });

    sub.on('event', (event: NDKEvent) => {
      if (!event.id || seenIds.has(event.id)) return;
      seenIds.add(event.id);
      untrack(() => {
        commentCount = seenIds.size;
      });
    });

    return () => {
      sub.stop();
    };
  });

  // Extract dominant color from thumbnail
  $effect(() => {
    const currentNpub = npub;
    const currentTreeName = treeName;
    const videoId = currentVideoId;
    void htreePrefixVersion;
    if (!currentNpub || !currentTreeName) return;

    // Reset color for new video
    untrack(() => {
      thumbnailColor = null;
    });

    const thumbUrl = getThumbnailUrl(currentNpub, currentTreeName, videoId || undefined);
    const img = new Image();
    img.crossOrigin = 'anonymous';

    img.onload = () => {
      // Extract color using canvas
      const canvas = document.createElement('canvas');
      const ctx = canvas.getContext('2d', { willReadFrequently: true });
      if (!ctx) return;

      const w = 32;
      const h = 18;
      canvas.width = w;
      canvas.height = h;

      try {
        ctx.drawImage(img, 0, 0, w, h);
        const imageData = ctx.getImageData(0, 0, w, h);
        const data = imageData.data;

        let r = 0, g = 0, b = 0, count = 0;

        for (let i = 0; i < data.length; i += 16) {
          const pr = data[i];
          const pg = data[i + 1];
          const pb = data[i + 2];

          const max = Math.max(pr, pg, pb);
          const min = Math.min(pr, pg, pb);
          const lightness = (max + min) / 2;

          if (lightness > 20 && lightness < 235) {
            const saturation = max === 0 ? 0 : (max - min) / max;
            const weight = 1 + saturation * 2;

            r += pr * weight;
            g += pg * weight;
            b += pb * weight;
            count += weight;
          }
        }

        if (count > 0) {
          thumbnailColor = {
            r: Math.round(r / count),
            g: Math.round(g / count),
            b: Math.round(b / count)
          };
        }
      } catch {
        // CORS or other error
      }
    };

    img.src = thumbUrl;
  });

  // Toggle like
  async function toggleLike() {
    if (!videoIdentifier || !isLoggedIn || liking) return;

    liking = true;
    try {
      const event = new NDKEvent(ndk);
      event.kind = 17; // External content reaction
      event.content = userLiked ? '' : '+'; // Toggle (note: can't really "unlike" in Nostr, but we track locally)

      // Build tags - include both npub path and nhash for discoverability
      const tags: string[][] = [
        ['i', videoIdentifier],
        ['k', 'video'],
      ];

      // Add nhash identifier for permalink reactions (uses video file CID, not directory)
      // Plain nhash is sufficient since it points directly to the file content
      if (videoNhash) {
        tags.push(['i', videoNhash]);
      }

      // Add p tag if we know the owner
      if (ownerPubkey) {
        tags.push(['p', ownerPubkey]);
      }

      event.tags = tags;

      await event.sign();
      await event.publish();

      // Update local state optimistically
      if (!userLiked) {
        likes.add(userPubkey!);
        userLiked = true;
      }
    } catch (e) {
      console.error('Failed to like video:', e);
    } finally {
      liking = false;
    }
  }

  // Ambient glow around video
  let ambient = $derived($ambientColor);
  let glowStyle = $derived.by(() => {
    if (!ambient) return '';
    const { r, g, b } = ambient;
    return `rgb(${r}, ${g}, ${b})`;
  });

  // Transform thumbnail color: boost saturation and brighten
  function getHighlightColor(color: { r: number; g: number; b: number }): { r: number; g: number; b: number } {
    let { r, g, b } = color;
    const max = Math.max(r, g, b);
    const min = Math.min(r, g, b);
    if (max > min) {
      const avg = (r + g + b) / 3;
      const boost = 2.0;
      r = Math.round(avg + (r - avg) * boost);
      g = Math.round(avg + (g - avg) * boost);
      b = Math.round(avg + (b - avg) * boost);
    }
    const brighten = 1.3;
    return {
      r: Math.min(255, Math.max(0, Math.round(r * brighten))),
      g: Math.min(255, Math.max(0, Math.round(g * brighten))),
      b: Math.min(255, Math.max(0, Math.round(b * brighten)))
    };
  }

  // Highlight color derived from thumbnail
  let highlightRgba = $derived.by(() => {
    if (!thumbnailColor) return null;
    const { r, g, b } = getHighlightColor(thumbnailColor);
    return `rgba(${r}, ${g}, ${b}, 0.2)`;
  });

  // Description box hover color
  let descriptionHoverStyle = $derived(highlightRgba ? `--desc-hover-color: ${highlightRgba};` : '');

  // Playlist active item background style
  let playlistActiveStyle = $derived(highlightRgba ? `background-color: ${highlightRgba};` : '');

  // Track video container position for glow
  let videoContainer: HTMLDivElement | undefined = $state();
  let glowRect = $state({ top: 0, left: 0, width: 0, height: 0 });

  $effect(() => {
    if (!videoContainer) return;

    function updateRect() {
      if (!videoContainer) return;
      const rect = videoContainer.getBoundingClientRect();
      glowRect = {
        top: rect.top,
        left: rect.left,
        width: rect.width,
        height: rect.height
      };
    }

    updateRect();
    window.addEventListener('resize', updateRect);
    window.addEventListener('scroll', updateRect, true);

    return () => {
      window.removeEventListener('resize', updateRect);
      window.removeEventListener('scroll', updateRect, true);
    };
  });
</script>

<AmbientGlow {videoRef} />

{#snippet videoContent()}
  <!-- Video Info -->
  <div class="mb-6">
    {#if editing}
      <!-- Edit form -->
      <div class="space-y-4">
        <div>
          <label class="block text-sm text-text-2 mb-1">Title</label>
          <input
            type="text"
            bind:value={editTitle}
            class="w-full bg-surface-0 border border-surface-3 rounded-lg p-3 text-text-1 focus:border-accent focus:outline-none"
            placeholder="Video title"
            disabled={saving}
          />
        </div>
        <div>
          <label class="block text-sm text-text-2 mb-1">Description</label>
          <textarea
            bind:value={editDescription}
            class="textarea w-full resize-none"
            placeholder="Video description..."
            rows="3"
            disabled={saving}
          ></textarea>
        </div>
        <div class="flex gap-2">
          <button onclick={saveEdit} class="btn-primary px-4 py-2" disabled={saving || !editTitle.trim()}>
            {saving ? 'Saving...' : 'Save'}
          </button>
          <button onclick={cancelEdit} class="btn-ghost px-4 py-2" disabled={saving}>
            Cancel
          </button>
        </div>
      </div>
    {:else}
      <!-- Title row -->
      <div class="flex items-center gap-2 mb-3">
        {#if effectiveVisibility !== 'public'}
          <VisibilityIcon visibility={effectiveVisibility} class="text-base text-text-3 mr-1" />
        {/if}
        <h1 class="text-xl font-semibold text-text-1 break-words min-w-0" data-testid="video-title">{title}</h1>
      </div>

      <!-- Owner info + action buttons row -->
      <div class="flex flex-wrap items-center justify-between gap-3 mb-4">
        <!-- Owner info -->
        <div class="flex items-center gap-3 min-w-0">
          {#if ownerPubkey}
            <a href={`#/${npub}`} class="shrink-0">
              <Avatar pubkey={ownerPubkey} size={40} />
            </a>
            <div class="min-w-0">
              <a href={`#/${npub}`} class="text-text-1 font-medium no-underline">
                <Name pubkey={ownerPubkey} />
              </a>
              <div class="text-sm text-text-3">
                {knownFollowers.size} known follower{knownFollowers.size !== 1 ? 's' : ''}
              </div>
            </div>
            <FollowButton pubkey={ownerPubkey} />
            {#if videoIdentifier}
              <VideoZapButton {videoIdentifier} {ownerPubkey} />
            {/if}
          {/if}
        </div>

        <!-- Action buttons -->
        <div class="flex items-center gap-1 shrink-0 flex-wrap">
          <button onclick={toggleTheaterMode} class="btn-ghost p-2 {theaterMode ? '' : 'hidden lg:block'}" title={theaterMode ? 'Exit theater mode' : 'Theater mode'}>
            <span class={theaterMode ? 'i-lucide-columns-2' : 'i-lucide-rectangle-horizontal'} class:text-lg={true}></span>
          </button>
          <!-- Like button -->
          {#if videoIdentifier}
            <button
              onclick={toggleLike}
              class="btn-ghost p-2 flex items-center gap-1"
              class:text-accent={userLiked}
              title={userLiked ? 'Liked' : 'Like'}
              disabled={!isLoggedIn || liking}
            >
              <span class={userLiked ? 'i-lucide-heart text-lg' : 'i-lucide-heart text-lg'} class:fill-current={userLiked}></span>
              {#if likes.size > 0}
                <span class="text-sm">{likes.size}</span>
              {/if}
            </button>
          {/if}
          <!-- Save to playlist button -->
          {#if isLoggedIn}
            <button
              onclick={handleSaveToPlaylist}
              class="btn-ghost p-2"
              title="Add to playlist"
              disabled={!rootCid}
            >
              <span class="i-lucide-bookmark text-lg"></span>
            </button>
          {/if}
          <ShareButton url={window.location.href} />
          <button onclick={handlePermalink} class="btn-ghost p-2" title="Permalink (content-addressed)" disabled={!videoNhash}>
            <span class="i-lucide-link text-lg"></span>
          </button>
          <button onclick={handleDownload} class="btn-ghost p-2" title="Download" disabled={!videoCid}>
            <span class="i-lucide-download text-lg"></span>
          </button>
          {#if isOwner}
            <button onclick={handleBlossomPush} class="btn-ghost p-2" title="Push to file servers">
              <span class="i-lucide-upload-cloud text-lg"></span>
            </button>
            <button onclick={startEdit} class="btn-ghost p-2" title="Edit">
              <span class="i-lucide-pencil text-lg"></span>
            </button>
            <button
              onclick={handleDelete}
              class="btn-ghost p-2 text-red-400 hover:text-red-300"
              title="Delete video"
              disabled={deleting}
            >
              <span class={deleting ? 'i-lucide-loader-2 animate-spin' : 'i-lucide-trash-2'} class:text-lg={true}></span>
            </button>
          {/if}
        </div>
      </div>

      <!-- Description with timestamp -->
      {#if videoDescription || videoCreatedAt}
        <VideoDescription
          text={videoDescription || ''}
          maxLines={4}
          maxChars={400}
          class="bg-surface-1 text-text-1 text-sm"
          style={descriptionHoverStyle}
          timestamp={videoCreatedAt ? formatTimeAgo(videoCreatedAt) : undefined}
        />
      {/if}
    {/if}
  </div>

  <!-- Comments (toggleable on mobile, always visible on desktop) -->
  {#if npub && treeName}
    <!-- Mobile toggle header -->
    <button
      class="lg:hidden w-full flex items-center justify-between py-3 border-t border-surface-3 text-text-1"
      onclick={() => mobileCommentsOpen = !mobileCommentsOpen}
    >
      <span class="text-lg font-semibold">Comments ({commentCount})</span>
      <span class={mobileCommentsOpen ? 'i-lucide-chevron-down' : 'i-lucide-chevron-right'} class:text-xl={true}></span>
    </button>
    <!-- Desktop: always show, Mobile: toggle -->
    <div class={mobileCommentsOpen ? '' : 'hidden lg:block'}>
      {#key `${npub}/${treeName}/${currentVideoId || ''}`}
        <VideoComments {npub} {treeName} nhash={videoNhash} filename={videoFileName} />
      {/key}
    </div>
  {/if}
{/snippet}

{#snippet sidebar()}
  {#if playlist && showPlaylistSidebar && playlist.items.length > 1}
    <div class="h-[600px] overflow-y-auto border border-text-3 rounded-lg mb-4">
      <PlaylistSidebar activeStyle={playlistActiveStyle} />
    </div>
  {/if}
  <FeedSidebar currentHref={`#/${npub}/${treeName ? encodeURIComponent(treeName) : ''}`} />
{/snippet}

<!--
  Layout uses CSS-only approach to avoid remounting video element.
  Theater mode: video full width, content+sidebar row below (max-w-6xl)
  Non-theater: constrained container (max-w-7xl), sidebar beside video+content
-->
<div class="flex-1 overflow-y-auto pb-4">
  <div class="{theaterMode ? '' : 'flex max-w-7xl mx-auto'}">
    <!-- Main column: video + content -->
    <div class="flex-1 min-w-0 {theaterMode ? '' : 'lg:px-4 lg:pt-3'}">
      <!-- Video Player (never remounts) -->
      <div class="relative">
        <div
          class="fixed pointer-events-none"
          style="
            top: {glowRect.top - 40}px;
            left: {glowRect.left - 40}px;
            width: {glowRect.width + 80}px;
            height: {glowRect.height + 80}px;
            background-color: {glowStyle || 'transparent'};
            filter: blur(60px);
            opacity: 0.25;
            transition: background-color 5s ease-out;
            z-index: 0;
          "
        ></div>
        <div
          bind:this={videoContainer}
          class="w-full mx-auto aspect-video max-h-[calc(100vh-180px)] relative z-10 {theaterMode ? '' : 'lg:rounded-xl lg:overflow-hidden'}"
          data-video-src={videoSrc}
          data-video-filename={videoFileName}
          data-htree-prefix={htreePrefix}
          data-video-load-runs={loadEffectRuns}
          data-video-key={loadedVideoKey ?? ''}
          data-video-root-hash={rootCid ? toHex(rootCid.hash).slice(0, 16) : ''}
          data-video-npub={npub ?? ''}
          data-video-tree-name={treeName ?? ''}
        >
          {#if error}
            <div class="w-full h-full flex items-center justify-center text-red-400">
              <span class="i-lucide-alert-circle mr-2"></span>
              {error}
            </div>
          {:else if videoSrc}
            <video
              bind:this={videoRef}
              use:applyVolumeSettings
              src={videoSrc}
              controls
              autoplay
              playsinline
              muted={initialVideoSettings.muted}
              class="w-full h-full"
              preload="metadata"
              onloadedmetadata={handleLoadedMetadata}
              ontimeupdate={handleTimeUpdate}
              onvolumechange={handleVolumeChange}
              onerror={handleVideoError}
              onended={handleEnded}
            >
              Your browser does not support the video tag.
            </video>
          {/if}
        </div>
      </div>

      <!-- Content below video wrapper - changes based on theater mode -->
      <div class="{theaterMode ? 'flex max-w-6xl mx-auto' : ''}">
        <div class="flex-1 min-w-0 px-4 py-4">
          {@render videoContent()}
        </div>
        <!-- Desktop sidebar (theater mode: beside content) -->
        {#if theaterMode}
          <div class="w-96 shrink-0 hidden lg:block overflow-y-auto pt-4">
            {@render sidebar()}
          </div>
        {/if}
      </div>
    </div>

    <!-- Desktop sidebar (non-theater: beside everything including video) -->
    {#if !theaterMode}
      <div class="w-96 shrink-0 hidden lg:block overflow-y-auto py-3">
        {@render sidebar()}
      </div>
    {/if}
  </div>

  <!-- Mobile sidebar (always below content) - show both playlist and feed -->
  <div class="lg:hidden pb-4">
    {#if playlist && showPlaylistSidebar && playlist.items.length > 1}
      <div class="h-[600px] overflow-y-auto border border-text-3 rounded-lg mb-4">
        <PlaylistSidebar activeStyle={playlistActiveStyle} />
      </div>
    {/if}
    <FeedSidebar currentHref={`#/${npub}/${treeName ? encodeURIComponent(treeName) : ''}`} />
  </div>
</div>
