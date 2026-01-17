<script lang="ts">
  /**
   * DocsRouter - Router for the docs app
   * Routes:
   * - / : Home (recent docs, followed users' docs)
   * - /settings : Settings page
   * - /users : User list (switch user)
   * - /:npub/edit : Edit profile page
   * - /:npub/profile : Profile page (alias)
   * - /:npub/follows : Following list
   * - /:npub/followers : Followers list
   * - /:npub/:treeName/... : Document view
   * - /:npub : Profile with docs
   */
  import { matchRoute } from '../../lib/router.svelte';
  import DocsHome from './DocsHome.svelte';
  import DocsProfileView from './DocsProfileView.svelte';
  import DocView from './DocView.svelte';
  import SettingsLayout from '../settings/SettingsLayout.svelte';
  import EditProfilePage from '../EditProfilePage.svelte';
  import UsersPage from '../UsersPage.svelte';
  import FollowsPage from '../FollowsPage.svelte';
  import FollowersPage from '../FollowersPage.svelte';

  const routePatterns = [
    { pattern: '/', component: DocsHome },
    { pattern: '/settings', component: SettingsLayout },
    { pattern: '/settings/:tab', component: SettingsLayout },
    { pattern: '/users', component: UsersPage },
    { pattern: '/:npub/edit', component: EditProfilePage },
    { pattern: '/:npub/profile', component: DocsProfileView },
    { pattern: '/:npub/follows', component: FollowsPage },
    { pattern: '/:npub/followers', component: FollowersPage },
    { pattern: '/:npub/:treeName/*', component: DocView },
    { pattern: '/:npub/:treeName', component: DocView },
    { pattern: '/:npub', component: DocsProfileView }, // Profile with docs
  ];

  interface Props {
    currentPath: string;
  }

  let { currentPath }: Props = $props();

  // Match route
  let matchedRoute = $derived.by(() => {
    for (const route of routePatterns) {
      const match = matchRoute(route.pattern, currentPath);
      if (match.matched) {
        return { component: route.component, params: match.params };
      }
    }
    return { component: DocsHome, params: {} };
  });
</script>

<svelte:component this={matchedRoute.component} {...matchedRoute.params} />
