<script lang="ts">
  /**
   * Comments Panel - Sidebar showing comment threads
   * Similar to Google Docs comments sidebar
   */
  import type { CommentsStore } from '../../lib/comments';
  import type { CommentsState } from '../../lib/comments/types';
  import { Avatar, Name } from '../User';
  import { npubToPubkey } from '../../nostr';

  interface Props {
    commentsStore: CommentsStore;
    userNpub: string | null;
    onClickThread?: (threadId: string) => void;
    onDeleteThread?: (threadId: string) => void;
  }

  let { commentsStore, userNpub, onClickThread, onDeleteThread }: Props = $props();

  // Close panel on Escape key
  $effect(() => {
    const handleKeyDown = (e: KeyboardEvent) => {
      if (e.key === 'Escape') {
        commentsStore.setPanelOpen(false);
      }
    };

    document.addEventListener('keydown', handleKeyDown);
    return () => document.removeEventListener('keydown', handleKeyDown);
  });

  let state = $state<CommentsState>({ threads: new Map(), activeThreadId: null, panelOpen: false });
  let replyInputs = $state<Record<string, string>>({});
  let showResolved = $state(false);

  // Subscribe to comments store
  $effect(() => {
    const unsub = commentsStore.subscribe((newState) => {
      state = newState;
    });
    return unsub;
  });

  // Get threads as array, sorted by creation time (newest first)
  let threads = $derived(
    Array.from(state.threads.values())
      .filter(t => showResolved || !t.resolved)
      .sort((a, b) => b.createdAt - a.createdAt)
  );

  let resolvedCount = $derived(
    Array.from(state.threads.values()).filter(t => t.resolved).length
  );

  function handleReply(threadId: string) {
    const content = replyInputs[threadId]?.trim();
    if (!content || !userNpub) return;

    commentsStore.addReply(threadId, content, userNpub);
    replyInputs[threadId] = '';
  }

  function handleResolve(threadId: string) {
    commentsStore.resolveThread(threadId);
  }

  function handleUnresolve(threadId: string) {
    commentsStore.unresolveThread(threadId);
  }

  function handleDelete(threadId: string) {
    if (confirm('Delete this comment thread?')) {
      onDeleteThread?.(threadId);
      commentsStore.deleteThread(threadId);
    }
  }

  function handleThreadClick(threadId: string) {
    commentsStore.setActiveThread(threadId);
    onClickThread?.(threadId);
  }

  function formatTime(timestamp: number): string {
    const date = new Date(timestamp);
    const now = new Date();
    const diff = now.getTime() - date.getTime();

    // Less than a minute
    if (diff < 60000) return 'Just now';

    // Less than an hour
    if (diff < 3600000) {
      const mins = Math.floor(diff / 60000);
      return `${mins}m ago`;
    }

    // Less than a day
    if (diff < 86400000) {
      const hours = Math.floor(diff / 3600000);
      return `${hours}h ago`;
    }

    // Less than a week
    if (diff < 604800000) {
      const days = Math.floor(diff / 86400000);
      return `${days}d ago`;
    }

    // Otherwise show date
    return date.toLocaleDateString();
  }
</script>

<div class="h-full flex flex-col bg-surface-1 border-l border-surface-3">
  <!-- Header -->
  <div class="shrink-0 px-4 py-3 border-b border-surface-3 flex items-center justify-between">
    <div class="flex items-center gap-2">
      <span class="i-lucide-message-square text-text-2"></span>
      <h3 class="font-medium text-text-1">Comments</h3>
      {#if threads.length > 0}
        <span class="text-xs bg-surface-2 px-1.5 py-0.5 rounded-full text-text-3">
          {threads.length}
        </span>
      {/if}
    </div>
    <button
      onclick={() => commentsStore.setPanelOpen(false)}
      class="btn-ghost p-1"
      title="Close panel"
    >
      <span class="i-lucide-x text-lg"></span>
    </button>
  </div>

  <!-- Resolved toggle -->
  {#if resolvedCount > 0}
    <div class="shrink-0 px-4 py-2 border-b border-surface-3">
      <button
        onclick={() => showResolved = !showResolved}
        class="text-sm text-text-3 hover:text-text-2 flex items-center gap-1"
      >
        <span class={showResolved ? 'i-lucide-eye-off' : 'i-lucide-eye'}></span>
        {showResolved ? 'Hide' : 'Show'} {resolvedCount} resolved
      </button>
    </div>
  {/if}

  <!-- Comments list -->
  <div class="flex-1 overflow-y-auto">
    {#if threads.length === 0}
      <div class="p-4 text-center text-text-3 text-sm">
        <span class="i-lucide-message-square-plus text-2xl mb-2 block opacity-50"></span>
        No comments yet.<br />
        Select text and click the comment button to add one.
      </div>
    {:else}
      <div class="p-2 space-y-2">
        {#each threads as thread (thread.id)}
          {@const isActive = state.activeThreadId === thread.id}

          <div
            class="rounded-lg border transition-colors cursor-pointer {isActive
              ? 'border-accent bg-accent/5'
              : 'border-surface-3 bg-surface-2 hover:border-surface-4'} {thread.resolved ? 'opacity-60' : ''}"
            onclick={() => handleThreadClick(thread.id)}
            onkeydown={(e) => e.key === 'Enter' && handleThreadClick(thread.id)}
            role="button"
            tabindex="0"
          >
            <!-- Quoted text -->
            {#if thread.quotedText}
              <div class="px-3 pt-3 pb-1">
                <div class="text-xs text-text-3 bg-surface-3/50 px-2 py-1 rounded border-l-2 border-accent/50 italic truncate">
                  "{thread.quotedText}"
                </div>
              </div>
            {/if}

            <!-- Comments in thread -->
            <div class="p-3 space-y-3">
              {#each thread.comments as comment, idx (comment.id)}
                {@const commentAuthorPubkey = npubToPubkey(comment.authorNpub)}
                <div class="flex gap-2 {idx > 0 ? 'pt-2 border-t border-surface-3' : ''}">
                  {#if commentAuthorPubkey}
                    <Avatar pubkey={commentAuthorPubkey} size={24} />
                  {:else}
                    <div class="w-6 h-6 rounded-full bg-surface-3 flex items-center justify-center">
                      <span class="i-lucide-user text-xs text-text-3"></span>
                    </div>
                  {/if}
                  <div class="flex-1 min-w-0">
                    <div class="flex items-center gap-2 text-xs text-text-3">
                      <a href="#/{comment.authorNpub}" class="font-medium text-text-2 truncate hover:text-accent" onclick={(e) => e.stopPropagation()}>
                        {#if commentAuthorPubkey}
                          <Name pubkey={commentAuthorPubkey} />
                        {:else}
                          {comment.authorNpub.slice(0, 12)}...
                        {/if}
                      </a>
                      <span>{formatTime(comment.createdAt)}</span>
                    </div>
                    <p class="text-sm text-text-1 mt-1 whitespace-pre-wrap break-words">
                      {comment.content}
                    </p>
                  </div>
                </div>
              {/each}
            </div>

            <!-- Actions -->
            {#if isActive}
              <div class="px-3 pb-3 space-y-2">
                <!-- Reply input -->
                {#if userNpub}
                  <div class="flex gap-2">
                    <input
                      type="text"
                      placeholder="Reply..."
                      bind:value={replyInputs[thread.id]}
                      onkeydown={(e) => {
                        if (e.key === 'Enter' && !e.shiftKey) {
                          e.preventDefault();
                          handleReply(thread.id);
                        }
                        e.stopPropagation();
                      }}
                      onclick={(e) => e.stopPropagation()}
                      class="input flex-1 text-sm py-1"
                    />
                    <button
                      onclick={(e) => {
                        e.stopPropagation();
                        handleReply(thread.id);
                      }}
                      disabled={!replyInputs[thread.id]?.trim()}
                      class="btn-ghost p-1 disabled:opacity-30"
                      title="Send reply"
                    >
                      <span class="i-lucide-send text-sm"></span>
                    </button>
                  </div>
                {/if}

                <!-- Thread actions -->
                <div class="flex items-center gap-1 pt-1">
                  {#if thread.resolved}
                    <button
                      onclick={(e) => {
                        e.stopPropagation();
                        handleUnresolve(thread.id);
                      }}
                      class="btn-ghost text-xs px-2 py-1 flex items-center gap-1"
                      title="Re-open thread"
                    >
                      <span class="i-lucide-circle-dot"></span>
                      Re-open
                    </button>
                  {:else}
                    <button
                      onclick={(e) => {
                        e.stopPropagation();
                        handleResolve(thread.id);
                      }}
                      class="btn-ghost text-xs px-2 py-1 flex items-center gap-1 text-success"
                      title="Resolve thread"
                    >
                      <span class="i-lucide-check-circle"></span>
                      Resolve
                    </button>
                  {/if}
                  <button
                    onclick={(e) => {
                      e.stopPropagation();
                      handleDelete(thread.id);
                    }}
                    class="btn-ghost text-xs px-2 py-1 flex items-center gap-1 text-danger"
                    title="Delete thread"
                  >
                    <span class="i-lucide-trash-2"></span>
                    Delete
                  </button>
                </div>
              </div>
            {/if}
          </div>
        {/each}
      </div>
    {/if}
  </div>
</div>
