<script lang="ts">
  /**
   * Shared circular share button component
   */
  import { open as openShareModal } from './Modals/ShareModal.svelte';

  interface Props {
    url: string;
    class?: string;
  }

  let { url, class: className = '' }: Props = $props();
</script>

<button
  onclick={() => openShareModal(url)}
  class="btn-circle btn-ghost {className}"
  title="Share"
>
  <span class="i-lucide-share text-base"></span>
</button>
