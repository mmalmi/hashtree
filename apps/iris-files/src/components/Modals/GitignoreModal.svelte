<script lang="ts" module>
  /**
   * Modal for handling .gitignore detection in directory uploads
   */

  export interface FileWithPath {
    file: File;
    relativePath: string;
  }

  export interface GitignoreTarget {
    dirName: string;
    includedFiles: FileWithPath[];
    excludedFiles: FileWithPath[];
    onDecision: (useGitignore: boolean, remember: boolean) => void;
  }

  let show = $state(false);
  let target = $state<GitignoreTarget | null>(null);

  export function open(t: GitignoreTarget) {
    target = t;
    show = true;
  }

  export function close() {
    show = false;
    target = null;
  }
</script>

<script lang="ts">
  import { formatBytes } from '../../store';

  let rememberChoice = $state(false);

  // Calculate sizes
  let excludedSize = $derived(
    target ? target.excludedFiles.reduce((sum, f) => sum + f.file.size, 0) : 0
  );
  let includedSize = $derived(
    target ? target.includedFiles.reduce((sum, f) => sum + f.file.size, 0) : 0
  );

  function handleUseGitignore() {
    target?.onDecision(true, rememberChoice);
    close();
  }

  function handleUploadAll() {
    target?.onDecision(false, rememberChoice);
    close();
  }

  function handleClose() {
    target?.onDecision(false, false);
    close();
  }
</script>

{#if show && target}
  <!-- svelte-ignore a11y_click_events_have_key_events -->
  <!-- svelte-ignore a11y_no_static_element_interactions -->
  <div class="fixed inset-0 z-50 flex items-center justify-center bg-black/50" onclick={handleClose}>
    <div class="bg-surface-1 rounded-lg shadow-lg p-6 w-full max-w-md mx-4" onclick={(e) => e.stopPropagation()}>
      <h2 class="text-lg font-semibold mb-4">.gitignore Detected</h2>

      <div class="mb-4">
        <p class="text-text-2 mb-3">
          Found <strong>.gitignore</strong> in <strong>{target.dirName}</strong>.
        </p>

        <!-- Summary of what will happen -->
        <div class="flex flex-col gap-1 mb-3 text-sm">
          <div class="flex justify-between text-text-2">
            <span>Files to upload:</span>
            <span class="text-success">{target.includedFiles.length} files ({formatBytes(includedSize)})</span>
          </div>
          <div class="flex justify-between text-text-3">
            <span>Files to skip:</span>
            <span>{target.excludedFiles.length} files ({formatBytes(excludedSize)})</span>
          </div>
        </div>

        <!-- Show some excluded files -->
        {#if target.excludedFiles.length > 0}
          <div class="mb-3">
            <div class="text-sm text-text-3 mb-1">Ignored files:</div>
            <div class="max-h-30 overflow-y-auto bg-surface-2 rounded p-2 text-sm">
              {#each target.excludedFiles.slice(0, 15) as f (f.relativePath)}
                <div class="flex justify-between py-0.5 text-text-3">
                  <span class="truncate flex-1 mr-2">{f.relativePath}</span>
                  <span>{formatBytes(f.file.size)}</span>
                </div>
              {/each}
              {#if target.excludedFiles.length > 15}
                <div class="text-text-3 py-1">...and {target.excludedFiles.length - 15} more</div>
              {/if}
            </div>
          </div>
        {/if}
      </div>

      <!-- Remember choice checkbox -->
      <label class="flex items-center gap-2 mb-4 cursor-pointer text-sm text-text-2">
        <input
          type="checkbox"
          bind:checked={rememberChoice}
          class="w-4 h-4 accent-accent"
        />
        <span>Remember my choice</span>
      </label>

      <div class="flex gap-2">
        <button onclick={handleUploadAll} class="btn-ghost">
          Upload All
        </button>
        <button onclick={handleUseGitignore} class="btn-success">
          <span class="i-lucide-filter mr-1"></span>
          Skip Ignored
        </button>
      </div>
    </div>
  </div>
{/if}
