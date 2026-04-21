import type { Event as NostrEvent } from 'nostr-tools';
import type { ManagedWebRTCMeshSessionConfig } from './managedMeshHost.js';
import {
  createNip44GiftWrap,
  createSimplePoolSignalingSender,
  type CreateSimplePoolSignalingSenderOptions,
  type SignalingInnerEvent,
  type SignalingTemplate,
  type SimplePoolPublishMode,
} from './signaling.js';

export interface CreateManagedNostrMeshSessionOptions<TEvent extends NostrEvent>
  extends Omit<ManagedWebRTCMeshSessionConfig, 'sendSignaling' | 'createSendSignaling'> {
  signEvent: (template: SignalingTemplate) => Promise<TEvent>;
  giftWrap?: (innerEvent: SignalingInnerEvent, recipientPubkey: string) => Promise<TEvent>;
  publishMode?: SimplePoolPublishMode;
  publishMaxWaitMs?: number;
  nowMs?: CreateSimplePoolSignalingSenderOptions<TEvent>['nowMs'];
}

export function createManagedNostrMeshSession<TEvent extends NostrEvent>(
  options: CreateManagedNostrMeshSessionOptions<TEvent>,
): ManagedWebRTCMeshSessionConfig {
  const {
    signEvent,
    giftWrap = createNip44GiftWrap<TEvent>(options.pubkey),
    publishMode = 'require-one',
    publishMaxWaitMs,
    nowMs,
    ...session
  } = options;

  return {
    ...session,
    createSendSignaling: createSimplePoolSignalingSender({
      signEvent,
      giftWrap,
      publishMode,
      publishMaxWaitMs,
      nowMs,
    }),
  };
}
