<script lang="ts">
  /**
   * Header - Sticky header with scroll-aware background
   * Transparent at top, smoothly fades to translucent as you scroll
   */
  import { onMount } from 'svelte';

  const SCROLL_THRESHOLD = 25; // Fully opaque after this many pixels
  let opacity = $state(0);

  interface Props {
    children?: import('svelte').Snippet;
  }

  let { children }: Props = $props();

  onMount(() => {
    const handleScroll = () => {
      opacity = Math.min(1, window.scrollY / SCROLL_THRESHOLD);
    };
    window.addEventListener('scroll', handleScroll, { passive: true });
    handleScroll();

    return () => window.removeEventListener('scroll', handleScroll);
  });
</script>

<header
  class="h-14 shrink-0 flex items-center px-4 md:px-6 gap-3 z-20 sticky top-0"
  style:background-color="rgba(15, 15, 15, {opacity})"
  style:backdrop-filter="blur({opacity * 12}px)"
>
  {@render children?.()}
</header>
