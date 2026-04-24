import type { Event as NostrEvent } from 'nostr-tools';
import type { ManagedWebRTCMeshSessionConfig } from './managedMeshHost.js';
import {
  createAuthenticatedNip44GiftWrap,
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
  encrypt?: (recipientPubkey: string, plaintext: string) => Promise<string> | string;
  publishMode?: SimplePoolPublishMode;
  publishMaxWaitMs?: number;
  nowMs?: CreateSimplePoolSignalingSenderOptions<TEvent>['nowMs'];
}

export function createManagedNostrMeshSession<TEvent extends NostrEvent>(
  options: CreateManagedNostrMeshSessionOptions<TEvent>,
): ManagedWebRTCMeshSessionConfig {
  const {
    signEvent,
    giftWrap,
    encrypt,
    publishMode = 'require-one',
    publishMaxWaitMs,
    nowMs,
    ...session
  } = options;
  const resolvedGiftWrap = giftWrap ?? (
    encrypt
      ? createAuthenticatedNip44GiftWrap<TEvent>({
        senderPubkey: options.pubkey,
        signEvent,
        encrypt,
        nowMs,
      })
      : createNip44GiftWrap<TEvent>(options.pubkey)
  );

  return {
    ...session,
    createSendSignaling: createSimplePoolSignalingSender({
      signEvent,
      giftWrap: resolvedGiftWrap,
      publishMode,
      publishMaxWaitMs,
      nowMs,
    }),
  };
}
