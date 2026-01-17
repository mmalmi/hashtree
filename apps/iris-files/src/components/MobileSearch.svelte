<script lang="ts">
  import SearchInput from './SearchInput.svelte';

  interface Props {
    showVideos?: boolean;
  }

  let { showVideos = true }: Props = $props();

  let expanded = $state(false);
  let containerRef: HTMLDivElement | undefined = $state();

  // Close on click outside and escape key
  $effect(() => {
    if (!expanded) return;

    const handleClickOutside = (e: MouseEvent) => {
      if (containerRef && !containerRef.contains(e.target as Node)) {
        expanded = false;
      }
    };

    const handleKeyDown = (e: KeyboardEvent) => {
      if (e.key === 'Escape') {
        expanded = false;
      }
    };

    document.addEventListener('mousedown', handleClickOutside);
    document.addEventListener('keydown', handleKeyDown);
    return () => {
      document.removeEventListener('mousedown', handleClickOutside);
      document.removeEventListener('keydown', handleKeyDown);
    };
  });
</script>

<div bind:this={containerRef} class="md:hidden">
  {#if expanded}
    <!-- Expanded search overlay -->
    <div class="absolute left-0 right-0 top-0 h-14 bg-surface-0 flex items-center px-3 z-50">
      <div class="flex-1">
        <SearchInput fullWidth autofocus {showVideos} onSelect={() => (expanded = false)} />
      </div>
      <button
        onclick={() => (expanded = false)}
        class="p-2 text-text-2 hover:text-text-1 bg-transparent border-none cursor-pointer"
        aria-label="Close search"
      >
        <span class="i-lucide-x text-lg"></span>
      </button>
    </div>
  {:else}
    <!-- Search icon button -->
    <button
      onclick={() => (expanded = true)}
      class="p-2 text-text-2 hover:text-text-1 bg-transparent border-none cursor-pointer"
      aria-label="Search"
    >
      <span class="i-lucide-search text-lg"></span>
    </button>
  {/if}
</div>
