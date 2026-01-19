<script lang="ts">
  /**
   * Tab navigation for repository views (Code, Pull Requests, Issues, Releases)
   * Uses query params (?tab=pulls, ?tab=issues, ?tab=releases) to avoid conflicts with directory names
   */
  import { routeStore } from '../../stores';
  interface Props {
    npub: string;
    repoName: string;
    activeTab: 'code' | 'pulls' | 'issues' | 'releases';
  }

  let { npub, repoName, activeTab }: Props = $props();
  let route = $derived($routeStore);

  const tabs = [
    { id: 'code', label: 'Code', icon: 'i-lucide-code', query: '' },
    { id: 'pulls', label: 'Pull Requests', icon: 'i-lucide-git-pull-request', query: '?tab=pulls' },
    { id: 'issues', label: 'Issues', icon: 'i-lucide-circle-dot', query: '?tab=issues' },
    { id: 'releases', label: 'Releases', icon: 'i-lucide-tag', query: '?tab=releases' },
  ] as const;

  function getHref(tab: typeof tabs[number]): string {
    const linkKey = route.params.get('k');
    if (!linkKey) {
      return `#/${npub}/${repoName}${tab.query}`;
    }
    const separator = tab.query ? '&' : '?';
    return `#/${npub}/${repoName}${tab.query}${separator}k=${linkKey}`;
  }
</script>

<div class="flex items-center gap-1 px-4 b-b-1 b-b-solid b-b-surface-3">
  {#each tabs as tab (tab.id)}
    <a
      href={getHref(tab)}
      class="flex items-center gap-2 px-3 py-3 text-sm transition-colors b-b-2 b-b-solid -mb-px no-underline {
        activeTab === tab.id
          ? 'b-b-accent text-text-1 font-medium'
          : 'b-b-transparent text-text-2 hover:text-text-1 hover:b-b-surface-3'
      }"
    >
      <span class="{tab.icon}"></span>
      {tab.label}
    </a>
  {/each}
</div>
