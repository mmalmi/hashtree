<script lang="ts">
  /**
   * ReadmePanel - Bordered panel for displaying README.md content
   */
  import { marked } from 'marked';
  import DOMPurify from 'dompurify';
  import { LinkType, type TreeEntry } from 'hashtree';
  import { routeStore } from '../../stores';

  interface Props {
    content: string;
    entries: TreeEntry[];
    canEdit: boolean;
  }

  let { content, entries, canEdit }: Props = $props();
  let route = $derived($routeStore);

  // Convert markdown to HTML and sanitize to prevent XSS
  let htmlContent = $derived(DOMPurify.sanitize(marked.parse(content, { async: false }) as string));

  function handleEdit() {
    const readmeEntry = entries.find(
      e => e.name.toLowerCase() === 'readme.md' && e.type !== LinkType.Dir
    );
    if (readmeEntry) {
      // Navigate to edit the README - use actual filename from entry
      const parts: string[] = [];
      if (route.npub && route.treeName) {
        parts.push(route.npub, route.treeName, ...route.path, readmeEntry.name);
      }
      window.location.hash = '/' + parts.map(encodeURIComponent).join('/') + '?edit=1';
    }
  }
</script>

<div class="bg-surface-0 b-1 b-surface-3 b-solid rounded-lg overflow-hidden">
  <div class="flex items-center justify-between px-4 py-2 b-b-1 b-b-solid b-b-surface-3">
    <div class="flex items-center gap-2">
      <span class="i-lucide-book-open text-text-2"></span>
      <span class="text-sm font-medium">README.md</span>
    </div>
    {#if canEdit}
      <button
        onclick={handleEdit}
        class="btn-ghost text-xs px-2 py-1"
      >
        Edit
      </button>
    {/if}
  </div>
  <div class="p-4 lg:p-6 prose prose-sm max-w-none text-text-1">
    <!-- eslint-disable-next-line svelte/no-at-html-tags -- sanitized with DOMPurify -->
    {@html htmlContent}
  </div>
</div>
