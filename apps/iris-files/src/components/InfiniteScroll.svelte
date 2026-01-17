<script lang="ts">
  import { onMount, onDestroy, type Snippet } from 'svelte';

  interface Props {
    onLoadMore: () => void;
    loading?: boolean;  // Don't trigger while loading
    children: Snippet;
  }

  let { onLoadMore, loading = false, children }: Props = $props();

  let sentinel: HTMLDivElement;
  let observer: IntersectionObserver | null = null;
  let isIntersecting = $state(false);

  function findNearestScrollingParent(element: HTMLElement): HTMLElement | null {
    let parent = element.parentElement;
    while (parent) {
      const computedStyle = getComputedStyle(parent);
      const overflowY = computedStyle.overflowY;
      if (overflowY === 'auto' || overflowY === 'scroll' || parent.hasAttribute('data-scrollable')) {
        return parent;
      }
      parent = parent.parentElement;
    }
    return null;
  }

  // Trigger load when intersecting and not loading
  $effect(() => {
    if (isIntersecting && !loading) {
      onLoadMore();
    }
  });

  onMount(() => {
    if (!sentinel) return;

    const scrollContainer = findNearestScrollingParent(sentinel);

    observer = new IntersectionObserver(
      (entries) => {
        isIntersecting = entries[0].isIntersecting;
      },
      {
        root: scrollContainer,
        rootMargin: '1000px',
        threshold: 0,
      }
    );

    observer.observe(sentinel);
  });

  onDestroy(() => {
    observer?.disconnect();
  });
</script>

{@render children()}
<div bind:this={sentinel} class="h-px"></div>
