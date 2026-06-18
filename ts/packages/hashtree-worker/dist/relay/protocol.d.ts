/**
 * Worker Protocol Types
 *
 * Message types for communication between main thread and hashtree worker.
 * Worker owns: HashTree, WebRTC, Nostr (via nostr-tools)
 * Main thread owns: UI, NIP-07 extension access (signing/encryption)
 */
import type { CID } from '@hashtree/core';
import type { BlossomBandwidthStats } from '@hashtree/core/worker';
export type { NostrFilter, UnsignedEvent, SignedEvent, SocialGraphEvent, BlossomBandwidthStats, BlossomBandwidthServerStats, } from '@hashtree/core/worker';
export type TreeVisibility = 'public' | 'link-visible' | 'private';
export interface TreeRootInfo {
    hash: Uint8Array;
    key?: Uint8Array;
    visibility: TreeVisibility;
    labels?: string[];
    updatedAt: number;
    snapshotNhash?: string;
    encryptedKey?: string;
    keyId?: string;
    selfEncryptedKey?: string;
    selfEncryptedLinkKey?: string;
}
export interface PeerStats {
    peerId: string;
    pubkey: string;
    connected: boolean;
    pool: 'follows' | 'other';
    requestsSent: number;
    requestsReceived: number;
    responsesSent: number;
    responsesReceived: number;
    bytesSent: number;
    bytesReceived: number;
    forwardedRequests: number;
    forwardedResolved: number;
    forwardedSuppressed: number;
}
export type WorkerRequest = {
    type: 'init';
    id: string;
    config: WorkerConfig;
} | {
    type: 'close';
    id: string;
} | {
    type: 'setIdentity';
    id: string;
    pubkey: string;
    nsec?: string;
} | {
    type: 'get';
    id: string;
    hash: Uint8Array;
} | {
    type: 'put';
    id: string;
    hash: Uint8Array;
    data: Uint8Array;
} | {
    type: 'has';
    id: string;
    hash: Uint8Array;
} | {
    type: 'delete';
    id: string;
    hash: Uint8Array;
} | {
    type: 'readFile';
    id: string;
    cid: CID;
} | {
    type: 'readFileRange';
    id: string;
    cid: CID;
    start: number;
    end?: number;
} | {
    type: 'readFileStream';
    id: string;
    cid: CID;
} | {
    type: 'writeFile';
    id: string;
    parentCid: CID | null;
    path: string;
    data: Uint8Array;
} | {
    type: 'deleteFile';
    id: string;
    parentCid: CID;
    path: string;
} | {
    type: 'listDir';
    id: string;
    cid: CID;
} | {
    type: 'resolveRoot';
    id: string;
    npub: string;
    path?: string;
} | {
    type: 'setTreeRootCache';
    id: string;
    npub: string;
    treeName: string;
    hash: Uint8Array;
    key?: Uint8Array;
    visibility: TreeVisibility;
    labels?: string[];
    encryptedKey?: string;
    keyId?: string;
    selfEncryptedKey?: string;
    selfEncryptedLinkKey?: string;
} | {
    type: 'getTreeRootInfo';
    id: string;
    npub: string;
    treeName: string;
} | {
    type: 'mergeTreeRootKey';
    id: string;
    npub: string;
    treeName: string;
    hash: Uint8Array;
    key: Uint8Array;
} | {
    type: 'subscribeTreeRoots';
    id: string;
    pubkey: string;
} | {
    type: 'unsubscribeTreeRoots';
    id: string;
    pubkey: string;
} | {
    type: 'subscribe';
    id: string;
    filters: NostrFilter[];
} | {
    type: 'unsubscribe';
    id: string;
    subId: string;
} | {
    type: 'publish';
    id: string;
    event: SignedEvent;
} | {
    type: 'registerMediaPort';
    port: MessagePort;
    debug?: boolean;
} | {
    type: 'getPeerStats';
    id: string;
} | {
    type: 'getRelayStats';
    id: string;
} | {
    type: 'getStorageStats';
    id: string;
} | {
    type: 'setWebRTCPools';
    id: string;
    pools: {
        follows: {
            max: number;
            satisfied: number;
        };
        other: {
            max: number;
            satisfied: number;
        };
    };
} | {
    type: 'setWebRTCForwardRateLimit';
    id: string;
    forwardRateLimit?: ForwardRateLimitConfig;
} | {
    type: 'sendWebRTCHello';
    id: string;
} | {
    type: 'setFollows';
    id: string;
    follows: string[];
} | {
    type: 'setBlossomServers';
    id: string;
    servers: BlossomServerConfig[];
} | {
    type: 'setStorageMaxBytes';
    id: string;
    maxBytes: number;
} | {
    type: 'setRelays';
    id: string;
    relays: string[];
} | {
    type: 'pushToBlossom';
    id: string;
    cidHash: Uint8Array;
    cidKey?: Uint8Array;
    treeName?: string;
} | {
    type: 'startBlossomSession';
    id: string;
    sessionId: string;
    totalChunks: number;
} | {
    type: 'endBlossomSession';
    id: string;
} | {
    type: 'republishTrees';
    id: string;
} | {
    type: 'republishTree';
    id: string;
    pubkey: string;
    treeName: string;
} | {
    type: 'ping';
    id: string;
} | {
    type: 'initSocialGraph';
    id: string;
    rootPubkey?: string;
} | {
    type: 'setSocialGraphRoot';
    id: string;
    pubkey: string;
} | {
    type: 'handleSocialGraphEvents';
    id: string;
    events: SocialGraphEvent[];
} | {
    type: 'getFollowDistance';
    id: string;
    pubkey: string;
} | {
    type: 'isFollowing';
    id: string;
    follower: string;
    followed: string;
} | {
    type: 'getFollows';
    id: string;
    pubkey: string;
} | {
    type: 'getFollowers';
    id: string;
    pubkey: string;
} | {
    type: 'getFollowedByFriends';
    id: string;
    pubkey: string;
} | {
    type: 'getSocialGraphSize';
    id: string;
} | {
    type: 'signed';
    id: string;
    event?: SignedEvent;
    error?: string;
} | {
    type: 'encrypted';
    id: string;
    ciphertext?: string;
    error?: string;
} | {
    type: 'decrypted';
    id: string;
    plaintext?: string;
    error?: string;
} | WebRTCEvent;
/** Blossom server configuration */
export interface BlossomServerConfig {
    url: string;
    read?: boolean;
    write?: boolean;
    preferBatchReads?: boolean;
}
export interface ForwardRateLimitConfig {
    maxForwardsPerPeerWindow?: number;
    windowMs?: number;
}
export interface WorkerConfig {
    relays: string[];
    blossomServers?: BlossomServerConfig[];
    pubkey: string;
    nsec?: string;
    storeName?: string;
    forwardRateLimit?: ForwardRateLimitConfig;
}
export type WorkerResponse = {
    type: 'ready';
} | {
    type: 'pong';
    id: string;
} | {
    type: 'error';
    id?: string;
    error: string;
} | {
    type: 'result';
    id: string;
    data?: Uint8Array;
    error?: string;
} | {
    type: 'bool';
    id: string;
    value: boolean;
    error?: string;
} | {
    type: 'cid';
    id: string;
    cid?: CID;
    error?: string;
} | {
    type: 'void';
    id: string;
    error?: string;
} | {
    type: 'dirListing';
    id: string;
    entries?: DirEntry[];
    error?: string;
} | {
    type: 'streamChunk';
    id: string;
    chunk: Uint8Array;
    done: boolean;
} | {
    type: 'event';
    subId: string;
    event: SignedEvent;
} | {
    type: 'eose';
    subId: string;
} | {
    type: 'peerStats';
    id: string;
    stats: PeerStats[];
} | {
    type: 'relayStats';
    id: string;
    stats: RelayStats[];
} | {
    type: 'storageStats';
    id: string;
    items: number;
    bytes: number;
} | {
    type: 'blossomBandwidth';
    stats: BlossomBandwidthStats;
} | {
    type: 'blossomUploadError';
    hash: string;
    error: string;
} | {
    type: 'blossomUploadProgress';
    progress: BlossomUploadProgress;
} | {
    type: 'blossomPushResult';
    id: string;
    pushed: number;
    skipped: number;
    failed: number;
    error?: string;
    errors?: string[];
} | {
    type: 'republishResult';
    id: string;
    count: number;
    error?: string;
    encryptionErrors?: string[];
} | {
    type: 'blossomPushStarted';
    treeName: string;
    totalChunks: number;
} | {
    type: 'blossomPushProgress';
    treeName: string;
    current: number;
    total: number;
} | {
    type: 'blossomPushComplete';
    treeName: string;
    pushed: number;
    skipped: number;
    failed: number;
} | {
    type: 'treeRootUpdate';
    npub: string;
    treeName: string;
    hash: Uint8Array;
    key?: Uint8Array;
    visibility: TreeVisibility;
    labels?: string[];
    updatedAt: number;
    snapshotNhash?: string;
    encryptedKey?: string;
    keyId?: string;
    selfEncryptedKey?: string;
    selfEncryptedLinkKey?: string;
} | {
    type: 'treeRootInfo';
    id: string;
    record?: TreeRootInfo;
    error?: string;
} | {
    type: 'socialGraphInit';
    id: string;
    version: number;
    size: number;
    error?: string;
} | {
    type: 'socialGraphVersion';
    version: number;
} | {
    type: 'followDistance';
    id: string;
    distance: number;
    error?: string;
} | {
    type: 'isFollowingResult';
    id: string;
    result: boolean;
    error?: string;
} | {
    type: 'pubkeyList';
    id: string;
    pubkeys: string[];
    error?: string;
} | {
    type: 'socialGraphSize';
    id: string;
    size: number;
    error?: string;
} | {
    type: 'signEvent';
    id: string;
    event: UnsignedEvent;
} | {
    type: 'nip44Encrypt';
    id: string;
    pubkey: string;
    plaintext: string;
} | {
    type: 'nip44Decrypt';
    id: string;
    pubkey: string;
    ciphertext: string;
} | WebRTCCommand;
export interface DirEntry {
    name: string;
    isDir: boolean;
    size?: number;
    cid?: CID;
}
export interface RelayStats {
    url: string;
    connected: boolean;
    eventsReceived: number;
    eventsSent: number;
}
/** Per-server upload status */
export interface BlossomServerStatus {
    url: string;
    uploaded: number;
    failed: number;
    skipped: number;
}
/** Overall blossom upload progress */
export interface BlossomUploadProgress {
    sessionId: string;
    totalChunks: number;
    processedChunks: number;
    servers: BlossomServerStatus[];
}
export interface MediaRequestByCid {
    type: 'media';
    requestId: string;
    cid: string;
    start: number;
    end?: number;
    mimeType?: string;
}
export interface MediaRequestByPath {
    type: 'mediaByPath';
    requestId: string;
    npub: string;
    path: string;
    start: number;
    end?: number;
    mimeType?: string;
}
export type MediaRequest = MediaRequestByCid | MediaRequestByPath;
export type MediaResponse = {
    type: 'headers';
    requestId: string;
    totalSize: number;
    mimeType: string;
    isLive?: boolean;
} | {
    type: 'chunk';
    requestId: string;
    data: Uint8Array;
} | {
    type: 'done';
    requestId: string;
} | {
    type: 'error';
    requestId: string;
    message: string;
};
/** Worker → Main: Commands to control WebRTC connections */
export type WebRTCCommand = {
    type: 'rtc:createPeer';
    peerId: string;
    pubkey: string;
} | {
    type: 'rtc:closePeer';
    peerId: string;
} | {
    type: 'rtc:createOffer';
    peerId: string;
} | {
    type: 'rtc:createAnswer';
    peerId: string;
} | {
    type: 'rtc:setLocalDescription';
    peerId: string;
    sdp: RTCSessionDescriptionInit;
} | {
    type: 'rtc:setRemoteDescription';
    peerId: string;
    sdp: RTCSessionDescriptionInit;
} | {
    type: 'rtc:addIceCandidate';
    peerId: string;
    candidate: RTCIceCandidateInit;
} | {
    type: 'rtc:sendData';
    peerId: string;
    data: Uint8Array;
};
/** Main → Worker: Events from WebRTC connections */
export type WebRTCEvent = {
    type: 'rtc:peerCreated';
    peerId: string;
} | {
    type: 'rtc:peerStateChange';
    peerId: string;
    state: RTCPeerConnectionState;
} | {
    type: 'rtc:peerClosed';
    peerId: string;
} | {
    type: 'rtc:offerCreated';
    peerId: string;
    sdp: RTCSessionDescriptionInit;
} | {
    type: 'rtc:answerCreated';
    peerId: string;
    sdp: RTCSessionDescriptionInit;
} | {
    type: 'rtc:descriptionSet';
    peerId: string;
    error?: string;
} | {
    type: 'rtc:iceCandidate';
    peerId: string;
    candidate: RTCIceCandidateInit | null;
} | {
    type: 'rtc:iceGatheringComplete';
    peerId: string;
} | {
    type: 'rtc:dataChannelOpen';
    peerId: string;
} | {
    type: 'rtc:dataChannelMessage';
    peerId: string;
    data: Uint8Array;
} | {
    type: 'rtc:dataChannelClose';
    peerId: string;
} | {
    type: 'rtc:dataChannelError';
    peerId: string;
    error: string;
} | {
    type: 'rtc:bufferHigh';
    peerId: string;
} | {
    type: 'rtc:bufferLow';
    peerId: string;
};
export declare function generateRequestId(): string;
//# sourceMappingURL=protocol.d.ts.map