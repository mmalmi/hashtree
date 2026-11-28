import type { AppError, ReportAppError } from './appError';
import { isProfileContentSection } from './profileContent';
import { getRootNhash, type FeedSnapshot, type ProfileSocialSnapshot } from './nostrIndex';
import type { ProfileSnapshot } from './profileNotes';
import { routeKey, threadRouteDiscoveryRef, type Route } from './appRoute';
import { waitForNextBrowserPaint } from './asyncTiming';

type RouteLoaderDeps = {
  getRoute: () => Route;
  getRoot: () => any | null;
  getLoadToken: () => number;
  getActivePubkey: () => string | null;
  hasVisibleContent: (route: Route) => boolean;
  clearRootBoundState: () => void;
  clearProfileViewState: () => void;
  clearThreadViewState: () => void;
  setLoading: (loading: boolean) => void;
  setPageError: (message: AppError) => void;
  reportError: ReportAppError;
  setFeed: (feed: FeedSnapshot | null) => void;
  getLoadedProfilePubkey: () => string | null;
  setLoadedProfilePubkey: (pubkey: string | null) => void;
  getAccountProfile: (pubkey: string) => any | null;
  getProfilePage: () => ProfileSnapshot | null;
  setProfilePage: (page: ProfileSnapshot | null) => void;
  setProfileSocial: (social: ProfileSocialSnapshot | null) => void;
  setProfileSocialLoading: (loading: boolean) => void;
  setProfilePersonalRoot: (root: any | null) => void;
  setProfileNotesLoading: (loading: boolean) => void;
  getProfileNoteFetchLimit: () => number;
  setNoteEngagementsEmpty: () => void;
  hydrateProfileRecordProgressively: (currentRoot: any, pubkey: string, token: number) => Promise<void>;
  hydrateProfileNotesPage: (
    currentRoot: any,
    pubkey: string,
    token: number,
    options?: {
      visibleThreshold?: number;
      onVisibleReady?: () => void;
    },
  ) => Promise<void>;
  hydrateProfileNotesProgressively: (
    currentRoot: any,
    pubkey: string,
    token: number,
    options?: {
      emitEvery?: number;
      visibleThreshold?: number;
      onVisibleReady?: () => void;
    },
  ) => Promise<void>;
  hydrateProfileLikes: (
    currentRoot: any,
    pubkey: string,
    token: number,
    options?: { updateLoading?: boolean },
  ) => Promise<void>;
  hydrateProfileFromPersonalIndex: (currentRoot: any | null, pubkey: string, token: number) => Promise<void>;
  scheduleProfileSocialHydration: (
    currentRoot: any,
    pubkey: string,
    token: number,
    options: {
      includeProfiles?: boolean;
      includeFollows?: boolean;
      includeFollowers?: boolean;
      progressSection?: 'follows' | 'followers';
      defer?: boolean;
    },
  ) => void;
  scheduleEmptyProfileRetry: (currentRoot: any, pubkey: string, token: number) => void;
  hydrateThread: (currentRoot: any, ref: string, token: number) => Promise<void>;
  hydrateNotifications: (currentRoot: any, activePubkey: string, token: number) => Promise<void>;
  hydrateHomeFeed: (
    currentRoot: any,
    feedOwnerPubkey: string,
    activePubkey: string,
    token: number,
  ) => Promise<void>;
  hydratePopularFeed: (root: any, token: number) => Promise<void>;
  hydrateForYouFeed: (
    currentRoot: any,
    activePubkey: string,
    token: number,
  ) => Promise<void>;
  hydrateViewerPersonalRoot: (currentRoot: any, activePubkey: string, token: number) => Promise<any>;
  hydrateViewerGraphFollows: (currentRoot: any, activePubkey: string, token: number) => Promise<void>;
  hydrateViewerFollows: (currentRoot: any, activePubkey: string, token: number) => Promise<void>;
  hydrateViewerMuteList: (currentRoot: any, activePubkey: string, token: number) => Promise<void>;
  hydrateViewerRelayMuteList: (activePubkey: string, token: number) => Promise<void>;
  hydrateViewerProfile: (currentRoot: any, activePubkey: string, token: number) => Promise<void>;
  setViewerProfileLoading: (loading: boolean) => void;
  resetViewerRouteState: () => void;
  getActiveFeedSourcePubkey: () => string | null;
  waitForRouteViewPaint?: () => Promise<void>;
  scheduleSecondaryWork?: (work: () => void) => void;
};

type PreparedProfileLoad = {
  changedProfile: boolean;
  changedRoot: boolean;
  isContentSection: boolean;
  pubkey: string;
};

export function shouldReloadProfileNotes(options: {
  changedProfile: boolean;
  changedRoot: boolean;
  page: ProfileSnapshot | null;
  fetchLimit: number;
}): boolean {
  if (options.changedProfile || options.changedRoot) {
    return true;
  }
  if (options.page?.durableHasMore === false) {
    return false;
  }
  return (options.page?.notes.length ?? 0) === 0
    || (options.page?.notes.length ?? 0) < options.fetchLimit;
}

export function createRouteLoader(deps: RouteLoaderDeps) {
  function scheduleIdleWork(work: () => void): void {
    if (deps.scheduleSecondaryWork) {
      deps.scheduleSecondaryWork(work);
      return;
    }
    if (typeof window !== 'undefined' && typeof window.requestIdleCallback === 'function') {
      window.requestIdleCallback(work, { timeout: 1_000 });
      return;
    }
    setTimeout(work, 0);
  }

  function prepareProfileView(
    route: Extract<Route, { kind: 'profile' }>,
    rootNhash: string,
  ): PreparedProfileLoad | null {
    if (!route.pubkey) {
      return null;
    }

    const existingPage = deps.getProfilePage();
    const changedProfile = deps.getLoadedProfilePubkey() !== route.pubkey;
    const changedRoot = existingPage?.rootNhash !== rootNhash;
    const isContentSection = isProfileContentSection(route.section) || route.section === 'likes';

    if (changedProfile) {
      deps.setProfileSocial(null);
      deps.setProfileSocialLoading(!isContentSection);
      deps.setProfilePersonalRoot(null);
      deps.setProfilePage({
        rootNhash,
        profile: deps.getAccountProfile(route.pubkey),
        notes: [],
      });
      deps.setNoteEngagementsEmpty();
      deps.setLoadedProfilePubkey(route.pubkey);
    } else if (!existingPage) {
      deps.setProfilePage({
        rootNhash,
        profile: deps.getAccountProfile(route.pubkey),
        notes: [],
      });
    }

    deps.setFeed(null);
    deps.setLoading(false);
    return {
      changedProfile,
      changedRoot,
      isContentSection,
      pubkey: route.pubkey,
    };
  }

  function startViewerHydration(
    root: any | null,
    activePubkey: string | null,
    route: Route,
    token: number,
  ): void {
    if (!activePubkey) {
      deps.resetViewerRouteState();
      return;
    }

    // The worker graph is available even without a content root. Direct
    // profiles need it for mutual followers before personal-root discovery.
    if (route.kind === 'profile') {
      void deps.hydrateViewerGraphFollows(root, activePubkey, token);
    }
    deps.setViewerProfileLoading(true);
    // Relays only refresh a cached list; nothing waits on this read.
    void deps.hydrateViewerRelayMuteList(activePubkey, token);
    const hydrateViewerFromIndexes = () => {
      void deps.hydrateViewerFollows(root, activePubkey, token);
      void deps.hydrateViewerMuteList(root, activePubkey, token);
      void deps.hydrateViewerProfile(root, activePubkey, token);
    };
    if (root) {
      // Read the open archive immediately instead of blocking follows, mutes,
      // and profile settings behind optional personal-root discovery.
      hydrateViewerFromIndexes();
    }
    void deps.hydrateViewerPersonalRoot(root, activePubkey, token).then((personalRoot) => {
      if (!root || personalRoot !== null) {
        hydrateViewerFromIndexes();
      }
    });
  }

  return {
    async runCurrentViewLoad(token: number): Promise<void> {
      const route = deps.getRoute();
      const isStaticRoute = route.kind === 'settings'
        || route.kind === 'search'
        || route.kind === 'wallet'
        || route.kind === 'accounts'
        || route.kind === 'username';
      if (isStaticRoute) {
        deps.setPageError('');
      }

      const root = deps.getRoot();
      const activePubkey = deps.getActivePubkey();
      const hadVisibleContent = deps.hasVisibleContent(route);
      const preparedProfile = route.kind === 'profile'
        ? prepareProfileView(route, root ? getRootNhash(root) : '')
        : null;

      if (!root) {
        if (!isStaticRoute && !hadVisibleContent && !preparedProfile) {
          deps.clearRootBoundState();
        }
        deps.setLoading(false);
        await (deps.waitForRouteViewPaint ?? waitForNextBrowserPaint)();
        if (
          token !== deps.getLoadToken()
          || routeKey(deps.getRoute()) !== routeKey(route)
        ) {
          return;
        }
        if (preparedProfile) {
          deps.setProfileNotesLoading(false);
          deps.scheduleProfileSocialHydration(null, preparedProfile.pubkey, token, {
            includeProfiles: false,
            defer: false,
          });
          if (route.kind === 'profile' && route.section !== 'likes') {
            // Offline, the profile's own personal Hashtree index still has its posts.
            void deps.hydrateProfileFromPersonalIndex(null, preparedProfile.pubkey, token);
          }
        }
        if (route.kind === 'thread') {
          await deps.hydrateThread(null, threadRouteDiscoveryRef(route), token);
        }
        startViewerHydration(null, activePubkey, route, token);
        return;
      }

      deps.setPageError('');
      if (!preparedProfile) {
        deps.setLoading(!hadVisibleContent);
      }

      if (isStaticRoute) {
        deps.setLoading(false);
      }

      // Updating the URL and starting cached Hashtree reads in the same task
      // can make even fire-and-forget async functions block the first paint
      // with synchronous setup or already-settled microtasks. Commit the new
      // route shell first, matching iris-client's render-before-subscribe flow.
      await (deps.waitForRouteViewPaint ?? waitForNextBrowserPaint)();
      if (
        token !== deps.getLoadToken()
        || routeKey(deps.getRoute()) !== routeKey(route)
      ) {
        return;
      }

      if (isStaticRoute) {
        startViewerHydration(root, activePubkey, route, token);
        return;
      }

      try {
        if (route.kind === 'profile') {
          if (!preparedProfile) {
            throw new Error('Invalid profile');
          }
          const needsProfileLikes = preparedProfile.changedProfile
            || preparedProfile.changedRoot
            || deps.getProfilePage()?.likedNotes === undefined;
          const shouldReloadLikes = route.section === 'likes' && needsProfileLikes;
          const shouldReloadNotes = route.section !== 'likes' && shouldReloadProfileNotes({
            changedProfile: preparedProfile.changedProfile,
            changedRoot: preparedProfile.changedRoot,
            page: deps.getProfilePage(),
            fetchLimit: deps.getProfileNoteFetchLimit(),
          });

          deps.setProfileNotesLoading(shouldReloadLikes || shouldReloadNotes);

          if (shouldReloadLikes) {
            deps.setNoteEngagementsEmpty();
            void deps.hydrateProfileLikes(root, preparedProfile.pubkey, token);
          } else {
            if (shouldReloadNotes) {
              deps.setNoteEngagementsEmpty();
              void deps.hydrateProfileNotesProgressively(root, preparedProfile.pubkey, token, {
                emitEvery: 4,
                // Paint the first verified post immediately. The same stream
                // keeps filling the prefetched page without blocking the view.
                visibleThreshold: 1,
              });
            } else {
              deps.setProfileNotesLoading(false);
            }
          }

          const startSecondaryProfileWork = () => {
            if (
              token !== deps.getLoadToken()
              || routeKey(deps.getRoute()) !== routeKey(route)
            ) {
              return;
            }
            void deps.hydrateProfileRecordProgressively(
              root,
              preparedProfile.pubkey,
              token,
            );
            deps.setProfileSocialLoading(true);
            deps.scheduleProfileSocialHydration(root, preparedProfile.pubkey, token, {
              includeProfiles: !preparedProfile.isContentSection,
              includeFollows: true,
              includeFollowers: true,
              progressSection: route.section === 'follows' || route.section === 'followers'
                ? route.section
                : undefined,
              defer: false,
            });
            if (!shouldReloadLikes && needsProfileLikes) {
              void deps.hydrateProfileLikes(
                root,
                preparedProfile.pubkey,
                token,
                { updateLoading: false },
              );
            }
            if (route.section !== 'likes') {
              void deps.hydrateProfileFromPersonalIndex(root, preparedProfile.pubkey, token);
            }
            startViewerHydration(root, activePubkey, route, token);
            deps.scheduleEmptyProfileRetry(root, preparedProfile.pubkey, token);
          };
          if (preparedProfile.isContentSection) {
            // Give the authored-note stream first access to the warm worker and
            // main thread. Identity, viewer, Likes-count, and social refreshes
            // are secondary to the first visible profile post.
            scheduleIdleWork(startSecondaryProfileWork);
          } else {
            startSecondaryProfileWork();
          }
          return;
        }

        if (route.kind === 'thread') {
          await deps.hydrateThread(root, threadRouteDiscoveryRef(route), token);
          startViewerHydration(root, activePubkey, route, token);
          return;
        }

        if (!activePubkey && route.kind === 'home' && route.tab === 'popular') {
          deps.clearProfileViewState();
          deps.clearThreadViewState();
          await deps.hydratePopularFeed(root, token);
          if (deps.getLoadToken() === token) { deps.resetViewerRouteState(); }
          return;
        }
        if (!activePubkey) {
          deps.clearProfileViewState();
          deps.setFeed(null);
          deps.clearThreadViewState();
          return;
        }

        if (route.kind === 'notifications') {
          deps.setFeed(null);
          deps.clearProfileViewState();
          deps.clearThreadViewState();
          deps.setNoteEngagementsEmpty();
          if (activePubkey) { await deps.hydrateNotifications(root, activePubkey, token); }
          startViewerHydration(root, activePubkey, route, token);
          return;
        }

        if (activePubkey && route.kind === 'home' && route.tab !== 'favorites') {
          await deps.hydrateViewerGraphFollows(root, activePubkey, token);
          if (token !== deps.getLoadToken()) { return; }
        }

        if (route.kind === 'home' && route.tab === 'for-you') {
          deps.clearProfileViewState();
          deps.clearThreadViewState();
          await deps.hydrateForYouFeed(root, activePubkey ?? '', token);
          startViewerHydration(root, activePubkey, route, token);
          return;
        }

        const feedOwnerPubkey = route.kind === 'home' && route.tab === 'favorites'
          ? 'favorites' : deps.getActiveFeedSourcePubkey() ?? activePubkey ?? '';
        await deps.hydrateHomeFeed(root, feedOwnerPubkey, activePubkey ?? '', token);
        startViewerHydration(root, activePubkey, route, token);
      } catch (error) {
        if (token !== deps.getLoadToken()) {
          return;
        }

        if (route.kind === 'profile') {
          deps.setProfileNotesLoading(false);
        }

        if (!hadVisibleContent) {
          deps.clearRootBoundState();
        }
        deps.setPageError(deps.reportError(
          route.kind === 'profile'
            ? "Couldn't load this profile."
            : route.kind === 'thread'
              ? "Couldn't load this thread."
              : route.kind === 'notifications'
                ? "Couldn't load notifications right now."
                : "Couldn't load the feed right now.",
          error,
        ));
      } finally {
        if (token === deps.getLoadToken()) {
          deps.setLoading(false);
        }
      }
    },
  };
}
