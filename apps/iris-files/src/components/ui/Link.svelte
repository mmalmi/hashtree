<script lang="ts">
  /**
   * Link component for hash-based navigation
   * Ensures proper hashchange events are fired for the router
   */
  import { push } from '../../lib/router.svelte';

  interface Props {
    href: string;
    class?: string;
    title?: string;
    children?: import('svelte').Snippet;
    [key: string]: unknown;
  }

  let { href, class: className = '', title, children, ...rest }: Props = $props();

  function handleClick(e: MouseEvent) {
    // Only handle left clicks without modifiers
    if (e.button !== 0 || e.metaKey || e.ctrlKey || e.shiftKey || e.altKey) {
      return;
    }

    // Only handle hash links
    if (href.startsWith('#/') || href.startsWith('#')) {
      e.preventDefault();
      const path = href.startsWith('#/') ? href.slice(1) : href.slice(1);
      push(path);
    }
  }
</script>

<a {href} class={className} {title} onclick={handleClick} {...rest}>
  {@render children?.()}
</a>
