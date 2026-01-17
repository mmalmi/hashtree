<script lang="ts" module>
  /**
   * AddToPlaylistModal - Add/remove a video to/from playlists
   * Uses CID (hash + key) to create a reference to the video
   * Shows which playlists already contain the video (like YouTube)
   */
  import type { CID } from 'hashtree';

  export interface AddToPlaylistTarget {
    videoCid: CID;
    videoTitle: string;
    videoSize: number;
  }

  let show = $state(false);
  let target = $state<AddToPlaylistTarget | null>(null);

  export function open(t: AddToPlaylistTarget) {
    target = t;
    show = true;
  }

  export function close() {
    show = false;
    target = null;
  }
</script>

<script lang="ts">
  import { nostrStore, saveHashtree } from '../../nostr';
  import { getTree } from '../../store';
  import { createTreesStore, type TreeEntry } from '../../stores/trees';
  import { LinkType, cid as makeCid } from 'hashtree';
  import { hasVideoFile, MIN_VIDEOS_FOR_STRUCTURE } from '../../stores/playlist';

  let npub = $derived($nostrStore.npub);

  interface PlaylistInfo {
    name: string;
    displayName: string;
    videoCount: number;
    rootCid: CID;
    containsVideo: boolean;
    videoEntryName: string | null; // Name of entry if video is in playlist
    visibility?: 'public' | 'link-visible' | 'private'; // Original visibility to preserve
  }

  // State
  let loading = $state(true);
  let playlists = $state<PlaylistInfo[]>([]);
  let pendingOps = $state<Set<string>>(new Set()); // Playlist names with pending operations
  let error = $state<string | null>(null);
  let createNew = $state(false);
  let inputRef = $state<HTMLInputElement | null>(null);
  let modalInput = $state('');

  // Track locally created playlists to avoid race condition with subscription
  let locallyCreatedPlaylists = $state<Map<string, PlaylistInfo>>(new Map());

  // Subscribe to trees store to get user's playlists
  let treesUnsubscribe: (() => void) | null = null;

  // Load user's playlists when modal opens
  $effect(() => {
    if (show && npub) {
      loadPlaylists();
    }
    if (!show) {
      // Reset state when modal closes
      loading = true;
      playlists = [];
      pendingOps = new Set();
      error = null;
      createNew = false;
      modalInput = '';
      locallyCreatedPlaylists = new Map();
      // Unsubscribe from trees store
      if (treesUnsubscribe) {
        treesUnsubscribe();
        treesUnsubscribe = null;
      }
    }
  });

  // Focus input when creating new playlist
  $effect(() => {
    if (createNew && inputRef) {
      inputRef.focus();
    }
  });

  /** Check if two CIDs have the same hash */
  function cidHashEquals(a: CID, b: CID): boolean {
    if (a.hash.length !== b.hash.length) return false;
    for (let i = 0; i < a.hash.length; i++) {
      if (a.hash[i] !== b.hash[i]) return false;
    }
    return true;
  }

  function loadPlaylists() {
    if (!npub || !target) return;

    loading = true;
    error = null;

    // Unsubscribe from previous subscription
    if (treesUnsubscribe) {
      treesUnsubscribe();
    }

    // Subscribe to user's trees
    const treesStore = createTreesStore(npub);
    treesUnsubscribe = treesStore.subscribe(async (trees: TreeEntry[]) => {
      // Filter for video trees (playlists)
      const videoTrees = trees.filter(t => t.name.startsWith('videos/'));

      if (videoTrees.length === 0) {
        playlists = [];
        loading = false;
        return;
      }

      const tree = getTree();
      const foundPlaylists: PlaylistInfo[] = [];

      for (const treeEntry of videoTrees) {
        const rootCid: CID = treeEntry.encryptionKey
          ? makeCid(treeEntry.hash, treeEntry.encryptionKey)
          : { hash: treeEntry.hash };

        try {
          const entries = await tree.listDirectory(rootCid);
          if (!entries || entries.length === 0) continue;

          // Check if this is a playlist (has subdirectories with videos)
          // Also check if target video is already in this playlist
          let videoCount = 0;
          let containsVideo = false;
          let videoEntryName: string | null = null;

          for (const entry of entries) {
            try {
              const subEntries = await tree.listDirectory(entry.cid);
              if (subEntries && hasVideoFile(subEntries)) {
                videoCount++;
                // Check if this entry's CID matches the target video CID
                if (target && cidHashEquals(entry.cid, target.videoCid)) {
                  containsVideo = true;
                  videoEntryName = entry.name;
                }
              }
            } catch {
              // Not a directory - might be a single video
            }
          }

          if (videoCount >= MIN_VIDEOS_FOR_STRUCTURE) {
            // It's a playlist with video subdirectories
            foundPlaylists.push({
              name: treeEntry.name,
              displayName: treeEntry.name.replace(/^videos\//, ''),
              videoCount,
              rootCid,
              containsVideo,
              videoEntryName,
              visibility: treeEntry.visibility,
            });
          }
        } catch {
          // Failed to read tree
        }
      }

      // Merge locally created playlists that aren't in foundPlaylists yet
      // This handles the race condition where detection hasn't found the new playlist
      const mergedPlaylists = [...foundPlaylists];
      for (const [name, localPlaylist] of locallyCreatedPlaylists) {
        if (!mergedPlaylists.some(p => p.name === name)) {
          mergedPlaylists.push(localPlaylist);
        }
      }

      playlists = mergedPlaylists;
      loading = false;
    });
  }

  async function togglePlaylist(playlist: PlaylistInfo) {
    if (!target || !npub || pendingOps.has(playlist.name)) return;

    pendingOps = new Set([...pendingOps, playlist.name]);
    error = null;

    try {
      const tree = getTree();

      if (playlist.containsVideo && playlist.videoEntryName) {
        // Remove from playlist
        const newRoot = await tree.removeEntry(playlist.rootCid, [], playlist.videoEntryName);

        // Save and publish to Nostr, preserving original visibility
        await saveHashtree(playlist.name, newRoot, { visibility: playlist.visibility || 'public' });

        // Update local state
        playlists = playlists.map(p =>
          p.name === playlist.name
            ? { ...p, containsVideo: false, videoEntryName: null, videoCount: p.videoCount - 1, rootCid: newRoot }
            : p
        );
      } else {
        // Add to playlist
        const videoId = `video_${Date.now()}`;

        const newRoot = await tree.setEntry(
          playlist.rootCid,
          [], // at root level
          videoId,
          target.videoCid,
          target.videoSize,
          LinkType.Dir
        );

        // Save and publish to Nostr, preserving original visibility
        await saveHashtree(playlist.name, newRoot, { visibility: playlist.visibility || 'public' });

        // Update local state
        playlists = playlists.map(p =>
          p.name === playlist.name
            ? { ...p, containsVideo: true, videoEntryName: videoId, videoCount: p.videoCount + 1, rootCid: newRoot }
            : p
        );
      }
    } catch (e) {
      console.error('Failed to update playlist:', e);
      error = e instanceof Error ? e.message : 'Failed to update playlist';
    } finally {
      pendingOps = new Set([...pendingOps].filter(n => n !== playlist.name));
    }
  }

  async function createPlaylist(e: Event) {
    e.preventDefault();
    const name = modalInput.trim();
    if (!name || !target || !npub || pendingOps.has('__new__')) return;

    pendingOps = new Set([...pendingOps, '__new__']);
    error = null;

    try {
      const tree = getTree();
      const treeName = `videos/${name}`;

      // Generate a unique ID for the video entry
      const videoId = `video_${Date.now()}`;

      // Create directory entries with the video reference
      const entries = [{
        name: videoId,
        cid: target.videoCid,
        size: target.videoSize,
        type: LinkType.Dir,
      }];

      // Create the playlist directory
      const result = await tree.putDirectory(entries, {});

      // Save and publish to Nostr
      await saveHashtree(treeName, result.cid, { visibility: 'public' });

      // Track locally created playlist to survive subscription updates
      const newPlaylist: PlaylistInfo = {
        name: treeName,
        displayName: name,
        videoCount: 1,
        rootCid: result.cid,
        containsVideo: true,
        videoEntryName: videoId,
      };
      locallyCreatedPlaylists = new Map([...locallyCreatedPlaylists, [treeName, newPlaylist]]);

      // Add to local state
      playlists = [...playlists, newPlaylist];

      // Go back to list view
      createNew = false;
      modalInput = '';
    } catch (e) {
      console.error('Failed to create playlist:', e);
      error = e instanceof Error ? e.message : 'Failed to create playlist';
    } finally {
      pendingOps = new Set([...pendingOps].filter(n => n !== '__new__'));
    }
  }

  function isPending(playlistName: string): boolean {
    return pendingOps.has(playlistName);
  }
</script>

{#if show && target}
  <!-- svelte-ignore a11y_click_events_have_key_events -->
  <!-- svelte-ignore a11y_no_static_element_interactions -->
  <div class="fixed inset-0 z-50 flex items-center justify-center bg-black/70" onclick={close}>
    <div class="bg-surface-1 rounded-lg shadow-lg w-full max-w-md mx-4" onclick={(e) => e.stopPropagation()}>
      <!-- Header -->
      <div class="p-4 border-b border-surface-3 flex items-center justify-between">
        <h2 class="text-lg font-semibold">Save to playlist</h2>
        <button onclick={close} class="btn-ghost p-1">
          <span class="i-lucide-x text-lg"></span>
        </button>
      </div>

      <!-- Content -->
      <div class="p-4 max-h-80 overflow-auto">
        {#if loading}
          <div class="flex items-center justify-center py-8">
            <span class="i-lucide-loader-2 text-2xl animate-spin text-text-3"></span>
          </div>
        {:else if createNew}
          <!-- Create new playlist form -->
          <form onsubmit={createPlaylist} class="space-y-4">
            <div>
              <label for="playlist-name" class="text-sm text-text-2 mb-1 block">Playlist name</label>
              <input
                id="playlist-name"
                bind:this={inputRef}
                type="text"
                placeholder="My Playlist"
                value={modalInput}
                oninput={(e) => modalInput = (e.target as HTMLInputElement).value}
                class="input w-full"
              />
            </div>
            {#if error}
              <p class="text-danger text-sm">{error}</p>
            {/if}
            <div class="flex justify-end gap-2">
              <button type="button" onclick={() => createNew = false} class="btn-ghost" disabled={isPending('__new__')}>
                Back
              </button>
              <button type="submit" class="btn-primary" disabled={isPending('__new__') || !modalInput.trim()}>
                {#if isPending('__new__')}
                  <span class="i-lucide-loader-2 animate-spin mr-1"></span>
                {/if}
                Create
              </button>
            </div>
          </form>
        {:else}
          <!-- Playlist list with checkboxes -->
          <div class="space-y-1">
            {#if playlists.length > 0}
              {#each playlists as playlistItem (playlistItem.name)}
                <button
                  onclick={() => togglePlaylist(playlistItem)}
                  disabled={isPending(playlistItem.name)}
                  class="w-full flex items-center gap-3 p-3 rounded-lg hover:bg-surface-2 transition-colors text-left"
                >
                  <!-- Checkbox-style indicator -->
                  <div class="w-5 h-5 flex items-center justify-center shrink-0">
                    {#if isPending(playlistItem.name)}
                      <span class="i-lucide-loader-2 animate-spin text-text-3"></span>
                    {:else if playlistItem.containsVideo}
                      <span class="i-lucide-check-square text-xl text-accent"></span>
                    {:else}
                      <span class="i-lucide-square text-xl text-text-3"></span>
                    {/if}
                  </div>
                  <div class="flex-1 min-w-0">
                    <p class="text-text-1 truncate">{playlistItem.displayName}</p>
                    <p class="text-xs text-text-3">{playlistItem.videoCount} video{playlistItem.videoCount === 1 ? '' : 's'}</p>
                  </div>
                </button>
              {/each}
            {:else}
              <p class="text-text-3 text-center py-4">No playlists yet</p>
            {/if}

            {#if error}
              <p class="text-danger text-sm text-center mt-2">{error}</p>
            {/if}
          </div>

          <!-- Create new button -->
          <button
            onclick={() => createNew = true}
            class="w-full mt-4 flex items-center justify-center gap-2 p-3 rounded-lg border border-dashed border-surface-3 hover:border-accent hover:bg-surface-2 transition-colors"
          >
            <span class="i-lucide-plus text-lg"></span>
            <span>Create new playlist</span>
          </button>
        {/if}
      </div>

      <!-- Footer with video info -->
      <div class="p-4 border-t border-surface-3 bg-surface-2 rounded-b-lg flex items-center justify-between">
        <p class="text-sm text-text-2 truncate flex-1 mr-2">
          <span class="i-lucide-video text-sm mr-1"></span>
          {target.videoTitle}
        </p>
        <button onclick={close} class="btn-ghost text-sm">
          Done
        </button>
      </div>
    </div>
  </div>
{/if}
