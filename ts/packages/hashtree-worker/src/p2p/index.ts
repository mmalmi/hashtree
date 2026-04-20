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
  createSignalingFilters,
  sendSignalingMessage,
  decodeSignalingEvent,
} from './signaling.js';

export type {
  SignalingEventLike,
  GiftSeal,
  SignalingTemplate,
  SignalingInnerEvent,
  SignalingFilters,
  DecodedSignalingEvent,
} from './signaling.js';
