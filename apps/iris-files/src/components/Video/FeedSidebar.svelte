<script lang="ts">
  /**
   * FeedSidebar - Shows feed videos in sidebar
   * Similar to YouTube's "Up next" / related videos
   */
  import { onMount } from 'svelte';
  import { feedStore, fetchFeedVideos } from '../../stores/feedStore';
  import { toHex } from 'hashtree';
  import { formatTimeAgo } from '../../utils/format';
  import { Name } from '../User';
  import { getThumbnailUrl, onHtreePrefixReady } from '../../lib/mediaUrl';
  import { logHtreeDebug } from '../../lib/htreeDebug';
  import { nostrStore } from '../../nostr';
  import { recentsStore, positionCacheVersion, getVideoPosition } from '../../stores/recents';
  import VideoThumbnail from './VideoThumbnail.svelte';
  import { SvelteMap } from 'svelte/reactivity';


  interface Props {
    /** Current video href to exclude from feed */
    currentHref?: string;
  }

  let { currentHref }: Props = $props();

  let feedVideos = $derived($feedStore);
  let pubkey = $derived($nostrStore.pubkey);
  let htreePrefixVersion = $state(0);
  onHtreePrefixReady(() => {
    htreePrefixVersion += 1;
  });

  const ensureFeed = () => {
    if ($feedStore.length === 0) {
      logHtreeDebug('feedSidebar:fetch', { pubkey });
      fetchFeedVideos();
    }
  };

  // Fetch feed videos on mount (fetch handles bootstrap fallback if user not ready).
  onMount(() => {
    ensureFeed();
  });

  $effect(() => {
    if (pubkey) {
      ensureFeed();
    }
  });

  // Filter out current video
  let displayVideos = $derived(
    feedVideos.filter(v => v.href !== currentHref)
  );

  // Build thumbnail URL for a video
  function buildThumbnailUrl(video: typeof feedVideos[0]): string | null {
    void htreePrefixVersion;
    if (!video.ownerNpub || !video.treeName) return null;
    const hashPrefix = video.rootCid?.hash ? toHex(video.rootCid.hash).slice(0, 8) : undefined;
    return getThumbnailUrl(video.ownerNpub, video.treeName, video.videoId || undefined, hashPrefix);
  }

  // Get recents to look up duration (feedStore doesn't have it)
  let recents = $derived($recentsStore);

  // Build a map of path -> duration from recents
  let recentsDurationMap = $derived.by(() => {
    const map = new SvelteMap<string, number>();
    for (const r of recents) {
      if (r.duration && r.duration > 0) {
        map.set(r.path, r.duration);
      }
    }
    return map;
  });

  // Helper to get duration for a video (from feedStore or recents)
  function getVideoDuration(video: typeof feedVideos[0]): number | undefined {
    if (video.duration) return video.duration;
    if (!video.ownerNpub || !video.treeName) return undefined;
    const path = video.videoId
      ? `/${video.ownerNpub}/${video.treeName}/${video.videoId}`
      : `/${video.ownerNpub}/${video.treeName}`;
    return recentsDurationMap.get(path);
  }

  // Compute progress for all videos reactively
  let videoProgress = $derived.by(() => {
    // Subscribe to position cache version for reactivity
    void $positionCacheVersion;
    const progressMap = new SvelteMap<string, number>();
    for (const video of displayVideos) {
      if (!video.ownerNpub || !video.treeName) continue;
      const path = video.videoId
        ? `/${video.ownerNpub}/${video.treeName}/${video.videoId}`
        : `/${video.ownerNpub}/${video.treeName}`;

      // Get duration from feedStore first, then fall back to recents
      const duration = video.duration || recentsDurationMap.get(path);
      if (!duration || duration <= 0) continue;

      const position = getVideoPosition(path);
      if (position <= 0) continue;
      const percent = Math.min((position / duration) * 100, 100);
      if (percent >= 1 && percent < 95) {
        progressMap.set(video.href, percent);
      }
    }
    return progressMap;
  });
</script>

<div class="px-4">
  {#if displayVideos.length > 0}
    <div class="space-y-2">
      {#each displayVideos as video (video.href)}
        {@const thumbnailUrl = buildThumbnailUrl(video)}
        {@const duration = getVideoDuration(video)}
        {@const progress = videoProgress.get(video.href) ?? 0}
        <a
          href={video.href}
          class="flex gap-2 group no-underline"
        >
          <!-- Thumbnail -->
          <VideoThumbnail
            src={thumbnailUrl}
            {duration}
            {progress}
            class="w-42 aspect-video shrink-0 rounded"
            iconSize="text-sm"
          />

          <!-- Info -->
          <div class="flex-1 min-w-0">
            <p class="text-sm text-text-1 line-clamp-2">
              {video.title}
            </p>
            {#if video.ownerPubkey}
              <span class="text-xs text-text-3 truncate block mt-0.5">
                <Name pubkey={video.ownerPubkey} />
              </span>
            {/if}
            {#if video.timestamp}
              <span class="text-xs text-text-3 block">
                {formatTimeAgo(video.timestamp)}
              </span>
            {/if}
          </div>
        </a>
      {/each}
    </div>
  {/if}
</div>
