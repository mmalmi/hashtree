<script lang="ts">
  import { navigate } from '../utils/navigate';
  import { nostrStore, loginWithExtension, loginWithNsec, generateNewKey } from '../nostr';
  import { Avatar } from './User';

  let showNsec = $state(false);
  let nsecInput = $state('');
  let error = $state('');

  let isLoggedIn = $derived($nostrStore.isLoggedIn);
  let npub = $derived($nostrStore.npub);
  let pubkey = $derived($nostrStore.pubkey);

  function goToProfile() {
    if (!npub) return;
    navigate(`/${npub}/profile`);
  }

  async function handleExtensionLogin() {
    error = '';
    const success = await loginWithExtension();
    if (!success) {
      error = 'Extension login failed. Is a nostr extension installed?';
    }
  }

  async function handleNsecLogin() {
    error = '';
    if (!nsecInput.trim()) {
      error = 'Please enter an nsec';
      return;
    }
    const success = await loginWithNsec(nsecInput.trim());
    if (!success) {
      error = 'Invalid nsec';
    } else {
      nsecInput = '';
      showNsec = false;
    }
  }

  async function handleGenerate() {
    error = '';
    await generateNewKey();
  }
</script>

{#if isLoggedIn && pubkey}
  <!-- Logged in: just show avatar that links to profile (double-click for accounts) -->
  <button
    onclick={goToProfile}
    ondblclick={() => navigate('/users')}
    class="bg-transparent border-none cursor-pointer p-0"
    title="My Profile (double-click for users)"
  >
    <Avatar pubkey={pubkey} size={36} />
  </button>
{:else}
  <div class="flex flex-col gap-2">
    <div class="flex gap-1 md:gap-2 flex-wrap">
      <button onclick={handleExtensionLogin} class="btn-success text-xs md:text-sm">
        <span class="hidden md:inline">Login (Extension)</span>
        <span class="md:hidden">Login</span>
      </button>

      <button
        onclick={() => (showNsec = !showNsec)}
        class="btn-ghost text-xs md:text-sm hidden md:block"
      >
        {showNsec ? 'Cancel' : 'nsec'}
      </button>

      <button onclick={handleGenerate} class="btn-ghost text-xs md:text-sm hidden md:block">
        New
      </button>
    </div>

    {#if showNsec}
      <div class="flex gap-2">
        <input
          type="password"
          bind:value={nsecInput}
          placeholder="nsec1..."
          class="flex-1 input text-sm"
        />
        <button onclick={handleNsecLogin} class="btn-success text-sm">
          Login
        </button>
      </div>
    {/if}

    {#if error}
      <p class="text-danger text-sm m-0">{error}</p>
    {/if}
  </div>
{/if}
