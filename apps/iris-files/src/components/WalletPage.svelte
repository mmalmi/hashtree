<script lang="ts">
  /**
   * WalletPage - Cashu wallet interface
   * Shows balance, receive/send, and mint management
   */
  import { walletStore, addMint, createReceiveQuote, receiveToken, DEFAULT_MINT } from '../stores/wallet';
  import { getBtcUsdRate, satsToUsd } from '../utils/btcRate';
  import { nostrStore } from '../nostr';
  import { getSecretKey } from '../nostr/auth';
  import QRCode from 'qrcode';
  import CopyText from './CopyText.svelte';

  type Tab = 'balance' | 'receive' | 'send' | 'mints';

  let isLoggedIn = $derived($nostrStore.isLoggedIn);
  let hasSecretKey = $derived(getSecretKey() !== null);

  let activeTab = $state<Tab>('balance');
  let btcRate = $state<number | null>(null);

  // Receive state
  let receiveAmount = $state('');
  let receiveInvoice = $state<string | null>(null);
  let receiveQrUrl = $state<string | null>(null);
  let receiveMint = $state(DEFAULT_MINT);
  let receiveLoading = $state(false);
  let receiveError = $state<string | null>(null);

  // Send state (receive ecash)
  let ecashToken = $state('');
  let sendLoading = $state(false);
  let sendError = $state<string | null>(null);
  let sendSuccess = $state(false);

  // Mint state
  let newMintUrl = $state('');
  let mintLoading = $state(false);
  let mintError = $state<string | null>(null);

  // Fetch BTC rate on mount
  $effect(() => {
    getBtcUsdRate().then(rate => { btcRate = rate; }).catch(console.error);
  });

  // Format USD amount
  function formatUsd(sats: number): string {
    if (!btcRate || sats === 0) return '';
    const usd = satsToUsd(sats, btcRate);
    return `$${usd.toFixed(2)}`;
  }

  // Generate receive invoice
  async function generateReceiveInvoice() {
    const amount = parseInt(receiveAmount);
    if (!amount || amount <= 0) {
      receiveError = 'Enter a valid amount';
      return;
    }

    receiveLoading = true;
    receiveError = null;

    try {
      const { invoice } = await createReceiveQuote(amount, receiveMint);
      receiveInvoice = invoice;
      receiveQrUrl = await QRCode.toDataURL(`lightning:${invoice.toUpperCase()}`, {
        width: 280,
        margin: 2,
        color: { dark: '#000000', light: '#ffffff' },
      });
    } catch (e) {
      receiveError = e instanceof Error ? e.message : 'Failed to create invoice';
    } finally {
      receiveLoading = false;
    }
  }

  // Claim ecash token
  async function claimToken() {
    if (!ecashToken.trim()) {
      sendError = 'Paste an ecash token';
      return;
    }

    sendLoading = true;
    sendError = null;
    sendSuccess = false;

    try {
      await receiveToken(ecashToken.trim());
      sendSuccess = true;
      ecashToken = '';
    } catch (e) {
      sendError = e instanceof Error ? e.message : 'Failed to receive token';
    } finally {
      sendLoading = false;
    }
  }

  // Add new mint
  async function handleAddMint() {
    if (!newMintUrl.trim()) {
      mintError = 'Enter a mint URL';
      return;
    }

    mintLoading = true;
    mintError = null;

    try {
      await addMint(newMintUrl.trim());
      newMintUrl = '';
    } catch (e) {
      mintError = e instanceof Error ? e.message : 'Failed to add mint';
    } finally {
      mintLoading = false;
    }
  }

  // Reset receive form
  function resetReceive() {
    receiveInvoice = null;
    receiveQrUrl = null;
    receiveAmount = '';
    receiveError = null;
  }
</script>

<div class="flex-1 flex flex-col min-h-0 bg-surface-0">
  <div class="max-w-2xl w-full mx-auto flex flex-col min-h-0 flex-1">
    <!-- Header -->
    <div class="border-b border-surface-3 p-4">
      <h1 class="text-xl font-semibold text-text-1 flex items-center gap-2">
        <span class="i-lucide-wallet text-accent"></span>
        Wallet
      </h1>
    </div>

    <!-- Tabs -->
    <div class="flex border-b border-surface-3">
      <button
        onclick={() => activeTab = 'balance'}
        class="px-4 py-3 text-sm font-medium transition-colors"
        class:text-accent={activeTab === 'balance'}
        class:border-b-2={activeTab === 'balance'}
        class:border-accent={activeTab === 'balance'}
        class:text-text-2={activeTab !== 'balance'}
      >
        Balance
      </button>
      <button
        onclick={() => activeTab = 'receive'}
        class="px-4 py-3 text-sm font-medium transition-colors"
        class:text-accent={activeTab === 'receive'}
        class:border-b-2={activeTab === 'receive'}
        class:border-accent={activeTab === 'receive'}
        class:text-text-2={activeTab !== 'receive'}
      >
        Receive
      </button>
      <button
        onclick={() => activeTab = 'send'}
        class="px-4 py-3 text-sm font-medium transition-colors"
        class:text-accent={activeTab === 'send'}
        class:border-b-2={activeTab === 'send'}
        class:border-accent={activeTab === 'send'}
        class:text-text-2={activeTab !== 'send'}
      >
        Redeem
      </button>
      <button
        onclick={() => activeTab = 'mints'}
        class="px-4 py-3 text-sm font-medium transition-colors"
        class:text-accent={activeTab === 'mints'}
        class:border-b-2={activeTab === 'mints'}
        class:border-accent={activeTab === 'mints'}
        class:text-text-2={activeTab !== 'mints'}
      >
        Mints
      </button>
    </div>

    <!-- Content -->
    <div class="flex-1 overflow-auto p-4">
      {#if !isLoggedIn}
        <div class="text-center py-12 text-text-3">
          <span class="i-lucide-wallet text-4xl mb-4 block opacity-50"></span>
          <p class="text-lg mb-2">Log in to use your wallet</p>
          <p class="text-sm">Your Cashu wallet is linked to your Nostr identity.</p>
        </div>
      {:else if !hasSecretKey}
        <div class="text-center py-12 text-text-3">
          <span class="i-lucide-key text-4xl mb-4 block opacity-50"></span>
          <p class="text-lg mb-2">Wallet requires nsec login</p>
          <p class="text-sm">Extension logins don't have access to your private key.<br/>Log in with your nsec to use the wallet.</p>
        </div>
      {:else if !$walletStore.initialized}
        <div class="text-center py-12 text-text-3">
          {#if $walletStore.error}
            <span class="i-lucide-alert-circle text-4xl mb-4 block text-red-400"></span>
            <p class="text-lg mb-2 text-red-400">Wallet unavailable</p>
            <p class="text-sm max-w-sm mx-auto">Cashu wallet integration is still in development. Check back soon!</p>
          {:else if $walletStore.loading}
            <span class="i-lucide-loader-2 animate-spin text-2xl mb-2 block"></span>
            <p>Initializing wallet...</p>
          {:else}
            <span class="i-lucide-wallet text-4xl mb-4 block opacity-50"></span>
            <p class="text-lg mb-2">Wallet not initialized</p>
            <p class="text-sm">Log in with nsec to use the wallet.</p>
          {/if}
        </div>
      {:else if activeTab === 'balance'}
      <!-- Balance Tab -->
      <div class="max-w-md mx-auto">
        <div class="bg-surface-1 rounded-xl p-6 text-center mb-6">
          <p class="text-text-3 text-sm mb-1">Total Balance</p>
          <p class="text-4xl font-bold text-text-1 mb-1">
            {$walletStore.balance.toLocaleString()} <span class="text-lg text-text-3">sats</span>
          </p>
          {#if btcRate && $walletStore.balance > 0}
            <p class="text-text-2">{formatUsd($walletStore.balance)}</p>
          {/if}
        </div>

        {#if $walletStore.mints.length > 0}
          <div class="space-y-2">
            <p class="text-sm text-text-3 font-medium">Balance by Mint</p>
            {#each $walletStore.mints as mint (mint.url)}
              <div class="bg-surface-1 rounded-lg p-3 flex justify-between items-center">
                <span class="text-sm text-text-2 truncate flex-1 mr-2">{mint.url.replace('https://', '')}</span>
                <span class="text-text-1 font-medium">{mint.balance.toLocaleString()}</span>
              </div>
            {/each}
          </div>
        {:else}
          <p class="text-center text-text-3 text-sm">
            No mints added yet. Add a mint to receive sats.
          </p>
        {/if}

        {#if $walletStore.error}
          <div class="mt-4 bg-red-500/20 text-red-400 rounded-lg p-3 text-sm">
            {$walletStore.error}
          </div>
        {/if}
      </div>

    {:else if activeTab === 'receive'}
      <!-- Receive Tab -->
      <div class="max-w-md mx-auto">
        {#if receiveInvoice && receiveQrUrl}
          <!-- Show invoice QR -->
          <div class="text-center">
            <div class="bg-white rounded-lg p-2 mb-4 inline-block">
              <img src={receiveQrUrl} alt="Lightning Invoice" class="w-70 h-70" />
            </div>
            <p class="text-sm text-text-2 mb-3">
              Scan with a lightning wallet or copy the invoice
            </p>
            <div class="bg-surface-1 p-3 rounded-lg mb-4">
              <CopyText text={receiveInvoice} truncate={40} class="text-sm" />
            </div>
            <button onclick={resetReceive} class="btn-ghost">
              ← Create new invoice
            </button>
          </div>
        {:else}
          <!-- Amount input -->
          <div class="mb-4">
            <label for="receive-amount" class="block text-sm text-text-2 mb-2">Amount (sats)</label>
            <input
              id="receive-amount"
              type="number"
              bind:value={receiveAmount}
              placeholder="Enter amount..."
              class="w-full bg-surface-1 border border-surface-3 rounded-lg p-3 text-text-1 focus:border-accent focus:outline-none"
            />
          </div>

          <div class="mb-4">
            <label for="receive-mint" class="block text-sm text-text-2 mb-2">Mint</label>
            <select
              id="receive-mint"
              bind:value={receiveMint}
              class="w-full bg-surface-1 border border-surface-3 rounded-lg p-3 text-text-1 focus:border-accent focus:outline-none"
            >
              <option value={DEFAULT_MINT}>{DEFAULT_MINT.replace('https://', '')}</option>
              {#each $walletStore.mints.filter(m => m.url !== DEFAULT_MINT) as mint (mint.url)}
                <option value={mint.url}>{mint.url.replace('https://', '')}</option>
              {/each}
            </select>
          </div>

          {#if receiveError}
            <div class="mb-4 bg-red-500/20 text-red-400 rounded-lg p-3 text-sm">
              {receiveError}
            </div>
          {/if}

          <button
            onclick={generateReceiveInvoice}
            disabled={receiveLoading}
            class="btn-primary w-full py-3"
          >
            {#if receiveLoading}
              <span class="i-lucide-loader-2 animate-spin"></span>
              Generating...
            {:else}
              Generate Invoice
            {/if}
          </button>
        {/if}
      </div>

    {:else if activeTab === 'send'}
      <!-- Send/Redeem Tab -->
      <div class="max-w-md mx-auto">
        <p class="text-sm text-text-2 mb-4">
          Paste an ecash token to redeem it to your wallet.
        </p>

        <div class="mb-4">
          <label for="send-ecash-token" class="block text-sm text-text-2 mb-2">Ecash Token</label>
          <textarea
            id="send-ecash-token"
            bind:value={ecashToken}
            placeholder="cashuA..."
            rows="4"
            class="w-full bg-surface-1 border border-surface-3 rounded-lg p-3 text-text-1 focus:border-accent focus:outline-none resize-none font-mono text-sm"
          ></textarea>
        </div>

        {#if sendError}
          <div class="mb-4 bg-red-500/20 text-red-400 rounded-lg p-3 text-sm">
            {sendError}
          </div>
        {/if}

        {#if sendSuccess}
          <div class="mb-4 bg-green-500/20 text-green-400 rounded-lg p-3 text-sm">
            Token redeemed successfully!
          </div>
        {/if}

        <button
          onclick={claimToken}
          disabled={sendLoading || !ecashToken.trim()}
          class="btn-primary w-full py-3"
        >
          {#if sendLoading}
            <span class="i-lucide-loader-2 animate-spin"></span>
            Redeeming...
          {:else}
            Redeem Token
          {/if}
        </button>
      </div>

    {:else if activeTab === 'mints'}
      <!-- Mints Tab -->
      <div class="max-w-md mx-auto">
        <p class="text-sm text-text-2 mb-4">
          Manage your Cashu mints. Mints hold your ecash tokens.
        </p>

        <!-- Current mints -->
        {#if $walletStore.mints.length > 0}
          <div class="space-y-2 mb-6">
            {#each $walletStore.mints as mint (mint.url)}
              <div class="bg-surface-1 rounded-lg p-3">
                <div class="flex justify-between items-center">
                  <span class="text-sm text-text-1 truncate flex-1 mr-2">
                    {mint.url.replace('https://', '')}
                  </span>
                  <span class="text-text-2 text-sm">{mint.balance.toLocaleString()} sats</span>
                </div>
              </div>
            {/each}
          </div>
        {/if}

        <!-- Add new mint -->
        <div class="border-t border-surface-3 pt-4">
          <label for="add-mint-url" class="block text-sm text-text-2 mb-2">Add Mint</label>
          <div class="flex gap-2">
            <input
              id="add-mint-url"
              type="url"
              bind:value={newMintUrl}
              placeholder="https://mint.example.com"
              class="flex-1 bg-surface-1 border border-surface-3 rounded-lg p-3 text-text-1 focus:border-accent focus:outline-none"
            />
            <button
              onclick={handleAddMint}
              disabled={mintLoading || !newMintUrl.trim()}
              class="btn-primary px-4"
            >
              {#if mintLoading}
                <span class="i-lucide-loader-2 animate-spin"></span>
              {:else}
                Add
              {/if}
            </button>
          </div>

          {#if mintError}
            <div class="mt-2 text-red-400 text-sm">
              {mintError}
            </div>
          {/if}
        </div>
      </div>
    {/if}
    </div>
  </div>
</div>
