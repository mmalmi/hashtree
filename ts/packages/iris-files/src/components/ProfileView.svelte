<script lang="ts">
  /**
   * ProfileView - displays user profile
   * Port of React ProfileView component
   */
  import { nip19 } from 'nostr-tools';
  import { nostrStore } from '../nostr';
  import { createProfileStore } from '../stores/profile';
  import { createFollowsStore, followPubkey, unfollowPubkey } from '../stores/follows';
  import ShareButton from './ShareButton.svelte';
  import { Avatar, Name, Badge, FollowedBy } from './User';
  import CopyText from './CopyText.svelte';
  import ProxyImg from './ProxyImg.svelte';
  import { getFollowsMe, getFollowers, fetchUserFollows, fetchUserFollowers, socialGraphStore } from '../utils/socialGraph';

  interface Props {
    npub?: string;
  }

  let { npub: npubProp }: Props = $props();

  let myPubkey = $derived($nostrStore.pubkey);
  let isLoggedIn = $derived($nostrStore.isLoggedIn);

  // Use provided npub or derive from logged-in user
  let npub = $derived(npubProp || (myPubkey ? nip19.npubEncode(myPubkey) : ''));

  // Decode npub to hex pubkey
  let pubkeyHex = $derived.by(() => {
    if (!npub) return '';
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

  // Social graph reactive state (subscribe to version changes)
  let graphVersion = $derived($socialGraphStore.version);

  // Follow state - recompute when graph changes
  let isFollowing = $derived.by(() => {
    graphVersion; // Subscribe to changes
    return myFollows.includes(pubkeyHex);
  });

  let followsMe = $derived.by(() => {
    graphVersion; // Subscribe to changes
    return getFollowsMe(pubkeyHex);
  });

  let knownFollowers = $derived.by(() => {
    graphVersion; // Subscribe to changes
    return getFollowers(pubkeyHex);
  });

  // Fetch social graph data when viewing a profile
  $effect(() => {
    if (pubkeyHex) {
      fetchUserFollows(pubkeyHex);
      fetchUserFollowers(pubkeyHex);
    }
  });

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
        <ShareButton url={window.location.href} />
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
        class="text-text-3 hover:text-text-1 no-underline"
      >
        <span class="font-bold text-text-1">{profileFollows.length}</span> Following
      </a>
      <a
        href="#/{npub}/followers"
        class="text-text-3 hover:text-text-1 no-underline"
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
</div>
