<script lang="ts">
  /**
   * VisibilitySelector - Select public/link-visible/private visibility
   */

  interface Props {
    value: 'public' | 'link-visible' | 'private';
    onchange: (value: 'public' | 'link-visible' | 'private') => void;
    disabled?: boolean;
    mode?: 'single' | 'batch';
  }

  let { value, onchange, disabled = false, mode = 'single' }: Props = $props();
</script>

<div>
  <label class="block text-sm text-text-2 mb-2">Visibility</label>
  <div class="flex gap-2">
    <button
      type="button"
      onclick={() => onchange('public')}
      class="flex-1 flex items-center justify-center gap-2 py-2 px-3 btn-ghost {value === 'public' ? 'ring-2 ring-accent bg-surface-3' : ''}"
      {disabled}
    >
      <span class="i-lucide-globe"></span>
      <span class="text-sm">Public</span>
    </button>
    <button
      type="button"
      onclick={() => onchange('link-visible')}
      class="flex-1 flex items-center justify-center gap-2 py-2 px-3 btn-ghost {value === 'link-visible' ? 'ring-2 ring-accent bg-surface-3' : ''}"
      {disabled}
    >
      <span class="i-lucide-link"></span>
      <span class="text-sm">Link-visible</span>
    </button>
    <button
      type="button"
      onclick={() => onchange('private')}
      class="flex-1 flex items-center justify-center gap-2 py-2 px-3 btn-ghost {value === 'private' ? 'ring-2 ring-accent bg-surface-3' : ''}"
      {disabled}
    >
      <span class="i-lucide-lock"></span>
      <span class="text-sm">Private</span>
    </button>
  </div>
  <p class="text-xs text-text-3 mt-2">
    {#if value === 'public'}
      Anyone can find and watch {mode === 'batch' ? 'these videos' : 'this video'}
    {:else if value === 'link-visible'}
      Only people with the link can watch
    {:else}
      Encrypted, only you can watch
    {/if}
  </p>
</div>
