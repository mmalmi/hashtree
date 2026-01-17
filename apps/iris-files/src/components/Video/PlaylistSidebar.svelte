<script lang="ts">
  /**
   * PlaylistSidebar - Shows playlist items with thumbnails
   * Desktop: sidebar on the right
   * Mobile: horizontal scroll below video
   */
  import { tick } from 'svelte';
  import {
    currentPlaylist,
    playAt,
    shuffleEnabled,
    repeatMode,
    toggleShuffle,
    cycleRepeatMode,
    type RepeatMode,
  } from '../../stores/playlist';
  import { formatDuration } from '../../utils/format';
  import { nostrStore, deleteTree } from '../../nostr';
  import { recentsStore, getVideoPosition } from '../../stores/recents';

  interface Props {
    mobile?: boolean;
    activeStyle?: string;
  }

  let { mobile = false, activeStyle = '' }: Props = $props();

  // Get watch progress for a playlist item
  function getWatchProgress(itemId: string, duration?: number): number {
    const playlist = $currentPlaylist;
    if (!playlist || !duration || duration <= 0) return 0;
    const path = `/${playlist.npub}/${playlist.treeName}/${itemId}`;
    // Subscribe to recents for reactivity
    void $recentsStore;
    const position = getVideoPosition(path);
    if (position <= 0) return 0;
    const percent = Math.min((position / duration) * 100, 100);
    // Only show if watched at least 1% but not finished (< 95%)
    return percent >= 1 && percent < 95 ? percent : 0;
  }

  let playlist = $derived($currentPlaylist);
  let shuffle = $derived($shuffleEnabled);
  let repeat = $derived($repeatMode);

  let currentUserNpub = $derived($nostrStore.npub);
  let isOwner = $derived(playlist && currentUserNpub === playlist.npub);
  let deleting = $state(false);

  async function handleDeletePlaylist() {
    if (!playlist) return;

    if (!confirm(`Delete playlist "${playlist.name}"? This will remove the entire playlist and all its videos.`)) {
      return;
    }

    deleting = true;
    try {
      await deleteTree(playlist.treeName);
      // Navigate to profile after deletion
      window.location.hash = `#/${playlist.npub}`;
    } catch (err) {
      console.error('Failed to delete playlist:', err);
      alert('Failed to delete playlist');
    } finally {
      deleting = false;
    }
  }

  // Action to scroll current item into view
  function scrollIfCurrent(node: HTMLElement, isCurrent: boolean) {
    if (isCurrent) {
      tick().then(() => {
        node.scrollIntoView({ block: 'nearest', inline: 'nearest' });
      });
    }
    return {
      update(newIsCurrent: boolean) {
        if (newIsCurrent) {
          tick().then(() => {
            node.scrollIntoView({ block: 'nearest', inline: 'nearest' });
          });
        }
      }
    };
  }

  function handleVideoClick(index: number) {
    const url = playAt(index);
    if (url) {
      window.location.hash = url;
    }
  }

  function getRepeatIcon(mode: RepeatMode): string {
    if (mode === 'one') return 'i-lucide-repeat-1';
    return 'i-lucide-repeat';
  }

  function getRepeatTitle(mode: RepeatMode): string {
    if (mode === 'none') return 'Repeat: Off';
    if (mode === 'all') return 'Repeat: All';
    return 'Repeat: One';
  }
</script>

{#if playlist && playlist.items.length > 0}
  {#if mobile}
    <!-- Mobile: Horizontal scroll below video -->
    <div class="rounded-lg overflow-hidden">
      <!-- Header -->
      <div class="p-2 flex items-center gap-2">
        <span class="i-lucide-list-video text-accent shrink-0"></span>
        <span class="font-medium text-text-1 truncate text-sm">{playlist.name}</span>
        <span class="text-xs text-text-3 shrink-0">({playlist.currentIndex + 1}/{playlist.items.length})</span>
        <div class="flex items-center gap-1 ml-auto shrink-0">
          <button
            onclick={cycleRepeatMode}
            class="btn-ghost p-1.5 {repeat !== 'none' ? 'text-accent' : 'text-text-3'}"
            title={getRepeatTitle(repeat)}
          >
            <span class="{getRepeatIcon(repeat)} text-sm"></span>
          </button>
          <button
            onclick={toggleShuffle}
            class="btn-ghost p-1.5 {shuffle ? 'text-accent' : 'text-text-3'}"
            title={shuffle ? 'Shuffle: On' : 'Shuffle: Off'}
          >
            <span class="i-lucide-shuffle text-sm"></span>
          </button>
          {#if isOwner}
            <button
              onclick={handleDeletePlaylist}
              disabled={deleting}
              class="btn-ghost p-1.5 text-text-3 hover:text-danger"
              title="Delete playlist"
            >
              {#if deleting}
                <span class="i-lucide-loader-2 animate-spin text-sm"></span>
              {:else}
                <span class="i-lucide-trash-2 text-sm"></span>
              {/if}
            </button>
          {/if}
        </div>
      </div>

      <!-- Horizontal scroll list -->
      <div class="flex gap-2 overflow-x-auto pb-2 px-2 scrollbar-hide">
        {#each playlist.items as item, i (item.id)}
          {@const isCurrent = i === playlist.currentIndex}
          {@const progress = getWatchProgress(item.id, item.duration)}
          <button
            use:scrollIfCurrent={isCurrent}
            onclick={() => handleVideoClick(i)}
            class="shrink-0 w-32 text-left rounded overflow-hidden {isCurrent ? 'ring-2 ring-accent' : ''}"
          >
            <!-- Thumbnail -->
            <div class="w-32 h-18 bg-surface-3 relative">
              {#if item.thumbnailUrl}
                <img
                  src={item.thumbnailUrl}
                  alt=""
                  class="w-full h-full object-cover"
                  loading="lazy"
                />
              {:else}
                <div class="w-full h-full flex items-center justify-center">
                  <span class="i-lucide-video text-text-3"></span>
                </div>
              {/if}
              {#if isCurrent}
                <div class="absolute inset-0 bg-black/40 flex items-center justify-center">
                  <span class="i-lucide-play-circle text-2xl text-white"></span>
                </div>
              {/if}
              {#if item.duration}
                <span class="absolute bottom-1 right-1 text-xs bg-black/80 text-white px-1 rounded">
                  {formatDuration(item.duration)}
                </span>
              {/if}
              {#if progress > 0}
                <div class="absolute bottom-0 left-0 right-0 h-1 bg-white/30">
                  <div class="h-full bg-danger" style="width: {progress}%"></div>
                </div>
              {/if}
            </div>
            <!-- Title -->
            <p class="text-xs text-text-1 line-clamp-2 p-1 {isCurrent ? 'text-accent' : ''}">{item.title}</p>
          </button>
        {/each}
      </div>
    </div>
  {:else}
    <!-- Desktop: Vertical sidebar -->
    <div class="rounded-lg overflow-hidden flex flex-col h-full">
      <!-- Header -->
      <div class="p-3 border-b border-surface-3">
        <div class="flex items-start justify-between gap-2">
          <div class="min-w-0">
            <h3 class="font-medium text-text-1 truncate">{playlist.name}</h3>
            <p class="text-xs text-text-3">{playlist.currentIndex + 1}/{playlist.items.length}</p>
          </div>
          {#if isOwner}
            <button
              onclick={handleDeletePlaylist}
              disabled={deleting}
              class="btn-ghost p-1.5 text-text-3 hover:text-danger shrink-0"
              title="Delete playlist"
            >
              {#if deleting}
                <span class="i-lucide-loader-2 animate-spin"></span>
              {:else}
                <span class="i-lucide-trash-2"></span>
              {/if}
            </button>
          {/if}
        </div>
      </div>

      <!-- Loop & Shuffle controls -->
        <div class="px-3 py-2 border-b border-surface-3 flex items-center gap-1">
          <button
            onclick={cycleRepeatMode}
            class="btn-ghost p-2 {repeat !== 'none' ? 'text-accent' : 'text-text-3'}"
            title={getRepeatTitle(repeat)}
          >
            <span class="{getRepeatIcon(repeat)} text-lg"></span>
          </button>
          <button
            onclick={toggleShuffle}
            class="btn-ghost p-2 {shuffle ? 'text-accent' : 'text-text-3'}"
            title={shuffle ? 'Shuffle: On' : 'Shuffle: Off'}
          >
            <span class="i-lucide-shuffle text-lg"></span>
          </button>
        </div>

        <!-- Video list -->
        <div class="flex-1 overflow-auto">
          {#each playlist.items as item, i (item.id)}
            {@const isCurrent = i === playlist.currentIndex}
            {@const progress = getWatchProgress(item.id, item.duration)}
            <button
              use:scrollIfCurrent={isCurrent}
              onclick={() => handleVideoClick(i)}
              class="w-full flex gap-2 p-2 text-left hover:bg-surface-2 transition-colors {isCurrent ? 'bg-surface-2' : ''}"
              style={isCurrent && activeStyle ? activeStyle : ''}
            >
              <!-- Index or playing indicator -->
              <div class="w-6 shrink-0 flex items-center justify-center text-xs text-text-3">
                {#if isCurrent}
                  <span class="i-lucide-play fill-current"></span>
                {:else}
                  {i + 1}
                {/if}
              </div>

              <!-- Thumbnail -->
              <div class="w-24 h-14 shrink-0 bg-surface-3 rounded overflow-hidden relative">
                {#if item.thumbnailUrl}
                  <img
                    src={item.thumbnailUrl}
                    alt=""
                    class="w-full h-full object-cover"
                    loading="lazy"
                  />
                {:else}
                  <div class="w-full h-full flex items-center justify-center">
                    <span class="i-lucide-video text-text-3"></span>
                  </div>
                {/if}
                {#if progress > 0}
                  <div class="absolute bottom-0 left-0 right-0 h-0.5 bg-white/30">
                    <div class="h-full bg-danger" style="width: {progress}%"></div>
                  </div>
                {/if}
              </div>

              <!-- Info -->
              <div class="flex-1 min-w-0">
                <p class="text-sm text-text-1 line-clamp-2 {isCurrent ? 'text-accent' : ''}">{item.title}</p>
                {#if item.duration}
                  <p class="text-xs text-text-3 mt-0.5">{formatDuration(item.duration)}</p>
                {/if}
              </div>
            </button>
          {/each}
        </div>
    </div>
  {/if}
{/if}

<style>
  .scrollbar-hide {
    -ms-overflow-style: none;
    scrollbar-width: none;
  }
  .scrollbar-hide::-webkit-scrollbar {
    display: none;
  }
</style>
