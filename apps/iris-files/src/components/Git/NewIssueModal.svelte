<script lang="ts" module>
  /**
   * Modal for creating a new issue
   */

  export interface NewIssueTarget {
    npub: string;
    repoName: string;
    onCreate?: (issue: { id: string; title: string }) => void;
  }

  let isOpen = $state(false);
  let target = $state<NewIssueTarget | null>(null);

  export function open(t: NewIssueTarget) {
    target = t;
    isOpen = true;
  }

  export function close() {
    isOpen = false;
    target = null;
  }
</script>

<script lang="ts">
  import { createIssue } from '../../nip34';

  let title = $state('');
  let description = $state('');
  let labels = $state('');
  let isSubmitting = $state(false);
  let error = $state<string | null>(null);

  function handleClose() {
    title = '';
    description = '';
    labels = '';
    error = null;
    close();
  }

  async function handleSubmit(e: Event) {
    e.preventDefault();
    if (!target || !title.trim()) return;

    isSubmitting = true;
    error = null;

    try {
      const labelList = labels.trim()
        ? labels.split(',').map(l => l.trim()).filter(Boolean)
        : [];

      const issue = await createIssue(target.npub, target.repoName, title.trim(), description.trim(), {
        labels: labelList,
      });

      if (issue) {
        target.onCreate?.({ id: issue.id, title: issue.title });
        handleClose();
        // Navigate to the new issue
        window.location.hash = `/${target.npub}/${target.repoName}/issues/${issue.id}`;
      } else {
        error = 'Failed to create issue';
      }
    } catch (err) {
      error = err instanceof Error ? err.message : 'Failed to create issue';
    } finally {
      isSubmitting = false;
    }
  }

  function handleKeyDown(e: KeyboardEvent) {
    if (e.key === 'Escape') {
      handleClose();
    }
  }
</script>

{#if isOpen && target}
  <div
    class="fixed inset-0 bg-black/50 flex items-center justify-center z-50"
    role="dialog" tabindex="-1"
    aria-modal="true"
    onclick={(e) => e.target === e.currentTarget && handleClose()}
    onkeydown={handleKeyDown}
  >
    <div class="bg-surface-0 rounded-lg shadow-xl w-full max-w-lg mx-4 overflow-hidden">
      <!-- Header -->
      <div class="flex items-center justify-between px-4 py-3 b-b-1 b-b-solid b-b-surface-3">
        <h2 class="text-lg font-semibold">New Issue</h2>
        <button onclick={handleClose} class="btn-ghost p-1" aria-label="Close">
          <span class="i-lucide-x text-lg"></span>
        </button>
      </div>

      <!-- Form -->
      <form onsubmit={handleSubmit} class="p-4 flex flex-col gap-4">
        {#if error}
          <div class="px-3 py-2 bg-danger/10 text-danger rounded-md text-sm">
            {error}
          </div>
        {/if}

        <div class="flex flex-col gap-1.5">
          <label for="issue-title" class="text-sm font-medium">Title</label>
          <input
            id="issue-title"
            type="text"
            bind:value={title}
            placeholder="Brief description of the issue"
            class="px-3 py-2 bg-surface-1 b-1 b-solid b-surface-3 rounded-md focus:outline-none focus:b-accent"
            required
          />
        </div>

        <div class="flex flex-col gap-1.5">
          <label for="issue-description" class="text-sm font-medium">Description</label>
          <textarea
            id="issue-description"
            bind:value={description}
            placeholder="Detailed description (markdown supported)"
            rows="6"
            class="px-3 py-2 bg-surface-1 b-1 b-solid b-surface-3 rounded-md focus:outline-none focus:b-accent resize-y"
          ></textarea>
        </div>

        <div class="flex flex-col gap-1.5">
          <label for="issue-labels" class="text-sm font-medium">Labels (optional)</label>
          <input
            id="issue-labels"
            type="text"
            bind:value={labels}
            placeholder="bug, enhancement, help wanted"
            class="px-3 py-2 bg-surface-1 b-1 b-solid b-surface-3 rounded-md focus:outline-none focus:b-accent"
          />
          <span class="text-xs text-text-3">Comma-separated list of labels</span>
        </div>

        <!-- Actions -->
        <div class="flex justify-end gap-2 pt-2">
          <button type="button" onclick={handleClose} class="btn-ghost px-4 py-2">
            Cancel
          </button>
          <button
            type="submit"
            disabled={!title.trim() || isSubmitting}
            class="btn-primary px-4 py-2 flex items-center gap-2"
          >
            {#if isSubmitting}
              <span class="i-lucide-loader-2 animate-spin"></span>
            {:else}
              <span class="i-lucide-circle-dot"></span>
            {/if}
            Create Issue
          </button>
        </div>
      </form>
    </div>
  </div>
{/if}
