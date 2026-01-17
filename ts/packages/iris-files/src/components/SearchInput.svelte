<script lang="ts">
  import { nhashEncode, isNHash, isNPath } from 'hashtree';
  import { nip19 } from 'nostr-tools';
  import { nostrStore } from '../nostr';
  import { follows } from '../utils/socialGraph';
  import { indexUsers, type UserIndexEntry } from '../stores/searchIndex';
  import { getProfileSync } from '../stores/profile';
  import { UserRow } from './User';
  import { search, getSuggestions, recordHistoryVisit, type SearchResult } from '../lib/search';
  import { isTauri } from '../tauri';

  interface Props {
    fullWidth?: boolean;
    autofocus?: boolean;
    onSelect?: () => void;
    showVideos?: boolean;
    placeholder?: string;
  }

  let { fullWidth = false, autofocus = false, onSelect, showVideos = true, placeholder = 'Search' }: Props = $props();

  let inputRef: HTMLInputElement | undefined = $state();

  // Match 64 hex chars optionally followed by /filename
  const HASH_PATTERN = /^([a-f0-9]{64})(\/.*)?$/i;

  let value = $state('');
  let focused = $state(false);
  let showDropdown = $state(false);
  let selectedIndex = $state(0);
  let containerRef: HTMLDivElement | undefined = $state();
  let searchResults = $state<SearchResult[]>([]);

  let userPubkey = $derived($nostrStore.pubkey);
  let userFollows = $derived(follows(userPubkey));

  // Index followed users when they change (with profile data if available)
  $effect(() => {
    if (!userFollows || userFollows.size === 0) return;

    const entries: UserIndexEntry[] = [];
    for (const pubkey of userFollows) {
      try {
        const npub = nip19.npubEncode(pubkey);
        const profile = getProfileSync(pubkey);
        entries.push({
          pubkey,
          npub,
          name: profile?.name,
          displayName: profile?.display_name,
          nip05: profile?.nip05,
        });
      } catch {
        // Skip invalid pubkeys
      }
    }

    if (entries.length > 0) {
      indexUsers(entries);
    }
  });

  // Debounced search
  let searchTimer: ReturnType<typeof setTimeout> | null = null;
  $effect(() => {
    const query = value.trim();

    // Clear timer on any change
    if (searchTimer) clearTimeout(searchTimer);

    if (query.length < 1) {
      // Show recent history when focused with empty query
      if (focused && isTauri()) {
        getSuggestions(8).then((results) => {
          searchResults = results;
        });
      } else {
        searchResults = [];
      }
      return;
    }

    // Debounce search
    searchTimer = setTimeout(async () => {
      try {
        const sources = showVideos ? undefined : ['history', 'user', 'apps'];
        const results = await search(query, { limit: 10, sources });
        searchResults = results;
      } catch {
        searchResults = [];
      }
    }, 150);

    return () => {
      if (searchTimer) clearTimeout(searchTimer);
    };
  });

  // Reset selection when results change
  $effect(() => {
    searchResults; // depend on results
    selectedIndex = 0;
  });

  // Autofocus when requested
  $effect(() => {
    if (autofocus && inputRef) {
      inputRef.focus();
    }
  });

  // Close dropdown when clicking outside
  $effect(() => {
    const handleClickOutside = (e: MouseEvent) => {
      if (containerRef && !containerRef.contains(e.target as Node)) {
        showDropdown = false;
      }
    };
    document.addEventListener('mousedown', handleClickOutside);
    return () => document.removeEventListener('mousedown', handleClickOutside);
  });

  function navigateTo(input: string): boolean {
    let trimmed = input.trim();

    function success(hash: string) {
      window.location.hash = hash;
      value = '';
      inputRef?.blur();
      return true;
    }

    // Extract hash fragment from full URL and navigate directly
    try {
      const url = new URL(trimmed);
      if (url.hash && url.hash.startsWith('#/')) {
        return success(url.hash);
      }
    } catch {
      // Not a URL
    }

    // Handle raw #/ paths pasted directly
    if (trimmed.startsWith('#/')) {
      return success(trimmed);
    }

    // npub
    if (trimmed.startsWith('npub1') && trimmed.length >= 63) {
      return success(`#/${trimmed}`);
    }

    // nhash or npath
    if (isNHash(trimmed) || isNPath(trimmed)) {
      return success(`#/${trimmed}`);
    }

    // Hex hash with optional path
    const hashMatch = trimmed.match(HASH_PATTERN);
    if (hashMatch) {
      const hash = hashMatch[1];
      const path = hashMatch[2] || '';
      const nhash = nhashEncode(hash);
      return success(`#/${nhash}${path}`);
    }

    // Route path (e.g. npub1.../treename)
    if (trimmed.startsWith('npub1')) {
      return success(`#/${trimmed}`);
    }

    return false;
  }

  function handleSelectResult(result: SearchResult) {
    // Record history visit for the selection
    recordHistoryVisit(
      result.path,
      result.label,
      result.type,
      result.pubkey,
      undefined // treeName would need to be extracted if available
    );

    window.location.hash = `#${result.path}`;
    value = '';
    showDropdown = false;
    inputRef?.blur();
    onSelect?.();
  }

  function handleInput(e: Event) {
    const newValue = (e.target as HTMLInputElement).value.trim();
    if (!navigateTo(newValue)) {
      value = (e.target as HTMLInputElement).value;
      showDropdown = true;
    } else {
      showDropdown = false;
    }
  }

  function handleKeyDown(e: KeyboardEvent) {
    if (showDropdown && searchResults.length > 0) {
      if (e.key === 'ArrowDown') {
        e.preventDefault();
        selectedIndex = (selectedIndex + 1) % searchResults.length;
        return;
      }
      if (e.key === 'ArrowUp') {
        e.preventDefault();
        selectedIndex = (selectedIndex - 1 + searchResults.length) % searchResults.length;
        return;
      }
      if (e.key === 'Enter' && searchResults[selectedIndex]) {
        e.preventDefault();
        handleSelectResult(searchResults[selectedIndex]);
        return;
      }
    }
    if (e.key === 'Enter') {
      navigateTo(value.trim());
      showDropdown = false;
    }
    if (e.key === 'Escape') {
      showDropdown = false;
      inputRef?.blur();
    }
  }

  function getResultIcon(result: SearchResult): string {
    if (result.icon) return result.icon;
    switch (result.type) {
      case 'video': return 'i-lucide-play-circle';
      case 'user': return 'i-lucide-user';
      case 'tree': return 'i-lucide-folder';
      case 'file': return 'i-lucide-file';
      default: return 'i-lucide-clock';
    }
  }
</script>

<div bind:this={containerRef} class="relative w-full {!fullWidth ? 'max-w-lg' : ''}">
  <div class="flex items-center gap-2 px-4 py-2 rounded-full bg-surface-0 b-1 b-solid b-surface-3 transition-colors {focused ? 'b-accent' : ''}">
    <span class="i-lucide-search text-sm text-muted shrink-0"></span>
    <input
      bind:this={inputRef}
      type="text"
      bind:value
      oninput={handleInput}
      onkeydown={handleKeyDown}
      onfocus={() => { focused = true; showDropdown = true; }}
      onblur={() => (focused = false)}
      {placeholder}
      class="bg-transparent border-none outline-none text-base text-text-1 placeholder:text-muted flex-1"
    />
  </div>

  <!-- Search results dropdown -->
  {#if showDropdown && searchResults.length > 0}
    <div class="absolute top-full left-0 right-0 mt-1 bg-surface-1 rounded-lg shadow-lg z-50 max-h-80 overflow-auto">
      {#each searchResults as result, index (result.id)}
        <button
          onclick={() => handleSelectResult(result)}
          onmouseenter={() => (selectedIndex = index)}
          class="w-full text-left flex items-center gap-3 px-3 py-2 {index === selectedIndex ? 'bg-surface-3' : 'hover:bg-surface-2'}"
        >
          {#if result.type === 'user' && result.pubkey}
            <UserRow pubkey={result.pubkey} avatarSize={28} />
          {:else if result.type === 'video' && result.thumbnail}
            <div class="w-16 h-12 shrink-0 overflow-hidden bg-surface-3 rounded flex items-center justify-center">
              <img
                src={result.thumbnail}
                alt=""
                class="w-full h-full object-cover"
                onerror={(e) => { (e.target as HTMLImageElement).style.display = 'none'; }}
              />
            </div>
            <div class="min-w-0 flex-1">
              <div class="text-sm text-text-1 truncate">{result.label}</div>
              {#if result.sublabel}
                <div class="text-xs text-text-3 truncate">{result.sublabel}</div>
              {/if}
            </div>
          {:else}
            <span class="{getResultIcon(result)} text-lg text-text-3 shrink-0"></span>
            <div class="min-w-0 flex-1">
              <div class="text-sm text-text-1 truncate">{result.label}</div>
              {#if result.sublabel}
                <div class="text-xs text-text-3 truncate">{result.sublabel}</div>
              {/if}
            </div>
          {/if}
        </button>
      {/each}
    </div>
  {/if}
</div>
