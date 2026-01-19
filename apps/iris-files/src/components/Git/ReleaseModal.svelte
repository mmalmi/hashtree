<script lang="ts" module>
  /**
   * Modal for creating or editing a release.
   */
  import type { TreeVisibility } from 'hashtree';
  import type { ReleaseDetail, ReleaseSummary } from '../../stores/releases';

  export interface ReleaseModalTarget {
    npub: string;
    repoName: string;
    visibility?: TreeVisibility;
    linkKey?: string;
    existingIds?: string[];
    release?: ReleaseDetail;
    onSave?: (release: ReleaseSummary) => void;
  }

  let isOpen = $state(false);
  let target = $state<ReleaseModalTarget | null>(null);

  export function open(t: ReleaseModalTarget) {
    target = t;
    isOpen = true;
  }

  export function close() {
    isOpen = false;
    target = null;
  }
</script>

<script lang="ts">
  import { saveRelease, type ReleaseAsset } from '../../stores/releases';
  import { formatBytes } from '../../store';
  import { getErrorMessage } from '../../utils/errorMessage';

  let title = $state('');
  let tag = $state('');
  let commit = $state('');
  let notes = $state('');
  let draft = $state(false);
  let prerelease = $state(false);
  let isSubmitting = $state(false);
  let error = $state<string | null>(null);
  let newAssets = $state<File[]>([]);
  let existingAssets = $state<ReleaseAsset[]>([]);
  let createdAt = $state<number | undefined>(undefined);
  let publishedAt = $state<number | undefined>(undefined);

  let missingLinkKey = $derived(
    target?.visibility === 'link-visible' && !target.linkKey
  );

  $effect(() => {
    if (!isOpen || !target) return;
    const release = target.release;
    title = release?.title ?? '';
    tag = release?.tag ?? '';
    commit = release?.commit ?? '';
    notes = release?.notes ?? '';
    draft = release?.draft ?? false;
    prerelease = release?.prerelease ?? false;
    createdAt = release?.created_at;
    publishedAt = release?.published_at;
    existingAssets = release?.assets ?? [];
    newAssets = [];
    error = null;
  });

  function handleClose() {
    title = '';
    tag = '';
    commit = '';
    notes = '';
    draft = false;
    prerelease = false;
    newAssets = [];
    existingAssets = [];
    error = null;
    close();
  }

  function handleAssetInput(e: Event) {
    const input = e.target as HTMLInputElement;
    if (!input.files) return;
    newAssets = [...newAssets, ...Array.from(input.files)];
    input.value = '';
  }

  function removeNewAsset(index: number) {
    newAssets = newAssets.filter((_, i) => i !== index);
  }

  async function handleSubmit(e: Event) {
    e.preventDefault();
    if (!target || !title.trim()) return;

    isSubmitting = true;
    error = null;

    try {
      const saved = await saveRelease({
        npub: target.npub,
        repoPath: target.repoName,
        title: title.trim(),
        tag: tag.trim() || undefined,
        commit: commit.trim() || undefined,
        notes,
        draft,
        prerelease,
        assets: newAssets,
        existingAssets,
        existingIds: target.existingIds,
        releaseId: target.release?.id,
        createdAt,
        publishedAt,
        visibility: target.visibility,
        linkKey: target.linkKey,
      });

      if (!saved) {
        error = 'Failed to save release';
        return;
      }

      target.onSave?.(saved);
      handleClose();
    } catch (err) {
      error = getErrorMessage(err);
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
    <div class="bg-surface-0 rounded-lg shadow-xl w-full max-w-2xl mx-4 overflow-hidden">
      <!-- Header -->
      <div class="flex items-center justify-between px-4 py-3 b-b-1 b-b-solid b-b-surface-3">
        <h2 class="text-lg font-semibold">{target.release ? 'Edit Release' : 'New Release'}</h2>
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
        {#if missingLinkKey}
          <div class="px-3 py-2 bg-warning/10 text-warning rounded-md text-sm">
            Link key missing. Open the repo with the shared link to publish releases.
          </div>
        {/if}

        <div class="grid grid-cols-1 md:grid-cols-2 gap-4">
          <div class="flex flex-col gap-1.5">
            <label for="release-title" class="text-sm font-medium">Title</label>
            <input
              id="release-title"
              type="text"
              bind:value={title}
              placeholder="Release title"
              class="px-3 py-2 bg-surface-1 b-1 b-solid b-surface-3 rounded-md focus:outline-none focus:b-accent"
              required
            />
          </div>

          <div class="flex flex-col gap-1.5">
            <label for="release-tag" class="text-sm font-medium">Tag (optional)</label>
            <input
              id="release-tag"
              type="text"
              bind:value={tag}
              placeholder="v1.2.3"
              class="px-3 py-2 bg-surface-1 b-1 b-solid b-surface-3 rounded-md focus:outline-none focus:b-accent"
            />
          </div>
        </div>

        <div class="flex flex-col gap-1.5">
          <label for="release-commit" class="text-sm font-medium">Commit (optional)</label>
          <input
            id="release-commit"
            type="text"
            bind:value={commit}
            placeholder="Commit SHA"
            class="px-3 py-2 bg-surface-1 b-1 b-solid b-surface-3 rounded-md focus:outline-none focus:b-accent font-mono text-sm"
          />
        </div>

        <div class="flex gap-4 flex-wrap text-sm">
          <label class="flex items-center gap-2">
            <input type="checkbox" bind:checked={draft} class="accent-accent" />
            Draft (visible only to you)
          </label>
          <label class="flex items-center gap-2">
            <input type="checkbox" bind:checked={prerelease} class="accent-accent" />
            Pre-release
          </label>
        </div>

        <div class="flex flex-col gap-1.5">
          <label for="release-notes" class="text-sm font-medium">Release notes (Markdown)</label>
          <textarea
            id="release-notes"
            bind:value={notes}
            placeholder="Highlights, breaking changes, upgrade notes..."
            rows="8"
            class="px-3 py-2 bg-surface-1 b-1 b-solid b-surface-3 rounded-md focus:outline-none focus:b-accent resize-y"
          ></textarea>
        </div>

        <div class="flex flex-col gap-2">
          <div class="flex items-center justify-between">
            <label for="release-assets" class="text-sm font-medium">Assets</label>
            <input
              id="release-assets"
              type="file"
              multiple
              class="hidden"
              onchange={handleAssetInput}
            />
            <label for="release-assets" class="btn-ghost text-sm px-3 py-1 cursor-pointer">
              Add files
            </label>
          </div>

          {#if existingAssets.length > 0}
            <div class="text-xs text-text-3">Existing assets (kept as-is)</div>
            <div class="flex flex-col gap-1 text-sm">
              {#each existingAssets as asset (asset.name)}
                <div class="flex items-center justify-between bg-surface-1 rounded-md px-3 py-2">
                  <span class="truncate">{asset.name}</span>
                  <span class="text-text-3 text-xs">{formatBytes(asset.size)}</span>
                </div>
              {/each}
            </div>
          {/if}

          {#if newAssets.length > 0}
            <div class="text-xs text-text-3">New assets</div>
            <div class="flex flex-col gap-1 text-sm">
              {#each newAssets as file, index (file.name)}
                <div class="flex items-center justify-between bg-surface-1 rounded-md px-3 py-2">
                  <div class="flex items-center gap-2 min-w-0">
                    <span class="truncate">{file.name}</span>
                    <span class="text-text-3 text-xs">{formatBytes(file.size)}</span>
                  </div>
                  <button
                    type="button"
                    onclick={() => removeNewAsset(index)}
                    class="btn-ghost p-1"
                    aria-label="Remove asset"
                  >
                    <span class="i-lucide-x text-sm"></span>
                  </button>
                </div>
              {/each}
            </div>
          {/if}
        </div>

        <!-- Actions -->
        <div class="flex justify-end gap-2 pt-2">
          <button type="button" onclick={handleClose} class="btn-ghost px-4 py-2">
            Cancel
          </button>
          <button
            type="submit"
            disabled={!title.trim() || isSubmitting || missingLinkKey}
            class="btn-primary px-4 py-2 flex items-center gap-2"
          >
            {#if isSubmitting}
              <span class="i-lucide-loader-2 animate-spin"></span>
            {:else}
              <span class="i-lucide-tag"></span>
            {/if}
            {target.release ? 'Save Release' : 'Create Release'}
          </button>
        </div>
      </form>
    </div>
  </div>
{/if}
