<script lang="ts">
  /**
   * IssuesView - Lists issues for a repository using NIP-34
   * Layout matches TreeRoute: FileBrowser on left, content on right
   */
  import { routeStore, treeRootStore, createTreesStore } from '../../stores';
  import { createIssuesStore, filterByStatus, countByStatus } from '../../stores/nip34';
  import { open as openNewIssueModal } from './NewIssueModal.svelte';
  import { nostrStore } from '../../nostr';
  import { encodeEventId, type Issue, type ItemStatus } from '../../nip34';
  import ItemStatusBadge from './ItemStatusBadge.svelte';
  import ItemListHeader from './ItemListHeader.svelte';
  import RepoTabNav from './RepoTabNav.svelte';
  import AuthorName from './AuthorName.svelte';
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

  // Get tree visibility info
  let treesStore = $derived(createTreesStore(npub));
  let trees = $state<Array<{ name: string; visibility?: string }>>([]);

  $effect(() => {
    const store = treesStore;
    const unsub = store.subscribe(value => {
      trees = value;
    });
    return unsub;
  });

  // Extract the base tree name from repoName (which may include subdir path)
  let baseTreeName = $derived(repoName.split('/')[0]);
  let currentTree = $derived(trees.find(t => t.name === baseTreeName));

  // Build back URL (to code tab at same location)
  let backUrl = $derived.by(() => {
    const linkKeySuffix = route.params.get('k') ? `?k=${route.params.get('k')}` : '';
    if (currentPath.length > 0) {
      // In a subdirectory - link to that directory
      return `#/${npub}/${route.treeName}/${currentPath.join('/')}${linkKeySuffix}`;
    }
    return `#/${npub}/${route.treeName}${linkKeySuffix}`;
  });

  // Get current directory name for header
  let currentDirName = $derived(currentPath.length > 0 ? currentPath[currentPath.length - 1] : baseTreeName);

  // Create store for this repo's issues
  let issuesStore = $derived(createIssuesStore(npub, repoName));
  let issuesState = $derived($issuesStore);

  // Filter issues
  let filteredIssues = $derived(filterByStatus(issuesState.items, issuesState.filter));
  let counts = $derived(countByStatus(issuesState.items));

  // Check if user can create issues (logged in)
  let userNpub = $derived($nostrStore.npub);
  let canCreate = $derived(!!userNpub);

  function handleFilterChange(filter: ItemStatus | 'all') {
    issuesStore.setFilter(filter);
  }

  function handleNewIssue() {
    openNewIssueModal({ npub, repoName, onCreate: () => {
      // Refresh the list after creating
      issuesStore.refresh();
    }});
  }

  function getIssueHref(issue: Issue): string {
    const encodedId = encodeEventId(issue.id);
    return `#/${npub}/${repoName}?tab=issues&id=${encodedId}`;
  }

  function formatDate(timestamp: number): string {
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

<!-- File browser on left - hidden on mobile since we're showing Issues content -->
<div class="hidden lg:flex lg:w-80 shrink-0 lg:border-r border-surface-3 flex-col min-h-0">
  <FileBrowser />
</div>

<!-- Right panel with Issues - shown on mobile -->
<div class="flex flex-1 flex-col min-w-0 min-h-0 bg-surface-0">
  <!-- Header with back button, avatar, visibility, folder name -->
  <ViewerHeader
    {backUrl}
    {npub}
    {rootCid}
    visibility={currentTree?.visibility}
    icon="i-lucide-folder-open text-warning"
    name={currentDirName}
  />

  <!-- Tab navigation -->
  <RepoTabNav {npub} {repoName} activeTab="issues" />

  <!-- Header with filter and new issue button -->
  <ItemListHeader
    type="issue"
    {counts}
    filter={issuesState.filter}
    onFilterChange={handleFilterChange}
    onNew={handleNewIssue}
    {canCreate}
  />

  <!-- Issues list -->
  <div class="flex-1 overflow-auto">
    {#if issuesState.loading}
      <div class="flex items-center justify-center py-12 text-text-3">
        <span class="i-lucide-loader-2 animate-spin mr-2"></span>
        Loading issues...
      </div>
    {:else if issuesState.error}
      <div class="flex flex-col items-center justify-center py-12 text-danger">
        <span class="i-lucide-alert-circle text-2xl mb-2"></span>
        <span>{issuesState.error}</span>
        <button onclick={() => issuesStore.refresh()} class="btn-ghost mt-2 text-sm">
          Try again
        </button>
      </div>
    {:else if filteredIssues.length === 0}
      <div class="flex flex-col items-center justify-center py-12 text-text-3">
        <span class="i-lucide-circle-dot text-4xl mb-4 opacity-50"></span>
        {#if issuesState.items.length === 0}
          <span class="text-lg mb-2">No issues yet</span>
          <span class="text-sm">Issues are used to track bugs, enhancements, and tasks</span>
        {:else}
          <span>No {issuesState.filter} issues</span>
        {/if}
      </div>
    {:else}
      <div class="divide-y divide-surface-3">
        {#each filteredIssues as issue (issue.id)}
          <div class="px-4 py-3 flex items-start gap-3">
            <!-- Status icon -->
            <div class="mt-1">
              <ItemStatusBadge status={issue.status} type="issue" />
            </div>

            <!-- Content -->
            <div class="flex-1 min-w-0">
              <div class="flex items-center gap-2 mb-1 flex-wrap">
                <a
                  href={getIssueHref(issue)}
                  class="font-medium text-text-1 hover:text-accent hover:underline truncate"
                >{issue.title}</a>
                {#each issue.labels as label (label)}
                  <span class="px-2 py-0.5 text-xs rounded-full bg-accent/10 text-accent">{label}</span>
                {/each}
              </div>
              <div class="text-sm text-text-3">
                opened {formatDate(issue.created_at)} by
                <AuthorName pubkey={issue.authorPubkey} npub={issue.author} />
              </div>
            </div>
          </div>
        {/each}
      </div>
    {/if}
  </div>
</div>
