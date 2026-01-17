<script lang="ts">
  /**
   * CopyText - clickable text with copy icon that copies to clipboard
   */
  interface Props {
    text: string;
    displayText?: string;
    truncate?: number;
    class?: string;
    testId?: string;
  }

  let { text, displayText, truncate, class: className = '', testId }: Props = $props();

  let copied = $state(false);

  async function handleCopy() {
    try {
      await navigator.clipboard.writeText(text);
      copied = true;
      setTimeout(() => (copied = false), 2000);
    } catch (e) {
      console.error('Failed to copy:', e);
    }
  }

  // Determine what to display
  let display = $derived.by(() => {
    let d = displayText ?? text;
    if (truncate && d.length > truncate) {
      const half = Math.floor((truncate - 3) / 2);
      d = d.slice(0, half) + '...' + d.slice(-half);
    }
    return d;
  });
</script>

<button
  onclick={handleCopy}
  class="inline text-text-2 hover:text-text-1 bg-transparent border-none cursor-pointer p-0 text-left {className}"
  title="Copy"
  data-testid={testId}
>
  {#if copied}
    <span class="i-lucide-check text-success text-xs mr-1 inline-block align-middle"></span>
  {:else}
    <span class="i-lucide-copy text-xs mr-1 inline-block align-middle"></span>
  {/if}
  <span class="font-mono break-all">{display}</span>
</button>
