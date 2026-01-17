<script lang="ts">
  /**
   * AddCommentModal - Modal for adding a new comment to selected text
   */

  interface Props {
    show: boolean;
    quotedText: string;
    onSubmit: (comment: string) => void;
    onCancel: () => void;
  }

  let { show, quotedText, onSubmit, onCancel }: Props = $props();

  let commentText = $state('');
  let textareaRef: HTMLTextAreaElement | undefined = $state();

  // Focus textarea when modal opens
  $effect(() => {
    if (show) {
      commentText = '';
      // Use setTimeout to ensure DOM is ready
      setTimeout(() => textareaRef?.focus(), 10);
    }
  });

  // Handle Escape key to close modal
  $effect(() => {
    if (!show) return;

    const handleKeyDown = (e: KeyboardEvent) => {
      if (e.key === 'Escape') {
        onCancel();
      }
    };

    document.addEventListener('keydown', handleKeyDown);
    return () => document.removeEventListener('keydown', handleKeyDown);
  });

  function handleSubmit() {
    const text = commentText.trim();
    if (!text) return;
    onSubmit(text);
    commentText = '';
  }

  function handleKeyDown(e: KeyboardEvent) {
    // Submit on Ctrl/Cmd + Enter
    if (e.key === 'Enter' && (e.ctrlKey || e.metaKey)) {
      e.preventDefault();
      handleSubmit();
    }
  }
</script>

{#if show}
  <!-- svelte-ignore a11y_click_events_have_key_events -->
  <!-- svelte-ignore a11y_no_static_element_interactions -->
  <div
    class="fixed inset-0 bg-black/70 flex-center z-1000 overflow-auto"
    onclick={(e) => {
      if (e.target === e.currentTarget) onCancel();
    }}
  >
    <div class="bg-surface-1 rounded-lg w-full max-w-md mx-4 border border-surface-3 shadow-xl">
      <!-- Header -->
      <div class="px-4 py-3 border-b border-surface-3 flex items-center justify-between">
        <h3 class="font-medium text-text-1 flex items-center gap-2">
          <span class="i-lucide-message-square-plus text-accent"></span>
          Add Comment
        </h3>
        <button onclick={onCancel} class="btn-ghost p-1" title="Close">
          <span class="i-lucide-x text-lg"></span>
        </button>
      </div>

      <!-- Quoted text -->
      <div class="px-4 pt-4">
        <div class="text-xs text-text-3 mb-1">Commenting on:</div>
        <div class="text-sm bg-surface-2 px-3 py-2 rounded border-l-2 border-accent/50 italic text-text-2 max-h-20 overflow-auto">
          "{quotedText}"
        </div>
      </div>

      <!-- Comment input -->
      <div class="p-4">
        <textarea
          bind:this={textareaRef}
          bind:value={commentText}
          onkeydown={handleKeyDown}
          placeholder="Write your comment..."
          class="input w-full min-h-24 resize-y text-sm"
          rows="3"
        ></textarea>
        <div class="text-xs text-text-3 mt-1">
          Press <kbd class="px-1 py-0.5 bg-surface-2 rounded text-text-2">Ctrl+Enter</kbd> to submit
        </div>
      </div>

      <!-- Actions -->
      <div class="px-4 pb-4 flex justify-end gap-2">
        <button onclick={onCancel} class="btn-ghost px-4 py-2">
          Cancel
        </button>
        <button
          onclick={handleSubmit}
          disabled={!commentText.trim()}
          class="btn-primary px-4 py-2 disabled:opacity-50"
        >
          Add Comment
        </button>
      </div>
    </div>
  </div>
{/if}
