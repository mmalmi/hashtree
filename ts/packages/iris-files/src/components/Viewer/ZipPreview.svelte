<script lang="ts">
  /**
   * ZIP file preview component
   * Shows contents of a ZIP file and allows extraction to current dir or subdirectory
   */
  import { unzipSync } from 'fflate';
  import { open as openExtractModal, type ArchiveFileInfo } from '../Modals/ExtractModal.svelte';

  interface Props {
    data: Uint8Array;
    filename: string;
    onDownload?: () => void;
  }

  let { data, filename, onDownload }: Props = $props();

  interface ZipEntry {
    name: string;
    size: number;
    isDirectory: boolean;
  }

  function formatSize(bytes: number): string {
    if (bytes < 1024) return `${bytes} B`;
    if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KB`;
    return `${(bytes / (1024 * 1024)).toFixed(1)} MB`;
  }

  function getFileIcon(filename: string): string {
    const ext = filename.split('.').pop()?.toLowerCase() || '';
    switch (ext) {
      case 'jpg': case 'jpeg': case 'png': case 'gif': case 'webp': case 'svg': case 'ico': case 'bmp':
        return 'i-lucide-image';
      case 'mp4': case 'webm': case 'mkv': case 'avi': case 'mov':
        return 'i-lucide-video';
      case 'mp3': case 'wav': case 'ogg': case 'flac': case 'm4a':
        return 'i-lucide-music';
      case 'exe': case 'com': case 'bat':
        return 'i-lucide-terminal';
      case 'js': case 'ts': case 'jsx': case 'tsx': case 'py': case 'rb': case 'go': case 'rs':
      case 'c': case 'cpp': case 'h': case 'java': case 'php': case 'sh': case 'bash':
        return 'i-lucide-file-code';
      case 'json': case 'yaml': case 'yml': case 'toml': case 'xml': case 'ini': case 'env':
        return 'i-lucide-file-json';
      case 'pdf': case 'doc': case 'docx': case 'txt': case 'md': case 'markdown': case 'rst':
        return 'i-lucide-file-text';
      case 'xls': case 'xlsx': case 'csv':
        return 'i-lucide-file-spreadsheet';
      case 'zip': case 'tar': case 'gz': case 'rar': case '7z':
        return 'i-lucide-file-archive';
      case 'html': case 'htm': case 'css': case 'scss': case 'sass': case 'less':
        return 'i-lucide-file-code';
      default:
        return 'i-lucide-file';
    }
  }

  // Parse ZIP contents
  let error = $state<string | null>(null);
  let entries = $state<ZipEntry[]>([]);
  let totalSize = $state(0);
  let unzipped = $state<Record<string, Uint8Array> | null>(null);

  $effect(() => {
    try {
      const result = unzipSync(data);
      const parsedEntries: ZipEntry[] = [];
      let size = 0;

      for (const [name, content] of Object.entries(result)) {
        // Skip Mac OS X metadata
        if (name.startsWith('__MACOSX/') || name.endsWith('.DS_Store')) {
          continue;
        }

        const isDirectory = name.endsWith('/') || content.length === 0;
        if (!isDirectory) {
          parsedEntries.push({
            name: name.replace(/\/$/, ''),
            size: content.length,
            isDirectory: false,
          });
          size += content.length;
        }
      }

      // Sort entries: directories first, then alphabetically
      parsedEntries.sort((a, b) => {
        if (a.isDirectory !== b.isDirectory) {
          return a.isDirectory ? -1 : 1;
        }
        return a.name.localeCompare(b.name);
      });

      entries = parsedEntries;
      totalSize = size;
      unzipped = result;
      error = null;
    } catch {
      error = 'Failed to read ZIP file';
      entries = [];
      totalSize = 0;
      unzipped = null;
    }
  });

  function handleExtract() {
    if (!unzipped) return;

    // Convert to ArchiveFileInfo format (just name and size for display)
    const archiveFiles: ArchiveFileInfo[] = [];
    for (const [name, content] of Object.entries(unzipped)) {
      // Skip Mac OS X metadata and directories
      if (name.startsWith('__MACOSX/') || name.endsWith('.DS_Store')) {
        continue;
      }
      if (name.endsWith('/') || content.length === 0) {
        continue;
      }
      archiveFiles.push({
        name,
        size: content.length,
      });
    }

    // Open extract modal with the archive data
    openExtractModal({ archiveName: filename, files: archiveFiles, archiveData: data, commonRoot: null });
  }
</script>

{#if error}
  <div class="w-full h-full flex flex-col items-center justify-center p-4">
    <span class="i-lucide-file-warning text-4xl text-danger mb-2"></span>
    <span class="text-danger">{error}</span>
    {#if onDownload}
      <button onclick={onDownload} class="btn-ghost mt-4">
        Download file
      </button>
    {/if}
  </div>
{:else}
  <div class="w-full h-full flex flex-col p-4">
    <!-- Header -->
    <div class="flex items-center justify-between mb-4 shrink-0">
      <div class="flex items-center gap-3">
        <span class="i-lucide-file-archive text-2xl text-accent"></span>
        <div>
          <div class="font-medium">{filename}</div>
          <div class="text-sm text-text-2">
            {entries.length} file{entries.length !== 1 ? 's' : ''} ({formatSize(totalSize)})
          </div>
        </div>
      </div>
      <button
        onclick={handleExtract}
        class="btn-success flex items-center gap-2"
        disabled={entries.length === 0}
      >
        <span class="i-lucide-archive"></span>
        Extract
      </button>
    </div>

    <!-- File list -->
    <div class="flex-1 overflow-auto bg-surface-2 rounded-lg border border-surface-3">
      {#if entries.length === 0}
        <div class="flex items-center justify-center h-full text-text-3">
          Empty archive
        </div>
      {:else}
        <div class="divide-y divide-surface-3">
          {#each entries as entry (entry.name)}
            <div class="flex items-center gap-3 px-3 py-2 hover:bg-surface-3/50">
              <span class="{getFileIcon(entry.name)} text-text-2 shrink-0"></span>
              <span class="flex-1 truncate text-sm">{entry.name}</span>
              <span class="text-text-3 text-sm shrink-0">{formatSize(entry.size)}</span>
            </div>
          {/each}
        </div>
      {/if}
    </div>
  </div>
{/if}
