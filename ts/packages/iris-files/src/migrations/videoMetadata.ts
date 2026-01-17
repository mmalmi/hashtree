/**
 * Video metadata migration utility
 *
 * Migrates video metadata from metadata.json/title.txt/description.txt
 * to link entry meta fields.
 */

import { getTree } from '../store';
import { updateLocalRootCacheHex } from '../treeRootCache';
import { toHex, LinkType, nhashEncode } from 'hashtree';
import type { CID, Hash, TreeVisibility } from 'hashtree';
import { getRefResolver } from '../refResolver';
import { getWorkerAdapter } from '../lib/workerInit';

/** Check if an entry is a video file */
function isVideoFile(name: string): boolean {
  return (
    name.startsWith('video.') ||
    name.endsWith('.webm') ||
    name.endsWith('.mp4') ||
    name.endsWith('.mov') ||
    name.endsWith('.mkv')
  );
}

/** Check if directory has a video file (is a single video, not playlist) */
function hasVideoFile(entries: Array<{ name: string }>): boolean {
  return entries.some(e => isVideoFile(e.name));
}

interface MigrationStats {
  videosProcessed: number;
  videosMigrated: number;
  playlistsProcessed: number;
  errors: number;
}

interface TreeInfo {
  name: string;
  hash: Hash;
  key?: Hash;
  visibility?: TreeVisibility;
}

/**
 * Fetch all video trees for a user using RefResolver
 * Waits for initial data burst to complete before returning
 */
async function fetchVideoTrees(npub: string): Promise<TreeInfo[]> {
  return new Promise((resolve) => {
    const resolver = getRefResolver();
    if (!resolver.list) {
      resolve([]);
      return;
    }

    const videoTrees: TreeInfo[] = [];
    let lastUpdateTime = Date.now();
    let checkInterval: ReturnType<typeof setInterval>;
    const unsub: { current?: () => void } = {};

    const checkStable = () => {
      // If no updates for 1 second, consider data stable
      if (Date.now() - lastUpdateTime > 1000) {
        clearInterval(checkInterval);
        unsub.current?.();
        resolve(videoTrees);
      }
    };

    // Start checking for stability after 500ms
    setTimeout(() => {
      checkInterval = setInterval(checkStable, 200);
    }, 500);

    // Maximum wait of 10 seconds
    setTimeout(() => {
      clearInterval(checkInterval);
      unsub.current?.();
      resolve(videoTrees);
    }, 10000);

    unsub.current = resolver.list(npub, (entries) => {
      lastUpdateTime = Date.now();
      videoTrees.length = 0; // Clear and rebuild

      for (const entry of entries) {
        const slashIdx = entry.key.indexOf('/');
        const name = slashIdx >= 0 ? entry.key.slice(slashIdx + 1) : '';

        if (name.startsWith('videos/')) {
          videoTrees.push({
            name,
            hash: entry.cid.hash,
            key: entry.cid.key,
            visibility: entry.visibility,
          });
        }
      }
    });
  });
}

/**
 * Run the metadata migration for all user's video trees
 */
export async function runVideoMetadataMigration(npub: string): Promise<void> {
  console.log('[Migration] Starting video metadata migration...');
  const stats: MigrationStats = {
    videosProcessed: 0,
    videosMigrated: 0,
    playlistsProcessed: 0,
    errors: 0,
  };

  const tree = getTree();

  // Fetch all video trees from relay
  const videoTrees = await fetchVideoTrees(npub);
  console.log('[Migration] Found video trees:', videoTrees.length);

  for (const treeInfo of videoTrees) {
    try {
      const rootCid: CID = treeInfo.key
        ? { hash: treeInfo.hash, key: treeInfo.key }
        : { hash: treeInfo.hash };
      const visibility = treeInfo.visibility || 'public';

      const entries = await tree.listDirectory(rootCid);
      if (!entries) {
        console.log('[Migration] No entries for:', treeInfo.name);
        continue;
      }
      console.log('[Migration] Processing:', treeInfo.name, 'entries:', entries.map(e => e.name));

      if (hasVideoFile(entries)) {
        // Single video - migrate it
        const migrated = await migrateSingleVideo(
          tree, npub, treeInfo.name, rootCid, entries, visibility
        );
        stats.videosProcessed++;
        if (migrated) stats.videosMigrated++;
      } else {
        // Playlist - migrate each video in it, accumulating changes
        stats.playlistsProcessed++;
        let currentRootCid = rootCid;
        let playlistModified = false;

        for (const entry of entries) {
          try {
            const subEntries = await tree.listDirectory(entry.cid);
            if (subEntries && hasVideoFile(subEntries)) {
              const result = await migratePlaylistVideo(
                tree, npub, treeInfo.name, currentRootCid, entry, subEntries, visibility
              );
              stats.videosProcessed++;
              if (result) {
                currentRootCid = result;
                playlistModified = true;
                stats.videosMigrated++;
              }
            }
          } catch {
            stats.errors++;
          }
        }

        // Push to Blossom and save once after all videos are processed
        if (playlistModified) {
          const adapter = getWorkerAdapter();
          if (adapter) {
            try {
              const pushResult = await adapter.pushToBlossom(currentRootCid.hash, currentRootCid.key, treeInfo.name);
              console.log('[Migration] Blossom push:', treeInfo.name, pushResult);
            } catch (e) {
              console.warn('[Migration] Blossom push failed:', treeInfo.name, e);
            }
          }

          updateLocalRootCacheHex(
            npub,
            treeInfo.name,
            toHex(currentRootCid.hash),
            currentRootCid.key ? toHex(currentRootCid.key) : undefined,
            visibility
          );
          console.log('[Migration] Migrated playlist:', treeInfo.name);
        }
      }
    } catch (e) {
      console.error('[Migration] Error processing tree:', treeInfo.name, e);
      stats.errors++;
    }
  }

  console.log('[Migration] Complete:', stats);
}

/**
 * Migrate a single video's metadata to link entry
 */
async function migrateSingleVideo(
  tree: ReturnType<typeof getTree>,
  npub: string,
  treeName: string,
  rootCid: CID,
  entries: Awaited<ReturnType<typeof tree.listDirectory>>,
  visibility: TreeVisibility
): Promise<boolean> {
  // Find video entry
  const videoEntry = entries?.find(e => isVideoFile(e.name));
  if (!videoEntry) {
    console.log('[Migration] No video file found in:', treeName);
    return false;
  }

  // Check if already migrated (has title in meta)
  const existingMeta = (videoEntry.meta as Record<string, unknown>) || {};
  if (existingMeta.title) {
    console.log('[Migration] Already migrated:', treeName);
    return false;
  }

  // Check if there's metadata to migrate
  const hasMetadataJson = entries?.some(e => e.name === 'metadata.json');
  const hasTitleTxt = entries?.some(e => e.name === 'title.txt');
  const hasDescTxt = entries?.some(e => e.name === 'description.txt');

  if (!hasMetadataJson && !hasTitleTxt) {
    console.log('[Migration] No metadata files to migrate:', treeName);
    return false;
  }

  // Read legacy metadata
  const newMeta: Record<string, unknown> = { ...existingMeta };

  // Try metadata.json first
  if (hasMetadataJson) {
    try {
      const result = await tree.resolvePath(rootCid, 'metadata.json');
      if (result) {
        const data = await tree.readFile(result.cid);
        if (data) {
          const metadata = JSON.parse(new TextDecoder().decode(data));
          if (metadata.title) newMeta.title = metadata.title;
          if (metadata.description) newMeta.description = metadata.description;
          if (metadata.createdAt) newMeta.createdAt = metadata.createdAt;
          if (metadata.originalDate) newMeta.originalDate = metadata.originalDate;
          if (metadata.duration) newMeta.duration = metadata.duration;
        }
      }
    } catch {}
  }

  // Fall back to title.txt
  if (!newMeta.title && hasTitleTxt) {
    try {
      const result = await tree.resolvePath(rootCid, 'title.txt');
      if (result) {
        const data = await tree.readFile(result.cid);
        if (data) {
          newMeta.title = new TextDecoder().decode(data).trim();
        }
      }
    } catch {}
  }

  // Fall back to description.txt
  if (!newMeta.description && hasDescTxt) {
    try {
      const result = await tree.resolvePath(rootCid, 'description.txt');
      if (result) {
        const data = await tree.readFile(result.cid);
        if (data) {
          newMeta.description = new TextDecoder().decode(data).trim();
        }
      }
    } catch {}
  }

  if (!newMeta.title) {
    console.log('[Migration] Could not read title from metadata files:', treeName);
    return false;
  }

  console.log('[Migration] Migrating:', treeName, 'title:', newMeta.title);

  // Update video entry with metadata
  let newRootCid = await tree.setEntry(
    rootCid,
    [],
    videoEntry.name,
    videoEntry.cid,
    videoEntry.size,
    videoEntry.type,
    newMeta
  );

  // Delete old metadata files
  if (hasMetadataJson) {
    try { newRootCid = await tree.removeEntry(newRootCid, [], 'metadata.json'); } catch {}
  }
  if (hasTitleTxt) {
    try { newRootCid = await tree.removeEntry(newRootCid, [], 'title.txt'); } catch {}
  }
  if (hasDescTxt) {
    try { newRootCid = await tree.removeEntry(newRootCid, [], 'description.txt'); } catch {}
  }

  // Push to Blossom first (so other users can fetch the data)
  const adapter = getWorkerAdapter();
  if (adapter) {
    try {
      const pushResult = await adapter.pushToBlossom(newRootCid.hash, newRootCid.key, treeName);
      console.log('[Migration] Blossom push:', treeName, pushResult);
    } catch (e) {
      console.warn('[Migration] Blossom push failed:', treeName, e);
    }
  }

  // Save updated tree root to Nostr
  updateLocalRootCacheHex(
    npub,
    treeName,
    toHex(newRootCid.hash),
    newRootCid.key ? toHex(newRootCid.key) : undefined,
    visibility
  );

  console.log('[Migration] Migrated single video:', treeName);
  return true;
}

/**
 * Migrate a playlist video's metadata to both video link entry and parent link entry
 * Returns the new root CID if migrated, null if skipped
 */
async function migratePlaylistVideo(
  tree: ReturnType<typeof getTree>,
  _npub: string,
  _treeName: string,
  playlistRootCid: CID,
  parentEntry: { name: string; cid: CID; size: number; type: LinkType; meta?: Record<string, unknown> },
  subEntries: Awaited<ReturnType<typeof tree.listDirectory>>,
  _visibility: TreeVisibility
): Promise<CID | null> {
  // Find video entry in subdirectory
  const videoEntry = subEntries?.find(e => isVideoFile(e.name));
  if (!videoEntry) return null;

  // Check if already migrated (parent has title in meta)
  const parentMeta = (parentEntry.meta as Record<string, unknown>) || {};
  if (parentMeta.title) return null;

  // Check if there's metadata to migrate
  const hasMetadataJson = subEntries?.some(e => e.name === 'metadata.json');
  const hasTitleTxt = subEntries?.some(e => e.name === 'title.txt');
  const hasDescTxt = subEntries?.some(e => e.name === 'description.txt');
  const hasInfoJson = subEntries?.some(e => e.name === 'info.json');

  if (!hasMetadataJson && !hasTitleTxt && !hasInfoJson) return null;

  // Read legacy metadata
  const newMeta: Record<string, unknown> = { ...parentMeta };
  const videoMeta: Record<string, unknown> = { ...(videoEntry.meta as Record<string, unknown> || {}) };

  // Try metadata.json first
  if (hasMetadataJson) {
    try {
      const result = await tree.resolvePath(parentEntry.cid, 'metadata.json');
      if (result) {
        const data = await tree.readFile(result.cid);
        if (data) {
          const metadata = JSON.parse(new TextDecoder().decode(data));
          if (metadata.title) {
            newMeta.title = metadata.title;
            videoMeta.title = metadata.title;
          }
          if (metadata.description) {
            newMeta.description = metadata.description;
            videoMeta.description = metadata.description;
          }
          if (metadata.createdAt) {
            newMeta.createdAt = metadata.createdAt;
            videoMeta.createdAt = metadata.createdAt;
          }
          if (metadata.originalDate) {
            newMeta.originalDate = metadata.originalDate;
            videoMeta.originalDate = metadata.originalDate;
          }
          if (metadata.duration) {
            newMeta.duration = metadata.duration;
            videoMeta.duration = metadata.duration;
          }
        }
      }
    } catch {}
  }

  // Try info.json (yt-dlp format)
  if (!newMeta.title && hasInfoJson) {
    try {
      const result = await tree.resolvePath(parentEntry.cid, 'info.json');
      if (result) {
        const data = await tree.readFile(result.cid);
        if (data) {
          const info = JSON.parse(new TextDecoder().decode(data));
          if (info.title) {
            newMeta.title = info.title;
            videoMeta.title = info.title;
          }
          if (info.description) {
            newMeta.description = info.description;
            videoMeta.description = info.description;
          }
          if (info.duration) {
            newMeta.duration = info.duration;
            videoMeta.duration = info.duration;
          }
        }
      }
    } catch {}
  }

  // Fall back to title.txt
  if (!newMeta.title && hasTitleTxt) {
    try {
      const result = await tree.resolvePath(parentEntry.cid, 'title.txt');
      if (result) {
        const data = await tree.readFile(result.cid);
        if (data) {
          const title = new TextDecoder().decode(data).trim();
          newMeta.title = title;
          videoMeta.title = title;
        }
      }
    } catch {}
  }

  // Fall back to description.txt
  if (!newMeta.description && hasDescTxt) {
    try {
      const result = await tree.resolvePath(parentEntry.cid, 'description.txt');
      if (result) {
        const data = await tree.readFile(result.cid);
        if (data) {
          const desc = new TextDecoder().decode(data).trim();
          newMeta.description = desc;
          videoMeta.description = desc;
        }
      }
    } catch {}
  }

  if (!newMeta.title) return null;

  // Find thumbnail for parent meta
  const thumbEntry = subEntries?.find(e =>
    e.name.startsWith('thumbnail.') ||
    e.name.endsWith('.jpg') ||
    e.name.endsWith('.webp') ||
    e.name.endsWith('.png')
  );
  if (thumbEntry && !newMeta.thumbnail) {
    newMeta.thumbnail = nhashEncode(thumbEntry.cid);
  }

  // Update video entry within subdirectory
  let newSubDirCid = await tree.setEntry(
    parentEntry.cid,
    [],
    videoEntry.name,
    videoEntry.cid,
    videoEntry.size,
    videoEntry.type,
    videoMeta
  );

  // Delete old metadata files from subdirectory
  if (hasMetadataJson) {
    try { newSubDirCid = await tree.removeEntry(newSubDirCid, [], 'metadata.json'); } catch {}
  }
  if (hasTitleTxt) {
    try { newSubDirCid = await tree.removeEntry(newSubDirCid, [], 'title.txt'); } catch {}
  }
  if (hasDescTxt) {
    try { newSubDirCid = await tree.removeEntry(newSubDirCid, [], 'description.txt'); } catch {}
  }

  // Calculate new size for subdirectory
  const newSubEntries = await tree.listDirectory(newSubDirCid);
  const newSize = newSubEntries?.reduce((sum, e) => sum + e.size, 0) || parentEntry.size;

  // Update parent entry with new subdirectory CID and metadata
  const newRootCid = await tree.setEntry(
    playlistRootCid,
    [],
    parentEntry.name,
    newSubDirCid,
    newSize,
    LinkType.Dir,
    newMeta
  );

  // Return the new root CID - Blossom push and save happen at playlist level
  return newRootCid;
}
