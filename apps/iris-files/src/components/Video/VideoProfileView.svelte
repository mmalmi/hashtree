<script lang="ts">
  /**
   * VideoProfileView - User's video channel page
   * Shows user info and their videos (including playlists)
   */
  import { nip19 } from 'nostr-tools';
  import { SvelteSet } from 'svelte/reactivity';
  import { nostrStore } from '../../nostr';
  import { createTreesStore, createProfileStore, type TreeEntry } from '../../stores';
  import { createFollowsStore } from '../../stores/follows';
  import ShareButton from '../ShareButton.svelte';
  import { Avatar, Name, FollowButton } from '../User';
  import VideoCard from './VideoCard.svelte';
  import PlaylistCard from './PlaylistCard.svelte';
  import ProxyImg from '../ProxyImg.svelte';
  import type { VideoItem } from './types';
  import { getFollowers, fetchUserFollows, fetchUserFollowers, socialGraphStore } from '../../utils/socialGraph';
  import { getLocalRootCache, getLocalRootKey } from '../../treeRootCache';
  import { detectPlaylistForCard, MIN_VIDEOS_FOR_STRUCTURE } from '../../stores/playlist';
  import type { CID } from 'hashtree';

  interface PlaylistInfo {
    key: string;
    title: string;
    treeName: string;
    ownerNpub: string | undefined;
    ownerPubkey: string | null;
    visibility: string | undefined;
    href: string;
    videoCount: number;
    thumbnailUrl?: string;
    isPlaylist: true;
  }

  /** Encode tree name for use in URL path */
  function encodeTreeNameForUrl(treeName: string): string {
    return encodeURIComponent(treeName);
  }

  interface Props {
    npub?: string;
  }

  let { npub }: Props = $props();

  // Current user
  let currentUserNpub = $derived($nostrStore.npub);
  let isOwnProfile = $derived(npub === currentUserNpub);

  // Profile owner pubkey
  let ownerPubkey = $derived.by(() => {
    if (!npub) return null;
    try {
      const decoded = nip19.decode(npub);
      if (decoded.type === 'npub') return decoded.data as string;
    } catch {}
    return null;
  });

  // Profile data
  let profileStore = $derived(createProfileStore(npub || ''));
  let profile = $state<{ name?: string; about?: string; picture?: string; banner?: string } | null>(null);

  $effect(() => {
    const store = profileStore;
    const unsub = store.subscribe(value => {
      profile = value;
    });
    return unsub;
  });

  // User's trees
  let treesStore = $derived(createTreesStore(npub));
  let trees = $state<TreeEntry[]>([]);

  $effect(() => {
    const store = treesStore;
    const unsub = store.subscribe(value => {
      trees = value;
    });
    return unsub;
  });

  // Filter to videos only (initial list before playlist detection)
  let videoTrees = $derived(trees.filter(t => t.name.startsWith('videos/')));

  // Track which trees are playlists (detected asynchronously)
  // Using plain object for better Svelte 5 reactivity (Maps don't track well)
  let playlistInfo = $state<Record<string, { videoCount: number; thumbnailUrl?: string }>>({});

  // Track video metadata (duration, title) for single videos
  let videoMetadata = $state<Record<string, { duration?: number; thumbnailUrl?: string; createdAt?: number; title?: string }>>({});

  // Track detection state - use $state so UI can react
  let detectionComplete = $state(false);

  // Detect playlists when trees change - only detect trees we haven't checked yet
  let detectingPlaylists = false;
  let detectedTreeNames = new SvelteSet<string>();
  $effect(() => {
    if (!npub) return;
    const currentVideoTrees = videoTrees;
    if (currentVideoTrees.length === 0) {
      detectionComplete = true;
      return;
    }
    // Find trees we haven't detected yet
    const newTrees = currentVideoTrees.filter(t => !detectedTreeNames.has(t.name));
    if (detectingPlaylists || newTrees.length === 0) {
      // Already detecting or nothing new to detect
      if (!detectingPlaylists) detectionComplete = true;
      return;
    }
    detectingPlaylists = true;
    if (!detectionComplete) detectionComplete = false; // Only show loading on first run
    detectPlaylists(newTrees).finally(() => {
      detectingPlaylists = false;
      detectionComplete = true;
    });
  });

  async function detectPlaylists(treesToCheck: typeof videoTrees) {
    const processTree = async (t: typeof treesToCheck[0]): Promise<void> => {
      // Mark as detected regardless of outcome
      detectedTreeNames.add(t.name);

      try {
        let rootCid: CID | null = null;
        if (t.hash) {
          rootCid = { hash: t.hash, key: t.encryptionKey };
        } else {
          const localHash = getLocalRootCache(npub!, t.name);
          if (localHash) {
            rootCid = { hash: localHash, key: getLocalRootKey(npub!, t.name) };
          }
        }
        if (!rootCid) return;

        const info = await detectPlaylistForCard(rootCid, npub!, t.name);
        if (info && info.videoCount >= MIN_VIDEOS_FOR_STRUCTURE) {
          playlistInfo = { ...playlistInfo, [t.name]: info };
        } else if (info && (info.duration || info.thumbnailUrl)) {
          // Store metadata for single videos (duration, thumbnail)
          videoMetadata = { ...videoMetadata, [t.name]: { duration: info.duration, thumbnailUrl: info.thumbnailUrl, createdAt: info.createdAt, title: info.title } };
        }
      } catch { /* ignore */ }
    };

    // Process trees in parallel (limit 4)
    const CONCURRENCY = 4;
    for (let i = 0; i < treesToCheck.length; i += CONCURRENCY) {
      await Promise.all(treesToCheck.slice(i, i + CONCURRENCY).map(processTree));
    }
  }

  // Get playlist tree names as a Set for efficient lookup
  // Using Object.keys() ensures Svelte tracks the object
  let playlistTreeNames = $derived(new Set(Object.keys(playlistInfo)));

  // Combined list of videos and playlists (only show after detection completes)
  let videos = $derived(
    detectionComplete
      ? videoTrees
          .filter(t => !playlistTreeNames.has(t.name)) // Exclude playlists
          .map(t => ({
            key: `/${npub}/${t.name}`,
            title: videoMetadata[t.name]?.title || t.name.slice(7),
            ownerPubkey: ownerPubkey,
            ownerNpub: npub,
            treeName: t.name,
            visibility: t.visibility,
            href: `#/${npub}/${encodeTreeNameForUrl(t.name)}${t.linkKey ? `?k=${t.linkKey}` : ''}`,
            duration: videoMetadata[t.name]?.duration,
            timestamp: videoMetadata[t.name]?.createdAt,
            isPlaylist: false,
          } as VideoItem))
      : []
  );

  let playlists = $derived(
    detectionComplete
      ? videoTrees
          .filter(t => playlistTreeNames.has(t.name))
          .map(t => {
            const info = playlistInfo[t.name];
            return {
              key: `/${npub}/${t.name}`,
              title: t.name.slice(7),
              ownerPubkey: ownerPubkey,
              ownerNpub: npub,
              treeName: t.name,
              visibility: t.visibility,
              href: `#/${npub}/${encodeTreeNameForUrl(t.name)}`,
              videoCount: info?.videoCount || 0,
              thumbnailUrl: info?.thumbnailUrl,
              isPlaylist: true,
            } as PlaylistInfo;
          })
      : []
  );

  // Follows store for the profile's following count
  let profileFollowsStore = $derived(ownerPubkey ? createFollowsStore(ownerPubkey) : null);
  let profileFollows = $state<string[]>([]);

  $effect(() => {
    if (!profileFollowsStore) {
      profileFollows = [];
      return;
    }
    const unsub = profileFollowsStore.subscribe(value => {
      profileFollows = value?.follows || [];
    });
    return () => {
      unsub();
      profileFollowsStore?.destroy();
    };
  });

  // Social graph for known followers
  let graphVersion = $derived($socialGraphStore.version);
  let knownFollowers = $derived.by(() => {
    graphVersion; // Subscribe to changes
    return ownerPubkey ? getFollowers(ownerPubkey) : new Set();
  });

  // Fetch social graph data when viewing a profile
  $effect(() => {
    if (ownerPubkey) {
      fetchUserFollows(ownerPubkey);
      fetchUserFollowers(ownerPubkey);
    }
  });

  function uploadVideo() {
    window.location.hash = '#/create';
  }

</script>

<div class="flex-1">
  <!-- Banner -->
  <div class="h-32 md:h-48 bg-surface-1">
    {#if profile?.banner}
      <ProxyImg
        src={profile.banner}
        alt=""
        width={1200}
        height={384}
        class="w-full h-full object-cover"
      />
    {/if}
  </div>

  <div class="max-w-6xl mx-auto px-4">
    <!-- Profile header -->
    <div class="flex flex-row items-start gap-4 md:gap-6 py-4 pb-4 mb-4 b-b-1 b-b-solid b-b-surface-3">
      <div class="shrink-0">
        {#if ownerPubkey}
          <div class="md:hidden">
            <Avatar pubkey={ownerPubkey} size={80} />
          </div>
          <div class="hidden md:block">
            <Avatar pubkey={ownerPubkey} size={160} />
          </div>
        {/if}
      </div>

      <div class="flex-1 min-w-0">
        <h1 class="text-2xl font-bold text-text-1">
          {#if ownerPubkey}
            <Name pubkey={ownerPubkey} />
          {:else}
            Unknown
          {/if}
        </h1>
        <div class="flex items-center gap-4 mt-1 text-sm text-text-3">
          <span><span class="font-bold text-text-2">{videos.length + playlists.length}</span> video{videos.length + playlists.length !== 1 ? 's' : ''}{playlists.length > 0 ? ` (${playlists.length} playlist${playlists.length !== 1 ? 's' : ''})` : ''}</span>
          <a href={`#/${npub}/follows`} class="text-text-3 hover:text-text-1 no-underline">
            <span class="font-bold text-text-2">{profileFollows.length}</span> Following
          </a>
          <a href={`#/${npub}/followers`} class="text-text-3 hover:text-text-1 no-underline">
            <span class="font-bold text-text-2">{knownFollowers.size}</span> Known Followers
          </a>
        </div>
        {#if profile?.about}
          <p class="text-text-3 text-sm mt-2 line-clamp-2">{profile.about}</p>
        {/if}
        <!-- Action buttons -->
        <div class="flex items-center gap-2 mt-3 flex-wrap">
          {#if isOwnProfile}
            <a href={`#/${npub}/edit`} class="btn-ghost px-3 py-1.5 text-sm no-underline whitespace-nowrap">
              Edit Profile
            </a>
            <a href="#/users" class="btn-ghost px-3 py-1.5 text-sm no-underline whitespace-nowrap" title="Switch user">
              Switch User
            </a>
            <button onclick={uploadVideo} class="btn-primary px-3 py-1.5 text-sm whitespace-nowrap">
              Upload Video
            </button>
          {:else if ownerPubkey}
            <FollowButton pubkey={ownerPubkey} />
          {/if}
          <ShareButton url={window.location.href} />
        </div>
      </div>
    </div>

    <!-- Playlists section -->
    {#if playlists.length > 0}
      <div class="mb-8">
        <h2 class="text-lg font-semibold text-text-1 mb-4">Playlists</h2>
        <div class="grid grid-cols-1 sm:grid-cols-2 lg:grid-cols-3 xl:grid-cols-4 gap-4">
          {#each playlists as playlist (playlist.href)}
            <PlaylistCard
              href={playlist.href}
              title={playlist.title}
              videoCount={playlist.videoCount}
              thumbnailUrl={playlist.thumbnailUrl}
              ownerPubkey={playlist.ownerPubkey}
              visibility={playlist.visibility}
              hideAuthor
            />
          {/each}
        </div>
      </div>
    {/if}

    <!-- Videos grid -->
    <div class="pb-8">
      {#if !detectionComplete && videoTrees.length > 0}
        <div class="text-center py-12 text-text-3">
          <p>Loading videos...</p>
        </div>
      {:else if videos.length === 0 && playlists.length === 0}
        <div class="text-center py-12 text-text-3">
          {#if isOwnProfile}
            <p>You haven't uploaded any videos yet.</p>
            <button onclick={uploadVideo} class="btn-primary mt-4 px-6 py-2">
              Upload Your First Video
            </button>
          {:else}
            <p>No videos yet.</p>
          {/if}
        </div>
      {:else if videos.length > 0}
        {#if playlists.length > 0}
          <h2 class="text-lg font-semibold text-text-1 mb-4">Videos</h2>
        {/if}
        <div class="grid grid-cols-1 sm:grid-cols-2 lg:grid-cols-3 xl:grid-cols-4 gap-4">
          {#each videos as video (video.href)}
            <VideoCard
              href={video.href}
              title={video.title}
              duration={video.duration}
              ownerPubkey={video.ownerPubkey}
              ownerNpub={video.ownerNpub}
              treeName={video.treeName}
              visibility={video.visibility}
              timestamp={video.timestamp}
              noHover
              hideAuthor
            />
          {/each}
        </div>
      {/if}
    </div>
  </div>
</div>
