<script lang="ts">
  /**
   * DocsProfileView - Profile page for docs.iris.to
   * Shows user profile header followed by their documents as A4 cards
   */
  import { nip19 } from 'nostr-tools';
  import { nostrStore } from '../../nostr';
  import { createProfileStore } from '../../stores/profile';
  import { createFollowsStore, followPubkey, unfollowPubkey } from '../../stores/follows';
  import { createTreesStore, type TreeEntry } from '../../stores';
  import { open as openCreateModal } from '../Modals/CreateModal.svelte';
  import { open as openShareModal } from '../Modals/ShareModal.svelte';
  import { Avatar, Name, Badge, FollowedBy } from '../User';
  import CopyText from '../CopyText.svelte';
  import ProxyImg from '../ProxyImg.svelte';
  import { getFollowsMe, getFollowers, socialGraphStore } from '../../utils/socialGraph';
  import DocCard from './DocCard.svelte';

  interface Props {
    npub: string;
  }

  let { npub }: Props = $props();

  let myPubkey = $derived($nostrStore.pubkey);
  let myNpub = $derived($nostrStore.npub);
  let isLoggedIn = $derived($nostrStore.isLoggedIn);

  // Decode npub to hex pubkey
  let pubkeyHex = $derived.by(() => {
    try {
      const decoded = nip19.decode(npub);
      return decoded.data as string;
    } catch {
      return '';
    }
  });

  let isOwnProfile = $derived(myPubkey === pubkeyHex);

  // Profile store
  let profileStore = $derived(createProfileStore(npub));
  let profile = $state<{ name?: string; display_name?: string; about?: string; banner?: string; picture?: string; nip05?: string; website?: string } | null>(null);

  $effect(() => {
    const store = profileStore;
    const unsub = store.subscribe(value => {
      profile = value;
    });
    return unsub;
  });

  // Follows store for my follows (to check if I'm following this user)
  let myFollowsStore = $derived(myPubkey ? createFollowsStore(myPubkey) : null);
  let myFollows = $state<string[]>([]);

  $effect(() => {
    if (!myFollowsStore) {
      myFollows = [];
      return;
    }
    const unsub = myFollowsStore.subscribe(value => {
      myFollows = value?.follows || [];
    });
    return () => {
      unsub();
      myFollowsStore?.destroy();
    };
  });

  // Follows store for the viewed profile (to show their following count)
  let profileFollowsStore = $derived(pubkeyHex ? createFollowsStore(pubkeyHex) : null);
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

  // Social graph reactive state
  let graphVersion = $derived($socialGraphStore.version);

  let isFollowing = $derived.by(() => {
    graphVersion;
    return myFollows.includes(pubkeyHex);
  });

  let followsMe = $derived.by(() => {
    graphVersion;
    return getFollowsMe(pubkeyHex);
  });

  let knownFollowers = $derived.by(() => {
    graphVersion;
    return getFollowers(pubkeyHex);
  });

  // Trees store for this user's docs
  let treesStore = $derived(createTreesStore(npub));
  let trees = $state<TreeEntry[]>([]);

  $effect(() => {
    const store = treesStore;
    const unsub = store.subscribe(value => {
      trees = value;
    });
    return unsub;
  });

  // Filter to only show document trees (docs/ prefix)
  let docs = $derived(
    trees
      .filter(t => t.name.startsWith('docs/'))
      .map(t => ({ ...t, displayName: t.name.slice(5) }))
  );

  let bannerError = $state(false);
  let followLoading = $state(false);

  function navigate(path: string) {
    window.location.hash = path;
  }

  async function handleFollow() {
    followLoading = true;
    if (isFollowing) {
      await unfollowPubkey(pubkeyHex);
    } else {
      await followPubkey(pubkeyHex);
    }
    followLoading = false;
  }

  function createNewDoc() {
    if (!myNpub) {
      alert('Please sign in to create a document');
      return;
    }
    openCreateModal('document');
  }
</script>

<div class="flex-1 flex flex-col min-h-0 bg-surface-0 overflow-y-auto">
  <!-- Banner -->
  <div class="h-32 md:h-40 bg-surface-2 relative shrink-0">
    {#if profile?.banner && !bannerError}
      <ProxyImg
        src={profile.banner}
        alt=""
        width={1200}
        height={320}
        class="w-full h-full object-cover"
        onerror={() => bannerError = true}
      />
    {/if}
  </div>

  <!-- Profile header -->
  <div class="px-4 pb-4 -mt-12 relative">
    <!-- Avatar -->
    <div class="mb-3">
      <Avatar pubkey={pubkeyHex} size={80} class="border-4 border-surface-0" />
    </div>

    <!-- Name and action buttons -->
    <div class="flex items-center justify-between gap-2">
      <div class="flex items-center gap-2 min-w-0">
        <h1 class="text-xl font-bold text-text-1 m-0 truncate">
          <Name pubkey={pubkeyHex} />
        </h1>
        {#if isOwnProfile}
          <span class="shrink-0 text-xs text-blue-500 flex items-center gap-1">
            <Badge pubKeyHex={pubkeyHex} size="sm" /> You
          </span>
        {:else if isFollowing}
          <span class="shrink-0 text-xs text-blue-500 flex items-center gap-1">
            <Badge pubKeyHex={pubkeyHex} size="sm" /> Following
          </span>
        {/if}
        {#if !isOwnProfile && followsMe}
          <span class="shrink-0 text-xs bg-surface-2 text-text-2 px-2 py-0.5 rounded">
            Follows you
          </span>
        {/if}
      </div>
      <div class="flex items-center gap-2 shrink-0">
        {#if isLoggedIn && isOwnProfile}
          <button
            onclick={() => navigate('/users')}
            class="btn-ghost"
            title="Switch user"
          >
            Switch User
          </button>
          <button
            onclick={() => navigate(`/${npub}/edit`)}
            class="btn-ghost"
          >
            Edit Profile
          </button>
        {/if}
        {#if isLoggedIn && !isOwnProfile}
          <button
            onclick={handleFollow}
            disabled={followLoading}
            class={isFollowing ? 'btn-ghost' : 'btn-success'}
          >
            {followLoading ? '...' : isFollowing ? 'Unfollow' : 'Follow'}
          </button>
        {/if}
        <button
          onclick={() => openShareModal(window.location.href)}
          class="btn-ghost"
          title="Share"
        >
          <span class="i-lucide-share text-base"></span>
        </button>
      </div>
    </div>

    <!-- npub with copy -->
    <CopyText
      text={npub}
      displayText={npub.slice(0, 8) + '...' + npub.slice(-4)}
      class="text-sm mt-1"
      testId="copy-npub"
    />

    {#if profile?.nip05}
      <div class="text-sm text-accent mt-1">{profile.nip05}</div>
    {/if}

    <!-- Followed by friends -->
    {#if !isOwnProfile && pubkeyHex}
      <FollowedBy pubkey={pubkeyHex} class="mt-2" />
    {/if}

    <!-- About -->
    {#if profile?.about}
      <p class="text-sm text-text-2 mt-3 whitespace-pre-wrap break-words">
        {profile.about}
      </p>
    {/if}

    <!-- Stats -->
    <div class="flex gap-4 mt-4 text-sm">
      <a
        href="#/{npub}/follows"
        class="text-text-2 hover:text-text-1"
      >
        <span class="font-bold text-text-1">{profileFollows.length}</span> Following
      </a>
      <a
        href="#/{npub}/followers"
        class="text-text-2 hover:text-text-1"
      >
        <span class="font-bold text-text-1">{knownFollowers.size}</span> Known Followers
      </a>
    </div>

    <!-- Website -->
    {#if profile?.website}
      <a
        href={profile.website.startsWith('http') ? profile.website : `https://${profile.website}`}
        target="_blank"
        rel="noopener noreferrer"
        class="text-sm text-accent mt-3 inline-block hover:underline"
      >
        {profile.website}
      </a>
    {/if}
  </div>

  <!-- Documents section -->
  <div class="px-4 pb-6">
    <h2 class="text-lg font-semibold text-text-1 mb-4">Documents</h2>

    <div class="grid gap-4" style="grid-template-columns: repeat(auto-fill, minmax(10rem, 1fr))">
      <!-- New Document card (only for own profile) -->
      {#if isOwnProfile}
        <div
          onclick={createNewDoc}
          class="bg-surface-1 rounded-lg b-1 b-dashed b-surface-3 hover:b-accent transition-colors cursor-pointer flex flex-col items-center justify-center gap-2"
          style="aspect-ratio: 210 / 297"
          role="button"
          tabindex="0"
        >
          <span class="i-lucide-plus text-4xl text-accent"></span>
          <span class="text-sm text-text-2">New Document</span>
        </div>
      {/if}

      <!-- Documents -->
      {#each docs as doc (doc.name)}
        {@const linkKeySuffix = doc.linkKey ? `?k=${doc.linkKey}` : ''}
        <DocCard
          href="#/{npub}/{doc.name}{linkKeySuffix}"
          displayName={doc.displayName}
          ownerPubkey={pubkeyHex}
          ownerNpub={npub}
          treeName={doc.name}
          visibility={doc.visibility}
          rootHashHex={doc.hashHex}
        />
      {/each}

      <!-- Empty state -->
      {#if docs.length === 0 && !isOwnProfile}
        <div class="col-span-full text-center text-text-3 py-8">
          No documents yet
        </div>
      {/if}
    </div>
  </div>
</div>
