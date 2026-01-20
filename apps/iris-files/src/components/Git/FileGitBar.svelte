<script lang="ts">
  /**
   * FileGitBar - Shows git commit info for a file being viewed
   * Displays last commit that modified the file, when, and link to history
   */
  import type { CID } from 'hashtree';
  import { getFileLastCommits } from '../../utils/wasmGit';
  import { open as openGitHistoryModal } from '../Modals/GitHistoryModal.svelte';

  interface Props {
    gitRootCid: CID;
    fileName: string;
    subpath?: string;
    canEdit?: boolean;
  }

  let { gitRootCid, fileName, subpath, canEdit = false }: Props = $props();

  // File commit info
  let commitInfo = $state<{ oid: string; message: string; timestamp: number } | null>(null);
  let loading = $state(true);

  // Load commit info for this file
  $effect(() => {
    const cid = gitRootCid;
    const file = fileName;
    const path = subpath;
    loading = true;
    commitInfo = null;

    let cancelled = false;

    (async () => {
      try {
        const result = await getFileLastCommits(cid, [file], path);
        if (!cancelled) {
          commitInfo = result.get(file) || null;
          loading = false;
        }
      } catch (err) {
        console.error('[FileGitBar] Failed to load commit info:', err);
        if (!cancelled) {
          loading = false;
        }
      }
    })();

    return () => { cancelled = true; };
  });

  function formatRelativeTime(timestamp: number): string {
    const now = Math.floor(Date.now() / 1000);
    const diff = now - timestamp;

    if (diff < 60) return `${diff} seconds ago`;
    if (diff < 3600) return `${Math.floor(diff / 60)} minutes ago`;
    if (diff < 86400) return `${Math.floor(diff / 3600)} hours ago`;
    if (diff < 604800) return `${Math.floor(diff / 86400)} days ago`;
    if (diff < 2592000) return `${Math.floor(diff / 604800)} weeks ago`;
    if (diff < 31536000) return `${Math.floor(diff / 2592000)} months ago`;
    return `${Math.floor(diff / 31536000)} years ago`;
  }

  function getCommitTitle(message: string): string {
    const firstLine = message.split('\n')[0];
    return firstLine.length > 60 ? firstLine.slice(0, 57) + '...' : firstLine;
  }

  function openHistory() {
    openGitHistoryModal({ dirCid: gitRootCid, canEdit });
  }
</script>

{#if !loading && commitInfo}
  <div
    class="shrink-0 px-3 py-1.5 bg-surface-1 border-b border-surface-2 flex items-center gap-3 text-sm"
    data-testid="git-file-bar"
  >
    <span class="i-lucide-git-commit text-text-3"></span>
    <span class="text-text-2 truncate flex-1" title={commitInfo.message}>
      {getCommitTitle(commitInfo.message)}
    </span>
    <span class="text-text-3 whitespace-nowrap">
      {formatRelativeTime(commitInfo.timestamp)}
    </span>
    <button
      onclick={openHistory}
      class="btn-ghost text-xs px-2 py-1"
      title="View file history"
    >
      <span class="i-lucide-history text-sm mr-1"></span>
      History
    </button>
  </div>
{:else if loading}
  <div
    class="shrink-0 px-3 py-1.5 bg-surface-1 border-b border-surface-2 flex items-center gap-3 text-sm"
    data-testid="git-file-bar"
  >
    <span class="i-lucide-loader-2 animate-spin text-text-3"></span>
    <span class="text-text-3">Loading commit info...</span>
  </div>
{/if}
