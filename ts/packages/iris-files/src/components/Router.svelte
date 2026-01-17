<script lang="ts">
  /**
   * Router component that handles route matching and rendering
   * Receives currentPath as a prop to ensure proper reactivity
   */
  import { matchRoute, currentFullHash, getQueryParamsFromHash } from '../lib/router.svelte';

  // Page components
  import SettingsLayout from './settings/SettingsLayout.svelte';
  import WalletPage from './WalletPage.svelte';
  import UsersPage from './UsersPage.svelte';
  import ProfileView from './ProfileView.svelte';
  import FollowsPage from './FollowsPage.svelte';
  import FollowersPage from './FollowersPage.svelte';
  import EditProfilePage from './EditProfilePage.svelte';

  // Route handlers
  import HomeRoute from '../routes/HomeRoute.svelte';
  import TreeRoute from '../routes/TreeRoute.svelte';
  import UserRoute from '../routes/UserRoute.svelte';

  // Git repository views (NIP-34)
  import PullRequestsView from './Git/PullRequestsView.svelte';
  import IssuesView from './Git/IssuesView.svelte';
  import PullRequestDetailView from './Git/PullRequestDetailView.svelte';
  import IssueDetailView from './Git/IssueDetailView.svelte';
  import CommitView from './Git/CommitView.svelte';
  import BranchCompareView from './Git/BranchCompareView.svelte';
  import MergeView from './Git/MergeView.svelte';

  // Route definitions with patterns
  // Note: More specific routes must come before less specific ones
  const routePatterns = [
    { pattern: '/', component: HomeRoute },
    { pattern: '/settings', component: SettingsLayout },
    { pattern: '/settings/:tab', component: SettingsLayout },
    { pattern: '/wallet', component: WalletPage },
    { pattern: '/users', component: UsersPage },
    { pattern: '/:npub/follows', component: FollowsPage },
    { pattern: '/:npub/followers', component: FollowersPage },
    { pattern: '/:npub/edit', component: EditProfilePage },
    { pattern: '/:npub/profile', component: UserRoute },
    // Generic tree routes
    { pattern: '/:npub/:treeName/*', component: TreeRoute },
    { pattern: '/:npub/:treeName', component: TreeRoute },
    { pattern: '/:id/*', component: UserRoute },
    { pattern: '/:id', component: UserRoute },
  ];

  // Check for ?tab=pulls or ?tab=issues query param (NIP-34 git repo views)
  // Also check for ?id= to show individual PR/Issue detail views
  // This allows PR/Issues views without interfering with actual directory names
  function parseNip34Query(fullHash: string): { tab: 'pulls' | 'issues'; id?: string } | null {
    const params = getQueryParamsFromHash(fullHash);
    const tab = params.get('tab');
    if (tab === 'pulls' || tab === 'issues') {
      const id = params.get('id') || undefined;
      return { tab, id };
    }
    return null;
  }

  // Check for ?commit=<hash> query param (commit view)
  function parseCommitQuery(fullHash: string): string | null {
    const params = getQueryParamsFromHash(fullHash);
    return params.get('commit');
  }

  // Check for ?compare=base...head query param (branch comparison view)
  function parseCompareQuery(fullHash: string): { base: string; head: string } | null {
    const params = getQueryParamsFromHash(fullHash);
    const compare = params.get('compare');
    if (!compare || !compare.includes('...')) return null;
    const [base, head] = compare.split('...');
    return base && head ? { base, head } : null;
  }

  // Check for ?merge=1&base=<base>&head=<head> query param (merge view)
  function parseMergeQuery(fullHash: string): { base: string; head: string; prId?: string; prPubkey?: string } | null {
    const params = getQueryParamsFromHash(fullHash);
    if (params.get('merge') !== '1') return null;
    const base = params.get('base');
    const head = params.get('head');
    if (!base || !head) return null;
    const prId = params.get('prId') || undefined;
    const prPubkey = params.get('prPubkey') || undefined;
    return { base, head, prId, prPubkey };
  }

  interface Props {
    currentPath: string;
  }

  let { currentPath }: Props = $props();

  // Subscribe to full hash for query param detection
  let fullHash = $derived($currentFullHash);

  // Check for NIP-34 tab query param (?tab=pulls or ?tab=issues) and optional id
  let nip34Query = $derived(parseNip34Query(fullHash));

  // Check for commit query param (?commit=<hash>)
  let commitHash = $derived(parseCommitQuery(fullHash));

  // Check for branch comparison query param (?compare=base...head)
  let compareQuery = $derived(parseCompareQuery(fullHash));

  // Check for merge query param (?merge=1&base=...&head=...)
  let mergeQuery = $derived(parseMergeQuery(fullHash));

  // Find matching route
  function findRoute(path: string) {
    for (const route of routePatterns) {
      const match = matchRoute(route.pattern, path);
      if (match.matched) {
        return { component: route.component, params: match.params };
      }
    }
    return { component: HomeRoute, params: {} };
  }

  // Derive route from path prop
  let route = $derived.by(() => findRoute(currentPath));

  // For NIP-34/compare/merge views, we need npub and treeName from the current route
  // The repo path is treeName + any wild path
  let repoPath = $derived.by(() => {
    const { treeName, wild } = route.params;
    if (!treeName) return '';
    return wild ? `${treeName}/${wild}` : treeName;
  });
</script>

<div class="flex-1 flex flex-col lg:flex-row min-h-0">
  {#if mergeQuery && route.params.npub && route.params.treeName}
    <MergeView npub={route.params.npub} repoName={repoPath || route.params.treeName} baseBranch={mergeQuery.base} headBranch={mergeQuery.head} prEventId={mergeQuery.prId} prAuthorPubkey={mergeQuery.prPubkey} />
  {:else if compareQuery && route.params.npub && route.params.treeName}
    <BranchCompareView npub={route.params.npub} repoName={repoPath || route.params.treeName} baseBranch={compareQuery.base} headBranch={compareQuery.head} />
  {:else if commitHash && route.params.npub && route.params.treeName}
    <CommitView npub={route.params.npub} repoName={repoPath || route.params.treeName} {commitHash} />
  {:else if nip34Query?.tab === 'pulls' && nip34Query.id && route.params.npub && route.params.treeName}
    <PullRequestDetailView npub={route.params.npub} repoName={repoPath} prId={nip34Query.id} />
  {:else if nip34Query?.tab === 'issues' && nip34Query.id && route.params.npub && route.params.treeName}
    <IssueDetailView npub={route.params.npub} repoName={repoPath} issueId={nip34Query.id} />
  {:else if nip34Query?.tab === 'pulls' && route.params.npub && route.params.treeName}
    <PullRequestsView npub={route.params.npub} repoName={repoPath} />
  {:else if nip34Query?.tab === 'issues' && route.params.npub && route.params.treeName}
    <IssuesView npub={route.params.npub} repoName={repoPath} />
  {:else if route.component === HomeRoute}
    <HomeRoute />
  {:else if route.component === SettingsLayout}
    <SettingsLayout />
  {:else if route.component === WalletPage}
    <WalletPage />
  {:else if route.component === UsersPage}
    <UsersPage />
  {:else if route.component === FollowsPage}
    <FollowsPage npub={route.params.npub} />
  {:else if route.component === FollowersPage}
    <FollowersPage npub={route.params.npub} />
  {:else if route.component === EditProfilePage}
    <EditProfilePage npub={route.params.npub} />
  {:else if route.component === ProfileView}
    <ProfileView npub={route.params.npub || ''} />
  {:else if route.component === TreeRoute}
    <TreeRoute npub={route.params.npub} treeName={route.params.treeName} wild={route.params.wild} />
  {:else if route.component === UserRoute}
    <UserRoute id={route.params.id || route.params.npub} wild={route.params.wild} />
  {:else}
    <HomeRoute />
  {/if}
</div>
