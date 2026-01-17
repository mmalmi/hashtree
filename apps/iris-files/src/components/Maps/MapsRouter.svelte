<script lang="ts">
  /**
   * MapsRouter - Router for the maps app
   */
  import { matchRoute } from '../../lib/router.svelte';
  import MapsHome from './MapsHome.svelte';
  import SettingsLayout from '../settings/SettingsLayout.svelte';
  import UsersPage from '../UsersPage.svelte';
  import EditProfilePage from '../EditProfilePage.svelte';
  import ProfileView from '../ProfileView.svelte';
  import FollowsPage from '../FollowsPage.svelte';
  import FollowersPage from '../FollowersPage.svelte';

  const routePatterns = [
    { pattern: '/', component: MapsHome },
    { pattern: '/settings', component: SettingsLayout },
    { pattern: '/settings/:tab', component: SettingsLayout },
    { pattern: '/users', component: UsersPage },
    { pattern: '/:npub/edit', component: EditProfilePage },
    { pattern: '/:npub/follows', component: FollowsPage },
    { pattern: '/:npub/followers', component: FollowersPage },
    { pattern: '/:npub/profile', component: ProfileView },
    { pattern: '/:npub', component: ProfileView },
  ];

  interface Props {
    currentPath: string;
  }

  let { currentPath }: Props = $props();

  let matchedRoute = $derived.by(() => {
    for (const route of routePatterns) {
      const match = matchRoute(route.pattern, currentPath);
      if (match.matched) {
        return { component: route.component, params: match.params };
      }
    }
    return { component: MapsHome, params: {} };
  });
</script>

<svelte:component this={matchedRoute.component} {...matchedRoute.params} />
