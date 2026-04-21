export {
  WebRTCController,
  type WebRTCControllerConfig,
} from './webrtcController.js';

export {
  MeshQueryRouter,
  encodeForwardRequest,
  type MeshQueryRouterConfig,
  type MeshQueryRouterPeer,
} from './meshQueryRouter.js';

export {
  QueryForwardingMachine,
  type QueryForwardingMachineConfig,
  type ForwardDecision,
  type ForwardTimeoutEvent,
} from './queryForwardingMachine.js';

export {
  WebRTCProxy,
  initWebRTCProxy,
  getWebRTCProxy,
  closeWebRTCProxy,
} from './webrtcProxy.js';

export {
  createWebRTCWorkerP2PProvider,
} from './clientBridge.js';

export type {
  WebRTCWorkerP2PProviderOptions,
} from './clientBridge.js';

export {
  ManagedWebRTCMeshHost,
} from './managedMeshHost.js';

export type {
  ManagedWebRTCMeshHostOptions,
  ManagedWebRTCMeshSessionConfig,
  WebRTCMeshPoolConfig,
} from './managedMeshHost.js';

export type {
  WebRTCCommand,
  WebRTCEvent,
} from './protocol.js';

export {
  SIGNALING_KIND,
  HELLO_TAG,
  MAX_EVENT_AGE_SEC,
  createDecryptingGiftUnwrapper,
  createNip44GiftWrap,
  createSecretKeyEventSigner,
  createSecretKeyGiftUnwrapper,
  createSignalingFilters,
  createSimplePoolSignalingSender,
  sendSignalingMessage,
  decodeSignalingEvent,
} from './signaling.js';

export type {
  CreateNip44GiftWrapOptions,
  GiftCiphertextDecryptor,
  CreateSimplePoolSignalingSenderOptions,
  SimplePoolPublishMode,
  SignalingEventLike,
  GiftSeal,
  SignalingTemplate,
  SignalingInnerEvent,
  SignalingFilters,
  DecodedSignalingEvent,
} from './signaling.js';

export {
  createManagedNostrMeshSession,
} from './session.js';

export type {
  CreateManagedNostrMeshSessionOptions,
} from './session.js';
