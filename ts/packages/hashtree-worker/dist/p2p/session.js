import { createAuthenticatedNip44GiftWrap, createNip44GiftWrap, createSimplePoolSignalingSender, } from './signaling.js';
export function createManagedNostrMeshSession(options) {
    const { signEvent, giftWrap, encrypt, publishMode = 'require-one', publishMaxWaitMs, nowMs, ...session } = options;
    const resolvedGiftWrap = giftWrap ?? (encrypt
        ? createAuthenticatedNip44GiftWrap({
            senderPubkey: options.pubkey,
            signEvent,
            encrypt,
            nowMs,
        })
        : createNip44GiftWrap(options.pubkey));
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
//# sourceMappingURL=session.js.map