<script lang="ts">
  /**
   * UsersPage - manage saved accounts
   * Shows list of accounts, allows adding nsec accounts, switching between accounts, and removing accounts
   */
  import { navigate } from '../utils/navigate';
  import { accountsStore, createAccountFromNsec, saveActiveAccountToStorage, hasNostrExtension, type Account } from '../accounts';
  import { loginWithNsec, loginWithExtension, generateNewKey, restoreSession } from '../nostr';
  import { Avatar, Name } from './User';
  import { BackButton } from './ui';

  // State
  let showAddNsec = $state(false);
  let nsecInput = $state('');
  let nsecError = $state<string | null>(null);
  let confirmingRemove = $state<string | null>(null); // pubkey of account being removed
  let extensionError = $state<string | null>(null);

  // Store values
  let accountsState = $derived($accountsStore);
  let accounts = $derived(accountsState.accounts);
  let activeAccountPubkey = $derived(accountsState.activeAccountPubkey);
  let hasExtension = $derived(hasNostrExtension());

  // Sort accounts by creation time (oldest first)
  let sortedAccounts = $derived(
    [...accounts].sort((a, b) => a.addedAt - b.addedAt)
  );

  function handleAddNsec() {
    nsecError = null;
    const nsec = nsecInput.trim();

    if (!nsec || !nsec.startsWith('nsec1')) {
      nsecError = 'Invalid nsec';
      return;
    }

    // Check if already exists
    const account = createAccountFromNsec(nsec);
    if (!account) {
      nsecError = 'Invalid nsec';
      return;
    }

    if (accounts.some(a => a.pubkey === account.pubkey)) {
      nsecError = 'Account already added';
      return;
    }

    // Add the account
    accountsStore.addAccount(account);
    nsecInput = '';
    showAddNsec = false;
  }

  function cancelAddNsec() {
    showAddNsec = false;
    nsecInput = '';
    nsecError = null;
  }

  async function switchToAccount(account: Account) {
    if (account.pubkey === activeAccountPubkey) return;

    // Save as active
    saveActiveAccountToStorage(account.pubkey);
    accountsStore.setActiveAccount(account.pubkey);

    // Log in with this account
    if (account.type === 'nsec' && account.nsec) {
      await loginWithNsec(account.nsec);
    } else {
      // For extension accounts, re-initialize from storage
      await restoreSession();
    }
  }

  function startRemoveAccount(pubkey: string) {
    confirmingRemove = pubkey;
  }

  function cancelRemoveAccount() {
    confirmingRemove = null;
  }

  function confirmRemoveAccount(pubkey: string) {
    accountsStore.removeAccount(pubkey);
    confirmingRemove = null;
  }

  async function handleGenerateNew() {
    await generateNewKey();
  }

  async function handleExtensionLogin() {
    extensionError = null;
    const success = await loginWithExtension();
    if (success) {
      navigate('/');
    } else {
      extensionError = 'Extension login failed. Is a Nostr extension installed?';
    }
  }

</script>

<div class="flex-1 flex flex-col min-h-0 bg-surface-0 p-6 max-w-2xl mx-auto w-full">
  <!-- Header -->
  <div class="flex items-center gap-4 mb-6">
    <BackButton href="/" />
    <h1 class="text-xl font-semibold">Users</h1>
  </div>

  <!-- Account list -->
  <div class="space-y-3 mb-6">
    {#each sortedAccounts as account (account.pubkey)}
      {@const isActive = account.pubkey === activeAccountPubkey}
      {@const isConfirming = confirmingRemove === account.pubkey}
      <div
        class="rounded-lg p-4 flex items-center gap-3 cursor-pointer transition-colors {isActive ? 'bg-surface-2' : 'bg-surface-1 hover:bg-surface-2'}"
        onclick={() => !isConfirming && switchToAccount(account)}
        role="button"
        tabindex="0"
        onkeypress={(e) => e.key === 'Enter' && !isConfirming && switchToAccount(account)}
        data-testid="account-item"
      >
        <!-- Avatar -->
        <a
          href={`#/${account.npub}/profile`}
          class="shrink-0"
          onclick={(e) => e.stopPropagation()}
        >
          <Avatar pubkey={account.pubkey} size={40} />
        </a>

        <!-- Info -->
        <div class="flex-1 min-w-0">
          <div class="font-medium truncate">
            <Name pubkey={account.pubkey} />
          </div>
        </div>

        <!-- Active indicator or actions -->
        <div class="shrink-0 flex items-center gap-2">
          {#if isActive}
            <span class="i-lucide-check-circle text-success text-lg"></span>
          {/if}

          {#if accounts.length > 1}
            {#if isConfirming}
              <button
                onclick={(e) => { e.stopPropagation(); confirmRemoveAccount(account.pubkey); }}
                class="btn-danger text-sm"
              >
                Remove
              </button>
              <button
                onclick={(e) => { e.stopPropagation(); cancelRemoveAccount(); }}
                class="btn-ghost text-sm"
              >
                Cancel
              </button>
            {:else}
              <button
                onclick={(e) => { e.stopPropagation(); startRemoveAccount(account.pubkey); }}
                class="btn-ghost text-danger"
                title="Remove account"
              >
                <span class="i-lucide-trash-2"></span>
              </button>
            {/if}
          {/if}
        </div>
      </div>
    {/each}
  </div>

  <!-- Add account section -->
  <div class="space-y-2">
    <button onclick={handleGenerateNew} class="btn-success w-full justify-center" data-testid="generate-new-account">
      <span class="i-lucide-plus"></span>
      Generate New Account
    </button>

    {#if hasExtension}
      <button onclick={handleExtensionLogin} class="btn-ghost w-full justify-center border border-surface-3" data-testid="extension-login">
        <span class="i-lucide-puzzle"></span>
        Login with Extension
      </button>
      {#if extensionError}
        <p class="text-danger text-sm text-center">{extensionError}</p>
      {/if}
    {/if}

    {#if showAddNsec}
      <div class="bg-surface-1 rounded-lg p-4 space-y-3">
        <h3 class="font-medium">Add account with secret key</h3>
        <input
          type="password"
          bind:value={nsecInput}
          placeholder="nsec1..."
          class="input w-full"
          onkeypress={(e) => e.key === 'Enter' && handleAddNsec()}
        />
        {#if nsecError}
          <p class="text-danger text-sm">{nsecError}</p>
        {/if}
        <div class="flex gap-2">
          <button onclick={cancelAddNsec} class="btn-ghost">Cancel</button>
          <button onclick={handleAddNsec} class="btn-success">Add</button>
        </div>
      </div>
    {:else}
      <button onclick={() => showAddNsec = true} class="btn-ghost w-full justify-center text-text-3" data-testid="add-with-nsec">
        <span class="i-lucide-key"></span>
        Add with Secret Key
      </button>
    {/if}
  </div>
</div>
