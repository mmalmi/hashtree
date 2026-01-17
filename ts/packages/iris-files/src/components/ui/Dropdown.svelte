<script lang="ts">
  /**
   * Shared dropdown component with click-outside and ESC to close
   */
  import type { Snippet } from 'svelte';

  interface Props {
    open: boolean;
    onClose: () => void;
    trigger: Snippet;
    children: Snippet;
    align?: 'left' | 'right';
  }

  let { open = $bindable(), onClose, trigger, children, align = 'left' }: Props = $props();

  let containerRef: HTMLDivElement | undefined = $state();

  // Handle click outside
  function handleClickOutside(e: MouseEvent) {
    if (!open || !containerRef) return;
    if (!containerRef.contains(e.target as Node)) {
      onClose();
    }
  }

  // Handle ESC key
  function handleKeyDown(e: KeyboardEvent) {
    if (!open) return;
    if (e.key === 'Escape') {
      e.preventDefault();
      onClose();
    }
  }

  $effect(() => {
    if (open) {
      document.addEventListener('click', handleClickOutside, true);
      document.addEventListener('keydown', handleKeyDown);
      return () => {
        document.removeEventListener('click', handleClickOutside, true);
        document.removeEventListener('keydown', handleKeyDown);
      };
    }
  });
</script>

<div class="relative" bind:this={containerRef}>
  {@render trigger()}
  {#if open}
    <div class="absolute top-full mt-1 rounded-lg shadow-lg z-10 min-w-40 max-h-60 overflow-auto {align === 'right' ? 'right-0' : 'left-0'}">
      {@render children()}
    </div>
  {/if}
</div>
