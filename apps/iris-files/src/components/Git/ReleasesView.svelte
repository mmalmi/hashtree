<script lang="ts">
  /**
   * ReleasesView - Lists releases for a repository.
   * Layout matches TreeRoute: FileBrowser on left, content on right.
   */
  import { routeStore, treeRootStore, createTreesStore } from '../../stores';
  import { createReleasesStore, buildReleaseTreeName, type ReleaseSummary } from '../../stores/releases';
  import { nostrStore } from '../../nostr';
  import { SvelteURLSearchParams } from 'svelte/reactivity';
  import { open as openReleaseModal } from './ReleaseModal.svelte';
  import RepoTabNav from './RepoTabNav.svelte';
  import FileBrowser from '../FileBrowser.svelte';
  import ViewerHeader from '../Viewer/ViewerHeader.svelte';

  interface Props {
    npub: string;
    repoName: string;
  }

  let { npub, repoName }: Props = $props();

  let route = $derived($routeStore);
  let rootCid = $derived($treeRootStore);
  let currentPath = $derived(route.path);

  // Tree visibility info for header and release publishing
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

  // Build back URL (to code tab at same location)
  let backUrl = $derived.by(() => {
    const linkKeySuffix = route.params.get('k') ? `?k=${route.params.get('k')}` : '';
    if (currentPath.length > 0) {
      return `#/${npub}/${route.treeName}/${currentPath.join('/')}${linkKeySuffix}`;
    }
    return `#/${npub}/${route.treeName}${linkKeySuffix}`;
  });

  // Folder name for header
  let currentDirName = $derived(currentPath.length > 0 ? currentPath[currentPath.length - 1] : baseTreeName);

  // Release store
  let releasesStore = $derived(createReleasesStore(npub, repoName));
  let releasesState = $derived($releasesStore);

  let isOwner = $derived($nostrStore.npub === npub);
  let visibleReleases = $derived(
    isOwner ? releasesState.items : releasesState.items.filter(r => !r.draft)
  );

  function handleNewRelease() {
    openReleaseModal({
      npub,
      repoName,
      visibility: releaseVisibility as 'public' | 'link-visible' | 'private',
      linkKey: releaseLinkKey ?? undefined,
      existingIds: releasesState.items.map(r => r.id),
      onSave: () => {
        releasesStore.refresh();
      },
    });
  }

  function getReleaseHref(release: ReleaseSummary): string {
    const linkKey = route.params.get('k');
    const query = new SvelteURLSearchParams();
    if (linkKey) query.set('k', linkKey);
    query.set('tab', 'releases');
    query.set('id', release.id);
    return `#/${npub}/${repoName}?${query.toString()}`;
  }

  function formatDate(timestamp: number | undefined): string {
    if (!timestamp) return 'unknown';
    const date = new Date(timestamp * 1000);
    const now = new Date();
    const diff = now.getTime() - date.getTime();
    const days = Math.floor(diff / (1000 * 60 * 60 * 24));

    if (days === 0) {
      const hours = Math.floor(diff / (1000 * 60 * 60));
      if (hours === 0) {
        const minutes = Math.floor(diff / (1000 * 60));
        return `${minutes}m ago`;
      }
      return `${hours}h ago`;
    }
    if (days < 7) return `${days}d ago`;
    if (days < 30) return `${Math.floor(days / 7)}w ago`;
    return date.toLocaleDateString();
  }
</script>

<!-- File browser on left - hidden on mobile since we're showing releases content -->
<div class="hidden lg:flex lg:w-80 shrink-0 lg:border-r border-surface-3 flex-col min-h-0">
  <FileBrowser />
</div>

<!-- Right panel with releases - shown on mobile -->
<div class="flex flex-1 flex-col min-w-0 min-h-0 bg-surface-0">
  <!-- Header with back button, avatar, visibility, folder name -->
  <ViewerHeader
    {backUrl}
    {npub}
    {rootCid}
    visibility={currentTree?.visibility}
    icon="i-lucide-tag text-accent"
    name={currentDirName}
  />

  <!-- Tab navigation -->
  <RepoTabNav {npub} {repoName} activeTab="releases" />

  <!-- Header with new release button -->
  <div class="flex items-center justify-between px-4 py-3 bg-surface-1 b-b-1 b-b-solid b-b-surface-3">
    <div class="text-sm text-text-3">
      {visibleReleases.length} release{visibleReleases.length !== 1 ? 's' : ''}
    </div>
    {#if isOwner}
      <button onclick={handleNewRelease} class="btn-primary flex items-center gap-2 px-3 h-9 text-sm">
        <span class="i-lucide-plus"></span>
        New Release
      </button>
    {/if}
  </div>

  <!-- Releases list -->
  <div class="flex-1 overflow-auto">
    {#if releasesState.loading}
      <div class="flex items-center justify-center py-12 text-text-3">
        <span class="i-lucide-loader-2 animate-spin mr-2"></span>
        Loading releases...
      </div>
    {:else if releasesState.error}
      <div class="flex flex-col items-center justify-center py-12 text-danger">
        <span class="i-lucide-alert-circle text-2xl mb-2"></span>
        <span>{releasesState.error}</span>
        <button onclick={() => releasesStore.refresh()} class="btn-ghost mt-2 text-sm">
          Try again
        </button>
      </div>
    {:else if visibleReleases.length === 0}
      <div class="flex flex-col items-center justify-center py-12 text-text-3">
        <span class="i-lucide-tag text-4xl mb-4 opacity-50"></span>
        {#if releasesState.items.length === 0}
          <span class="text-lg mb-2">No releases yet</span>
          <span class="text-sm">Publish release notes and attach build artifacts</span>
        {:else}
          <span>No published releases</span>
        {/if}
      </div>
    {:else}
      <div class="divide-y divide-surface-3">
        {#each visibleReleases as release (release.id)}
          <div class="px-4 py-3 flex items-start gap-3">
            <div class="mt-1 text-text-3">
              <span class="i-lucide-tag"></span>
            </div>
            <div class="flex-1 min-w-0">
              <div class="flex items-center gap-2 mb-1 flex-wrap">
                <a
                  href={getReleaseHref(release)}
                  class="font-medium text-text-1 hover:text-accent hover:underline truncate"
                >{release.title}</a>
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
          </div>
        {/each}
      </div>
    {/if}
  </div>
</div>
