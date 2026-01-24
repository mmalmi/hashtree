<script lang="ts">
  import { currentPath, refreshKey } from '../lib/router.svelte';
  import AppLauncher from './AppLauncher.svelte';
  import AppFrame from './AppFrame.svelte';
  import NHashViewer from './NHashViewer.svelte';
  import SettingsLayout from './settings/SettingsLayout.svelte';
  import WalletPage from './WalletPage.svelte';
  import ProfileView from './ProfileView.svelte';
  import EditProfilePage from './EditProfilePage.svelte';
  import UsersPage from './UsersPage.svelte';
  import TreeRoute from '../routes/TreeRoute.svelte';
  import { isNHash } from '@hashtree/core';

  // Parse the current path to determine what to render
  function parseRoute(path: string) {
    // Root: show app launcher
    if (path === '/' || path === '') {
      return { type: 'launcher' as const };
    }

    // /settings/*
    if (path === '/settings' || path.startsWith('/settings/')) {
      return { type: 'settings' as const };
    }

    // /wallet
    if (path === '/wallet') {
      return { type: 'wallet' as const };
    }

    // /users - switch user
    if (path === '/users') {
      return { type: 'users' as const };
    }

    // /profile or /npub...
    if (path === '/profile') {
      return { type: 'profile' as const, npub: undefined };
    }
    if (path.startsWith('/npub1')) {
      // Extract just the npub (63 chars: npub1 + 58 bech32 chars)
      const match = path.match(/^\/npub1[a-z0-9]{58}/);
      if (match) {
        const npub = match[0].slice(1); // Remove leading /
        const remainder = path.slice(match[0].length);
        // Check for /edit suffix
        if (remainder === '/edit') {
          return { type: 'editProfile' as const, npub };
        }
        // Check for tree path: /npub.../treeName/... (has content after npub)
        if (remainder.startsWith('/') && remainder.length > 1) {
          const pathParts = remainder.slice(1).split('/');
          const treeName = pathParts[0];
          const wild = pathParts.slice(1).join('/') || undefined;
          return { type: 'tree' as const, npub, treeName, wild };
        }
        return { type: 'profile' as const, npub };
      }
    }

    // /app/{encodedUrl} - load external app in iframe
    if (path.startsWith('/app/')) {
      const encodedUrl = path.slice(5); // Remove '/app/'
      try {
        const appUrl = decodeURIComponent(encodedUrl);
        return { type: 'app' as const, url: appUrl };
      } catch {
        return { type: 'launcher' as const };
      }
    }

    // /nhash... - view saved PWA from hashtree
    if (path.startsWith('/nhash')) {
      const nhash = path.slice(1); // Remove leading /
      // Extract just the nhash part (before any subpath)
      const parts = nhash.split('/');
      const nhashPart = parts[0];
      const subpath = parts.slice(1).join('/');

      if (isNHash(nhashPart)) {
        return { type: 'nhash' as const, nhash: nhashPart, subpath };
      }
      return { type: 'launcher' as const };
    }

    // /files - redirect to files app (could be handled differently)
    if (path === '/files' || path.startsWith('/files/')) {
      // For now, just show launcher with a note
      return { type: 'launcher' as const };
    }

    // Default: show launcher
    return { type: 'launcher' as const };
  }

  let route = $derived(parseRoute($currentPath));
</script>

{#if route.type === 'launcher'}
  <AppLauncher />
{:else if route.type === 'app'}
  {#key $refreshKey}
    <AppFrame appUrl={route.url} />
  {/key}
{:else if route.type === 'nhash'}
  <NHashViewer nhash={route.nhash} subpath={route.subpath} />
{:else if route.type === 'settings'}
  <SettingsLayout />
{:else if route.type === 'wallet'}
  <WalletPage />
{:else if route.type === 'profile'}
  <ProfileView npub={route.npub} />
{:else if route.type === 'editProfile'}
  <EditProfilePage npub={route.npub} />
{:else if route.type === 'users'}
  <UsersPage />
{:else if route.type === 'tree'}
  <div class="flex-1 flex flex-col lg:flex-row min-h-0">
    <TreeRoute npub={route.npub} treeName={route.treeName} wild={route.wild} />
  </div>
{/if}
