<script lang="ts">
  /**
   * BlossomProgress - Shared component for displaying blossom server upload progress
   */
  import type { WorkerBlossomUploadProgress } from '../../workerAdapter';

  interface Props {
    progress: WorkerBlossomUploadProgress | null;
    showChunkCount?: boolean;
  }

  let { progress, showChunkCount = false }: Props = $props();
</script>

{#if progress && progress.servers.length > 0}
  <div class="space-y-1.5 text-xs">
    {#if showChunkCount}
      <div class="text-text-3">
        Uploading to {progress.servers.length} server{progress.servers.length > 1 ? 's' : ''}
        ({progress.processedChunks} chunks processed)
      </div>
    {/if}
    {#each progress.servers as server (server.url)}
      {@const serverTotal = server.uploaded + server.skipped + server.failed}
      {@const pct = progress.processedChunks > 0 ? Math.round((serverTotal / progress.processedChunks) * 100) : 0}
      {@const serverName = new URL(server.url).hostname}
      <div class="flex items-center gap-2">
        <span class="w-24 truncate text-text-3" title={server.url}>{serverName}</span>
        <div class="flex-1 h-1.5 bg-surface-2 rounded-full overflow-hidden">
          <div
            class="h-full transition-all duration-200 {server.failed > 0 ? (server.uploaded > 0 ? 'bg-yellow-500' : 'bg-red-500') : 'bg-green-500'}"
            style="width: {pct}%"
          ></div>
        </div>
        <span class="w-12 text-right text-text-3">{pct}%</span>
        {#if server.failed > 0}
          <span class="text-red-400" title="{server.failed} failed">!</span>
        {/if}
      </div>
    {/each}
  </div>
{/if}
