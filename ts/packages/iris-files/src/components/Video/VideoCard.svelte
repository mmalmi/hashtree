<script lang="ts">
  /**
   * VideoCard - 16:9 aspect ratio video card for video grid
   * SW handles finding the actual thumbnail file (jpg, webp, png, etc.)
   */
  import VisibilityIcon from '../VisibilityIcon.svelte';
  import { Avatar, Name } from '../User';
  import { getThumbnailUrl, onHtreePrefixReady } from '../../lib/mediaUrl';
  import { formatTimeAgo } from '../../utils/format';
  import { recentsStore, getVideoPosition } from '../../stores/recents';
  import { extractDominantColor, rgbToRgba, type RGB } from '../../utils/colorExtract';
  import VideoThumbnail from './VideoThumbnail.svelte';

  interface Props {
    href: string;
    title: string;
    duration?: number;
    ownerPubkey?: string | null;
    ownerNpub?: string | null;
    treeName?: string | null;
    /** For playlist videos: the video folder name within the playlist tree */
    videoId?: string | null;
    visibility?: string;
    /** Direct thumbnail URL (e.g., from nhash in metadata) */
    thumbnailUrl?: string | null;
    /** Root hash hex prefix for cache busting (updates trigger thumbnail retry) */
    rootHashHex?: string | null;
    /** Unix timestamp in seconds for "ago" display */
    timestamp?: number | null;
    /** Enable theme color hover effect (default false) */
    themeHover?: boolean;
    /** Disable hover effect entirely (default false) */
    noHover?: boolean;
    /** Hide author info (avatar + name) e.g., on profile pages where owner is already shown */
    hideAuthor?: boolean;
  }

  let { href, title, duration, ownerPubkey, ownerNpub, treeName, videoId, visibility, thumbnailUrl: propThumbnailUrl, rootHashHex, timestamp, themeHover = false, noHover = false, hideAuthor = false }: Props = $props();

  let htreePrefixVersion = $state(0);
  onHtreePrefixReady(() => {
    htreePrefixVersion += 1;
  });

  // Build thumbnail URL - use prop if provided, otherwise use unified thumbnail URL
  // Include hash prefix so URL changes when tree root updates, triggering retry for failed thumbnails
  let thumbnailUrl = $derived.by(() => {
    void htreePrefixVersion;
    if (propThumbnailUrl) return propThumbnailUrl;
    if (!ownerNpub || !treeName) return null;
    return getThumbnailUrl(ownerNpub, treeName, videoId || undefined, rootHashHex?.slice(0, 8) || undefined);
  });


  // Build path for recents lookup (matches how VideoView stores it)
  let recentPath = $derived.by(() => {
    if (!ownerNpub || !treeName) return null;
    if (videoId) {
      return `/${ownerNpub}/${treeName}/${videoId}`;
    }
    return `/${ownerNpub}/${treeName}`;
  });

  // Get watch progress from recents store
  let watchProgress = $derived.by(() => {
    if (!recentPath || !duration || duration <= 0) return 0;
    // Subscribe to recents store to get reactive updates
    void $recentsStore;
    const position = getVideoPosition(recentPath);
    if (position <= 0) return 0;
    // Calculate percentage, cap at 100%
    const percent = Math.min((position / duration) * 100, 100);
    // Only show if watched at least 1% but not finished (< 95%)
    return percent >= 1 && percent < 95 ? percent : 0;
  });

  // Extract dominant color from thumbnail for hover effect (only when enabled)
  let themeColor = $state<RGB | null>(null);

  $effect(() => {
    if (!themeHover) return;
    const url = thumbnailUrl;
    if (!url) return;

    themeColor = null;
    extractDominantColor(url).then(color => {
      themeColor = color;
    });
  });

  let hoverStyle = $derived(themeHover && themeColor ? `--hover-color: ${rgbToRgba(themeColor, 0.15)};` : '');

</script>

<a
  {href}
  class="relative rounded-lg transition-all duration-200 no-underline flex flex-col overflow-visible group isolate {noHover ? '' : 'video-card'}"
  style={hoverStyle}
>
  <!-- Thumbnail with 16:9 aspect ratio -->
  <VideoThumbnail
    src={thumbnailUrl}
    {duration}
    progress={watchProgress}
    class="video-thumb aspect-video rounded-lg z-10"
  />

  <!-- Info - compact like YouTube -->
  <div class="pt-2 pb-1 flex gap-2 relative z-10">
    {#if ownerPubkey && !hideAuthor}
      <div class="shrink-0">
        <Avatar pubkey={ownerPubkey} size={36} />
      </div>
    {/if}
    <div class="min-w-0 flex-1">
      <div class="flex items-center gap-3">
        <VisibilityIcon {visibility} class="text-base text-text-3 shrink-0" />
        <h3 class="text-base font-medium text-text-1 line-clamp-2 leading-tight">{title}</h3>
      </div>
      <div class="flex items-center gap-1 text-sm text-text-3 mt-0.5">
        {#if ownerPubkey && !hideAuthor}
          <Name pubkey={ownerPubkey} />
        {/if}
        {#if timestamp}
          {#if ownerPubkey && !hideAuthor}<span class="opacity-70">·</span>{/if}
          <span class="opacity-70">{formatTimeAgo(timestamp)}</span>
        {/if}
      </div>
    </div>
  </div>
</a>

<style>
  .video-card::before {
    content: '';
    position: absolute;
    inset: 0;
    border-radius: 12px;
    background: transparent;
    transition: all 0.3s ease-out;
    pointer-events: none;
    z-index: -1;
  }

  .video-card:hover::before {
    inset: -12px;
    background: var(--hover-color, rgba(255, 255, 255, 0.08));
  }

  .video-thumb {
    transition: border-radius 0.3s ease-out;
  }

  .video-card:hover .video-thumb {
    border-radius: 0;
  }
</style>
