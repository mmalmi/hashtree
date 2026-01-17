<script lang="ts">
  /**
   * CopyInput - GitHub-style readonly input with copy button
   */
  interface Props {
    text: string;
    class?: string;
  }

  let { text, class: className = '' }: Props = $props();

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
</script>

<div class="flex items-center gap-1 {className}">
  <input
    type="text"
    readonly
    value={text}
    class="flex-1 min-w-0 input text-xs font-mono bg-surface-2 px-2 py-1.5"
    onclick={(e) => (e.target as HTMLInputElement).select()}
  />
  <button
    onclick={handleCopy}
    class="btn-ghost p-1.5 shrink-0"
    title="Copy"
  >
    {#if copied}
      <span class="i-lucide-check text-success"></span>
    {:else}
      <span class="i-lucide-copy"></span>
    {/if}
  </button>
</div>
