<script lang="ts">
  /**
   * BatchVideoList - List of videos with checkboxes for batch upload
   */
  import type { YtDlpVideo } from '../../utils/ytdlp';

  interface Props {
    videos: YtDlpVideo[];
    selectedIds: Set<string>;
    currentUploadingId?: string | null;
    disabled?: boolean;
    onToggle: (id: string) => void;
    onSelectAll: () => void;
    onDeselectAll: () => void;
    formatSize: (bytes: number) => string;
  }

  let {
    videos,
    selectedIds,
    currentUploadingId = null,
    disabled = false,
    onToggle,
    onSelectAll,
    onDeselectAll,
    formatSize,
  }: Props = $props();

  let selectedSize = $derived(
    videos
      .filter(v => selectedIds.has(v.id))
      .reduce((sum, v) => sum + (v.videoFile?.size || 0) + (v.infoJson?.size || 0) + (v.thumbnail?.size || 0), 0)
  );

  let allSelected = $derived(videos.length > 0 && selectedIds.size === videos.length);
</script>

<!-- Select all / Deselect all -->
<div class="flex items-center justify-between mb-2">
  <span class="text-sm text-text-2">
    {selectedIds.size} of {videos.length} selected ({formatSize(selectedSize)})
  </span>
  <div class="flex gap-2">
    <button
      type="button"
      class="btn-ghost text-xs px-2 py-1"
      onclick={onSelectAll}
      disabled={disabled || allSelected}
    >
      Select all
    </button>
    <button
      type="button"
      class="btn-ghost text-xs px-2 py-1"
      onclick={onDeselectAll}
      disabled={disabled || selectedIds.size === 0}
    >
      Deselect all
    </button>
  </div>
</div>

<!-- Video list with checkboxes -->
<div class="max-h-64 overflow-auto space-y-1 border border-surface-3 rounded-lg p-2">
  {#each videos as video (video.id)}
    {@const isSelected = selectedIds.has(video.id)}
    {@const isCurrentlyUploading = currentUploadingId === video.id}
    <label class="flex items-center gap-2 text-sm p-1.5 rounded cursor-pointer hover:bg-surface-3 {isCurrentlyUploading ? 'bg-accent/20' : ''} {!isSelected ? 'opacity-50' : ''}">
      <input
        type="checkbox"
        checked={isSelected}
        onchange={() => onToggle(video.id)}
        {disabled}
        class="w-4 h-4 accent-accent"
      />
      <span class="i-lucide-video text-text-3 shrink-0"></span>
      <span class="text-text-2 truncate flex-1">{video.title}</span>
      {#if video.videoFile}
        <span class="text-text-3 text-xs shrink-0">{formatSize(video.videoFile.size)}</span>
      {/if}
    </label>
  {/each}
</div>
