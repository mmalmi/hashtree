<script lang="ts">
  /**
   * VideoLayout - Shared layout for video pages
   * Used by VideoView and VideoNHashView
   */
  import type { Snippet } from 'svelte';
  import FeedSidebar from './FeedSidebar.svelte';

  interface Props {
    /** Snippet for video player area */
    videoPlayer: Snippet;
    /** Snippet for content below video (title, actions, description, comments) */
    videoContent: Snippet;
    /** Optional snippet for additional sidebar content (e.g., playlist) */
    sidebarExtra?: Snippet;
    /** Current video href to exclude from feed */
    currentHref: string;
  }

  let { videoPlayer, videoContent, sidebarExtra, currentHref }: Props = $props();
</script>

<div class="flex-1 overflow-y-auto pb-4">
  <div class="flex max-w-7xl mx-auto">
    <!-- Main column: video + content -->
    <div class="flex-1 min-w-0 lg:px-4 lg:pt-3">
      <!-- Video Player -->
      <div class="w-full mx-auto aspect-video max-h-[calc(100vh-180px)] lg:rounded-xl lg:overflow-hidden">
        {@render videoPlayer()}
      </div>

      <!-- Content below video -->
      <div class="px-4 py-4">
        {@render videoContent()}
      </div>
    </div>

    <!-- Desktop sidebar -->
    <div class="w-96 shrink-0 hidden lg:block overflow-y-auto py-3">
      {#if sidebarExtra}
        {@render sidebarExtra()}
      {/if}
      <FeedSidebar {currentHref} />
    </div>
  </div>

  <!-- Mobile sidebar (below content) -->
  <div class="lg:hidden px-4 pb-4">
    {#if sidebarExtra}
      {@render sidebarExtra()}
    {/if}
    <FeedSidebar {currentHref} />
  </div>
</div>
