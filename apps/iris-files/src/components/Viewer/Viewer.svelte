<script lang="ts">
  /**
   * Viewer - main file viewer component
   * Port of React Viewer component
   */
  import { toHex, nhashEncode, LinkType } from '@hashtree/core';
  import { SvelteURLSearchParams } from 'svelte/reactivity';
  import { routeStore, treeRootStore, currentDirCidStore, directoryEntriesStore, currentHash, createTreesStore, addRecent, isViewingFileStore, resolvingPathStore, recentlyChangedFiles } from '../../stores';
  import { getTree, decodeAsText, formatBytes } from '../../store';
  import { nostrStore, npubToPubkey } from '../../nostr';
  import { deleteEntry } from '../../actions';
  import { open as openRenameModal } from '../Modals/RenameModal.svelte';
  import { open as openShareModal } from '../Modals/ShareModal.svelte';
  import { open as openBlossomPushModal } from '../Modals/BlossomPushModal.svelte';
  import { getNhashFileUrl } from '../../lib/mediaUrl';
  import { getQueryParamsFromHash } from '../../lib/router.svelte';
  import DirectoryActions from './DirectoryActions.svelte';
  import FileEditor from './FileEditor.svelte';
  import HtmlViewer from './HtmlViewer.svelte';
  import MediaPlayer from './MediaPlayer.svelte';
  import YjsDocumentEditor from './YjsDocumentEditor.svelte';
  import ZipPreview from './ZipPreview.svelte';
  import DosBox from './DosBox.svelte';
  import CodeViewer from './CodeViewer.svelte';
  import { TreeRow } from '../ui';
  import FileGitBar from '../Git/FileGitBar.svelte';

  let route = $derived($routeStore);
  let rootCid = $derived($treeRootStore);
  let currentDirCid = $derived($currentDirCidStore);
  let dirEntries = $derived($directoryEntriesStore);
  let entries = $derived(dirEntries.entries);
  let hash = $derived($currentHash);

  // Check if user can edit (owns the tree or is not viewing another user's tree)
  let userNpub = $derived($nostrStore.npub);
  let isLoggedIn = $derived($nostrStore.isLoggedIn);
  let viewedNpub = $derived(route.npub);
  let canEdit = $derived(!viewedNpub || viewedNpub === userNpub || !isLoggedIn);

  // Get current tree for visibility info
  let targetNpub = $derived(viewedNpub || userNpub);
  let treesStore = $derived(createTreesStore(targetNpub));
  let trees = $derived($treesStore);
  let currentTreeName = $derived(route.treeName);
  let currentTree = $derived(currentTreeName ? trees.find(t => t.name === currentTreeName) : null);

  // Get filename from URL path - uses actual isDirectory check from hashtree
  let urlPath = $derived(route.path);
  let lastSegment = $derived(urlPath.length > 0 ? urlPath[urlPath.length - 1] : null);
  let isViewingFile = $derived($isViewingFileStore);
  let resolvingPath = $derived($resolvingPathStore);
  let hasFile = $derived(isViewingFile && lastSegment);
  let urlFileName = $derived(hasFile ? lastSegment : null);

  // Git repo detection - check if we're viewing a file in a git repo
  // Check for .git in parent directory entries or git root from URL param
  let hasGitDir = $derived(entries.some(e => e.name === '.git' && e.type === LinkType.Dir));
  let gitRootFromUrl = $derived(route.params.get('g'));
  let isInGitRepo = $derived(hasGitDir || gitRootFromUrl !== null);

  // Resolve git root CID
  let gitRootCid = $state<typeof currentDirCid>(null);

  $effect(() => {
    if (hasGitDir && currentDirCid) {
      // We're at the git root - use current directory CID
      gitRootCid = currentDirCid;
    } else if (gitRootFromUrl !== null && rootCid) {
      // We're in a subdirectory - resolve gitRoot path to get CID
      const tree = getTree();
      const pathParts = gitRootFromUrl === '' ? [] : gitRootFromUrl.split('/');

      let cancelled = false;
      (async () => {
        try {
          if (pathParts.length === 0) {
            if (!cancelled) gitRootCid = rootCid;
          } else {
            const result = await tree.resolvePath(rootCid, pathParts.join('/'));
            if (!cancelled && result) {
              gitRootCid = result.cid;
            }
          }
        } catch {
          if (!cancelled) gitRootCid = null;
        }
      })();
      return () => { cancelled = true; };
    } else {
      gitRootCid = null;
    }
  });

  // Subpath for file (path from git root to file's parent directory)
  let fileSubpath = $derived.by(() => {
    if (!isInGitRepo || urlPath.length <= 1) return undefined;

    // Path without the filename (path to parent directory from tree root)
    const parentPath = urlPath.slice(0, -1);

    if (gitRootFromUrl !== null) {
      // We're in a subdirectory - need to subtract the git root path
      const gitRootParts = gitRootFromUrl === '' ? [] : gitRootFromUrl.split('/');
      const subpathParts = parentPath.slice(gitRootParts.length);
      return subpathParts.length > 0 ? subpathParts.join('/') : undefined;
    }

    if (hasGitDir) {
      // We're at the git root - subpath is just the parent path
      return parentPath.length > 0 ? parentPath.join('/') : undefined;
    }

    return undefined;
  });

  // For video streaming: compute effective tree name by absorbing path segments
  // Non-encoded URLs like /#/npub/videos/videoName/video.webm parse as:
  //   treeName='videos', path=['videoName', 'video.webm']
  // But broadcaster publishes to tree 'videos/videoName', so effective tree should be:
  //   effectiveTreeName='videos/videoName', effectivePath='video.webm'
  let effectiveVideoTree = $derived.by(() => {
    if (!currentTreeName || urlPath.length < 2) {
      return { treeName: currentTreeName, path: urlPath.join('/') };
    }
    // Absorb all but the last path segment into the tree name
    const pathWithoutFile = urlPath.slice(0, -1);
    const newTreeName = [currentTreeName, ...pathWithoutFile].join('/');
    const newPath = urlPath[urlPath.length - 1] || '';
    return { treeName: newTreeName, path: newPath };
  });

  // Debug hook for testing - expose effective tree name for video streaming
  $effect(() => {
    if (typeof window !== 'undefined') {
      // eslint-disable-next-line @typescript-eslint/no-explicit-any
      const w = window as any;
      w.__viewerMediaPlayerTreeName = effectiveVideoTree.treeName;
      w.__viewerUrlPath = urlPath;
    }
  });

  // Parse query params from URL hash - use currentHash store for reactivity
  let searchParams = $derived.by(() => {
    return getQueryParamsFromHash(hash);
  });

  let isEditing = $derived(searchParams.get('edit') === '1');

  // Find entry in current entries list, or create synthetic entry for file permalinks
  // Use $state for caching previous entry to avoid flicker during store updates
  let cachedEntry = $state<typeof entries[0] | null>(null);

  // Pure derived that finds entry from current state
  let currentEntry = $derived.by(() => {
    if (!urlFileName) return null;

    // First try to find the file in entries (works for files within directories)
    const fromEntries = entries.find(e => e.name === urlFileName && e.type !== LinkType.Dir);
    if (fromEntries) return fromEntries;

    // For direct file permalinks (no directory listing), the rootCid IS the file's CID
    // Create a synthetic entry since there's no directory listing
    if (route.isPermalink && rootCid && entries.length === 0) {
      return {
        name: urlFileName,
        cid: rootCid,
        size: 0,
        type: LinkType.Blob,
        meta: { synthetic: true },
      };
    }

    return null;
  });

  // Update cache when we have a valid entry, clear when filename changes
  $effect(() => {
    if (currentEntry) {
      cachedEntry = currentEntry;
    } else if (cachedEntry && urlFileName !== cachedEntry.name) {
      // Clear cache when navigating to different file
      cachedEntry = null;
    }
  });

  // Use current entry, or fall back to cached entry during transitions
  let entryFromStore = $derived.by(() => {
    if (currentEntry) return currentEntry;
    // Keep cached entry if filename matches (prevents flicker during loading)
    if (cachedEntry && urlFileName && cachedEntry.name === urlFileName) {
      return cachedEntry;
    }
    return null;
  });

  // Get files only (no directories) for prev/next navigation
  let filesOnly = $derived(entries.filter(e => e.type !== LinkType.Dir));
  let currentFileIndex = $derived(urlFileName ? filesOnly.findIndex(e => e.name === urlFileName) : -1);
  // Wrap around at start/end
  let prevFile = $derived(
    filesOnly.length > 1 && currentFileIndex >= 0
      ? filesOnly[(currentFileIndex - 1 + filesOnly.length) % filesOnly.length]
      : null
  );
  let nextFile = $derived(
    filesOnly.length > 1 && currentFileIndex >= 0
      ? filesOnly[(currentFileIndex + 1) % filesOnly.length]
      : null
  );

  // Navigate to a file in the same directory
  function navigateToFile(fileName: string) {
    const dirPath = route.path.slice(0, -1); // Remove current filename
    const parts: string[] = [];
    if (route.npub && route.treeName) {
      parts.push(route.npub, route.treeName, ...dirPath, fileName);
    }
    const linkKeySuffix = route.params.get('k') ? `?k=${route.params.get('k')}` : '';
    window.location.hash = '/' + parts.map(encodeURIComponent).join('/') + linkKeySuffix;
  }

  // Check if we have a tree context (for showing actions)
  let hasTreeContext = $derived(!!rootCid || !!route.treeName);

  // Check if current directory is a Yjs document (contains .yjs file)
  let isYjsDocument = $derived(entries.some(e => e.name === '.yjs' && e.type !== LinkType.Dir));

  // Get current directory name from path
  let currentDirName = $derived.by(() => {
    const pathSegments = route.path;
    return pathSegments.length > 0 ? pathSegments[pathSegments.length - 1] : route.treeName || 'Document';
  });
  let yjsViewKey = $derived.by(() => {
    if (!route.npub || !route.treeName) return '';
    const pathKey = route.path.join('/');
    const linkKey = route.params.get('k') ?? '';
    return `${route.npub}/${route.treeName}/${pathKey}?k=${linkKey}`;
  });

  // File content state - raw binary data
  let fileData = $state<Uint8Array | null>(null);
  // Decoded text content (null if binary)
  let fileContent = $state<string | null>(null);
  let loading = $state(false);
  // Only show loading indicator after 2 seconds (avoid flash for fast loads)
  let showLoading = $state(false);
  let loadingTimer: ReturnType<typeof setTimeout> | null = null;
  // Track bytes loaded for progress display
  let bytesLoaded = $state(0);
  let fileRetryTimer: ReturnType<typeof setTimeout> | null = null;
  let fileRetryAttempts = 0;
  let forceFileRetry = false;
  let fileRetryToken = $state(0);
  const FILE_RETRY_DELAY_MS = 1500;
  const FILE_RETRY_MAX = 6;

  // Fullscreen mode - check URL param
  let isFullscreen = $derived.by(() => {
    return getQueryParamsFromHash(hash).get('fullscreen') === '1';
  });

  function exitFullscreen() {
    const currentHash = window.location.hash;
    const newHash = currentHash
      .replace(/[?&]fullscreen=1/g, '')
      .replace(/\?$/, '')
      .replace(/\?&/, '?');
    window.location.hash = newHash;
  }

  function toggleFullscreen() {
    if (isFullscreen) {
      exitFullscreen();
    } else {
      // Add fullscreen param
      const currentHash = window.location.hash;
      const hasQuery = currentHash.includes('?');
      window.location.hash = hasQuery ? `${currentHash}&fullscreen=1` : `${currentHash}?fullscreen=1`;
    }
  }

  // MIME type detection
  function getMimeType(filename?: string): string | null {
    if (!filename) return null;
    const ext = filename.split('.').pop()?.toLowerCase();
    const mimeTypes: Record<string, string> = {
      // Images
      png: 'image/png',
      jpg: 'image/jpeg',
      jpeg: 'image/jpeg',
      gif: 'image/gif',
      webp: 'image/webp',
      avif: 'image/avif',
      svg: 'image/svg+xml',
      ico: 'image/x-icon',
      bmp: 'image/bmp',
      // PDF
      pdf: 'application/pdf',
      // Audio
      mp3: 'audio/mpeg',
      wav: 'audio/wav',
      flac: 'audio/flac',
      m4a: 'audio/mp4',
      ogg: 'audio/ogg',
    };
    return ext ? mimeTypes[ext] || null : null;
  }

  // Track previous entry CID to avoid reloading when only dir listing changed
  let prevEntryCidHash: string | null = null;

  // Load file content when entry changes
  $effect(() => {
    fileRetryToken;
    const entry = entryFromStore;
    const entryCidHash = entry?.cid?.hash ? toHex(entry.cid.hash) : null;
    const shouldForce = forceFileRetry;
    forceFileRetry = false;

    // Skip if CID hasn't changed (dir listing updated but file is the same)
    if (!shouldForce && entryCidHash && entryCidHash === prevEntryCidHash) {
      return;
    }
    prevEntryCidHash = entryCidHash;

    // Clear loading timer
    if (loadingTimer) {
      clearTimeout(loadingTimer);
      loadingTimer = null;
    }
    if (fileRetryTimer) {
      clearTimeout(fileRetryTimer);
      fileRetryTimer = null;
    }
    fileRetryAttempts = 0;

    fileData = null;
    fileContent = null;
    loading = false;
    showLoading = false;
    bytesLoaded = 0;

    if (!entry) return;

    // Skip loading for video/audio/image/pdf files - they stream via SW URLs
    // Skip loading for DOS executables - they use their own loader
    if (isVideo || isAudio || isImage || isPdf || isDos) return;

    loading = true;
    let cancelled = false;

    // Show loading indicator only after 2 seconds delay
    loadingTimer = setTimeout(() => {
      if (!cancelled && loading) {
        showLoading = true;
      }
    }, 2000);

    // Use streaming to track progress for large files
    const isPermalink = route.isPermalink;
    (async () => {
      try {
        const tree = getTree();
        const chunks: Uint8Array[] = [];

        for await (const chunk of tree.readFileStream(entry.cid, { prefetch: 5 })) {
          if (cancelled) break;
          chunks.push(chunk);
          bytesLoaded += chunk.length;
        }

        if (cancelled) return;

        // Combine chunks into single array
        const totalLength = chunks.reduce((sum, c) => sum + c.length, 0);
        const data = new Uint8Array(totalLength);
        let offset = 0;
        for (const chunk of chunks) {
          data.set(chunk, offset);
          offset += chunk.length;
        }

        fileData = data;
        // Try to decode as text (for code/text files)
        fileContent = decodeAsText(data);
        // Note: Images, videos, audio, PDFs all use SW URLs (no blob URLs needed)
      } catch {
        // Ignore errors
      } finally {
        loading = false;
        showLoading = false;
        if (loadingTimer) {
          clearTimeout(loadingTimer);
          loadingTimer = null;
        }
        const expectedSize = entry?.size;
        const isSynthetic = entry?.meta && 'synthetic' in entry.meta;
        const shouldRetryEmpty = bytesLoaded === 0 && fileContent === '' && (isSynthetic || expectedSize !== 0);
        if (!cancelled && isPermalink && ((fileData === null && fileContent === null) || shouldRetryEmpty)) {
          scheduleFileRetry();
        }
      }
    })();

    return () => {
      cancelled = true;
      if (loadingTimer) {
        clearTimeout(loadingTimer);
        loadingTimer = null;
      }
    };
  });

  // Track file visits in recents
  $effect(() => {
    if (!urlFileName || !route.npub || !route.treeName) return;

    // Build full path for the file
    const pathParts = route.path.join('/');
    const fullPath = `/${route.npub}/${route.treeName}${pathParts ? '/' + pathParts : ''}`;

    addRecent({
      type: 'file',
      label: urlFileName,
      path: fullPath,
      npub: route.npub,
      treeName: route.treeName,
    });
  });

  function scheduleFileRetry(): void {
    if (fileRetryTimer || fileRetryAttempts >= FILE_RETRY_MAX) return;
    fileRetryAttempts += 1;
    fileRetryTimer = setTimeout(() => {
      fileRetryTimer = null;
      forceFileRetry = true;
      fileRetryToken += 1;
    }, FILE_RETRY_DELAY_MS);
  }

  function exitEditMode() {
    // Remove ?edit=1 from URL
    const hashBase = window.location.hash.split('?')[0];
    const params = new SvelteURLSearchParams(window.location.hash.split('?')[1] || '');
    params.delete('edit');
    const queryString = params.toString();
    window.location.hash = queryString ? `${hashBase}?${queryString}` : hashBase;
  }

  function enterEditMode() {
    // Add ?edit=1 to URL
    const hashBase = window.location.hash.split('?')[0];
    const params = new SvelteURLSearchParams(window.location.hash.split('?')[1] || '');
    params.set('edit', '1');
    window.location.hash = `${hashBase}?${params.toString()}`;
  }

  function handleDelete() {
    if (!entryFromStore) return;
    if (confirm(`Delete ${entryFromStore.name}?`)) {
      deleteEntry(entryFromStore.name);
      // Navigate back to directory
      const dirPath = route.path.slice(0, -1);
      const parts: string[] = [];
      if (route.npub && route.treeName) {
        parts.push(route.npub, route.treeName, ...dirPath);
      }
      const linkKeySuffix = route.params.get('k') ? `?k=${route.params.get('k')}` : '';
      window.location.hash = '#/' + parts.map(encodeURIComponent).join('/') + linkKeySuffix;
    }
  }

  // Keyboard navigation for file viewing
  $effect(() => {
    if (!entryFromStore && !urlFileName) return; // Only when viewing a file
    if (isEditing) return; // Don't navigate when editing
    if (isDos) return; // Don't interfere with DOSBox keyboard input

    const handleKeyDown = (e: KeyboardEvent) => {
      // Don't interfere with browser shortcuts (Cmd/Ctrl + arrows)
      if (e.metaKey || e.ctrlKey) return;
      // Don't interfere when focus is in input/textarea/canvas
      const target = e.target as HTMLElement;
      if (target.tagName === 'INPUT' || target.tagName === 'TEXTAREA' || target.tagName === 'CANVAS' || target.isContentEditable) return;

      const key = e.key.toLowerCase();

      // Escape: exit fullscreen first, or back to directory
      if (key === 'escape') {
        e.preventDefault();
        if (isFullscreen) {
          exitFullscreen();
        } else {
          (document.activeElement as HTMLElement)?.blur();
          window.location.hash = backUrl.slice(1); // Remove leading #
        }
        return;
      }

      // j/k/ArrowDown/ArrowUp: next/prev file (vertical navigation)
      // l/ArrowRight: next file (horizontal navigation)
      if ((key === 'j' || key === 'arrowdown' || key === 'l' || key === 'arrowright') && nextFile) {
        e.preventDefault();
        navigateToFile(nextFile.name);
        return;
      }

      // k/ArrowUp/h/ArrowLeft: prev file
      if ((key === 'k' || key === 'arrowup' || key === 'h' || key === 'arrowleft') && prevFile) {
        e.preventDefault();
        navigateToFile(prevFile.name);
        return;
      }
    };

    document.addEventListener('keydown', handleKeyDown);
    return () => document.removeEventListener('keydown', handleKeyDown);
  });

  // Check if file looks like text based on extension
  function isLikelyTextFile(filename: string): boolean {
    const textExtensions = ['txt', 'md', 'json', 'js', 'ts', 'jsx', 'tsx', 'css', 'scss', 'html', 'xml', 'yaml', 'yml', 'toml', 'ini', 'cfg', 'conf', 'sh', 'bash', 'py', 'rb', 'go', 'rs', 'c', 'cpp', 'h', 'hpp', 'java', 'php', 'sql', 'svelte', 'vue'];
    const ext = filename.split('.').pop()?.toLowerCase() || '';
    return textExtensions.includes(ext);
  }

  let isTextFile = $derived(urlFileName ? isLikelyTextFile(urlFileName) : false);

  // Check if file is HTML (should be rendered in iframe)
  function isHtmlFile(filename: string): boolean {
    const ext = filename.split('.').pop()?.toLowerCase() || '';
    return ext === 'html' || ext === 'htm';
  }

  let isHtml = $derived(urlFileName ? isHtmlFile(urlFileName) : false);

  // Check if file is a video
  function isVideoFile(filename: string): boolean {
    const videoExtensions = ['mp4', 'webm', 'ogg', 'ogv', 'mov', 'avi', 'mkv'];
    const ext = filename.split('.').pop()?.toLowerCase() || '';
    return videoExtensions.includes(ext);
  }

  let isVideo = $derived(urlFileName ? isVideoFile(urlFileName) : false);

  // Check if file is an image
  function isImageFile(filename: string): boolean {
    const imageExtensions = ['png', 'jpg', 'jpeg', 'gif', 'webp', 'avif', 'svg', 'ico', 'bmp'];
    const ext = filename.split('.').pop()?.toLowerCase() || '';
    return imageExtensions.includes(ext);
  }

  let isImage = $derived(urlFileName ? isImageFile(urlFileName) : false);

  // Check if file is audio
  function isAudioFile(filename: string): boolean {
    const audioExtensions = ['mp3', 'wav', 'flac', 'm4a', 'ogg', 'aac'];
    const ext = filename.split('.').pop()?.toLowerCase() || '';
    return audioExtensions.includes(ext);
  }

  let isAudio = $derived(urlFileName ? isAudioFile(urlFileName) : false);

  // Check if file is PDF
  function isPdfFile(filename: string): boolean {
    const ext = filename.split('.').pop()?.toLowerCase() || '';
    return ext === 'pdf';
  }

  let isPdf = $derived(urlFileName ? isPdfFile(urlFileName) : false);

  // Check if file is ZIP archive
  function isZipFile(filename: string): boolean {
    const ext = filename.split('.').pop()?.toLowerCase() || '';
    return ext === 'zip';
  }

  let isZip = $derived(urlFileName ? isZipFile(urlFileName) : false);

  // Check if file is DOS executable
  function isDosExecutable(filename: string): boolean {
    const ext = filename.split('.').pop()?.toLowerCase() || '';
    return ext === 'exe' || ext === 'com' || ext === 'bat';
  }

  let isDos = $derived(urlFileName ? isDosExecutable(urlFileName) : false);

  // Check if file is a live stream
  // Shows LIVE indicator when:
  // 1. URL has ?live=1 param, OR
  // 2. File is in recentlyChangedFiles (being actively updated)
  let recentlyChanged = $derived($recentlyChangedFiles);
  let isLiveStream = $derived.by(() => {
    // Check URL param first
    if (getQueryParamsFromHash(window.location.hash).get('live') === '1') return true;

    // Show LIVE for files that are actively being updated
    if (urlFileName && recentlyChanged.has(urlFileName)) {
      return true;
    }

    return false;
  });

  // Build permalink URL for the current file
  let permalinkUrl = $derived.by(() => {
    if (!entryFromStore?.cid?.hash) return null;
    const hashHex = toHex(entryFromStore.cid.hash);
    const keyHex = entryFromStore.cid.key ? toHex(entryFromStore.cid.key) : undefined;
    const nhash = nhashEncode({ hash: hashHex, decryptKey: keyHex });
    return `#/${nhash}/${encodeURIComponent(entryFromStore.name)}`;
  });

  // Stable key based on CID - prevents re-render when dir updates but file hasn't changed
  let cidKey = $derived(entryFromStore?.cid?.hash ? toHex(entryFromStore.cid.hash) : urlFileName);

  // Build back URL (directory without file)
  let backUrl = $derived.by(() => {
    const dirPath = route.path.slice(0, -1);
    const parts: string[] = [];
    if (route.npub && route.treeName) {
      parts.push(route.npub, route.treeName, ...dirPath);
    }
    const linkKeySuffix = route.params.get('k') ? `?k=${route.params.get('k')}` : '';
    return '#/' + parts.map(encodeURIComponent).join('/') + linkKeySuffix;
  });

  // Get file icon based on extension
  // eslint-disable-next-line @typescript-eslint/no-unused-vars
  function _getFileIcon(_filename: string): string {
    const ext = filename.split('.').pop()?.toLowerCase() || '';
    const iconMap: Record<string, string> = {
      // Images
      png: 'i-lucide-image',
      jpg: 'i-lucide-image',
      jpeg: 'i-lucide-image',
      gif: 'i-lucide-image',
      webp: 'i-lucide-image',
      svg: 'i-lucide-image',
      // Video
      mp4: 'i-lucide-video',
      webm: 'i-lucide-video',
      mov: 'i-lucide-video',
      // Audio
      mp3: 'i-lucide-music',
      wav: 'i-lucide-music',
      flac: 'i-lucide-music',
      // Code
      js: 'i-lucide-file-code',
      ts: 'i-lucide-file-code',
      jsx: 'i-lucide-file-code',
      tsx: 'i-lucide-file-code',
      py: 'i-lucide-file-code',
      rs: 'i-lucide-file-code',
      go: 'i-lucide-file-code',
      // Documents
      md: 'i-lucide-file-text',
      txt: 'i-lucide-file-text',
      pdf: 'i-lucide-file-text',
      // Archive
      zip: 'i-lucide-archive',
      tar: 'i-lucide-archive',
      gz: 'i-lucide-archive',
    };
    return iconMap[ext] || 'i-lucide-file';
  }

  // Download handler - uses streaming when File System Access API is available
  async function handleDownload() {
    if (!entryFromStore) return;

    const tree = getTree();
    const mimeType = getMimeType(urlFileName || '') || 'application/octet-stream';
    const fileName = entryFromStore.name;

    // Try streaming download with File System Access API (Chrome/Edge)
    if (window.showSaveFilePicker) {
      try {
        const handle = await window.showSaveFilePicker({
          suggestedName: fileName,
          types: [{
            description: 'File',
            accept: { [mimeType]: ['.' + (fileName.split('.').pop() || '')] },
          }],
        });
        const writable = await handle.createWritable();

        // Stream from hashtree directly to file
        for await (const chunk of tree.readFileStream(entryFromStore.cid, { prefetch: 5 })) {
          await writable.write(chunk);
        }
        await writable.close();
        return;
      } catch (err: unknown) {
        // User cancelled or API failed - fall back to blob method
        if (err instanceof Error && err.name === 'AbortError') return;
        console.warn('File System Access API failed, falling back to blob:', err);
      }
    }

    // Fallback: navigate to SW URL with ?download=1 query param
    // SW will serve with Content-Disposition: attachment header
    const baseUrl = getNhashFileUrl(entryFromStore.cid, fileName);
    const separator = baseUrl.includes('?') ? '&' : '?';
    window.location.href = `${baseUrl}${separator}download=1`;
  }

  // Share handler
  function handleShare() {
    // Strip ?edit=1 from URL when sharing
    let url = window.location.href;
    url = url.replace(/[?&]edit=1/, '');
    openShareModal(url);
  }
</script>

{#if urlFileName && isEditing}
  <!-- Edit mode -->
  {#if entryFromStore && loading}
    <!-- Existing file still loading - show spinner -->
    <div class="flex-1 flex items-center justify-center">
      <span class="i-lucide-loader-2 animate-spin text-2xl text-text-3"></span>
    </div>
  {:else}
    <!-- File loaded, or new file (no entry) -->
    <FileEditor
      fileName={urlFileName}
      initialContent={fileContent || ''}
      onDone={exitEditMode}
    />
  {/if}
{:else if urlFileName && entryFromStore}
  <!-- File view - show content -->
  <div class="flex-1 flex flex-col min-h-0 bg-surface-0">
    <!-- Header - hidden in fullscreen -->
    {#if !isFullscreen}
    <div class="shrink-0 px-3 py-2 border-b border-surface-2 flex flex-wrap items-center justify-between gap-2" data-testid="viewer-header">
      <div class="flex items-center gap-2 min-w-0">
        <a href={backUrl} class="btn-ghost p-1 no-underline" title="Back to folder" data-testid="viewer-back">
          <span class="i-lucide-chevron-left text-lg"></span>
        </a>
        <TreeRow
          name={entryFromStore.name}
          isFolder={false}
          ownerPubkey={viewedNpub ? npubToPubkey(viewedNpub) : null}
          showHashIcon={route.isPermalink && !viewedNpub}
          visibility={currentTree?.visibility}
          hasKey={!!rootCid?.key}
        />
        {#if isLiveStream}
          <span class="ml-2 px-1.5 py-0.5 text-xs font-bold bg-red-600 text-white rounded animate-pulse">LIVE</span>
        {/if}
      </div>
      <div class="flex items-center gap-1 flex-wrap">
        <button onclick={handleDownload} class="btn-ghost" title="Download file" data-testid="viewer-download" disabled={loading && !isVideo}>
          Download
        </button>
        {#if permalinkUrl}
          <a href={permalinkUrl} class="btn-ghost no-underline" title={entryFromStore?.cid?.hash ? toHex(entryFromStore.cid.hash) : ''} data-testid="viewer-permalink">
            Permalink
          </a>
        {/if}
        <button onclick={toggleFullscreen} class="btn-ghost" title={isFullscreen ? "Exit fullscreen" : "Fullscreen"} data-testid="viewer-fullscreen">
          <span class={isFullscreen ? "i-lucide-minimize text-base" : "i-lucide-maximize text-base"}></span>
        </button>
        <button onclick={handleShare} class="btn-ghost" title="Share" data-testid="viewer-share">
          <span class="i-lucide-share text-base"></span>
        </button>
        {#if entryFromStore?.cid}
          <button
            onclick={() => openBlossomPushModal(entryFromStore.cid, entryFromStore.name, false, route.npub ? npubToPubkey(route.npub) : undefined, route.treeName)}
            class="btn-ghost"
            title="Push to file servers"
            data-testid="viewer-push"
          >
            <span class="i-lucide-upload-cloud text-base"></span>
          </button>
        {/if}
        {#if canEdit}
          <button onclick={() => openRenameModal(entryFromStore.name)} class="btn-ghost" data-testid="viewer-rename">Rename</button>
          {#if isTextFile || isHtml}
            <button
              onclick={enterEditMode}
              class="btn-ghost"
              disabled={loading || fileContent === null}
              data-testid="viewer-edit"
            >
              Edit
            </button>
          {/if}
          <button onclick={handleDelete} class="btn-ghost text-danger" data-testid="viewer-delete">Delete</button>
        {/if}
        <!-- Prev/Next file navigation - mobile only -->
        {#if filesOnly.length > 1 && prevFile && nextFile}
          <button
            onclick={() => navigateToFile(prevFile.name)}
            class="btn-ghost lg:hidden"
            title={`Previous: ${prevFile.name}`}
          >
            <span class="i-lucide-chevron-left text-base"></span>
          </button>
          <button
            onclick={() => navigateToFile(nextFile.name)}
            class="btn-ghost lg:hidden"
            title={`Next: ${nextFile.name}`}
          >
            <span class="i-lucide-chevron-right text-base"></span>
          </button>
        {/if}
      </div>
    </div>
    {/if}

    <!-- Git file bar - shows commit info when viewing a file in a git repo -->
    {#if isInGitRepo && gitRootCid && urlFileName && !isFullscreen}
      <FileGitBar
        {gitRootCid}
        fileName={urlFileName}
        subpath={fileSubpath}
        {canEdit}
      />
    {/if}

    <!-- Content -->
    {#if isVideo && entryFromStore?.cid}
      <!-- Key by filename to prevent remount on CID change during live streaming -->
      {#key urlFileName}
        <MediaPlayer
          cid={entryFromStore.cid}
          fileName={urlFileName}
          type="video"
          npub={targetNpub}
          treeName={effectiveVideoTree.treeName}
          path={effectiveVideoTree.path}
        />
      {/key}
    {:else if isHtml && fileContent !== null}
      {#key cidKey}
        <HtmlViewer content={fileContent} fileName={urlFileName} />
      {/key}
    {:else if isImage && entryFromStore?.cid}
      <!-- Image viewer - uses SW URL for caching, keyed by CID -->
      {#key cidKey}
        {@const imageUrl = getNhashFileUrl(entryFromStore.cid, urlFileName || 'image')}
        <div class="flex-1 flex items-center justify-center overflow-auto bg-surface-0 p-4">
          {#if isFullscreen}
            <img
              src={imageUrl}
              alt={urlFileName}
              class="max-w-full max-h-full object-contain"
              data-testid="image-viewer"
            />
          {:else}
            <!-- Height-limited view, click to expand -->
            <button
              onclick={toggleFullscreen}
              class="cursor-zoom-in bg-transparent border-none p-0"
              title="Click to view full size"
            >
              <img
                src={imageUrl}
                alt={urlFileName}
                class="max-w-full object-contain"
                style="max-height: calc(100vh - 200px);"
                data-testid="image-viewer"
              />
            </button>
          {/if}
        </div>
      {/key}
    {:else if isAudio && entryFromStore?.cid}
      <!-- Audio player - keyed by filename for live streaming support -->
      {#key urlFileName}
        <MediaPlayer
          cid={entryFromStore.cid}
          fileName={urlFileName}
          type="audio"
          npub={targetNpub}
          treeName={effectiveVideoTree.treeName}
          path={effectiveVideoTree.path}
        />
      {/key}
    {:else if isPdf && entryFromStore?.cid}
      <!-- PDF viewer - uses SW URL for caching, keyed by CID -->
      {#key cidKey}
        <iframe
          src={getNhashFileUrl(entryFromStore.cid, urlFileName || 'document.pdf')}
          class="flex-1 w-full border-none"
          title={urlFileName}
        ></iframe>
      {/key}
    {:else if isZip && fileData}
      <!-- ZIP preview - keyed by CID -->
      {#key cidKey}
        <ZipPreview data={fileData} filename={urlFileName} onDownload={handleDownload} />
      {/key}
    {:else if isDos && currentDirCid}
      <!-- DOS executable - keyed by filename since dirCid changes on file updates -->
      {#key urlFileName}
        <DosBox directoryCid={currentDirCid} exeName={urlFileName} />
      {/key}
    {:else}
      <!-- Text/binary fallback - keyed by CID -->
      {#key cidKey}
        <div class="flex-1 overflow-auto p-4 b-1 b-solid b-transparent">
          {#if showLoading}
            <div class="text-muted animate-fade-in flex flex-col items-start gap-1" data-testid="loading-indicator">
              <span>Loading...</span>
              {#if bytesLoaded > 0}
                <span class="text-sm opacity-70">
                  {bytesLoaded < 1024 * 1024
                    ? `${Math.round(bytesLoaded / 1024)}KB`
                    : `${(bytesLoaded / (1024 * 1024)).toFixed(1)}MB`}
                </span>
              {/if}
            </div>
          {:else if fileContent !== null && urlFileName}
            <CodeViewer content={fileContent} filename={urlFileName} />
          {:else if !loading && entryFromStore}
            <!-- Binary/unsupported format fallback - show download pane -->
            <!-- svelte-ignore a11y_click_events_have_key_events -->
            <!-- svelte-ignore a11y_no_static_element_interactions -->
            <div class="w-full h-full p-3">
              <div
                class="w-full h-full flex flex-col items-center justify-center text-accent cursor-pointer hover:bg-accent/10 transition-colors border border-accent/50 rounded-lg"
                onclick={handleDownload}
              >
                <span class="i-lucide-download text-4xl mb-2"></span>
                <span class="text-sm mb-1">{urlFileName}</span>
                {#if entryFromStore.size}
                  <span class="text-xs text-text-2">{formatBytes(entryFromStore.size)}</span>
                {/if}
              </div>
            </div>
          {/if}
        </div>
      {/key}
    {/if}
  </div>
{:else if hasTreeContext && isYjsDocument && currentDirCid}
  <!-- Yjs Document view - show Tiptap editor -->
  {#key yjsViewKey}
    <YjsDocumentEditor
      dirCid={currentDirCid}
      dirName={currentDirName}
      entries={entries}
    />
  {/key}
{:else if hasTreeContext && !resolvingPath}
  <!-- Directory view - show DirectoryActions -->
  <div class="flex-1 flex flex-col min-h-0 bg-surface-0">
    <DirectoryActions />
  </div>
{:else if resolvingPath}
  <!-- Resolving path - show empty placeholder to avoid flash of wrong content -->
  <div class="flex-1 flex items-center justify-center bg-surface-0">
  </div>
{:else}
  <!-- No content view -->
  <div class="flex-1 flex items-center justify-center bg-surface-0 text-muted">
    <span>Select a file to view</span>
  </div>
{/if}
