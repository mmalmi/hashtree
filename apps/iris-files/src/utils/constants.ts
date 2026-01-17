// Nostr Event Kinds Constants
// Based on NIPs (Nostr Implementation Possibilities)
// Only includes constants that are actually used in the codebase

const TEST_BOOTSTRAP_PUBKEY = import.meta.env.VITE_TEST_BOOTSTRAP_PUBKEY as string | undefined;
// Default content/social graph bootstrap pubkey (sirius)
export const DEFAULT_BOOTSTRAP_PUBKEY =
  import.meta.env.VITE_TEST_MODE && TEST_BOOTSTRAP_PUBKEY
    ? TEST_BOOTSTRAP_PUBKEY
    : '4523be58d395b1b196a9b8c82b038b6895cb02b683d0c253a955068dba1facd0';

// Seed pubkeys to populate the video feed when follows are empty.
export const DEFAULT_VIDEO_FEED_PUBKEYS = [
  'feb535ca427f7ab7551adfc5fcc39132b10cbc11ce5841f05588680e94f4e71a',
];

// NIP-01: Basic protocol
export const KIND_METADATA = 0 // User profile metadata
export const KIND_TEXT_NOTE = 1 // Text note
export const KIND_CONTACTS = 3 // Contact list (follows)

// NIP-18: Reposts
export const KIND_REPOST = 6

// NIP-25: Reactions
export const KIND_REACTION = 7

// NIP-16: Event Treatment
export const KIND_EPHEMERAL = 20000 // Ephemeral events

// NIP-04: Encrypted Direct Messages
export const KIND_CHAT_MESSAGE = 14 // Encrypted direct message (double-ratchet)

// NIP-28: Public chat
export const KIND_CHANNEL_CREATE = 40 // Channel creation
export const KIND_CHANNEL_MESSAGE = 42 // Channel message

// NIP-57: Lightning zaps
export const KIND_ZAP_RECEIPT = 9735

// NIP-51: Lists
export const KIND_MUTE_LIST = 10000 // Mute list (deprecated, use 30000)
export const KIND_FLAG_LIST = 16463 // Flagged/reported users list

// NIP-78: App-specific data
export const KIND_APP_DATA = 30078

// Long-form content
export const KIND_LONG_FORM_CONTENT = 30023

// HTTP authentication
export const KIND_HTTP_AUTH = 27235

// Blossom authorization
export const KIND_BLOSSOM_AUTH = 24242

// Debug/development
export const KIND_DEBUG_DATA = 30000 // Used for encrypted debug key-value storage

// Classified listings
export const KIND_CLASSIFIED = 30402

// Highlights
export const KIND_HIGHLIGHT = 9802

// NIP-68: Picture-first feeds
export const KIND_PICTURE_FIRST = 20

// Additional kinds found in codebase
export const KIND_WALLET_CONNECT = 6927

// NIP-34: Git Repositories
export const KIND_REPO_ANNOUNCEMENT = 30617 // Repository announcement
export const KIND_REPO_STATE = 30618 // Repository state (branch/tag tracking)
export const KIND_PATCH = 1617 // Patch (code changes under 60kb)
export const KIND_PULL_REQUEST = 1618 // Pull request
export const KIND_PR_UPDATE = 1619 // PR update (revision)
export const KIND_ISSUE = 1621 // Issue (bug report, feature request)
// Status events (1630-1633)
export const KIND_STATUS_OPEN = 1630 // Open/default
export const KIND_STATUS_APPLIED = 1631 // Applied/Merged/Resolved
export const KIND_STATUS_CLOSED = 1632 // Closed
export const KIND_STATUS_DRAFT = 1633 // Draft

// Debug namespaces for debug pkg
export const DEBUG_NAMESPACES = {
  // NDK
  NDK_RELAY: "ndk:relay",
  NDK_RELAY_CONN: "ndk:relay:conn",
  NDK_RELAY_ERROR: "ndk:relay:error",
  NDK_RELAY_WARN: "ndk:relay:warn",
  NDK_SUBSCRIPTION: "ndk:subscription",
  NDK_SUBSCRIPTION_ERROR: "ndk:subscription:error",
  NDK_SUBSCRIPTION_WARN: "ndk:subscription:warn",
  NDK_CACHE: "ndk:cache",
  NDK_CACHE_ERROR: "ndk:cache:error",
  NDK_CACHE_WARN: "ndk:cache:warn",
  NDK_POOL: "ndk:pool",
  NDK_POOL_ERROR: "ndk:pool:error",
  NDK_POOL_WARN: "ndk:pool:warn",
  NDK_WORKER: "ndk:worker",
  NDK_WORKER_ERROR: "ndk:worker:error",
  NDK_WORKER_WARN: "ndk:worker:warn",
  NDK_TRANSPORT: "ndk:transport",

  // WebRTC
  WEBRTC_PEER: "webrtc:peer",
  WEBRTC_PEER_LIFECYCLE: "webrtc:peer:lifecycle", // connect/disconnect/state changes
  WEBRTC_PEER_MESSAGES: "webrtc:peer:messages", // offer/answer/ICE candidates
  WEBRTC_PEER_DATA: "webrtc:peer:data", // data channel events
  WEBRTC_SIGNALING: "webrtc:signaling",

  // Cashu
  CASHU_WALLET: "cashu:wallet",
  CASHU_WALLET_ERROR: "cashu:wallet:error",
  CASHU_WALLET_WARN: "cashu:wallet:warn",
  CASHU_MINT: "cashu:mint",
  CASHU_MINT_ERROR: "cashu:mint:error",
  CASHU_MINT_WARN: "cashu:mint:warn",

  // UI
  UI_FEED: "ui:feed",
  UI_FEED_ERROR: "ui:feed:error",
  UI_FEED_WARN: "ui:feed:warn",
  UI_CHAT: "ui:chat",
  UI_CHAT_ERROR: "ui:chat:error",
  UI_CHAT_WARN: "ui:chat:warn",

  // Hooks
  HOOKS: "hooks",
  HOOKS_ERROR: "hooks:error",
  HOOKS_WARN: "hooks:warn",

  // Utils
  UTILS: "utils",
  UTILS_ERROR: "utils:error",
  UTILS_WARN: "utils:warn",
} as const
