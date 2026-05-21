import type { Event as NostrEvent } from 'nostr-tools';
import type { ManagedWebRTCMeshSessionConfig } from './managedMeshHost.js';
import { type CreateSimplePoolSignalingSenderOptions, type SignalingInnerEvent, type SignalingTemplate, type SimplePoolPublishMode } from './signaling.js';
export interface CreateManagedNostrMeshSessionOptions<TEvent extends NostrEvent> extends Omit<ManagedWebRTCMeshSessionConfig, 'sendSignaling' | 'createSendSignaling'> {
    signEvent: (template: SignalingTemplate) => Promise<TEvent>;
    giftWrap?: (innerEvent: SignalingInnerEvent, recipientPubkey: string) => Promise<TEvent>;
    encrypt?: (recipientPubkey: string, plaintext: string) => Promise<string> | string;
    publishMode?: SimplePoolPublishMode;
    publishMaxWaitMs?: number;
    nowMs?: CreateSimplePoolSignalingSenderOptions<TEvent>['nowMs'];
}
export declare function createManagedNostrMeshSession<TEvent extends NostrEvent>(options: CreateManagedNostrMeshSessionOptions<TEvent>): ManagedWebRTCMeshSessionConfig;
//# sourceMappingURL=session.d.ts.map