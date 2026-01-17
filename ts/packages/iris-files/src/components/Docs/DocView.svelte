<script lang="ts">
  /**
   * DocView - Document view for docs.iris.to
   * Resolves tree path and renders YjsDocumentEditor
   */
  import { routeStore, treeRootStore, createTreesStore } from '../../stores';
  import { getTree } from '../../store';
  import { type CID, type TreeEntry } from 'hashtree';
  import YjsDocumentEditor from '../Viewer/YjsDocumentEditor.svelte';
  import { nostrStore } from '../../nostr';
  import { nip19 } from 'nostr-tools';
  import { addRecent, updateRecentVisibility } from '../../stores/recents';

  let route = $derived($routeStore);
  let treeRoot = $derived($treeRootStore);

  // Set selectedTree when viewing own document (required for autosave to work)
  function setSelectedTreeIfOwn(npubStr: string, treeNameVal: string) {
    let pubkey: string | null = null;
    try {
      const decoded = nip19.decode(npubStr);
      if (decoded.type === 'npub') {
        pubkey = decoded.data as string;
      }
    } catch {
      return;
    }

    const state = nostrStore.getState();
    if (pubkey && state.isLoggedIn && state.pubkey === pubkey) {
      const currentSelected = state.selectedTree;
      if (!currentSelected || currentSelected.name !== treeNameVal) {
        nostrStore.setSelectedTree({
          id: '',
          name: treeNameVal,
          pubkey,
          rootHash: currentSelected?.rootHash || '',
          rootKey: currentSelected?.rootKey,
          visibility: currentSelected?.visibility ?? 'public',
          created_at: Math.floor(Date.now() / 1000),
        });
      }
    }
  }

  // Set selectedTree when route changes
  $effect(() => {
    const npub = route.npub;
    const treeName = route.treeName;
    if (npub && treeName) {
      setSelectedTreeIfOwn(npub, treeName);
    }
  });

  // Get trees for visibility info
  let treesStore = $derived(createTreesStore(route.npub));
  let trees = $state<Array<{ name: string; visibility?: string }>>([]);

  $effect(() => {
    const store = treesStore;
    const unsub = store.subscribe(value => {
      trees = value;
    });
    return unsub;
  });

  let currentTreeVisibility = $derived(
    route.treeName ? trees.find(t => t.name === route.treeName)?.visibility : undefined
  );

  // Add to recents when viewing a doc
  $effect(() => {
    const npub = route.npub;
    const treeName = route.treeName;
    const linkKey = route.params.get('k');
    if (npub && treeName?.startsWith('docs/')) {
      addRecent({
        type: 'tree',
        label: treeName.slice(5), // Remove 'docs/' prefix for display
        path: `/${npub}/${treeName}`,
        npub,
        treeName,
        linkKey: linkKey ?? undefined,
      });
    }
  });

  // Update visibility when it becomes available
  $effect(() => {
    const npub = route.npub;
    const treeName = route.treeName;
    const visibility = currentTreeVisibility;
    if (npub && treeName?.startsWith('docs/') && visibility) {
      updateRecentVisibility(
        `/${npub}/${treeName}`,
        visibility as 'public' | 'link-visible' | 'private'
      );
    }
  });

  // Resolved directory state
  let dirCid = $state<CID | null>(null);
  let dirName = $state<string>('');
  let entries = $state<TreeEntry[]>([]);
  let loading = $state(true);
  let error = $state<string | null>(null);

  // Track previous path to detect path changes vs root-only changes
  let prevPathKey = '';

  // Check if this is a yjs document (has .yjs file)
  let isYjsDoc = $derived(entries.some(e => e.name === '.yjs'));

  // Resolve path when route or tree root changes
  $effect(() => {
    const root = treeRoot;
    const path = route.path;
    const pathKey = path.join('/');

    if (!root) {
      loading = true;
      return;
    }

    // Only show loading state on initial load or path change, NOT on root-only updates
    // This prevents YjsDocumentEditor from unmounting when tree root updates after save
    const isPathChange = pathKey !== prevPathKey;
    if (isPathChange || !dirCid) {
      loading = true;
    }
    prevPathKey = pathKey;
    error = null;

    const tree = getTree();
    const pathStr = path.join('/');

    (async () => {
      try {
        // If path is empty, we're at the tree root
        let targetCid: CID;
        if (path.length === 0) {
          targetCid = root;
          dirName = route.treeName || 'Document';
        } else {
          const result = await tree.resolvePath(root, pathStr);
          if (!result) {
            error = 'Document not found';
            loading = false;
            return;
          }
          targetCid = result.cid;
          dirName = path[path.length - 1];
        }

        // Check if it's a directory
        const isDir = await tree.isDirectory(targetCid);
        if (!isDir) {
          error = 'Not a document directory';
          loading = false;
          return;
        }

        // List entries
        const dirEntries = await tree.listDirectory(targetCid);
        dirCid = targetCid;
        entries = dirEntries;
        loading = false;
      } catch (e) {
        console.error('[DocView] Error resolving path:', e);
        error = 'Failed to load document';
        loading = false;
      }
    })();
  });
</script>

{#if loading}
  <div class="flex-1 flex items-center justify-center text-text-3">
    <span class="i-lucide-loader-2 animate-spin mr-2"></span>
    Loading document...
  </div>
{:else if error}
  <div class="flex-1 flex flex-col items-center justify-center text-text-3 p-6">
    <span class="i-lucide-file-x text-4xl mb-4"></span>
    <p class="text-lg mb-2">{error}</p>
    <a href="#/" class="text-accent hover:underline">Back to home</a>
  </div>
{:else if isYjsDoc && dirCid}
  <YjsDocumentEditor {dirCid} {dirName} {entries} />
{:else if dirCid}
  <!-- Not a yjs doc - show option to convert or simple view -->
  <div class="flex-1 flex flex-col items-center justify-center text-text-3 p-6">
    <span class="i-lucide-file-question text-4xl mb-4"></span>
    <p class="text-lg mb-2">This is not a collaborative document</p>
    <p class="text-sm mb-4">It doesn't contain a .yjs configuration file</p>
    <a href="#/" class="text-accent hover:underline">Back to home</a>
  </div>
{/if}
