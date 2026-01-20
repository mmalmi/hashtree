<script lang="ts">
  /**
   * FileTable - GitHub-style file listing with commit info
   */
  import { LinkType, type TreeEntry } from 'hashtree';
  import type { Readable } from 'svelte/store';
  import type { CommitInfo } from '../../stores/git';
  import type { CIStatus } from '../../stores/ci';
  import CIStatusBadge from './CIStatusBadge.svelte';
  import { open as openCIRunsModal } from '../Modals/CIRunsModal.svelte';

  interface Props {
    entries: TreeEntry[];
    fileCommits: Map<string, { oid: string; message: string; timestamp: number }>;
    buildEntryHref: (entry: TreeEntry) => string;
    buildCommitHref: (commitOid: string) => string;
    latestCommit?: CommitInfo | null;
    commitsLoading?: boolean;
    /** Optional href for parent directory (..) navigation */
    parentHref?: string | null;
    /** Optional CI status for the current commit */
    ciStatus?: CIStatus | null;
    /** Optional CI status store for live updates */
    ciStatusStore?: Readable<CIStatus> | null;
    /** Repo path for CI modal context */
    repoPath?: string;
  }

  let { entries, fileCommits, buildEntryHref, buildCommitHref, latestCommit = null, commitsLoading = false, parentHref = null, ciStatus = null, ciStatusStore = null, repoPath = '' }: Props = $props();

  // Sort entries: directories first, then files, alphabetically
  let sortedEntries = $derived([...entries].sort((a, b) => {
    const aIsDir = a.type === LinkType.Dir;
    const bIsDir = b.type === LinkType.Dir;
    if (aIsDir && !bIsDir) return -1;
    if (!aIsDir && bIsDir) return 1;
    return a.name.localeCompare(b.name);
  }));

  function getFileIcon(filename: string): string {
    const ext = filename.split('.').pop()?.toLowerCase() || '';
    const iconMap: Record<string, string> = {
      js: 'i-lucide-file-code',
      ts: 'i-lucide-file-code',
      jsx: 'i-lucide-file-code',
      tsx: 'i-lucide-file-code',
      py: 'i-lucide-file-code',
      md: 'i-lucide-file-text',
      txt: 'i-lucide-file-text',
      json: 'i-lucide-file-json',
      png: 'i-lucide-image',
      jpg: 'i-lucide-image',
      gif: 'i-lucide-image',
      svg: 'i-lucide-image',
    };
    return iconMap[ext] || 'i-lucide-file';
  }

  // Format relative time like GitHub
  function formatRelativeTime(timestamp: number): string {
    const now = Math.floor(Date.now() / 1000);
    const diff = now - timestamp;

    if (diff < 60) return 'just now';
    if (diff < 3600) return `${Math.floor(diff / 60)} min ago`;
    if (diff < 86400) return `${Math.floor(diff / 3600)} hours ago`;
    if (diff < 604800) return `${Math.floor(diff / 86400)} days ago`;
    if (diff < 2592000) return `${Math.floor(diff / 604800)} weeks ago`;
    if (diff < 31536000) return `${Math.floor(diff / 2592000)} months ago`;
    return `${Math.floor(diff / 31536000)} years ago`;
  }

  // Get first line of commit message (truncated)
  function getCommitTitle(message: string): string {
    const firstLine = message.split('\n')[0];
    return firstLine.length > 50 ? firstLine.slice(0, 47) + '...' : firstLine;
  }

  function handleCIClick(event: MouseEvent) {
    event.stopPropagation();
    if (!ciStatus || ciStatus.loading || ciStatus.jobs.length === 0) return;
    openCIRunsModal({ status: ciStatus, repoPath, statusStore: ciStatusStore ?? undefined });
  }
</script>

<table class="w-full text-sm border-collapse">
  <!-- Commit info header row - always shown -->
  <thead>
    <tr class="bg-surface-1 b-b-1 b-b-solid b-b-surface-3">
      {#if latestCommit}
        <!-- Commit info -->
        <td class="py-3 px-4 w-10">
          <span class="i-lucide-user-circle text-text-3"></span>
        </td>
        <td class="py-3 px-4 text-text-1 font-medium whitespace-nowrap">
          {latestCommit.author}
        </td>
        <td class="py-3 px-4 truncate max-w-md hidden sm:table-cell" title={latestCommit.message}>
          <a
            href={buildCommitHref(latestCommit.oid)}
            class="text-text-2 hover:text-accent hover:underline no-underline"
            onclick={(e) => e.stopPropagation()}
          >
            {getCommitTitle(latestCommit.message)}
          </a>
        </td>
        <td class="py-3 px-4 text-right whitespace-nowrap flex items-center justify-end gap-2">
          {#if ciStatus?.status}
            <button
              class="btn-circle btn-ghost"
              onclick={handleCIClick}
              title="View CI runs"
            >
              <CIStatusBadge status={ciStatus.status} compact />
            </button>
          {/if}
          <span class="text-text-3">{formatRelativeTime(latestCommit.timestamp)}</span>
        </td>
      {:else if commitsLoading}
        <!-- Loading state -->
        <td class="py-3 px-4 w-10">
          <span class="i-lucide-loader-2 animate-spin text-text-3"></span>
        </td>
        <td class="py-3 px-4 text-text-3" colspan="3">
          Loading commit info...
        </td>
      {:else}
        <!-- No commits yet -->
        <td class="py-3 px-4 w-10">
          <span class="i-lucide-git-commit text-text-3"></span>
        </td>
        <td class="py-3 px-4 text-text-3" colspan="3">
          No commits yet
        </td>
      {/if}
    </tr>
  </thead>
  <tbody>
    {#if parentHref}
      <tr
        onclick={() => window.location.hash = parentHref.slice(1)}
        class="b-b-1 b-b-solid b-b-surface-3 hover:bg-surface-1 cursor-pointer"
      >
        <td class="py-2 px-3 w-8">
          <span class="i-lucide-folder text-warning"></span>
        </td>
        <td class="py-2 px-3 text-accent whitespace-nowrap" colspan="3">
          ..
        </td>
      </tr>
    {/if}
    {#each sortedEntries as entry (entry.name)}
      {@const isGitDir = entry.name === '.git'}
      {@const href = buildEntryHref(entry)}
      {@const commitInfo = fileCommits.get(entry.name)}
      <tr
        onclick={() => window.location.hash = href.slice(1)}
        class="b-b-1 b-b-solid b-b-surface-3 hover:bg-surface-1 cursor-pointer {isGitDir ? 'opacity-50' : ''}"
      >
        <td class="py-2 px-3 w-8">
          <span class="{entry.type === LinkType.Dir ? 'i-lucide-folder text-warning' : `${getFileIcon(entry.name)} text-text-2`}"></span>
        </td>
        <td class="py-2 px-3 {isGitDir ? 'text-text-3' : 'text-accent'} whitespace-nowrap">
          {entry.name}
        </td>
        <td class="py-2 px-3 truncate max-w-xs hidden md:table-cell" title={commitInfo?.message}>
          {#if commitInfo}
            <a
              href={buildCommitHref(commitInfo.oid)}
              class="text-muted hover:text-accent hover:underline no-underline"
              onclick={(e) => e.stopPropagation()}
            >
              {getCommitTitle(commitInfo.message)}
            </a>
          {/if}
        </td>
        <td class="py-2 px-3 text-right text-muted whitespace-nowrap w-24">
          {commitInfo ? formatRelativeTime(commitInfo.timestamp) : ''}
        </td>
      </tr>
    {:else}
      <tr>
        <td colspan="4" class="py-4 px-3 text-center text-muted">
          Empty directory
        </td>
      </tr>
    {/each}
  </tbody>
</table>
