/**
 * File operations - create, save, upload files
 */
import { LinkType, type CID } from 'hashtree';
import { autosaveIfOwn } from '../nostr';
import { getTree } from '../store';
import { markFilesChanged } from '../stores/recentlyChanged';
import { setUploadProgress } from '../stores/upload';
import { extractArchive } from '../utils/compression';
import { parseRoute } from '../utils/route';
import { getCurrentRootCid, getCurrentPathFromUrl, updateRoute } from './route';
import { initVirtualTree } from './tree';

// Save edited file
export async function saveFile(entryName: string | undefined, content: string): Promise<Uint8Array | null> {
  if (!entryName) return null;

  const rootCid = getCurrentRootCid();
  if (!rootCid) return null;

  const tree = getTree();
  const data = new TextEncoder().encode(content);
  const currentPath = getCurrentPathFromUrl();

  // putFile returns CID with key (encrypted by default)
  const { cid: fileCid, size } = await tree.putFile(data);

  // setEntry uses root CID and entry CID - handles encryption automatically
  const newRootCid = await tree.setEntry(
    rootCid,
    currentPath,
    entryName,
    fileCid,
    size
  );

  // Publish to nostr - resolver will pick up the update automatically
  autosaveIfOwn(newRootCid);

  // Mark file as recently changed for LIVE indicator
  markFilesChanged(new Set([entryName]));

  return data;
}

// Create new file
export async function createFile(name: string, content: string = '') {
  if (!name) return;

  const rootCid = getCurrentRootCid();
  const tree = getTree();
  const data = new TextEncoder().encode(content);
  const currentPath = getCurrentPathFromUrl();

  // putFile returns CID (encrypted by default)
  const { cid: fileCid, size } = await tree.putFile(data);

  if (rootCid) {
    // Add to existing tree
    const newRootCid = await tree.setEntry(
      rootCid,
      currentPath,
      name,
      fileCid,
      size
    );
    // Publish to nostr - resolver will pick up the update
    autosaveIfOwn(newRootCid);
  } else {
    // Initialize virtual tree with this file
    const result = await initVirtualTree([{ name, cid: fileCid, size }]);
    if (!result) return; // Failed to initialize
  }

  // Mark file as recently changed for LIVE indicator
  markFilesChanged(new Set([name]));

  // Navigate to the newly created file with edit mode
  updateRoute(name, { edit: true });
}

// Save binary file (used by DOSBox to sync files back)
export async function saveBinaryFile(entryName: string, data: Uint8Array): Promise<void> {
  if (!entryName) return;

  const rootCid = getCurrentRootCid();
  if (!rootCid) return;

  const tree = getTree();
  const currentPath = getCurrentPathFromUrl();

  const { cid: fileCid, size } = await tree.putFile(data);
  const newRootCid = await tree.setEntry(
    rootCid,
    currentPath,
    entryName,
    fileCid,
    size
  );

  autosaveIfOwn(newRootCid);
  markFilesChanged(new Set([entryName]));
}

// Upload a single file (used for "Keep as ZIP" option)
export async function uploadSingleFile(fileName: string, data: Uint8Array): Promise<void> {
  const tree = getTree();
  const route = parseRoute();
  const currentPath = getCurrentPathFromUrl();

  const { cid: fileCid, size } = await tree.putFile(data);

  let rootCid = getCurrentRootCid();

  if (rootCid) {
    const newRootCid = await tree.setEntry(
      rootCid,
      currentPath,
      fileName,
      fileCid,
      size
    );
    rootCid = newRootCid;
    markFilesChanged(new Set([fileName]));
  } else if (route.npub && route.treeName) {
    // Virtual tree case - initialize and save to nostr
    const result = await initVirtualTree([{ name: fileName, cid: fileCid, size }]);
    if (result) {
      rootCid = result;
      markFilesChanged(new Set([fileName]));
    }
  } else {
    // No tree context - create new encrypted tree
    const result = await tree.putDirectory([{ name: fileName, cid: fileCid, size }]);
    rootCid = result.cid;
    markFilesChanged(new Set([fileName]));
  }

  if (rootCid) {
    autosaveIfOwn(rootCid);
  }
}

// Upload extracted files from an archive
// archiveData is the raw ZIP, archiveName is for extractArchive
// If subdirName is provided, files will be extracted into a subdirectory with that name
export async function uploadExtractedFiles(archiveData: Uint8Array, archiveName: string, subdirName?: string): Promise<void> {
  // Show extracting status
  setUploadProgress({
    current: 0,
    total: 1,
    fileName: 'Extracting archive...',
    bytes: 0,
    totalBytes: archiveData.length,
    status: 'writing',
  });

  // Allow UI to update before heavy sync extraction
  await new Promise(r => setTimeout(r, 50));

  // Extract all files at once (ZIP format requires this - sync operation)
  const extractedFiles = extractArchive(archiveData, archiveName);
  if (extractedFiles.length === 0) {
    setUploadProgress(null);
    return;
  }

  const tree = getTree();
  const currentPath = getCurrentPathFromUrl();
  const total = extractedFiles.length;
  const totalBytes = extractedFiles.reduce((sum, f) => sum + f.data.length, 0);
  let bytesProcessed = 0;

  let rootCid: CID | null = getCurrentRootCid();

  // If extracting to subdirectory, create it first
  if (subdirName) {
    setUploadProgress({
      current: 0,
      total,
      fileName: `Creating ${subdirName}/`,
      bytes: 0,
      totalBytes,
      status: 'writing',
    });

    const { cid: emptyDirCid } = await tree.putDirectory([]);

    if (rootCid) {
      // Add subdirectory to existing tree
      const newRootCid = await tree.setEntry(
        rootCid,
        currentPath,
        subdirName,
        emptyDirCid,
        0,
        LinkType.Dir
      );
      rootCid = newRootCid;
    } else {
      // Initialize virtual tree with the subdirectory
      const result = await initVirtualTree([{ name: subdirName, cid: emptyDirCid, size: 0, type: LinkType.Dir }]);
      if (result) {
        rootCid = result;
      }
    }
  }

  // Base path for extraction (includes subdirName if provided)
  const basePath = subdirName ? [...currentPath, subdirName] : currentPath;

  // Collect all unique directory paths that need to be created
  const dirsToCreate = new Set<string>();
  for (const file of extractedFiles) {
    const pathParts = file.name.split('/');
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

  // Create all directories first
  const createdDirs = new Set<string>();
  for (const dirPathStr of sortedDirs) {
    if (createdDirs.has(dirPathStr)) continue;

    const parts = dirPathStr.split('/');
    const dirName = parts.pop()!;
    const parentPath = [...basePath, ...parts];

    // Create empty directory
    const { cid: emptyDirCid } = await tree.putDirectory([]);

    if (rootCid) {
      const newRootCid = await tree.setEntry(
        rootCid,
        parentPath,
        dirName,
        emptyDirCid,
        0,
        LinkType.Dir
      );
      rootCid = newRootCid;
    } else {
      // First item - create an encrypted tree with this directory
      const result = await tree.putDirectory([{ name: dirName, cid: emptyDirCid, size: 0, type: LinkType.Dir }]);
      rootCid = result.cid;
    }

    createdDirs.add(dirPathStr);
  }

  // Process each extracted file
  for (let i = 0; i < extractedFiles.length; i++) {
    const file = extractedFiles[i];

    // Update progress
    setUploadProgress({
      current: i + 1,
      total,
      fileName: file.name,
      bytes: bytesProcessed,
      totalBytes,
      status: 'writing',
    });

    // Yield to UI every 10 files to keep it responsive
    if (i % 10 === 0) {
      await new Promise(r => setTimeout(r, 0));
    }

    // Store the file
    const { cid: fileCid, size } = await tree.putFile(file.data);
    bytesProcessed += file.data.length;

    // Parse path to handle nested directories
    const pathParts = file.name.split('/');
    const fileName = pathParts.pop()!;
    const targetPath = pathParts.length > 0 ? [...basePath, ...pathParts] : basePath;

    if (rootCid) {
      const newRootCid = await tree.setEntry(
        rootCid,
        targetPath,
        fileName,
        fileCid,
        size,
        LinkType.Blob
      );
      rootCid = newRootCid;
    } else {
      // First file - create an encrypted tree
      const result = await tree.putDirectory([{ name: fileName, cid: fileCid, size }]);
      rootCid = result.cid;
    }

    // Publish periodically (every 50 files) so UI updates without overwhelming
    if (rootCid && (i % 50 === 0 || i === extractedFiles.length - 1)) {
      autosaveIfOwn(rootCid);
    }
  }

  // Final publish to ensure all files are saved
  if (rootCid) {
    autosaveIfOwn(rootCid);
  }

  // Clear progress
  setUploadProgress(null);
}
