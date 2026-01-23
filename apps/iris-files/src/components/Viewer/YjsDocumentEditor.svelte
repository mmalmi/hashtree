<script lang="ts">
  /**
   * Yjs Document Editor - Tiptap-based collaborative editor
   * Shows when a directory contains a .yjs file
   *
   * When collaborators are defined, this component will also fetch and merge
   * deltas from those users' hashtrees at the same relative path.
   */
  import { onMount, onDestroy, tick } from 'svelte';
  import { Editor } from '@tiptap/core';
  import StarterKit from '@tiptap/starter-kit';
  import Placeholder from '@tiptap/extension-placeholder';
  import Image from '@tiptap/extension-image';
  import * as Y from 'yjs';
  import Collaboration from '@tiptap/extension-collaboration';
  import { toHex, LinkType } from '@hashtree/core';
  import type { CID, TreeEntry } from '@hashtree/core';
  import { getTree } from '../../store';
  import { routeStore, createTreesStore, getTreeRootSync } from '../../stores';
  import { open as openForkModal } from '../Modals/ForkModal.svelte';
  import { open as openShareModal } from '../Modals/ShareModal.svelte';
  import { open as openCollaboratorsModal } from '../Modals/CollaboratorsModal.svelte';
  import { open as openBlossomPushModal } from '../Modals/BlossomPushModal.svelte';
  import { autosaveIfOwn, nostrStore, npubToPubkey, deleteTree } from '../../nostr';
  import { updateLocalRootCacheHex } from '../../treeRootCache';
  import { getCurrentRootCid, deleteCurrentFolder } from '../../actions';
  import VisibilityIcon from '../VisibilityIcon.svelte';
  import { Avatar } from '../User';
  import { CommentMark, createCommentsStore, type CommentsStore } from '../../lib/comments';
  import type { CommentsState } from '../../lib/comments/types';
  import { CommentsPanel, AddCommentModal } from '../Comments';
  import EditorToolbar from './EditorToolbar.svelte';
  import {
    saveImageToTree,
    generateImageFilename,
    loadDeltasFromEntries,
    loadCollaboratorDeltas,
    setupCollaboratorSubscriptions,
  } from '../../lib/yjs';
  import { createThrottledCapture, getThumbnailFilename } from '../../lib/yjs/thumbnail';

  interface Props {
    dirCid: CID;
    dirName: string;
    entries: TreeEntry[];
  }

  let { dirCid, dirName, entries }: Props = $props();

  let route = $derived($routeStore);
  let userNpub = $derived($nostrStore.npub);
  let viewedNpub = $derived(route.npub);
  let editorElement: HTMLElement | undefined = $state();
  let editor: Editor | undefined = $state();
  let ydoc: Y.Doc | undefined = $state();
  let saveStatus = $state<'idle' | 'saving' | 'saved'>('idle');
  let lastSaved = $state<Date | null>(null);
  let saveTimer: ReturnType<typeof setTimeout> | undefined;
  let loading = $state(true);
  let imageFileInput: HTMLInputElement | undefined = $state();

  // Throttled thumbnail capture (captures at most once per 30 seconds)
  const captureThrottled = createThrottledCapture(30000);

  // Expose for testing
  if (typeof window !== 'undefined') {
    window.__thumbnailCaptureReset = () => captureThrottled.reset();
  }

  // Comments state
  let commentsStore: CommentsStore | undefined = $state();
  let commentsState = $state<CommentsState>({ threads: new Map(), activeThreadId: null, panelOpen: false });
  let hasTextSelection = $state(false);

  // Add comment modal state
  let showAddCommentModal = $state(false);
  let pendingCommentSelection = $state<{ from: number; to: number; text: string } | null>(null);

  // Collaborators list
  let collaborators = $state<string[]>([]);

  // Check if current user is owner of this tree
  let isOwnTree = $derived(!viewedNpub || viewedNpub === userNpub);

  // Check if user is listed as an editor
  let isEditor = $derived(userNpub ? collaborators.includes(userNpub) : false);

  // Can edit if own tree or editor
  let canEdit = $derived(isOwnTree || isEditor);

  // Get owner pubkey for avatar display
  let ownerNpub = $derived(viewedNpub || userNpub);
  let ownerPubkey = $derived(ownerNpub ? npubToPubkey(ownerNpub) : null);

  // Get trees for visibility info
  let targetNpub = $derived(viewedNpub || userNpub);
  let treesStore = $derived(createTreesStore(targetNpub));
  let trees = $state<Array<{ name: string; visibility?: string }>>([]);

  $effect(() => {
    const store = treesStore;
    const unsub = store.subscribe(value => {
      trees = value;
    });
    return unsub;
  });

  let currentTree = $derived(route.treeName ? trees.find(t => t.name === route.treeName) : null);
  let visibility = $derived(currentTree?.visibility || 'public');

  // Find the .yjs file entry
  let yjsEntry = $derived(entries.find(e => e.name === '.yjs'));

  // Track previous collaborators to detect changes
  let prevCollaboratorsKey = '';
  let prevYjsEntryCid = '';
  let prevDirCid = '';

  // Reload editors list when .yjs file changes (e.g., collaborators updated)
  $effect(() => {
    const cidKey = yjsEntry
      ? `${toHex(yjsEntry.cid.hash)}:${yjsEntry.cid.key ? toHex(yjsEntry.cid.key) : ''}`
      : '';
    if (!cidKey || cidKey === prevYjsEntryCid) return;
    prevYjsEntryCid = cidKey;
    void loadEditors();
  });

  $effect(() => {
    const cidKey = dirCid
      ? `${toHex(dirCid.hash)}:${dirCid.key ? toHex(dirCid.key) : ''}`
      : '';
    if (!cidKey || cidKey === prevDirCid) return;
    prevDirCid = cidKey;
    void loadEditors();
  });


  // Reactively update subscriptions when collaborators change
  $effect(() => {
    const currentKey = collaborators.join(',');
    if (currentKey !== prevCollaboratorsKey && ydoc && collaborators.length > 0) {
      prevCollaboratorsKey = currentKey;
      setupCollabSubscriptions(collaborators);
    }
  });

  // Track previous entries CIDs to detect changes from FileBrowser updates
  let prevEntriesCids = '';

  // Reactively reload deltas when entries change (e.g., FileBrowser detects new delta files)
  $effect(() => {
    // Create a key from delta-related entries (deltas folder and state.yjs)
    const deltasEntry = entries.find(e => e.name === 'deltas' && e.type === LinkType.Dir);
    const stateEntry = entries.find(e => e.name === 'state.yjs' && e.type !== LinkType.Dir);
    const currentCids = [
      deltasEntry ? toHex(deltasEntry.cid.hash) : '',
      stateEntry ? toHex(stateEntry.cid.hash) : ''
    ].join(',');

    if (currentCids !== prevEntriesCids && ydoc && prevEntriesCids !== '') {
      // Entries changed after initial load - reload deltas
      prevEntriesCids = currentCids;
      loadDeltasFromEntries(entries).then(deltas => {
        for (const delta of deltas) {
          Y.applyUpdate(ydoc!, delta, 'remote');
        }
      });
    } else if (prevEntriesCids === '') {
      // Initial load - just set the key
      prevEntriesCids = currentCids;
    }
  });

  // Reactively update editor's editable state when canEdit changes
  $effect(() => {
    if (editor && editor.isEditable !== canEdit) {
      editor.setEditable(canEdit);
    }
  });

  // Save image to attachments/ directory
  async function saveImage(data: Uint8Array, filename: string): Promise<string | null> {
    if (!userNpub || !route.treeName) {
      console.warn('[YjsDoc] Missing userNpub or treeName, cannot save image');
      return null;
    }

    return saveImageToTree(
      data,
      filename,
      route.path,
      userNpub,
      route.treeName,
      isOwnTree,
      isOwnTree ? undefined : (visibility as import('hashtree').TreeVisibility)
    );
  }

  // Handle image upload from file input, paste, or drop
  async function handleImageUpload(file: File): Promise<void> {
    if (!file.type.startsWith('image/')) return;

    const data = new Uint8Array(await file.arrayBuffer());
    const filename = generateImageFilename(file);

    // Save to tree (updates the root cache)
    const savedFilename = await saveImage(data, filename);
    if (savedFilename && editor) {
      // Insert with attachments: src - MutationObserver resolves to /htree/ URL
      // Include uploader's npub so collaborator images can be resolved from their tree
      const uploaderNpub = userNpub;
      editor.chain().focus().setImage({ src: `attachments:${uploaderNpub}/${savedFilename}` }).run();
    }
  }

  // Handle paste event for images
  // eslint-disable-next-line @typescript-eslint/no-unused-vars
  function handlePaste(event: ClipboardEvent): void {
    const items = event.clipboardData?.items;
    if (!items) return;

    for (const item of items) {
      if (item.type.startsWith('image/')) {
        event.preventDefault();
        const file = item.getAsFile();
        if (file) handleImageUpload(file);
        return;
      }
    }
  }

  // Handle drop event for images
  // eslint-disable-next-line @typescript-eslint/no-unused-vars
  function handleDrop(event: DragEvent): void {
    const files = event.dataTransfer?.files;
    if (!files) return;

    for (const file of files) {
      if (file.type.startsWith('image/')) {
        event.preventDefault();
        handleImageUpload(file);
        return;
      }
    }
  }

  // Trigger file input for image upload
  function triggerImageUpload(): void {
    imageFileInput?.click();
  }

  // Handle file input change
  function handleFileInputChange(event: Event): void {
    const input = event.target as HTMLInputElement;
    const file = input.files?.[0];
    if (file) {
      handleImageUpload(file);
      input.value = ''; // Reset for next upload
    }
  }

  // No longer needed - images are loaded via /htree/ URLs on demand
   
  async function loadDocumentImages(): Promise<void> {
    // Images are now loaded via SW URLs, no preloading needed
  }

  // Subscription cleanup function
  let cleanupCollabSubscriptions: (() => void) | null = null;

  // Setup live subscriptions to collaborators' trees
  function setupCollabSubscriptions(collaboratorNpubs: string[]) {
    // Clean up existing subscriptions
    if (cleanupCollabSubscriptions) {
      cleanupCollabSubscriptions();
    }

    if (!ydoc) return;

    cleanupCollabSubscriptions = setupCollaboratorSubscriptions(
      collaboratorNpubs,
      route.treeName,
      route.path,
      viewedNpub,
      userNpub,
      ydoc,
      () => collaborators,
      (npubs) => { collaborators = npubs; }
    );
  }

  // Save state snapshot to tree (full document state)
  // When editing another user's document, saves to OUR tree at the same path
  async function saveStateSnapshot(): Promise<void> {
    const tree = getTree();
    if (!ydoc || !userNpub || !route.treeName) {
      console.warn('[YjsDoc] Missing ydoc, userNpub, or treeName, cannot save');
      return;
    }


    // Always save to OUR OWN tree, even when viewing someone else's document
    // This way our edits are published to our tree and synced via subscriptions
    let rootCid = getTreeRootSync(userNpub, route.treeName);

    // If we don't have our own tree yet, we need to create one
    if (!rootCid) {
      const { cid: emptyDirCid } = await tree.putDirectory([]);
      rootCid = emptyDirCid;
    }

    saveStatus = 'saving';

    try {
      // Encode full state snapshot (not just incremental delta)
      const stateUpdate = Y.encodeStateAsUpdate(ydoc);

      // Create timestamp-based filename
      const timestamp = Date.now().toString(36);
      const random = Math.random().toString(36).slice(2, 6);
      const deltaName = `${timestamp}-${random}`;

      // Get current path - this is the document path we're editing
      const currentPath = route.path;
      const deltasPath = [...currentPath, 'deltas'];

      // Ensure path exists in our tree - create directories as needed
      // E.g., if path is ['public', 'collab-doc'], we need to ensure 'public' dir exists first
      for (let i = 0; i < currentPath.length; i++) {
        const parentPath = currentPath.slice(0, i);
        const dirName = currentPath[i];
        const fullPath = currentPath.slice(0, i + 1).join('/');

        const pathExists = await tree.resolvePath(rootCid, fullPath);
        if (!pathExists) {
          const { cid: emptyDirCid } = await tree.putDirectory([]);
          rootCid = await tree.setEntry(rootCid, parentPath, dirName, emptyDirCid, 0, LinkType.Dir);
        }
      }

      // Check if document folder has .yjs file
      const docResult = await tree.resolvePath(rootCid, currentPath.join('/'));
      if (docResult) {
        const docEntries = await tree.listDirectory(docResult.cid);
        const hasYjsFile = docEntries.some(e => e.name === '.yjs' && e.type !== LinkType.Dir);

        if (!hasYjsFile) {
          // Create .yjs file with collaborators
          const yjsContent = collaborators.join('\n') + '\n';
          const yjsData = new TextEncoder().encode(yjsContent);
          const { cid: yjsCid, size: yjsSize } = await tree.putFile(yjsData);
          rootCid = await tree.setEntry(rootCid, currentPath, '.yjs', yjsCid, yjsSize, LinkType.Blob);
        }
      }

      // Check if deltas folder exists
      const deltasResult = await tree.resolvePath(rootCid, deltasPath.join('/'));
      if (!deltasResult) {
        // Create deltas folder
        const { cid: emptyDirCid } = await tree.putDirectory([]);
        rootCid = await tree.setEntry(rootCid, currentPath, 'deltas', emptyDirCid, 0, LinkType.Dir);
      }

      // Write the state snapshot file
      const { cid: deltaCid, size: deltaSize } = await tree.putFile(stateUpdate);
      const newRootCid = await tree.setEntry(
        rootCid,
        deltasPath,
        deltaName,
        deltaCid,
        deltaSize,
        LinkType.Blob
      );

      // Publish update
      if (isOwnTree) {
        // Own tree - use autosaveIfOwn which handles visibility settings
        autosaveIfOwn(newRootCid);
      } else {
        // Editing someone else's document - save to our own tree
        // Use updateLocalRootCacheHex which triggers throttled publish to Nostr
        // Pass visibility so our copy uses the same visibility (especially for link-visible with linkKey)
        updateLocalRootCacheHex(
          userNpub,
          route.treeName!,
          toHex(newRootCid.hash),
          newRootCid.key ? toHex(newRootCid.key) : undefined,
          (visibility as import('hashtree').TreeVisibility) || 'public'
        );
      }

      saveStatus = 'saved';
      lastSaved = new Date();

      // Capture and save thumbnail (throttled, non-blocking)
      if (editorElement && isOwnTree) {
        captureThumbnail(newRootCid);
      }
    } catch (e) {
      console.error('[YjsDoc] Failed to save state snapshot:', e);
      saveStatus = 'error';
    }
  }

  // Capture and save thumbnail to document directory (fire-and-forget)
  async function captureThumbnail(currentRootCid: CID) {
    if (!editorElement || !userNpub || !route.treeName) return;

    try {
      const thumbnailData = await captureThrottled(editorElement);
      if (!thumbnailData) return; // Throttled or failed

      const tree = getTree();
      const currentPath = route.path;

      // Save thumbnail as .thumbnail.jpg in the document directory
      const { cid: thumbCid, size: thumbSize } = await tree.putFile(thumbnailData);
      const newRootCid = await tree.setEntry(
        currentRootCid,
        currentPath,
        getThumbnailFilename(),
        thumbCid,
        thumbSize,
        LinkType.Blob
      );

      // Publish the update with thumbnail
      autosaveIfOwn(newRootCid);
    } catch {
      // Silently fail - thumbnail is not critical
    }
  }

  // Debounced save - saves full state snapshot (not incremental delta)
  function scheduleSave() {
    if (saveTimer) clearTimeout(saveTimer);
    saveTimer = setTimeout(() => saveStateSnapshot(), 1000);
  }

  // Load editors from .yjs file
  async function loadEditors() {
    try {
      const tree = getTree();
      let docDirCid = dirCid;

      if (!docDirCid && route.treeName && targetNpub) {
        const root = getTreeRootSync(targetNpub, route.treeName);
        if (root) {
          const resolved = await tree.resolvePath(root, route.path);
          docDirCid = resolved?.cid;
        }
      }

      if (!docDirCid) {
        collaborators = [];
        return;
      }

      const docEntries = await tree.listDirectory(docDirCid);
      const yjsConfigEntry = docEntries.find(e => e.name === '.yjs' && e.type !== LinkType.Dir);

      if (!yjsConfigEntry) {
        collaborators = [];
        return;
      }

      const data = await tree.readFile(yjsConfigEntry.cid);
      if (data) {
        const text = new TextDecoder().decode(data);
        collaborators = text.split('\n').filter(line => line.trim().startsWith('npub1'));
      } else {
        collaborators = [];
      }
    } catch (e) {
      console.error('[YjsDoc] Failed to load editors:', e);
      collaborators = [];
    }
  }

  // Save editors to .yjs file
  async function saveCollaborators(npubs: string[]) {
    const tree = getTree();
    let currentRootCid = getCurrentRootCid();
    if (!currentRootCid) {
      console.warn('[YjsDoc] No rootCid, cannot save editors');
      return;
    }

    try {
      // Create .yjs content with editors (one npub per line)
      const content = npubs.join('\n') + '\n';
      const data = new TextEncoder().encode(content);
      const { cid: yjsCid, size: yjsSize } = await tree.putFile(data);

      // Update .yjs file in tree
      const newRootCid = await tree.setEntry(
        currentRootCid,
        route.path,
        '.yjs',
        yjsCid,
        yjsSize,
        false
      );

      autosaveIfOwn(newRootCid);
      collaborators = npubs;
    } catch (e) {
      console.error('[YjsDoc] Failed to save editors:', e);
    }
  }

  // Handle share
  function handleShare() {
    openShareModal(window.location.href);
  }

  // Handle fork
  function handleFork() {
    if (!dirCid) return;
    openForkModal(dirCid, dirName);
  }

  // Handle collaborators
  function handleCollaborators() {
    if (isOwnTree) {
      openCollaboratorsModal({ npubs: collaborators, onSave: saveCollaborators });
    } else {
      openCollaboratorsModal({ npubs: collaborators });
    }
  }

  // Handle delete
  async function handleDelete() {
    if (confirm(`Delete document "${dirName}" and all its contents?`)) {
      // If at tree root (path is empty), delete the entire tree
      // Otherwise delete the current folder within the tree
      if (route.path.length === 0 && route.treeName) {
        await deleteTree(route.treeName);
        window.location.hash = '/';
      } else {
        deleteCurrentFolder();
      }
    }
  }

  onMount(async () => {
    if (import.meta.env.VITE_TEST_MODE) {
      window.__reloadYjsEditors = () => loadEditors();
    }

    // Load editors
    await loadEditors();

    // Load images from attachments directory
    await loadDocumentImages();

    // Create Yjs document
    ydoc = new Y.Doc();

    // Load existing deltas from current view's entries
    const localDeltas = await loadDeltasFromEntries(entries);
    for (const delta of localDeltas) {
      Y.applyUpdate(ydoc, delta, 'remote');
    }

    // Load deltas from collaborators' trees
    if (collaborators.length > 0) {
      await loadCollaboratorDeltas(collaborators, route.npub, route.path, route.treeName, ydoc);
    }

    // Set loading false and wait for DOM to render editorElement
    loading = false;
    await tick();

    if (!editorElement) return;

    // Create comments store backed by Yjs
    commentsStore = createCommentsStore(ydoc);

    // Subscribe to comments state changes
    const unsubComments = commentsStore.subscribe((state) => {
      commentsState = state;
    });

    // Create Tiptap editor with Yjs collaboration, image support, and comments
    editor = new Editor({
      element: editorElement,
      extensions: [
        StarterKit.configure({
          history: false, // Yjs handles history
        }),
        Placeholder.configure({
          placeholder: 'Start typing...',
        }),
        Collaboration.configure({
          document: ydoc,
        }),
        Image.configure({
          inline: false,
          allowBase64: false,
        }),
        CommentMark.configure({
          HTMLAttributes: {
            class: 'comment-highlight',
          },
        }),
      ],
      editable: canEdit,
      editorProps: {
        attributes: {
          class: 'prose prose-invert max-w-none focus:outline-none min-h-[200px] p-4',
        },
        handlePaste: (view, event) => {
          const items = event.clipboardData?.items;
          if (!items) return false;

          for (const item of items) {
            if (item.type.startsWith('image/')) {
              event.preventDefault();
              const file = item.getAsFile();
              if (file) handleImageUpload(file);
              return true;
            }
          }
          return false;
        },
        handleDrop: (view, event, slice, moved) => {
          if (moved) return false;

          const files = event.dataTransfer?.files;
          if (!files) return false;

          for (const file of files) {
            if (file.type.startsWith('image/')) {
              event.preventDefault();
              handleImageUpload(file);
              return true;
            }
          }
          return false;
        },
      },
    });

    // Set up live subscriptions to collaborators' trees
    if (collaborators.length > 0) {
      setupCollabSubscriptions(collaborators);
    }

    // Listen for updates and save (full state snapshot, not incremental delta)
    ydoc.on('update', (_update: Uint8Array, origin: unknown) => {
      if (origin !== 'remote') {
        scheduleSave();
      }
    });

    // Set up image URL resolution for attachments:* sources
    // Use MutationObserver to watch for new images and resolve their sources
    const observer = new MutationObserver((mutations) => {
      for (const mutation of mutations) {
        for (const node of mutation.addedNodes) {
          if (node instanceof HTMLImageElement) {
            resolveImageSrc(node);
          } else if (node instanceof HTMLElement) {
            const images = node.querySelectorAll('img');
            for (const img of images) {
              resolveImageSrc(img as HTMLImageElement);
            }
          }
        }
      }
    });

    observer.observe(editorElement, { childList: true, subtree: true });

    // Resolve existing images
    const existingImages = editorElement.querySelectorAll('img');
    for (const img of existingImages) {
      resolveImageSrc(img as HTMLImageElement);
    }

    // Cleanup observer and comments on destroy
    const editorOriginalDestroy = editor.destroy.bind(editor);
    editor.destroy = () => {
      observer.disconnect();
      unsubComments();
      editorOriginalDestroy();
    };
  });

  // Resolve image src from attachments:filename to /htree/ URL
  // Format: attachments:npub/filename (new) or attachments:filename (legacy)
  function resolveImageSrc(img: HTMLImageElement): void {
    const src = img.getAttribute('src');
    if (!src || !src.startsWith('attachments:')) return;

    const attachmentPath = src.replace('attachments:', '');

    // Check if npub is included in the path (new format: npub/filename)
    let imageNpub: string;
    let filename: string;

    if (attachmentPath.startsWith('npub1')) {
      // New format: attachments:npub1.../filename
      const slashIndex = attachmentPath.indexOf('/');
      if (slashIndex > 0) {
        imageNpub = attachmentPath.slice(0, slashIndex);
        filename = attachmentPath.slice(slashIndex + 1);
      } else {
        // Malformed, fall back to viewed/user npub
        imageNpub = viewedNpub || userNpub || '';
        filename = attachmentPath;
      }
    } else {
      // Legacy format: attachments:filename - use viewed npub (owner's tree)
      imageNpub = viewedNpub || userNpub || '';
      filename = attachmentPath;
    }

    const treeName = route.treeName;

    if (!imageNpub || !treeName) {
      img.dataset.pendingResolve = 'true';
      return;
    }

    // Build /htree/ URL: /htree/{npub}/{treeName}/{path}/attachments/{filename}
    // treeName must be URL-encoded (may contain slashes like "docs/docname")
    const encodedTreeName = encodeURIComponent(treeName);
    const pathParts = [...route.path, 'attachments', filename];
    const encodedPath = pathParts.map(encodeURIComponent).join('/');
    img.src = `/htree/${imageNpub}/${encodedTreeName}/${encodedPath}`;
  }

  // Re-resolve any images that failed to load initially (after npub/treeName available)
  // eslint-disable-next-line @typescript-eslint/no-unused-vars
  function _retryPendingImages(): void {
    if (!editorElement) return;
    const pendingImages = editorElement.querySelectorAll('img[data-pending-resolve="true"]');
    for (const img of pendingImages) {
      resolveImageSrc(img as HTMLImageElement);
    }
  }

  onDestroy(() => {
    if (saveTimer) clearTimeout(saveTimer);
    // Clean up collaborator subscriptions
    if (cleanupCollabSubscriptions) {
      cleanupCollabSubscriptions();
      cleanupCollabSubscriptions = null;
    }
    // Clean up comments store
    commentsStore?.destroy();
    editor?.destroy();
    ydoc?.destroy();
  });

  // Comment functions
  function addComment() {
    if (!editor || !commentsStore || !userNpub) return;

    const { from, to } = editor.state.selection;
    if (from === to) return; // No selection

    const selectedText = editor.state.doc.textBetween(from, to, ' ');
    if (!selectedText.trim()) return;

    // Store the selection and show modal
    pendingCommentSelection = { from, to, text: selectedText };
    showAddCommentModal = true;
  }

  function handleAddCommentSubmit(commentText: string) {
    if (!editor || !commentsStore || !userNpub || !pendingCommentSelection) return;

    const { from, to, text } = pendingCommentSelection;

    // Create the thread in the store
    const threadId = commentsStore.createThread(text, commentText, userNpub);

    // Restore selection and apply the comment mark
    editor.chain()
      .focus()
      .setTextSelection({ from, to })
      .setComment(threadId)
      .run();

    // Close modal and clear pending selection
    showAddCommentModal = false;
    pendingCommentSelection = null;
  }

  function handleAddCommentCancel() {
    showAddCommentModal = false;
    pendingCommentSelection = null;
    editor?.chain().focus().run();
  }

  function handleCommentThreadClick(threadId: string) {
    if (!editor) return;

    // Find the comment mark in the document and scroll to it
    const { doc } = editor.state;
    let found = false;

    doc.descendants((node, pos) => {
      if (found) return false;

      const commentMark = node.marks.find(
        mark => mark.type.name === 'comment' && mark.attrs.commentId === threadId
      );

      if (commentMark) {
        // Select the commented text
        editor?.chain().focus().setTextSelection({ from: pos, to: pos + node.nodeSize }).run();
        found = true;
        return false;
      }
    });
  }

  function handleDeleteThread(threadId: string) {
    if (!editor) return;
    // Remove the comment highlight from the document
    editor.chain().removeCommentById(threadId).run();
  }

  // Handle click on commented text in the editor
  function handleEditorClick(event: MouseEvent) {
    const target = event.target as HTMLElement;
    const commentHighlight = target.closest('.comment-highlight');

    if (commentHighlight) {
      const commentId = commentHighlight.getAttribute('data-comment-id');
      if (commentId && commentsStore) {
        // Open panel and select thread
        commentsStore.setPanelOpen(true);
        commentsStore.setActiveThread(commentId);
      }
    }
  }

  function toggleCommentsPanel() {
    commentsStore?.togglePanel();
  }

  // Track selection changes to enable/disable comment button
  $effect(() => {
    if (!editor) return;

    const updateSelection = () => {
      const { from, to } = editor!.state.selection;
      hasTextSelection = from !== to;
    };

    editor.on('selectionUpdate', updateSelection);
    return () => {
      editor?.off('selectionUpdate', updateSelection);
    };
  });
</script>

<div class="flex-1 flex flex-col min-h-0 bg-surface-0">
  <!-- Status bar -->
  <div class="shrink-0 px-4 py-2 border-b border-surface-3 flex flex-wrap items-center justify-between gap-2 bg-surface-1 text-sm">
    <div class="flex items-center gap-2 min-w-0">
      <a href="#/" class="btn-ghost p-1" title="Back to home">
        <span class="i-lucide-chevron-left text-lg"></span>
      </a>
      {#if ownerPubkey && ownerNpub}
        <a href="#/{ownerNpub}" class="shrink-0">
          <Avatar pubkey={ownerPubkey} size={20} />
        </a>
      {/if}
      <span class="i-lucide-file-text text-text-2 shrink-0"></span>
      <span class="font-medium text-text-1 truncate">{dirName}</span>
      <VisibilityIcon {visibility} class="text-text-2 text-sm" />
      {#if canEdit}
        <span class="i-lucide-pencil text-xs text-text-3" title={isOwnTree ? "You can edit this document" : "Editing as editor - saves to your tree"}></span>
      {/if}
      {#if !canEdit}
        <span class="text-xs px-2 py-0.5 rounded bg-surface-2 text-text-3">Read-only</span>
      {/if}
      {#if isEditor && !isOwnTree}
        <span class="text-xs px-2 py-0.5 rounded bg-success/20 text-success" title="You are an editor - edits save to your tree">Editor</span>
      {/if}
    </div>
    <div class="flex items-center gap-2 shrink-0">
      <!-- Save status -->
      <div class="flex items-center gap-2 text-text-3">
        {#if saveStatus === 'saving'}
          <span class="i-lucide-loader-2 animate-spin"></span>
          <span>Saving...</span>
        {:else if lastSaved}
          <span class="text-xs">Saved {lastSaved.toLocaleTimeString()}</span>
        {/if}
      </div>
      <!-- Share button -->
      <button onclick={handleShare} class="btn-ghost" title="Share document">
        <span class="i-lucide-share"></span>
      </button>
      <button onclick={() => openBlossomPushModal(dirCid, dirName, true, route.npub ? npubToPubkey(route.npub) : undefined, route.treeName)} class="btn-ghost" title="Push to file servers">
        <span class="i-lucide-upload-cloud"></span>
      </button>
      <!-- Collaborators button -->
      <button onclick={handleCollaborators} class="btn-ghost flex items-center gap-1" title={isOwnTree ? 'Manage editors' : 'View editors'}>
        <span class="i-lucide-users"></span>
        {#if collaborators.length > 0}
          <span class="text-xs bg-surface-2 px-1.5 rounded-full">{collaborators.length}</span>
        {/if}
      </button>
      <!-- Fork button -->
      <button onclick={handleFork} class="btn-ghost flex items-center gap-1" title="Fork document as new tree">
        <span class="i-lucide-git-fork"></span>
        Fork
      </button>
      <!-- Delete button - only for own tree -->
      {#if isOwnTree}
        <button onclick={handleDelete} class="btn-ghost text-danger" title="Delete document">
          Delete
        </button>
      {/if}
    </div>
  </div>

  <!-- Formatting Toolbar -->
  {#if canEdit && editor && !loading}
    <EditorToolbar
      {editor}
      {hasTextSelection}
      {userNpub}
      commentsCount={commentsState.threads.size}
      commentsPanelOpen={commentsState.panelOpen}
      onAddComment={addComment}
      onToggleCommentsPanel={toggleCommentsPanel}
      onImageUpload={triggerImageUpload}
    />
  {/if}

  <!-- Editor area with comments panel -->
  <div class="flex-1 flex min-h-0">
    <!-- Main editor area - A4 paper style on large screens -->
    <!-- svelte-ignore a11y_click_events_have_key_events -->
    <!-- svelte-ignore a11y_no_static_element_interactions -->
    <div class="flex-1 overflow-auto bg-[#0d0d14]" onclick={handleEditorClick}>
      <div class="a4-page bg-[#1a1a24]">
        {#if loading}
          <div class="flex items-center justify-center min-h-[400px] text-text-3 p-4 md:p-8">
            <span class="i-lucide-loader-2 animate-spin mr-2"></span>
            Loading document...
          </div>
        {:else}
          <div bind:this={editorElement} class="ProseMirror-container prose prose-sm max-w-none min-h-full"></div>
        {/if}
      </div>
    </div>

    <!-- Comments panel (CSS visibility toggle) -->
    {#if commentsStore}
      <div class="w-80 shrink-0 border-l border-surface-3 {commentsState.panelOpen ? '' : 'hidden'}">
        <CommentsPanel
          {commentsStore}
          {userNpub}
          onClickThread={handleCommentThreadClick}
          onDeleteThread={handleDeleteThread}
        />
      </div>
    {/if}
  </div>
</div>

<!-- Hidden file input for image upload -->
<input
  bind:this={imageFileInput}
  type="file"
  accept="image/*"
  onchange={handleFileInputChange}
  class="hidden"
/>

<!-- Add Comment Modal -->
<AddCommentModal
  show={showAddCommentModal}
  quotedText={pendingCommentSelection?.text || ''}
  onSubmit={handleAddCommentSubmit}
  onCancel={handleAddCommentCancel}
/>

<style>
  /* A4 paper styling for large screens */
  .a4-page {
    min-height: 100%;
  }

  @media (min-width: 900px) {
    .a4-page {
      max-width: 816px;
      margin: 2rem auto;
      min-height: calc(100% - 4rem);
      box-shadow: 0 4px 20px rgba(0, 0, 0, 0.5);
      border-radius: 4px;
    }
  }

  :global(.ProseMirror-container .ProseMirror) {
    min-height: 200px;
    padding: 1rem;
  }

  @media (min-width: 900px) {
    :global(.ProseMirror-container .ProseMirror) {
      padding: 2rem 3rem;
    }
  }
  :global(.ProseMirror-container .ProseMirror:focus) {
    outline: none;
  }
  :global(.ProseMirror-container .ProseMirror p.is-editor-empty:first-child::before) {
    color: var(--color-text-3);
    content: attr(data-placeholder);
    float: left;
    height: 0;
    pointer-events: none;
  }

  /* Image styles */
  :global(.ProseMirror-container .ProseMirror img) {
    max-width: 100%;
    height: auto;
    margin: 1rem 0;
    cursor: pointer;
  }

  :global(.ProseMirror-container .ProseMirror img.ProseMirror-selectednode) {
    outline: 3px solid var(--color-accent);
    outline-offset: 3px;
    box-shadow: 0 0 0 6px rgba(var(--color-accent-rgb, 99, 102, 241), 0.2);
    cursor: grab;
  }

  /* Comment highlight styles */
  :global(.ProseMirror-container .comment-highlight) {
    background-color: rgba(255, 213, 79, 0.3);
    border-bottom: 2px solid rgba(255, 213, 79, 0.7);
    padding: 0 2px;
    margin: 0 -2px;
    border-radius: 2px;
    cursor: pointer;
    transition: background-color 0.15s;
  }

  :global(.ProseMirror-container .comment-highlight:hover) {
    background-color: rgba(255, 213, 79, 0.5);
  }
</style>
