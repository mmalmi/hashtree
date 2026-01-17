<script lang="ts">
  /**
   * Shared folder action buttons - used in FileBrowser and Viewer
   * Port of React FolderActions component
   */
  import { nhashEncode, toHex, LinkType } from 'hashtree';
  import type { CID } from 'hashtree';
  import { SvelteMap } from 'svelte/reactivity';
  import { open as openCreateModal } from './Modals/CreateModal.svelte';
  import { open as openRenameModal } from './Modals/RenameModal.svelte';
  import { open as openForkModal } from './Modals/ForkModal.svelte';
  import ShareButton from './ShareButton.svelte';
  import { open as openBlossomPushModal } from './Modals/BlossomPushModal.svelte';
  import { npubToPubkey } from '../nostr';
  import { uploadFiles, uploadDirectory } from '../stores/upload';
  import { deleteCurrentFolder, buildRouteUrl } from '../actions';
  import { nostrStore, autosaveIfOwn, deleteTree } from '../nostr';
  import { getTree } from '../store';
  import { createZipFromDirectory, downloadBlob } from '../utils/compression';
  import { setUploadProgress } from '../stores/upload';
  import { readFilesFromWebkitDirectory, supportsDirectoryUpload } from '../utils/directory';
  import { routeStore, createTreesStore } from '../stores';
  import { isGitRepo, initGitRepo } from '../utils/git';
  import { getCurrentRootCid } from '../actions/route';

  interface Props {
    dirCid?: CID | null;
    canEdit: boolean;
  }

  let { dirCid = null, canEdit }: Props = $props();

  let isDownloading = $state(false);
  let isInitializingGit = $state(false);
  let isGitRepoCheck = $state<boolean | null>(null);
  let dirInputRef: HTMLInputElement | undefined = $state();

  let hasDirectorySupport = supportsDirectoryUpload();
  let route = $derived($routeStore);
  let userNpub = $derived($nostrStore.npub);
  let userProfile = $derived($nostrStore.profile);

  // Get user's own trees for fork name suggestions
  let ownTreesStore = $derived(createTreesStore(userNpub));
  let ownTrees = $state<Array<{ name: string }>>([]);

  $effect(() => {
    const store = ownTreesStore;
    const unsub = store.subscribe(value => {
      ownTrees = value;
    });
    return unsub;
  });

  let ownTreeNames = $derived(ownTrees.map(t => t.name));

  // Check if directory is already a git repo
  $effect(() => {
    if (!dirCid) {
      isGitRepoCheck = null;
      return;
    }
    let cancelled = false;
    isGitRepo(dirCid).then(result => {
      if (!cancelled) isGitRepoCheck = result;
    });
    return () => { cancelled = true; };
  });

  // Check if we're in a subdirectory (not root)
  let isSubdir = $derived(route.path.length > 0);
  let currentDirName = $derived(isSubdir ? route.path[route.path.length - 1] : null);

  // For fork, use current dir name or tree name as suggestion
  let forkBaseName = $derived(currentDirName || route.treeName || 'folder');

  // Suggest a fork name - use dirName unless it already exists as a top-level tree
  function suggestForkName(dirName: string, existingTreeNames: string[]): string {
    if (!existingTreeNames.includes(dirName)) {
      return dirName;
    }
    // Add suffix to find unique name
    let i = 2;
    while (existingTreeNames.includes(`${dirName}-${i}`)) {
      i++;
    }
    return `${dirName}-${i}`;
  }

  // Handle fork button click
  function handleFork() {
    if (!dirCid) return;
    const suggestedName = suggestForkName(forkBaseName, ownTreeNames);
    openForkModal(dirCid, suggestedName);
  }

  // Handle git init
  async function handleGitInit() {
    if (!dirCid || isInitializingGit || isGitRepoCheck) return;

    isInitializingGit = true;
    try {
      const tree = getTree();
      const authorName = userProfile?.name || 'Anonymous';
      const authorEmail = userProfile?.nip05 || 'anon@hashtree.local';

      // Initialize git repo and get .git files
      const gitFiles = await initGitRepo(dirCid, authorName, authorEmail);

      // Build the .git directory in hashtree
      // First, organize files by directory
      const dirMap = new SvelteMap<string, Array<{ name: string; cid: CID; size: number; type: LinkType }>>();
      dirMap.set('.git', []);

      // Create directory entries
      for (const file of gitFiles) {
        if (file.isDir) {
          dirMap.set(file.name, []);
        }
      }

      // Add files to their parent directories
      for (const file of gitFiles) {
        if (!file.isDir) {
          const { cid, size } = await tree.putFile(file.data);
          const parentDir = file.name.substring(0, file.name.lastIndexOf('/'));
          const fileName = file.name.substring(file.name.lastIndexOf('/') + 1);

          const entries = dirMap.get(parentDir);
          if (entries) {
            entries.push({ name: fileName, cid, size, type: LinkType.Blob });
          }
        }
      }

      // Build directories from deepest to root
      const sortedDirs = Array.from(dirMap.keys())
        .filter(d => d !== '.git')
        .sort((a, b) => b.split('/').length - a.split('/').length);

      for (const dirPath of sortedDirs) {
        const entries = dirMap.get(dirPath) || [];
        const { cid } = await tree.putDirectory(entries);

        const parentDir = dirPath.substring(0, dirPath.lastIndexOf('/'));
        const dirName = dirPath.substring(dirPath.lastIndexOf('/') + 1);

        const parentEntries = dirMap.get(parentDir);
        if (parentEntries) {
          parentEntries.push({ name: dirName, cid, size: 0, type: LinkType.Dir });
        }
      }

      // Build .git directory
      const gitEntries = dirMap.get('.git') || [];
      const { cid: gitDirCid } = await tree.putDirectory(gitEntries);

      // Add .git to current directory
      const treeRootCid = getCurrentRootCid();
      if (!treeRootCid) throw new Error('No tree root');

      const newRootCid = await tree.setEntry(
        treeRootCid,
        route.path,
        '.git',
        gitDirCid,
        0,
        LinkType.Dir
      );

      // Save and publish
      autosaveIfOwn(newRootCid);
      isGitRepoCheck = true;
    } catch (err) {
      console.error('Git init failed:', err);
      alert(`Git init failed: ${err instanceof Error ? err.message : err}`);
    } finally {
      isInitializingGit = false;
    }
  }

  // Handle download as ZIP
  async function handleDownloadZip() {
    if (!dirCid || isDownloading) return;
    isDownloading = true;
    try {
      const tree = getTree();
      const zipData = await createZipFromDirectory(tree, dirCid, forkBaseName, (progress) => {
        setUploadProgress({
          current: progress.current,
          total: progress.total,
          fileName: progress.fileName,
          status: 'zipping',
        });
      });
      setUploadProgress(null);
      const zipName = `${forkBaseName}.zip`;
      downloadBlob(zipData, zipName, 'application/zip');
    } catch (err) {
      console.error('Failed to create ZIP:', err);
      setUploadProgress(null);
      alert('Failed to create ZIP file');
    } finally {
      isDownloading = false;
    }
  }

  // Handle file upload
  function handleFileUpload(e: Event) {
    const input = e.target as HTMLInputElement;
    if (input.files) uploadFiles(input.files);
    input.value = '';
  }

  // Handle directory upload
  function handleDirUpload(e: Event) {
    const input = e.target as HTMLInputElement;
    if (input.files && input.files.length > 0) {
      const result = readFilesFromWebkitDirectory(input.files);
      uploadDirectory(result);
    }
    input.value = '';
  }

  // Build stream URL - goes to directory with ?stream=1
  let streamUrl = $derived.by(() => {
    const streamQueryParams = [
      route.params.get('k') ? `k=${route.params.get('k')}` : '',
      'stream=1',
    ].filter(Boolean).join('&');
    return route.npub && route.treeName
      ? `#/${route.npub}/${route.treeName}${route.path.length ? '/' + route.path.join('/') : ''}?${streamQueryParams}`
      : null;
  });

  let btnClass = 'flex items-center gap-1 px-2 h-7 text-xs lg:px-3 lg:h-9 lg:text-sm';
</script>

{#if dirCid || canEdit}
  <div class="flex flex-row flex-wrap items-center gap-1">
    <!-- Share and permalink first -->
    {#if dirCid?.hash}
      {@const shareUrl = window.location.origin + window.location.pathname + '#' + buildRouteUrl(route.npub, route.treeName, route.path, undefined, route.params.get('k'))}
      <ShareButton url={shareUrl} />
      <a
        href="#/{nhashEncode({ hash: toHex(dirCid.hash), decryptKey: dirCid.key ? toHex(dirCid.key) : undefined })}"
        class="btn-ghost no-underline {btnClass}"
        title={toHex(dirCid.hash)}
        data-testid="permalink-link"
      >
        <span class={dirCid.key ? "i-lucide-lock text-base" : "i-lucide-link text-base"}></span>
        Permalink
      </a>
    {/if}

    <!-- Edit actions -->
    {#if canEdit}
      <label class="btn-success cursor-pointer {btnClass}" title="Add files">
        <span class="i-lucide-plus"></span>
        Add
        <input
          type="file"
          multiple
          onchange={handleFileUpload}
          class="hidden"
        />
      </label>

      {#if hasDirectorySupport}
        <label class="btn-ghost cursor-pointer {btnClass}" title="Add a folder with all its contents">
          <span class="i-lucide-folder-plus"></span>
          Add Folder
          <input
            bind:this={dirInputRef}
            type="file"
            webkitdirectory
            onchange={handleDirUpload}
            class="hidden"
          />
        </label>
      {/if}

      <button onclick={() => openCreateModal('file')} class="btn-ghost {btnClass}" title="New File">
        <span class="i-lucide-file-plus"></span>
        New File
      </button>

      <button onclick={() => openCreateModal('folder')} class="btn-ghost {btnClass}" title="New Folder">
        <span class="i-lucide-folder-plus"></span>
        New Folder
      </button>

      <button onclick={() => openCreateModal('document')} class="btn-ghost {btnClass}" title="New Document">
        <span class="i-lucide-file-text"></span>
        New Document
      </button>

      {#if streamUrl}
        <a href={streamUrl} class="btn-ghost no-underline {btnClass}" title="Stream">
          <span class="i-lucide-video"></span>
          Stream
        </a>
      {/if}

      {#if dirCid && isGitRepoCheck === false}
        <button
          onclick={handleGitInit}
          disabled={isInitializingGit}
          class="btn-ghost {btnClass}"
          title="Initialize git repository"
          data-testid="git-init-btn"
        >
          <span class={isInitializingGit ? "i-lucide-loader-2 animate-spin" : "i-lucide-git-branch"}></span>
          {isInitializingGit ? 'Initializing...' : 'Git Init'}
        </button>
      {/if}

      {#if isSubdir && currentDirName}
        <button onclick={() => openRenameModal(currentDirName!)} class="btn-ghost {btnClass}" title="Rename">
          <span class="i-lucide-pencil"></span>
          Rename
        </button>
      {/if}

      <button
        onclick={async () => {
          if (isSubdir && currentDirName) {
            if (confirm(`Delete folder "${currentDirName}" and all its contents?`)) {
              deleteCurrentFolder();
            }
          } else if (route.treeName) {
            if (confirm(`Delete "${route.treeName}"? This will remove it from your tree list.`)) {
              await deleteTree(route.treeName);
              window.location.hash = '/';
            }
          }
        }}
        class="btn-ghost text-danger {btnClass}"
        title="Delete"
        data-testid="delete-folder-btn"
      >
        <span class="i-lucide-trash-2"></span>
        Delete
      </button>
    {/if}

    <!-- Secondary actions: ZIP, Fork -->
    {#if dirCid}
      <button
        onclick={handleDownloadZip}
        disabled={isDownloading}
        class="btn-ghost {btnClass}"
        title="Download directory as ZIP"
      >
        <span class={isDownloading ? "i-lucide-loader-2 animate-spin" : "i-lucide-archive"}></span>
        {isDownloading ? 'Zipping...' : 'ZIP'}
      </button>
      <button onclick={handleFork} class="btn-ghost {btnClass}" title="Fork as new top-level folder">
        <span class="i-lucide-git-fork"></span>
        Fork
      </button>
      <button
        onclick={() => openBlossomPushModal(dirCid!, forkBaseName, true, route.npub ? npubToPubkey(route.npub) : undefined, route.treeName || undefined)}
        class="btn-ghost {btnClass}"
        title="Push to file servers"
        data-testid="blossom-push-btn"
      >
        <span class="i-lucide-upload-cloud"></span>
        Push
      </button>
    {/if}
  </div>
{/if}
