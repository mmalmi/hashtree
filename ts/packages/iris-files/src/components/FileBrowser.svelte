<script lang="ts">
  /**
   * FileBrowser - displays directory contents and tree list
   * Svelte port of the React component
   */
  import { toHex, nhashEncode, LinkType, type TreeEntry as HashTreeEntry } from 'hashtree';
  import { formatBytes } from '../store';
  import { deleteEntry, moveEntry, moveToParent } from '../actions';
  import { open as openCreateModal } from './Modals/CreateModal.svelte';
  import ShareButton from './ShareButton.svelte';
  import { uploadFiles, uploadDirectory } from '../stores/upload';
  import { recentlyChangedFiles } from '../stores/recentlyChanged';
  import { nostrStore, npubToPubkey } from '../nostr';
  import { UserRow, Avatar } from './User';
  import FolderActions from './FolderActions.svelte';
  import VisibilityIcon from './VisibilityIcon.svelte';
  import { TreeRow } from './ui';
  import { treeRootStore, routeStore, createTreesStore, type TreeEntry, currentDirCidStore, isViewingFileStore, resolvingPathStore, directoryEntriesStore } from '../stores';
  import { readFilesFromDataTransfer, hasDirectoryItems } from '../utils/directory';

  import { getFileIcon } from '../utils/fileIcon';
  import { BREAKPOINTS } from '../utils/breakpoints';

  // Build query string from params
  function buildQueryString(params: { k?: string | null; g?: string | null }): string {
    const parts: string[] = [];
    if (params.k) parts.push(`k=${params.k}`);
    if (params.g) parts.push(`g=${encodeURIComponent(params.g)}`);
    return parts.length > 0 ? '?' + parts.join('&') : '';
  }

  // Build href for an entry
  function buildEntryHref(
    entry: { name: string; type: LinkType },
    currentNpub: string | null,
    currentTreeName: string | null,
    currentPath: string[],
    rootCidForHref: { hash: Uint8Array; key?: Uint8Array } | null,
    linkKey: string | null,
    gitRootPath: string | null
  ): string {
    const parts: string[] = [];
    const suffix = buildQueryString({ k: linkKey, g: gitRootPath });

    if (currentNpub && currentTreeName) {
      parts.push(currentNpub, currentTreeName);
      parts.push(...currentPath);
      parts.push(entry.name);
      return '#/' + parts.map(encodeURIComponent).join('/') + suffix;
    } else if (rootCidForHref?.hash) {
      // Include encryption key in nhash if available
      const nhash = nhashEncode({
        hash: toHex(rootCidForHref.hash),
        decryptKey: rootCidForHref.key ? toHex(rootCidForHref.key) : undefined
      });
      parts.push(nhash);
      parts.push(...currentPath);
      parts.push(entry.name);
      return '#/' + parts.map(encodeURIComponent).join('/') + suffix;
    }

    parts.push(...currentPath);
    parts.push(entry.name);
    return '#/' + parts.map(encodeURIComponent).join('/') + suffix;
  }

  // Build href for a tree (root of tree, no path)
  function buildTreeHref(ownerNpub: string, treeName: string, linkKey?: string): string {
    const base = `#/${encodeURIComponent(ownerNpub)}/${encodeURIComponent(treeName)}`;
    return linkKey ? `${base}?k=${linkKey}` : base;
  }

  let isLoggedIn = $derived($nostrStore.isLoggedIn);
  let userNpub = $derived($nostrStore.npub);
  let selectedTree = $derived($nostrStore.selectedTree);
  let route = $derived($routeStore);
  let rootCid = $derived($treeRootStore);
  let currentDirCid = $derived($currentDirCidStore);
  let recentlyChanged = $derived($recentlyChangedFiles);

  let currentNpub = $derived(route.npub);
  let currentTreeName = $derived(route.treeName);
  // Get visibility for current tree (from selectedTree if available)
  // Note: currentTreeVisibility uses effectiveTree which is derived after trees store is created
  // Get directory path (exclude file if URL points to file)
  let urlPath = $derived(route.path);
  let lastSegment = $derived(urlPath.length > 0 ? urlPath[urlPath.length - 1] : null);
  let isViewingFile = $derived($isViewingFileStore);
  let currentPath = $derived(isViewingFile ? urlPath.slice(0, -1) : urlPath);
  let rootHash = $derived(rootCid?.hash ?? null);
  let linkKey = $derived(route.params.get('k'));

  let inTreeView = $derived(!!currentTreeName || !!rootHash);
  let viewedNpub = $derived(currentNpub);
  let isOwnTrees = $derived(!viewedNpub || viewedNpub === userNpub);
  let canEdit = $derived(isOwnTrees || !isLoggedIn);

  // Compute share URL for the current view
  let shareUrl = $derived.by(() => {
    const base = window.location.origin + window.location.pathname + '#';
    const npub = viewedNpub || userNpub;
    let url = base;
    if (npub) {
      url += `/${npub}`;
      if (currentTreeName) {
        url += `/${currentTreeName}`;
        if (urlPath.length > 0) {
          url += '/' + urlPath.join('/');
        }
      }
    } else {
      url += '/';
    }
    if (linkKey) {
      url += `?k=${linkKey}`;
    }
    return url;
  });

  // Git root tracking - from URL or detected from .git directory
  let gitRootFromUrl = $derived(route.params.get('g'));

  // Check if we're missing the decryption key (either no rootCid yet, or rootCid without key)
  let missingDecryptionKey = $derived(!rootCid?.key);

  // Get trees from resolver subscription
  let targetNpub = $derived(viewedNpub || userNpub);

  // Create trees store for the target user
  let treesStore = $derived(createTreesStore(targetNpub));
  let trees = $state<TreeEntry[]>([]);

  // Find the current tree in the trees list (needed for non-owners to get visibility)
  let currentTreeFromList = $derived.by(() => {
    if (!currentTreeName) return null;
    return trees.find(t => t.name === currentTreeName) || null;
  });

  // Get the effective tree info (prefer selectedTree for owner, fall back to list for non-owner)
  let effectiveTree = $derived(isOwnTrees ? selectedTree : currentTreeFromList);

  // Get visibility for current tree
  let currentTreeVisibility = $derived(effectiveTree?.visibility ?? 'public');

  // Check if we're trying to access a protected tree without proper key
  // For non-owners, we need to check if this is a protected tree even before rootCid arrives
  let isProtectedTreeWithoutAccess = $derived(
    !isOwnTrees &&
    missingDecryptionKey &&
    effectiveTree &&
    (effectiveTree.visibility === 'link-visible' || effectiveTree.visibility === 'private')
  );

  // Subscribe to trees store
  $effect(() => {
    const store = treesStore;
    const unsub = store.subscribe(value => {
      trees = value;
    });
    return unsub;
  });

  // Sort trees: public, link, private first, then alphabetically
  let sortedTrees = $derived(
    isOwnTrees
      ? [...trees].sort((a, b) => {
          const defaultFolderOrder = ['public', 'link', 'private'];
          const aIdx = defaultFolderOrder.indexOf(a.name);
          const bIdx = defaultFolderOrder.indexOf(b.name);
          if (aIdx >= 0 && bIdx >= 0) return aIdx - bIdx;
          if (aIdx >= 0) return -1;
          if (bIdx >= 0) return 1;
          return a.name.localeCompare(b.name);
        })
      : trees
  );

  // Directory entries - use global store (shared with Viewer)
  let dirEntries = $derived($directoryEntriesStore);
  let entries = $derived(dirEntries.entries);
  let loadingEntries = $derived(dirEntries.loading);
  let resolvingPath = $derived($resolvingPathStore);

  // Check if current directory has .git folder (making it a git root)
  let hasGitDir = $derived(entries.some((e: HashTreeEntry) => e.name === '.git' && e.type === LinkType.Dir));

  // Calculate effective git root path to propagate to subdirectories
  // If current dir has .git, this becomes the git root (use current path)
  // Otherwise, keep propagating the existing gitRoot from URL
  let effectiveGitRoot = $derived.by(() => {
    if (hasGitDir) {
      // Current directory is a git root - use current path as git root
      // Empty path means tree root, so use empty string
      return currentPath.length > 0 ? currentPath.join('/') : '';
    }
    // Not a git root - keep existing gitRoot from URL (or null)
    return gitRootFromUrl;
  });
  let isDraggingOver = $state(false);
  let fileListRef: HTMLDivElement | undefined = $state();

  // Scroll to top when route changes
  $effect(() => {
    route.path; // track route.path changes
    fileListRef?.scrollTo(0, 0);
  });

  // Handle external file drop
  async function handleFileDrop(e: DragEvent) {
    e.preventDefault();
    isDraggingOver = false;
    if (!canEdit) return;

    const dataTransfer = e.dataTransfer;
    if (!dataTransfer) return;

    if (hasDirectoryItems(dataTransfer) || dataTransfer.items?.length > 0) {
      const result = await readFilesFromDataTransfer(dataTransfer);
      if (result.files.length > 0) {
        await uploadDirectory(result);
        return;
      }
    }

    const files = dataTransfer.files;
    if (files && files.length > 0) {
      await uploadFiles(files);
    }
  }

  function handleFileDragOver(e: DragEvent) {
    if (!canEdit) return;
    if (e.dataTransfer?.types.includes('Files')) {
      e.preventDefault();
      e.dataTransfer.dropEffect = 'copy';
      isDraggingOver = true;
    }
  }

  function handleFileDragLeave(e: DragEvent) {
    const target = e.currentTarget as HTMLElement;
    const rect = target.getBoundingClientRect();
    if (
      e.clientX < rect.left ||
      e.clientX > rect.right ||
      e.clientY < rect.top ||
      e.clientY > rect.bottom
    ) {
      isDraggingOver = false;
    }
  }

  // Internal drag-drop for moving files between folders
  let draggingEntry = $state<string | null>(null);
  let dropTargetDir = $state<string | null>(null);

  function handleEntryDragStart(e: DragEvent, entryName: string) {
    if (!canEdit) {
      e.preventDefault();
      return;
    }
    draggingEntry = entryName;
    e.dataTransfer!.effectAllowed = 'move';
    e.dataTransfer!.setData('text/plain', entryName);
  }

  function handleEntryDragEnd() {
    draggingEntry = null;
    dropTargetDir = null;
  }

  function handleDirDragOver(e: DragEvent, dirName: string) {
    if (!canEdit || !draggingEntry || draggingEntry === dirName) return;
    e.preventDefault();
    e.dataTransfer!.dropEffect = 'move';
    dropTargetDir = dirName;
  }

  function handleDirDragLeave() {
    dropTargetDir = null;
  }

  async function handleDirDrop(e: DragEvent, dirName: string) {
    e.preventDefault();
    e.stopPropagation();
    dropTargetDir = null;

    if (!canEdit || !draggingEntry || draggingEntry === dirName) return;

    await moveEntry(draggingEntry, dirName);
    draggingEntry = null;
  }

  // Handle drop on parent directory (..)
  async function handleParentDrop(e: DragEvent) {
    e.preventDefault();
    e.stopPropagation();
    dropTargetDir = null;

    if (!canEdit || !draggingEntry) return;

    await moveToParent(draggingEntry);
    draggingEntry = null;
  }

  function handleParentDragOver(e: DragEvent) {
    if (!canEdit || !draggingEntry) return;
    e.preventDefault();
    e.dataTransfer!.dropEffect = 'move';
    dropTargetDir = '..';
  }

  function buildDirHref(path: string[]): string {
    const parts: string[] = [];
    const suffix = linkKey ? `?k=${linkKey}` : '';

    if (currentNpub && currentTreeName) {
      parts.push(currentNpub, currentTreeName);
      parts.push(...path);
      return '#/' + parts.map(encodeURIComponent).join('/') + suffix;
    } else if (rootCid?.hash) {
      // Include encryption key in nhash if available
      const nhash = nhashEncode({
        hash: toHex(rootCid.hash),
        decryptKey: rootCid.key ? toHex(rootCid.key) : undefined
      });
      parts.push(nhash);
      parts.push(...path);
      return '#/' + parts.map(encodeURIComponent).join('/') + suffix;
    }

    parts.push(...path);
    return '#/' + parts.map(encodeURIComponent).join('/') + suffix;
  }

  function buildRootHref(): string {
    if (viewedNpub) return `#/${viewedNpub}`;
    return '#/';
  }

  let hasParent = $derived(currentNpub || currentPath.length > 0);
  let currentDirName = $derived(
    currentPath.length > 0
      ? currentPath[currentPath.length - 1]
      : currentTreeName || (rootCid?.hash ? nhashEncode({ hash: toHex(rootCid.hash), decryptKey: rootCid.key ? toHex(rootCid.key) : undefined }).slice(0, 16) + '...' : '')
  );

  // Get file name from URL (last segment if viewing a file)
  let selectedFileName = $derived(isViewingFile && lastSegment ? lastSegment : null);

  // Find selected entry
  let selectedEntry = $derived(selectedFileName ? entries.find(e => e.name === selectedFileName) : null);
  let selectedIndex = $derived(selectedEntry ? entries.findIndex(e => e.name === selectedEntry.name) : -1);

  // Keyboard navigation state
  let focusedIndex = $state(-1);
  let treeFocusedIndex = $state(-1);

  // Detect when only file browser is shown (no side-by-side viewer)
  // Below lg breakpoint, viewer is hidden so arrow keys should just focus
  let isFileBrowserOnly = $state(false);
  $effect(() => {
    const checkLayout = () => {
      isFileBrowserOnly = window.innerWidth < BREAKPOINTS.lg;
    };
    checkLayout();
    window.addEventListener('resize', checkLayout);
    return () => window.removeEventListener('resize', checkLayout);
  });

  // Navigation item counts
  let specialItemCount = $derived((hasParent ? 1 : 0) + 1); // parent? + current
  let navItemCount = $derived(specialItemCount + entries.length);

  // Auto-focus file list when view changes
  $effect(() => {
    // Track dependencies
    void [inTreeView, currentTreeName, currentPath.join('/')];
    // Small delay to ensure DOM is ready after navigation
    const timer = setTimeout(() => {
      fileListRef?.focus();
    }, 50);
    return () => clearTimeout(timer);
  });

  // Keyboard navigation handler for file browser
  function handleKeyDown(e: KeyboardEvent) {
    const key = e.key.toLowerCase();

    // Handle Delete key - delete focused or selected item
    if ((key === 'delete' || key === 'backspace') && canEdit) {
      const entryIndex = focusedIndex - specialItemCount;
      const targetEntry = entryIndex >= 0 ? entries[entryIndex] : selectedEntry;
      if (targetEntry) {
        e.preventDefault();
        if (confirm(`Delete ${targetEntry.name}?`)) {
          deleteEntry(targetEntry.name);
          focusedIndex = -1;
        }
      }
      return;
    }

    // Handle Enter key - navigate to focused item
    if (key === 'enter' && focusedIndex >= 0) {
      e.preventDefault();
      if (hasParent && focusedIndex === 0) {
        // Navigate to parent
        window.location.hash = currentPath.length > 0 ? buildDirHref(currentPath.slice(0, -1)).slice(1) : buildRootHref().slice(1);
        focusedIndex = -1;
      } else if (focusedIndex === (hasParent ? 1 : 0)) {
        // Navigate to current
        window.location.hash = buildDirHref(currentPath).slice(1);
        focusedIndex = -1;
      } else {
        // Navigate to entry
        const entryIndex = focusedIndex - specialItemCount;
        const entry = entries[entryIndex];
        if (entry) {
          const href = buildEntryHref(entry, currentNpub, currentTreeName, currentPath, rootCid, linkKey, effectiveGitRoot);
          window.location.hash = href.slice(1);
          focusedIndex = -1;
        }
      }
      return;
    }

    if (key !== 'arrowup' && key !== 'arrowdown' && key !== 'arrowleft' && key !== 'arrowright' && key !== 'j' && key !== 'k' && key !== 'h' && key !== 'l') return;

    // Don't prevent browser back/forward navigation (Ctrl/Cmd + Arrow)
    if (e.ctrlKey || e.metaKey) return;

    e.preventDefault();

    // Start from focused index, or derive from selected entry
    let currentIndex = focusedIndex;
    if (currentIndex < 0 && selectedIndex >= 0) {
      currentIndex = selectedIndex + specialItemCount;
    }

    let newIndex: number;

    if (key === 'arrowdown' || key === 'arrowright' || key === 'j' || key === 'l') {
      newIndex = currentIndex < navItemCount - 1 ? currentIndex + 1 : 0;
    } else {
      newIndex = currentIndex > 0 ? currentIndex - 1 : navItemCount - 1;
    }

    // Check if it's a special item or entry
    if (hasParent && newIndex === 0) {
      // Parent directory - just focus
      focusedIndex = newIndex;
    } else if (newIndex === (hasParent ? 1 : 0)) {
      // Current directory - just focus
      focusedIndex = newIndex;
    } else {
      // Entry
      const entryIndex = newIndex - specialItemCount;
      const newEntry = entries[entryIndex];
      if (newEntry) {
        if (newEntry.type === LinkType.Dir || isFileBrowserOnly) {
          // Directory or single-pane layout: just focus it, don't navigate
          // When viewer isn't visible, files also just focus - use Enter to navigate
          focusedIndex = newIndex;
        } else {
          // File on desktop with side-by-side layout: navigate to show in viewer
          focusedIndex = -1;
          const href = buildEntryHref(newEntry, currentNpub, currentTreeName, currentPath, rootCid, linkKey, effectiveGitRoot);
          window.location.hash = href.slice(1);
        }
      }
    }
  }

  // Keyboard navigation handler for tree list
  function handleTreeListKeyDown(e: KeyboardEvent) {
    if (sortedTrees.length === 0) return;

    const key = e.key.toLowerCase();

    // Handle Enter key - navigate to focused tree
    if (key === 'enter' && treeFocusedIndex >= 0) {
      e.preventDefault();
      const tree = sortedTrees[treeFocusedIndex];
      if (tree) {
        window.location.hash = buildTreeHref(targetNpub!, tree.name, tree.linkKey).slice(1);
        treeFocusedIndex = -1;
      }
      return;
    }

    if (key !== 'arrowup' && key !== 'arrowdown' && key !== 'j' && key !== 'k') return;

    e.preventDefault();

    // Find currently selected tree index
    const selectedTreeIndex = currentTreeName ? sortedTrees.findIndex(t => t.name === currentTreeName) : -1;
    const currentIndex = treeFocusedIndex >= 0 ? treeFocusedIndex : selectedTreeIndex;
    let newIndex: number;

    if (key === 'arrowdown' || key === 'j') {
      newIndex = currentIndex < sortedTrees.length - 1 ? currentIndex + 1 : 0;
    } else {
      newIndex = currentIndex > 0 ? currentIndex - 1 : sortedTrees.length - 1;
    }

    treeFocusedIndex = newIndex;
  }
</script>

<div class="flex-1 flex flex-col min-h-0 bg-surface-0">
  {#if !inTreeView}
    <!-- Tree list view -->
    <div class="h-10 shrink-0 px-3 border-b border-surface-2 flex items-center gap-2 bg-surface-0">
      {#if viewedNpub}
        <a href="#/{viewedNpub}/profile" class="no-underline min-w-0">
          <UserRow pubkey={npubToPubkey(viewedNpub) || viewedNpub} avatarSize={24} showBadge class="min-w-0" />
        </a>
      {:else if isLoggedIn && userNpub}
        <a href="#/{userNpub}/profile" class="no-underline min-w-0">
          <UserRow pubkey={npubToPubkey(userNpub) || userNpub} avatarSize={24} showBadge class="min-w-0" />
        </a>
      {:else}
        <span class="text-sm text-text-2">Folders</span>
      {/if}
      <div class="ml-auto">
        <ShareButton url={shareUrl} />
      </div>
    </div>

    {#if isOwnTrees}
      <button
        onclick={() => openCreateModal('tree')}
        class="shrink-0 mx-3 mt-3 btn-ghost border border-dashed border-surface-2 flex items-center justify-center gap-2 py-3 text-sm text-text-2 hover:text-text-1 hover:border-accent"
      >
        <span class="i-lucide-folder-plus"></span>
        New Folder
      </button>
    {/if}

    <div
      bind:this={fileListRef}
      data-testid="file-list"
      class="flex-1 overflow-auto pb-4 outline-none"
      tabindex="0"
      role="listbox"
      aria-label="File list"
      onkeydown={handleTreeListKeyDown}
    >
      {#if sortedTrees.length === 0}
        <div class="p-8 text-center text-muted">
          Add files to begin
        </div>
      {:else}
        {#each sortedTrees as tree, idx (tree.name)}
          <TreeRow
            href={buildTreeHref(targetNpub!, tree.name, tree.linkKey)}
            name={tree.name}
            visibility={tree.visibility}
            visibilityPosition="right"
            selected={currentTreeName === tree.name}
            focused={treeFocusedIndex === idx}
          />
        {/each}
      {/if}
    </div>
  {:else if !rootCid && currentTreeName && !isOwnTrees && !isProtectedTreeWithoutAccess}
    <!-- Waiting for non-owned tree root to resolve - show loading state -->
    <div class="flex-1 flex items-center justify-center text-text-3 text-sm">
      <span class="i-lucide-loader-2 animate-spin mr-2"></span>
      Loading...
    </div>
  {:else}
    <!-- File browser view -->
    <!-- Desktop: show user row -->
    {#if viewedNpub}
      <div class="hidden lg:flex h-10 shrink-0 px-3 border-b border-surface-2 items-center gap-2 bg-surface-0">
        <a href="#/{viewedNpub}/profile" class="no-underline min-w-0">
          <UserRow pubkey={npubToPubkey(viewedNpub) || viewedNpub} avatarSize={24} showBadge class="min-w-0" />
        </a>
      </div>
    {/if}
    <!-- Mobile: show rich header with back, avatar, visibility, folder name -->
    <div class="lg:hidden shrink-0 px-3 py-2 border-b border-surface-2 flex items-center gap-2 bg-surface-0">
      {#if hasParent}
        <a href={currentPath.length > 0 ? buildDirHref(currentPath.slice(0, -1)) : buildRootHref()} class="btn-ghost p-1 no-underline" title="Back">
          <span class="i-lucide-chevron-left text-lg"></span>
        </a>
      {/if}
      {#if viewedNpub}
        <a href="#/{viewedNpub}/profile" class="shrink-0">
          <Avatar pubkey={npubToPubkey(viewedNpub) || ''} size={20} />
        </a>
      {/if}
      <VisibilityIcon visibility={currentTreeVisibility} class="text-text-3 shrink-0" />
      <span class="i-lucide-folder-open text-warning shrink-0"></span>
      <span class="font-medium text-text-1 truncate">{currentDirName || currentTreeName}</span>
    </div>

    <!-- Mobile action buttons -->
    {#if currentDirCid || canEdit}
      <div class="lg:hidden px-3 py-2 border-b border-surface-2 bg-surface-0">
        <FolderActions dirCid={currentDirCid} {canEdit} />
      </div>
    {/if}

    <div
      bind:this={fileListRef}
      data-testid="file-list"
      class="flex-1 overflow-auto relative outline-none pb-4 {isDraggingOver ? 'bg-accent/10' : ''}"
      tabindex="0"
      role="listbox"
      aria-label="File list"
      onkeydown={handleKeyDown}
      ondragover={handleFileDragOver}
      ondragleave={handleFileDragLeave}
      ondrop={handleFileDrop}
    >
      {#if isDraggingOver}
        <div class="absolute inset-0 flex items-center justify-center pointer-events-none z-10 border-2 border-dashed border-accent rounded m-2">
          <span class="text-accent font-medium">Drop files to add</span>
        </div>
      {/if}

      {#if resolvingPath}
        <!-- Loading state - show minimal placeholder while resolving path -->
        <div class="p-4"></div>
      {:else}
        <!-- Parent directory row -->
        {#if hasParent}
          <a
            href={currentPath.length > 0 ? buildDirHref(currentPath.slice(0, -1)) : buildRootHref()}
            class="p-3 border-b border-surface-2 flex items-center gap-3 no-underline text-text-1 hover:bg-surface-2/50 {focusedIndex === 0 ? 'ring-2 ring-inset ring-accent' : ''} {dropTargetDir === '..' ? 'bg-accent/20' : ''}"
            ondragover={(e) => handleParentDragOver(e)}
            ondragleave={handleDirDragLeave}
            ondrop={(e) => handleParentDrop(e)}
          >
            <span class="i-lucide-folder text-warning shrink-0"></span>
            <span class="truncate">..</span>
          </a>
        {/if}

        <!-- Current directory row -->
        <a
          href={buildDirHref(currentPath)}
          class="p-3 border-b border-surface-2 flex items-center gap-3 no-underline text-text-1 hover:bg-surface-2/50 {!selectedEntry && focusedIndex < 0 ? 'bg-surface-2' : ''} {focusedIndex === (hasParent ? 1 : 0) ? 'ring-2 ring-inset ring-accent' : ''}"
        >
          <span class="shrink-0 i-lucide-folder-open text-warning"></span>
          <span class="truncate flex-1">{currentDirName}</span>
          {#if currentPath.length === 0}
            {#if route.isPermalink}
              <!-- Permalink view: show link-lock if has key, globe if no key -->
              {#if rootCid?.key}
                <span class="relative inline-block shrink-0 text-text-2" title="Encrypted (has key)">
                  <span class="i-lucide-link"></span>
                  <span class="i-lucide-lock absolute -bottom-0.5 -right-1.5 text-[0.6em]"></span>
                </span>
              {:else}
                <span class="i-lucide-globe text-text-2" title="Public"></span>
              {/if}
            {:else}
              <VisibilityIcon visibility={currentTreeVisibility} class="text-text-2" />
            {/if}
          {/if}
        </a>

        {#if isProtectedTreeWithoutAccess}
          <!-- Protected tree without access - show appropriate message -->
          <div class="p-8 text-center">
            <div class="inline-flex items-center justify-center mb-4">
              {#if effectiveTree?.visibility === 'link-visible'}
                {#if linkKey}
                  <span class="i-lucide-key-round text-3xl text-danger"></span>
                {:else}
                  <span class="relative inline-block shrink-0 text-3xl text-text-3">
                    <span class="i-lucide-link"></span>
                    <span class="i-lucide-lock absolute -bottom-0.5 -right-1.5 text-[0.6em]"></span>
                  </span>
                {/if}
              {:else}
                <span class="i-lucide-lock text-3xl text-text-3"></span>
              {/if}
            </div>
            <div class="text-text-2 font-medium mb-2">
              {#if effectiveTree?.visibility === 'link-visible'}
                {linkKey ? 'Invalid Link Key' : 'Link Required'}
              {:else}
                Private Folder
              {/if}
            </div>
            <div class="text-text-3 text-sm max-w-xs mx-auto">
              {#if effectiveTree?.visibility === 'link-visible'}
                {linkKey
                  ? 'The link key provided is invalid or has expired. Ask the owner for a new link.'
                  : 'This folder requires a special link to access. Ask the owner for the link with the access key.'}
              {:else}
                This folder is private and can only be accessed by its owner.
              {/if}
            </div>
          </div>
        {:else if loadingEntries}
          <!-- Loading entries - show nothing to avoid flash -->
          <div class="p-4 pl-6"></div>
        {:else if entries.length === 0}
          <div class="p-4 pl-6 text-center text-muted text-sm">
            {isDraggingOver ? '' : 'Empty directory'}
          </div>
        {:else}
          {#each entries as entry, idx (entry.name)}
            <a
              href={buildEntryHref(entry, currentNpub, currentTreeName, currentPath, rootCid, linkKey, effectiveGitRoot)}
              class="p-3 pl-9 border-b border-surface-2 flex items-center gap-3 no-underline text-text-1 hover:bg-surface-2/50 {selectedEntry?.name === entry.name && focusedIndex < 0 ? 'bg-surface-2' : ''} {focusedIndex === idx + specialItemCount ? 'ring-2 ring-inset ring-accent' : ''} {recentlyChanged.has(entry.name) && selectedEntry?.name !== entry.name ? 'animate-pulse-live' : ''} {draggingEntry === entry.name ? 'opacity-50' : ''} {dropTargetDir === entry.name ? 'bg-accent/20' : ''}"
              draggable={canEdit}
              ondragstart={(e) => handleEntryDragStart(e, entry.name)}
              ondragend={handleEntryDragEnd}
              ondragover={entry.type === LinkType.Dir ? (e) => handleDirDragOver(e, entry.name) : undefined}
              ondragleave={entry.type === LinkType.Dir ? handleDirDragLeave : undefined}
              ondrop={entry.type === LinkType.Dir ? (e) => handleDirDrop(e, entry.name) : undefined}
            >
              <span class="shrink-0 {entry.type === LinkType.Dir ? 'i-lucide-folder text-warning' : `${getFileIcon(entry.name)} text-text-2`}"></span>
              <span class="truncate flex-1 min-w-0" title={entry.name}>{entry.name}</span>
              <span class="shrink-0 text-muted text-sm min-w-12 text-right">
                {entry.type !== LinkType.Dir && entry.size !== undefined ? formatBytes(entry.size) : ''}
              </span>
            </a>
          {/each}
        {/if}
      {/if}
    </div>
  {/if}
</div>
