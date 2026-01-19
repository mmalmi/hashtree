<script lang="ts">
  /**
   * ReleaseDetailView - Shows a single release with notes and assets.
   * Layout matches TreeRoute: FileBrowser on left, content on right.
   */
  import { marked } from 'marked';
  import DOMPurify from 'dompurify';
  import { routeStore, createTreesStore } from '../../stores';
  import { nostrStore } from '../../nostr';
  import { getNpubFileUrl } from '../../lib/mediaUrl';
  import {
    fetchReleaseDetail,
    deleteRelease,
    buildReleaseTreeName,
    type ReleaseDetail,
  } from '../../stores/releases';
  import { formatBytes } from '../../store';
  import { open as openReleaseModal } from './ReleaseModal.svelte';
  import RepoTabNav from './RepoTabNav.svelte';
  import FileBrowser from '../FileBrowser.svelte';

  interface Props {
    npub: string;
    repoName: string;
    releaseId: string;
  }

  let { npub, repoName, releaseId }: Props = $props();

  let route = $derived($routeStore);

  let treesStore = $derived(createTreesStore(npub));
  let trees = $state<Array<{ name: string; visibility?: string; linkKey?: string }>>([]);

  $effect(() => {
    const store = treesStore;
    const unsub = store.subscribe(value => {
      trees = value;
    });
    return unsub;
  });

  let baseTreeName = $derived(repoName.split('/')[0]);
  let currentTree = $derived(trees.find(t => t.name === baseTreeName));

  let releaseTreeName = $derived(buildReleaseTreeName(repoName));
  let releaseTree = $derived(trees.find(t => t.name === releaseTreeName));
  let releaseVisibility = $derived(releaseTree?.visibility ?? currentTree?.visibility ?? 'public');
  let releaseLinkKey = $derived(releaseTree?.linkKey ?? currentTree?.linkKey ?? route.params.get('k') ?? undefined);

  let release = $state<ReleaseDetail | null>(null);
  let loading = $state(true);
  let error = $state<string | null>(null);
  let isOwner = $derived($nostrStore.npub === npub);

  $effect(() => {
    if (npub && repoName && releaseId) {
      loadRelease();
    }
  });

  async function loadRelease() {
    loading = true;
    error = null;

    try {
      const result = await fetchReleaseDetail(npub, repoName, releaseId);
      if (!result) {
        error = 'Release not found';
        release = null;
        return;
      }
      if (result.draft && !isOwner) {
        error = 'Release not found';
        release = null;
        return;
      }
      release = result;
    } catch (err) {
      error = err instanceof Error ? err.message : 'Failed to load release';
    } finally {
      loading = false;
    }
  }

  let notesHtml = $derived(
    release?.notes
      ? DOMPurify.sanitize(marked.parse(release.notes, { async: false }) as string)
      : ''
  );

  function getBackHref(): string {
    const linkKeySuffix = route.params.get('k') ? `?k=${route.params.get('k')}&tab=releases` : '?tab=releases';
    return `#/${npub}/${repoName}${linkKeySuffix}`;
  }

  function formatDate(timestamp: number | undefined): string {
    if (!timestamp) return 'unknown';
    return new Date(timestamp * 1000).toLocaleString();
  }

  function getAssetHref(asset: { path: string }): string {
    if (!release) return '#';
    const assetPath = `${release.id}/${asset.path}`;
    return getNpubFileUrl(npub, releaseTreeName, assetPath);
  }

  async function handleDelete() {
    if (!release) return;
    if (releaseVisibility === 'link-visible' && !releaseLinkKey) {
      alert('Link key missing. Open the repo with the shared link to delete this release.');
      return;
    }
    const confirmed = window.confirm(`Delete release "${release.title}"? This cannot be undone.`);
    if (!confirmed) return;

    const success = await deleteRelease(
      npub,
      repoName,
      release.id,
      releaseVisibility as 'public' | 'link-visible' | 'private',
      releaseLinkKey ?? undefined
    );

    if (success) {
      window.location.hash = getBackHref();
    } else {
      alert('Failed to delete release');
    }
  }

  function handleEdit() {
    if (!release) return;
    openReleaseModal({
      npub,
      repoName,
      visibility: releaseVisibility as 'public' | 'link-visible' | 'private',
      linkKey: releaseLinkKey ?? undefined,
      release,
      onSave: () => loadRelease(),
    });
  }
</script>

<!-- File browser on left - hidden on mobile since we're showing release detail -->
<div class="hidden lg:flex lg:w-80 shrink-0 lg:border-r border-surface-3 flex-col min-h-0">
  <FileBrowser />
</div>

<!-- Right panel with release detail - shown on mobile -->
<div class="flex flex-1 flex-col min-w-0 min-h-0 bg-surface-0">
  <!-- Tab navigation -->
  <RepoTabNav {npub} {repoName} activeTab="releases" />

  <!-- Content -->
  <div class="flex-1 overflow-auto">
    {#if loading}
      <div class="flex items-center justify-center py-12 text-text-3">
        <span class="i-lucide-loader-2 animate-spin mr-2"></span>
        Loading release...
      </div>
    {:else if error}
      <div class="flex flex-col items-center justify-center py-12 text-danger">
        <span class="i-lucide-alert-circle text-2xl mb-2"></span>
        <span>{error}</span>
        <a href={getBackHref()} class="btn-ghost mt-4">
          <span class="i-lucide-arrow-left mr-2"></span>
          Back to releases
        </a>
      </div>
    {:else if release}
      <!-- Header -->
      <div class="p-4 b-b-1 b-b-solid b-b-surface-3">
        <div class="flex items-start gap-3">
          <a href={getBackHref()} class="mt-1 text-text-3 hover:text-text-1" aria-label="Go back">
            <span class="i-lucide-arrow-left"></span>
          </a>
          <div class="flex-1 min-w-0">
            <div class="flex items-center gap-2 mb-2 flex-wrap">
              <h1 class="text-xl font-semibold text-text-1">{release.title}</h1>
              {#if release.draft}
                <span class="px-2 py-0.5 text-xs rounded-full bg-warning/10 text-warning">Draft</span>
              {/if}
              {#if release.prerelease}
                <span class="px-2 py-0.5 text-xs rounded-full bg-accent/10 text-accent">Pre-release</span>
              {/if}
            </div>
            <div class="text-sm text-text-3 flex items-center gap-2 flex-wrap">
              {#if release.tag}
                <span class="font-mono text-xs bg-surface-2 px-1 rounded">{release.tag}</span>
              {/if}
              <span>published {formatDate(release.published_at ?? release.created_at)}</span>
              {#if release.commit}
                <span class="font-mono text-xs bg-surface-2 px-1 rounded">{release.commit.slice(0, 7)}</span>
              {/if}
            </div>
          </div>

          {#if isOwner}
            <div class="flex gap-2">
              <button onclick={handleEdit} class="btn-ghost text-sm">
                <span class="i-lucide-pencil mr-1"></span>
                Edit
              </button>
              <button onclick={handleDelete} class="btn-ghost text-sm text-danger">
                <span class="i-lucide-trash-2 mr-1"></span>
                Delete
              </button>
            </div>
          {/if}
        </div>
      </div>

      <!-- Notes -->
      <div class="p-4 b-b-1 b-b-solid b-b-surface-3">
        {#if release.notes}
          <div class="prose prose-sm max-w-none text-text-2">
            <!-- eslint-disable-next-line svelte/no-at-html-tags -- sanitized with DOMPurify -->
            {@html notesHtml}
          </div>
        {:else}
          <div class="text-text-3 text-sm">No release notes provided.</div>
        {/if}
      </div>

      <!-- Assets -->
      <div class="p-4">
        <h2 class="text-sm font-medium text-text-2 mb-3">
          Assets {release.assets.length > 0 ? `(${release.assets.length})` : ''}
        </h2>
        {#if release.assets.length > 0}
          <div class="flex flex-col gap-2">
            {#each release.assets as asset (asset.name)}
              <a
                href={getAssetHref(asset)}
                class="flex items-center justify-between bg-surface-1 rounded-md px-3 py-2 text-sm text-text-1 hover:text-accent"
                download
              >
                <span class="truncate">{asset.name}</span>
                <span class="text-text-3 text-xs">{formatBytes(asset.size)}</span>
              </a>
            {/each}
          </div>
        {:else}
          <div class="text-text-3 text-sm">No assets uploaded.</div>
        {/if}
      </div>
    {/if}
  </div>
</div>
