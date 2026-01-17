<script lang="ts" module>
  /**
   * Modal for running git commands in a repository
   * Supports both read-only commands (status, log, etc.) and write commands (commit, add, etc.)
   */
  import type { CID } from 'hashtree';

  export interface GitShellTarget {
    dirCid: CID;
    canEdit: boolean;
    onChange?: (newCid: CID) => void;
  }

  let show = $state(false);
  let target = $state<GitShellTarget | null>(null);

  export function open(t: GitShellTarget) {
    target = t;
    show = true;
  }

  export function close() {
    show = false;
    target = null;
  }
</script>

<script lang="ts">
  import { tick } from 'svelte';
  import { getProfileSync } from '../../stores/profile';
  import { runGitCommand, applyGitChanges } from '../../utils/git';
  import { getErrorMessage } from '../../utils/errorMessage';
  import { nostrStore } from '../../nostr';

  interface CommandResult {
    command: string;
    output: string;
    error?: string;
    wasWrite?: boolean;
  }

  let inputValue = $state('');
  let commandHistory = $state<CommandResult[]>([]);
  let isRunning = $state(false);
  let outputContainer: HTMLDivElement | undefined = $state();
  let inputElement: HTMLInputElement | undefined = $state();

  // Track current dirCid (may change after write commands)
  let currentDirCid = $state<CID | null>(null);

  // Reset currentDirCid when target changes
  $effect(() => {
    if (target) {
      currentDirCid = target.dirCid;
    }
  });

  // Handle ESC key
  function handleKeyDown(e: KeyboardEvent) {
    if (e.key === 'Escape') {
      e.preventDefault();
      close();
    }
  }

  $effect(() => {
    if (show) {
      document.addEventListener('keydown', handleKeyDown);
      // Clear history when modal opens
      commandHistory = [];
      void tick().then(() => inputElement?.focus());
      return () => document.removeEventListener('keydown', handleKeyDown);
    }
  });

  // Scroll to bottom when new output is added
  $effect(() => {
    if (outputContainer && commandHistory.length > 0) {
      outputContainer.scrollTop = outputContainer.scrollHeight;
    }
  });

  // Write commands that modify the repository
  const writeCommands = ['add', 'commit', 'reset', 'checkout', 'merge', 'rebase', 'cherry-pick', 'revert', 'tag', 'branch', 'rm', 'mv'];

  function isWriteCommand(command: string): boolean {
    const firstArg = command.trim().split(/\s+/)[0];
    return writeCommands.includes(firstArg);
  }

  // Help text for the git shell
  // Only list commands that are verified to work in our wasm-git integration
  const helpText = `Available commands:

Read-only:
  status      Show working tree status
  log         Show commit logs
  branch      List branches
  rev-parse   Parse revision strings

Write (when editing is enabled):
  add <file>  Add file contents to the index
  add -A      Add all changes
  commit -m   Record changes to the repository
  checkout    Switch branches or restore files

Not supported:
  clone, push, pull, fetch (network commands)
  diff, show, tag, reset, rm, mv (not yet tested)`;

  async function runCommand() {
    if (!target || !currentDirCid || !inputValue.trim() || isRunning) return;

    const command = inputValue.trim();
    inputValue = '';
    isRunning = true;

    // Restore focus immediately after submit (not after command completes)
    // Use setTimeout to ensure the input is re-rendered after isRunning changes
    setTimeout(() => inputElement?.focus(), 0);

    // Handle help command locally
    if (command === 'help' || command === '--help' || command === '-h') {
      commandHistory = [...commandHistory, {
        command,
        output: helpText,
      }];
      isRunning = false;
      return;
    }

    const isWrite = isWriteCommand(command);

    // Check if write command is allowed
    if (isWrite && !target.canEdit) {
      commandHistory = [...commandHistory, {
        command,
        output: '',
        error: 'Write commands are not allowed (read-only mode)',
        wasWrite: true,
      }];
      isRunning = false;
      return;
    }

    try {
      // Get author info from nostr profile for commits
      let authorName = 'User';
      let authorEmail = 'user@example.com';

      if (isWrite) {
        const state = nostrStore.getState();
        if (state.pubkey) {
          const profile = getProfileSync(state.pubkey);
          if (profile) {
            authorName = profile.name || profile.display_name || 'User';
          }
          // Use npub-based email
          authorEmail = `${state.npub?.slice(0, 16) || 'user'}@hashtree.local`;
        }
      }

      const result = await runGitCommand(currentDirCid, command, {
        authorName,
        authorEmail,
      });

      // If write command returned updated .git files, persist them
      if (result.gitFiles && result.gitFiles.length > 0 && target.onChange) {
        const newDirCid = await applyGitChanges(currentDirCid, result.gitFiles);
        currentDirCid = newDirCid;
        target.onChange(newDirCid);

        commandHistory = [...commandHistory, {
          command,
          output: result.output || '(changes saved)',
          error: result.error,
          wasWrite: true,
        }];
      } else {
        commandHistory = [...commandHistory, {
          command,
          output: result.output,
          error: result.error,
          wasWrite: isWrite,
        }];
      }
    } catch (err) {
      commandHistory = [...commandHistory, {
        command,
        output: '',
        error: getErrorMessage(err),
        wasWrite: isWrite,
      }];
    } finally {
      isRunning = false;
    }
  }

  function handleInputKeyDown(e: KeyboardEvent) {
    if (e.key === 'Enter' && !e.shiftKey) {
      e.preventDefault();
      runCommand();
    }
  }

  function clearHistory() {
    commandHistory = [];
  }

  /**
   * Colorize git output for better readability
   */
  function colorizeOutput(output: string, command: string): string {
    const lines = output.split('\n');
    const firstWord = command.trim().split(/\s+/)[0];

    // Escape HTML to prevent XSS
    const escape = (s: string) => s
      .replace(/&/g, '&amp;')
      .replace(/</g, '&lt;')
      .replace(/>/g, '&gt;');

    return lines.map(line => {
      const escaped = escape(line);

      // Diff output coloring
      if (firstWord === 'diff' || firstWord === 'show') {
        if (line.startsWith('+') && !line.startsWith('+++')) {
          return `<span class="text-success">${escaped}</span>`;
        }
        if (line.startsWith('-') && !line.startsWith('---')) {
          return `<span class="text-error">${escaped}</span>`;
        }
        if (line.startsWith('@@')) {
          return `<span class="text-accent">${escaped}</span>`;
        }
        if (line.startsWith('diff --git') || line.startsWith('index ') || line.startsWith('---') || line.startsWith('+++')) {
          return `<span class="text-text-3">${escaped}</span>`;
        }
      }

      // Status output coloring
      if (firstWord === 'status') {
        if (line.includes('modified:') || line.includes('deleted:') || line.includes('renamed:')) {
          return `<span class="text-warning">${escaped}</span>`;
        }
        if (line.includes('new file:')) {
          return `<span class="text-success">${escaped}</span>`;
        }
        if (line.startsWith('\t')) {
          // Untracked files
          return `<span class="text-error">${escaped}</span>`;
        }
      }

      // Log output - commit hashes
      if (firstWord === 'log') {
        if (line.startsWith('commit ')) {
          return `<span class="text-warning">${escaped}</span>`;
        }
        if (line.startsWith('Author:') || line.startsWith('Date:')) {
          return `<span class="text-text-3">${escaped}</span>`;
        }
      }

      // Branch output
      if (firstWord === 'branch') {
        if (line.startsWith('* ')) {
          return `<span class="text-success">${escaped}</span>`;
        }
      }

      return escaped;
    }).join('\n');
  }
</script>

{#if show && target}
  <!-- svelte-ignore a11y_click_events_have_key_events -->
  <!-- svelte-ignore a11y_no_static_element_interactions -->
  <div class="fixed inset-0 z-50 flex items-center justify-center bg-black/50" onclick={close}>
    <div
      class="bg-surface-1 rounded-lg shadow-lg w-full max-w-3xl mx-4 max-h-[80vh] flex flex-col"
      onclick={(e) => e.stopPropagation()}
      data-testid="git-shell-modal"
    >
      <!-- Header -->
      <div class="flex items-center justify-between p-4 b-b-1 b-b-solid b-b-surface-3">
        <h2 class="text-lg font-semibold flex items-center gap-2">
          <span class="i-lucide-terminal"></span>
          Git Shell
        </h2>
        <div class="flex items-center gap-2">
          <button onclick={clearHistory} class="btn-circle btn-ghost" title="Clear output">
            <span class="i-lucide-trash-2 text-lg"></span>
          </button>
          <button onclick={close} class="btn-circle btn-ghost" aria-label="Close">
            <span class="i-lucide-x text-lg"></span>
          </button>
        </div>
      </div>

      <!-- Output area -->
      <div
        bind:this={outputContainer}
        class="flex-1 overflow-auto p-4 font-mono text-sm bg-black/30 min-h-[200px] max-h-[400px]"
      >
        {#if commandHistory.length === 0}
          <div class="text-text-3">
            Type a git command below (e.g., status, log, branch). Type <span class="text-accent">help</span> for available commands.
          </div>
        {:else}
          {#each commandHistory as result, index (index)}
            <div class="mb-4">
              <div class="text-accent flex items-center gap-2">
                <span class="text-text-3">$</span>
                <span>git {result.command}</span>
              </div>
              {#if result.error}
                <pre class="text-error mt-1 whitespace-pre-wrap">{result.error}</pre>
              {:else if result.output}
                <!-- eslint-disable-next-line svelte/no-at-html-tags -- colorizeOutput escapes HTML -->
                <pre class="text-text-2 mt-1 whitespace-pre-wrap">{@html colorizeOutput(result.output, result.command)}</pre>
              {:else}
                <div class="text-text-3 mt-1">(no output)</div>
              {/if}
            </div>
          {/each}
        {/if}
        {#if isRunning}
          <div class="flex items-center gap-2 text-text-3">
            <span class="i-lucide-loader-2 animate-spin"></span>
            Running...
          </div>
        {/if}
      </div>

      <!-- Input area -->
      <div class="p-4 b-t-1 b-t-solid b-t-surface-3">
        <div class="flex items-center gap-2 font-mono">
          <span class="text-text-3">$ git</span>
          <input
            type="text"
            bind:value={inputValue}
            bind:this={inputElement}
            onkeydown={handleInputKeyDown}
            placeholder="status"
            class="flex-1 bg-surface-2 rounded px-3 py-2 text-sm focus:outline-none focus:ring-2 focus:ring-accent"
            disabled={isRunning}
          />
          <button
            onclick={runCommand}
            disabled={isRunning || !inputValue.trim()}
            class="btn-primary px-4 py-2"
          >
            {#if isRunning}
              <span class="i-lucide-loader-2 animate-spin"></span>
            {:else}
              Run
            {/if}
          </button>
        </div>
        <div class="mt-2 text-xs text-text-3">
          {#if target?.canEdit}
            Tip: Write commands (add, commit, etc.) are supported. Network commands (clone, push, pull) are not supported.
          {:else}
            Tip: Read-only mode. Commands like status, log, diff, branch, show work. Network commands are not supported.
          {/if}
        </div>
      </div>
    </div>
  </div>
{/if}
