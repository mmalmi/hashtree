<script lang="ts">
  /**
   * VideoThumbnail - Reusable video thumbnail with duration and progress bar
   * Used by VideoCard, FeedSidebar, PlaylistSidebar, etc.
   */
  import { formatDuration } from '../../utils/format';

  interface Props {
    /** Thumbnail URL */
    src?: string | null;
    /** Video duration in seconds */
    duration?: number;
    /** Watch progress percentage (0-100) */
    progress?: number;
    /** Additional classes for the container */
    class?: string;
    /** Size of fallback icon (default: text-4xl) */
    iconSize?: string;
  }

  let { src, duration, progress = 0, class: className = '', iconSize = 'text-4xl' }: Props = $props();

  let imageError = $state(false);
  let lastSrc = $state<string | null>(null);

  // Reset error when src changes
  $effect.pre(() => {
    if (src && src !== lastSrc) {
      imageError = false;
      lastSrc = src;
    }
  });
</script>

<div class="relative bg-surface-2 overflow-hidden {className}">
  {#if src && !imageError}
    <img
      {src}
      alt=""
      class="absolute inset-0 w-full h-full object-cover"
      loading="lazy"
      onerror={() => imageError = true}
    />
  {:else}
    <div class="absolute inset-0 flex items-center justify-center">
      <span class="i-lucide-video text-text-3 {iconSize}"></span>
    </div>
  {/if}

  <!-- Duration label - positioned above progress bar -->
  {#if duration}
    <div class="absolute bottom-2 right-1 bg-black/80 text-white text-[10px] px-1 rounded z-10">
      {formatDuration(duration)}
    </div>
  {/if}

  <!-- Watch progress bar -->
  {#if progress > 0}
    <div class="absolute bottom-0 left-0 right-0 h-1 bg-white/30">
      <div class="h-full bg-danger" style="width: {progress}%"></div>
    </div>
  {/if}
</div>
