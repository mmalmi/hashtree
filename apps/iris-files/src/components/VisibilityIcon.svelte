<script lang="ts">
  /**
   * VisibilityIcon - displays icon for tree visibility level
   * Shows nothing if visibility is undefined (not yet resolved)
   */
  import type { TreeVisibility } from 'hashtree';

  interface Props {
    visibility: TreeVisibility | undefined;
    class?: string;
  }

  let { visibility, class: className = '' }: Props = $props();

  function getVisibilityInfo(vis: TreeVisibility): { icon: string; title: string } {
    switch (vis) {
      case 'public':
        return { icon: 'i-lucide-globe', title: 'Public' };
      case 'link-visible':
        return { icon: 'i-lucide-link', title: 'Link-visible (link only)' };
      case 'private':
        return { icon: 'i-lucide-lock', title: 'Private' };
    }
  }

  let info = $derived(visibility ? getVisibilityInfo(visibility) : null);
</script>

{#if !visibility || !info || visibility === 'public'}
  <!-- Don't show anything if visibility not resolved or public -->
{:else if visibility === 'link-visible'}
  <!-- LinkLockIcon - combined link icon with small lock in bottom-right corner -->
  <span class="relative inline-flex items-center shrink-0 {className}" title={info.title}>
    <span class="i-lucide-link"></span>
    <span class="i-lucide-lock absolute -bottom-0.5 -right-1.5 text-[0.6em]"></span>
  </span>
{:else}
  <span
    class="shrink-0 {info.icon} {className}"
    title={info.title}
  ></span>
{/if}
