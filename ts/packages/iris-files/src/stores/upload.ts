/**
 * Upload store - manages file upload with progress tracking
 * Uses Svelte stores for state management
 *
 * All uploads use encryption by default (CHK - Content Hash Key).
 */
import { writable, get } from 'svelte/store';
import { toHex, nhashEncode, cid, LinkType, videoChunker } from 'hashtree';
import type { CID } from 'hashtree';
import { getTree } from '../store';
import { autosaveIfOwn, saveHashtree, nostrStore } from '../nostr';
import { navigate } from '../utils/navigate';
import { parseRoute } from '../utils/route';
import { getCurrentPathFromUrl } from '../actions/route';
import { markFilesChanged } from './recentlyChanged';
import { open as openExtractModal } from '../components/Modals/ExtractModal.svelte';
import { open as openGitignoreModal } from '../components/Modals/GitignoreModal.svelte';
import { isArchiveFile, getArchiveFileList } from '../utils/compression';
import { nip19 } from 'nostr-tools';
import type { FileWithPath, DirectoryReadResult } from '../utils/directory';
import { findGitignoreFile, parseGitignoreFromFile, applyGitignoreFilter, applyDefaultIgnoreFilter } from '../utils/directory';
import { getTreeRootSync } from './treeRoot';
import { settingsStore } from '../stores/settings';
import { toast } from '../stores/toast';

// Video file extensions for faster streaming start
const VIDEO_EXTENSIONS = new Set(['.mp4', '.webm', '.mkv', '.mov', '.avi', '.m4v', '.ogv', '.3gp']);

function isVideoFile(filename: string): boolean {
  const ext = filename.toLowerCase().slice(filename.lastIndexOf('.'));
  return VIDEO_EXTENSIONS.has(ext);
}

// Upload progress type
export interface UploadProgress {
  current: number;
  total: number;
  fileName: string;
  bytes?: number;
  totalBytes?: number;
  status?: 'reading' | 'writing' | 'finalizing' | 'zipping';
}

// Svelte store for upload progress
export const uploadProgress = writable<UploadProgress | null>(null);

// Cancellation flag
let uploadCancelled = false;

export function setUploadProgress(progress: UploadProgress | null) {
  uploadProgress.set(progress);
}

export function getUploadProgress() {
  return get(uploadProgress);
}

/**
 * Cancel any in-progress upload
 */
export function cancelUpload() {
  uploadCancelled = true;
  setUploadProgress(null);
}

/**
 * Check if upload was cancelled and reset flag
 */
function checkCancelled(): boolean {
  if (uploadCancelled) {
    uploadCancelled = false;
    return true;
  }
  return false;
}

// Stall detection timeout (10 seconds)
const STALL_TIMEOUT_MS = 10000;

/**
 * Wrap an async operation with stall detection
 * Shows a warning toast if the operation takes too long
 */
async function withStallDetection<T>(
  operation: Promise<T>,
  message: string
): Promise<T> {
  let toastShown = false;
  const timeoutId = setTimeout(() => {
    toastShown = true;
    toast.warning(message);
  }, STALL_TIMEOUT_MS);

  try {
    return await operation;
  } finally {
    clearTimeout(timeoutId);
    // If we showed a stall warning, show success when it completes
    if (toastShown) {
      toast.success('Operation resumed');
    }
  }
}

/**
 * Upload files to the current directory
 */
export async function uploadFiles(files: FileList): Promise<void> {
  if (!files.length) return;

  // Reset cancellation flag at start
  uploadCancelled = false;

  // Convert FileList to array immediately to prevent it from being cleared
  const filesArray = Array.from(files);
  const total = filesArray.length;
  const uploadedFileNames: string[] = [];
  const tree = getTree();
  const route = parseRoute();
  const dirPath = getCurrentPathFromUrl();

  // Get current rootCid from resolver - track locally for multi-file uploads
  let currentRootCid: CID | null = getTreeRootSync(route.npub, route.treeName);

  // Check if we need to initialize a new tree (virtual directory case)
  let needsTreeInit = !currentRootCid?.hash && route.npub && route.treeName;
  let isOwnTree = false;
  let routePubkey: string | null = null;

  if (needsTreeInit) {
    const nostrState = get(nostrStore);
    try {
      const decoded = nip19.decode(route.npub!);
      if (decoded.type === 'npub') routePubkey = decoded.data as string;
    } catch {}
    isOwnTree = routePubkey === nostrState.pubkey;
  }

  for (let i = 0; i < filesArray.length; i++) {
    // Check for cancellation at start of each file
    if (checkCancelled()) return;

    const file = filesArray[i];
    if (!file) continue;
    const totalBytes = file.size;

    setUploadProgress({
      current: i + 1,
      total,
      fileName: file.name,
      bytes: 0,
      totalBytes,
    });

    let fileCid: CID;
    let size: number;

    // Check if this is an archive file - needs buffering for extraction
    if (isArchiveFile(file.name)) {
      // Buffer the file for archive extraction
      const chunks: Uint8Array[] = [];
      let bytesRead = 0;
      const reader = file.stream().getReader();

      while (true) {
        setUploadProgress({
          current: i + 1,
          total,
          fileName: file.name,
          bytes: bytesRead,
          totalBytes,
          status: 'reading',
        });
        const readResult = await withStallDetection(
          reader.read(),
          `Reading ${file.name} is taking longer than expected...`
        );
        if (readResult.done) break;
        chunks.push(readResult.value);
        bytesRead += readResult.value.length;
      }

      // Combine chunks
      const data = new Uint8Array(bytesRead);
      let offset = 0;
      for (const chunk of chunks) {
        data.set(chunk, offset);
        offset += chunk.length;
      }

      try {
        // Get file list without decompressing - fast and low memory
        const archiveInfo = getArchiveFileList(data);
        if (archiveInfo.files.length > 0) {
          setUploadProgress(null);
          // Pass archive data for extraction later (one file at a time)
          // Include commonRoot so modal knows if files already have a root folder
          openExtractModal({ archiveName: file.name, files: archiveInfo.files, archiveData: data, commonRoot: archiveInfo.commonRoot });
          // Don't continue with normal upload - the modal will handle it
          return;
        }
      } catch (err) {
        console.warn('Failed to parse archive, uploading as regular file:', err);
        // Fall through to normal upload
      }

      // Use encrypted file storage (default)
      setUploadProgress({
        current: i + 1,
        total,
        fileName: file.name,
        bytes: bytesRead,
        totalBytes,
        status: 'writing',
      });
      const result = await withStallDetection(
        tree.putFile(data),
        `Writing ${file.name} is taking longer than expected...`
      );
      fileCid = result.cid;
      size = result.size;
    } else {
      // Stream upload - no buffering needed
      // Use videoChunker for video files (smaller first chunk for faster playback start)
      const chunker = isVideoFile(file.name) ? videoChunker() : undefined;
      const stream = tree.createStream({ chunker });
      let bytesRead = 0;
      const reader = file.stream().getReader();

      while (true) {
        setUploadProgress({
          current: i + 1,
          total,
          fileName: file.name,
          bytes: bytesRead,
          totalBytes,
          status: 'reading',
        });
        const readResult = await withStallDetection(
          reader.read(),
          `Reading ${file.name} is taking longer than expected...`
        );
        if (readResult.done) break;
        setUploadProgress({
          current: i + 1,
          total,
          fileName: file.name,
          bytes: bytesRead,
          totalBytes,
          status: 'writing',
        });
        await withStallDetection(
          stream.append(readResult.value),
          `Writing ${file.name} is taking longer than expected...`
        );
        bytesRead += readResult.value.length;
      }

      setUploadProgress({
        current: i + 1,
        total,
        fileName: file.name,
        bytes: bytesRead,
        totalBytes,
        status: 'finalizing',
      });
      const result = await withStallDetection(
        stream.finalize(),
        `Finalizing ${file.name} is taking longer than expected...`
      );
      fileCid = cid(result.hash, result.key);
      size = result.size;
    }
    uploadedFileNames.push(file.name);

    // Add file to tree immediately after upload completes
    if (currentRootCid?.hash) {
      // Add to existing tree - setEntry handles encryption based on rootCid.key
      const newRootCid = await tree.setEntry(
        currentRootCid,
        dirPath,
        file.name,
        fileCid,
        size
      );
      currentRootCid = newRootCid;
      // Mark this file as changed for pulse effect
      markFilesChanged(new Set([file.name]));
      // Update cache immediately so file appears in UI one by one
      autosaveIfOwn(newRootCid);
    } else if (needsTreeInit) {
      // First file in a new virtual directory - create encrypted tree
      const { cid: newRootCid } = await tree.putDirectory([{ name: file.name, cid: fileCid, size }]);
      currentRootCid = newRootCid;
      markFilesChanged(new Set([file.name]));

      if (isOwnTree && routePubkey) {
        // Save to nostr (fire-and-forget, works offline)
        const currentVisibility = nostrStore.getState().selectedTree?.visibility ?? 'public';
        saveHashtree(route.treeName!, newRootCid, { visibility: currentVisibility }).catch(() => {
          // Network error is OK - local changes are saved, will sync when online
        });
        nostrStore.setSelectedTree({
          id: '',
          name: route.treeName!,
          pubkey: routePubkey,
          rootHash: toHex(newRootCid.hash),
          rootKey: newRootCid.key ? toHex(newRootCid.key) : undefined,
          visibility: currentVisibility,
          created_at: Math.floor(Date.now() / 1000),
        });
      }
      needsTreeInit = false; // Tree is now initialized
    } else {
      // No existing tree and not a virtual directory - create new encrypted root
      const { cid: newRootCid } = await tree.putDirectory([{ name: file.name, cid: fileCid, size }]);
      currentRootCid = newRootCid;
      markFilesChanged(new Set([file.name]));
      if (i === 0) {
        navigate('/');
      }
    }
  }

  // Note: autosaveIfOwn is called after each file for instant UI updates
  // No need for final autosave since we save progressively

  setUploadProgress(null);

  // If single file uploaded, navigate to it
  if (uploadedFileNames.length === 1) {
    const currentRoute = parseRoute();
    const fileName = uploadedFileNames[0];

    if (currentRoute.npub && currentRoute.treeName) {
      // Tree route: /npub/treeName/path/filename
      const parts = [currentRoute.npub, currentRoute.treeName, ...dirPath, fileName];
      let url = '/' + parts.map(encodeURIComponent).join('/');
      // Preserve linkKey for link-visible trees
      if (currentRoute.linkKey) {
        url += `?k=${currentRoute.linkKey}`;
      }
      navigate(url);
    } else if (currentRoute.hash) {
      // Hash route: /nhash1.../path/filename
      const nhash = nhashEncode(currentRoute.hash);
      const parts = [nhash, ...dirPath, fileName];
      navigate('/' + parts.map(encodeURIComponent).join('/'));
    }
  }
}

/**
 * Upload files with path information (for directory uploads)
 * Files are uploaded with their relative paths preserved in the tree structure
 */
export async function uploadFilesWithPaths(filesWithPaths: FileWithPath[]): Promise<void> {
  if (!filesWithPaths.length) return;

  // Reset cancellation flag at start
  uploadCancelled = false;

  const total = filesWithPaths.length;
  const uploadedFileNames: string[] = [];
  const tree = getTree();
  const route = parseRoute();
  const dirPath = getCurrentPathFromUrl();

  // Get current rootCid from resolver - track locally for multi-file uploads
  let currentRootCid: CID | null = getTreeRootSync(route.npub, route.treeName);

  // Check if we need to initialize a new tree
  let needsTreeInit = !currentRootCid?.hash && route.npub && route.treeName;
  let isOwnTree = false;
  let routePubkey: string | null = null;

  if (needsTreeInit) {
    const nostrState = get(nostrStore);
    try {
      const decoded = nip19.decode(route.npub!);
      if (decoded.type === 'npub') routePubkey = decoded.data as string;
    } catch {}
    isOwnTree = routePubkey === nostrState.pubkey;
  }

  // Collect all unique directory paths that need to be created
  const dirsToCreate = new Set<string>();
  for (const { relativePath } of filesWithPaths) {
    const pathParts = relativePath.split('/');
    pathParts.pop(); // Remove filename
    // Add all parent paths
    for (let i = 1; i <= pathParts.length; i++) {
      dirsToCreate.add(pathParts.slice(0, i).join('/'));
    }
  }

  // Sort directories by depth (shallowest first)
  const sortedDirs = Array.from(dirsToCreate).sort((a, b) =>
    a.split('/').length - b.split('/').length
  );

  // Create directories first (before processing files)
  const createdDirs = new Set<string>();

  // Helper to ensure directory exists
  const ensureDir = async (dirPathStr: string) => {
    if (createdDirs.has(dirPathStr)) return;

    const parts = dirPathStr.split('/');
    const dirName = parts.pop()!;
    const parentPath = [...dirPath, ...parts];

    // Create empty directory
    const { cid: emptyDirCid } = await tree.putDirectory([]);

    if (currentRootCid?.hash) {
      const newRootCid = await tree.setEntry(
        currentRootCid,
        parentPath,
        dirName,
        emptyDirCid,
        0,
        LinkType.Dir
      );
      currentRootCid = newRootCid;
    } else if (needsTreeInit) {
      const { cid: rootCidVal } = await tree.putDirectory([]);
      const newRootCid = await tree.setEntry(
        rootCidVal,
        parentPath,
        dirName,
        emptyDirCid,
        0,
        LinkType.Dir
      );
      currentRootCid = newRootCid;

      if (isOwnTree && routePubkey) {
        const currentVisibility = nostrStore.getState().selectedTree?.visibility ?? 'public';
        saveHashtree(route.treeName!, newRootCid, { visibility: currentVisibility }).catch(() => {
          // Network error is OK - local changes are saved, will sync when online
        });
        nostrStore.setSelectedTree({
          id: '',
          name: route.treeName!,
          pubkey: routePubkey,
          rootHash: toHex(newRootCid.hash),
          rootKey: newRootCid.key ? toHex(newRootCid.key) : undefined,
          visibility: currentVisibility,
          created_at: Math.floor(Date.now() / 1000),
        });
      }
      needsTreeInit = false;
    } else {
      const { cid: rootCidVal } = await tree.putDirectory([]);
      const newRootCid = await tree.setEntry(
        rootCidVal,
        parentPath,
        dirName,
        emptyDirCid,
        0,
        LinkType.Dir
      );
      currentRootCid = newRootCid;
    }

    createdDirs.add(dirPathStr);
  };

  // Create all directories first
  for (const dir of sortedDirs) {
    if (checkCancelled()) return;
    await ensureDir(dir);
  }

  for (let i = 0; i < filesWithPaths.length; i++) {
    // Check for cancellation at start of each file
    if (checkCancelled()) return;

    const { file, relativePath } = filesWithPaths[i];
    if (!file) continue;
    const totalBytes = file.size;

    setUploadProgress({
      current: i + 1,
      total,
      fileName: relativePath,
      bytes: 0,
      totalBytes,
      status: 'reading',
    });

    // Stream upload - no buffering needed
    // Use videoChunker for video files (smaller first chunk for faster playback start)
    const chunker = isVideoFile(file.name) ? videoChunker() : undefined;
    const stream = tree.createStream({ chunker });
    let bytesRead = 0;
    const reader = file.stream().getReader();

    while (true) {
      const readResult = await withStallDetection(
        reader.read(),
        `Reading ${relativePath} is taking longer than expected...`
      );
      if (readResult.done) break;
      await withStallDetection(
        stream.append(readResult.value),
        `Writing ${relativePath} is taking longer than expected...`
      );
      bytesRead += readResult.value.length;
      setUploadProgress({
        current: i + 1,
        total,
        fileName: relativePath,
        bytes: bytesRead,
        totalBytes,
        status: 'writing',
      });
    }

    setUploadProgress({
      current: i + 1,
      total,
      fileName: relativePath,
      bytes: bytesRead,
      totalBytes,
      status: 'finalizing',
    });
    const result = await withStallDetection(
      stream.finalize(),
      `Finalizing ${relativePath} is taking longer than expected...`
    );
    const fileCid = cid(result.hash, result.key);
    const size = result.size;

    // Parse the relative path to get directory components and filename
    const pathParts = relativePath.split('/');
    const fileName = pathParts.pop()!;
    const fileDirPath = pathParts; // Directory path within the uploaded structure

    // Combine with current directory path
    const fullDirPath = [...dirPath, ...fileDirPath];

    uploadedFileNames.push(relativePath);

    // Add file to tree (directories already exist)
    try {
      if (currentRootCid?.hash) {
        const newRootCid = await tree.setEntry(
          currentRootCid,
          fullDirPath,
          fileName,
          fileCid,
          size
        );
        currentRootCid = newRootCid;

        // Mark this file as changed for pulse effect (use just filename for display in current dir)
        if (fileDirPath.length === 0) {
          markFilesChanged(new Set([fileName]));
        }
        // Update cache immediately so file appears in UI one by one
        autosaveIfOwn(newRootCid);
      }
    } catch (err) {
      console.error(`Failed to add file ${relativePath}:`, err);
      toast.error(`Failed to add ${relativePath}`);
      // Continue with next file instead of stopping entirely
    }
  }

  // Note: autosaveIfOwn is called after each file for instant UI updates

  setUploadProgress(null);
}

/**
 * Upload a directory with gitignore support
 * Checks for .gitignore at root and handles filtering based on user preference
 */
export async function uploadDirectory(result: DirectoryReadResult): Promise<void> {
  const { files, hasGitignore, rootDirName } = result;

  if (files.length === 0) return;

  const settings = get(settingsStore);
  const gitignoreBehavior = settings.upload.gitignoreBehavior;
  const dirName = rootDirName || 'directory';

  // Always apply default ignore patterns (.git, .DS_Store, etc.)
  const { included: defaultFiltered } = applyDefaultIgnoreFilter(files);

  // If no .gitignore, upload with defaults applied
  if (!hasGitignore) {
    await uploadFilesWithPaths(defaultFiltered);
    return;
  }

  // If user chose to always skip gitignore, upload with defaults only
  if (gitignoreBehavior === 'never') {
    await uploadFilesWithPaths(defaultFiltered);
    return;
  }

  // Find and parse the .gitignore file
  const gitignoreFileEntry = findGitignoreFile(files, rootDirName);
  if (!gitignoreFileEntry) {
    // .gitignore detection was wrong, upload with defaults only
    await uploadFilesWithPaths(defaultFiltered);
    return;
  }

  const patterns = await parseGitignoreFromFile(gitignoreFileEntry.file);
  // applyGitignoreFilter includes default patterns, so .git etc. are filtered
  const { included, excluded } = applyGitignoreFilter(files, patterns);

  // If nothing would be excluded beyond defaults, just upload
  if (excluded.length === 0) {
    await uploadFilesWithPaths(included);
    return;
  }

  // If user chose to always use gitignore, filter and upload
  if (gitignoreBehavior === 'always') {
    await uploadFilesWithPaths(included);
    return;
  }

  // Ask the user (behavior === 'ask')
  // Show modal and wait for decision via Promise
  return new Promise<void>((resolve) => {
    openGitignoreModal({
      allFiles: files,
      includedFiles: included,
      excludedFiles: excluded,
      dirName,
      onDecision: async (gitignore, rememberGlobally) => {
        // If user checked "remember", update global setting
        if (rememberGlobally) {
          settingsStore.setUploadSettings({
            gitignoreBehavior: gitignore ? 'always' : 'never',
          });
        }

        const filesToUpload = gitignore ? included : files;
        await uploadFilesWithPaths(filesToUpload);
        resolve();
      },
    });
  });
}
