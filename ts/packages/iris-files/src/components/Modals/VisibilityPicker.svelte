<script lang="ts">
  /**
   * Visibility picker for selecting tree visibility (public/link-visible/private)
   */
  import type { TreeVisibility } from 'hashtree';

  interface Props {
    value: TreeVisibility;
    onchange: (value: TreeVisibility) => void;
  }

  let { value, onchange }: Props = $props();

  function getVisibilityTitle(vis: TreeVisibility): string {
    switch (vis) {
      case 'public': return 'Anyone can browse this folder';
      case 'link-visible': return 'Only accessible with a special link';
      case 'private': return 'Only you can access this folder';
    }
  }

  function getVisibilityIcon(vis: TreeVisibility): string {
    switch (vis) {
      case 'public': return 'i-lucide-globe';
      case 'link-visible': return 'i-lucide-link';
      case 'private': return 'i-lucide-lock';
    }
  }
</script>

<div>
  <span class="text-sm text-text-2 mb-2 block">Visibility</span>
  <div class="flex gap-2">
    {#each ['public', 'link-visible', 'private'] as vis (vis)}
      <button
        type="button"
        onclick={() => onchange(vis as TreeVisibility)}
        class="flex-1 flex items-center justify-center gap-2 py-2 px-3 btn-ghost {value === vis
          ? 'ring-2 ring-accent bg-surface-3'
          : ''}"
        title={getVisibilityTitle(vis as TreeVisibility)}
      >
        <span class={getVisibilityIcon(vis as TreeVisibility)}></span>
        <span class="text-sm capitalize">{vis}</span>
      </button>
    {/each}
  </div>
  <p class="text-xs text-text-3 mt-2">
    {getVisibilityTitle(value)}
  </p>
</div>
