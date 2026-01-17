<script lang="ts">
  /**
   * TreeRow - reusable row component for displaying trees/folders
   * Used in FileBrowser, RecentsView, FollowsTreesView
   */
  import type { TreeVisibility } from 'hashtree';
  import { Avatar } from '../User';
  import VisibilityIcon from '../VisibilityIcon.svelte';
  import { getFileIcon } from '../../utils/fileIcon';

  interface Props {
    /** URL to navigate to (optional - if not provided, renders as div) */
    href?: string | null;
    /** Display name */
    name: string;
    /** Whether this is a folder (true) or file (false) */
    isFolder?: boolean;
    /** Visibility level */
    visibility?: TreeVisibility | null;
    /** Where to show visibility icon: 'left' (after avatar), 'right' (after name) */
    visibilityPosition?: 'left' | 'right';
    /** Owner's pubkey (shows avatar) */
    ownerPubkey?: string | null;
    /** Show hash icon instead of avatar (for permalinks) */
    showHashIcon?: boolean;
    /** Whether item has encryption key */
    hasKey?: boolean;
    /** File size (for files) */
    size?: string | null;
    /** Time string to display */
    time?: string | null;
    /** Whether this row is selected/active */
    selected?: boolean;
    /** Whether this row is focused (keyboard nav) */
    focused?: boolean;
    /** Whether this row was recently changed */
    recentlyChanged?: boolean;
    /** Disable hover effect */
    noHover?: boolean;
    /** Additional classes */
    class?: string;
  }

  let {
    href = null,
    name,
    isFolder = true,
    visibility = null,
    visibilityPosition = 'left',
    ownerPubkey = null,
    showHashIcon = false,
    hasKey = false,
    size = null,
    time = null,
    selected = false,
    focused = false,
    recentlyChanged = false,
    noHover = false,
    class: className = '',
  }: Props = $props();

  let icon = $derived(isFolder ? 'i-lucide-folder' : getFileIcon(name));
  let iconColor = $derived(isFolder ? 'text-warning' : 'text-text-2');

  let baseClass = $derived(
    `flex items-center gap-2 text-text-1
    ${selected ? 'bg-surface-2' : href && !noHover ? 'hover:bg-surface-2/50' : ''}
    ${focused ? 'ring-2 ring-inset ring-accent' : ''}
    ${recentlyChanged && !selected ? 'animate-pulse-live' : ''}
    ${className}`
  );

  let rowClass = $derived(
    href
      ? `w-full p-3 border-b border-surface-2 no-underline ${baseClass}`
      : `min-w-0 ${baseClass}`
  );
</script>

{#snippet content()}
  {#if ownerPubkey}
    <Avatar pubkey={ownerPubkey} size={20} class="shrink-0" />
  {:else if showHashIcon}
    <span class="i-lucide-hash text-accent shrink-0"></span>
  {/if}

  <!-- Visibility on left (after avatar, before folder icon) -->
  {#if visibilityPosition === 'left'}
    {#if visibility}
      <VisibilityIcon {visibility} class="text-text-3 shrink-0" />
    {:else if showHashIcon}
      {#if hasKey}
        <span class="relative inline-block shrink-0 text-text-3" title="Encrypted">
          <span class="i-lucide-link"></span>
          <span class="i-lucide-lock absolute -bottom-0.5 -right-1.5 text-[0.6em]"></span>
        </span>
      {:else}
        <span class="i-lucide-globe text-text-3 shrink-0" title="Public"></span>
      {/if}
    {/if}
  {/if}

  <span class="{icon} {iconColor} shrink-0"></span>
  <span class="text-sm text-text-1 truncate flex-1 min-w-0">{name}</span>

  <!-- Visibility on right (after name) -->
  {#if visibilityPosition === 'right'}
    {#if visibility}
      <VisibilityIcon {visibility} class="text-text-2 shrink-0" />
    {:else if showHashIcon}
      {#if hasKey}
        <span class="relative inline-block shrink-0 text-text-2" title="Encrypted">
          <span class="i-lucide-link"></span>
          <span class="i-lucide-lock absolute -bottom-0.5 -right-1.5 text-[0.6em]"></span>
        </span>
      {:else}
        <span class="i-lucide-globe text-text-2 shrink-0" title="Public"></span>
      {/if}
    {/if}
  {/if}

  {#if size}
    <span class="text-xs text-text-3 shrink-0">{size}</span>
  {/if}

  {#if time}
    <span class="text-xs text-text-3 shrink-0">{time}</span>
  {/if}
{/snippet}

{#if href}
  <a {href} class={rowClass}>
    {@render content()}
  </a>
{:else}
  <div class={rowClass}>
    {@render content()}
  </div>
{/if}
