<script lang="ts">
  /**
   * EditProfilePage - edit user profile
   * Port of React EditProfilePage
   */
  import { nostrStore, ndk } from '../nostr';
  import { createProfileStore, invalidateProfile as invalidateProfileFn } from '../stores/profile';
  import { nip19 } from 'nostr-tools';
  import { NDKEvent } from 'ndk';
  import { BackButton } from './ui';

  interface Props {
    npub?: string;
  }

  let { npub }: Props = $props();

  let myPubkey = $derived($nostrStore.pubkey);
  let isLoggedIn = $derived($nostrStore.isLoggedIn);

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
  let profile = $state<{ name?: string; display_name?: string; about?: string; banner?: string; picture?: string; nip05?: string; website?: string; lud16?: string } | null>(null);

  $effect(() => {
    if (!npub) return;
    const store = profileStore;
    const unsub = store.subscribe(value => {
      profile = value;
    });
    return unsub;
  });

  // Form state
  let name = $state('');
  let about = $state('');
  let picture = $state('');
  let banner = $state('');
  let website = $state('');
  let nip05Field = $state('');
  let lud16 = $state('');
  let saving = $state(false);
  let error = $state('');

  // Populate form when profile loads
  $effect(() => {
    if (profile) {
      name = profile.name || profile.display_name || '';
      about = profile.about || '';
      picture = profile.picture || '';
      banner = profile.banner || '';
      website = profile.website || '';
      nip05Field = profile.nip05 || '';
      lud16 = profile.lud16 || '';
    }
  });

  // Redirect if not own profile
  $effect(() => {
    if (pubkeyHex && myPubkey && !isOwnProfile) {
      window.location.hash = `/${npub}`;
    }
  });

  function navigate(path: string) {
    window.location.hash = path;
  }

  async function handleSave() {
    saving = true;
    error = '';

    try {
      const profileData: Record<string, string> = {
        name,
        display_name: name,
        about,
        picture,
        banner,
        website,
        nip05: nip05Field,
        lud16,
      };

      // Remove empty fields
      const cleanedProfile = Object.fromEntries(
        Object.entries(profileData).filter(([, v]) => v)
      );

      const event = new NDKEvent(ndk);
      event.kind = 0;
      event.content = JSON.stringify(cleanedProfile);

      await event.publish();

      // Invalidate cache and refetch - small delay to let relays propagate
      setTimeout(() => {
        invalidateProfileFn(pubkeyHex);
      }, 500);

      // Navigate back to profile
      navigate(`/${npub}/profile`);
    } catch (e) {
      console.error('Failed to save profile:', e);
      error = 'Failed to save profile';
      saving = false;
    }
  }

  function handleCancel() {
    navigate(`/${npub}/profile`);
  }
</script>

{#if !isLoggedIn || !isOwnProfile}
  <div class="flex-1 flex items-center justify-center bg-surface-0">
    <div class="text-text-2">Not authorized</div>
  </div>
{:else}
  <div class="flex-1 flex flex-col min-h-0 bg-surface-0 overflow-y-auto">
    <!-- Header -->
    <div class="sticky top-0 z-10 bg-surface-1 border-b border-surface-3 px-4 py-3 flex items-center gap-3">
      <BackButton onclick={handleCancel} />
      <h1 class="text-lg font-semibold flex-1">Edit Profile</h1>
      <button
        onclick={handleSave}
        class="btn-success"
        disabled={saving}
      >
        {saving ? 'Saving...' : 'Save'}
      </button>
    </div>

    <!-- Form -->
    <div class="p-4 flex flex-col gap-4 max-w-lg mx-auto w-full">
      {#if error}
        <div class="text-danger text-sm bg-danger/10 p-3 rounded">{error}</div>
      {/if}

      <div>
        <label for="profile-name" class="text-sm text-text-2 block mb-1">Name</label>
        <input
          id="profile-name"
          type="text"
          bind:value={name}
          placeholder="Your name"
          class="w-full p-3 rounded bg-surface-2 border border-surface-3 text-text-1"
        />
      </div>

      <div>
        <label for="profile-about" class="text-sm text-text-2 block mb-1">About</label>
        <textarea
          id="profile-about"
          bind:value={about}
          placeholder="Tell us about yourself"
          rows="4"
          class="w-full p-3 rounded bg-surface-2 border border-surface-3 text-text-1 resize-none"
        ></textarea>
      </div>

      <div>
        <label for="profile-picture" class="text-sm text-text-2 block mb-1">Profile Picture URL</label>
        <input
          id="profile-picture"
          type="url"
          bind:value={picture}
          placeholder="https://..."
          class="w-full p-3 rounded bg-surface-2 border border-surface-3 text-text-1"
        />
      </div>

      <div>
        <label for="profile-banner" class="text-sm text-text-2 block mb-1">Banner URL</label>
        <input
          id="profile-banner"
          type="url"
          bind:value={banner}
          placeholder="https://..."
          class="w-full p-3 rounded bg-surface-2 border border-surface-3 text-text-1"
        />
      </div>

      <div>
        <label for="profile-website" class="text-sm text-text-2 block mb-1">Website</label>
        <input
          id="profile-website"
          type="url"
          bind:value={website}
          placeholder="https://..."
          class="w-full p-3 rounded bg-surface-2 border border-surface-3 text-text-1"
        />
      </div>

      <div>
        <label for="profile-nip05" class="text-sm text-text-2 block mb-1">NIP-05</label>
        <input
          id="profile-nip05"
          type="text"
          bind:value={nip05Field}
          placeholder="you@example.com"
          class="w-full p-3 rounded bg-surface-2 border border-surface-3 text-text-1"
        />
      </div>

      <div>
        <label for="profile-lud16" class="text-sm text-text-2 block mb-1">Lightning Address</label>
        <input
          id="profile-lud16"
          type="text"
          bind:value={lud16}
          placeholder="you@getalby.com"
          class="w-full p-3 rounded bg-surface-2 border border-surface-3 text-text-1"
          data-testid="lud16-input"
        />
      </div>

      <!-- Bottom padding for mobile -->
      <div class="h-8"></div>
    </div>
  </div>
{/if}
